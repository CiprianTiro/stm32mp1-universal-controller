SUMMARY = "Production Image for STM32MP1 Universal Smart Home Hub"
DESCRIPTION = "Custom Software-Defined Linux distribution for edge automation."
LICENSE = "MIT"

# Inherit the base core-image class infrastructure
inherit core-image

# Base package features required for early development
IMAGE_FEATURES += "debug-tweaks ssh-server-openssh"

# System packages to explicitly bake into the rootfs filesystem
IMAGE_INSTALL += " \
    packagegroup-core-boot \
    bash \
    coreutils \
"