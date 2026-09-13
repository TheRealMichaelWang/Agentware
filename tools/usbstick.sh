#!/bin/bash
# Lay out a USB stick that boots Agentware on real hardware (docs/OnDevicePlan.md,
# Phase 1.5), without root: udisks lets the desktop user partition and format a
# removable drive, and everything else is a copy.
#
#   tools/usbstick.sh /dev/sdX
#
# Wipes the stick. Three partitions: a 512 MiB FAT32 EFI system partition
# holding the kernel as EFI/BOOT/BOOTX64.EFI (the EFI stub boots it with no
# loader, its command line built in: see kernel-usb-src/.config CONFIG_CMDLINE)
# and the initramfs at its root; a 1 GiB ext4 system volume holding the sysroot
# tree; a 64 MiB ext4 state volume, empty, which the supervisor fills on the
# first boot. The built-in command line names them /dev/sda2 and /dev/sda3,
# which holds when the stick is the only USB disk present.
#
# The kernel is the bare-metal one (kernel/agentware-usb.config, built in the
# kernel-usb-src worktree: EFI framebuffer console until amdgpu takes over,
# amdgpu built in, NVMe, UAS, and the command line, which ends `console=tty0`
# so PID 1's stderr is the screen), not the QEMU kernel, and the initramfs
# is the one carrying the GPU's firmware. `make usb` builds all of it.
#
# Secure Boot must be off in the firmware setup: the kernel is unsigned.
set -e
DEV=${1:?usage: tools/usbstick.sh /dev/sdX}
NAME=$(basename "$DEV")
ROOT=$(cd "$(dirname "$0")/.." && pwd)
KERNEL=$ROOT/kernel-usb-src/arch/x86/boot/bzImage
INITRAMFS=$ROOT/initramfs-usb.cpio.gz
[ -f "$KERNEL" ] && [ -f "$INITRAMFS" ] && [ -d "$ROOT/sysroot/bin" ] || { echo "run make usb first" >&2; exit 1; }

call() { gdbus call --system --dest org.freedesktop.UDisks2 --object-path "/org/freedesktop/UDisks2/block_devices/$1" --method "org.freedesktop.UDisks2.$2" "${@:3}" > /dev/null; }
# A stick's partitions are sda1, sda2; a loop device's are loop0p1, loop0p2.
# The loop device is how the layout is tested before a stick is: a raw file
# under `udisksctl loop-setup -f`, laid out by this script, booted under
# OVMF by tools/usbboot.py.
part() { case "$DEV" in *[0-9]) echo "${DEV}p$1";; *) echo "${DEV}$1";; esac; }

for part in $(lsblk -ln -o NAME "$DEV" | tail -n +2); do udisksctl unmount -b "/dev/$part" 2> /dev/null || true; done
call "$NAME" Block.Format gpt '{}'
ESP=c12a7328-f81f-11d2-ba4b-00a0c93ec93b
LINUX=0fc63daf-8483-4772-8e79-3d69d8477de4
call "$NAME" PartitionTable.CreatePartitionAndFormat 1048576   536870912  $ESP   agentware-boot   '{}' vfat "{'label': <'AWBOOT'>}"
call "$NAME" PartitionTable.CreatePartitionAndFormat 538968064 1073741824 $LINUX agentware-system '{}' ext4 "{'label': <'agentware-system'>, 'take-ownership': <true>}"
call "$NAME" PartitionTable.CreatePartitionAndFormat 1612709888 67108864  $LINUX agentware-state  '{}' ext4 "{'label': <'agentware-state'>, 'take-ownership': <true>}"

BOOT=$(udisksctl mount -b "$(part 1)" | sed -n 's/.* at //p')
SYSTEM=$(udisksctl mount -b "$(part 2)" | sed -n 's/.* at //p')
mkdir -p "$BOOT/EFI/BOOT"
cp "$KERNEL" "$BOOT/EFI/BOOT/BOOTX64.EFI"
cp "$INITRAMFS" "$BOOT/initramfs.cpio.gz"
cp -a "$ROOT/sysroot/." "$SYSTEM/"
sync
udisksctl unmount -b "$(part 1)" > /dev/null
udisksctl unmount -b "$(part 2)" > /dev/null
lsblk "$DEV" -o NAME,SIZE,FSTYPE,LABEL
echo "==> $DEV is ready. Turn Secure Boot off in the firmware setup, then boot from it."
