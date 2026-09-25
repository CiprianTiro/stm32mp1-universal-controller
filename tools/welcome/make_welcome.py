#!/usr/bin/env python3
"""Generates the welcome screen pictures (issue #59).

One design, used in two places so the hand-over from the bootloader to the UI
is seamless:

  * U-Boot shows a BMP from the boot partition as soon as it reads the boot
    menu (extlinux "MENU BACKGROUND"). It stays on screen until the kernel's
    real display driver takes over (~12 s after power-on).
        -> welcome_480x800.bmp / welcome_800x480.bmp
           (8-bit palette BMPs, the format ST's own splash images use)
    It shows "Starting..." but NO loading dots: a picture can't move, and
    dots that stand still for 10 s look like a frozen boot. The dots appear
    (already moving) when the UI takes over.
  * The UI (Slint) then shows the same picture while it waits for the
    backend, with the loading dots animated and the text below them drawn
    by Slint itself (so it can change, e.g. to "Still starting...").
        -> welcome.png (background only: icon + name)

The positions of the dots and the status text are printed at the end; they
must match the constants in linux_a7/ui_layer/ui/app.slint (WelcomeScreen).

Run from the repo root after changing the design, and commit the outputs:
    python3 tools/welcome/make_welcome.py
Needs Pillow (pip install pillow).
"""
import json
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

REPO = Path(__file__).resolve().parents[2]
FONT = REPO / "linux_a7/ui_layer/fonts/DejaVuSans.ttf"  # the UI's own font
UI_OUT = REPO / "linux_a7/ui_layer/ui/images/welcome.png"
BMP_DIR = REPO / "yocto_layers/meta-universal-controller/recipes-bsp/u-boot/u-boot-stm32mp-splash"

# The palette: the design tokens' "splash" colors (issue #39), the same
# ones the UI's welcome screen uses (theme.slint's Splash) -- one source, so
# the boot image and the welcome screen can't drift apart.
SPLASH = json.loads((REPO / "linux_a7/ui_layer/ui/tokens.json").read_text())["splash"]
BACKGROUND = SPLASH["background"]  # deep navy
ACCENT = SPLASH["accent"]          # sky blue: icon, active dot
TITLE = SPLASH["title"]            # near-white
MUTED = SPLASH["muted"]            # status text
DOT_OFF = SPLASH["dot-off"]        # inactive dots (UI only)

# Layout of the 480x800 portrait screen (the DK2 panel's native orientation).
WIDTH, HEIGHT = 480, 800
ICON_CENTER_Y = 300
TITLE_Y = 450            # centre line of the product name
DOTS_Y = 560             # centre line of the loading dots
DOT_RADIUS = 7
DOT_SPACING = 28         # centre to centre
STATUS_Y = 600           # centre line of the status text
STATUS_TEXT = "Starting…"

# Everything is drawn 4x larger and scaled down at the end: Pillow's shapes
# have no anti-aliasing of their own, and this gives smooth edges.
SS = 4


def font(size):
    return ImageFont.truetype(str(FONT), size * SS)


def draw_icon(draw, cx, cy):
    """A house outline with a 'signal' inside: a smart-home hub."""
    s = SS
    w = 12 * s  # stroke width
    # Roof: a wide inverted V; body: a rectangle below it.
    roof = [(cx - 95 * s, cy - 5 * s), (cx, cy - 90 * s), (cx + 95 * s, cy - 5 * s)]
    draw.line(roof, fill=ACCENT, width=w, joint="curve")
    body = (cx - 70 * s, cy - 22 * s, cx + 70 * s, cy + 85 * s)
    draw.rounded_rectangle(body, radius=10 * s, outline=ACCENT, width=w)
    # Inside: a dot and two arcs radiating upwards (like a Wi-Fi symbol).
    dot_y = cy + 55 * s
    r = 9 * s
    draw.ellipse((cx - r, dot_y - r, cx + r, dot_y + r), fill=ACCENT)
    for radius in (30, 52):
        rr = radius * s
        draw.arc((cx - rr, dot_y - rr, cx + rr, dot_y + rr), start=225, end=315, fill=ACCENT, width=8 * s)


def centered_text(draw, text, cx, cy, size, color):
    # anchor "mm": the given point is the middle of the text, both ways.
    draw.text((cx, cy), text, font=font(size), fill=color, anchor="mm")


def background(width, height, offset_y):
    """Icon + name, i.e. everything that doesn't change while loading."""
    img = Image.new("RGB", (width * SS, height * SS), BACKGROUND)
    draw = ImageDraw.Draw(img)
    cx = width // 2 * SS
    draw_icon(draw, cx, (ICON_CENTER_Y + offset_y) * SS)
    centered_text(draw, "Universal Controller", cx, (TITLE_Y + offset_y) * SS, 34, TITLE)
    return img, draw


def add_status(draw, width, offset_y):
    """The boot image's status text, at the same place the UI draws it (no
    dots -- see the header comment)."""
    cx = width // 2
    centered_text(draw, STATUS_TEXT, cx * SS, (STATUS_Y + offset_y) * SS, 20, MUTED)


def finish(img, width, height):
    return img.resize((width, height), Image.LANCZOS)


def save_bmp(img, path):
    # 8 bits per pixel with a 256-colour palette, like ST's splash images:
    # small, and a format U-Boot's BMP code always supports. The picture
    # only uses a handful of colours plus their anti-aliased blends, so 256
    # is plenty.
    img.quantize(colors=256, method=Image.Quantize.MEDIANCUT).save(path, format="BMP")


def main():
    UI_OUT.parent.mkdir(parents=True, exist_ok=True)
    BMP_DIR.mkdir(parents=True, exist_ok=True)

    # UI background: icon + name only.
    img, _ = background(WIDTH, HEIGHT, 0)
    finish(img, WIDTH, HEIGHT).save(UI_OUT, optimize=True)

    # Boot image, portrait (the one the DK2 uses).
    img, draw = background(WIDTH, HEIGHT, 0)
    add_status(draw, WIDTH, 0)
    save_bmp(finish(img, WIDTH, HEIGHT), BMP_DIR / "welcome_480x800.bmp")

    # Landscape variant (other ST boards; kept so both splash files are ours).
    # Same content, moved up so it fits in 480 pixels of height.
    img, draw = background(800, 480, -180)
    add_status(draw, 800, -180)
    save_bmp(finish(img, 800, 480), BMP_DIR / "welcome_800x480.bmp")

    print(f"wrote {UI_OUT.relative_to(REPO)}")
    print(f"wrote {BMP_DIR.relative_to(REPO)}/welcome_480x800.bmp, welcome_800x480.bmp")
    print(f"Slint constants: dots y={DOTS_Y} r={DOT_RADIUS} spacing={DOT_SPACING}, status y={STATUS_Y}")


main()
