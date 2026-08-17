# Variables
KERNEL := kernel-build/arch/x86/boot/bzImage
INITRAMFS_ARCHIVE := initramfs.cpio.gz
FS_DIR := initramfs
AW_CORE_DIR := agentwarecore
AW_APPS_DIR := agentwareapps
TARGET := x86_64-unknown-linux-musl
BIN_DIR := $(AW_CORE_DIR)/target/$(TARGET)/release
APPS_BIN_DIR := $(AW_APPS_DIR)/target/$(TARGET)/release

.PHONY: all build buildcore buildapps pack run selftest clean

# Guest display size. virtio-vga defaults to 1280x800 and the compositor takes
# the driver's preferred mode, so these two numbers are the whole of it.
#
# The virtual monitor is deliberately larger than the reported host desktop:
# 2560x1440, a custom monitor QEMU invents. Sizing to the host was tried twice
# and produced a window the host desktop then shrank or matched, which reads as
# "nothing changed". Instead the guest renders a large screen, the compositor
# scales its interface up to match (1.5x at 1440 rows), and the window opens
# fullscreen with zoom-to-fit so whatever the host really has is filled edge to
# edge. Ctrl+Alt+F leaves fullscreen.
#
# Override per run: make run DISPLAY_W=3840 DISPLAY_H=2160
HOST_PX := $(shell tools/hostsize.sh)
DISPLAY_W ?= 2560
DISPLAY_H ?= 1440

# Shared QEMU invocation. virtio-vga is what gives the guest /dev/dri/card0,
# which haimanager will render onto via DRM/KMS.
# virtio-tablet is what aligns the host mouse with the guest cursor. A PS/2
# mouse is relative, so QEMU can only stream deltas and the two pointers drift
# apart the moment the window is scaled; a tablet is absolute, so the host hands
# over the position itself. The PS/2 devices stay, because the monitor's
# injected input and the wheel arrive through them.
# The guest clock is set from the host's local time rather than UTC. The
# taskbar shows the kernel's idea of the time as it stands, because there is
# no timezone database in the image and no setting for one yet; on the host's
# local time the clock in the corner reads the same as the one on the host.
QEMU := qemu-system-x86_64 -enable-kvm -m 4G -cpu host \
	-kernel $(KERNEL) -initrd $(INITRAMFS_ARCHIVE) \
	-device virtio-vga,xres=$(DISPLAY_W),yres=$(DISPLAY_H) \
	-device virtio-tablet-pci -rtc base=localtime -no-reboot

# ---------------------------------------------------------
# Default Target
# ---------------------------------------------------------
all: run

# ---------------------------------------------------------
# Build Targets
# ---------------------------------------------------------

# 1. Build every crate in the agentwarecore workspace: awproto, the supervisor
# and its stand-in binaries, and the haimanager.
buildcore:
	@echo "==> Building Agentware Core..."
	cd $(AW_CORE_DIR) && cargo build --release --target $(TARGET)

# 2. Build the first-party applications. A separate workspace because apps are
# clients of the display protocol, not parts of the system.
buildapps:
	@echo "==> Building Agentware Apps..."
	cd $(AW_APPS_DIR) && cargo build --release --target $(TARGET)

# 3. Build All Userland
build: buildcore buildapps
	@echo "==> All Userland components built successfully."

# ---------------------------------------------------------
# Packaging & Execution
# ---------------------------------------------------------

# Pack the virtual hard drive (depends on the full 'build')
pack: build
	@echo "==> Packing initramfs..."
	
	# 1. Create the directories the image needs
	mkdir -p $(FS_DIR)/dev $(FS_DIR)/bin $(FS_DIR)/apps
	
	# 2. Create the console device node (Requires sudo)
	@if [ ! -c $(FS_DIR)/dev/console ]; then \
		echo "Creating /dev/console..."; \
		sudo mknod -m 600 $(FS_DIR)/dev/console c 5 1; \
	fi
	
	# 3. Copy the compiled Rust binaries
	cp $(BIN_DIR)/supervisor $(FS_DIR)/init
	cp $(BIN_DIR)/haimanager $(FS_DIR)/bin/haimanager

	# 3a. The workspace process, one per agentdesk, forked by PID 1 when a
	# workspace is created. The stand-in it replaced is removed so a stale
	# image cannot boot it.
	rm -f $(FS_DIR)/bin/awapp
	cp $(BIN_DIR)/agentdesk $(FS_DIR)/bin/agentdesk
	# The per-turn worker, forked on the agentdesk's request. A scripted
	# stand-in until an agent with a model behind it exists.
	cp $(BIN_DIR)/awagent $(FS_DIR)/bin/awagent

	# 3b. Applications. An app is a folder, not a binary: /apps/<name>/ holds
	# exec, icon.svg, description.txt and name.txt, and the broker forks
	# /apps/<name>/exec. Binaries stale-shipped under the old layout are
	# removed so the image cannot boot a copy the build no longer produces.
	# The settings app is built from agentwarecore because what it edits is
	# system state; the rest come from agentwareapps.
	rm -f $(FS_DIR)/bin/awnotes $(FS_DIR)/bin/awcalc
	mkdir -p $(FS_DIR)/apps/awcalc $(FS_DIR)/apps/awfiles $(FS_DIR)/apps/awsettings
	cp $(APPS_BIN_DIR)/awcalc $(FS_DIR)/apps/awcalc/exec
	cp $(AW_APPS_DIR)/awcalc/icon.svg $(AW_APPS_DIR)/awcalc/description.txt $(AW_APPS_DIR)/awcalc/name.txt $(FS_DIR)/apps/awcalc/
	cp $(APPS_BIN_DIR)/awfiles $(FS_DIR)/apps/awfiles/exec
	cp $(AW_APPS_DIR)/awfiles/icon.svg $(AW_APPS_DIR)/awfiles/description.txt $(AW_APPS_DIR)/awfiles/name.txt $(FS_DIR)/apps/awfiles/
	cp $(BIN_DIR)/awsettings $(FS_DIR)/apps/awsettings/exec
	cp $(AW_CORE_DIR)/awsettings/icon.svg $(AW_CORE_DIR)/awsettings/description.txt $(AW_CORE_DIR)/awsettings/name.txt $(FS_DIR)/apps/awsettings/

	# 3b'. Wallpapers. Read by the compositor when a workspace names one, and
	# listed by the settings app; SVG so one file serves every display size.
	rm -rf $(FS_DIR)/wallpapers
	mkdir -p $(FS_DIR)/wallpapers
	cp wallpapers/*.svg $(FS_DIR)/wallpapers/

	# 3b''. /home, where the file browser opens, with a few files to find.
	# In RAM like everything else: what is made there lasts until power off.
	rm -rf $(FS_DIR)/home
	cp -r home $(FS_DIR)/home

	# 3c. Stand-in binaries used by `make selftest` to exercise the service
	# table and the control socket. Harmless to ship; nothing starts them
	# without the selftest flag on the kernel command line.
	#   awtest      a service that exits, crashes, or runs on demand
	#   awstubborn  an app that ignores SIGTERM, to force the cgroup.kill path
	#   awctl       the control socket client
	#   awui        a stand-in compositor that receives passed descriptors
	# awtest doubles as the selftest's desk and agent stand-in from /bin, and
	# both it and awstubborn are also staged as app packages so the selftest
	# drives the same /apps/<name>/exec spawn path the real system uses.
	cp $(BIN_DIR)/awtest $(FS_DIR)/bin/awtest
	cp $(BIN_DIR)/awctl $(FS_DIR)/bin/awctl
	cp $(BIN_DIR)/awui $(FS_DIR)/bin/awui
	rm -f $(FS_DIR)/bin/awstubborn
	mkdir -p $(FS_DIR)/apps/awtest $(FS_DIR)/apps/awstubborn
	cp $(BIN_DIR)/awtest $(FS_DIR)/apps/awtest/exec
	cp $(BIN_DIR)/awstubborn $(FS_DIR)/apps/awstubborn/exec
	
	# 4. Pack the filesystem.
	#
	# No sudo needed: cpio records a device node's major/minor from stat, it
	# never opens the device. -R 0:0 makes everything root-owned inside the
	# archive regardless of who ran the build.
	cd $(FS_DIR) && find . -print0 | cpio --null -o --format=newc -R 0:0 --quiet | gzip -9 > ../$(INITRAMFS_ARCHIVE)

# Boot QEMU (depends on 'pack' being finished)
#
# agentware.demo is on the command line because no agent with a model behind
# it exists yet: it substitutes a scripted one, so a message sent from a
# workspace runs a turn on the calculator. Without it the same message is
# answered with an error. Either way the machine boots to one blank agentdesk.
run: pack
	@echo "==> Booting Agentware fullscreen: guest $(DISPLAY_W)x$(DISPLAY_H), host desktop $(if $(HOST_PX),$(HOST_PX),unknown). Ctrl+Alt+F to un-fullscreen."
	$(QEMU) -display gtk,zoom-to-fit=on,full-screen=on,show-cursor=off -serial stdio \
		-append "console=tty0 console=ttyS0,115200 agentware.demo"

# Headless boot that exercises the supervisor end to end and powers itself off.
#
# Passing agentware.selftest makes the supervisor spawn a throwaway child, reap
# it, and run a full shutdown. QEMU exiting on its own is the pass signal; if it
# hangs, something in that chain is broken.
selftest: pack
	@echo "==> Running supervisor selftest (headless)..."
	$(QEMU) -display none -serial stdio \
		-append "console=ttyS0,115200 agentware.selftest"
# ---------------------------------------------------------
# Utilities
# ---------------------------------------------------------
clean:
	@echo "==> Cleaning build artifacts..."
	cd $(AW_CORE_DIR) && cargo clean
	cd $(AW_APPS_DIR) && cargo clean
	rm -f $(INITRAMFS_ARCHIVE)
	rm -f $(FS_DIR)/init