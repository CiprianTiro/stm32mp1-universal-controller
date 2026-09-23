# firmware_m4

Cortex-M4 firmware for the STM32MP157F-DK2, built on Zephyr RTOS (decision
recorded in GitHub issue #11 / superseded #17 — Zephyr from the start, not a
FreeRTOS-first staging plan). Loaded and controlled from the A7 side via
Linux's `remoteproc` framework; talks to `linux_a7/backend_daemon` over
RPMsg (issue #12).

The firmware is purely reactive (issue #14): no heartbeat or other
automatic activity. It waits for LED commands from the A7 over RPMsg and
drives LD7 accordingly -- see "RPMsg protocol" below.

## One-time workspace setup

The firmware builds against a separate, ordinary Zephyr workspace -- by
default `~/zephyrproject` -- rather than vendoring Zephyr into this repo
(it's a multi-GB checkout). `west.yml` in this directory pins the Zephyr
version this firmware is written against (**v4.4.2**); the workspace must be
at that same version.

```bash
python3.12 -m venv ~/zephyrproject/.venv      # west needs Python >= 3.12
source ~/zephyrproject/.venv/bin/activate
pip install west
west init -m https://github.com/zephyrproject-rtos/zephyr --mr v4.4.2 ~/zephyrproject
cd ~/zephyrproject && west update             # ~3-4 GB
pip install -r zephyr/scripts/requirements.txt
west sdk install --toolchains arm-zephyr-eabi
```

System packages needed first (Ubuntu/Debian): `git cmake ninja-build gperf
ccache dfu-util device-tree-compiler python3-venv`.

A workspace somewhere else works too: pass
`ZEPHYR_WORKSPACE=/path/to/it` to the make commands below.

Don't run `west init -l firmware_m4` inside this repo (an older version of
this README said to): it turns the repo root into a second west workspace,
which then shadows `~/zephyrproject` and breaks `make build-m4` with
`unknown command "build"` until its `zephyr/`/`modules/` are also
downloaded.

## Build and deploy

All from the repo root:

| Command | What it does |
|---|---|
| `make build-m4` | Builds only this firmware -> `firmware_m4/build/zephyr/zephyr.elf` |
| `make build-hw` | Builds **everything**: runs `build-m4` first, then the Yocto image, which packages that ELF (recipe `m4-firmware`) alongside the kernel and A7 apps |
| `make flash-hw` | Flashes the full image over USB (board in recovery boot mode) |
| `make flash-m4` | M4-only update on a running board: builds, copies the ELF over SSH, restarts the M4. No reflash, no reboot |

`make flash-m4` targets `stm32mp1.local`; if that name doesn't resolve on
your network use `make flash-m4 BOARD_HOST=<board IP>`. The copied firmware
survives reboots; the next `make flash-hw` replaces it with whatever that
image contains.

(`stm32mp157c_dk2` is Zephyr's upstream board target -- it matches our
DK2's M4 core, RAM/flash carveouts, and 4" MIPI-DSI touch panel exactly;
the A7-side security differences between the C/F SoC variants don't affect
the M4.)

### How it's started on the board

The image's `m4-firmware.service` (yocto_layers/meta-universal-controller/
recipes-core/m4-firmware/) runs at boot, before `backend-daemon`: it
loads `/lib/firmware/rproc-m4-fw` into the M4 and starts it via remoteproc.
By hand, as root on the board:

```bash
systemctl restart m4-firmware    # stop + start (e.g. after copying a new ELF)
cat /sys/class/remoteproc/remoteproc0/state          # "running" / "offline"
cat /sys/kernel/debug/remoteproc/remoteproc0/trace0  # M4 printk output (RAM console)
```

`stop` can take up to ~15s: the firmware doesn't ack the shutdown request
(no code here handles it), so remoteproc falls back to force-stopping via
the RCC reset line after a timeout -- `state` stays `running` until then,
that's expected, not stuck.

## RPMsg protocol (v0)

Endpoint channel name: `rpmsg-raw` — the specific name Linux's in-tree
`rpmsg_char` driver auto-binds to (`drivers/rpmsg/rpmsg_char.c`'s
`rpmsg_chrdev_id_table`), so a plain `/dev/rpmsgN` shows up for userspace
to open directly once the M4 announces it — no custom kernel module
needed, unlike ST's own `rpmsg-client-sample`/`rpmsg-tty` samples.

One text request per message, one text reply per request (RPMsg frames
each write as one message, so no delimiters are needed):

| Request (A7 -> M4) | Reply (M4 -> A7) |
|---|---|
| `LED ON` | `LED ON` |
| `LED OFF` | `LED OFF` |
| `LED STATUS` | `LED ON` or `LED OFF` (current state, unchanged) |
| anything else | `ERR: unknown command` |

The reply always states the LED's resulting state.

`linux_a7/backend_daemon/src/rpmsg.rs` is the A7-side counterpart:
discovers the device dynamically via `/sys/class/rpmsg/rpmsg*/name` (the
number isn't fixed - depends on boot order), opens it on the first command,
and reopens it (with one retry) if the M4 was restarted in between.

Manual test from the target shell, without `backend_daemon` running (needs
a single read-write file descriptor - `cat`+`echo >` as two separate opens
gets `Device or resource busy`, this device doesn't support concurrent
opens):

```bash
exec 3<>/dev/rpmsg0
echo -n 'LED ON' >&3
timeout 3 dd bs=256 count=1 <&3 2>/dev/null   # -> "LED ON"
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
