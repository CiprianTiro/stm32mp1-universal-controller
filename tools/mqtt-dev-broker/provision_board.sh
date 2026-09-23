#!/usr/bin/env bash
# provision_board.sh -- gives a running board its MQTT identity and config:
# copies the CA + device certificate/key onto it and writes the config file
# backend-daemon reads, then restarts the daemon.
#
# Usage:
#   ./provision_board.sh <board-host> <broker-host> [thing-name]
#   ./provision_board.sh stm32mp1.local 192.168.1.141            # thing "dk2-01"
#
# Everything lands on the board's userfs partition (/usr/local), which is
# per-device storage: it isn't part of the image, so it survives reboots
# and future rootfs updates. A full `make flash-hw` may rewrite userfs --
# re-run this script after flashing.
#
# AWS IoT Core: same file layout, just different files. Put Amazon's root
# CA (AmazonRootCA1.pem) as ca.crt and the certificate/key AWS generated for
# the Thing as <thing>.crt / <thing>.key in certs/aws/ (git-ignored, like
# all of certs/), then point CERTS_DIR at it and use the AWS endpoint:
#   CERTS_DIR=certs/aws ./provision_board.sh stm32mp1.local \
#       a1q88gwapwaxnn-ats.iot.eu-central-1.amazonaws.com
# Switching back to the dev broker is just running it again without
# CERTS_DIR and with the PC's IP.
set -euo pipefail

BOARD="${1:?usage: $0 <board-host> <broker-host> [thing-name]}"
BROKER="${2:?usage: $0 <board-host> <broker-host> [thing-name]}"
THING="${3:-dk2-01}"

cd "$(dirname "$0")"
# Which certificate set to install: the dev CA's (default) or AWS's.
CERTS="${CERTS_DIR:-certs}"
DEST=/usr/local/etc/universal-controller
SSH_OPTS=(-o StrictHostKeyChecking=accept-new)

# Create the device's certificate if it doesn't exist yet (dev CA only --
# AWS certificates can only come from AWS).
if [ ! -f "${CERTS}/${THING}.crt" ]; then
  if [ "${CERTS}" != certs ]; then
    echo "error: ${CERTS}/${THING}.crt not found" >&2
    exit 1
  fi
  ./gen_certs.sh "$BROKER" "$THING"
fi

echo "-> copying certificate for '${THING}' to ${BOARD}:${DEST}/mqtt/"
# 700 on the directory, 600 on the key: only root can read the device's
# private key -- whoever has it can impersonate this device.
ssh "${SSH_OPTS[@]}" "root@${BOARD}" "mkdir -p ${DEST}/mqtt && chmod 700 ${DEST}/mqtt"
scp -q "${SSH_OPTS[@]}" "${CERTS}/ca.crt" "root@${BOARD}:${DEST}/mqtt/ca.crt"
scp -q "${SSH_OPTS[@]}" "${CERTS}/${THING}.crt" "root@${BOARD}:${DEST}/mqtt/device.crt"
scp -q "${SSH_OPTS[@]}" "${CERTS}/${THING}.key" "root@${BOARD}:${DEST}/mqtt/device.key"
ssh "${SSH_OPTS[@]}" "root@${BOARD}" "chmod 600 ${DEST}/mqtt/device.key"

echo "-> writing ${DEST}/backend-daemon.env"
# Certificate paths aren't listed: mqtt.rs defaults to exactly the three
# files copied above.
ssh "${SSH_OPTS[@]}" "root@${BOARD}" "cat > ${DEST}/backend-daemon.env" <<EOF
# backend-daemon per-device settings (written by provision_board.sh)
MQTT_BROKER_HOST=${BROKER}
MQTT_THING_NAME=${THING}
EOF

echo "-> restarting backend-daemon"
ssh "${SSH_OPTS[@]}" "root@${BOARD}" "systemctl restart backend-daemon && sleep 3 && journalctl -u backend-daemon -n 20 --no-pager | grep mqtt"
