SUMMARY = "Universal Controller hub daemon (Tokio async runtime)"
DESCRIPTION = "Async Rust backend daemon for the Universal Controller hub -- \
owns every external connection (devices, mobile/LAN clients, cloud, the M4) \
and all application state, on a Tokio runtime. See linux_a7/backend_daemon \
and ARCHITECTURE.md for the design. Sprint 2 Task 8, issue #10."
HOMEPAGE = "https://github.com/CiprianTiro/stm32mp1-universal-controller"
LICENSE = "GPL-3.0-only"
LIC_FILES_CHKSUM = "file://${COMMON_LICENSE_DIR}/GPL-3.0-only;md5=c79ff39f19dfec6d293b95dea7b07891"

inherit cargo systemd

# Real source lives at linux_a7/backend_daemon/ (repo root), not duplicated
# into this layer (unlike rust-hello.bb) -- referencing it via
# FILESEXTRAPATHS means editing that source and rebuilding actually picks up
# the change, since bitbake tracks a checksum of the fetched file:// content.
# A plain S pointing straight at that directory with no fetcher at all would
# build from current disk state too, but bitbake would have no way to notice
# the source changed and re-trigger do_compile.
#
# The build always runs inside the yocto-builder Docker container (see
# run_build.sh), which only bind-mounts yocto_layers/ as its workspace --
# linux_a7/, being outside that directory, is invisible in there by default.
# docker-compose.yml adds a second, read-only mount specifically for it at
# /home/builder/linux_a7, which is why this is an absolute in-container path
# rather than a FILE_DIRNAME-relative climb (which was tried first and
# fails: paths outside /home/builder/workspace simply don't exist in the
# container's filesystem, no amount of "../" reaches them).
FILESEXTRAPATHS:prepend := "/home/builder/linux_a7/backend_daemon:"

# file://src/main.rs (not just main.rs) preserves that subpath under
# ${WORKDIR}, landing at ${WORKDIR}/src/main.rs already correctly laid out
# for cargo -- no do_configure:prepend shuffle needed here, unlike
# rust-hello.bb's flat file:// list.
SRC_URI = " \
    file://Cargo.toml \
    file://Cargo.lock \
    file://src/main.rs \
"

# The actual dependency tree (Tokio + transitive deps), pinned to exactly
# what's resolved in Cargo.lock. Regenerate this list by running
# `cargo bitbake` from linux_a7/backend_daemon/ whenever dependencies change,
# then merge the new crate:// list back in here.
#
# GOTCHA hit twice while setting this up: any `cargo` command that touches
# Cargo.lock (cargo add/update/check after editing Cargo.toml) regenerates it
# with "version = 4" at the top -- Cargo 1.97+'s new default. Both
# cargo-bitbake (generating this recipe) and Yocto's own bundled cargo here
# (scarthgap-era) fail outright on that ("lock file version 4 requires
# -Znext-lockfile-bump"). After any such command, manually edit that line
# back to "version = 3" in linux_a7/backend_daemon/Cargo.lock before
# rebuilding -- the rest of the file's content is otherwise unaffected.
SRC_URI += " \
    crate://crates.io/errno/0.3.14 \
    crate://crates.io/libc/0.2.189 \
    crate://crates.io/mio/1.2.3 \
    crate://crates.io/pin-project-lite/0.2.17 \
    crate://crates.io/proc-macro2/1.0.107 \
    crate://crates.io/quote/1.0.47 \
    crate://crates.io/signal-hook-registry/1.4.8 \
    crate://crates.io/syn/3.0.6 \
    crate://crates.io/tokio-macros/2.7.2 \
    crate://crates.io/tokio/1.53.1 \
    crate://crates.io/unicode-ident/1.0.26 \
    crate://crates.io/wasi/0.11.1+wasi-snapshot-preview1 \
    crate://crates.io/windows-link/0.2.1 \
    crate://crates.io/windows-sys/0.61.2 \
"

SRC_URI[errno-0.3.14.sha256sum] = "39cab71617ae0d63f51a36d69f866391735b51691dbda63cf6f96d042b63efeb"
SRC_URI[libc-0.2.189.sha256sum] = "3eaf3ede3fee6db1a4c2ee091bf8a8b4dccdc6d17f656fb07896ee72867612f2"
SRC_URI[mio-1.2.3.sha256sum] = "4b18443e9c262bfe8fa82f51666e2642c53393f7e5c27b3e1aeab922cff5b9d8"
SRC_URI[pin-project-lite-0.2.17.sha256sum] = "a89322df9ebe1c1578d689c92318e070967d1042b512afbe49518723f4e6d5cd"
SRC_URI[proc-macro2-1.0.107.sha256sum] = "985e7ec9bb745e6ce6535b544d84d6cd6f7ad8bd711c398938ae983b91a766d9"
SRC_URI[quote-1.0.47.sha256sum] = "1fbf4db142a473a8d80c26bbf18454ed458bf8d26c8219c331daecfdbd079001"
SRC_URI[signal-hook-registry-1.4.8.sha256sum] = "c4db69cba1110affc0e9f7bcd48bbf87b3f4fc7c61fc9155afd4c469eb3d6c1b"
SRC_URI[syn-3.0.6.sha256sum] = "8593e8e72159ed2257d083c7a454a85cbf854f37a0966d8d483aff8c8a3ebcee"
SRC_URI[tokio-macros-2.7.2.sha256sum] = "78773a2a397f451582ce068015985c33193cf6dea8b74d2a639fe457b2f07b0e"
SRC_URI[tokio-1.53.1.sha256sum] = "202caea871b69668250d242070849eb495be178ed697a3e98aebce5bc81a0bed"
SRC_URI[unicode-ident-1.0.26.sha256sum] = "d245f478577f809a851594d02313b640fb437e0bb33866753cff937863096954"
SRC_URI[wasi-0.11.1+wasi-snapshot-preview1.sha256sum] = "ccf3ec651a847eb01de73ccad15eb7d99f80485de043efb2f370cd654f4ea44b"
SRC_URI[windows-link-0.2.1.sha256sum] = "f0805222e57f7521d6a62e36fa9163bc891acd422f971defe97d64e70d0a4fe5"
SRC_URI[windows-sys-0.61.2.sha256sum] = "ae137229bcbd6cdf0f7b80a31df61766145077ddf49416a728b02cb3921ff3fc"

SRC_URI += "file://backend-daemon.service"

S = "${WORKDIR}"
CARGO_SRC_DIR = ""

SYSTEMD_SERVICE:${PN} = "backend-daemon.service"
SYSTEMD_AUTO_ENABLE:${PN} = "enable"

do_install:append() {
    install -d ${D}${systemd_system_unitdir}
    install -m 0644 ${WORKDIR}/backend-daemon.service ${D}${systemd_system_unitdir}/backend-daemon.service
}
