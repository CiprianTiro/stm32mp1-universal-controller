#!/usr/bin/env python3
"""Tiny command-line client for backend_daemon's WebSocket API (ws.rs,
protocol v2 since issue #34 -- see the wiki's Device-Model page).

Virtual (test) devices are added by hand with `add`; real devices with
`add-device`, the same setup wizard the touchscreen shows (issue #40).

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

Virtual test devices for the capabilities of issue #77 (cover, climate,
lock, energy), and controlling them:

    python3 tools/hub_ws.py <board> add blind "Blind" '{"cover": {"position": 50, "can_position": true}}'
    python3 tools/hub_ws.py <board> action blind cover open          (also close, stop)
    python3 tools/hub_ws.py <board> set blind cover '{"position": 30}'
    python3 tools/hub_ws.py <board> add ac "AC" '{"climate": {"mode": "off", "target": 22, "current": 25.5, "fans": ["auto", "low", "high"], "fan": "auto"}}'
    python3 tools/hub_ws.py <board> set ac climate '{"mode": "cool", "target": 23}'
    python3 tools/hub_ws.py <board> add door "Front door" '{"lock": {"state": "locked"}}'
    python3 tools/hub_ws.py <board> set door lock '{"state": "unlocked", "confirmed": true}'
    python3 tools/hub_ws.py <board> add meter "Meter" '{"energy": {"power_w": 40.2, "energy_kwh": 1.23}}'

(Unlocking without "confirmed": true is refused: the unlock rule, wiki
Device-Model. Energy is read-only: a virtual meter keeps what it was added with.)

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

The hub's location, for sunrise/sunset automations (issue #47), in degrees
(north and east positive), e.g. Bucharest:

    python3 tools/hub_ws.py <board> set-location 44.43 26.10

Scenes and automations (issue #47). The JSON format is on the wiki
(Automations page); saving checks it like the hub would run it:

    python3 tools/hub_ws.py <board> automations                  scenes + automations
    python3 tools/hub_ws.py <board> save-scene '{"name": "Movie", "steps": [
        {"device": "lamp-1", "capability": "dimmer", "value": {"level": 20}}]}'
    python3 tools/hub_ws.py <board> run-scene movie
    python3 tools/hub_ws.py <board> delete-scene movie
    python3 tools/hub_ws.py <board> save-automation '{"name": "Evening",
        "triggers": [{"type": "sun", "event": "sunset", "offset_min": -15}],
        "steps": [{"scene": "movie"}]}'
    python3 tools/hub_ws.py <board> disable-automation evening   (enable-automation)
    python3 tools/hub_ws.py <board> run-automation evening       its steps now (testing)
    python3 tools/hub_ws.py <board> delete-automation evening
    python3 tools/hub_ws.py <board> log                          what ran (since the hub started)

A device's secret webhook address (issue #72): calling it makes the hub
read that device's state at once (e.g. a Shelly's "action" on switching):

    python3 tools/hub_ws.py <board> webhook <id>

Remote control and other actions (issue #44): one-off requests that
change no state by themselves -- a remote's button, typed text, a TV's
channel list:

    python3 tools/hub_ws.py <board> press <id> <BUTTON>       UP DOWN LEFT RIGHT OK BACK HOME
                                                              MENU VOLUME_UP CHANNEL_UP PLAY ...
    python3 tools/hub_ws.py <board> type <id> '<text>'        into the TV's text field (+ Enter)
    python3 tools/hub_ws.py <board> action <id> media apps
    python3 tools/hub_ws.py <board> action <id> media launch '{"app": "netflix"}'
    python3 tools/hub_ws.py <board> action <id> media channels                 first 100, and "total"
    python3 tools/hub_ws.py <board> action <id> media channels '{"query": "pro", "offset": 0, "limit": 100}'
    python3 tools/hub_ws.py <board> action <id> media tune '{"channel": "<id from the list>"}'

An IR remote device as a light (issue #85): its switch, colour and
brightness buttons; ORDER = what the strip shows for Red, Green, Blue
(RGB if wired right, GRB if Red and Green are swapped):

    python3 tools/hub_ws.py <board> ir-light <id> on [ORDER]
    python3 tools/hub_ws.py <board> ir-light <id> off

Adding real devices (issue #40): the hub's setup wizard, step by step in
the terminal. `found` lists what the hub sees on the network; pick one with
found=<address>, or start from a device type (and optionally its way in):

    python3 tools/hub_ws.py <board> found                    "Found on your network"
    python3 tools/hub_ws.py <board> discover                 search the network now
    python3 tools/hub_ws.py <board> hosts                    who is on the network, with makers (#73)
    python3 tools/hub_ws.py <board> templates                device types that can be added
    python3 tools/hub_ws.py <board> add-device wled found=192.168.1.139
    python3 tools/hub_ws.py <board> add-device wled advanced
    python3 tools/hub_ws.py <board> pair-again <id>          e.g. a TV shown "unauthorized"
    python3 tools/hub_ws.py <board> reconfigure <id>         change its address/settings

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
import getpass
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
        case ["set-location", latitude, longitude]:
            return {"action": "set_settings", "latitude": float(latitude), "longitude": float(longitude)}
        case ["automations"]:
            return {"action": "list_automations"}
        case ["save-scene", scene]:
            return {"action": "save_scene", "scene": json.loads(scene)}
        case ["run-scene", scene_id]:
            return {"action": "run_scene", "id": scene_id}
        case ["delete-scene", scene_id]:
            return {"action": "delete_scene", "id": scene_id}
        case ["save-automation", automation]:
            return {"action": "save_automation", "automation": json.loads(automation)}
        case ["enable-automation" | "disable-automation" as what, automation_id]:
            return {"action": "set_automation_enabled", "id": automation_id, "enabled": what.startswith("enable")}
        case ["run-automation", automation_id]:
            return {"action": "run_automation", "id": automation_id}
        case ["delete-automation", automation_id]:
            return {"action": "delete_automation", "id": automation_id}
        case ["log"]:
            return {"action": "get_automation_log"}
        case ["webhook", device_id]:
            return {"action": "webhook_url", "id": device_id}
        case ["press", device_id, button]:
            return {"action": "device_action", "id": device_id, "capability": "remote",
                    "name": "press", "args": {"button": button}}
        case ["ir-light", device_id, "on" | "off" as what, *order] if len(order) <= 1:
            args = {"enabled": what == "on"}
            if order:
                args["order"] = order[0].upper()
            return {"action": "device_action", "id": device_id, "capability": "remote",
                    "name": "light", "args": args}
        case ["type", device_id, text]:
            return {"action": "device_action", "id": device_id, "capability": "remote",
                    "name": "type", "args": {"text": text}}
        case ["action", device_id, capability, name, *args] if len(args) <= 1:
            return {"action": "device_action", "id": device_id, "capability": capability,
                    "name": name, "args": json.loads(args[0]) if args else {}}
        case ["found"]:
            return {"action": "list_found"}
        case ["discover"]:
            return {"action": "discover_now"}
        case ["hosts"]:
            return {"action": "network_hosts"}
        case ["templates"]:
            return {"action": "list_templates"}
        case ["pair-again", device_id]:
            return {"action": "wizard_reauth", "device": device_id}
        case ["reconfigure", device_id]:
            return {"action": "wizard_reconfigure", "device": device_id}
        case ["add-device", template, *rest] if len(rest) <= 1:
            request = {"action": "wizard_start", "template": template}
            if rest and rest[0].startswith("found="):
                request["found"] = rest[0].removeprefix("found=")
            elif rest:
                request["variant"] = rest[0]
            return request
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


async def ask(ws, request):
    await ws.send(json.dumps(request))
    return json.loads(await ws.recv())


def answer_step(view):
    """Shows one wizard step and asks for its answer: ("answer", values),
    or ("finish", name, room) on the last one."""
    step = view["step"]
    print(f"\n-- step {view['number']}: {step}")
    if step == "info":
        print(view["text"])
        input("(Enter to go on) ")
        return ("answer", {})
    if step == "discover":
        found = view["found"]
        for i, device in enumerate(found, 1):
            print(f"  {i}. {device['name']}  ({device['address']})")
        if not found:
            print("  nothing found yet")
        pick = input("Number to pick, Enter to search again: ").strip()
        if pick.isdigit() and 1 <= int(pick) <= len(found):
            return ("answer", {"found": found[int(pick) - 1]["address"]})
        return ("answer", {})
    if step in ("form", "choice", "code_from_device"):
        fields = view["fields"] if step == "form" else [view["field"]]
        values = {}
        for field in fields:
            if field.get("hint"):
                print(f"  ({field['hint']})")
            for choice in field.get("choices", []):
                print(f"  {choice['value']}: {choice['label']}")
            current = field.get("value") or ""
            prompt = f"{field['label']}{' [' + current + ']' if current else ''}: "
            if field["type"] == "secret":
                typed = getpass.getpass(prompt)
            else:
                typed = input(prompt).strip() or current
            values[field["id"]] = typed
        return ("answer", values)
    if step == "confirm_on_device":
        print(view["hint"])
        for hint in view["hints"]:
            print(f"  - {hint}")
        print(f"waiting up to {view['timeout_s']} s for the confirmation...")
        return ("answer", {})
    if step == "test":
        print("Testing...")
        return ("answer", {})
    if step == "save":
        if view["summary"]:
            print(f"Found: {view['summary']}")
        input(f"Save the changes to {view['device']!r}? (Enter = yes, Ctrl+C = no) ")
        return ("finish", "", "")
    if step == "name":
        if view["summary"]:
            print(f"Found: {view['summary']}")
        name = input(f"Name [{view['name']}]: ").strip() or view["name"]
        room = input(f"Room [{view['room']}]: ").strip() or view["room"]
        return ("finish", name, room)
    sys.exit(f"this tool doesn't know the step {step!r}")


async def run_wizard(ws, start):
    """Walks through the hub's wizard (ws.rs wizard_*) until the device is
    added. A refused answer shows the reason and asks the same step again."""
    reply = await ask(ws, start)
    if reply["type"] != "wizard_step":
        sys.exit(reply.get("message", reply))
    view = reply
    try:
        while True:
            action = answer_step(view)
            if action[0] == "finish":
                reply = await ask(ws, {"action": "wizard_finish", "session": view["session"],
                                       "name": action[1], "room": action[2]})
            else:
                reply = await ask(ws, {"action": "wizard_answer", "session": view["session"], "values": action[1]})
            if reply["type"] == "device":
                device = reply["device"]
                print(f"\nDone: {device['name']!r} (id {device['id']}).")
                return
            if reply["type"] == "wizard_error":
                where = f" [{reply['field']}]" if reply.get("field") else ""
                print(f"\n!! {reply['message']}{where}")
                if reply.get("detail"):
                    print(f"   ({reply['detail']})")
                if not reply.get("session"):
                    return
                # Retrying a test or pairing step unchanged fails the same
                # way: let the person go back (to fix an address) or stop.
                choice = input("Enter = try again, b = back, c = cancel: ").strip().lower()
                if choice == "c":
                    await ws.send(json.dumps({"action": "wizard_cancel", "session": view["session"]}))
                    print("cancelled")
                    return
                if choice == "b":
                    reply = await ask(ws, {"action": "wizard_back", "session": view["session"]})
                    if reply["type"] == "wizard_step":
                        view = reply
                    else:
                        print(f"!! {reply.get('message', reply)}")
                continue  # the (same or previous) step again
            view = reply
    except (KeyboardInterrupt, EOFError):
        await ws.send(json.dumps({"action": "wizard_cancel", "session": view["session"]}))
        print("\ncancelled")


async def main():
    if len(sys.argv) < 3:
        sys.exit(__doc__)
    request = build_request(sys.argv[2:])
    if request["action"] == "pair":
        await pair(sys.argv[1].partition(":")[0])
        return
    ws = await connect(sys.argv[1])
    try:
        if request["action"] in ("wizard_start", "wizard_reauth", "wizard_reconfigure"):
            await run_wizard(ws, request)
            return
        await ws.send(json.dumps(request))
        print(json.dumps(json.loads(await ws.recv()), indent=2))
        # "type" means typing AND sending it, as on a phone keyboard.
        if request["action"] == "device_action" and request["name"] == "type":
            await ws.send(json.dumps({**request, "name": "submit", "args": {}}))
            print(json.dumps(json.loads(await ws.recv()), indent=2))
        if request["action"] == "subscribe":
            # Print every event until Ctrl+C.
            async for message in ws:
                print(json.dumps(json.loads(message)))
    finally:
        await ws.close()


asyncio.run(main())
