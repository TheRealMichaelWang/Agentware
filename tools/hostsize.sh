#!/bin/sh
# Print the host desktop size as "WxH", or nothing if it cannot be learned.
#
# Tried in order:
#   1. xrandr, the ordinary answer on a Linux desktop. Not installed everywhere,
#      which is how the first version of this detection silently failed: the
#      Makefile piped its absence to /dev/null and fell back without saying so.
#   2. WSLg's compositor log, which records the Windows desktop size it was
#      given. There is no X tool guaranteed to exist inside WSL, but this file
#      is written by WSLg itself.
#
# Printing nothing is a legitimate answer; the Makefile falls back and its boot
# line says so.

if command -v xrandr >/dev/null 2>&1; then
    mode=$(xrandr --current 2>/dev/null \
        | sed -n 's/.*current \([0-9]\{3,\}\) x \([0-9]\{3,\}\).*/\1x\2/p' | head -1)
    if [ -n "$mode" ]; then
        echo "$mode"
        exit 0
    fi
fi

if [ -r /mnt/wslg/weston.log ]; then
    mode=$(grep -a 'DesktopWidth' /mnt/wslg/weston.log | tail -1 \
        | sed -n 's/.*DesktopWidth:\([0-9]\+\), DesktopHeight:\([0-9]\+\).*/\1x\2/p')
    if [ -n "$mode" ]; then
        echo "$mode"
        exit 0
    fi
fi

exit 0
