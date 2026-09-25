SUMMARY = "Firewall and kernel hardening for the Universal Controller hub"
DESCRIPTION = "An nftables firewall that only lets in the hub's own \
services (default deny), loaded before the network comes up, and kernel \
settings (sysctl) that reveal less and refuse network tricks. Issue #37. \
The -ssh package opens SSH in the firewall; development images only."
HOMEPAGE = "https://github.com/CiprianTiro/stm32mp1-universal-controller"
LICENSE = "GPL-3.0-only"
LIC_FILES_CHKSUM = "file://${COMMON_LICENSE_DIR}/GPL-3.0-only;md5=c79ff39f19dfec6d293b95dea7b07891"

inherit systemd allarch

SRC_URI = " \
    file://hub-firewall.nft \
    file://hub-firewall.service \
    file://ssh.nft \
    file://90-hub-hardening.conf \
"

S = "${WORKDIR}"

SYSTEMD_SERVICE:${PN} = "hub-firewall.service"
SYSTEMD_AUTO_ENABLE:${PN} = "enable"

do_install() {
    install -D -m 0644 ${WORKDIR}/hub-firewall.nft ${D}${sysconfdir}/hub-firewall/hub.nft
    install -D -m 0644 ${WORKDIR}/hub-firewall.service ${D}${systemd_system_unitdir}/hub-firewall.service
    install -D -m 0644 ${WORKDIR}/ssh.nft ${D}${sysconfdir}/hub-firewall/hub.d/ssh.nft
    install -D -m 0644 ${WORKDIR}/90-hub-hardening.conf ${D}${nonarch_libdir}/sysctl.d/90-hub-hardening.conf
}

# Two packages: the firewall itself, and the addition that opens SSH (22),
# which only development images install (the production image has no sshd
# either). -ssh is listed first so its file isn't claimed by the main one.
PACKAGES =+ "${PN}-ssh"
FILES:${PN}-ssh = "${sysconfdir}/hub-firewall/hub.d/ssh.nft"
RDEPENDS:${PN}-ssh = "${PN}"

FILES:${PN} += " \
    ${sysconfdir}/hub-firewall/hub.nft \
    ${nonarch_libdir}/sysctl.d/90-hub-hardening.conf \
"
# nft, the tool that loads the rules (meta-networking).
RDEPENDS:${PN} = "nftables"
