//! Stage 1 and 2 of boot: bring up the virtual filesystems and put the machine
//! into a known state.
//!
//! Nothing else in the userland can work until this runs. `haimanager` needs
//! `/dev/dri/card0` and `/dev/input/event*`, both of which only appear once
//! devtmpfs is mounted. Note that `CONFIG_DEVTMPFS_MOUNT=y` does *not* help
//! here: the kernel only auto-mounts devtmpfs when booting a real root
//! filesystem, never for an initramfs, and the supervisor boots from an
//! initramfs on purpose. PID 1 must not depend on the disk it exists to
//! bring up, so its pages live in RAM and the disk is something it mounts:
//! the system volume, holding everything else the OS is.

use std::fs;
use std::io;
use std::io::Write as _;
use std::path::Path;

use rustix::mount::{self, MountFlags, UnmountFlags};

use crate::klog::{kerr, kinfo, kwarn};

/// One entry in the boot mount table.
struct MountPoint {
    source: &'static str,
    target: &'static str,
    fstype: &'static str,
    flags: MountFlags,
    /// Filesystem-specific options, the fifth argument to `mount(2)`.
    data: Option<&'static std::ffi::CStr>,
    /// If true, failing to mount this aborts the boot. If false, we log and
    /// carry on: the system is degraded but still usable.
    required: bool,
}

/// The mount table, in dependency order.
///
/// Order matters more than it looks. `/dev/pts` and `/dev/shm` are created
/// *inside* devtmpfs, so they can only be made after `/dev` itself is mounted,
/// and `/sys/fs/cgroup` only exists once sysfs is up.
const MOUNTS: &[MountPoint] = &[
    MountPoint {
        source: "proc",
        target: "/proc",
        fstype: "proc",
        flags: MountFlags::NOSUID.union(MountFlags::NOEXEC).union(MountFlags::NODEV),
        data: None,
        required: true,
    },
    MountPoint {
        source: "sysfs",
        target: "/sys",
        fstype: "sysfs",
        flags: MountFlags::NOSUID.union(MountFlags::NOEXEC).union(MountFlags::NODEV),
        data: None,
        required: true,
    },
    // The one that matters: this is what populates /dev/dri and /dev/input,
    // and it is why Agentware needs no udev.
    MountPoint {
        source: "devtmpfs",
        target: "/dev",
        fstype: "devtmpfs",
        flags: MountFlags::NOSUID,
        data: Some(c"mode=0755"),
        required: true,
    },
    MountPoint {
        source: "devpts",
        target: "/dev/pts",
        fstype: "devpts",
        flags: MountFlags::NOSUID.union(MountFlags::NOEXEC),
        data: Some(c"mode=0620,gid=5,ptmxmode=0666"),
        required: false,
    },
    MountPoint {
        source: "tmpfs",
        target: "/dev/shm",
        fstype: "tmpfs",
        flags: MountFlags::NOSUID.union(MountFlags::NODEV),
        data: Some(c"mode=1777"),
        required: false,
    },
    // Home of the supervisor's control socket, so it has to be a real mount
    // rather than a directory in the initramfs image.
    MountPoint {
        source: "tmpfs",
        target: "/run",
        fstype: "tmpfs",
        flags: MountFlags::NOSUID.union(MountFlags::NODEV),
        data: Some(c"mode=0755"),
        required: true,
    },
    MountPoint {
        source: "tmpfs",
        target: "/tmp",
        fstype: "tmpfs",
        flags: MountFlags::NOSUID.union(MountFlags::NODEV),
        data: Some(c"mode=1777"),
        required: false,
    },
    // One cgroup per agentdesk is created under here, which is what makes a
    // workspace killable as a unit. Also where per-workspace resource limits
    // will go.
    MountPoint {
        source: "cgroup2",
        target: "/sys/fs/cgroup",
        fstype: "cgroup2",
        flags: MountFlags::NOSUID.union(MountFlags::NOEXEC).union(MountFlags::NODEV),
        data: None,
        required: false,
    },
];

/// Mount every virtual filesystem in the table.
///
/// Returns an error only if a `required` mount fails, in which case the caller
/// should not continue booting.
pub fn mount_virtual_filesystems() -> io::Result<()> {
    for mp in MOUNTS {
        if let Err(err) = fs::create_dir_all(mp.target) {
            if mp.required {
                return Err(io::Error::other(format!(
                    "could not create mount point {}: {err}",
                    mp.target
                )));
            }
            kwarn!("could not create mount point {}: {err}", mp.target);
            continue;
        }

        match mount::mount(mp.source, mp.target, mp.fstype, mp.flags, mp.data) {
            Ok(()) => kinfo!("mounted {} on {}", mp.fstype, mp.target),
            // Already mounted, which is what the kernel's own devtmpfs
            // automount looks like from here: the state this stage exists to
            // reach, not a failure.
            Err(rustix::io::Errno::BUSY) => {
                kinfo!("{} already mounted on {}", mp.fstype, mp.target)
            }
            Err(err) if mp.required => {
                return Err(io::Error::other(format!(
                    "could not mount {} on {}: {err}",
                    mp.fstype, mp.target
                )));
            }
            Err(err) => kwarn!("could not mount {} on {}: {err} (continuing)", mp.fstype, mp.target),
        }
    }

    // Holds the control socket that the compositor and the agentdesks use to ask
    // the supervisor to fork things.
    if let Err(err) = fs::create_dir_all("/run/agentware") {
        kwarn!("could not create /run/agentware: {err}");
    }

    Ok(())
}

/// Where the system volume is mounted: the OS itself, everything except the
/// supervisor. The supervisor rides the initramfs so that a missing or dying
/// disk is something it reports rather than something that takes it down;
/// this volume is the first thing it goes looking for.
pub const SYSTEM_DIR: &str = "/system";

/// The block device the system volume is expected on: the first virtio
/// drive. `agentware.system=/dev/...` on the kernel command line names
/// another.
const SYSTEM_DEVICE: &str = "/dev/vda";

/// The directories the system volume provides, bound over the root so that
/// no path anywhere else in the userland changes: `/bin/haimanager` is
/// `/system/bin/haimanager` without any process having to know it.
/// `home` is deliberately not here. It used to be, and that made a person's
/// files part of the OS image: `make pack` rewrites that image, `make run`
/// packs first, so every boot during development was a reinstall and anything
/// saved was gone. Files are the machine's, not the install's, so `/home` comes
/// off the state volume instead. See [`mount_home`].
const SYSTEM_DIRS: &[&str] = &["bin", "apps", "default_wallpapers", "default_themes"];

/// How long to wait for a volume's device to appear. virtio-blk is built in
/// and there before init runs; the bound keeps a machine without a drive
/// from waiting long on one.
const VOLUME_WAIT: std::time::Duration = std::time::Duration::from_millis(500);

/// Mount the system volume and bind its directories into the root.
///
/// Everything the OS is, beyond PID 1, arrives here: binaries, applications,
/// wallpapers, themes, `/home`. All of it demand-pages from the disk, so
/// code costs memory only while it runs. Failure is loud but not fatal:
/// the supervisor stays up with the console alive and says exactly what is
/// missing, which is the entire reason it does not live on this volume.
pub fn mount_system() {
    if let Err(err) = fs::create_dir_all(SYSTEM_DIR) {
        kerr!("could not create {SYSTEM_DIR}: {err}");
        return;
    }
    let device = kernel_arg("agentware.system").unwrap_or_else(|| SYSTEM_DEVICE.to_owned());

    let deadline = std::time::Instant::now() + VOLUME_WAIT;
    while !Path::new(&device).exists() {
        if std::time::Instant::now() >= deadline {
            kerr!("system: no volume at {device}; nothing beyond the supervisor can run");
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    // Read-write, because /home lives here and files a person saves are
    // theirs to keep.
    if let Err(err) = mount::mount(
        device.as_str(),
        SYSTEM_DIR,
        "ext4",
        MountFlags::NOSUID.union(MountFlags::NODEV),
        None,
    ) {
        kerr!("system: could not mount {device} on {SYSTEM_DIR}: {err}; nothing beyond the supervisor can run");
        return;
    }
    kinfo!("system: mounted {device} on {SYSTEM_DIR}");

    for name in SYSTEM_DIRS {
        let source = format!("{SYSTEM_DIR}/{name}");
        if !Path::new(&source).is_dir() {
            kwarn!("system: the volume has no {name}/");
            continue;
        }
        let target = format!("/{name}");
        if let Err(err) = fs::create_dir_all(&target) {
            kwarn!("system: could not create {target}: {err}");
            continue;
        }
        match mount::mount_bind(&source, &target) {
            Ok(()) => kinfo!("system: {target} is {source}"),
            Err(err) => kwarn!("system: could not bind {source} on {target}: {err}"),
        }
    }
}

/// Where the state volume is mounted: the one directory that outlives the
/// machine, holding `settings.xml` and whatever else earns a place there.
pub const STATE_DIR: &str = "/state";

/// The block device the state volume is expected on: the second virtio
/// drive QEMU is booted with, the first being the root filesystem.
/// `agentware.state=/dev/...` on the kernel command line names another,
/// which is how a partition on real hardware will be reached until the
/// supervisor can find one by label.
const STATE_DEVICE: &str = "/dev/vdb";

/// Mount the state volume, if the machine has one.
///
/// Not fatal without: `/state` is then a directory in the RAM image, settings
/// last until power off, and the log says so. This is the whole of persistence
/// in Agentware: workspaces, applications and conversations still live in
/// RAM and die with the machine, by choice; preferences are the one thing a
/// person expects to find as they left them.
pub fn mount_state() {
    if let Err(err) = fs::create_dir_all(STATE_DIR) {
        kwarn!("could not create {STATE_DIR}: {err}");
        return;
    }
    let device = kernel_arg("agentware.state").unwrap_or_else(|| STATE_DEVICE.to_owned());

    let deadline = std::time::Instant::now() + VOLUME_WAIT;
    while !Path::new(&device).exists() {
        if std::time::Instant::now() >= deadline {
            kwarn!("state: no volume at {device}; settings will not outlive this boot");
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    match mount::mount(
        device.as_str(),
        STATE_DIR,
        "ext4",
        MountFlags::NOSUID.union(MountFlags::NODEV),
        None,
    ) {
        Ok(()) => kinfo!("state: mounted {device} on {STATE_DIR}"),
        Err(err) => kwarn!("state: could not mount {device} on {STATE_DIR}: {err}; settings will not outlive this boot"),
    }
}

/// Where a person's files are: `/home`.
pub const HOME_DIR: &str = "/home";

/// Put `/home` on the state volume, seeding it from the image on a first run.
///
/// **Files are the machine's, not the install's.** `/home` used to be a
/// directory of the system volume, bound over the root like `bin` and `apps`,
/// which meant it was rebuilt every time the OS image was: `make pack` writes
/// that image whole and `make run` packs first, so a file saved in one session
/// was gone by the next boot. That is not a person losing work to a bug, it is
/// the OS reinstalling itself under them, and no amount of care in an
/// application could have survived it.
///
/// So the image's `home/` is now what an installer would call the skeleton: it
/// is copied onto the state volume the first time there is nowhere to copy it
/// to, and never consulted again. A reinstall (`make pack`) leaves a person's
/// files alone. `make cleanstate` is what erases them, which is the gesture
/// that already meant "this machine has never been booted".
///
/// Without a state volume this still binds nothing and leaves the image's
/// files visible read-write in RAM, which is the same graceful degradation
/// settings get: the machine works, and the log says what will not last.
pub fn mount_home() {
    let seed = format!("{SYSTEM_DIR}/home");
    let home = format!("{STATE_DIR}/home");

    // Nothing was mounted on /state, so there is no disk to put files on. The
    // image's own home/ is already bound nowhere; expose it as it is.
    if !is_mount_point(STATE_DIR) {
        if Path::new(&seed).is_dir() {
            let _ = fs::create_dir_all(HOME_DIR);
            match mount::mount_bind(&seed, HOME_DIR) {
                Ok(()) => kwarn!("home: no state volume; {HOME_DIR} is the image's and will not outlive this boot"),
                Err(err) => kwarn!("home: could not bind {seed} on {HOME_DIR}: {err}"),
            }
        }
        return;
    }

    if !Path::new(&home).exists() {
        match copy_tree(Path::new(&seed), Path::new(&home)) {
            Ok(n) => kinfo!("home: first run, copied {n} file(s) from the image"),
            Err(err) => kwarn!("home: could not seed {home} from {seed}: {err}"),
        }
    }
    if let Err(err) = fs::create_dir_all(&home) {
        kwarn!("home: could not create {home}: {err}");
        return;
    }
    if let Err(err) = fs::create_dir_all(HOME_DIR) {
        kwarn!("home: could not create {HOME_DIR}: {err}");
        return;
    }
    match mount::mount_bind(&home, HOME_DIR) {
        Ok(()) => kinfo!("home: {HOME_DIR} is {home}, on the state volume"),
        Err(err) => kwarn!("home: could not bind {home} on {HOME_DIR}: {err}"),
    }
}

/// Whether anything is mounted at a path, by asking whether it and its parent
/// are on the same device. A mount point is the one place they differ.
fn is_mount_point(path: &str) -> bool {
    let Ok(here) = fs::metadata(path) else { return false };
    let Some(parent) = Path::new(path).parent() else { return false };
    let Ok(above) = fs::metadata(parent) else { return false };
    std::os::unix::fs::MetadataExt::dev(&here) != std::os::unix::fs::MetadataExt::dev(&above)
}

/// Copy a directory tree, returning how many files were written.
///
/// Only what a skeleton holds: directories and ordinary files. It runs once, on
/// a machine's first boot, over the handful of files the image ships.
///
/// Every file is synced, and so is the directory holding it. That is not
/// caution, it is the bug this had: `fs::copy` leaves the contents in the page
/// cache, so a machine switched off soon after its first boot came back with
/// `/home` full of files of the right names and zero length. It looked like a
/// broken application, since what noticed was a text editor opening a file and
/// finding nothing in it.
fn copy_tree(from: &Path, to: &Path) -> io::Result<usize> {
    fs::create_dir_all(to)?;
    let mut written = 0;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let target = to.join(entry.file_name());
        if kind.is_dir() {
            written += copy_tree(&entry.path(), &target)?;
        } else if kind.is_file() {
            let bytes = fs::read(entry.path())?;
            let mut file = fs::File::create(&target)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            written += 1;
        }
    }
    fs::File::open(to)?.sync_all()?;
    Ok(written)
}

/// Point the resolver at a nameserver, and say whether there is a network.
///
/// The kernel configures the interface itself from the `ip=` boot argument
/// (CONFIG_IP_PNP), so the supervisor holds no networking code; what the
/// kernel cannot provide is `/etc/resolv.conf`, which is a userspace
/// convention musl's resolver reads. Under QEMU's slirp the DNS proxy is
/// always 10.0.2.3. The file lives on the initramfs root, deliberately: name
/// resolution must not depend on the disk any more than PID 1 does.
///
/// The agent is the only process that talks to the outside; everything else
/// on the machine still speaks only the sockets it was handed at spawn.
pub fn configure_network() {
    match count_matching("/sys/class/net", "eth") {
        Some(n) if n > 0 => kinfo!("net: {n} interface(s); kernel autoconfiguration applies"),
        _ => {
            kwarn!("net: no interface; the agent will not reach a model");
            return;
        }
    }
    if let Err(err) = fs::create_dir_all("/etc") {
        kwarn!("net: could not create /etc: {err}");
        return;
    }
    match fs::write("/etc/resolv.conf", "nameserver 10.0.2.3\n") {
        Ok(()) => kinfo!("net: wrote /etc/resolv.conf (nameserver 10.0.2.3)"),
        Err(err) => kwarn!("net: could not write /etc/resolv.conf: {err}"),
    }
}

/// One `key=value` from the kernel command line.
fn kernel_arg(key: &str) -> Option<String> {
    let cmdline = fs::read_to_string("/proc/cmdline").ok()?;
    cmdline
        .split_whitespace()
        .find_map(|word| word.strip_prefix(key).and_then(|rest| rest.strip_prefix('=')))
        .map(str::to_owned)
}

/// Unmount everything in reverse order, best effort. Called during shutdown.
///
/// `DETACH` is a lazy unmount: it detaches the tree immediately even if
/// something still holds a reference. During shutdown that is exactly what we
/// want, because the alternative is hanging forever on a stuck process. The
/// state volume goes first and gets a proper unmount attempt before the lazy
/// one, since it is the one filesystem whose contents matter afterwards.
pub fn unmount_all() {
    // /home is a bind out of the state volume, so it comes off before the
    // volume under it or the proper unmount below can never succeed and the
    // one filesystem whose contents matter would always get the lazy one.
    let _ = mount::unmount(HOME_DIR, UnmountFlags::DETACH);
    if mount::unmount(STATE_DIR, UnmountFlags::empty()).is_err()
        && let Err(err) = mount::unmount(STATE_DIR, UnmountFlags::DETACH)
    {
        kwarn!("could not unmount {STATE_DIR}: {err}");
    }
    // The binds come off before the volume under them, and the volume gets a
    // proper unmount attempt before the lazy one, because it is a disk whose
    // contents matter afterwards. This is why PID 1 does not live on it:
    // nothing is left running from it by now, so it actually unmounts.
    for name in SYSTEM_DIRS.iter().rev() {
        let _ = mount::unmount(format!("/{name}"), UnmountFlags::DETACH);
    }
    if mount::unmount(SYSTEM_DIR, UnmountFlags::empty()).is_err()
        && let Err(err) = mount::unmount(SYSTEM_DIR, UnmountFlags::DETACH)
    {
        kwarn!("could not unmount {SYSTEM_DIR}: {err}");
    }
    for mp in MOUNTS.iter().rev() {
        if let Err(err) = mount::unmount(mp.target, UnmountFlags::DETACH) {
            kwarn!("could not unmount {}: {err}", mp.target);
        }
    }
}

/// Lift the kernel's rate limit on `/dev/kmsg` writes.
///
/// `printk.devkmsg` defaults to `ratelimit`, which allows roughly ten messages
/// per five seconds per open file and silently discards the rest. Since every
/// supervisor log line is a `/dev/kmsg` write, the default drops exactly the
/// messages that matter most: a burst of service crashes, or the tail of a
/// shutdown. Discovered the hard way, when the eleventh line of a boot and
/// everything after it disappeared.
///
/// Must run after `/proc` is mounted and before [`crate::klog::open`].
pub fn unrestrict_kmsg() {
    if let Err(err) = fs::write("/proc/sys/kernel/printk_devkmsg", "on\n") {
        kwarn!("could not lift the /dev/kmsg rate limit: {err}");
    }
}

/// Set the least severe level that still reaches the console.
///
/// The kernel prints a message only when its level is *strictly less than*
/// `console_loglevel`, the first of the four values in
/// `/proc/sys/kernel/printk`. That off-by-one is easy to get backwards, so this
/// takes the lowest severity that should remain visible and adds the one
/// itself: passing `klog::INFO` shows info and everything more severe.
///
/// Once `haimanager` owns the display this should drop to `klog::WARN`,
/// otherwise kernel messages will scribble over the compositor's framebuffer.
pub fn set_console_loglevel(min_visible: u8) {
    let value = min_visible.saturating_add(1);
    if let Err(err) = fs::write("/proc/sys/kernel/printk", format!("{value}\n")) {
        kwarn!("could not set console loglevel: {err}");
    }
}

/// Stop the kernel from hard-resetting the machine on Ctrl-Alt-Del.
///
/// With this off, the keystroke sends `SIGINT` to PID 1 instead, so the
/// supervisor gets to run an orderly shutdown rather than having the power
/// yanked mid-write.
pub fn disable_ctrl_alt_del() {
    use rustix::system::{RebootCommand, reboot};
    if let Err(err) = reboot(RebootCommand::CadOff) {
        kwarn!("could not disable ctrl-alt-del: {err}");
    }
}

/// Re-enable the kernel's own Ctrl-Alt-Del handling.
///
/// Used by the panic path: if the supervisor has given up, the keystroke is the
/// user's only way to reboot the machine, so hand it back to the kernel.
pub fn enable_ctrl_alt_del() {
    use rustix::system::{RebootCommand, reboot};
    let _ = reboot(RebootCommand::CadOn);
}

pub fn set_hostname(name: &str) {
    if let Err(err) = fs::write("/proc/sys/kernel/hostname", name) {
        kwarn!("could not set hostname: {err}");
    }
}

/// Log what the kernel actually gave us.
///
/// If `/dev/dri/card0` is missing,
/// `haimanager` has nothing to render onto and there is no point going further,
/// so it is worth saying loudly at boot rather than debugging it later through
/// a compositor that will not start.
pub fn boot_report() {
    report_present("/proc/self", "procfs");
    report_present("/sys/class", "sysfs");

    match count_dir_entries("/dev/dri") {
        Some(n) if n > 0 => kinfo!("DRM: /dev/dri present with {n} node(s)"),
        _ => kerr!("DRM: no /dev/dri nodes, haimanager will have no display"),
    }

    match count_matching("/dev/input", "event") {
        Some(n) if n > 0 => kinfo!("input: {n} evdev node(s) under /dev/input"),
        _ => kwarn!("input: no /dev/input/event* nodes, there will be no keyboard or mouse"),
    }
}

fn report_present(path: &str, label: &str) {
    if Path::new(path).exists() {
        kinfo!("{label}: ok");
    } else {
        kerr!("{label}: missing ({path} not found)");
    }
}

fn count_dir_entries(path: &str) -> Option<usize> {
    Some(fs::read_dir(path).ok()?.flatten().count())
}

fn count_matching(path: &str, prefix: &str) -> Option<usize> {
    let count = fs::read_dir(path)
        .ok()?
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(prefix))
        .count();
    Some(count)
}
