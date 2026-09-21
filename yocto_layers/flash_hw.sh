#!/usr/bin/env bash
# =============================================================================
# USB DFU flashing wrapper for STM32MP157F-DK2, driving STM32_Programmer_CLI
# instead of the GUI. Flashes a FlashLayout .tsv built by build-hw.
#
# Board must be in recovery/USB boot mode (SW1: BOOT2=0, BOOT0=0) and
# connected over USB before running this.
# =============================================================================
set -euo pipefail

VARIANT="${1:-optee}"

DEPLOY_DIR="build/tmp/deploy/images/stm32mp1"
TSV_REL="flashlayout_universal-controller-image/${VARIANT}/FlashLayout_sdcard_stm32mp157f-dk2-${VARIANT}.tsv"

# CLI resolves the binary paths referenced inside the .tsv (arm-trusted-firmware/,
# fip/, ...) relative to the current working directory, not the .tsv's own
# folder -- so we must cd into DEPLOY_DIR before invoking it.
cd "$(dirname "$0")/${DEPLOY_DIR}"

if [ ! -f "${TSV_REL}" ]; then
    echo "Error: flash layout not found: ${DEPLOY_DIR}/${TSV_REL}"
    echo "Available variants:"
    find flashlayout_universal-controller-image -maxdepth 1 -mindepth 1 -type d -printf '  - %f\n'
    exit 1
fi

CLI="$(command -v STM32_Programmer_CLI || true)"
if [ -z "${CLI}" ]; then
    CLI="${HOME}/STMicroelectronics/STM32Cube/STM32CubeProgrammer/bin/STM32_Programmer_CLI"
fi
if [ ! -x "${CLI}" ]; then
    echo "Error: STM32_Programmer_CLI not found. Install STM32CubeProgrammer or add it to PATH."
    exit 1
fi

echo "  -> Flashing ${TSV_REL}"
echo "  -> Make sure SW1 is set to recovery/USB mode (BOOT2=0, BOOT0=0) and the board is connected over USB."
read -rp "Press Enter to start flashing, or Ctrl+C to abort... "

"${CLI}" -c port=usb1 -w "${TSV_REL}"

# Every flash writes a fresh rootfs, which regenerates the board's SSH host
# key on first boot -- drop the stale cached entry so ssh doesn't refuse to
# connect over the mismatch.
ssh-keygen -f "${HOME}/.ssh/known_hosts" -R "stm32mp1.local" >/dev/null 2>&1 || true

echo "  -> Done. Flip SW1 back to SD/eMMC boot mode (BOOT2=1, BOOT0=1) and power-cycle the board."
