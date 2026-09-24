# Boot menu and kernel command line tweaks for a faster boot (issue #59).
#
# UBOOT_EXTLINUX_TIMEOUT: how long U-Boot's boot menu waits for someone to
# pick an entry over the serial console, in TENTHS of a second. ST's
# default 20 = 2 s on every single boot. 1 is the minimum that still boots
# on its own: U-Boot rounds it up to 1 s. NOT 0 -- because the menu has a
# "menu title" line, U-Boot treats it as interactive, and a timeout of 0
# then means "wait for input forever" (boot/pxe_utils.c + common/menu.c),
# i.e. a board that never boots without a serial cable.
UBOOT_EXTLINUX_TIMEOUT = "1"

# "quiet": the kernel no longer prints every boot message to the serial
# console. At 115200 baud each line costs milliseconds, and the kernel
# waits for them. Nothing is lost: `dmesg` and `journalctl -k` still have
# every line. (ST's own layer adds "loglevel=1 quiet" only when
# ST_DEBUG_TRACE = "0", which would also rebuild TF-A/OP-TEE/U-Boot with
# less logging -- more than we want.)
UBOOT_EXTLINUX_KERNEL_ARGS:append = " quiet"
