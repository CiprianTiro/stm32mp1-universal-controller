# firmware_m4

Cortex-M4 firmware for the STM32MP157F-DK2, built on Zephyr RTOS (decision
recorded in GitHub issue #11 / superseded #17 — Zephyr from the start, not a
FreeRTOS-first staging plan). Loaded and controlled from the A7 side via
Linux's `remoteproc` framework; talks to `linux_a7/backend_daemon` over
RPMsg (Task 10, not yet implemented — this is the Hello World stage: a
GPIO+console heartbeat, no IPC yet).

This app currently just toggles the on-board LED (`led0` / LD7) and prints
an incrementing counter once a second — proof that `west`/the Zephyr SDK
toolchain, the board target, and `remoteproc` load/start/stop all work end
to end, before any IPC is layered on top.

## One-time workspace setup

This directory is a [T2-topology](https://docs.zephyrproject.org/latest/develop/west/workspaces.html)
Zephyr application: it has its own `west.yml` pinning the Zephyr version,
rather than vendoring Zephyr itself into this repo (it's a multi-GB
checkout — same reasoning as keeping Yocto's `poky` etc. out of
hand-written directories, just via a west workspace instead of a
submodule). `west init` fetches Zephyr and its HAL modules as siblings of
`firmware_m4/` — run it from the repo root, not from inside this directory:

```bash
# From the repo root:
python3.12+ -m venv ~/zephyrproject/.venv   # west needs Python >= 3.12
source ~/zephyrproject/.venv/bin/activate
pip install west

west init -l firmware_m4
west update                                  # pulls zephyr/ + modules/, ~3-4 GB
pip install -r zephyr/scripts/requirements.txt
west sdk install --toolchains arm-zephyr-eabi
```

System packages needed first (Ubuntu/Debian): `git cmake ninja-build gperf
ccache dfu-util device-tree-compiler python3-venv`.

`zephyr/`, `modules/`, `tools/`, `bootloader/`, and `.west/` land at the
repo root as a side effect of `west init -l` — they're git-ignored, not
part of this repo.

## Build

```bash
source ~/zephyrproject/.venv/bin/activate
west build -p always -b stm32mp157c_dk2 firmware_m4 -d firmware_m4/build
```

(`stm32mp157c_dk2` is Zephyr's upstream board target — it matches our
DK2's M4 core, RAM/flash carveouts, and 4" MIPI-DSI touch panel exactly;
the A7-side security differences between the C/F SoC variants don't affect
the M4.)

Output: `firmware_m4/build/zephyr/zephyr.elf`.

## Deploy (on the DK2 target, as root)

```bash
cp zephyr.elf /lib/firmware/
echo zephyr.elf > /sys/class/remoteproc/remoteproc0/firmware
echo start > /sys/class/remoteproc/remoteproc0/state
```

Heartbeat output (RAM console, not a wired UART by default — see
`stm32mp157c_dk2_defconfig`) shows up at:

```bash
cat /sys/kernel/debug/remoteproc/remoteproc0/trace0
```

Stop/restart without rebooting the A7:

```bash
echo stop > /sys/class/remoteproc/remoteproc0/state
echo start > /sys/class/remoteproc/remoteproc0/state
```
