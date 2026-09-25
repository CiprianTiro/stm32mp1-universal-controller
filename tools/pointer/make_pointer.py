#!/usr/bin/env python3
"""Draws the UI's mouse pointer (issue #38) into
linux_a7/ui_layer/ui/images/pointer.png.

Why the UI needs its own: Slint's KMS backend draws a pointer only with its
GPU renderers; the software renderer the hub uses leaves it out, so a
mouse worked but was invisible. app.slint draws this image at the pointer's
position instead (main.rs follows the mouse).

The arrow is the classic 12x19 one: black outline, white inside, so it
stands out on light and dark backgrounds. Its tip is the top-left pixel,
the exact point that gets clicked. Pure Python (no Pillow): a PNG is just a
few zlib-compressed rows with a checksum per chunk.

    python3 tools/pointer/make_pointer.py
"""
import struct
import zlib
from pathlib import Path

# X = outline (black), o = inside (white), . = transparent.
ARROW = """
X...........
XX..........
XoX.........
XooX........
XoooX.......
XooooX......
XoooooX.....
XooooooX....
XoooooooX...
XooooooooX..
XoooooooooX.
XooooooXXXXX
XoooXooX....
XooX.XooX...
XoX..XooX...
XX....XooX..
X.....XooX..
.......XooX.
........XX..
""".split()

COLORS = {"X": (0, 0, 0, 255), "o": (255, 255, 255, 255), ".": (0, 0, 0, 0)}


def chunk(kind, data):
    body = kind + data
    return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))


def main():
    height, width = len(ARROW), len(ARROW[0])
    # Each row: filter type 0 ("none"), then RGBA bytes.
    raw = b"".join(b"\0" + b"".join(bytes(COLORS[c]) for c in row) for row in ARROW)
    png = (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0))  # 8-bit RGBA
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )
    out = Path(__file__).resolve().parents[2] / "linux_a7/ui_layer/ui/images/pointer.png"
    out.write_bytes(png)
    print(f"wrote {out} ({width}x{height}, {len(png)} bytes)")


if __name__ == "__main__":
    main()
