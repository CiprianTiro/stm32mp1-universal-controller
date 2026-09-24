#!/bin/sh
# Starts avahi (the ".local" name service: stm32mp1.local) with the config
# backend_daemon wrote, if there is one (issue #36, see network.rs's
# follow_avahi): the stock config plus "allow-interfaces=<the link that
# carries traffic>".
#
# Why: with the cable AND WiFi on the same home network, the hub has two
# addresses there. Announcing the name on both, avahi hears its own
# announcement from the other interface with a different address, takes it
# for another device using the same name, and renames the hub
# "stm32mp1-2.local" -- everything using stm32mp1.local then fails. Seen on
# the DK2 every now and then since #61, reproducibly when the WiFi
# reconnected while the cable was in. On one interface there's nothing to
# clash with.
#
# Without that file (early in boot, before the backend runs, or on a system
# without it) avahi starts exactly as its own service would.
CONF=/run/hub-avahi/avahi-daemon.conf
if [ -f "$CONF" ]; then
    exec /usr/sbin/avahi-daemon -s -f "$CONF"
fi
exec /usr/sbin/avahi-daemon -s
