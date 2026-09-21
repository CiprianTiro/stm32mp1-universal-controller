SUMMARY = "Production Image for STM32MP1 Universal Smart Home Hub"
DESCRIPTION = "Custom Software-Defined Linux distribution for edge automation."
LICENSE = "MIT"

# Inherit the base core-image class infrastructure
inherit core-image

# NOTE on debug-tweaks (passwordless root, open SSH policy):
# Deliberately NOT baked in here, because this recipe is shared by BOTH the
# hardware build (yocto_layers/build/) and the QEMU build (yocto_layers/build-qemu/).
# Baking "debug-tweaks" into the shared recipe would ship passwordless root
# access on real hardware by default. Instead, run_build.sh injects
# EXTRA_IMAGE_FEATURES per build directory based on TARGET_MACHINE:
#   - yocto_layers/build-qemu/conf/local.conf -> debug-tweaks ON (dev/test convenience)
#   - yocto_layers/build/conf/local.conf      -> debug-tweaks OFF (production safety)
# On hardware, root has no password at all once debug-tweaks is off, so
# ssh-root-key (below, hardware-only) provides the actual way in: it installs
# an authorized_keys file, and the openssh bbappend forces
# "PermitRootLogin prohibit-password" so key auth is the only path.

# ssh access is legitimate on both targets (remote debug / deployment), the
# risk is specifically the passwordless-root part covered above, not sshd itself.
#
# Named directly in IMAGE_INSTALL below rather than only via
# IMAGE_FEATURES += "ssh-server-openssh": on real hardware that feature
# flag showed up as installed with no error, but the sshd.service unit was
# genuinely absent from the built rootfs (systemctl: "could not be found",
# not "inactive"/"disabled") - most likely a stale sstate artifact from
# this build directory's pre-existing build history predating this recipe.
# Naming the packages explicitly removes the indirection either way.
IMAGE_INSTALL += " \
    packagegroup-core-boot \
    bash \
    coreutils \
    openssh \
    openssh-sshd \
    openssh-sftp-server \
    rust-hello \
"

# Hardware-only: key-based root access (see NOTE above). Not installed on
# qemuarm64, where debug-tweaks' blank-password root already covers dev access.
IMAGE_INSTALL:append:stm32mp1common = " ssh-root-key"
