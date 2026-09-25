# Adds kernel config fragments on top of ST's linux-stm32mp recipe,
# following the exact same SRC_URI + KERNEL_CONFIG_FRAGMENTS pattern that
# recipe already uses for its own fragments (see
# meta-st-stm32mp/recipes-kernel/linux/linux-stm32mp_6.6.bb) -- this isn't a
# different mechanism, just one more fragment merged in the same way.
#
# `${THISDIR}` is this .bbappend's own directory -- FILESEXTRAPATHS adds
# ${THISDIR}/files to where SRC_URI's file:// fetcher looks, which is where
# files/fbdev-emulation.cfg actually lives, right next to this .bbappend.
FILESEXTRAPATHS:prepend := "${THISDIR}/files:"

SRC_URI += "file://fbdev-emulation.cfg;subdir=fragments"
# The firewall's address families (issue #37, see the fragment).
SRC_URI += "file://nftables.cfg;subdir=fragments"

KERNEL_CONFIG_FRAGMENTS:append = " ${WORKDIR}/fragments/fbdev-emulation.cfg"
KERNEL_CONFIG_FRAGMENTS:append = " ${WORKDIR}/fragments/nftables.cfg"
