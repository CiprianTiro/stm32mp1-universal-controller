SUMMARY = "WiFi client setup for the Universal Controller hub"
DESCRIPTION = "Runs wpa_supplicant on wlan0 with its configuration on the \
userfs partition, and gives WiFi a lower route priority than Ethernet so \
the cable is preferred and WiFi takes over when it's unplugged. Issue #61."
HOMEPAGE = "https://github.com/CiprianTiro/stm32mp1-universal-controller"
LICENSE = "GPL-3.0-only"
LIC_FILES_CHKSUM = "file://${COMMON_LICENSE_DIR}/GPL-3.0-only;md5=c79ff39f19dfec6d293b95dea7b07891"

inherit systemd

SRC_URI = " \
    file://25-wlan.network \
    file://hub-wifi-init.sh \
    file://10-hub-wifi.conf \
"

S = "${WORKDIR}"

# wpa_supplicant itself is already in the image (ST's package groups), but
# say so explicitly: without it this recipe does nothing.
RDEPENDS:${PN} = "wpa-supplicant"

do_install() {
    install -D -m 0644 ${WORKDIR}/25-wlan.network ${D}${sysconfdir}/systemd/network/25-wlan.network
    install -D -m 0755 ${WORKDIR}/hub-wifi-init.sh ${D}${libexecdir}/hub-wifi-init
    install -D -m 0644 ${WORKDIR}/10-hub-wifi.conf \
        ${D}${systemd_system_unitdir}/wpa_supplicant@wlan0.service.d/10-hub-wifi.conf

    # Enable the wlan0 instance of wpa_supplicant's template unit. This is
    # exactly what `systemctl enable wpa_supplicant@wlan0` would create: a
    # link named after the INSTANCE, pointing at the TEMPLATE file. Done by
    # hand because the unit belongs to another recipe (wpa-supplicant), so
    # this recipe's SYSTEMD_SERVICE can't enable it.
    install -d ${D}${sysconfdir}/systemd/system/multi-user.target.wants
    ln -sf ${systemd_system_unitdir}/wpa_supplicant@.service \
        ${D}${sysconfdir}/systemd/system/multi-user.target.wants/wpa_supplicant@wlan0.service
}

FILES:${PN} += " \
    ${sysconfdir}/systemd/network/25-wlan.network \
    ${libexecdir}/hub-wifi-init \
    ${systemd_system_unitdir}/wpa_supplicant@wlan0.service.d/10-hub-wifi.conf \
    ${sysconfdir}/systemd/system/multi-user.target.wants/wpa_supplicant@wlan0.service \
"
