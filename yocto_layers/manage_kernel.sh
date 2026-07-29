#!/usr/bin/env bash
# =============================================================================
# Yocto Kernel Configuration Automation Engine
# Handles interactive menuconfig and automated fragment (.cfg) extraction
# =============================================================================
set -euo pipefail

COMMAND="${1:-menuconfig}"
BUILD_DIR="build"
KERNEL_RECIPE="virtual/kernel"
LAYER_DIR="meta-universal-controller"

# Ensure this script is executing inside the container sandbox context
INSIDE_CONTAINER=false
if [ -f /.dockerenv ] || [ "${USER:-}" = "builder" ]; then
    INSIDE_CONTAINER=true
fi

if [ "$INSIDE_CONTAINER" = false ]; then
    echo "🐳 Forwarding kernel task [$COMMAND] into Docker sandbox..."
    docker compose exec yocto-builder bash -c "cd /home/builder/workspace && ./manage_kernel.sh $COMMAND"
    exit 0
fi

# =============================================================================
# CONTAINER BOUNDARY EXECUTION
# =============================================================================
set +u
source poky/oe-init-build-env "${BUILD_DIR}" > /dev/null
set -u

if [ "$COMMAND" = "menuconfig" ]; then
    echo "🎨 Launching interactive Kernel Menuconfig..."
    bitbake -c menuconfig ${KERNEL_RECIPE}

elif [ "$COMMAND" = "save" ]; then
    echo "💾 Running diffconfig to extract your configuration..."
    
    # Run diffconfig and capture output path
    DIFF_OUT=$(bitbake -c diffconfig ${KERNEL_RECIPE} 2>&1)
    
    # Extract the physical path of the fragment file from BitBake's output logs
    FRAGMENT_PATH=$(echo "$DIFF_OUT" | grep -oE '/[^ ]+/fragment\.cfg' | head -n 1 || true)
    
    if [ -z "$FRAGMENT_PATH" ] || [ ! -f "$FRAGMENT_PATH" ]; then
        echo "❌ Error: Could not find a generated fragment.cfg file."
        echo "Did you actually modify any configuration settings inside menuconfig?"
        exit 1
    fi

    # Define target path inside your custom layer repository
    TARGET_RECIPE_DIR="../${LAYER_DIR}/recipes-kernel/linux"
    mkdir -p "${TARGET_RECIPE_DIR}/files"

    # Move fragment to custom layer path
    TIMESTAMP=$(date +%Y%m%d_%H%M%S)
    FINAL_FRAGMENT_NAME="kernel_mod_${TIMESTAMP}.cfg"
    cp "$FRAGMENT_PATH" "${TARGET_RECIPE_DIR}/files/${FINAL_FRAGMENT_NAME}"
    
    echo "============================================================================="
    echo "🎯 SUCCESS: Configuration fragment saved!"
    echo "📄 File generated: ${LAYER_DIR}/recipes-kernel/linux/files/${FINAL_FRAGMENT_NAME}"
    echo "============================================================================="
    echo "Next steps: Link this .cfg file inside your linux-stm32mp_%.bbappend recipe."
    echo "============================================================================="
fi