#!/usr/bin/env python3
"""Tiny command-line client for backend_daemon's WebSocket API (ws.rs,
protocol v2 since issue #34 -- see the wiki's Device-Model page).

Until the add-device wizard exists, this is how devices are added, changed
and removed by hand.

PAIRING (issue #35). The hub only accepts paired clients from the LAN, over
TLS (wss://<hub>:8443). Pair once: on the hub's screen open Settings ->
Paired devices -> Pair a new device, then

    python3 tools/hub_ws.py stm32mp1.local pair

and type the 6-digit code shown there. The key and the hub's certificate
fingerprint are saved in ~/.config/universal-controller/hubs.json (readable
only by you); every later command connects with them, and refuses to talk
to a hub whose certificate doesn't match the saved fingerprint.

    python3 tools/hub_ws.py <board> pair
    python3 tools/hub_ws.py <board> hello
    python3 tools/hub_ws.py <board> list
    python3 tools/hub_ws.py <board> add <id> <name> '<capabilities JSON>' [<room>]
    python3 tools/hub_ws.py <board> set <id> <capability> '<value JSON>'
    python3 tools/hub_ws.py <board> on|off <id>
    python3 tools/hub_ws.py <board> rename <id> <name> [<room>]
    python3 tools/hub_ws.py <board> remove <id>
    python3 tools/hub_ws.py <board> watch                    live events, Ctrl+C to stop

<board> is a host name or IP (the LAN door, wss on port 8443, needs pairing),
or localhost:<port> for an SSH tunnel to the hub's own door (plain ws, see
below). Examples:

    python3 tools/hub_ws.py stm32mp1.local add lamp-1 "Desk lamp" \
        '{"switch": {"on": false}, "dimmer": {"level": 50}}' Office
    python3 tools/hub_ws.py stm32mp1.local set lamp-1 dimmer '{"level": 30}'
    python3 tools/hub_ws.py stm32mp1.local on ld7

Network (issue #61):

    python3 tools/hub_ws.py <board> net                      status of Ethernet/WiFi
    python3 tools/hub_ws.py <board> wifi-scan
    python3 tools/hub_ws.py <board> wifi-connect <ssid> [<password>]
    python3 tools/hub_ws.py <board> wifi-forget
    python3 tools/hub_ws.py <board> wifi-country <XX>

Hub settings (issue #39): the screen's design preset and the time zone.
Allowed for every paired client; the hub's screen follows at once.

    python3 tools/hub_ws.py <board> settings
    python3 tools/hub_ws.py <board> set-settings mode=light accent=violet
    python3 tools/hub_ws.py <board> set-settings density=compact time_zone=Europe/Bucharest

  mode: dark | light | auto, accent: sky | emerald | amber | violet | rose,
  density: comfortable | compact, time_zone: an IANA name.

Pairing management (normally done on the hub's screen), hub itself only:

    python3 tools/hub_ws.py localhost:18080 start-pairing    shows a code
    python3 tools/hub_ws.py localhost:18080 clients          paired clients
    python3 tools/hub_ws.py localhost:18080 revoke <client-id>

Everything WiFi except `net` is only accepted from the hub itself. From the
PC, go through an SSH tunnel (the connection then arrives from 127.0.0.1 on
the board), and use `localhost:18080` as the board:

    ssh -N -L 18080:127.0.0.1:8080 root@stm32mp1.local &
    python3 tools/hub_ws.py localhost:18080 wifi-scan

Needs the `websockets` package (pip install websockets).
"""
import asyncio
import hashlib
import json
import os
import socket
import ssl
import sys
from pathlib import Path

import websockets

# Where pairing results are kept: {"<host>": {"token": ..., "fingerprint": ...}}
CREDENTIALS = Path.home() / ".config" / "universal-controller" / "hubs.json"
LAN_PORT = 8443


def parse_value(text):
    try:
        return json.loads(text)
    except json.JSONDecodeError:
        return text


def build_request(args):
    """Turns the command-line words after <board> into one ws.rs request."""
    match args:
        case ["pair"]:
            return {"action": "pair"}  # completed in pair() below
        case ["hello"]:
            return {"action": "hello"}
        case ["list"]:
            return {"action": "list_devices"}
        case ["add", device_id, name, capabilities, *room] if len(room) <= 1:
            device = {"id": device_id, "name": name, "capabilities": json.loads(capabilities)}
            if room:
                device["room"] = room[0]
            return {"action": "add_device", "device": device}
        case ["set", device_id, capability, value]:
            return {"action": "command", "id": device_id, "capability": capability, "value": json.loads(value)}
        case ["on" | "off" as state, device_id]:
            return {"action": "command", "id": device_id, "capability": "switch", "value": {"on": state == "on"}}
        case ["rename", device_id, name, *room] if len(room) <= 1:
            request = {"action": "update_device_info", "id": device_id, "name": name}
            if room:
                request["room"] = room[0]
            return request
        case ["remove", device_id]:
            return {"action": "remove_device", "id": device_id}
        case ["watch"]:
            return {"action": "subscribe"}
        case ["net"]:
            return {"action": "get_network_status"}
        case ["wifi-scan"]:
            return {"action": "wifi_scan"}
        case ["wifi-connect", ssid]:
            return {"action": "wifi_connect", "ssid": ssid}
        case ["wifi-connect", ssid, password]:
            return {"action": "wifi_connect", "ssid": ssid, "password": password}
        case ["wifi-forget"]:
            return {"action": "wifi_forget"}
        case ["wifi-country", country]:
            return {"action": "set_wifi_country", "country": country}
        case ["start-pairing"]:
            return {"action": "start_pairing"}
        case ["clients"]:
            return {"action": "list_clients"}
        case ["revoke", client_id]:
            return {"action": "revoke_client", "id": client_id}
        case ["settings"]:
            return {"action": "get_settings"}
        case ["set-settings", *pairs] if pairs and all("=" in p for p in pairs):
            return {"action": "set_settings", **dict(p.split("=", 1) for p in pairs)}
    sys.exit(__doc__)


def load_credentials():
    try:
        return json.loads(CREDENTIALS.read_text())
    except (OSError, ValueError):
        return {}


def save_credentials(all_credentials):
    CREDENTIALS.parent.mkdir(parents=True, exist_ok=True)
    os.chmod(CREDENTIALS.parent, 0o700)
    # Written with mode 600 from the start: the key is as good as a password.
    fd = os.open(CREDENTIALS, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w") as f:
        json.dump(all_credentials, f, indent=2)


def lan_tls():
    """TLS that accepts the hub's self-made certificate; WE check it
    ourselves afterwards, against the pinned fingerprint (pinned_ok)."""
    context = ssl.create_default_context()
    context.check_hostname = False
    context.verify_mode = ssl.CERT_NONE
    return context


def fingerprint_of(ws):
    """SHA-256 of the certificate the hub presented, as hex."""
    der = ws.transport.get_extra_info("ssl_object").getpeercert(binary_form=True)
    return hashlib.sha256(der).hexdigest()


def short(fingerprint):
    return " ".join(fingerprint[i:i + 4] for i in range(0, 16, 4))


async def pair(host):
    """Pairs this PC with the hub: the code comes from the hub's screen."""
    async with websockets.connect(f"wss://{host}:{LAN_PORT}/ws", ssl=lan_tls(), open_timeout=10) as ws:
        fingerprint = fingerprint_of(ws)
        # With the QR code, the app gets the fingerprint from the hub's
        # screen. Typing the code, we can only show it and let the user
        # compare it with the screen.
        print(f"Hub certificate: {short(fingerprint)}  (the same as on the hub's pairing screen?)")
        code = input("Code shown on the hub's screen: ").strip()
        name = f"hub_ws.py on {socket.gethostname()}"
        await ws.send(json.dumps({"action": "pair", "code": code, "client_name": name}))
        reply = json.loads(await ws.recv())
        if reply.get("type") != "paired":
            sys.exit(f"Pairing failed: {reply.get('message', reply)}")
        if reply["hub_fingerprint"] != fingerprint:
            sys.exit("Pairing refused: the hub reports a different certificate than it presented")
        all_credentials = load_credentials()
        all_credentials[host] = {"token": reply["token"], "fingerprint": fingerprint}
        save_credentials(all_credentials)
        print(f"Paired as {reply['client_id']} ({name}). Saved in {CREDENTIALS}")


async def connect(board):
    """An open, logged-in connection: plain ws for localhost (SSH tunnel to
    the hub's own door), wss + pinned certificate + key for the LAN."""
    host, _, port = board.partition(":")
    if host in ("localhost", "127.0.0.1"):
        return await websockets.connect(f"ws://{host}:{port or 8080}/ws", open_timeout=10, ping_timeout=60)
    credentials = load_credentials().get(host)
    if not credentials:
        sys.exit(f"Not paired with {host} yet: run `python3 tools/hub_ws.py {host} pair` first")
    ws = await websockets.connect(f"wss://{host}:{port or LAN_PORT}/ws", ssl=lan_tls(), open_timeout=10, ping_timeout=60)
    if fingerprint_of(ws) != credentials["fingerprint"]:
        await ws.close()
        sys.exit(f"STOP: {host} presented a different certificate than when paired -- "
                 "not the same hub, or someone in between. Nothing was sent.")
    await ws.send(json.dumps({"action": "auth", "token": credentials["token"]}))
    reply = json.loads(await ws.recv())
    if reply.get("type") != "authenticated":
        await ws.close()
        sys.exit(f"Login refused: {reply.get('message', reply)}")
    return ws


async def main():
    if len(sys.argv) < 3:
        sys.exit(__doc__)
    request = build_request(sys.argv[2:])
    if request["action"] == "pair":
        await pair(sys.argv[1].partition(":")[0])
        return
    ws = await connect(sys.argv[1])
    try:
        await ws.send(json.dumps(request))
        print(json.dumps(json.loads(await ws.recv()), indent=2))
        if request["action"] == "subscribe":
            # Print every event until Ctrl+C.
            async for message in ws:
                print(json.dumps(json.loads(message)))
    finally:
        await ws.close()


asyncio.run(main())
