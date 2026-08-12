# Variables
KERNEL := kernel-build/arch/x86/boot/bzImage
INITRAMFS_ARCHIVE := initramfs.cpio.gz
FS_DIR := initramfs
AW_CORE_DIR := agentwarecore
TARGET := x86_64-unknown-linux-musl

.PHONY: all build buildcore buildsupervisor pack run selftest clean

# Shared QEMU invocation. virtio-vga is what gives the guest /dev/dri/card0,
# which haimanager will render onto via DRM/KMS.
QEMU := qemu-system-x86_64 -enable-kvm -m 4G -cpu host \
	-kernel $(KERNEL) -initrd $(INITRAMFS_ARCHIVE) \
	-device virtio-vga -no-reboot

# ---------------------------------------------------------
# Default Target
# ---------------------------------------------------------
all: run

# ---------------------------------------------------------
# Build Targets
# ---------------------------------------------------------

# 1. Build the PID 1 Supervisor
buildsupervisor:
	@echo "==> Building Supervisor..."
	cd $(AW_CORE_DIR)/supervisor && cargo build --release --target $(TARGET)

# 2. Build Agentware Core (Supervisor + Future Display Server/Agent Harnesses)
buildcore: buildsupervisor
	@echo "==> Agentware Core build complete."
	# (Future) Add 'builddisplayserver' as a dependency above

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
	
	# 1. Create the /dev directory
	mkdir -p $(FS_DIR)/dev
	
	# 2. Create the console device node (Requires sudo)
	@if [ ! -c $(FS_DIR)/dev/console ]; then \
		echo "Creating /dev/console..."; \
		sudo mknod -m 600 $(FS_DIR)/dev/console c 5 1; \
	fi
	
	# 3. Copy the compiled Rust binaries
	cp $(AW_CORE_DIR)/supervisor/target/$(TARGET)/release/supervisor $(FS_DIR)/init

	# 3b. Stand-in binaries used by `make selftest` to exercise the service
	# table and the control socket. Harmless to ship; nothing starts them
	# without the selftest flag on the kernel command line.
	#   awtest      a service that exits, crashes, or runs on demand
	#   awstubborn  an app that ignores SIGTERM, to force the cgroup.kill path
	#   awctl       the control socket client
	#   awui        a stand-in compositor that receives passed descriptors
	mkdir -p $(FS_DIR)/bin
	cp $(AW_CORE_DIR)/supervisor/target/$(TARGET)/release/awtest $(FS_DIR)/bin/awtest
	cp $(AW_CORE_DIR)/supervisor/target/$(TARGET)/release/awstubborn $(FS_DIR)/bin/awstubborn
	cp $(AW_CORE_DIR)/supervisor/target/$(TARGET)/release/awctl $(FS_DIR)/bin/awctl
	cp $(AW_CORE_DIR)/supervisor/target/$(TARGET)/release/awui $(FS_DIR)/bin/awui
	
	# 4. Pack the filesystem.
	#
	# No sudo needed: cpio records a device node's major/minor from stat, it
	# never opens the device. -R 0:0 makes everything root-owned inside the
	# archive regardless of who ran the build.
	cd $(FS_DIR) && find . -print0 | cpio --null -o --format=newc -R 0:0 --quiet | gzip -9 > ../$(INITRAMFS_ARCHIVE)

# Boot QEMU (depends on 'pack' being finished)
run: pack
	@echo "==> Booting Agentware in QEMU..."
	$(QEMU) -display gtk -serial stdio \
		-append "console=tty0 console=ttyS0,115200"

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
	cd $(AW_CORE_DIR)/supervisor && cargo clean
	rm -f $(INITRAMFS_ARCHIVE)
	rm -f $(FS_DIR)/init