# Variables
KERNEL := kernel-build/arch/x86/boot/bzImage
INITRAMFS_ARCHIVE := initramfs.cpio.gz
FS_DIR := initramfs
AW_CORE_DIR := agentwarecore
TARGET := x86_64-unknown-linux-musl

.PHONY: all build buildcore buildsupervisor pack run clean

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
	
	# 3. Copy the compiled Rust binary
	cp $(AW_CORE_DIR)/supervisor/target/$(TARGET)/release/supervisor $(FS_DIR)/init
	
	# 4. Pack the filesystem (Requires sudo to read the device node)
	cd $(FS_DIR) && sudo find . -print0 | sudo cpio --null -o --format=newc | gzip -9 > ../$(INITRAMFS_ARCHIVE)
	
	# 5. Give ownership of the final image back to your user
	sudo chown $(USER):$(USER) $(INITRAMFS_ARCHIVE)

# Boot QEMU (depends on 'pack' being finished)
run: pack
	@echo "==> Booting Agentware in QEMU..."
	qemu-system-x86_64 -enable-kvm -m 4G \
		-cpu host \
		-kernel $(KERNEL) \
		-initrd $(INITRAMFS_ARCHIVE) \
		-device virtio-vga \
		-display gtk \
		-serial stdio \
		-append "console=tty0 console=ttyS0,115200"
# ---------------------------------------------------------
# Utilities
# ---------------------------------------------------------
clean:
	@echo "==> Cleaning build artifacts..."
	cd $(AW_CORE_DIR)/supervisor && cargo clean
	rm -f $(INITRAMFS_ARCHIVE)
	rm -f $(FS_DIR)/init