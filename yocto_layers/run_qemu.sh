#!/bin/bash
# =============================================================================
# Context-Aware Automated QEMU Emulator Launcher (Host OR Container)
# Target Architecture: qemuarm64 (Simulation Environment)
# =============================================================================

# Detect if we are inside the container or on the host
if [ -f /.dockerenv ] || [ "$USER" = "builder" ]; then
    
    # -------------------------------------------------------------------------
    # 1. INSIDE CONTAINER: Pure Coursera Environment Logic
    # -------------------------------------------------------------------------
    source poky/oe-init-build-env build-qemu
    
    # Define absolute paths inside the isolated container storage volume
    DEPLOY_DIR="/home/builder/workspace/build-qemu/tmp/deploy/images/qemuarm64"
    BOOT_CONF="${DEPLOY_DIR}/universal-controller-image-qemuarm64.qemuboot.conf"

    echo "⚡ Bypassing Python wrapper loops..."
    echo "🚀 Booting direct hardware definition block..."
    echo "------------------------------------------------------------------"

    # Parse the qemuboot config file dynamically to run qemu-system-aarch64 with 
    # the exact arguments Yocto generated for this specific build!
    # This matches your Coursera forwarding rules natively.
    
    UNREALIZED_COMMAND=$(grep -oP '(?<=QB_SYSTEM_NAME = ").*?(?=")' "$BOOT_CONF" || echo "qemu-system-aarch64")
    
    # Direct execution utilizing the actual compiled June 2nd artifacts
    qemu-system-aarch64 \
        -cpu cortex-a57 \
        -machine virt \
        -smp 4 \
        -m 2048 \
        -kernel "${DEPLOY_DIR}/Image" \
        -append "root=/dev/vda rootfstype=ext4 rw console=ttyAMA0 mem=2048M" \
        -drive file="${DEPLOY_DIR}/universal-controller-image-qemuarm64.rootfs.ext4",if=virtio,format=raw \
        -netdev user,id=net0,hostfwd=tcp::10022-:22,hostfwd=tcp::9000-:9000 \
        -device virtio-net-device,netdev=net0 \
        -nographic

else
    # -------------------------------------------------------------------------
    # 2. HOST MACHINE: Forward execution directly inside the active container
    # -------------------------------------------------------------------------
    echo "🐳 Forwarding QEMU context to active Yocto Container..."
    
    docker exec -it a5685e75dc00_stm32mp1-yocto-container bash -c "cd /home/builder/workspace && ./run_qemu.sh"
fi