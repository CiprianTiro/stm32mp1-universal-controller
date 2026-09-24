#!/usr/bin/env bash
# =============================================================================
# USB DFU flashing wrapper for STM32MP157F-DK2, driving STM32_Programmer_CLI
# instead of the GUI. Flashes a FlashLayout .tsv built by build-hw.
#
# Board must be in recovery/USB boot mode (SW1: BOOT2=0, BOOT0=0) and
# connected over USB before running this.
#
# userfs (/usr/local) is KEPT by default (issue #56): it holds the board's
# identity (AWS certificate + key, backend-daemon.env) and the device
# registry (#33), none of which is part of the image. To wipe it on
# purpose, e.g. to test a brand-new board:
#   FULL_FLASH=1 make flash-hw
# =============================================================================
set -euo pipefail

VARIANT="${1:-optee}"
FULL_FLASH="${FULL_FLASH:-0}"
BOARD_HOST="${BOARD_HOST:-stm32mp1.local}"
# Where userfs backups go (git-ignored, see .gitignore): absolute, because
# the script cd's into the deploy directory below.
BACKUP_ROOT="$(cd "$(dirname "$0")" && pwd)/userfs-backups"

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

# --- Safety net: back up userfs while the board is still running -----------
# Only possible if it's up and reachable now (before it's switched to USB
# mode). Short timeout + BatchMode: if it isn't, we just say so and go on,
# never hang or ask for a password. `tar` runs ON the board and streams the
# archive over ssh into a file here.
BACKUP_DIR="${BACKUP_ROOT}/$(date +%Y%m%d-%H%M%S)"
echo "  -> Backing up ${BOARD_HOST}:/usr/local/{etc,var} (if the board is reachable)..."
# umask 077: everything created from here on (folders, archive) is
# readable by you only -- the backup contains the device's private key.
umask 077
mkdir -p "${BACKUP_ROOT}"
chmod 700 "${BACKUP_ROOT}"   # in case the folder already existed
if ssh -o ConnectTimeout=5 -o BatchMode=yes -o StrictHostKeyChecking=accept-new "root@${BOARD_HOST}" \
       'cd /usr/local && tar czf - $(ls -d etc var 2>/dev/null)' > "${BACKUP_ROOT}/.partial.tar.gz" 2>/dev/null \
   && [ -s "${BACKUP_ROOT}/.partial.tar.gz" ]; then
    mkdir -p "${BACKUP_DIR}"
    mv "${BACKUP_ROOT}/.partial.tar.gz" "${BACKUP_DIR}/userfs.tar.gz"
    echo "     saved ${BACKUP_DIR}/userfs.tar.gz  (restore: make restore-userfs)"
else
    rm -f "${BACKUP_ROOT}/.partial.tar.gz"
    echo "     board not reachable, no backup taken"
fi

# --- Keep userfs: flash a copy of the layout that skips it -----------------
# The first column of each line tells the programmer what to do with that
# partition: P = write the image into it, PE = create the partition but
# leave it Empty (nothing written, so what's already there stays). A board
# that never had a userfs gets an empty partition; its fstab line
# (x-systemd.makefs, see base-files_%.bbappend) formats it on first boot.
# The image column is set to "none" as well, the way the layout already
# marks its other empty partitions (fip-b, u-boot-env).
# The copy sits next to the original: the binary paths inside it are
# relative to the deploy directory either way.
if [ "${FULL_FLASH}" = 1 ]; then
    FLASH_TSV="${TSV_REL}"
    echo "  -> FULL_FLASH=1: userfs WILL BE ERASED (board identity + devices)"
else
    FLASH_TSV="${TSV_REL%.tsv}-keep-userfs.tsv"
    # Column 3 is the partition name; only that one line changes.
    awk -F'\t' 'BEGIN { OFS = "\t" } $3 == "userfs" && $1 == "P" { $1 = "PE"; $7 = "none"; found = 1 } { print } END { exit !found }' \
        "${TSV_REL}" > "${FLASH_TSV}" || { echo "Error: no 'P ... userfs' line in ${TSV_REL}"; exit 1; }
    echo "  -> userfs is kept (set FULL_FLASH=1 to wipe it)"
fi

echo "  -> Flashing ${FLASH_TSV}"
echo "  -> Make sure SW1 is set to recovery/USB mode (BOOT2=0, BOOT0=0) and the board is connected over USB."
read -rp "Press Enter to start flashing, or Ctrl+C to abort... "

"${CLI}" -c port=usb1 -w "${FLASH_TSV}"

# Every flash writes a fresh rootfs, which regenerates the board's SSH host
# key on first boot -- drop the stale cached entry so ssh doesn't refuse to
# connect over the mismatch.
ssh-keygen -f "${HOME}/.ssh/known_hosts" -R "stm32mp1.local" >/dev/null 2>&1 || true

echo "  -> Done. Flip SW1 back to SD/eMMC boot mode (BOOT2=1, BOOT0=1) and power-cycle the board."
