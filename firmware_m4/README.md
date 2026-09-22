# firmware_m4

Cortex-M4 firmware for the STM32MP157F-DK2, built on Zephyr RTOS (decision
recorded in GitHub issue #11 / superseded #17 — Zephyr from the start, not a
FreeRTOS-first staging plan). Loaded and controlled from the A7 side via
Linux's `remoteproc` framework; talks to `linux_a7/backend_daemon` over
RPMsg (issue #12).

Three things run on the M4 concurrently: a heartbeat thread (LED toggle +
`printk` counter, issue #11 — proof `west`/the SDK/`remoteproc` load-start-
stop all work before any IPC is layered on top), and two threads handling
the RPMsg link (issue #12) — see "RPMsg protocol" below.

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
cp zephyr.elf /lib/firmware/rproc-m4-fw   # matches the board DT's default firmware name
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

`stop` can take up to ~15s: the firmware doesn't ack the shutdown request
(no code here handles it), so remoteproc falls back to force-stopping via
the RCC reset line after a timeout — `state` stays `running` until then,
that's expected, not stuck.

## RPMsg protocol (v0)

Endpoint channel name: `rpmsg-raw` — the specific name Linux's in-tree
`rpmsg_char` driver auto-binds to (`drivers/rpmsg/rpmsg_char.c`'s
`rpmsg_chrdev_id_table`), so a plain `/dev/rpmsgN` shows up for userspace
to open directly once the M4 announces it — no custom kernel module
needed, unlike ST's own `rpmsg-client-sample`/`rpmsg-tty` samples.

Deliberately minimal, not meant to survive Sprint 3 - just proves the link
works end to end before any real framing exists:

- **Request** (A7 → M4): arbitrary text bytes.
- **Response** (M4 → A7): `ACK <n>: <the request bytes>`, where `<n>` is a
  message counter kept on the M4 side (state, not just an echo — proof this
  is a genuine two-way round trip).

`linux_a7/backend_daemon/src/rpmsg.rs` is the A7-side counterpart:
discovers the device dynamically via `/sys/class/rpmsg/rpmsg*/name` (the
number isn't fixed - depends on boot order), then sends a ping every 10s
and logs the reply.

Manual test from the target shell, without `backend_daemon` running (needs
a single read-write file descriptor - `cat`+`echo >` as two separate opens
gets `Device or resource busy`, this device doesn't support concurrent
opens):

```bash
exec 3<>/dev/rpmsg0
echo -n 'hello from A7' >&3
timeout 3 dd bs=256 count=1 <&3 2>/dev/null   # -> "ACK 1: hello from A7"
exec 3<&-
```

### The bug that made this take three rebuild cycles

M4-side logs showed complete success at every step (`rpmsg_create_ept`
returned 0, valid address, `support_ns=1`) and the mailbox kick fired -
yet Linux never created the channel, not even as an unbound device. Root
cause: `CONFIG_IPM_MAX_DATA_SIZE=0` on this board — STM32's IPCC mailbox is
a pure doorbell, it can signal an ID but carries no data payload. The
`mailbox_notify()` callback unconditionally called
`ipm_send(ipm_handle, 0, id, &id, 4)` (a 4-byte payload); the IPM driver
silently rejected it since it exceeds the 0-byte max, so the notify
returned "success" on the M4 side while Linux's virtio_rpmsg_bus never
actually got interrupted. Fix, matching Zephyr's own
`openamp_rsc_table` sample:

```c
#if CONFIG_IPM_MAX_DATA_SIZE > 0
	ipm_send(ipm_handle, 0, id, &id, 4);
#else
	ipm_send(ipm_handle, 0, id, NULL, 0);
#endif
```

Found by bisecting against the unmodified upstream sample (which worked)
rather than continuing to read OpenAMP source — see the GitHub wiki's
[M4-Firmware](https://github.com/CiprianTiro/stm32mp1-universal-controller/wiki/M4-Firmware)
page for the full debugging trail.
