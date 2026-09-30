# firmware_ir_blaster

Firmware for the ESP32-S3 IR blaster add-on
([#42](https://github.com/CiprianTiro/stm32mp1-universal-controller/issues/42)).
Wiring, parts and pinouts: wiki page **IR-Blaster-Hardware**.

**Current stage: hardware bring-up, verified.** The receiver prints every IR
frame it hears (decoded if NEC, raw otherwise), and a serial console sends NEC
codes or replays the last frame heard. Tested on an IR LED strip and a small
NEC projector (~2 m range with two LEDs). Network, pairing, mutual TLS, the
code library and signed OTA come next (see the issue's DoD).

## Why ESP-IDF, not Zephyr

The DoD prefers Zephyr (same RTOS as the M4) but allows ESP-IDF if Zephyr
lacks RMT support, and it does: Zephyr v4.4.2 has no driver for the ESP32's
RMT peripheral, the hardware that sends and measures IR pulses. ESP-IDF also
brings what later steps need: secure boot v2, flash encryption, signed OTA,
mDNS, BLE provisioning.

## One-time setup

ESP-IDF **v6.0.3** in `~/esp/esp-idf` (no sudo needed):

```bash
mkdir -p ~/esp && cd ~/esp
git clone -b v6.0.3 --recursive --shallow-submodules --depth 1 \
    https://github.com/espressif/esp-idf.git esp-idf
cd esp-idf && ./install.sh esp32s3
```

Somewhere else works too: pass `IDF_PATH=/path/to/esp-idf` to the make
commands. Your user must be in the `dialout` group to open the serial port.

**Connect the board through its COM port** (the USB-serial chip, shows up as
`/dev/ttyACM0`), not the one marked USB: the console runs on COM, and only
COM powers the board's `5Vin` pin, which feeds the IR LEDs. That also needs
the board's `IN-OUT` pads bridged; see the wiki page IR-Blaster-Hardware.

## Build, flash, console

From the repo root:

```bash
make build-ir                         # compile only
make flash-ir                         # build + flash + open the console (Ctrl+] quits)
make flash-ir IR_PORT=/dev/ttyACM1    # if the board is on another port
make monitor-ir                       # console only
```

If flashing can't connect: hold **BOOT**, press and release **RST**,
release **BOOT** (forces the chip's download mode), then run it again.
Press **RST** afterwards to start the new firmware. (Only needed on a
board that still runs other firmware, or through the native USB port.)

## Console commands

| Command | What it does |
|---|---|
| `nec <address> <command> [repeats]` | Send an NEC code. LG TV power: `nec 0x04 0x08`. With repeats, adds "button held" frames |
| `replay` | Send the last frame the receiver heard ("learn from remote") |
| `last` | Print the last frame again, decoded + raw durations |
| `duty <10-50>` | Carrier duty cycle in % (default 33), for range experiments |
| `ledtest [seconds]` | Hardware check: LEDs steadily on, 1-5 s (default 3). LED 1's long leg should read ~2.9 V |
| `help` | List commands |

Every frame the receiver hears prints as, for example:

```
IR received: NEC address=0x04 command=0x08 (0x20DF10EF)
IR received: unknown protocol, 67 marks, durations in us:
 +3400 -1700 +420 -1300 ...
```

The receiver also hears the blaster's own LEDs, directly or reflected:
sending `nec 0x04 0x08` prints `IR received (our own send): NEC ...`. That's
a quick self-test of the whole chain. Such echoes are never kept as the
"last frame", so `replay` always sends what a real remote sent.

Receive filtering: pulses under 200 us are joined back into their
neighbours (breadboard noise can cut a 9 ms NEC leader in two), and
unrecognised frames under 8 marks are dropped as noise.

## Files

| File | What |
|---|---|
| `main/main.c` | start-up, console commands, printing |
| `main/ir_tx.c` | sending: RMT channel on GPIO4, 38 kHz carrier |
| `main/ir_rx.c` | receiving: RMT channel on GPIO5, frame hand-over |
| `main/ir_nec.c` | NEC protocol encode/decode |
| `main/ir_frame.h` | the shared frame format (mark/space durations in us) |
| `sdkconfig.defaults` | board settings: 16 MB flash, octal PSRAM, USB console |
