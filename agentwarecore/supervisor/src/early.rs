//! Stage 1 and 2 of boot: bring up the virtual filesystems and put the machine
//! into a known state.
//!
//! Nothing else in the userland can work until this runs. `haimanager` needs
//! `/dev/dri/card0` and `/dev/input/event*`, both of which only appear once
//! devtmpfs is mounted. Note that `CONFIG_DEVTMPFS_MOUNT=y` does *not* help
//! here: the kernel only auto-mounts devtmpfs when booting a real root
//! filesystem, never for an initramfs. We have to do it ourselves.

use std::fs;
use std::io;
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

/// Where the state volume is mounted: the one directory that outlives the
/// machine, holding `settings.xml` and whatever else earns a place there.
pub const STATE_DIR: &str = "/state";

/// The block device the state volume is expected on: the virtio drive QEMU
/// is booted with. `agentware.state=/dev/...` on the kernel command line
/// names another, which is how a partition on real hardware will be reached
/// until the supervisor can find one by label.
const STATE_DEVICE: &str = "/dev/vda";

/// How long to wait for the state device to appear. virtio-blk is built in
/// and there before init runs; the bound keeps a machine without a drive,
/// which the self-test is, from waiting long on one.
const STATE_WAIT: std::time::Duration = std::time::Duration::from_millis(500);

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

    let deadline = std::time::Instant::now() + STATE_WAIT;
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
    if mount::unmount(STATE_DIR, UnmountFlags::empty()).is_err()
        && let Err(err) = mount::unmount(STATE_DIR, UnmountFlags::DETACH)
    {
        kwarn!("could not unmount {STATE_DIR}: {err}");
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
