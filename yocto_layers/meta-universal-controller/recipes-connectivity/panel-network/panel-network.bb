SUMMARY = "Network setup for the Universal Controller wall panel"
DESCRIPTION = "Ethernet and WiFi by DHCP for the Raspberry Pi wall panel \
(issue #107). Ethernet works as it is; WiFi is set by writing wifi.txt on \
the SD card's boot partition, imported once at start and then deleted. \
WiFi gets a lower route priority than Ethernet, as on the hub."
HOMEPAGE = "https://github.com/CiprianTiro/stm32mp1-universal-controller"
LICENSE = "GPL-3.0-only"
LIC_FILES_CHKSUM = "file://${COMMON_LICENSE_DIR}/GPL-3.0-only;md5=c79ff39f19dfec6d293b95dea7b07891"

inherit systemd allarch

SRC_URI = " \
    file://25-wlan.network \
    file://panel-wifi-import.sh \
    file://panel-wifi-import.service \
    file://10-panel-wifi.conf \
"
S = "${WORKDIR}"

# wpa_passphrase hashes the password when importing wifi.txt.
RDEPENDS:${PN} = "wpa-supplicant wpa-supplicant-passphrase"

SYSTEMD_SERVICE:${PN} = "panel-wifi-import.service"
SYSTEMD_AUTO_ENABLE:${PN} = "enable"

do_install() {
    # The same wlan0 setup as the hub (hub-wifi): DHCP, Ethernet preferred.
    install -D -m 0644 ${WORKDIR}/25-wlan.network ${D}${sysconfdir}/systemd/network/25-wlan.network
    install -D -m 0755 ${WORKDIR}/panel-wifi-import.sh ${D}${libexecdir}/panel-wifi-import
    install -D -m 0644 ${WORKDIR}/panel-wifi-import.service ${D}${systemd_system_unitdir}/panel-wifi-import.service
    # wpa_supplicant on wlan0, with wpa_supplicant-wlan0.conf (what
    # `systemctl enable wpa_supplicant@wlan0` would create).
    install -D -m 0644 ${WORKDIR}/10-panel-wifi.conf \
        ${D}${systemd_system_unitdir}/wpa_supplicant@wlan0.service.d/10-panel-wifi.conf
    install -d ${D}${sysconfdir}/systemd/system/multi-user.target.wants
    ln -sf ${systemd_system_unitdir}/wpa_supplicant@.service \
        ${D}${sysconfdir}/systemd/system/multi-user.target.wants/wpa_supplicant@wlan0.service
}

FILES:${PN} += " \
    ${sysconfdir}/systemd/network/25-wlan.network \
    ${libexecdir}/panel-wifi-import \
    ${systemd_system_unitdir}/wpa_supplicant@wlan0.service.d/10-panel-wifi.conf \
    ${sysconfdir}/systemd/system/multi-user.target.wants/wpa_supplicant@wlan0.service \
"
