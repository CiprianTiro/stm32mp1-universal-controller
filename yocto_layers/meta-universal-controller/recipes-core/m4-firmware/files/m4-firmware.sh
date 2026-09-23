#!/bin/sh
# m4-firmware.sh -- starts/stops the Cortex-M4 firmware through the Linux
# remoteproc framework. Run at boot by m4-firmware.service (see that file).
#
# remoteproc is the kernel's interface for managing the "other" cores on a
# chip. Everything is plain text files in sysfs:
#   .../remoteproc0/firmware  -- file NAME to load, looked up in /lib/firmware
#   .../remoteproc0/state     -- read: "offline"/"running"; write: "start"/"stop"
#
# Replaces ST's own st-m4firmware-load-default.sh, which is hardcoded to
# ST's demo firmware (OpenAMP_TTY_echo.elf) and can't load ours.

RPROC=/sys/class/remoteproc/remoteproc0
FW_NAME=rproc-m4-fw

# The remoteproc driver can be a kernel module that udev loads early in
# boot, so remoteproc0 may not exist the instant this runs. Wait up to ~10 s
# for it instead of failing on a race. If it never appears, the device tree
# in use doesn't enable the M4 (see the M4-Firmware wiki page: the extlinux
# default label must be stm32mp157f-dk2-m4-examples).
wait_for_rproc() {
  i=0
  while [ ! -e "$RPROC/state" ]; do
    i=$((i + 1))
    if [ "$i" -gt 20 ]; then
      echo "m4-firmware: $RPROC never appeared -- M4 not enabled in device tree?" >&2
      exit 1
    fi
    sleep 0.5
  done
}

case "$1" in
  start)
    wait_for_rproc
    # Already running (e.g. the service was restarted by hand): nothing to
    # do. Writing "start" to a running core is an error in remoteproc.
    if [ "$(cat "$RPROC/state")" = "running" ]; then
      exit 0
    fi
    echo -n "$FW_NAME" > "$RPROC/firmware" || exit 1
    echo start > "$RPROC/state" || exit 1
    echo "m4-firmware: started $FW_NAME"
    ;;
  stop)
    wait_for_rproc
    if [ "$(cat "$RPROC/state")" = "running" ]; then
      echo stop > "$RPROC/state"
    fi
    ;;
  *)
    echo "usage: $0 start|stop" >&2
    exit 2
    ;;
esac
