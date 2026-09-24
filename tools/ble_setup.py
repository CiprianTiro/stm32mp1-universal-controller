#!/usr/bin/env python3
"""Sets a hub up over Bluetooth, the way the phone app will (issue #36).

On the hub's screen open Settings -> Paired devices -> Pair a new device
(the hub is only visible over Bluetooth while that code is shown), then:

    python3 tools/ble_setup.py

It asks for the code on the hub's screen and the WiFi to join, finds the
hub, and then (see backend_daemon/src/ble.rs for the protocol):
  1. agrees on an encryption key with the hub via SPAKE2 -- derived from the
     code, which itself is never sent -- and checks the hub used the same
     code before anything secret leaves this computer;
  2. sends the WiFi name and password encrypted (AES-256-GCM);
  3. gets back a key for the hub's LAN connection (the same pairing as
     `hub_ws.py pair`), saved in ~/.config/universal-controller/hubs.json
     under the name given with --host (default stm32mp1.local);
  4. waits for the hub to report whether it joined the WiFi.

Needs a Bluetooth adapter and: pip install bleak spake2 cryptography
This file is also the reference implementation for the phone app (#48).
"""
import argparse
import asyncio
import getpass
import hashlib
import hmac
import json
import os
import socket
import sys
from pathlib import Path

from bleak import BleakClient, BleakScanner
from cryptography.hazmat.primitives import hashes
from cryptography.hazmat.primitives.ciphers.aead import AESGCM
from cryptography.hazmat.primitives.kdf.hkdf import HKDF
from spake2 import SPAKE2_A

# Must match ble.rs.
SERVICE_UUID = "7f1c0000-5a3b-4c6e-9d2a-1b3c5d7e9f00"
RX_UUID = "7f1c0001-5a3b-4c6e-9d2a-1b3c5d7e9f00"
TX_UUID = "7f1c0002-5a3b-4c6e-9d2a-1b3c5d7e9f00"
SALT = AAD = b"uc-ble-v1"
ID_APP, ID_HUB = b"uc-app", b"uc-hub"
CREDENTIALS = Path.home() / ".config" / "universal-controller" / "hubs.json"


def hkdf(key, info):
    return HKDF(algorithm=hashes.SHA256(), length=32, salt=SALT, info=info).derive(key)


def seal(enc, plaintext):
    nonce = os.urandom(12)
    return nonce + AESGCM(enc).encrypt(nonce, plaintext, AAD)


def open_sealed(enc, data):
    return AESGCM(enc).decrypt(data[:12], data[12:], AAD)


def frame(kind, payload):
    return bytes([kind]) + len(payload).to_bytes(2, "big") + payload


async def request(client, kind, payload, expect, timeout=40.0):
    """Writes one request, then reads the answer until it's of type
    `expect` (or an error). The hub keeps the last answer for us."""
    await client.write_gatt_char(RX_UUID, frame(kind, payload), response=True)
    loop = asyncio.get_running_loop()
    deadline = loop.time() + timeout
    while loop.time() < deadline:
        answer = bytes(await client.read_gatt_char(TX_UUID))
        if answer[:1] == bytes([0xFF]):
            sys.exit(f"Hub: {answer[1:].decode(errors='replace')}")
        if answer[:1] == bytes([expect]):
            return answer[1:]
        await asyncio.sleep(0.5)
    sys.exit("No answer from the hub in time")


def save_credentials(host, token, fingerprint):
    try:
        all_credentials = json.loads(CREDENTIALS.read_text())
    except (OSError, ValueError):
        all_credentials = {}
    all_credentials[host] = {"token": token, "fingerprint": fingerprint}
    CREDENTIALS.parent.mkdir(parents=True, exist_ok=True)
    os.chmod(CREDENTIALS.parent, 0o700)
    fd = os.open(CREDENTIALS, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w") as f:
        json.dump(all_credentials, f, indent=2)


async def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--host", default="stm32mp1.local", help="name to save the LAN key under")
    args = parser.parse_args()

    # Everything is asked BEFORE searching: the PC's Bluetooth system
    # forgets a found device shortly after the search ends, and connecting
    # to it then fails with "device not found" (seen when the questions
    # came in between).
    code = input("Code shown on the hub's screen: ").replace(" ", "")
    ssid = input("WiFi to join: ")
    password = getpass.getpass(f"Password for {ssid} (empty = open network): ")

    print("Looking for a hub in Bluetooth setup mode (15 s)...")
    device = await BleakScanner.find_device_by_filter(
        lambda d, adv: SERVICE_UUID in [u.lower() for u in adv.service_uuids], timeout=15.0
    )
    if device is None:
        sys.exit("No hub found. Is the pairing code shown on the hub's screen?")
    print(f"Found {device.name or device.address}, connecting...")

    async with BleakClient(device) as client:
        # 1. Key exchange.
        spake = SPAKE2_A(code.encode(), idA=ID_APP, idB=ID_HUB)
        message_a = spake.start()
        answer = await request(client, 0x01, message_a, expect=0x01)
        message_b, confirm = answer[:33], answer[33:]
        key = spake.finish(message_b)
        enc, confirm_key = hkdf(key, b"enc"), hkdf(key, b"confirm")
        expected = hmac.new(confirm_key, message_a + message_b, hashlib.sha256).digest()
        if not hmac.compare_digest(expected, confirm):
            sys.exit("Wrong code (the hub derived a different key). Nothing secret was sent.")
        print("Code confirmed, the connection is encrypted.")

        # 2. The WiFi details, encrypted.
        setup = {"ssid": ssid, "password": password, "client_name": f"ble_setup.py on {socket.gethostname()}"}
        answer = await request(client, 0x02, seal(enc, json.dumps(setup).encode()), expect=0x02)
        paired = json.loads(open_sealed(enc, answer))
        save_credentials(args.host, paired["token"], paired["hub_fingerprint"])
        print(f"Paired as {paired['client_id']}; LAN key saved for {args.host} in {CREDENTIALS}")

        # 3. The WiFi result.
        # (Nothing to send: the hub replaces its answer with the result
        # once the attempt is over; just keep reading.)
        print(f"The hub is joining {ssid}...")
        loop = asyncio.get_running_loop()
        deadline = loop.time() + 40
        while loop.time() < deadline:
            raw = bytes(await client.read_gatt_char(TX_UUID))
            if raw[:1] == bytes([0x03]):
                result = json.loads(open_sealed(enc, raw[1:]))
                if result.get("wifi") == "joined":
                    print(f"Done: the hub joined {ssid}.")
                else:
                    print(f"The hub could not join {ssid}: {result.get('error')}")
                return
            await asyncio.sleep(1)
        print("No WiFi result in time; check the hub's screen.")


asyncio.run(main())
