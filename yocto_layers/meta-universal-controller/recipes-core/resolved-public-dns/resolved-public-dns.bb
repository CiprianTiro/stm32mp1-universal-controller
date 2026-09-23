SUMMARY = "Public DNS servers alongside the DHCP-provided ones"
DESCRIPTION = "Drop-in for systemd-resolved that adds Cloudflare and Google \
DNS as global servers, so a misbehaving ISP resolver can't cut the hub off \
from its cloud broker. See files/50-public-dns.conf for the full reasoning."
LICENSE = "MIT"
LIC_FILES_CHKSUM = "file://${COMMON_LICENSE_DIR}/MIT;md5=0835ade698e0bcf8506ecda2f7b4f302"

inherit allarch

SRC_URI = "file://50-public-dns.conf"

S = "${WORKDIR}"

# A drop-in directory instead of editing resolved.conf itself: systemd reads
# every *.conf in resolved.conf.d/ after the main file, so this recipe never
# has to patch or bbappend the systemd recipe (which would rebuild systemd).
do_install() {
    install -d ${D}${sysconfdir}/systemd/resolved.conf.d
    install -m 0644 ${WORKDIR}/50-public-dns.conf ${D}${sysconfdir}/systemd/resolved.conf.d/
}

FILES:${PN} = "${sysconfdir}/systemd/resolved.conf.d/50-public-dns.conf"
