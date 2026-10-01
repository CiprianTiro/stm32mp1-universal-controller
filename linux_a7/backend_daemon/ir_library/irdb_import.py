#!/usr/bin/env python3
"""Turns the Flipper Zero IR database into the hub's IR code library (#82).

The hub's code finder ("I don't have the remote") tries known code sets for
a device type and brand until one works. Those sets come from Flipper-IRDB
(https://github.com/Lucaslhm/Flipper-IRDB, licence CC0 1.0 = public domain,
so it may be shipped in the hub image as-is). This tool reads a checkout of
it and writes:

    <out>/index.json          the types, their brands and how many sets each
    <out>/<type>.json         one file per device type, read by the hub only
                              when the finder runs for that type

    python3 linux_a7/backend_daemon/ir_library/irdb_import.py <Flipper-IRDB checkout> <out dir>

The Yocto recipe (ir-library.bb) runs it during the build on a fixed commit of the database,
so no generated file is kept in git. Only the standard library is used.

FLIPPER'S FORMAT (.ir files, "IR signals file", version 1): blocks of
`key: value` lines separated by `#` lines, one block per button:

    name: POWER
    type: parsed
    protocol: NEC
    address: 00 00 00 00        <- 4 bytes, LOW BYTE FIRST
    command: 40 00 00 00

or a recorded signal:

    name: POWER
    type: raw
    frequency: 38000
    duty_cycle: 0.330000
    data: 9000 4500 560 560 ... <- microseconds: mark, space, mark, ...

WHAT A CODE BECOMES. The blaster understands two codes
(firmware_ir_blaster/PROTOCOL.md, "Codes"):

    {"proto": "nec", "address": 0-65535, "command": 0-255}
    {"proto": "raw", "carrier_hz": 30000-60000, "timings": [...]}

NEC (and NECext that fits, see nec_code) and raw codes are written in that
form, ready to send. Every other protocol (Samsung32, RC5, SIRC, ...) is
kept as Flipper parsed it, e.g. {"proto": "samsung32", "address": 7,
"command": 2}: the hub turns those into raw timings itself (its own
encoders), and until it knows a protocol it leaves the sets that use it
out of the finder.
"""
import argparse
import json
import os
import re
import subprocess
import sys
from collections import Counter

# Library format version: the hub refuses a library whose format it doesn't
# know (rather than misreading it).
FORMAT = 1

# The device types shipped, by Flipper-IRDB folder: hub type id and the name
# the touchscreen shows. Left out on purpose:
#   ACs            an AC remote sends the WHOLE state each time (mode,
#                  temperature, fan), so a library "button" is one frozen
#                  state; ACs get real protocol support in #83.
#   _Converted_    72 MB bulk-converted from other databases, unchecked.
#   Toys, Miscellaneous, and the rare types (bidets, laserdisc, ...): not
#                  worth their size on the hub.
TYPES = {
    "LED_Lighting": ("led_lighting", "LED lights and strips"),
    "TVs": ("tv", "TVs"),
    "Projectors": ("projector", "Projectors"),
    "Fans": ("fan", "Fans"),
    "Heaters": ("heater", "Heaters"),
    "Air_Purifiers": ("air_purifier", "Air purifiers"),
    "Humidifiers": ("humidifier", "Humidifiers"),
    "Fireplaces": ("fireplace", "Electric fireplaces"),
    "Audio_and_Video_Receivers": ("av_receiver", "AV receivers and amplifiers"),
    "SoundBars": ("soundbar", "Soundbars"),
    "Speakers": ("speaker", "Speakers"),
    "Streaming_Devices": ("streaming", "Streaming boxes"),
    "Cable_Boxes": ("cable_box", "Cable and satellite boxes"),
    "Blu-Ray": ("bluray", "Blu-ray players"),
    "DVD_Players": ("dvd", "DVD players"),
    "Monitors": ("monitor", "Monitors"),
}

# The blaster's limits for a raw code (PROTOCOL.md): one RMT duration is at
# most 32767 us (15 bits at 1 MHz), at most 1024 durations per code.
RAW_MAX_US = 32767
RAW_MAX_LEN = 1024
CARRIER_MIN, CARRIER_MAX = 30000, 60000

# The brand of no-name remotes.
GENERIC = "Generic (no brand)"

# The hub's limits for a button (device.rs, the `remote` capability).
BUTTON_NAME_MAX = 24
BUTTONS_MAX = 100


class Stats:
    """What happened to every code, printed at the end so a new database
    version that suddenly drops many codes is noticed."""

    def __init__(self):
        self.c = Counter()

    def add(self, what):
        self.c[what] += 1

    def report(self, out):
        for what, n in sorted(self.c.items()):
            print(f"  {n:7d}  {what}", file=out)


def parse_ir_file(text):
    """One .ir file -> list of dicts, one per button block. Keys are
    lowercased; anything that isn't `key: value` (comments, `#`) ends the
    current block."""
    blocks, cur = [], {}
    for line in text.splitlines():
        line = line.strip()
        if line.startswith("#") or not line:
            if cur:
                blocks.append(cur)
                cur = {}
            continue
        key, sep, value = line.partition(":")
        if not sep:
            continue
        key = key.strip().lower()
        if key == "name" and "name" in cur:
            # A new button without a `#` line in between.
            blocks.append(cur)
            cur = {}
        cur[key] = value.strip()
    if cur:
        blocks.append(cur)
    # The file header (Filetype/Version) is a block without a name.
    return [b for b in blocks if "name" in b and "type" in b]


def le_bytes(text):
    """Flipper's "00 EF 00 00" (low byte first) -> 0xEF00. None if bad."""
    try:
        parts = [int(p, 16) for p in text.split()]
    except ValueError:
        return None
    if not parts or any(p > 0xFF for p in parts):
        return None
    return sum(b << (8 * i) for i, b in enumerate(parts))


def nec_code(proto, address, command):
    """A Flipper NEC / NECext code -> the blaster's nec code, or None when
    it doesn't fit (then it stays parsed for the hub's NEC encoder).

    An NEC frame is 4 bytes: address, address check, command, command
    check. Standard NEC: each check byte is the inverse (~) of the byte
    before it. Extended NEC (NECext) uses both address bytes as a 16-bit
    address. The blaster (ir_nec.c) sends:
      address <= 0xFF  -> address, ~address   (standard)
      address >  0xFF  -> low byte, high byte (extended)
      and always       -> command, ~command
    so a code fits when its command check byte is the inverse, and an
    extended address isn't one the blaster would mistake for standard
    (e.g. NECext address 0x0005: bytes 05 00, but the blaster would send
    05 FA)."""
    if proto == "nec":
        # Flipper's NEC: 8-bit address and command; the checks are implied.
        if address <= 0xFF and command <= 0xFF:
            return {"proto": "nec", "address": address, "command": command}
        return None
    # necext: address 16 bits, command 16 bits (command, command check).
    lo, hi = command & 0xFF, command >> 8
    if command > 0xFFFF or hi != (~lo & 0xFF):
        return None
    if address > 0xFFFF:
        return None
    a_lo, a_hi = address & 0xFF, address >> 8
    if a_hi == (~a_lo & 0xFF):
        # The "extended" address is really a standard one.
        return {"proto": "nec", "address": a_lo, "command": lo}
    if address <= 0xFF:
        return None  # the blaster would send the wrong check byte
    return {"proto": "nec", "address": address, "command": lo}


def raw_code(block, stats):
    """A Flipper raw block -> the blaster's raw code, or None."""
    try:
        freq = int(float(block.get("frequency", "38000")))
        timings = [int(t) for t in block.get("data", "").split()]
    except ValueError:
        stats.add("raw: unreadable, skipped")
        return None
    if not CARRIER_MIN <= freq <= CARRIER_MAX:
        stats.add(f"raw: carrier {freq} Hz outside the blaster's range, skipped")
        return None
    timings = [abs(t) for t in timings if t != 0]
    # A recording ends with a mark (the LED is off afterwards anyway); a
    # trailing space only delays the reply.
    if len(timings) % 2 == 0:
        timings = timings[:-1]
    if not timings:
        stats.add("raw: empty, skipped")
        return None
    if len(timings) > RAW_MAX_LEN:
        stats.add("raw: longer than 1024 durations, skipped")
        return None
    if any(t > RAW_MAX_US for t in timings):
        # A long gap in the middle (usually the pause before a repeat
        # frame, ~40 ms for NEC). Shortened to the blaster's maximum:
        # receivers only need "a long pause" there. Counted, so it shows
        # in the report.
        stats.add("raw: gap shortened to 32767 us")
        timings = [min(t, RAW_MAX_US) for t in timings]
    stats.add("raw: kept")
    return {"proto": "raw", "carrier_hz": freq, "timings": timings}


def block_to_code(block, stats):
    """One button block -> a code (blaster form or parsed), or None."""
    kind = block["type"].lower()
    if kind == "raw":
        return raw_code(block, stats)
    if kind != "parsed":
        stats.add(f"unknown type {kind!r}, skipped")
        return None
    proto = block.get("protocol", "").strip().lower()
    address = le_bytes(block.get("address", ""))
    command = le_bytes(block.get("command", ""))
    if not proto or address is None or command is None:
        stats.add("parsed: unreadable, skipped")
        return None
    if proto in ("nec", "necext"):
        code = nec_code(proto, address, command)
        if code is not None:
            stats.add(f"{proto}: ready for the blaster")
            return code
        stats.add(f"{proto}: kept parsed (needs the hub's NEC encoder)")
    else:
        stats.add(f"{proto}: kept parsed (needs a hub encoder)")
    return {"proto": proto, "address": address, "command": command}


def button_name(name):
    """Flipper names are free text ("Vol_up", "POWER"): underscores become
    spaces, whitespace is squeezed, cut to the hub's limit."""
    name = re.sub(r"\s+", " ", name.replace("_", " ")).strip()
    return name[:BUTTON_NAME_MAX].rstrip()


def set_name(file_stem):
    return re.sub(r"\s+", " ", file_stem.replace("_", " ")).strip()


def read_set(path, set_id, stats):
    """One .ir file -> a code set, or None if no button is usable."""
    with open(path, encoding="utf-8", errors="replace") as f:
        blocks = parse_ir_file(f.read())
    buttons, seen = [], set()
    for block in blocks:
        name = button_name(block["name"])
        if not name:
            stats.add("button without a name, skipped")
            continue
        code = block_to_code(block, stats)
        if code is None:
            continue
        # Button names are the key on the hub: the same name twice in a
        # file (a second recording of POWER) keeps the first.
        if name.lower() in seen:
            stats.add("button name repeated in its file, skipped")
            continue
        if len(buttons) == BUTTONS_MAX:
            stats.add("more than 100 buttons in a file, rest skipped")
            break
        seen.add(name.lower())
        buttons.append([name, code])
    if not buttons:
        stats.add("file without a usable button, skipped")
        return None
    stem = os.path.splitext(os.path.basename(path))[0]
    return {"id": set_id, "name": set_name(stem), "buttons": buttons}


def read_type(root, folder, stats):
    """All sets of one type folder -> {brand: [sets]}. The brand is the
    first folder under the type (TVs/LG/...); files directly in the type
    folder, and Flipper's "Unknown" folders, go under GENERIC."""
    brands = {}
    base = os.path.join(root, folder)
    for dirpath, dirnames, filenames in os.walk(base):
        dirnames.sort()  # same output every run
        for fn in sorted(filenames):
            if not fn.lower().endswith(".ir"):
                continue
            path = os.path.join(dirpath, fn)
            rel = os.path.relpath(path, base)
            parts = rel.split(os.sep)
            brand = parts[0].replace("_", " ") if len(parts) > 1 else GENERIC
            # Flipper's "Unknown" folders hold no-name remotes (most cheap
            # LED strips): that's what the person should look for.
            if brand.lower() == "unknown":
                brand = GENERIC
            set_id = folder + "/" + os.path.splitext(rel)[0].replace(os.sep, "/")
            s = read_set(path, set_id, stats)
            if s is not None:
                brands.setdefault(brand, []).append(s)
    return brands


def source_commit(root):
    """The checkout's git commit, so the library says what it was made
    from. None when it isn't a git checkout (e.g. a Yocto tarball; the
    recipe then passes --commit)."""
    try:
        out = subprocess.run(["git", "-C", root, "rev-parse", "HEAD"],
                             capture_output=True, text=True, check=True)
        return out.stdout.strip()
    except (OSError, subprocess.CalledProcessError):
        return None


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("irdb", help="Flipper-IRDB checkout")
    ap.add_argument("out", help="output directory (created)")
    ap.add_argument("--commit", help="source commit, if not a git checkout")
    args = ap.parse_args()

    stats = Stats()
    source = {
        "name": "Flipper-IRDB",
        "url": "https://github.com/Lucaslhm/Flipper-IRDB",
        "licence": "CC0-1.0",
        "commit": args.commit or source_commit(args.irdb),
    }
    os.makedirs(args.out, exist_ok=True)
    index = {"format": FORMAT, "source": source, "types": []}
    for folder, (type_id, type_name) in TYPES.items():
        if not os.path.isdir(os.path.join(args.irdb, folder)):
            print(f"warning: {folder}/ not in the database", file=sys.stderr)
            continue
        brands = read_type(args.irdb, folder, stats)
        doc = {"format": FORMAT, "type": type_id, "name": type_name,
               "source": source, "brands": brands}
        # Compact (no spaces): the hub reads it, people use the index.
        with open(os.path.join(args.out, type_id + ".json"), "w") as f:
            json.dump(doc, f, separators=(",", ":"), sort_keys=False)
        index["types"].append({
            "id": type_id,
            "name": type_name,
            "brands": {b: len(sets) for b, sets in sorted(brands.items())},
        })
    with open(os.path.join(args.out, "index.json"), "w") as f:
        json.dump(index, f, indent=1)

    print(f"IR library from {source['name']} {source['commit'] or '(commit unknown)'}:")
    for t in index["types"]:
        print(f"  {t['id']:13s} {len(t['brands']):4d} brands {sum(t['brands'].values()):5d} sets")
    print("Codes:")
    stats.report(sys.stdout)


if __name__ == "__main__":
    main()
