# Room sensor hardware (#86)

Four wires between the ESP32-S3 board and a sensor breakout board. No other
parts are needed: the breakout boards carry the I2C pull-up resistors.

**Wire it with USB unplugged. Connect the board through its COM port.**

## Parts

| | Part | Notes |
|---|---|---|
| U1 | ESP32-S3 dev board, YD-ESP32-S3 N16R8 | the same board as the IR blaster; the spare one |
| U2 | sensor breakout: GY-SHT21 / GY-21 | the chip on it varies: SHT21, HTU21D, Si7021 -- or, on the board bought for this, a **TI HDC1080**. Any of them, or an SHT3x or AHT20 board: same four wires, found by itself |
| — | 4 female-female jumper wires, 10–20 cm | the length is the point: it keeps the sensor away from the board's warmth |

## Wiring

| Sensor pin | Wire | Board pin |
|---|---|---|
| `VIN` (or `VCC`) | red | `3V3` |
| `GND` | black | `GND` |
| `SDA` | any colour | `8` (GPIO8) |
| `SCL` | another colour | `9` (GPIO9) |

- **Power the sensor from `3V3`, never `5V`.** The breakout's pull-up
  resistors then pull SDA and SCL up to 3.3 V, which is what the ESP32's pins
  can take. Most GY-21 boards have a regulator and would run on 5 V too, but
  some pull SDA/SCL up to their supply, and ESP32 pins are **not 5 V
  tolerant**.
- Use the pins **printed** `8` and `9` on the board, not positions counted
  along the header. On the YD-ESP32-S3 they are on the left header (USB
  ports at the bottom), between `3V3` at the top and `GND` further down.
- **Why GPIO 8 and 9:** they have no special function on the N16R8. The
  off-limits pins are in [the IR blaster's notes](../../firmware_ir_blaster/hardware/README.md#why-these-gpios):
  26–37 (flash/PSRAM), 0/3/45/46 (strapping), 19/20 (USB), 43/44 (console).

## Checking the wiring

On the console (`make flash-sensor`, or `make monitor-sensor`):

```
sensor> i2cscan
I2C bus on SDA=GPIO8, SCL=GPIO9:
  0x40 answers
(SHT3x: 0x44/0x45, AHT20: 0x38, HDC1080 or SHT21/HTU21/Si7021: 0x40)
sensor> read
hdc1080: 24.08 C, 71.6 % humidity
```

Nothing answers: SDA and SCL swapped (the most common mistake), a loose
jumper, or no 3V3/GND. The start-up log says `found hdc1080 at 0x40` (or
`sht21`, ...) once the sensor is seen. A sensor plugged in later is found
within a minute.

**Something answers but measuring fails:** a chip the firmware doesn't know,
at a known address. The "GY-SHT21" bought for this turned out to carry a TI
HDC1080, not an SHT21: it took the SHT21's reset byte (0xFE, for the HDC1080
just a register address) but refused its measure command. Asking its ID
registers told it apart: `i2c 0x40 w FE r 2` → `54 49` ("TI"), `i2c 0x40 w
FF r 2` → `10 50` (HDC1080). Both are supported now.

## Placement

The ESP32 with its WiFi on is a small heater. Even at 80 MHz with modem
sleep it warms itself and the air right around it.

1. **Sensor below the board, not above:** warm air rises.
2. **On the cable, in free air:** not touching the board, not inside a
   closed case with it. If the board goes into a case, the sensor stays
   outside it or in its own vented corner.
3. **Where the room's air is:** at roughly head height when sitting, away
   from radiators, windows, direct sun and the hub's own warm boards.

## Verifying it against a reference thermometer

1. Put a thermometer you trust right next to the sensor (not next to the
   board).
2. Leave both for **30 minutes**: the board takes a while to reach its
   steady warmth, and so does the thermometer.
3. Compare. Within ±0.5 °C: done. More: move the sensor further from the
   board, wait again. A difference that stays the same, wherever you put it,
   is the sensor's own error: set **Temperature correction** on the device
   (−2 to +1 °C in half degrees).

Humidity follows temperature: air 1 °C warmer reads about 6 % lower relative
humidity. A sensor that reads warm because of the board also reads too dry.
The correction fixes the temperature; placement fixes both.
