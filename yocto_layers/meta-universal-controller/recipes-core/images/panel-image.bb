SUMMARY = "Wall panel for the Universal Controller hub (Raspberry Pi 3 B / B+)"
DESCRIPTION = "Issue #107: a Raspberry Pi 3 Model B (or B+) with the official 7-inch \
touchscreen as a second screen for the hub. Power on -> the hub's UI in \
panel mode (#106), nothing else: no desktop, no X/Wayland, no GPU stack, \
no package manager, no service listening on the network. Build: \
./run_build.sh rpi (see the Wall-Panel wiki page)."
LICENSE = "MIT"

inherit core-image

IMAGE_INSTALL = " \
    packagegroup-core-boot \
    ${CORE_IMAGE_EXTRA_INSTALL} \
    ui-layer \
    ui-layer-panel \
    panel-network \
    panel-tuning \
    panel-hardening \
    kernel-modules \
    linux-firmware-rpidistro-bcm43430 \
    linux-firmware-rpidistro-bcm43455 \
"

# Development image (as the DK2's): SSH with the maintainer's key only
# (ssh-root-key, PermitRootLogin prohibit-password), opened in the
# firewall by panel-hardening-ssh. A production panel drops these four.
IMAGE_INSTALL += "openssh openssh-sshd ssh-root-key panel-hardening-ssh"

# 64 MB free on the root filesystem for logs and the panel's state.
IMAGE_ROOTFS_EXTRA_SPACE = "65536"
