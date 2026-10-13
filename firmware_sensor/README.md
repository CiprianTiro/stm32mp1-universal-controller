# firmware_sensor

Firmware for the ESP32-S3 room sensor add-on
([#86](https://github.com/CiprianTiro/stm32mp1-universal-controller/issues/86)):
temperature and humidity, measured once a minute and sent to the hub.
Wiring and parts: [hardware/README.md](hardware/README.md). The protocol:
[PROTOCOL.md](PROTOCOL.md). It's the same board as the IR blaster (YD-ESP32-S3
N16R8). Setup, pairing and the hub connection are the IR blaster's too: the
two share that code in [`../firmware_common/uc_addon/`](../firmware_common/uc_addon/uc_addon.h).

## Supported sensors

The sensor is found by itself at start: no setting to change.

| Sensor | I2C address | Boards |
|---|---|---|
| Sensirion SHT3x (SHT30, SHT31, SHT35) | 0x44 / 0x45 | GY-SHT31-D, ... |
| Aosong AHT20 (AHT21, AHT25) | 0x38 | AHT20 boards, AHT20+BMP280 combos |
| Texas Instruments HDC1080 | 0x40 | sold as "GY-SHT21" too: the one on the bench (2026-10-13) was one |
| Sensirion SHT21, HTU21D, Si7021 | 0x40 | GY-21, GY-SHT21 |

## Build, flash, console

ESP-IDF v6.0.3, set up as in [the IR blaster's README](../firmware_ir_blaster/README.md#one-time-setup).
From the repo root:

```bash
make build-sensor                          # compile only
make flash-sensor                          # build + flash + open the console (Ctrl+] quits)
make flash-sensor IR_PORT=/dev/ttyACM1     # if the board is on another port
make monitor-sensor                        # console only
```

Connect the board through its **COM** port, as for the blaster. If flashing
can't connect: hold **BOOT**, press and release **RST**, release **BOOT**,
then run it again. Afterwards press **RST** to start the new firmware.

## Console commands

| Command | What it does |
|---|---|
| `read` | Measure now and print: `hdc1080: 24.08 C, 71.6 % humidity` |
| `i2cscan` | List what answers on the I2C bus: the first check when no sensor is found |
| `i2c <address> [w <hex bytes>] [wait <ms>] [r <count>]` | Development: talk to a chip by hand. `i2c 0x40 w FE r 2` answering `54 49` ("TI") is how the HDC1080 was recognised |
| `status` | WiFi, hub connection, firmware version |
| `wifi <name> <password>` | Join a network (development; normally set over Bluetooth). `wifi forget` forgets it and restarts into Bluetooth setup |
| `pairing` | The pairing code the hub's wizard asks for |
| `unpair` | Forget the hub |
| `factory-reset yes` | Forget WiFi, hub and identity: new key, **new pairing code** |
| `help` | List commands |

The last five are shared with the IR blaster (`uc_addon/addon_console.c`).
**Holding BOOT for 5 s** forgets the WiFi and restarts into Bluetooth setup,
as on the blaster.

## Adding it to the hub

Devices → Add → **Room sensor** →
- **New sensor (set up over Bluetooth)**: the pairing code (console:
  `pairing`), the WiFi name and password. The hub sends the WiFi over
  Bluetooth, then pairs.
- **Already on the WiFi: find it**: picks it from the network, then asks for
  the code.

The test step shows the first reading. The device then shows temperature and
humidity, updated every minute, and has one setting: **Temperature
correction**, −2 to +1 °C in half degrees. Use it if the sensor reads a
constant amount too high or too low next to a thermometer you trust.

## Keeping it cool

A sensor next to an ESP32 reads the ESP32's warmth, not the room's. So:

- **80 MHz CPU** (`sdkconfig.defaults`): it has almost nothing to do.
- **WiFi modem sleep** (`uc_addon.h`'s `wifi_power_save`): the radio sleeps
  between the router's beacons. The hub's messages arrive ~0.1–0.3 s later,
  which doesn't matter for a sensor.
- **PSRAM off**: not needed, and it would draw power.
- **The sensor on a cable**, below the board (heat rises), outside any case.
  See [hardware/README.md](hardware/README.md).

## Files

| File | What |
|---|---|
| `main/main.c` | start-up, what makes it a sensor (`uc_addon_config_t`), console `read` / `i2cscan` |
| `main/sensor.c` | finds the sensor on the I2C bus, measures every 60 s (SHT3x, AHT20, HDC1080, SHT21: commands, CRC, formulas) |
| `main/sensor_link.c` | the sensor's part of the hub connection: `read`, the `reading` event |
| `main/version.h` | firmware and protocol version |
| `sdkconfig.defaults` | board settings: 16 MB flash, PSRAM off, 80 MHz, console on COM, TLS options |
| `partitions.csv` | the blaster's layout: two 4 MB app slots, NVS, NVS keys |
| `PROTOCOL.md` | what differs from the blaster's protocol |
