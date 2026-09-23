SUMMARY = "Universal Controller Cortex-M4 firmware (Zephyr) + boot-time loader"
DESCRIPTION = "Installs the Zephyr M4 firmware ELF as /lib/firmware/rproc-m4-fw \
and a systemd service that starts it through remoteproc at boot, so the \
RPMsg link backend-daemon uses is up without any manual step."
HOMEPAGE = "https://github.com/CiprianTiro/stm32mp1-universal-controller"
LICENSE = "GPL-3.0-only"
LIC_FILES_CHKSUM = "file://${COMMON_LICENSE_DIR}/GPL-3.0-only;md5=c79ff39f19dfec6d293b95dea7b07891"

inherit systemd

# DK2 only: the firmware is built for this board's M4 and remoteproc0 only
# exists with the M4-enabled device tree.
COMPATIBLE_MACHINE = "(stm32mp1common)"

# The ELF is NOT built by bitbake: Zephyr needs its own SDK/toolchain
# (west + zephyr-sdk), which lives on the host, not in the yocto-builder
# container. `make build-m4` builds it on the host into
# firmware_m4/build/zephyr/zephyr.elf, and docker-compose.yml mounts
# firmware_m4/ read-only at /home/builder/firmware_m4 so this recipe can
# pick it up -- same mechanism as linux_a7/ for backend-daemon.bb.
# So: run `make build-m4` before `make build-hw`. If the ELF is missing,
# bitbake's do_fetch fails with "Unable to find file file://zephyr.elf".
# bitbake checksums the file, so a rebuilt ELF is picked up automatically.
FILESEXTRAPATHS:prepend := "/home/builder/firmware_m4/build/zephyr:"

SRC_URI = " \
    file://zephyr.elf \
    file://m4-firmware.sh \
    file://m4-firmware.service \
"

S = "${WORKDIR}"

do_install() {
    # Name must match what m4-firmware.sh writes to remoteproc0/firmware.
    install -D -m 0644 ${WORKDIR}/zephyr.elf ${D}${nonarch_base_libdir}/firmware/rproc-m4-fw
    install -D -m 0755 ${WORKDIR}/m4-firmware.sh ${D}${bindir}/m4-firmware.sh
    install -D -m 0644 ${WORKDIR}/m4-firmware.service ${D}${systemd_system_unitdir}/m4-firmware.service
}

FILES:${PN} += "${nonarch_base_libdir}/firmware/rproc-m4-fw"

SYSTEMD_SERVICE:${PN} = "m4-firmware.service"
SYSTEMD_AUTO_ENABLE:${PN} = "enable"

# The ELF is Cortex-M4 code, not Linux/Cortex-A7 code, so Yocto's usual
# post-processing is wrong for it:
#  - don't strip it or split out debug info (Yocto's A7 strip tool would
#    mangle it, and remoteproc loads it as-is);
#  - skip the "arch" QA check, which would flag it as built for the wrong
#    machine -- it is, on purpose.
INHIBIT_PACKAGE_STRIP = "1"
INHIBIT_PACKAGE_DEBUG_SPLIT = "1"
INHIBIT_SYSROOT_STRIP = "1"
INSANE_SKIP:${PN} += "arch"
