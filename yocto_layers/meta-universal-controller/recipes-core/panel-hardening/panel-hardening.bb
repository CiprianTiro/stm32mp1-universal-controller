SUMMARY = "Firewall and kernel hardening for the Universal Controller wall panel"
DESCRIPTION = "The wall panel's (issue #107) nftables firewall: default \
deny, nothing offered to the network, only answers to what the panel \
started (and mDNS answers when looking for hubs). Reuses the hub's \
firewall service, kernel settings and the dev-only SSH opening \
(hub-hardening, issue #37)."
HOMEPAGE = "https://github.com/CiprianTiro/stm32mp1-universal-controller"
LICENSE = "GPL-3.0-only"
LIC_FILES_CHKSUM = "file://${COMMON_LICENSE_DIR}/GPL-3.0-only;md5=c79ff39f19dfec6d293b95dea7b07891"

inherit systemd allarch

# The hub's service, sysctl settings and ssh.nft, with the panel's rules.
FILESEXTRAPATHS:prepend := "${THISDIR}/../hub-hardening/files:"
SRC_URI = " \
    file://panel-firewall.nft \
    file://hub-firewall.service \
    file://ssh.nft \
    file://90-hub-hardening.conf \
"
S = "${WORKDIR}"

SYSTEMD_SERVICE:${PN} = "hub-firewall.service"
SYSTEMD_AUTO_ENABLE:${PN} = "enable"

do_install() {
    # Installed where the hub's service loads it from.
    install -D -m 0644 ${WORKDIR}/panel-firewall.nft ${D}${sysconfdir}/hub-firewall/hub.nft
    install -D -m 0644 ${WORKDIR}/hub-firewall.service ${D}${systemd_system_unitdir}/hub-firewall.service
    install -D -m 0644 ${WORKDIR}/ssh.nft ${D}${sysconfdir}/hub-firewall/hub.d/ssh.nft
    install -D -m 0644 ${WORKDIR}/90-hub-hardening.conf ${D}${nonarch_libdir}/sysctl.d/90-hub-hardening.conf
}

# As on the hub: -ssh opens SSH, development images only.
PACKAGES =+ "${PN}-ssh"
FILES:${PN}-ssh = "${sysconfdir}/hub-firewall/hub.d/ssh.nft"
RDEPENDS:${PN}-ssh = "${PN}"
FILES:${PN} += " \
    ${sysconfdir}/hub-firewall/hub.nft \
    ${nonarch_libdir}/sysctl.d/90-hub-hardening.conf \
"
RDEPENDS:${PN} = "nftables"
# The hub's and the panel's firewall are the same files: never both.
RCONFLICTS:${PN} = "hub-hardening"
