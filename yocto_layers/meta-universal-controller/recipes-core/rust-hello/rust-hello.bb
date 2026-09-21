SUMMARY = "Minimal Rust cross-toolchain smoke test"
DESCRIPTION = "Trivial hello-world binary that exists solely to prove the \
Rust cross-toolchain (cargo.bbclass, already shipped in oe-core -- no \
separate meta-rust layer needed) produces a working binary for both \
qemuarm64 (aarch64) and stm32mp1 (Cortex-A7, armv7ve). Not a real \
application -- see Sprint 2 Task 7, issue #9."
LICENSE = "CLOSED"

# Bump this every time files/main.rs (or Cargo.toml/.lock) changes without
# PV changing. rust-hello's identity is otherwise always "1.0-r0" no matter
# what the local source says -- do_compile/do_install/do_package_write_rpm
# DO pick up local file content changes correctly, but do_rootfs assembles
# the final image via real package-manager machinery (dnf/rpm here), which
# treats a given name-version-release as immutable once seen. Without a PR
# bump, a content-only edit can compile+package fine yet never make it into
# the built image, because rootfs assembly still resolves the same NEVRA.
PR = "r3"

inherit cargo

SRC_URI = "file://Cargo.toml file://Cargo.lock file://main.rs"

S = "${WORKDIR}"

# cargo.bbclass expects a standard cargo layout (Cargo.toml at ${S}, source
# under ${S}/src/) -- the local file:// fetcher drops everything flat into
# WORKDIR, so main.rs needs moving into src/ before cargo's do_configure
# validates the manifest. do_unpack itself is a *python* task (base.bbclass),
# so this can't hook :append there with shell code -- do_configure:prepend
# is the first shell-typed task in the chain, and cargo_common_do_configure
# doesn't run until after this prepend block.
do_configure:prepend() {
    if [ -e ${S}/main.rs ]; then
        install -d ${S}/src
        mv ${S}/main.rs ${S}/src/main.rs
    fi
}
