SUMMARY = "Low-heat settings for the Universal Controller wall panel"
DESCRIPTION = "Issue #107: the Raspberry Pi wall panel runs hot with its \
display. Besides config.txt (rpi-panel.inc: 1 GHz at most, Bluetooth off), \
the CPU clock follows the load and rests at its slowest, and the UI may \
dim and turn off the backlight when nobody uses the panel."
HOMEPAGE = "https://github.com/CiprianTiro/stm32mp1-universal-controller"
LICENSE = "GPL-3.0-only"
LIC_FILES_CHKSUM = "file://${COMMON_LICENSE_DIR}/GPL-3.0-only;md5=c79ff39f19dfec6d293b95dea7b07891"

inherit systemd allarch

SRC_URI = "file://panel-cpufreq.service file://90-panel-backlight.rules"
S = "${WORKDIR}"

SYSTEMD_SERVICE:${PN} = "panel-cpufreq.service"
SYSTEMD_AUTO_ENABLE:${PN} = "enable"

do_install() {
    install -D -m 0644 ${WORKDIR}/panel-cpufreq.service ${D}${systemd_system_unitdir}/panel-cpufreq.service
    install -D -m 0644 ${WORKDIR}/90-panel-backlight.rules ${D}${nonarch_base_libdir}/udev/rules.d/90-panel-backlight.rules
}

FILES:${PN} += "${nonarch_base_libdir}/udev/rules.d/90-panel-backlight.rules"
