#!/usr/bin/env bash
# gen_certs.sh -- creates everything the development MQTT broker needs for
# mutual TLS (both sides prove who they are with a certificate), the same
# way AWS IoT Core works -- just with our own certificate authority (CA)
# instead of Amazon's.
#
# Produces, in ./certs/ (git-ignored -- private keys never go in the repo):
#   ca.crt / ca.key          our dev CA: signs everything below. Whoever
#                            holds ca.key can mint devices -- keep it here.
#   server.crt / server.key  the broker's identity. The board checks this
#                            against ca.crt, and that the address it
#                            connected to is listed inside it.
#   <thing>.crt / <thing>.key  one device's identity. The certificate's
#                            Common Name (CN) IS the device name: the broker
#                            uses it as the username, and the ACL only lets
#                            that name use its own topics.
#
# Usage:
#   ./gen_certs.sh <broker-ip> [thing-name]
#   ./gen_certs.sh 192.168.1.141            # CA + server + device "dk2-01"
#   ./gen_certs.sh 192.168.1.141 dk2-02     # adds another device, reuses CA
#
# Re-running is safe: the CA and server cert are only created if missing,
# so existing device certs stay valid. Delete certs/ to start over.
set -euo pipefail

BROKER_IP="${1:?usage: $0 <broker-ip> [thing-name]}"
THING="${2:-dk2-01}"
DAYS=825   # ~2 years; long-lived is fine for a dev CA

cd "$(dirname "$0")"
mkdir -p certs
cd certs

# --- 1. Certificate authority (once) ---------------------------------------
if [ ! -f ca.key ]; then
  echo "-> creating dev CA"
  openssl req -x509 -newkey rsa:2048 -nodes -sha256 -days "$DAYS" \
    -keyout ca.key -out ca.crt -subj "/CN=universal-controller dev CA"
fi

# --- 2. Broker (server) certificate (once) ---------------------------------
# subjectAltName lists every address the board may use to reach the broker.
# TLS clients check it: connecting to 192.168.1.141 only works if that exact
# IP is in here. Add more with extra IP:/DNS: entries if your PC's address
# changes (then delete server.* and re-run).
if [ ! -f server.key ]; then
  echo "-> creating broker certificate for ${BROKER_IP}"
  openssl req -newkey rsa:2048 -nodes -sha256 \
    -keyout server.key -out server.csr -subj "/CN=${BROKER_IP}"
  openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key -CAcreateserial \
    -days "$DAYS" -sha256 -out server.crt \
    -extfile <(printf "subjectAltName=IP:%s,DNS:localhost,IP:127.0.0.1\n" "$BROKER_IP")
  rm server.csr
fi

# --- 3. Device certificate -------------------------------------------------
# RSA 2048 matches what AWS IoT issues for devices, so the board-side code
# is exercised with the same kind of key it will get in production.
if [ ! -f "${THING}.key" ]; then
  echo "-> creating device certificate for ${THING}"
  openssl req -newkey rsa:2048 -nodes -sha256 \
    -keyout "${THING}.key" -out "${THING}.csr" -subj "/CN=${THING}"
  openssl x509 -req -in "${THING}.csr" -CA ca.crt -CAkey ca.key -CAcreateserial \
    -days "$DAYS" -sha256 -out "${THING}.crt"
  rm "${THING}.csr"
fi

# The broker runs as its own user (uid 1883) inside the container, so its
# key must be world-readable to be usable through the bind mount. Fine for
# a dev-only key on your own PC; a real deployment would chown it instead.
# The CA and device keys stay private to you.
chmod 644 ca.crt server.crt server.key "${THING}.crt"
chmod 600 ca.key "${THING}.key"

echo
echo "Done. Device '${THING}' needs these three files on the board:"
echo "  certs/ca.crt  certs/${THING}.crt  certs/${THING}.key"
