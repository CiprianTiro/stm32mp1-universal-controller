# Development MQTT broker

A local stand-in for AWS IoT Core, so `backend_daemon`'s cloud sync
(`linux_a7/backend_daemon/src/mqtt.rs`, issue #26) can be developed and
tested without a cloud account. It behaves like AWS where the board's code
can tell the difference:

- **TLS only**, port 8883, no plaintext MQTT.
- **Mutual TLS**: every client needs a certificate signed by our own dev CA.
  The certificate's Common Name is the client's identity — there are no
  passwords.
- **Per-device access rules** (`acl`): a device can only use its own
  `$aws/things/<name>/shadow/...` topics, like an AWS IoT policy.
- **AWS's real Device Shadow topic names and JSON**, so switching to AWS is
  a config change on the board, not a code change.

What it does *not* emulate: AWS's shadow *service* (the cloud-side document
that computes `delta` from `desired` vs `reported`). Here you send the delta
yourself — see "Send the board a command" below.

Runs in Docker (`eclipse-mosquitto:2`) — nothing to install on the PC.

## One-time setup

```bash
cd tools/mqtt-dev-broker
./gen_certs.sh <your-PC-IP>              # CA, broker cert, device "dk2-01"
./gen_certs.sh <your-PC-IP> operator     # a "cloud side" identity for testing
docker compose up -d                     # broker now listening on :8883
```

Certificates land in `certs/` (git-ignored — they include private keys).
The broker certificate is only valid for the IP given; if your PC's IP
changes, delete `certs/server.*` and re-run `./gen_certs.sh <new-IP>`.

## Put the identity on the board

```bash
./provision_board.sh stm32mp1.local <your-PC-IP>      # thing name defaults to dk2-01
```

This copies `ca.crt`, the device certificate and key to
`/usr/local/etc/universal-controller/mqtt/` on the board, writes
`/usr/local/etc/universal-controller/backend-daemon.env`, and restarts
`backend-daemon`. `/usr/local` is the board's `userfs` partition —
per-device storage that isn't part of the image. Re-run after a full
`make flash-hw`.

The script prints the daemon's `mqtt:` log lines; `connected to ...` means
it worked.

## Watch and command the board

Commands run inside the broker container with the `operator` identity:

```bash
X="docker exec uc-mqtt-dev-broker"
H="-h 127.0.0.1 -p 8883 --cafile /mosquitto/certs/ca.crt"
O="--cert /mosquitto/certs/operator.crt --key /mosquitto/certs/operator.key"

# See everything the board reports (every 10 s, and after each change)
$X mosquitto_sub $H $O -t '$aws/things/dk2-01/shadow/#' -v

# Send the board a command (a "delta": what should change)
$X mosquitto_pub $H $O -t '$aws/things/dk2-01/shadow/update/delta' \
  -m '{"state":{"devices":{"lamp-1":{"on":true}}}}'
```

The board applies the delta to its device state and immediately reports
back `{"state":{"reported":{"devices":{...}}}}`.

## Board-side settings

In `/usr/local/etc/universal-controller/backend-daemon.env`:

| Variable | Required | Default |
|---|---|---|
| `MQTT_BROKER_HOST` | yes (unset = cloud sync off) | — |
| `MQTT_THING_NAME` | yes | — |
| `MQTT_BROKER_PORT` | no | `8883` |
| `MQTT_CA_FILE` / `MQTT_CERT_FILE` / `MQTT_KEY_FILE` | no | `ca.crt` / `device.crt` / `device.key` in `/usr/local/etc/universal-controller/mqtt/` |

## Switching to AWS IoT Core later

No code changes. In AWS: create a Thing (e.g. `dk2-01`), let AWS create its
certificate, attach a policy allowing it `$aws/things/dk2-01/shadow/*`. On
the board: replace the three files with Amazon's root CA (`AmazonRootCA1.pem`
→ `ca.crt`) and the Thing's certificate/key, and set `MQTT_BROKER_HOST` to
your account's AWS IoT endpoint (`xxxx-ats.iot.<region>.amazonaws.com`).
