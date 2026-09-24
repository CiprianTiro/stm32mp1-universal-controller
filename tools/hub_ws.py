#!/usr/bin/env python3
"""Tiny command-line client for backend_daemon's WebSocket API (ws.rs).

Until the add-device wizard exists, this is how devices are added, changed
and removed by hand -- e.g. to test the persistent registry (issue #33).

    python3 tools/hub_ws.py <board> list
    python3 tools/hub_ws.py <board> set <id> <key>=<value> [<key>=<value> ...]
    python3 tools/hub_ws.py <board> remove <id>

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
    sys.exit(__doc__)


async def main():
    if len(sys.argv) < 3:
        sys.exit(__doc__)
    request = build_request(sys.argv[2:])
    async with websockets.connect(f"ws://{sys.argv[1]}:8080/ws") as ws:
        await ws.send(json.dumps(request))
        print(json.dumps(json.loads(await ws.recv()), indent=2))


asyncio.run(main())
