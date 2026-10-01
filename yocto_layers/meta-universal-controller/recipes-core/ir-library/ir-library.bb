SUMMARY = "IR code library for the hub's IR blasters (issue #82)"
DESCRIPTION = "Known IR codes by device type and brand, for the code finder \
(devices without their remote). Made at build time from the Flipper Zero \
community's IR database (Flipper-IRDB) by \
linux_a7/backend_daemon/ir_library/irdb_import.py; read by backend-daemon's \
src/ir_library.rs from /usr/share/universal-controller/ir-library."
HOMEPAGE = "https://github.com/Lucaslhm/Flipper-IRDB"
# The data is CC0 1.0 (public domain); the converter is the hub's own code
# (GPL-3.0-only, like backend-daemon). The licence text is checked so a
# licence change upstream fails the build instead of slipping in.
LICENSE = "CC0-1.0 & GPL-3.0-only"
LIC_FILES_CHKSUM = " \
    file://${S}/LICENSE;md5=473a7959b44c2f42c375d904305b6307 \
    file://${COMMON_LICENSE_DIR}/GPL-3.0-only;md5=c79ff39f19dfec6d293b95dea7b07891 \
"

# Plain data, the same on every machine; python3-native runs the converter.
inherit allarch python3native

# The database at a fixed commit (SRCREV): bitbake checks out exactly
# that, so the content is the reviewed one. (Not GitHub's tarball of the
# commit: those aren't guaranteed to stay byte-identical, Yocto's QA
# warns.) To update: put a newer commit below, then check the converter's
# report (bitbake -c compile ir-library, temp/log.do_compile) for codes
# that suddenly got skipped.
SRC_URI = " \
    git://github.com/Lucaslhm/Flipper-IRDB.git;protocol=https;branch=main \
    file://ir_library/irdb_import.py \
"
SRCREV = "d126fb1b6f1e114c52b4a8c19839ea65e3a9c24d"

# The converter lives with the backend's source (same folder as
# backend-daemon.bb uses; mounted into the build container, see there).
FILESEXTRAPATHS:prepend := "/home/builder/linux_a7/backend_daemon:"

S = "${WORKDIR}/git"

do_configure[noexec] = "1"

do_compile() {
    rm -rf ${B}/ir-library
    ${PYTHON} ${WORKDIR}/ir_library/irdb_import.py ${S} ${B}/ir-library --commit ${SRCREV}
}

do_install() {
    install -d ${D}${datadir}/universal-controller/ir-library
    install -m 0644 ${B}/ir-library/*.json ${D}${datadir}/universal-controller/ir-library/
}

FILES:${PN} = "${datadir}/universal-controller/ir-library"
