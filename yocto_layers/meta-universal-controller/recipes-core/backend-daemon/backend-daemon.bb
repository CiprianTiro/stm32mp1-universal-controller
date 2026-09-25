SUMMARY = "Universal Controller hub daemon (Tokio async runtime)"
DESCRIPTION = "Async Rust backend daemon for the Universal Controller hub -- \
owns every external connection (devices, mobile/LAN clients, cloud, the M4) \
and all application state, on a Tokio runtime. See linux_a7/backend_daemon \
and ARCHITECTURE.md for the design. Sprint 2 Task 8 (#10) / Task 11 (#13)."
HOMEPAGE = "https://github.com/CiprianTiro/stm32mp1-universal-controller"
LICENSE = "GPL-3.0-only"
LIC_FILES_CHKSUM = "file://${COMMON_LICENSE_DIR}/GPL-3.0-only;md5=c79ff39f19dfec6d293b95dea7b07891"

# NOTE: deliberately NOT `inherit cargo` any more (issue #37) -- built the
# same way as ui-layer.bb: with the rustup toolchain in the yocto-builder
# container (see yocto_layers/Dockerfile), borrowing only Yocto's C
# cross-toolchain for C code and the final link.
#
# Why: the cargo class compiles with Poky's own Rust, 1.75 on scarthgap.
# Security fixes increasingly need newer compilers -- the one that forced
# this: `time` fixed a denial-of-service bug (RUSTSEC-2026-0009) only in
# versions that need Rust 1.88. Stuck on 1.75, the hub could not take such
# fixes at all, which `make audit` (zero known vulnerabilities) can't
# accept. Side benefits: no more crate:// list with 170 checksums to keep
# in sync with Cargo.lock by hand, no Cargo.lock "version = 3" edits, no
# pinning crates back for the old compiler.
# Tradeoff (same as ui-layer): do_compile downloads the crates itself (see
# do_compile[network]) instead of bitbake's fetcher. Integrity is the same:
# --locked builds exactly what Cargo.lock lists, and cargo checks every
# downloaded crate against the SHA-256 checksum recorded there.
inherit systemd useradd

# Real source lives at linux_a7/backend_daemon/ (repo root), not duplicated
# into this layer (unlike rust-hello.bb) -- referencing it via
# FILESEXTRAPATHS means editing that source and rebuilding actually picks up
# the change, since bitbake tracks a checksum of the fetched file:// content.
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
# for cargo. patched/rumqttc is a whole folder (a patched copy of one
# dependency, see its PATCHED.md), fetched as is.
SRC_URI = " \
    file://Cargo.toml \
    file://Cargo.lock \
    file://src/auth.rs \
    file://src/ble.rs \
    file://src/control.rs \
    file://src/device.rs \
    file://src/health.rs \
    file://src/helper.rs \
    file://src/hotspot.rs \
    file://src/main.rs \
    file://src/mqtt.rs \
    file://src/network.rs \
    file://src/rpmsg.rs \
    file://src/settings.rs \
    file://src/shadow.rs \
    file://src/state.rs \
    file://src/store.rs \
    file://src/tls.rs \
    file://src/ws.rs \
    file://patched/rumqttc \
"

# The rustup toolchain baked into the container image (Dockerfile). Absolute
# paths because bitbake scrubs PATH/HOME down to its own minimal set inside
# tasks, so `cargo` and rustup's default ~ lookups wouldn't resolve.
RUSTUP_HOME_DIR = "/home/builder/.rustup"
RUSTUP_CARGO_HOME = "/home/builder/.cargo"

# The Rust target for the machine being built: 32-bit ARM with hardware
# floating point on the DK2 (Cortex-A7), 64-bit ARM for QEMU (qemuarm64).
# ("aarch64" is an override bitbake sets itself when TARGET_ARCH is aarch64.)
BACKEND_RUST_TARGET = "armv7-unknown-linux-gnueabihf"
BACKEND_RUST_TARGET:aarch64 = "aarch64-unknown-linux-gnu"
# Optimize for the actual CPU (Cortex-A7, with NEON) instead of the generic
# armv7 baseline. QEMU's generic ARM64: no extra flag.
BACKEND_RUST_CPU_FLAGS = "-C target-cpu=cortex-a7"
BACKEND_RUST_CPU_FLAGS:aarch64 = ""

# Crates are downloaded by cargo during do_compile (see the NOTE at the top)
# -- bitbake blocks network in every task except do_fetch unless told
# otherwise. Cached in ${RUSTUP_CARGO_HOME}/registry, so only once.
do_compile[network] = "1"

do_compile() {
    # Make sure the toolchain can build for this machine. Instant if the
    # target is installed already (the Dockerfile adds both); a container
    # made from an older image gets the missing one here, once.
    ${RUSTUP_CARGO_HOME}/bin/rustup target add ${BACKEND_RUST_TARGET}

    # Cargo wants the linker to be a single executable path, but Yocto's
    # ${CC} is a command PLUS flags ("arm-poky-linux-gnueabi-gcc -mthumb
    # -mfpu=neon-vfpv4 -mfloat-abi=hard -mcpu=cortex-a7 --sysroot=...").
    # A tiny wrapper script bridges that. ${LDFLAGS} adds Yocto's standard
    # link flags (e.g. --hash-style=gnu, which Yocto's QA checks require).
    # The same wrapper serves as the C compiler for the dependencies that
    # compile C code through the `cc` crate: ring (crypto) and libdbus-sys
    # (D-Bus for Bluetooth, built from source -- the "vendored" feature).
    printf '#!/bin/sh\nexec %s %s "$@"\n' "${CC}" "${LDFLAGS}" > ${WORKDIR}/target-link.sh
    printf '#!/bin/sh\nexec %s "$@"\n' "${CC}" > ${WORKDIR}/target-cc.sh
    # Build scripts (build.rs) and proc-macros run on the BUILD machine
    # (x86_64, inside the container), so they need the host's own gcc, not
    # the ARM one. Rust's default host linker name is `cc`, which isn't in
    # bitbake's restricted PATH -- point it at ${BUILD_CC} explicitly.
    printf '#!/bin/sh\nexec %s "$@"\n' "${BUILD_CC}" > ${WORKDIR}/host-cc.sh
    chmod +x ${WORKDIR}/target-link.sh ${WORKDIR}/target-cc.sh ${WORKDIR}/host-cc.sh

    export RUSTUP_HOME="${RUSTUP_HOME_DIR}"
    export CARGO_HOME="${RUSTUP_CARGO_HOME}"

    # Cargo's and the cc crate's per-target variables are named after the
    # target: upper-case with underscores for cargo
    # (CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABIHF_LINKER), underscores for
    # cc (CC_armv7_unknown_linux_gnueabihf). Target-specific names win over
    # the plain CC/CFLAGS bitbake exports, so host build scripts don't get
    # ARM flags or vice versa.
    target_upper=$(echo ${BACKEND_RUST_TARGET} | tr 'a-z-' 'A-Z_')
    target_lower=$(echo ${BACKEND_RUST_TARGET} | tr '-' '_')
    export "CARGO_TARGET_${target_upper}_LINKER=${WORKDIR}/target-link.sh"
    export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="${WORKDIR}/host-cc.sh"
    # --remap-path-prefix: rustc embeds source file paths in the binary (for
    # panic messages / debug info), which would leak build-machine paths
    # like /home/builder/workspace/build/tmp/... -- Yocto's QA flags that
    # as a [buildpaths] issue. These rewrite them to neutral paths instead.
    export "CARGO_TARGET_${target_upper}_RUSTFLAGS=${BACKEND_RUST_CPU_FLAGS} --remap-path-prefix=${WORKDIR}=/usr/src/debug/${PN}/${PV} --remap-path-prefix=${RUSTUP_CARGO_HOME}=/cargo"
    export "CC_${target_lower}=${WORKDIR}/target-cc.sh"
    export "CFLAGS_${target_lower}=${CFLAGS}"
    export CC_x86_64_unknown_linux_gnu="${WORKDIR}/host-cc.sh"
    export CFLAGS_x86_64_unknown_linux_gnu="${BUILD_CFLAGS}"

    ${RUSTUP_CARGO_HOME}/bin/cargo build \
        --release \
        --locked \
        --target ${BACKEND_RUST_TARGET} \
        --manifest-path ${S}/Cargo.toml \
        --target-dir ${B}/target
}

SRC_URI += " \
    file://backend-daemon.service \
    file://backend-daemon-tmpfiles.conf \
    file://hub-helper.sh \
    file://hub-helper.socket \
    file://hub-helper@.service \
"

S = "${WORKDIR}"

# The daemon's own user (issue #37): backend-daemon.service runs it as
# "hubd" instead of root. A system account (uid below 1000), with its own
# group of the same name, no home folder and no login shell -- nobody can
# log in as it; it only exists to own the daemon's files and process.
# Further groups (rpmsg for the M4 channel) are added by the unit's
# SupplementaryGroups=, so this recipe doesn't depend on who creates them.
USERADD_PACKAGES = "${PN}"
USERADD_PARAM:${PN} = "--system --user-group --no-create-home --home-dir /nonexistent --shell /sbin/nologin hubd"

# hub-helper.socket is enabled (it's how the daemon gets the few things it
# needs root for, see hub-helper.sh); hub-helper@.service is started by it,
# per request, and is never enabled itself.
SYSTEMD_SERVICE:${PN} = "backend-daemon.service hub-helper.socket"
SYSTEMD_AUTO_ENABLE:${PN} = "enable"

do_install() {
    install -D -m 0755 ${B}/target/${BACKEND_RUST_TARGET}/release/backend-daemon ${D}${bindir}/backend-daemon
    install -d ${D}${systemd_system_unitdir}
    install -m 0644 ${WORKDIR}/backend-daemon.service ${D}${systemd_system_unitdir}/backend-daemon.service
    install -m 0644 ${WORKDIR}/hub-helper.socket ${D}${systemd_system_unitdir}/hub-helper.socket
    install -m 0644 ${WORKDIR}/hub-helper@.service ${D}${systemd_system_unitdir}/hub-helper@.service
    install -D -m 0755 ${WORKDIR}/hub-helper.sh ${D}${libexecdir}/hub-helper
    install -D -m 0644 ${WORKDIR}/backend-daemon-tmpfiles.conf ${D}${nonarch_libdir}/tmpfiles.d/backend-daemon.conf
}

FILES:${PN} += " \
    ${systemd_system_unitdir}/hub-helper@.service \
    ${libexecdir}/hub-helper \
    ${nonarch_libdir}/tmpfiles.d/backend-daemon.conf \
"
