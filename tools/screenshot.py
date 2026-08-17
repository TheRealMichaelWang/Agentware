#!/usr/bin/env python3
"""Boot Agentware in QEMU, capture the screen, and write a PNG.

Graphics work needs to be looked at, and a serial log cannot show whether the
picture is right. This drives QEMU's monitor to take a `screendump`, which
produces a PPM, and converts it to PNG so it can be viewed directly.

Usage: tools/screenshot.py OUT.png [--seconds N] [--append "KERNEL CMDLINE"]
"""

import argparse
import json
import os
import socket
import struct
import subprocess
import sys
import tempfile
import time
import zlib

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
KERNEL = os.path.join(ROOT, "kernel-build/arch/x86/boot/bzImage")
INITRAMFS = os.path.join(ROOT, "initramfs.cpio.gz")


def qemu(monitor_path, qmp_path, serial_path, append, width, height, state, keep_state):
    # The state volume. By default a capture boots against a throwaway copy of
    # the machine's state (QEMU's snapshot mode writes to a temp file), so a
    # test never changes what `make run` sees; --keep-state boots against the
    # image for real, which is how persistence across boots is checked. A
    # missing image means no drive, and the guest says so at boot.
    drive = []
    if state and os.path.exists(state):
        drive = ["-drive", "file=%s,if=virtio,format=raw%s" % (state, "" if keep_state else ",snapshot=on")]
    return subprocess.Popen(
        [
            "qemu-system-x86_64", "-enable-kvm", "-m", "4G", "-cpu", "host",
            "-kernel", KERNEL, "-initrd", INITRAMFS,
            *drive,
            # virtio-vga's preferred mode is the one the compositor picks, so
            # these two numbers decide the whole guest display.
            "-device", "virtio-vga,xres=%d,yres=%d" % (width, height),
            # The same absolute pointing device the interactive window has, so
            # what the tool exercises is what the human uses.
            "-device", "virtio-tablet-pci",
            "-no-reboot",
            "-display", "none",
            "-serial", "file:" + serial_path,
            "-monitor", "unix:%s,server,nowait" % monitor_path,
            "-qmp", "unix:%s,server,nowait" % qmp_path,
            "-append", append,
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.STDOUT,
    )


def monitor_command(path, command, deadline=20.0):
    """Send one command to the QEMU monitor and return what it says back."""
    started = time.time()
    while True:
        try:
            sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            sock.settimeout(5.0)
            sock.connect(path)
            break
        except (FileNotFoundError, ConnectionRefusedError):
            if time.time() - started > deadline:
                raise
            time.sleep(0.1)

    with sock:
        time.sleep(0.3)               # let the banner arrive
        try:
            sock.recv(65536)
        except socket.timeout:
            pass
        sock.sendall((command + "\n").encode())
        time.sleep(0.8)               # let the command run
        try:
            return sock.recv(65536).decode(errors="replace")
        except socket.timeout:
            return ""


def qmp_tablet(path, fx, fy, click):
    """Move the absolute tablet to a screen fraction, optionally clicking.

    The human monitor only speaks relative `mouse_move`, which drives the PS/2
    mouse. The tablet is driven over QMP with input-send-event, which is the
    same path a real pointer takes through QEMU.
    """
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.settimeout(5.0)
    sock.connect(path)
    with sock:
        handle = sock.makefile("rw")
        json.loads(handle.readline())              # greeting
        def execute(command, arguments=None):
            request = {"execute": command}
            if arguments:
                request["arguments"] = arguments
            handle.write(json.dumps(request) + "\n")
            handle.flush()
            while True:
                reply = json.loads(handle.readline())
                if "return" in reply or "error" in reply:
                    return reply
        execute("qmp_capabilities")
        events = [
            {"type": "abs", "data": {"axis": "x", "value": int(fx * 32767)}},
            {"type": "abs", "data": {"axis": "y", "value": int(fy * 32767)}},
        ]
        execute("input-send-event", {"events": events})
        if click:
            time.sleep(0.2)
            for down in (True, False):
                execute("input-send-event", {"events": [
                    {"type": "btn", "data": {"down": down, "button": "left"}}]})
                time.sleep(0.1)


def read_ppm(path):
    """Parse a binary PPM (P6). QEMU writes nothing else."""
    with open(path, "rb") as handle:
        data = handle.read()

    fields = []
    offset = 0
    while len(fields) < 4:
        while offset < len(data) and data[offset : offset + 1].isspace():
            offset += 1
        if data[offset : offset + 1] == b"#":
            while data[offset : offset + 1] not in (b"\n", b""):
                offset += 1
            continue
        start = offset
        while offset < len(data) and not data[offset : offset + 1].isspace():
            offset += 1
        fields.append(data[start:offset])

    magic, width, height, maxval = fields
    if magic != b"P6":
        raise ValueError("expected a P6 PPM, got %r" % magic)
    if int(maxval) != 255:
        raise ValueError("only 8-bit PPMs are supported")

    offset += 1  # the single whitespace byte after maxval
    width, height = int(width), int(height)
    return width, height, data[offset : offset + width * height * 3]


def write_png(path, width, height, rgb):
    """Minimal PNG encoder: one IHDR, one IDAT, one IEND, no filtering."""

    def chunk(kind, payload):
        body = kind + payload
        return (
            struct.pack(">I", len(payload))
            + body
            + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)
        )

    raw = bytearray()
    for y in range(height):
        raw.append(0)  # filter type 0, none
        raw += rgb[y * width * 3 : (y + 1) * width * 3]

    header = struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)
    with open(path, "wb") as handle:
        handle.write(b"\x89PNG\r\n\x1a\n")
        handle.write(chunk(b"IHDR", header))
        handle.write(chunk(b"IDAT", zlib.compress(bytes(raw), 6)))
        handle.write(chunk(b"IEND", b""))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("output")
    parser.add_argument("--seconds", type=float, default=6.0,
                        help="how long to let the guest boot before capturing")
    parser.add_argument("--append", default="console=ttyS0,115200",
                        help="kernel command line")
    parser.add_argument("--serial", default=None,
                        help="where to write the serial log")
    parser.add_argument("--width", type=int, default=1600,
                        help="guest display width")
    parser.add_argument("--height", type=int, default=1000,
                        help="guest display height")
    parser.add_argument("--state", default=os.path.join(ROOT, "state.img"),
                        help="the state volume image (default: state.img in the repo)")
    parser.add_argument("--keep-state", action="store_true",
                        help="write to the state image for real instead of a "
                             "throwaway snapshot; for checking persistence")
    parser.add_argument("--do", action="append", default=[], metavar="CMD",
                        help="a QEMU monitor command to run before capturing, "
                             "repeatable. e.g. --do 'sendkey a' "
                             "--do 'mouse_move 100 50' --do 'mouse_button 1'. "
                             "The wheel is the third argument to mouse_move, "
                             "as in 'mouse_move 0 0 -1'; mouse_button's wheel "
                             "bits do not reach a PS/2 guest.")
    args = parser.parse_args()

    workdir = tempfile.mkdtemp(prefix="agentware-shot-")
    monitor_path = os.path.join(workdir, "monitor.sock")
    qmp_path = os.path.join(workdir, "qmp.sock")
    ppm_path = os.path.join(workdir, "screen.ppm")
    serial_path = args.serial or os.path.join(workdir, "serial.log")

    guest = qemu(monitor_path, qmp_path, serial_path, args.append, args.width, args.height,
                 args.state, args.keep_state)
    try:
        time.sleep(args.seconds)
        if guest.poll() is not None:
            print("qemu exited early (status %s)" % guest.returncode, file=sys.stderr)

        # Injected input goes through the same monitor connection, so it is
        # ordered against the capture rather than racing it. Commands starting
        # with "abs" drive the tablet instead: "abs 0.5 0.9" points at a screen
        # fraction, "abs 0.5 0.9 click" also clicks there.
        for command in args.do:
            # "sleep N" waits between injected inputs, for gestures that need
            # the guest to catch up: an agent turn, an app being forked.
            if command.startswith("sleep "):
                time.sleep(float(command.split()[1]))
                continue
            if command.startswith("abs "):
                parts = command.split()
                qmp_tablet(qmp_path, float(parts[1]), float(parts[2]),
                           len(parts) > 3 and parts[3] == "click")
                time.sleep(0.3)
            else:
                monitor_command(monitor_path, command)
        if args.do:
            time.sleep(0.5)   # let the guest react before the shutter

        reply = monitor_command(monitor_path, "screendump %s" % ppm_path)
        if not os.path.exists(ppm_path):
            print("no screendump produced. monitor said: %s" % reply.strip(), file=sys.stderr)
            return 1

        width, height, rgb = read_ppm(ppm_path)
        write_png(args.output, width, height, rgb)
        print("captured %dx%d -> %s" % (width, height, args.output))

        if os.path.exists(serial_path):
            print("--- serial tail ---")
            with open(serial_path, errors="replace") as handle:
                for line in handle.read().splitlines()[-25:]:
                    print(line)
        return 0
    finally:
        guest.kill()
        guest.wait()


if __name__ == "__main__":
    sys.exit(main())
