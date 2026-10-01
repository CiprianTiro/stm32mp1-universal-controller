#!/usr/bin/env python3
# Copyright (c) 2026 Ciprian Tironeac
# SPDX-License-Identifier: Apache-2.0
"""
hub_sim.py -- plays the hub's part of PROTOCOL.md from a PC (#42).

For testing the blaster firmware without the hub: it connects with mutual
TLS, pairs with the pairing code, and sends the same requests the hub's
adapter will send. Standard library + the `openssl` command only.

It keeps a small "hub identity" of its own (key + self-signed certificate)
and the pinned blaster fingerprint in ~/.cache/uc-irb-hub-sim/, like the
real hub does in its TLS directory.

    hub_sim.py <address> hello
    hub_sim.py <address> pair ABCD-EFGH-JKMN-PQRS
    hub_sim.py <address> nec 0x00 0x40 [repeats]
    hub_sim.py <address> learn [seconds]
    hub_sim.py <address> send-raw 9000 4500 560 ...
    hub_sim.py <address> listen          # print "heard" events until Ctrl+C
    hub_sim.py <address> unpair

<address> is the blaster's IP (the console's `status` shows it) or its
mDNS name, e.g. irb-7ce8b1b091e4.local.
"""
import hashlib
import hmac
import json
import os
import socket
import ssl
import subprocess
import sys
import time

PORT = 7443
STATE_DIR = os.path.expanduser("~/.cache/uc-irb-hub-sim")
KEY = os.path.join(STATE_DIR, "hub.key")
CERT = os.path.join(STATE_DIR, "hub.crt")
PINS = os.path.join(STATE_DIR, "pins.json")


def ensure_identity():
    """Our own key + certificate (ECDSA P-256), made once with openssl."""
    if os.path.exists(KEY) and os.path.exists(CERT):
        return
    os.makedirs(STATE_DIR, mode=0o700, exist_ok=True)
    subprocess.run(
        ["openssl", "req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256",
         "-nodes", "-keyout", KEY, "-out", CERT, "-days", "36500", "-subj", "/CN=hub-sim"],
        check=True, capture_output=True)
    print(f"created test hub identity in {STATE_DIR}")


def own_fingerprint():
    with open(CERT) as f:
        der = ssl.PEM_cert_to_DER_cert(f.read())
    return hashlib.sha256(der).hexdigest()


def load_pins():
    try:
        with open(PINS) as f:
            return json.load(f)
    except FileNotFoundError:
        return {}


def save_pins(pins):
    with open(PINS, "w") as f:
        json.dump(pins, f, indent=2)


class Link:
    """One TLS connection to the blaster, JSON lines both ways."""

    def __init__(self, address):
        ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
        # No authority checks: we check the fingerprint ourselves (pinning).
        ctx.check_hostname = False
        ctx.verify_mode = ssl.CERT_NONE
        ctx.load_cert_chain(CERT, KEY)
        raw = socket.create_connection((address, PORT), timeout=10)
        self.sock = ctx.wrap_socket(raw)
        self.blaster_fp = hashlib.sha256(self.sock.getpeercert(binary_form=True)).hexdigest()
        self.hub_fp = own_fingerprint()
        self.file = self.sock.makefile("rwb")
        self.next_id = 1

    def request(self, op, timeout=10, **fields):
        """Sends a request, returns its reply; prints events that arrive meanwhile."""
        req_id = self.next_id
        self.next_id += 1
        msg = {"id": req_id, "op": op, **fields}
        self.file.write((json.dumps(msg) + "\n").encode())
        self.file.flush()
        self.sock.settimeout(timeout)
        while True:
            line = self.file.readline()
            if not line:
                raise ConnectionError("the blaster closed the connection")
            reply = json.loads(line)
            if reply.get("id") == req_id:
                return reply
            print("event:", reply)

    def events(self):
        self.sock.settimeout(None)
        while True:
            line = self.file.readline()
            if not line:
                raise ConnectionError("the blaster closed the connection")
            yield json.loads(line)


def proof(code, role, blaster_fp, hub_fp):
    code = code.replace("-", "").replace(" ", "").upper()
    message = f"uc-irb-pair-v1 {role} {blaster_fp} {hub_fp}".encode()
    return hmac.new(code.encode(), message, hashlib.sha256).hexdigest()


def main():
    if len(sys.argv) < 3:
        print(__doc__)
        sys.exit(1)
    address, cmd, args = sys.argv[1], sys.argv[2], sys.argv[3:]
    ensure_identity()
    link = Link(address)
    pins = load_pins()

    # Pinning: once paired, the blaster must show the same certificate.
    pinned = pins.get(address)
    if pinned and pinned != link.blaster_fp and cmd != "pair":
        sys.exit(f"REFUSED: {address}'s certificate changed ({link.blaster_fp[:16]}..., "
                 f"pinned {pinned[:16]}...). Not our blaster?")
    print(f"connected, blaster certificate {link.blaster_fp[:16]}...")

    if cmd == "hello":
        print(link.request("hello"))
    elif cmd == "pair":
        mine = proof(args[0], "hub", link.blaster_fp, link.hub_fp)
        reply = link.request("pair", proof=mine)
        print(reply)
        if reply.get("ok"):
            expected = proof(args[0], "blaster", link.blaster_fp, link.hub_fp)
            if not hmac.compare_digest(reply.get("proof", ""), expected):
                sys.exit("REFUSED: the blaster's proof is wrong -- not the blaster whose code this is")
            pins[address] = link.blaster_fp
            save_pins(pins)
            print("paired, blaster pinned")
    elif cmd == "nec":
        code = {"proto": "nec", "address": int(args[0], 0), "command": int(args[1], 0)}
        extra = {"repeats": int(args[2])} if len(args) > 2 else {}
        started = time.monotonic()
        print(link.request("send", code=code, **extra), f"({(time.monotonic() - started) * 1000:.0f} ms)")
    elif cmd == "send-raw":
        code = {"proto": "raw", "timings": [int(a) for a in args]}
        print(link.request("send", code=code))
    elif cmd == "learn":
        seconds = int(args[0]) if args else 15
        print(f"press a button on the remote (within {seconds} s)...")
        print(link.request("learn", timeout=seconds + 5, timeout_ms=seconds * 1000))
    elif cmd == "listen":
        print("listening for remotes, Ctrl+C to stop")
        last_ping = time.monotonic()
        link.sock.settimeout(1)
        while True:
            try:
                line = link.file.readline()
                if line:
                    print(json.loads(line))
            except (socket.timeout, TimeoutError):
                pass
            if time.monotonic() - last_ping > 20:
                link.file.write((json.dumps({"id": 0, "op": "ping"}) + "\n").encode())
                link.file.flush()
                last_ping = time.monotonic()
    elif cmd == "unpair":
        print(link.request("unpair"))
        pins.pop(address, None)
        save_pins(pins)
    else:
        print(link.request(cmd))


if __name__ == "__main__":
    main()
