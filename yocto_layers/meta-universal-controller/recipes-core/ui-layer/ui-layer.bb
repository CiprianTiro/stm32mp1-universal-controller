SUMMARY = "Universal Controller hub UI (Slint on DRM/KMS: touchscreen or HDMI monitor)"
DESCRIPTION = "Slint GUI drawing through DRM/KMS on the DK2's touchscreen, \
or on an HDMI monitor when one is connected (#38) -- no X11/Wayland \
compositor. See linux_a7/ui_layer and ARCHITECTURE.md for the design. \
Sprint 3 Task 12 (#14)."
HOMEPAGE = "https://github.com/CiprianTiro/stm32mp1-universal-controller"
LICENSE = "GPL-3.0-only"
LIC_FILES_CHKSUM = "file://${COMMON_LICENSE_DIR}/GPL-3.0-only;md5=c79ff39f19dfec6d293b95dea7b07891"

# NOTE: deliberately NOT `inherit cargo` (unlike backend-daemon.bb).
#
# The cargo class compiles with Poky's own bundled Rust, which on scarthgap
# is rustc 1.75 -- too old for Slint 1.18 (its font stack needs edition
# 2024 / rustc >= 1.85). Instead, do_compile below runs the modern rustup
# toolchain installed in the yocto-builder container image (see
# yocto_layers/Dockerfile), and only borrows Yocto's own C cross-toolchain
# (${CC}) for the final link, so the binary still matches this image's
# exact glibc/ABI. Upgrading Poky's Rust or the whole Yocto release was
# considered and rejected: far more invasive, and risks meta-st-stm32mp
# BSP compatibility.
inherit systemd useradd pkgconfig

# Input through libinput (issue #38): Slint's KMS backend reads the touch
# panel, a USB mouse and a USB keyboard itself. At build time the Rust
# wrappers link against these C libraries, so they must be in this
# recipe's sysroot: libinput (input devices), libxkbcommon (keyboard
# layouts), udev (from systemd: finds the input devices, and notices one
# being plugged in). At run time Yocto adds the libraries to the image by
# itself (it sees the binary needs them) -- except xkeyboard-config: pure
# data files (the keyboard layouts, /usr/share/X11/xkb) that libxkbcommon
# reads, which nothing can detect, so it's listed by hand.
# pkgconfig (above): the libudev-sys crate asks pkg-config where libudev
# is; that class provides pkg-config and points it at the TARGET's sysroot.
DEPENDS += "libinput libxkbcommon udev"
RDEPENDS:${PN} += "xkeyboard-config"

# Hardware-only (the image only installs it on stm32mp1common, see
# universal-controller-image.bb), and do_compile below hardcodes the
# 32-bit ARM Rust target -- refusing other machines up front gives a clear
# "not compatible" message instead of a confusing link error.
COMPATIBLE_MACHINE = "(stm32mp1common)"

# Real source lives at linux_a7/ui_layer/ (repo root), not duplicated into
# this layer -- see backend-daemon.bb's own comment for why (same reasoning,
# same FILESEXTRAPATHS mechanism, same Docker bind-mount).
FILESEXTRAPATHS:prepend := "/home/builder/linux_a7/ui_layer:"

SRC_URI = " \
    file://Cargo.toml \
    file://Cargo.lock \
    file://build.rs \
    file://ui/app.slint \
    file://ui/devices.slint \
    file://ui/theme.slint \
    file://ui/common.slint \
    file://ui/keyboard.slint \
    file://ui/network.slint \
    file://ui/settings.slint \
    file://ui/appearance.slint \
    file://ui/tokens.json \
    file://ui/images/welcome.png \
    file://ui/images/pointer.png \
    file://fonts/DejaVuSans.ttf \
    file://src/main.rs \
    file://src/display.rs \
    file://src/pointer.rs \
    file://src/theme.rs \
    file://src/zones.rs \
    file://src/ws_client.rs \
    file://ui-layer.service \
"

S = "${WORKDIR}"

# The rustup toolchain baked into the container image (Dockerfile). Absolute
# paths because bitbake scrubs PATH/HOME down to its own minimal set inside
# tasks, so `cargo` and rustup's default ~ lookups wouldn't resolve.
RUSTUP_HOME_DIR = "/home/builder/.rustup"
RUSTUP_CARGO_HOME = "/home/builder/.cargo"
UI_RUST_TARGET = "armv7-unknown-linux-gnueabihf"

# Crate dependencies are NOT listed in SRC_URI (bitbake's crate:// fetcher
# is tied to the cargo class this recipe no longer uses). Cargo downloads
# them itself during do_compile, which needs network access -- bitbake
# blocks network in every task except do_fetch unless told otherwise.
# Tradeoff, accepted for now: this recipe can't build fully offline. The
# downloads are cached in ${RUSTUP_CARGO_HOME}/registry, so they only
# happen once per container, and --locked below keeps them pinned to
# exactly what Cargo.lock says.
do_compile[network] = "1"

do_compile() {
    # Cargo wants the linker to be a single executable path, but Yocto's
    # ${CC} is a command PLUS flags ("arm-poky-linux-gnueabi-gcc -mthumb
    # -mfpu=neon-vfpv4 -mfloat-abi=hard -mcpu=cortex-a7 --sysroot=...").
    # A tiny wrapper script bridges that. ${LDFLAGS} adds Yocto's standard
    # link flags (e.g. --hash-style=gnu, which Yocto's QA checks require).
    # The same wrapper idea serves as the C compiler for any dependency that
    # compiles C code through the `cc` crate.
    printf '#!/bin/sh\nexec %s %s "$@"\n' "${CC}" "${LDFLAGS}" > ${WORKDIR}/target-link.sh
    printf '#!/bin/sh\nexec %s "$@"\n' "${CC}" > ${WORKDIR}/target-cc.sh
    # Build scripts (build.rs) and proc-macros run on the BUILD machine
    # (x86_64, inside the container), so they need the host's own gcc, not
    # the ARM one. Rust's default host linker name is `cc`, which isn't in
    # bitbake's restricted PATH -- point it at ${BUILD_CC} explicitly.
    printf '#!/bin/sh\nexec %s "$@"\n' "${BUILD_CC}" > ${WORKDIR}/host-cc.sh
    chmod +x ${WORKDIR}/target-link.sh ${WORKDIR}/target-cc.sh ${WORKDIR}/host-cc.sh

    export RUSTUP_HOME="${RUSTUP_HOME_DIR}"
    # pkg-config (the Rust crate) refuses to answer for a different target
    # than the build machine unless told this is on purpose (#38).
    export PKG_CONFIG_ALLOW_CROSS=1
    # libudev-sys's build script runs a bare `rustc` (to test for an
    # optional libudev feature). bitbake's PATH has no rustc, and a missing
    # command makes that script crash -- so the rustup toolchain goes on
    # PATH. (The test itself may fail, compiling for the build machine;
    # the script then just leaves that optional feature off, which nothing
    # here needs.)
    export PATH="${RUSTUP_CARGO_HOME}/bin:${PATH}"
    export CARGO_HOME="${RUSTUP_CARGO_HOME}"

    # Linkers, per target (target triple upper-cased, dashes -> underscores).
    export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABIHF_LINKER="${WORKDIR}/target-link.sh"
    export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="${WORKDIR}/host-cc.sh"
    # Let rustc optimize for the actual CPU (Cortex-A7, with NEON) instead
    # of the generic armv7 baseline -- Slint's software renderer does all
    # its pixel work on the CPU, so this is worth having.
    # --remap-path-prefix: rustc embeds source file paths in the binary (for
    # panic messages / debug info), which would leak build-machine paths
    # like /home/builder/workspace/build/tmp/... -- Yocto's QA flags that
    # as a [buildpaths] issue. These rewrite them to neutral paths instead.
    export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABIHF_RUSTFLAGS="-C target-cpu=cortex-a7 --remap-path-prefix=${WORKDIR}=/usr/src/debug/${PN}/${PV} --remap-path-prefix=${RUSTUP_CARGO_HOME}=/cargo"

    # C compilers for the `cc` crate, per target. Target-specific variable
    # names win over the plain CC/CFLAGS bitbake exports, so host build
    # scripts don't accidentally get ARM flags or vice versa.
    export CC_armv7_unknown_linux_gnueabihf="${WORKDIR}/target-cc.sh"
    export CFLAGS_armv7_unknown_linux_gnueabihf="${CFLAGS}"
    export CC_x86_64_unknown_linux_gnu="${WORKDIR}/host-cc.sh"
    export CFLAGS_x86_64_unknown_linux_gnu="${BUILD_CFLAGS}"

    # bindgen (linuxfb uses it for <linux/fb.h>) runs clang to parse C
    # headers. By default clang reads the BUILD machine's /usr/include --
    # x86_64 headers, giving wrong struct layouts, or failing outright on
    # <asm/types.h>. Pointing it at the ARM target triple and this recipe's
    # target sysroot makes it read the board's real kernel/libc headers.
    export BINDGEN_EXTRA_CLANG_ARGS_armv7_unknown_linux_gnueabihf="--target=${UI_RUST_TARGET} --sysroot=${STAGING_DIR_TARGET}"

    ${RUSTUP_CARGO_HOME}/bin/cargo build \
        --release \
        --locked \
        --target ${UI_RUST_TARGET} \
        --manifest-path ${S}/Cargo.toml \
        --target-dir ${B}/target
}

do_install() {
    install -D -m 0755 ${B}/target/${UI_RUST_TARGET}/release/ui-layer ${D}${bindir}/ui-layer
    install -d ${D}${systemd_system_unitdir}
    install -m 0644 ${WORKDIR}/ui-layer.service ${D}${systemd_system_unitdir}/ui-layer.service
}

# The UI's own user (issue #37): ui-layer.service runs it as "hubui"
# instead of root. A system account with its own group, no home folder and
# no login shell. The groups for the screen and touch panel (video, input)
# are added by the unit's SupplementaryGroups=.
USERADD_PACKAGES = "${PN}"
USERADD_PARAM:${PN} = "--system --user-group --no-create-home --home-dir /nonexistent --shell /sbin/nologin hubui"

SYSTEMD_SERVICE:${PN} = "ui-layer.service"
SYSTEMD_AUTO_ENABLE:${PN} = "enable"
