#!/usr/bin/env bash
# =============================================================================
# Context-Aware Automated QEMU Emulator Launcher (Host OR Container)
# Target Architecture: qemuarm64 (Simulation Environment)
# =============================================================================
set -euo pipefail

BUILD_DIR="build-qemu"
TARGET_MACHINE="qemuarm64"
# Must match TARGET_IMAGE in run_build.sh. Passed explicitly to runqemu below
# so it can't accidentally pick up a stale core-image-minimal artifact left
# over from an earlier build in tmp/deploy/images/${TARGET_MACHINE}/.
TARGET_IMAGE="universal-controller-image"

# Forward host ports to QEMU container layout: 10022 -> SSH(22), 9000 -> App Backend
export QB_SLIRP_OPT="-netdev user,id=net0,hostfwd=tcp::10022-:22,hostfwd=tcp::9000-:9000"

# Detect execution context boundary
INSIDE_CONTAINER=false
if [ -f /.dockerenv ] || [ "${USER:-}" = "builder" ]; then
    INSIDE_CONTAINER=true
fi

# =============================================================================
# EXECUTION ROUTER LOGIC
# =============================================================================
if [ "$INSIDE_CONTAINER" = true ]; then
    echo "🎮 Launching QEMU ARM64 Simulation Platform..."
    echo "🔌 Port Forwarding Active: Host:10022 -> QEMU:22 (SSH)"
    echo "🔌 Port Forwarding Active: Host:9000  -> QEMU:9000 (App)"
    echo "------------------------------------------------------------------"
    
    # Initialize the Yocto path markers specifically targeting the QEMU directory
    set +u
    source poky/oe-init-build-env "${BUILD_DIR}" > /dev/null
    set -u

    # Fire up the emulator engine headlessly inside this active terminal window
    runqemu "${TARGET_MACHINE}" "${TARGET_IMAGE}" slirp nographic

else
    echo "🐳 Host detected. Verifying Docker backend virtualization health..."
    docker compose up -d

    echo "⚡ Spawning interactive QEMU simulation engine context..."
    echo "------------------------------------------------------------------"

    # We MUST use 'exec -it' (interactive tty) so the QEMU console displays
    # directly on your host terminal screen and captures your keyboard inputs!
    docker compose exec -it yocto-builder bash -c "cd /home/builder/workspace && ./run_qemu.sh"
fi