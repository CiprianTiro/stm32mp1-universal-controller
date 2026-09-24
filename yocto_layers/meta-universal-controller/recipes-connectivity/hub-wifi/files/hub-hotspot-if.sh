#!/bin/sh
# Creates / removes the setup hotspot's interface (issue #36).
#
# The DK2's WiFi chip can be a normal WiFi client (wlan0) and an access point
# at the same time, as two interfaces -- verified on the board: the AP and
# the client both run, as long as they're on the same channel (hotspot.rs
# picks wlan0's channel).
#
# uap0's MAC address is left to the driver, on purpose. brcmfmac gives a
# second interface its own unique address, and the chip's FIRMWARE uses that
# address for the access point. An earlier version set a different one with
# `ip link set uap0 address ...`: Linux then announced that address to
# phones (ARP), but the firmware kept its own -- so a phone's packets for the
# hub were addressed to a MAC the chip didn't consider its own, and were
# dropped. Broadcasts (DHCP, ARP) still worked, so the phone got an address
# but the setup page timed out (found on the DK2, #36).
set -eu

case "${1:-}" in
up)
    iw dev uap0 info >/dev/null 2>&1 || iw dev wlan0 interface add uap0 type __ap
    ;;
down)
    iw dev uap0 del 2>/dev/null || true
    ;;
*)
    echo "usage: $0 up|down" >&2
    exit 2
    ;;
esac
