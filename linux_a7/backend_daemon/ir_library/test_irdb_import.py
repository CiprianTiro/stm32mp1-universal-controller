#!/usr/bin/env python3
"""Tests for irdb_import.py:  python3 -m unittest linux_a7/backend_daemon/ir_library/test_irdb_import.py"""
import json
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import irdb_import as imp  # noqa: E402

# A small .ir file in Flipper's format: the header block (no name), two NEC
# buttons, a repeated name, and a raw recording with a long repeat gap.
SAMPLE = """Filetype: IR signals file
Version: 1
#
name: POWER
type: parsed
protocol: NEC
address: 00 00 00 00
command: 40 00 00 00
#
name: Vol_up
type: parsed
protocol: NECext
address: 00 EF 00 00
command: 03 FC 00 00
#
name: power
type: parsed
protocol: NEC
address: 00 00 00 00
command: 41 00 00 00
#
name: Blue
type: raw
frequency: 38000
duty_cycle: 0.330000
data: 9000 4500 560 560 560 40567 9000 2250 560 1000
"""


class ConvertTests(unittest.TestCase):
    def test_le_bytes(self):
        self.assertEqual(imp.le_bytes("00 EF 00 00"), 0xEF00)
        self.assertEqual(imp.le_bytes("40 00 00 00"), 0x40)
        self.assertIsNone(imp.le_bytes("zz"))

    def test_nec(self):
        self.assertEqual(imp.nec_code("nec", 0x00, 0x40),
                         {"proto": "nec", "address": 0, "command": 0x40})

    def test_necext_extended_address(self):
        # The standard 24-key LED strip remote: address 0xEF00, ON = 0x03.
        self.assertEqual(imp.nec_code("necext", 0xEF00, 0xFC03),
                         {"proto": "nec", "address": 0xEF00, "command": 0x03})

    def test_necext_that_is_standard(self):
        # Address bytes 04 FB: FB is ~04, so it's plain NEC address 4 (LG).
        self.assertEqual(imp.nec_code("necext", 0xFB04, 0xF708),
                         {"proto": "nec", "address": 4, "command": 8})

    def test_necext_not_fitting(self):
        # 16-bit command without the inverse check byte.
        self.assertIsNone(imp.nec_code("necext", 0xEF00, 0x1203))
        # Extended address below 0x100: the blaster would send 05 FA.
        self.assertIsNone(imp.nec_code("necext", 0x0005, 0xFC03))

    def test_raw(self):
        stats = imp.Stats()
        code = imp.raw_code({"frequency": "38000", "data": "9000 4500 560 40567 560 900"}, stats)
        # Trailing space dropped, the 40567 us gap shortened.
        self.assertEqual(code, {"proto": "raw", "carrier_hz": 38000,
                                "timings": [9000, 4500, 560, 32767, 560]})

    def test_raw_bad_carrier(self):
        self.assertIsNone(imp.raw_code({"frequency": "455000", "data": "500 500 500"}, imp.Stats()))

    def test_other_protocol_kept_parsed(self):
        block = {"name": "Power", "type": "parsed", "protocol": "Samsung32",
                 "address": "07 00 00 00", "command": "02 00 00 00"}
        self.assertEqual(imp.block_to_code(block, imp.Stats()),
                         {"proto": "samsung32", "address": 7, "command": 2})

    def test_button_name(self):
        self.assertEqual(imp.button_name("Vol_up"), "Vol up")
        self.assertEqual(len(imp.button_name("x" * 40)), imp.BUTTON_NAME_MAX)


class FileTests(unittest.TestCase):
    def test_read_set(self):
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, "LED_44Key.ir")
            with open(path, "w") as f:
                f.write(SAMPLE)
            s = imp.read_set(path, "LED_Lighting/Unknown/LED_44Key", imp.Stats())
        self.assertEqual(s["name"], "LED 44Key")
        names = [b[0] for b in s["buttons"]]
        # "power" repeats "POWER" (case-insensitive): the first one is kept.
        self.assertEqual(names, ["POWER", "Vol up", "Blue"])
        self.assertEqual(s["buttons"][0][1], {"proto": "nec", "address": 0, "command": 0x40})

    def test_whole_run(self):
        with tempfile.TemporaryDirectory() as d:
            irdb, out = os.path.join(d, "irdb"), os.path.join(d, "out")
            os.makedirs(os.path.join(irdb, "LED_Lighting", "Unknown"))
            with open(os.path.join(irdb, "LED_Lighting", "Unknown", "LED_44Key.ir"), "w") as f:
                f.write(SAMPLE)
            sys.argv = ["irdb_import.py", irdb, out, "--commit", "abc"]
            with open(os.devnull, "w") as null:
                old, sys.stdout = sys.stdout, null
                try:
                    imp.main()
                finally:
                    sys.stdout = old
            with open(os.path.join(out, "index.json")) as f:
                index = json.load(f)
            with open(os.path.join(out, "led_lighting.json")) as f:
                lib = json.load(f)
        self.assertEqual(index["source"]["commit"], "abc")
        self.assertEqual(index["types"][0]["brands"], {imp.GENERIC: 1})
        self.assertEqual(lib["brands"][imp.GENERIC][0]["id"], "LED_Lighting/Unknown/LED_44Key")


if __name__ == "__main__":
    unittest.main()
