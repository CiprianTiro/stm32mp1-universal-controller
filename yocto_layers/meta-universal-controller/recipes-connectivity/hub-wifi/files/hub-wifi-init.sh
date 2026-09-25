#!/bin/sh
# Runs before wpa_supplicant starts (issue #61): makes sure its config file
# exists on the userfs partition. A brand-new board (or one flashed with
# FULL_FLASH=1) has none yet; wpa_supplicant refuses to start without one.
#
# The file lives on /usr/local, not in /etc, for the same reasons as the
# MQTT identity: it's per board, must survive reflashing (#56) and rootfs
# updates, and /etc will become read-only (#18). It holds WiFi passwords,
# so the folder is 700 and the file 600 (root only).
set -eu

DIR=/usr/local/etc/universal-controller/wifi
CONF="$DIR/wpa_supplicant.conf"

umask 077
mkdir -p "$DIR"
chmod 700 "$DIR"

# -s: exists AND is not empty. An empty file (e.g. power lost while it was
# first created) is replaced too.
if [ ! -s "$CONF" ]; then
    # ctrl_interface: the control socket backend_daemon uses to scan and
    #   connect (/run/wpa_supplicant/wlan0), only accessible to root and
    #   the group hubd -- backend_daemon's user (issue #37).
    # update_config=1: lets SAVE_CONFIG write networks back to this file
    #   (wpa_supplicant writes a .tmp file and renames it into place).
    # p2p_disabled: no WiFi Direct; nothing uses it.
    # No "country=" line: the chip stays in world mode (legal everywhere)
    #   until the backend sets the hub's country.
    cat > "$CONF.tmp" <<CONF
ctrl_interface=DIR=/run/wpa_supplicant GROUP=hubd
update_config=1
p2p_disabled=1
CONF
    mv "$CONF.tmp" "$CONF"
    echo "hub-wifi: created empty WiFi configuration $CONF"
fi

# A config made before issue #37 gives the control socket to root only;
# backend_daemon no longer runs as root and would be locked out of the
# WiFi. wpa_supplicant writes this line back unchanged on every save, so it
# is fixed here, at start -- only when needed, to not rewrite the file on
# every boot. (sed -i writes a new file and renames it into place, like
# wpa_supplicant's own save; umask 077 above keeps it root-only.)
CTRL='ctrl_interface=DIR=/run/wpa_supplicant GROUP=hubd'
if ! grep -qx "$CTRL" "$CONF"; then
    sed -i "s|^ctrl_interface=.*|$CTRL|" "$CONF"
    echo "hub-wifi: WiFi control socket now for group hubd (backend_daemon)"
fi

# Root-only, even if an older wpa_supplicant (before UMask=0077 in its
# drop-in) or someone by hand left it readable for others.
chmod 600 "$CONF"
