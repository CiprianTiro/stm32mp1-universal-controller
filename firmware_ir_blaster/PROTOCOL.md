# Hub ↔ IR blaster protocol (v1)

How the hub talks to an IR blaster (#42). The firmware side is
`main/hub_link.c`, the hub side the `ir-blaster` adapter in `backend_daemon`.

## Finding it

The blaster announces itself by mDNS:

| | |
|---|---|
| service | `_uc-irblaster._tcp`, port **7443** |
| host name | `irb-<mac>.local`, e.g. `irb-7ce8b1b091e4.local` |
| TXT `id` | `irb-<mac>`, the blaster's stable id |
| TXT `fw` | firmware version, e.g. `0.2.0` |
| TXT `paired` | `1` if it already belongs to a hub, else `0` |
| TXT `proto` | protocol version, `1` |

## Connection: mutual TLS, pinned both ways

- The **hub connects**, the blaster is the TLS server (TLS 1.2, ECDSA P-256).
  One hub connection at a time; a new one replaces the old.
- **Both sides present a certificate.** The blaster's is self-signed, made on
  its first start (key made on the chip, never leaves it). The hub uses its
  own identity certificate (`tls.rs`, the one the app pins too).
- Nobody checks certificates against an authority. Each side checks the
  other's **fingerprint**: SHA-256 of the certificate (DER), 64 lowercase hex
  digits. The hub pins the blaster's at pairing, the blaster the hub's.
- **Paired blaster:** a connection whose client certificate isn't the pinned
  hub's is closed right after the handshake.
- **Unpaired blaster:** any client certificate is accepted, but only `hello`
  and `pair` are allowed until pairing succeeds.

## Pairing

Every blaster has a **pairing code**: 16 characters, shown as
`XXXX-XXXX-XXXX-XXXX` (Crockford base32: digits and A-Z without I, L, O, U;
80 random bits). Made on first start, printed on the serial console at
start and by the `pairing` command, later on a label. It never changes, so a
printed label stays valid.

The code proves both sides are who the person means, without ever being
sent. Let `FB` = the blaster's certificate fingerprint and `FH` = the hub's,
both as seen **in this TLS connection**, and `code` = the code without
dashes, upper case:

```
hub proof     = HMAC-SHA256(key = code, "uc-irb-pair-v1 hub "     + FB + " " + FH)
blaster proof = HMAC-SHA256(key = code, "uc-irb-pair-v1 blaster " + FB + " " + FH)
```

(hex, lowercase). The hub sends its proof; the blaster checks it, pins `FH`
and answers with its own proof; the hub checks that and pins `FB`. A machine
in the middle has different certificates, so its proofs can't match without
the code. After **5 wrong proofs** the blaster refuses pairing until it's
restarted.

## Messages

One JSON object per line (`\n`-terminated, UTF-8), at most 16 KiB per line.

**Requests** (hub → blaster) carry an `id` the reply repeats:

```json
{"id": 7, "op": "send", "code": {"proto": "nec", "address": 0, "command": 64}}
```

**Replies** (blaster → hub): `{"id": 7, "ok": true, ...}` or
`{"id": 7, "ok": false, "error": "<kind>", "detail": "<text for the log>"}`.

**Events** (blaster → hub, no `id`): `{"event": "heard", "code": {...}}`.

| op | request fields | reply fields | notes |
|---|---|---|---|
| `hello` | – | `device`, `fw`, `proto`, `paired`, `fingerprint` (FB) | allowed unpaired |
| `pair` | `proof` | `proof` | only when unpaired |
| `ping` | – | – | the hub sends one every 20 s; a connection silent for 60 s is closed |
| `send` | `code`, optional `repeats` (0-20, NEC only) | – | reply after the IR frame has gone out |
| `learn` | optional `timeout_ms` (1000-60000, default 15000) | `code` | reply when the receiver hears a remote (not our own send), or `error: "timeout"` |
| `unpair` | – | – | the blaster forgets the hub; the connection is closed after the reply |

Events:

| event | fields | when |
|---|---|---|
| `heard` | `code` | the receiver heard a remote while no `learn` was waiting (so the hub can follow the original remote) |

Error kinds: `bad_request`, `not_paired`, `already_paired`, `bad_proof`,
`locked`, `timeout`, `busy`, `failed`.

## Codes

```json
{"proto": "nec", "address": 0, "command": 64}
{"proto": "raw", "carrier_hz": 38000, "timings": [9000, 4500, 560, 560, 560, 1690, 560]}
```

- `nec`: `address` 0-65535 (above 255 = extended NEC), `command` 0-255.
- `raw`: microseconds, alternating mark (LED on, carrier) and space (off),
  starting with a mark; 1-1024 values, each 1-32767. `carrier_hz` 30000-60000.
- `learn` and `heard` return `nec` when the frame decodes as NEC, else `raw`.
