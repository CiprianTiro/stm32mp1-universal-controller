#!/bin/sh
# hub-helper (issue #37): does the few things backend_daemon needs root for,
# now that backend_daemon itself runs as the unprivileged user "hubd".
#
# systemd runs one copy of this script per connection to
# /run/hub-helper.sock (hub-helper.socket, Accept=yes), with the connection
# as stdin/stdout. Only root and the group hubd may connect (SocketMode=0660).
#
# It reads ONE word and knows exactly four. There are no arguments and the
# word is never passed on to anything, so there's nothing to inject into:
# the worst a compromised backend_daemon can do through here is open/close
# the setup hotspot, restart avahi, or flush one file to the flash.
#
# Answer: "OK", or "ERR <reason>". See linux_a7/backend_daemon/src/helper.rs
# for the other side.

WIFI_DIR=/usr/local/etc/universal-controller/wifi

# `read` stops at the first newline; a client that sends nothing is cut
# off by RuntimeMaxSec= in hub-helper@.service.
read -r command || exit 1

case "$command" in
    hotspot-start) output=$(systemctl start hub-hotspot.service 2>&1) ;;
    hotspot-stop)  output=$(systemctl stop hub-hotspot.service 2>&1) ;;
    avahi-restart) output=$(systemctl restart avahi-daemon.service 2>&1) ;;
    # fsync wpa_supplicant's config (root-only: it holds the WiFi passwords)
    # and its folder, after the backend had it saved (network.rs).
    wifi-sync)     output=$(sync "$WIFI_DIR/wpa_supplicant.conf" "$WIFI_DIR" 2>&1) ;;
    *)
        # Not echoed back or logged in full: it came from outside.
        echo "ERR unknown command"
        echo "hub-helper: refused an unknown command" >&2
        exit 1
        ;;
esac
status=$?

if [ "$status" -eq 0 ]; then
    echo "OK"
    echo "hub-helper: $command done" >&2
else
    # One line: the first line of systemctl's message is the useful part.
    reason=$(printf '%s' "$output" | head -n 1)
    echo "ERR $reason"
    echo "hub-helper: $command failed: $reason" >&2
fi
