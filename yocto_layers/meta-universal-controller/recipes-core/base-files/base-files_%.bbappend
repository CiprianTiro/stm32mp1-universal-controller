# fstab changes for the DK2 (hardware only):
#   1. mount the eMMC's "userfs" partition at /usr/local
#   2. grow the rootfs and userfs filesystems to fill their partitions
#
# Why growing is needed: bitbake builds each filesystem image only as big
# as its contents plus a little slack (rootfs ~147 MB, userfs ~3 MB), and
# the flash layout writes those images as-is into much bigger partitions
# (rootfs 3 GB, userfs ~10 GB). The ext4 inside stays small -- the rest of
# the partition is unused -- so the rootfs was 95% full and /usr/local
# would only have had ~3 MB. Building full-size images instead would make
# every flash write gigabytes of zeros over USB.
#
# /usr/local is where per-device data lives: things that must NOT come
# from the image because they differ per unit, and must survive rootfs
# updates (RAUC, #16) and a read-only rootfs (#18). First user: the MQTT
# device certificate and config (see backend_daemon's mqtt.rs, #26).
#
# userfs options, in order:
#   /dev/disk/by-partlabel/userfs   the partition by its GPT name, not by
#                                   mmcblk1p11 (numbering can differ)
#   nofail                          if anything goes wrong, boot continues
#                                   without it instead of hanging
#   x-systemd.makefs                if the partition has NO filesystem yet,
#                                   systemd creates an ext4 one before
#                                   mounting. It never touches a partition
#                                   that already has one, so data persists.
#   x-systemd.growfs                after mounting, grow the ext4 to fill the
#                                   whole partition (online, no unmount).
#                                   Does nothing once it is already full size.
#   x-systemd.device-timeout=10s    don't wait the default 90 s for it
#
# NOTE for #18 (read-only rootfs): growing needs a writable filesystem, so
# the rootfs growfs below must go once "/" is read-only / dm-verity. The
# image will then need a fixed size instead (IMAGE_ROOTFS_SIZE).
do_install:append:stm32mp1common() {
    # Stock line: "/dev/root  /  auto  defaults  1  1" -> add growfs to "/".
    # systemd turns this into systemd-growfs-root.service at boot.
    sed -i -E 's#^(/dev/root\s+/\s+\S+\s+)defaults#\1defaults,x-systemd.growfs#' ${D}${sysconfdir}/fstab
    grep -q 'x-systemd.growfs' ${D}${sysconfdir}/fstab || bbfatal "rootfs fstab line not found, growfs not added"

    cat >> ${D}${sysconfdir}/fstab <<'EOF'

# Per-device persistent data (certificates, config) -- see base-files bbappend.
/dev/disk/by-partlabel/userfs  /usr/local  ext4  defaults,nofail,x-systemd.makefs,x-systemd.growfs,x-systemd.device-timeout=10s  0  2
EOF
}
