SUMMARY = "Root SSH authorized_keys for key-only production access"
DESCRIPTION = "Installs the maintainer's public key so root is reachable over \
SSH with debug-tweaks (blank-password root) disabled. Paired with the \
openssh bbappend that forces PermitRootLogin prohibit-password on hardware."
LICENSE = "CLOSED"

inherit allarch

SRC_URI = "file://authorized_keys"

S = "${WORKDIR}"

do_install() {
    install -d -m 0700 ${D}${ROOT_HOME}/.ssh
    install -m 0600 ${WORKDIR}/authorized_keys ${D}${ROOT_HOME}/.ssh/authorized_keys
}

FILES:${PN} += "${ROOT_HOME}/.ssh/authorized_keys"

# Directory/file are user-owned config, not something a package manager
# should ever try to remove or diff against on upgrade.
CONFFILES:${PN} += "${ROOT_HOME}/.ssh/authorized_keys"
