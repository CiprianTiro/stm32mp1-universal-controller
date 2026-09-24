#!/usr/bin/env python3
"""Tiny command-line client for backend_daemon's WebSocket API (ws.rs).

Until the add-device wizard exists, this is how devices are added, changed
and removed by hand -- e.g. to test the persistent registry (issue #33).

    python3 tools/hub_ws.py <board> list
    python3 tools/hub_ws.py <board> set <id> <key>=<value> [<key>=<value> ...]
    python3 tools/hub_ws.py <board> remove <id>

Network (issue #61):

    python3 tools/hub_ws.py <board> net                      status of Ethernet/WiFi
    python3 tools/hub_ws.py <board> wifi-scan
    python3 tools/hub_ws.py <board> wifi-connect <ssid> [<password>]
    python3 tools/hub_ws.py <board> wifi-forget
    python3 tools/hub_ws.py <board> wifi-country <XX>

Everything except `net` is only accepted from the hub itself. From the PC,
go through an SSH tunnel (the connection then arrives from 127.0.0.1 on
the board), and use `localhost:18080` as the board:

    ssh -N -L 18080:127.0.0.1:8080 root@stm32mp1.local &
    python3 tools/hub_ws.py localhost:18080 wifi-scan

<board> is a host name or IP, e.g. stm32mp1.local. Values are parsed as JSON
when possible (true, 80, "text"), otherwise used as plain strings:

    python3 tools/hub_ws.py stm32mp1.local set lamp-1 on=true brightness=80

Needs the `websockets` package (pip install websockets).
"""
import asyncio
import json
import sys

import websockets


def parse_value(text):
    try:
        return json.loads(text)
    except json.JSONDecodeError:
        return text


def build_request(args):
    """Turns the command-line words after <board> into one ws.rs request."""
    match args:
        case ["list"]:
            return {"action": "get_all_devices"}
        case ["set", device_id, *pairs] if pairs and all("=" in p for p in pairs):
            properties = {k: parse_value(v) for k, v in (p.split("=", 1) for p in pairs)}
            return {"action": "update_device", "id": device_id, "properties": properties}
        case ["remove", device_id]:
            return {"action": "remove_device", "id": device_id}
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
    sys.exit(__doc__)


async def main():
    if len(sys.argv) < 3:
        sys.exit(__doc__)
    request = build_request(sys.argv[2:])
    # "host" or "host:port" (default port 8080).
    board = sys.argv[1] if ":" in sys.argv[1] else f"{sys.argv[1]}:8080"
    # A WiFi connect can take up to ~25 s on the hub; don't give up sooner.
    async with websockets.connect(f"ws://{board}/ws", open_timeout=10, ping_timeout=60) as ws:
        await ws.send(json.dumps(request))
        print(json.dumps(json.loads(await ws.recv()), indent=2))


asyncio.run(main())
