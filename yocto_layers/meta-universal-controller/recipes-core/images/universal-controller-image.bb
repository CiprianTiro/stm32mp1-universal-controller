SUMMARY = "Production Image for STM32MP1 Universal Smart Home Hub"
DESCRIPTION = "Custom Software-Defined Linux distribution for edge automation."
LICENSE = "MIT"

# Inherit the base core-image class infrastructure
inherit core-image

# NOTE on debug-tweaks (passwordless root, open SSH policy):
# Deliberately NOT baked in here, because this recipe is shared by BOTH the
# hardware build (yocto_layers/build/) and the QEMU build (yocto_layers/build-qemu/).
# Baking "debug-tweaks" into the shared recipe would ship passwordless root
# access on real hardware by default. Instead, EXTRA_IMAGE_FEATURES is set
# per build directory:
#   - yocto_layers/build-qemu/conf/local.conf -> debug-tweaks ON (dev/test convenience)
#   - yocto_layers/build/conf/local.conf      -> debug-tweaks OFF (production safety)
# See both local.conf files for the actual toggle.

# ssh access is legitimate on both targets (remote debug / deployment), the
# risk is specifically the passwordless-root part covered above, not sshd itself.
IMAGE_FEATURES += "ssh-server-openssh"

# System packages to explicitly bake into the rootfs filesystem
IMAGE_INSTALL += " \
    packagegroup-core-boot \
    bash \
    coreutils \
"
