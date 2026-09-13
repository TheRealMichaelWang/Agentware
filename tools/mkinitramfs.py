#!/usr/bin/env python3
"""Write the boot initramfs: the supervisor, a console, and on real hardware
the GPU's firmware. Nothing else.

The initramfs holds exactly what must exist before any disk does: /init (the
supervisor, PID 1) and the /dev/console node its early messages go to. The
kernel does not auto-mount devtmpfs for an initramfs, so the node has to be
in the archive; a device node in an archive is just a header saying 5:1, so
writing one needs no privileges, which is the whole reason this script exists
instead of `find | cpio` over a staging tree that would have needed sudo to
hold the node.

Firmware is the one other thing that belongs here, and only on a machine
with a GPU. amdgpu is built into the bare-metal kernel, there is no module
loader, and the driver asks for its firmware while it probes, before the
supervisor has mounted anything; the kernel looks under /lib/firmware on
the root it has at that moment, which is this archive. The files come from
the linux-firmware submodule, named as the driver names them
(`amdgpu/gc_11_5_1_pfp.bin`), and the QEMU initramfs carries none, because
virtio-gpu wants none and every byte here is unpacked into RAM at boot.

Usage: tools/mkinitramfs.py SUPERVISOR_BINARY OUT.cpio.gz
                            [--firmware-tree DIR --firmware NAME...]
"""

import argparse
import gzip
import os
import sys


def entry(name, mode, body=b"", rdev=(0, 0), nlink=1):
    """One newc-format cpio record."""
    header = "070701" + "".join(
        "%08x" % field
        for field in (
            0,              # ino
            mode,
            0, 0,           # uid, gid: root
            nlink,
            0,              # mtime
            len(body),
            0, 0,           # dev
            rdev[0], rdev[1],
            len(name) + 1,
            0,              # check
        )
    )
    out = header.encode() + name.encode() + b"\0"
    out += b"\0" * (-len(out) % 4)
    out += body
    out += b"\0" * (-len(body) % 4)
    return out


def firmware_entries(tree, names):
    """The records for each firmware file, with the directories above it.

    The kernel's unpacker creates nothing it is not told to: a file whose
    parent directory has no record of its own is silently dropped, so every
    directory on the way down gets one, once.
    """
    records = []
    made = set()
    for name in names:
        parts = ("lib", "firmware", *name.split("/"))
        for depth in range(1, len(parts)):
            directory = "/".join(parts[:depth])
            if directory not in made:
                made.add(directory)
                records.append(entry(directory, 0o040755, nlink=2))
        with open(os.path.join(tree, name), "rb") as source:
            records.append(entry("/".join(parts), 0o100644, body=source.read()))
    return records


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("supervisor")
    parser.add_argument("out")
    parser.add_argument("--firmware-tree", metavar="DIR",
                        help="the linux-firmware checkout firmware names are relative to")
    parser.add_argument("--firmware", metavar="NAME", nargs="*", default=[],
                        help="a file under the tree, as the driver requests it")
    args = parser.parse_args()
    if args.firmware and not args.firmware_tree:
        parser.error("--firmware needs --firmware-tree")

    with open(args.supervisor, "rb") as source:
        supervisor = source.read()

    archive = b"".join([
        entry("dev", 0o040755, nlink=2),
        entry("dev/console", 0o020600, rdev=(5, 1)),
        entry("init", 0o100755, body=supervisor),
        *firmware_entries(args.firmware_tree, args.firmware),
        entry("TRAILER!!!", 0),
    ])
    with gzip.open(args.out, "wb", compresslevel=9) as out:
        out.write(archive)
    return 0


if __name__ == "__main__":
    sys.exit(main())
