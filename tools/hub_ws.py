#!/usr/bin/env python3
"""Tiny command-line client for backend_daemon's WebSocket API (ws.rs,
protocol v2 since issue #34 -- see the wiki's Device-Model page).

Until the add-device wizard exists, this is how devices are added, changed
and removed by hand.

    python3 tools/hub_ws.py <board> hello
    python3 tools/hub_ws.py <board> list
    python3 tools/hub_ws.py <board> add <id> <name> '<capabilities JSON>' [<room>]
    python3 tools/hub_ws.py <board> set <id> <capability> '<value JSON>'
    python3 tools/hub_ws.py <board> on|off <id>
    python3 tools/hub_ws.py <board> rename <id> <name> [<room>]
    python3 tools/hub_ws.py <board> remove <id>
    python3 tools/hub_ws.py <board> watch                    live events, Ctrl+C to stop

<board> is a host name or IP, optionally with a port (default 8080), e.g.
stm32mp1.local. Examples:

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

Everything WiFi except `net` is only accepted from the hub itself. From the
PC, go through an SSH tunnel (the connection then arrives from 127.0.0.1 on
the board), and use `localhost:18080` as the board:

    ssh -N -L 18080:127.0.0.1:8080 root@stm32mp1.local &
    python3 tools/hub_ws.py localhost:18080 wifi-scan

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
        if request["action"] == "subscribe":
            # Print every event until Ctrl+C.
            async for message in ws:
                print(json.dumps(json.loads(message)))


asyncio.run(main())
