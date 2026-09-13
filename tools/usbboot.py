#!/usr/bin/env python3
"""Boot a USB stick's image the way the Framework boots the stick, and
photograph the screen.

The bare-metal kernel has no serial port to log to, no loader in front of it,
and a built-in command line naming its volumes as /dev/sda2 and /dev/sda3.
Nothing in tools/screenshot.py exercises any of that: it hands QEMU a kernel
and an initramfs directly and reads a serial log. This boots a raw image laid
out by tools/usbstick.sh (on a loop device; see the `part` note there) under
OVMF, as a USB disk, with `-serial none`, and takes a screendump at each of
the moments asked for, because the screen is the only place anything shows.
It is how the abort in Phase 1.5 was reproduced byte for byte, and how every
change to the stick is tried before a stick is written.

Usage: tools/usbboot.py STICK.img OUT-PREFIX --at 8 --at 20 [--gpu]

Writes OUT-PREFIX-<seconds>.png for each --at. The image is booted against a
snapshot, so a run never changes it. --gpu gives the guest virtio-vga as the
Framework's amdgpu stands in for it; without it there is no DRM device at
all, which is the case the supervisor's report is read under.
"""

import argparse
import os
import shutil
import subprocess
import sys
import tempfile
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from screenshot import monitor_command, read_ppm, write_png  # noqa: E402

OVMF_CODE = "/usr/share/OVMF/OVMF_CODE_4M.fd"
OVMF_VARS = "/usr/share/OVMF/OVMF_VARS_4M.fd"


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("image")
    parser.add_argument("prefix")
    parser.add_argument("--at", type=float, action="append", default=[],
                        help="seconds after power-on to take a screendump; repeatable")
    parser.add_argument("--gpu", action="store_true",
                        help="give the guest a virtio-vga, standing in for a real card")
    parser.add_argument("--width", type=int, default=1600)
    parser.add_argument("--height", type=int, default=1000)
    args = parser.parse_args()
    moments = sorted(args.at) or [10.0]

    with tempfile.TemporaryDirectory() as work:
        # OVMF wants its variable store writable; a private copy keeps the
        # system's pristine and the boot order forgotten between runs.
        vars_path = os.path.join(work, "OVMF_VARS.fd")
        shutil.copy(OVMF_VARS, vars_path)
        monitor_path = os.path.join(work, "monitor")
        video = ["-device", "virtio-vga,xres=%d,yres=%d" % (args.width, args.height)] \
            if args.gpu else ["-vga", "std"]
        guest = subprocess.Popen(
            [
                "qemu-system-x86_64", "-enable-kvm", "-m", "4G", "-cpu", "host",
                "-drive", "if=pflash,format=raw,readonly=on,file=" + OVMF_CODE,
                "-drive", "if=pflash,format=raw,file=" + vars_path,
                # The image as a USB disk on an xHCI controller, which is what
                # the Framework's stick is: the kernel names it sda, and the
                # built-in command line counts on that.
                "-device", "qemu-xhci",
                "-drive", "file=%s,if=none,id=stick,format=raw,snapshot=on" % args.image,
                "-device", "usb-storage,drive=stick",
                "-device", "usb-kbd", "-device", "usb-mouse",
                *video,
                "-serial", "none",
                "-no-reboot",
                "-display", "none",
                "-monitor", "unix:%s,server,nowait" % monitor_path,
            ],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.STDOUT,
        )
        started = time.time()
        try:
            for moment in moments:
                remaining = started + moment - time.time()
                if remaining > 0:
                    time.sleep(remaining)
                if guest.poll() is not None:
                    print("qemu exited (status %s) before %gs" % (guest.returncode, moment),
                          file=sys.stderr)
                    return 1
                ppm_path = os.path.join(work, "shot.ppm")
                reply = monitor_command(monitor_path, "screendump %s" % ppm_path)
                if not os.path.exists(ppm_path):
                    print("no screendump at %gs. monitor said: %s" % (moment, reply.strip()),
                          file=sys.stderr)
                    return 1
                width, height, rgb = read_ppm(ppm_path)
                out = "%s-%g.png" % (args.prefix, moment)
                write_png(out, width, height, rgb)
                os.unlink(ppm_path)
                print("captured %dx%d at %gs -> %s" % (width, height, moment, out))
            return 0
        finally:
            guest.kill()
            guest.wait()


if __name__ == "__main__":
    sys.exit(main())
