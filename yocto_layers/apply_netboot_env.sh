#!/usr/bin/env bash
# =============================================================================
# Triggers a ONE-SHOT netboot over serial: sets the `netboot` env var in RAM
# and runs it immediately, but never touches `bootcmd` and never `saveenv`s.
# This is deliberate -- nothing here is persisted to the board's eMMC, so a
# failed TFTP/NFS boot (server down, bad path, whatever) never strands the
# board on a broken default boot path. It just falls back to the persisted
# `bootcmd` (normal eMMC boot) on the next power-cycle, no `make flash-hw`
# recovery needed. Called from sync_netboot.sh -- run it again any time you
# want to netboot after that.
#
# Needs `expect` (sudo apt install expect) and bootdelay=1 in the stock env,
# so it starts watching serial *before* triggering a reboot -- reacting to
# the "Hit any key" banner takes low milliseconds, well inside that window.
# =============================================================================
set -euo pipefail

SERIAL_DEV="${SERIAL_DEV:-/dev/ttyACM0}"
SERVERIP="${SERVERIP:-192.168.1.141}"
NFS_ROOTPATH="${NFS_ROOTPATH:-/srv/nfs/stm32mp1-rootfs}"
BOARD_HOST="${BOARD_HOST:-stm32mp1.local}"

if ! command -v expect >/dev/null 2>&1; then
    echo "  -> WARNING: 'expect' not installed, skipping netboot env push (host files are still synced)."
    echo "     Install it once with: sudo apt install expect"
    echo "     Then push the env yourself per the Network-Boot wiki page, or re-run this script."
    exit 0
fi

if [ ! -e "${SERIAL_DEV}" ]; then
    echo "  -> WARNING: ${SERIAL_DEV} not found, skipping netboot env push (host files are still synced)."
    echo "     Connect the board's ST-Link USB and re-run: ./apply_netboot_env.sh"
    exit 0
fi

# Prefer a clean reboot if the board's already up and reachable; otherwise
# assume it's freshly flashed / powered off and ask for a manual power-cycle.
if ssh -o BatchMode=yes -o ConnectTimeout=3 "root@${BOARD_HOST}" true 2>/dev/null; then
    echo "  -> Board reachable over SSH, triggering a clean reboot..."
    ssh -o BatchMode=yes "root@${BOARD_HOST}" reboot || true
else
    echo "  -> Board not reachable over SSH."
    read -rp "     Power-cycle it now, then press Enter (the script is already watching serial)... "
fi

# The \\\$ (not \$) is deliberate: this string is embedded inside a TCL
# double-quoted "send ..." string below, and TCL treats $ as its own
# variable-substitution character. \$ in the final text tells TCL "literal
# dollar sign", so the U-Boot variable references reach the board intact
# instead of TCL trying (and failing) to substitute its own variables.
NETBOOT_CMD="dhcp; setenv serverip ${SERVERIP}; tftp \\\${kernel_addr_r} uImage; tftp \\\${fdt_addr_r} stm32mp157f-dk2.dtb; setenv bootargs console=\\\${console},\\\${baudrate} root=/dev/nfs nfsroot=\\\${serverip}:${NFS_ROOTPATH},tcp,v3 ip=dhcp rw; bootm \\\${kernel_addr_r} - \\\${fdt_addr_r}"

expect <<EOF
set timeout 30
log_user 1

exec stty -F ${SERIAL_DEV} 115200 raw -echo
set fd [open ${SERIAL_DEV} r+]
fconfigure \$fd -blocking 0 -buffering none
spawn -open \$fd

expect {
    "Hit any key to stop autoboot" {
        send "\r"
    }
    timeout {
        puts "\n  -> Timed out waiting for autoboot prompt -- board may not have power-cycled in time."
        exit 1
    }
}

expect "STM32MP>"
send "setenv netboot '${NETBOOT_CMD}'\r"
expect "STM32MP>"
send "run netboot\r"
expect {
    "login:" { puts "\n  -> Netboot succeeded (RAM-only -- bootcmd untouched, nothing saved to eMMC)." }
    timeout  { puts "\n  -> Boot started but didn't see a login prompt within timeout -- check serial manually." }
}
EOF

echo "  -> Done."
