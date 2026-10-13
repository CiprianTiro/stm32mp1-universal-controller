# Production hardening: on real hardware (not qemuarm64), debug-tweaks is
# off and root has no password at all, so password auth would just be a
# dead end anyway. Say so explicitly rather than relying on that side
# effect, so this survives someone re-enabling debug-tweaks later.
do_install:append:stm32mp1common() {
    echo "PermitRootLogin prohibit-password" >> ${D}${sysconfdir}/ssh/sshd_config
}

# The Raspberry Pi wall panel (issue #107): the same rule.
do_install:append:rpi() {
    echo "PermitRootLogin prohibit-password" >> ${D}${sysconfdir}/ssh/sshd_config
}
