#!/usr/bin/env bash
# =============================================================================
# Puts a userfs backup taken by flash_hw.sh back onto the board (issue #56):
# the board's identity (AWS certificate + key, backend-daemon.env) and the
# device registry. Needed only after FULL_FLASH=1, or if a normal flash
# lost userfs after all.
#
# Usage:  ./restore_userfs.sh [path/to/userfs.tar.gz]   (default: newest)
#         BOARD_HOST=192.168.1.130 ./restore_userfs.sh
# =============================================================================
set -euo pipefail

BOARD_HOST="${BOARD_HOST:-stm32mp1.local}"
BACKUP_ROOT="$(cd "$(dirname "$0")" && pwd)/userfs-backups"

# Default: the newest backup. The folder names are timestamps
# (YYYYMMDD-HHMMSS), so sorting them by name sorts them by date.
ARCHIVE="${1:-$(ls -d "${BACKUP_ROOT}"/*/userfs.tar.gz 2>/dev/null | sort | tail -n 1)}"
if [ -z "${ARCHIVE}" ] || [ ! -f "${ARCHIVE}" ]; then
    echo "Error: no backup found in ${BACKUP_ROOT}"
    exit 1
fi

echo "  -> Restoring ${ARCHIVE} to ${BOARD_HOST}:/usr/local"
# The archive is streamed over ssh and unpacked ON the board. tar running
# as root keeps the files' original owners and modes (700 on the mqtt
# directory, 600 on the private key). The daemon is stopped first so it
# can't save its (empty) registry over the restored one.
ssh -o StrictHostKeyChecking=accept-new "root@${BOARD_HOST}" \
    'systemctl stop backend-daemon && tar xzf - -C /usr/local && systemctl start backend-daemon' < "${ARCHIVE}"

echo "  -> Done. Check: ssh root@${BOARD_HOST} journalctl -u backend-daemon -n 20"
