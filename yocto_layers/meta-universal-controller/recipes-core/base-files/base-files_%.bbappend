# Mounts the eMMC's "userfs" partition at /usr/local (hardware only).
#
# ST's flash layout creates a large userfs partition (~10 GB on the DK2's
# eMMC), but the image never put a filesystem on it or mounted it -- the
# stock fstab only has a commented-out line pointing at the SD card
# (mmcblk0). /usr/local is where per-device data lives: things that must
# NOT come from the image because they differ per unit, and must survive
# rootfs updates (RAUC, #16) and a read-only rootfs (#18). First user: the
# MQTT device certificate and config (see backend_daemon's mqtt.rs, #26).
#
# Options, in order:
#   /dev/disk/by-partlabel/userfs   the partition by its GPT name, not by
#                                   mmcblk1p11 (numbering can differ)
#   nofail                          if anything goes wrong, boot continues
#                                   without it instead of hanging
#   x-systemd.makefs                if the partition has NO filesystem yet
#                                   (true after flashing), systemd creates
#                                   an ext4 one before mounting. It never
#                                   touches a partition that already has
#                                   one, so data persists across reboots.
#   x-systemd.device-timeout=10s    don't wait the default 90 s for it
do_install:append:stm32mp1common() {
    cat >> ${D}${sysconfdir}/fstab <<'EOF'

# Per-device persistent data (certificates, config) -- see base-files bbappend.
/dev/disk/by-partlabel/userfs  /usr/local  ext4  defaults,nofail,x-systemd.makefs,x-systemd.device-timeout=10s  0  2
EOF
}
