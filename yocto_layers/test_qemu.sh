#!/usr/bin/env bash
# =============================================================================
# Context-Aware Automated QEMU Smoke Test Launcher (Host OR Container)
# Boots universal-controller-image under qemuarm64, drives the serial console
# via pexpect (qemu_smoke_test.py), and reports pass/fail. Run this BEFORE
# every hardware flash cycle - it's minutes instead of a flash/boot/serial
# round trip on the STM32MP157F-DK2.
# =============================================================================
set -euo pipefail

# Detect execution context boundary (same convention as run_build.sh / run_qemu.sh)
INSIDE_CONTAINER=false
if [ -f /.dockerenv ] || [ "${USER:-}" = "builder" ]; then
    INSIDE_CONTAINER=true
fi

if [ "$INSIDE_CONTAINER" = true ]; then
    echo "============================================================================="
    echo "🧪 Running automated QEMU boot smoke test"
    echo "============================================================================="
    python3 qemu_smoke_test.py "$@"
else
    echo "🐳 Host detected. Verifying Docker backend virtualization health..."
    docker compose up -d

    echo "⚡ Forwarding smoke test execution into container sandbox..."
    echo "-----------------------------------------------------------------------------"
    # Not interactive (-it) on purpose: this is meant to be scriptable/CI-friendly,
    # unlike run_qemu.sh which is an interactive console session.
    docker compose exec -T yocto-builder bash -c "cd /home/builder/workspace && ./test_qemu.sh $*"
fi
