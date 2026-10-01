#!/usr/bin/env python3
"""The IR add-on board on a 6 x 28 hole perfboard (separate pads), issue #42.

    python3 firmware_ir_blaster/hardware/tools/perfboard.py

Describes where every part leg goes and which holes are joined, CHECKS the
result against the circuit (every net joins exactly the legs it should,
and nothing else), then draws ../images/perfboard.svg: the part side and
the copper side (mirrored: what you see when you turn the board over).

Holes are described here on a simple grid -- columns A-F across the short
side, rows 1-28 along the long side, row 1 at the IR LEDs' edge -- and
SHOWN with the names printed on the board we use (a 2 x 8 cm board): the
long side lettered A..Z then A' B' (a second A and B), the short side
numbered 01..06. Grid row 1 = printed A, row 27 = A', row 28 = B'; grid
column A = printed 01 ... F = 06. So grid hole "D3" is printed "C04".

The drawing shows the board the way it's held to read its print: A at the
left, 06 at the top. That side is the PART side.
"""
import os
import sys

COLS = "ABCDEF"
ROWS = 28

# ---- The layout, in the names printed on the board. ------------------------
# Letters A..Z, A', B' along the long side; numbers 01..06 across. Held with
# A at the left and 06 at the top, the print reading normally: the part side.
LETTERS = [chr(ord("A") + i) for i in range(26)] + ["A'", "B'"]


def grid(name):
    """Printed name -> grid hole: "C04" -> "D3" (letter = row, number = column)."""
    letter, number = name[:-2], int(name[-2:])
    return f"{COLS[number - 1]}{LETTERS.index(letter) + 1}"


def printed(hole):
    """Grid hole -> the name printed on the board: "D3" -> "C04"."""
    c, r = pos(hole)
    return f"{LETTERS[r - 1]}{c + 1:02d}"


def pos(hole):
    return COLS.index(hole[0]), int(hole[1:])


# The parts: name -> {pin: hole}. Resistors lie flat, legs 4 holes apart.
PARTS_PRINTED = {
    "D1": {"+": "A01", "-": "A02"},              # IR LED 1 (long leg = +)
    "D2": {"+": "A03", "-": "A04"},              # IR LED 2
    "R1a": {"1": "B01", "2": "F01"},             # 47 ohm
    "R1b": {"1": "B02", "2": "F02"},             # 47 ohm
    "C1": {"+": "G02", "-": "G03"},              # 100 uF, polarised
    "Q1": {"E": "B03", "B": "B04", "C": "B05"},  # 2N2222, flat face toward the B' end
    "R3": {"1": "C04", "2": "G04"},              # 10 k
    "R2": {"2": "C05", "1": "G05"},              # 1 k (2 = base side)
    "R4": {"1": "W05", "2": "A'05"},             # 100 ohm
    "C2": {"1": "Z03", "2": "Z04"},              # 100 nF
    "U1": {"OUT": "B'02", "GND": "B'03", "VCC": "B'04"},  # CHQ1838, lens out of the B' end
}

# The wires to the ESP32 board.
WIRES_PRINTED = {"5Vin": "G01", "GND": "I03", "GPIO4": "H05", "3V3": "V05", "GPIO5": "A'02"}

# Solder bridges between NEIGHBOURING holes (copper side).
BRIDGES_PRINTED = [
    ("A01", "B01"), ("B01", "B02"),              # LED 1 + to both 47 ohm
    ("F01", "F02"), ("F01", "G01"), ("F02", "G02"),  # 5V: 47 ohms, wire, C1 +
    ("A02", "A03"),                              # LED 1 - to LED 2 +
    ("A04", "A05"), ("A05", "B05"),              # LED 2 - to Q1 collector
    ("B04", "C04"), ("C04", "C05"),              # Q1 base, R3, R2
    ("G04", "G03"),                              # R3 to GND
    ("G05", "H05"),                              # R2 to the GPIO4 wire
    ("B'02", "A'02"),                            # receiver OUT to the GPIO5 wire
    ("B'04", "A'04"), ("A'04", "Z04"), ("A'04", "A'05"),  # receiver VCC, C2, R4
    ("V05", "W05"),                              # R4 to the 3V3 wire
]

# The GND line: a bare wire on the copper side along the 03 holes, from
# B03 (Q1's emitter) to B'03 (the receiver's GND), soldered to them.
BUS_PRINTED = ("B03", "B'03")

# Insulated wires on the part side between holes that aren't neighbours
# (none needed in this layout).
JUMPERS_PRINTED = []

PARTS = {part: {pin: grid(h) for pin, h in legs.items()} for part, legs in PARTS_PRINTED.items()}
WIRES = {name: grid(h) for name, h in WIRES_PRINTED.items()}
BRIDGES = [(grid(a), grid(b)) for a, b in BRIDGES_PRINTED]
JUMPERS = [(grid(a), grid(b)) for a, b in JUMPERS_PRINTED]
_bus_a, _bus_b = grid(BUS_PRINTED[0]), grid(BUS_PRINTED[1])
assert _bus_a[0] == _bus_b[0], "the GND line runs along one number"
BUS = (_bus_a[0], pos(_bus_a)[1], pos(_bus_b)[1])


# ---- The circuit (firmware_ir_blaster/hardware/README.md). ----------------
NETS = {
    "LED +": ["D1.+", "R1a.1", "R1b.1"],
    "5V": ["R1a.2", "R1b.2", "C1.+", "wire.5Vin"],
    "LED1-LED2": ["D1.-", "D2.+"],
    "collector": ["D2.-", "Q1.C"],
    "base": ["Q1.B", "R3.1", "R2.2"],
    "GPIO4": ["R2.1", "wire.GPIO4"],
    "GND": ["Q1.E", "R3.2", "C1.-", "wire.GND", "U1.GND", "C2.1"],
    "receiver VCC": ["U1.VCC", "C2.2", "R4.2"],
    "3V3": ["R4.1", "wire.3V3"],
    "GPIO5": ["U1.OUT", "wire.GPIO5"],
}


def neighbours(a, b):
    (ca, ra), (cb, rb) = pos(a), pos(b)
    return abs(ca - cb) + abs(ra - rb) == 1


def check():
    """Returns a list of problems (empty: the board is the circuit)."""
    problems = []
    used = {}
    pins = {}
    for part, legs in PARTS.items():
        for pin, hole in legs.items():
            pins[f"{part}.{pin}"] = hole
    for name, hole in WIRES.items():
        pins[f"wire.{name}"] = hole
    for pin, hole in pins.items():
        if hole in used:
            problems.append(f"{hole} holds {used[hole]} and {pin}")
        used[hole] = pin
    for a, b in BRIDGES:
        if not neighbours(a, b):
            problems.append(f"bridge {a}-{b}: not neighbours")
    col, top, bottom = BUS
    bus = [f"{col}{r}" for r in range(top, bottom + 1)]

    # Union-find over holes.
    parent = {}

    def find(x):
        parent.setdefault(x, x)
        while parent[x] != x:
            parent[x] = parent[parent[x]]
            x = parent[x]
        return x

    def join(a, b):
        parent[find(a)] = find(b)

    for a, b in BRIDGES + JUMPERS:
        join(a, b)
    for a, b in zip(bus, bus[1:]):
        join(a, b)
    groups = {}
    for pin, hole in pins.items():
        groups.setdefault(find(hole), set()).add(pin)
    want = {frozenset(v) for v in NETS.values()}
    got = {frozenset(v) for v in groups.values()}
    for net, members in NETS.items():
        if frozenset(members) not in got:
            problems.append(f"net {net}: wanted {sorted(members)}")
    for g in got - want:
        problems.append(f"joined but shouldn't be (or incomplete): {sorted(g)}")
    return problems


# ---- Drawing. --------------------------------------------------------------
# Landscape, as the board is held: printed A..B' left to right, 06 at the
# top on the part side. The copper side is the board turned over its long
# edge (top to bottom): letters still left to right, but 01 at the top.
P = 24              # hole pitch in the drawing (px)
LEFT = 120          # room for the row numbers and the LEDs left of the board
BOARD_W = P * (ROWS - 1)
BOARD_H = P * (len(COLS) - 1)
PANEL_H = BOARD_H + 280
WIDTH = LEFT + BOARD_W + 120

NET_COLOUR = {"5Vin": "#d62828", "3V3": "#f77f00", "GND": "#1d4ed8", "GPIO4": "#15803d", "GPIO5": "#15803d"}


def xy(hole, copper, top):
    c, r = pos(hole)
    x = LEFT + (r - 1) * P
    y = top + (c if copper else len(COLS) - 1 - c) * P
    return x, y


def svg():
    out = []
    height = 2 * PANEL_H
    out.append(f'<svg xmlns="http://www.w3.org/2000/svg" width="{WIDTH}" height="{height}" viewBox="0 0 {WIDTH} {height}" font-family="sans-serif" font-size="12">')
    out.append(f'<rect width="{WIDTH}" height="{height}" fill="#ffffff"/>')
    for copper in (False, True):
        y0 = copper * PANEL_H
        top = y0 + 80
        title = ("COPPER SIDE: the board turned over its long edge (01 now at the top)" if copper
                 else "PART SIDE: hold the board so the print reads A at the left, 06 at the top")
        out.append(f'<text x="10" y="{y0 + 24}" font-size="16" font-weight="bold">{title}</text>')
        # Board, holes, printed names.
        out.append(f'<rect x="{LEFT - 16}" y="{top - 16}" width="{BOARD_W + 32}" height="{BOARD_H + 32}" rx="6" fill="#e9d8a6" stroke="#9a7b4f"/>')
        for r in range(1, ROWS + 1):
            x, _ = xy(f"A{r}", copper, top)
            out.append(f'<text x="{x}" y="{top - 22}" text-anchor="middle" font-weight="bold">{LETTERS[r - 1]}</text>')
            out.append(f'<text x="{x}" y="{top + BOARD_H + 32}" text-anchor="middle" fill="#666">{LETTERS[r - 1]}</text>')
        for c, col in enumerate(COLS):
            _, y = xy(f"{col}1", copper, top)
            out.append(f'<text x="14" y="{y + 4}" font-weight="bold">{c + 1:02d}</text>')
        for col in COLS:
            for r in range(1, ROWS + 1):
                x, y = xy(f"{col}{r}", copper, top)
                ring = "#c58b4a" if copper else "#d9b98a"
                out.append(f'<circle cx="{x}" cy="{y}" r="6" fill="{ring}" stroke="#8a5a2b" stroke-width="0.8"/><circle cx="{x}" cy="{y}" r="2.2" fill="#fff"/>')
        (copper_side if copper else part_side)(out, top)
    out.append("</svg>")
    return "\n".join(out)


def wire_rings(out, top, copper):
    for name, hole in WIRES.items():
        x, y = xy(hole, copper, top)
        out.append(f'<circle cx="{x}" cy="{y}" r="8" fill="none" stroke="{NET_COLOUR[name]}" stroke-width="3"/>')


def copper_side(out, top):
    col, first, last = BUS
    x0, y = xy(f"{col}{first}", True, top)
    x1, _ = xy(f"{col}{last}", True, top)
    out.append(f'<line x1="{x0}" y1="{y}" x2="{x1}" y2="{y}" stroke="#9ca3af" stroke-width="5" stroke-linecap="round"/>')
    out.append(f'<line x1="{x0}" y1="{y}" x2="{x1}" y2="{y}" stroke="#1d4ed8" stroke-width="1.5" stroke-dasharray="3 3"/>')
    for a, b in BRIDGES:
        (xa, ya), (xb, yb) = xy(a, True, top), xy(b, True, top)
        out.append(f'<line x1="{xa}" y1="{ya}" x2="{xb}" y2="{yb}" stroke="#6b7280" stroke-width="10" stroke-linecap="round"/>')
        out.append(f'<line x1="{xa}" y1="{ya}" x2="{xb}" y2="{yb}" stroke="#d1d5db" stroke-width="5" stroke-linecap="round"/>')
    wire_rings(out, top, True)
    notes = [
        f"Blue dashed: the GND line, a bare wire along the {COLS.index(col) + 1:02d} holes from {printed(f'{col}{first}')} to {printed(f'{col}{last}')}, soldered to them.",
        "Grey: solder bridges, each joining two neighbouring holes (a cut-off leg laid across both is easiest).",
        "Coloured rings: where the 5 wires to the ESP32 go through (the colours as on the part side).",
        "The hole names are the same on both sides: find a hole by its printed letter and number.",
    ]
    for i, n in enumerate(notes):
        out.append(f'<text x="10" y="{top + BOARD_H + 64 + i * 18}">{n}</text>')


def part_side(out, top):
    def at(h):
        return xy(h, False, top)

    def text(x, y, t, anchor="start", colour="#000", weight="normal", size=12):
        out.append(f'<text x="{x}" y="{y}" text-anchor="{anchor}" fill="{colour}" font-weight="{weight}" font-size="{size}">{t}</text>')

    def legs(*holes):
        for h in holes:
            x, y = at(h)
            out.append(f'<circle cx="{x}" cy="{y}" r="3" fill="#444"/>')

    # IR LEDs, lying flat, pointing out of the A end (left).
    for name in ("D1", "D2"):
        (xa, ya), (xk, yk) = at(PARTS[name]["+"]), at(PARTS[name]["-"])
        cy = (ya + yk) / 2
        out.append(f'<path d="M {xa} {ya} H {xa - 26} M {xk} {yk} H {xk - 26}" stroke="#555" stroke-width="2"/>')
        out.append(f'<rect x="{xa - 62}" y="{cy - 10}" width="36" height="20" rx="10" fill="#dbeafe" stroke="#1e3a8a"/>')
        text(xa - 44, cy + 4, name, "middle", "#1e3a8a", "bold")
        text(xa - 30, ya + 4, "+", "end", "#d62828", "bold", 14)
        legs(PARTS[name]["+"], PARTS[name]["-"])
    # Flat resistors: legs, and the body in the middle with its name.
    for name in ("R1a", "R1b", "R3", "R2", "R4"):
        (x1, y1), (x2, y2) = [at(h) for h in PARTS[name].values()]
        out.append(f'<line x1="{x1}" y1="{y1}" x2="{x2}" y2="{y2}" stroke="#777" stroke-width="2"/>')
        mx = (x1 + x2) / 2
        out.append(f'<rect x="{mx - 30}" y="{y1 - 8}" width="60" height="16" rx="7" fill="#d6b48a" stroke="#8a5a2b"/>')
        text(mx, y1 + 4, name, "middle", "#111", "bold", 10)
        legs(*PARTS[name].values())
    # C2: small, standing on two neighbouring holes.
    (x1, y1), (x2, y2) = [at(h) for h in PARTS["C2"].values()]
    out.append(f'<line x1="{x1}" y1="{y1}" x2="{x2}" y2="{y2}" stroke="#d97706" stroke-width="13" stroke-linecap="round" opacity="0.9"/>')
    text(x1 - 12, (y1 + y2) / 2 + 4, "C2", "end", "#111", "bold", 10)
    legs(*PARTS["C2"].values())
    # C1: polarised, the stripe on the - side.
    (xp, yp), (xm, ym) = at(PARTS["C1"]["+"]), at(PARTS["C1"]["-"])
    out.append(f'<circle cx="{xp}" cy="{(yp + ym) / 2}" r="11" fill="#1f2937" opacity="0.85"/>')
    out.append(f'<rect x="{xp - 11}" y="{ym - 7}" width="22" height="5" fill="#e5e7eb" opacity="0.9"/>')
    text(xp + 14, yp + 4, "+", "start", "#d62828", "bold", 14)
    text(xp - 14, ym - 6, "C1", "end", "#111", "bold", 10)
    legs(PARTS["C1"]["+"], PARTS["C1"]["-"])
    # Q1: TO-92 seen from above, flat face toward the B' end (right).
    (xe, ye), (xc, yc) = at(PARTS["Q1"]["E"]), at(PARTS["Q1"]["C"])
    out.append(f'<path d="M {xe + 7} {yc - 9} V {ye + 9} H {xe - 2} Q {xe - 22} {(ye + yc) / 2} {xe - 2} {yc - 9} Z" fill="#111827" opacity="0.85"/>')
    for pin in ("C", "B", "E"):
        x, y = at(PARTS["Q1"][pin])
        text(x - 11, y + 4, pin, "end", "#fff", "bold", 10)
    legs(*PARTS["Q1"].values())
    text(xe, yc - 14, "Q1", "middle", "#111", "bold", 10)
    # Receiver: lens out of the B' end (right).
    (xo, yo), (xv, yv) = at(PARTS["U1"]["OUT"]), at(PARTS["U1"]["VCC"])
    out.append(f'<rect x="{xo + 22}" y="{yv - 10}" width="26" height="{yo - yv + 20}" rx="4" fill="#111827" opacity="0.9"/>')
    out.append(f'<circle cx="{xo + 42}" cy="{(yo + yv) / 2}" r="6" fill="#4b5563"/>')
    for pin in ("OUT", "GND", "VCC"):
        x, y = at(PARTS["U1"][pin])
        out.append(f'<line x1="{x}" y1="{y}" x2="{x + 22}" y2="{y}" stroke="#555" stroke-width="2"/>')
        text(x + 52, y + 4, pin, "start", "#111", "bold", 10)
    legs(*PARTS["U1"].values())
    # Insulated links, if any.
    for a, b in JUMPERS:
        (x1, y1), (x2, y2) = at(a), at(b)
        out.append(f'<path d="M {x1} {y1} Q {x1 - 16} {(y1 + y2) / 2} {x2} {y2}" fill="none" stroke="#1d4ed8" stroke-width="3"/>')
    wire_rings(out, top, False)
    # The legend under the board, in printed names.
    P_ = PARTS_PRINTED
    lines = [
        f"D1, D2 IR LEDs, flat, pointing out of the A end: long leg (+) in {P_['D1']['+']} and {P_['D2']['+']}, short legs {P_['D1']['-']} and {P_['D2']['-']}",
        "Resistors lying flat, legs 4 holes apart: " + " \u2022 ".join(
            f"{n} {v} {P_[n]['1']}-{P_[n]['2']}" for n, v in (("R1a", "47 \u03a9"), ("R1b", "47 \u03a9"), ("R3", "10 k\u03a9"), ("R2", "1 k\u03a9"), ("R4", "100 \u03a9"))),
        f"C1 100 \u00b5F: + (long leg) in {P_['C1']['+']}, stripe (\u2212) in {P_['C1']['-']} \u2022 C2 100 nF: {P_['C2']['1']}-{P_['C2']['2']}",
        f"Q1 2N2222, flat face toward the B' end: E in {P_['Q1']['E']}, B in {P_['Q1']['B']}, C in {P_['Q1']['C']}",
        f"U1 CHQ1838, lens pointing out of the B' end: OUT {P_['U1']['OUT']}, GND {P_['U1']['GND']}, VCC {P_['U1']['VCC']}",
    ]
    for i, line in enumerate(lines):
        text(10, top + BOARD_H + 62 + i * 18, line)
    y = top + BOARD_H + 62 + len(lines) * 18 + 8
    x = 10
    text(x, y, "Wires to the ESP32:", weight="bold")
    x += 135
    for name, hole in WIRES.items():
        out.append(f'<circle cx="{x + 6}" cy="{y - 4}" r="7" fill="none" stroke="{NET_COLOUR[name]}" stroke-width="3"/>')
        text(x + 18, y, f"{printed(hole)} {name}", colour=NET_COLOUR[name], weight="bold")
        x += 105


def main():
    problems = check()
    for p in problems:
        print("PROBLEM:", p)
    if problems:
        sys.exit(1)
    print("check: the board matches the circuit (all", len(NETS), "nets)")
    if "--list" in sys.argv:
        for part, legs in PARTS.items():
            print(part, ", ".join(f"{pin} {printed(h)}" for pin, h in legs.items()))
        for name, hole in WIRES.items():
            print("wire", name, printed(hole))
        print("bridges:", ", ".join(f"{printed(a)}-{printed(b)}" for a, b in BRIDGES))
        print("links:", ", ".join(f"{printed(a)}-{printed(b)}" for a, b in JUMPERS))
        col, first, last = BUS
        print("GND line:", printed(f"{col}{first}"), "to", printed(f"{col}{last}"))
    here = os.path.dirname(os.path.abspath(__file__))
    path = os.path.join(here, "..", "images", "perfboard.svg")
    with open(path, "w") as f:
        f.write(svg())
    print("wrote", os.path.normpath(path))


if __name__ == "__main__":
    main()
