#!/usr/bin/env bash
# =============================================================================
# Hybrid Automated Yocto Build System Wrapper for Host OR Container Execution
# Targets: stm32mp1 (ST Common Profile) & qemuarm64 (Simulation)
# Custom Image: universal-controller-image
# =============================================================================
set -euo pipefail

MODE="${1:-hardware}"
# CHANGED: Re-routed from legacy core-image-minimal to our custom OS specification
TARGET_IMAGE="universal-controller-image"

if [ "$MODE" = "qemu" ]; then
    TARGET_MACHINE="qemuarm64"
    BUILD_DIR="build-qemu"
else
    TARGET_MACHINE="stm32mp1"
    BUILD_DIR="build"
fi

# Detect if the script is running inside the Docker container
INSIDE_CONTAINER=false
if [ -f /.dockerenv ] || [ "${USER:-}" = "builder" ]; then
    INSIDE_CONTAINER=true
fi

# =============================================================================
# PATH A: CORE COMPILATION LOGIC (RUNS INSIDE THE CONTAINER ENVIRONMENT)
# =============================================================================
compile_inside_container() {
    echo "  -> Initializing BitBake workspace tracking structure..."
    set +u
    source poky/oe-init-build-env "${BUILD_DIR}" > /dev/null
    set -u
    
    # Inject TARGET_MACHINE and performance threading defensively into local.conf
    if ! grep -q "MACHINE = \"${TARGET_MACHINE}\"" conf/local.conf; then
        echo "  -> Injecting target machine profile configuration..."
        echo "MACHINE = \"${TARGET_MACHINE}\"" >> conf/local.conf
        
        # Automatically accept ST's required End User License Agreement (EULA)
        if [ "${TARGET_MACHINE}" = "stm32mp1" ]; then
            echo 'ACCEPT_EULA_stm32mp1 = "1"' >> conf/local.conf
        fi
        
        CORES=$(nproc)
        echo "  -> Optimizing performance parameters for $CORES CPU cores..."
        echo "BB_NUMBER_THREADS = \"$CORES\"" >> conf/local.conf
        echo "PARALLEL_MAKE = \"-j $CORES\"" >> conf/local.conf
    fi

    # OPTIMIZED: Parsing bblayers.conf directly is faster and much safer than bitbake-layers execution
    echo "  -> Verifying metadata layer paths configuration..."
    for layer in meta-oe meta-python; do
        if ! grep -q "$layer" conf/bblayers.conf; then
            echo "     [+] Adding openembedded:$layer layer extension"
            bitbake-layers add-layer ../meta-openembedded/$layer
        fi
    done

    if [ "${TARGET_MACHINE}" != "qemuarm64" ]; then
        if ! grep -q 'meta-st-stm32mp' conf/bblayers.conf; then
            echo "     [+] Adding STMicroelectronics BSP hardware layer"
            bitbake-layers add-layer ../meta-st-stm32mp
        fi
    fi

    # CHANGED: Explicitly checks and registers your custom layer configuration
    if ! grep -q 'meta-universal-controller' conf/bblayers.conf; then
        echo "     [+] Adding custom universal-controller layer"
        bitbake-layers add-layer ../meta-universal-controller
    fi

    echo "-----------------------------------------------------------------------------"
    echo "🔥 Firing up BitBake compilation engine..."
    echo "📦 Image Recipe Target: ${TARGET_IMAGE}"
    echo "-----------------------------------------------------------------------------"
    bitbake "${TARGET_IMAGE}"
}

# =============================================================================
# MAIN EXECUTION ROUTER ROUTINE
# =============================================================================
echo "============================================================================="
echo "🚀 Starting Automated Yocto Build System"
echo "🎯 Target Machine : ${TARGET_MACHINE}"
echo "📁 Build Directory: ${BUILD_DIR}"
echo "📍 Execution Mode : $( $INSIDE_CONTAINER && echo 'CONTAINER SANDBOX' || echo 'HOST WORKSTATION' )"
echo "============================================================================="

if [ "$INSIDE_CONTAINER" = true ]; then
    compile_inside_container
else
    echo "📦 Step 1: Synchronizing Git submodules on host storage..."
    git submodule sync
    git submodule update --init --recursive

    echo "🐳 Step 2: Checking Docker container daemon sanity..."
    docker-compose up -d

    echo "⚡ Step 3: Forwarding execution sequence into container sandbox..."
    echo "-----------------------------------------------------------------------------"
    docker-compose exec yocto-builder bash -c "cd /home/builder/workspace && ./run_build.sh $MODE"
fi

echo "-----------------------------------------------------------------------------"
echo "✅ Build execution loop completed successfully!"
echo "============================================================================="