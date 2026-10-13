# Hub ↔ room sensor protocol (v1)

How the hub talks to a room sensor (#86). The firmware side is
`main/sensor_link.c` (on top of `firmware_common/uc_addon/hub_link.c`), the
hub side the `room-sensor` adapter in `backend_daemon`.

The connection works **exactly like the IR blaster's**: the same mutual TLS
pinned both ways, the same pairing with a 16-character code, the same
message framing and the same `hello`, `pair`, `ping` and `unpair` requests.
They share the code that does it. Read
[the IR blaster's PROTOCOL.md](../firmware_ir_blaster/PROTOCOL.md) first;
this page lists only what differs.

## Names

The sensor uses its own names, so a hub never mistakes it for a blaster:

| | IR blaster | Room sensor |
|---|---|---|
| mDNS service | `_uc-irblaster._tcp` | `_uc-sensor._tcp` |
| device id / host name | `irb-<mac>` | `ths-<mac>` |
| Bluetooth setup name | `PROV_IRB_xxxx` | `PROV_THS_xxxx` |
| Bluetooth setup service | `1f2b9c64-3a5e-4c1d-9f0a-5b6e7d8c9a01` | `0395185a-39fd-49b5-bcfb-ee47c0a2f65b` |
| pairing proof label | `uc-irb-pair-v1` | `uc-ths-pair-v1` |
| the add-on's proof role | `blaster` | `sensor` |

Port **7443** and the TXT records (`id`, `fw`, `paired`, `proto`) are the same.

The pairing proofs, with `FS` = the sensor's certificate fingerprint:

```
hub proof    = HMAC-SHA256(key = code, "uc-ths-pair-v1 hub "    + FS + " " + FH)
sensor proof = HMAC-SHA256(key = code, "uc-ths-pair-v1 sensor " + FS + " " + FH)
```

A different label per product means a proof made for one kind of add-on
is never valid for another.

## Messages

| op | request fields | reply fields | notes |
|---|---|---|---|
| `read` | – | a reading (below) | the latest measurement, at once (it doesn't measure again) |

| event | fields | when |
|---|---|---|
| `reading` | a reading (below) | after every measurement (every 60 s), when the sensor is found or lost, and right after the hub connects |

A **reading**:

```json
{"sensor": "sht21", "temperature": 21.43, "humidity": 48.2, "age_s": 12}
```

- `sensor`: which sensor was found: `sht3x` (SHT30/31/35), `aht20`
  (AHT20/21/25), `hdc1080` (TI HDC1080), `sht21` (SHT21, HTU21D, Si7021).
  `""` means no sensor answers on the bus; then the other three fields are
  left out.
- `temperature`: °C, 2 decimals. `humidity`: % relative humidity, 0–100, 1
  decimal. These are the sensor's own values: the hub applies the
  per-device temperature correction, not the sensor.
- `age_s`: seconds since the measurement. A failed measurement (a loose
  cable) keeps the previous reading, so its age grows. After 3 failures in a
  row the sensor counts as gone (`sensor: ""`) and is looked for again every
  minute.

A sensor whose WiFi sleeps between the router's beacons (it does: less
heat) answers about 0.1–0.3 s later than the blaster. The hub's ping every
20 s and its 45 s silence limit allow for that easily.
