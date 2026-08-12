# Variables
KERNEL := kernel-build/arch/x86/boot/bzImage
INITRAMFS_ARCHIVE := initramfs.cpio.gz
FS_DIR := initramfs
AW_CORE_DIR := agentwarecore
TARGET := x86_64-unknown-linux-musl
BIN_DIR := $(AW_CORE_DIR)/target/$(TARGET)/release

.PHONY: all build buildcore pack run selftest clean

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
QEMU := qemu-system-x86_64 -enable-kvm -m 4G -cpu host \
	-kernel $(KERNEL) -initrd $(INITRAMFS_ARCHIVE) \
	-device virtio-vga,xres=$(DISPLAY_W),yres=$(DISPLAY_H) \
	-device virtio-tablet-pci -no-reboot

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

# 3. Build All Userland (Core + Future 1st-party apps/tools)
build: buildcore
	@echo "==> All Userland components built successfully."
	# (Future) Add 'buildapps' as a dependency above

# ---------------------------------------------------------
# Packaging & Execution
# ---------------------------------------------------------

# Pack the virtual hard drive (depends on the full 'build')
pack: build
	@echo "==> Packing initramfs..."
	
	# 1. Create the directories the image needs
	mkdir -p $(FS_DIR)/dev $(FS_DIR)/bin
	
	# 2. Create the console device node (Requires sudo)
	@if [ ! -c $(FS_DIR)/dev/console ]; then \
		echo "Creating /dev/console..."; \
		sudo mknod -m 600 $(FS_DIR)/dev/console c 5 1; \
	fi
	
	# 3. Copy the compiled Rust binaries
	cp $(BIN_DIR)/supervisor $(FS_DIR)/init
	cp $(BIN_DIR)/haimanager $(FS_DIR)/bin/haimanager

	# 3a. The reference client for the display protocol. It stands in for both
	# the agentdesk and an application until either exists, which is what gives
	# the compositor something real to render and diff.
	cp $(BIN_DIR)/awapp $(FS_DIR)/bin/awapp
	# The same binary under a second name, because the spawn broker forks an
	# app by name with no arguments, so two names is how there are two apps.
	cp $(BIN_DIR)/awapp $(FS_DIR)/bin/awnotes
	# The per-turn worker, forked on the agentdesk's request.
	cp $(BIN_DIR)/awagent $(FS_DIR)/bin/awagent

	# 3b. Stand-in binaries used by `make selftest` to exercise the service
	# table and the control socket. Harmless to ship; nothing starts them
	# without the selftest flag on the kernel command line.
	#   awtest      a service that exits, crashes, or runs on demand
	#   awstubborn  an app that ignores SIGTERM, to force the cgroup.kill path
	#   awctl       the control socket client
	#   awui        a stand-in compositor that receives passed descriptors
	cp $(BIN_DIR)/awtest $(FS_DIR)/bin/awtest
	cp $(BIN_DIR)/awstubborn $(FS_DIR)/bin/awstubborn
	cp $(BIN_DIR)/awctl $(FS_DIR)/bin/awctl
	cp $(BIN_DIR)/awui $(FS_DIR)/bin/awui
	
	# 4. Pack the filesystem.
	#
	# No sudo needed: cpio records a device node's major/minor from stat, it
	# never opens the device. -R 0:0 makes everything root-owned inside the
	# archive regardless of who ran the build.
	cd $(FS_DIR) && find . -print0 | cpio --null -o --format=newc -R 0:0 --quiet | gzip -9 > ../$(INITRAMFS_ARCHIVE)

# Boot QEMU (depends on 'pack' being finished)
#
# agentware.demo is on the command line because startmenu does not exist yet,
# so without it nothing would ever ask the broker for a workspace and the
# compositor would come up with no clients. It substitutes a stand-in start menu
# and a stand-in agentdesk, and drops out the moment either is written.
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
	rm -f $(INITRAMFS_ARCHIVE)
	rm -f $(FS_DIR)/init