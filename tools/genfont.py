#!/usr/bin/env python3
"""Generate the haimanager's bitmap font from ASCII art.

The font is written here as pictures and emitted as binary literals, because a
glyph is far easier to get right, and to review, when it looks like the letter
it draws. The generated Rust keeps that property: `0b01110` still reads as a row
of pixels.

Clean-room on purpose. The kernel's bitmap fonts are GPL-2.0 and the only fonts
installed on a typical build host are TrueType, which would mean a rasterizer
dependency inside the process that owns the screen.

Usage: tools/genfont.py > agentwarecore/haimanager/src/paint/font.rs
"""

import sys

# 5 wide, 7 tall. Indexed from 0x20; every printable ASCII character through
# 0x7E must be present and in order.
GLYPHS = r"""
 .....|.....|.....|.....|.....|.....|.....
!..#..|..#..|..#..|..#..|..#..|.....|..#..
"..#.#|..#.#|.....|.....|.....|.....|.....
#.#.#.|.#.#.|#####|.#.#.|#####|.#.#.|.#.#.
$...#.|.####|#.#..|.###.|..#.#|####.|..#..
%##...|##..#|...#.|..#..|.#...|#..##|...##
&.##..|#..#.|#.#..|.#...|#.#.#|#..#.|.##.#
'..#..|..#..|.....|.....|.....|.....|.....
(...#.|..#..|.#...|.#...|.#...|..#..|...#.
).#...|..#..|...#.|...#.|...#.|..#..|.#...
*.....|..#..|#.#.#|.###.|#.#.#|..#..|.....
+.....|..#..|..#..|#####|..#..|..#..|.....
,.....|.....|.....|.....|..##.|..#..|.#...
-.....|.....|.....|#####|.....|.....|.....
......|.....|.....|.....|.....|.##..|.##..
/....#|...#.|...#.|..#..|.#...|.#...|#....
0.###.|#...#|#..##|#.#.#|##..#|#...#|.###.
1..#..|.##..|..#..|..#..|..#..|..#..|.###.
2.###.|#...#|....#|...#.|..#..|.#...|#####
3#####|...#.|..#..|...#.|....#|#...#|.###.
4...#.|..##.|.#.#.|#..#.|#####|...#.|...#.
5#####|#....|####.|....#|....#|#...#|.###.
6..##.|.#...|#....|####.|#...#|#...#|.###.
7#####|....#|...#.|..#..|.#...|.#...|.#...
8.###.|#...#|#...#|.###.|#...#|#...#|.###.
9.###.|#...#|#...#|.####|....#|...#.|.##..
:.....|.##..|.##..|.....|.##..|.##..|.....
;.....|.##..|.##..|.....|.##..|..#..|.#...
<...#.|..#..|.#...|#....|.#...|..#..|...#.
=.....|.....|#####|.....|#####|.....|.....
>.#...|..#..|...#.|....#|...#.|..#..|.#...
?.###.|#...#|....#|...#.|..#..|.....|..#..
@.###.|#...#|#.###|#.#.#|#.###|#....|.###.
A.###.|#...#|#...#|#####|#...#|#...#|#...#
B####.|#...#|#...#|####.|#...#|#...#|####.
C.###.|#...#|#....|#....|#....|#...#|.###.
D###..|#..#.|#...#|#...#|#...#|#..#.|###..
E#####|#....|#....|####.|#....|#....|#####
F#####|#....|#....|####.|#....|#....|#....
G.###.|#...#|#....|#.###|#...#|#...#|.####
H#...#|#...#|#...#|#####|#...#|#...#|#...#
I.###.|..#..|..#..|..#..|..#..|..#..|.###.
J..###|...#.|...#.|...#.|...#.|#..#.|.##..
K#...#|#..#.|#.#..|##...|#.#..|#..#.|#...#
L#....|#....|#....|#....|#....|#....|#####
M#...#|##.##|#.#.#|#.#.#|#...#|#...#|#...#
N#...#|#...#|##..#|#.#.#|#..##|#...#|#...#
O.###.|#...#|#...#|#...#|#...#|#...#|.###.
P####.|#...#|#...#|####.|#....|#....|#....
Q.###.|#...#|#...#|#...#|#.#.#|#..#.|.##.#
R####.|#...#|#...#|####.|#.#..|#..#.|#...#
S.####|#....|#....|.###.|....#|....#|####.
T#####|..#..|..#..|..#..|..#..|..#..|..#..
U#...#|#...#|#...#|#...#|#...#|#...#|.###.
V#...#|#...#|#...#|#...#|#...#|.#.#.|..#..
W#...#|#...#|#...#|#.#.#|#.#.#|##.##|#...#
X#...#|#...#|.#.#.|..#..|.#.#.|#...#|#...#
Y#...#|#...#|.#.#.|..#..|..#..|..#..|..#..
Z#####|....#|...#.|..#..|.#...|#....|#####
[.###.|.#...|.#...|.#...|.#...|.#...|.###.
\#....|.#...|.#...|..#..|...#.|...#.|....#
].###.|...#.|...#.|...#.|...#.|...#.|.###.
^..#..|.#.#.|#...#|.....|.....|.....|.....
_.....|.....|.....|.....|.....|.....|#####
`.#...|..#..|.....|.....|.....|.....|.....
a.....|.....|.###.|....#|.####|#...#|.####
b#....|#....|####.|#...#|#...#|#...#|####.
c.....|.....|.###.|#....|#....|#...#|.###.
d....#|....#|.####|#...#|#...#|#...#|.####
e.....|.....|.###.|#...#|#####|#....|.###.
f..##.|.#..#|.#...|####.|.#...|.#...|.#...
g.....|.....|.####|#...#|.####|....#|.###.
h#....|#....|####.|#...#|#...#|#...#|#...#
i..#..|.....|.##..|..#..|..#..|..#..|.###.
j...#.|.....|..##.|...#.|...#.|#..#.|.##..
k#....|#....|#..#.|#.#..|##...|#.#..|#..#.
l.##..|..#..|..#..|..#..|..#..|..#..|.###.
m.....|.....|##.#.|#.#.#|#.#.#|#...#|#...#
n.....|.....|####.|#...#|#...#|#...#|#...#
o.....|.....|.###.|#...#|#...#|#...#|.###.
p.....|.....|####.|#...#|####.|#....|#....
q.....|.....|.####|#...#|.####|....#|....#
r.....|.....|#.##.|##...|#....|#....|#....
s.....|.....|.####|#....|.###.|....#|####.
t.#...|.#...|####.|.#...|.#...|.#..#|..##.
u.....|.....|#...#|#...#|#...#|#..##|.##.#
v.....|.....|#...#|#...#|#...#|.#.#.|..#..
w.....|.....|#...#|#...#|#.#.#|#.#.#|.#.#.
x.....|.....|#...#|.#.#.|..#..|.#.#.|#...#
y.....|.....|#...#|#...#|.####|....#|.###.
z.....|.....|#####|...#.|..#..|.#...|#####
{...##|..#..|..#..|.#...|..#..|..#..|...##
|..#..|..#..|..#..|..#..|..#..|..#..|..#..
}##...|..#..|..#..|...#.|..#..|..#..|##...
~.....|.#..#|#.#.#|#..#.|.....|.....|.....
"""


def main():
    lines = [line for line in GLYPHS.split("\n") if line]
    rows = []
    for index, line in enumerate(lines):
        char = line[0]
        expected = chr(0x20 + index)
        if char != expected:
            sys.exit("glyph %d is %r, expected %r" % (index, char, expected))

        art = line[1:].split("|")
        if len(art) != 7:
            sys.exit("glyph %r has %d rows, expected 7" % (char, len(art)))
        for row in art:
            if len(row) != 5 or set(row) - set(".# "):
                sys.exit("glyph %r has a bad row %r" % (char, row))

        bits = ["0b" + "".join("1" if c == "#" else "0" for c in row) for row in art]
        rows.append((char, bits))

    if len(rows) != 95:
        sys.exit("expected 95 glyphs, got %d" % len(rows))

    out = sys.stdout.write
    out("//! A 5x7 bitmap font.\n//!\n")
    out("//! Generated by `tools/genfont.py`; edit the art there, not the table\n")
    out("//! here. Each row is five bits, most significant bit leftmost, so the\n")
    out("//! literals below still read as pictures of the glyphs they draw.\n//!\n")
    out("//! Clean-room. The kernel's bitmap fonts are GPL-2.0, which would reach\n")
    out("//! into every binary that linked this one.\n\n")
    out("/// Glyph width in pixels, before scaling.\npub const WIDTH: usize = 5;\n\n")
    out("/// Glyph height in pixels, before scaling.\npub const HEIGHT: usize = 7;\n\n")
    out("/// The first character in the table.\nconst FIRST: u8 = 0x20;\n\n")
    out("#[rustfmt::skip]\nstatic GLYPHS: [[u8; HEIGHT]; 95] = [\n")
    for char, bits in rows:
        label = "space" if char == " " else char
        out("    [%s], // %s\n" % (", ".join(bits), label))
    out("];\n\n")
    out("/// The bitmap for a character, or the bitmap for `?` if it has none.\n")
    out("///\n")
    out("/// Substituting rather than skipping matters: a missing glyph that took\n")
    out("/// no space would silently shorten a line of text, and the mistake would\n")
    out("/// look like a layout bug rather than a font one.\n")
    out("pub fn glyph(character: char) -> &'static [u8; HEIGHT] {\n")
    out("    let code = character as u32;\n")
    out("    let index = code.wrapping_sub(FIRST as u32) as usize;\n")
    out("    GLYPHS.get(index).unwrap_or(&GLYPHS[('?' as u8 - FIRST) as usize])\n")
    out("}\n")


if __name__ == "__main__":
    main()
