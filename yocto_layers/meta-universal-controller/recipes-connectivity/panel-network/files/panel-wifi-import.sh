#!/bin/sh
# panel-wifi-import -- the wall panel's WiFi, from the SD card (issue #107).
#
# The panel has no setup screen for its own WiFi yet. Instead, as on
# Raspberry Pi OS: write a file on the SD card's boot partition (FAT, so
# any PC can) and the panel takes it at its next start:
#
#   /boot/wifi.txt
#     ssid=MyNetwork
#     password=my secret
#     country=RO
#
# It becomes /etc/wpa_supplicant/wpa_supplicant-wlan0.conf (root only),
# with the password stored as its hash (wpa_passphrase), and wifi.txt is
# DELETED: the password doesn't stay readable on the FAT partition.
# Ethernet needs none of this.
set -eu

FILE=/boot/wifi.txt
CONF=/etc/wpa_supplicant/wpa_supplicant-wlan0.conf
[ -f "$FILE" ] || exit 0

# key=value; Windows line ends allowed; the first of each key counts.
value() {
    sed -n "s/^$1=//p" "$FILE" | head -n 1 | tr -d '\r'
}
ssid=$(value ssid)
password=$(value password)
country=$(value country)

if [ -z "$ssid" ]; then
    echo "panel-wifi-import: $FILE has no ssid= line, ignored"
    exit 0
fi

umask 077
mkdir -p "$(dirname "$CONF")"
{
    echo "ctrl_interface=/run/wpa_supplicant"
    [ -n "$country" ] && echo "country=$country"
    if [ -z "$password" ]; then
        printf 'network={\n\tssid="%s"\n\tkey_mgmt=NONE\n}\n' "$ssid"
    else
        # The hashed key only: drop the "#psk=" line with the password.
        wpa_passphrase "$ssid" "$password" | grep -v '^\s*#psk='
    fi
} > "$CONF.new"
mv "$CONF.new" "$CONF"
rm -f "$FILE"
sync
echo "panel-wifi-import: WiFi \"$ssid\" set from $FILE (file deleted)"
