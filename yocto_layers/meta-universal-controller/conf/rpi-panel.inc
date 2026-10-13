# rpi-panel.inc -- the Raspberry Pi wall panel's build settings (issue
# #107). Pulled into build-rpi/conf/local.conf by run_build.sh (mode
# "rpi"); the DK2's build never reads it.
#
# The panel: a Raspberry Pi 3B+ with the official 7" DSI touchscreen
# (800x480), running ui_layer in panel mode (#106) and nothing else.
#
# 32-bit (MACHINE raspberrypi3), not 64: the same ARMv7 Rust target as
# the DK2's A7 (ui-layer.bb builds for armv7-unknown-linux-gnueabihf), and
# less memory per process on the Pi's 1 GB. The Cortex-A53 runs ARMv7 code
# natively.

INIT_MANAGER = "systemd"
# No dev shortcuts (blank root password...): SSH with the maintainer's key
# only, as on the DK2 (ssh-root-key, openssh bbappend).
EXTRA_IMAGE_FEATURES = ""
VIRTUAL-RUNTIME_base-utils-syslog = ""

# The WiFi chip's firmware (Cypress/Synaptics, meta-raspberrypi's
# linux-firmware-rpidistro) carries this licence flag.
LICENSE_FLAGS_ACCEPTED += "synaptics-killswitch"

# The DK2-only parts of meta-universal-controller append to ST's recipes
# (U-Boot, the ST kernel, extlinux): not in this build.
BBMASK += " \
    meta-universal-controller/recipes-bsp/ \
    meta-universal-controller/recipes-kernel/ \
    meta-universal-controller/recipes-core/m4-firmware/ \
"

# One SD card image: boot (FAT, config.txt) + root (ext4).
IMAGE_FSTYPES = "wic.bz2 wic.bmap"

# ---- config.txt (meta-raspberrypi's rpi-config) -------------------------
# The display: KMS (vc4) with the official 7" DSI panel and its touch
# controller (one overlay for both).
VC4DTBO = "vc4-kms-v3d"
DISABLE_SPLASH = "1"
DISABLE_RPI_BOOT_LOGO = "1"

# HEAT (the panel runs hot with its display): the UI needs far less than
# the A53's 1.4 GHz. Capped at 1 GHz; at rest the governor (panel-tuning)
# drops it to arm_freq_min. Measured before/after: see the Wall-Panel
# wiki page.
ARM_FREQ = "1000"
# KMS takes its graphics memory from the kernel (CMA): the firmware's own
# share can be the minimum. No 3D: the UI draws in software.
GPU_MEM = "64"

# Added at the end of config.txt (printf'ed: \n = a new line; written on
# one line so no line gets a leading space):
#   dtoverlay=vc4-kms-dsi-7inch,invx,invy
#                                the official 7" DSI display + its touch,
#                                the touch flipped (invx,invy) to match
#                                the UI rotated 180 degrees (ui-layer's
#                                10-panel.conf)
#   arm_freq_min=600             the slowest clock, at rest (the governor
#                                moves between this and ARM_FREQ)
#   dtoverlay=disable-bt         Bluetooth off until the speaker (#110):
#                                one radio less, a bit less heat
RPI_EXTRA_CONFIG = "\n# Wall panel (issue #107)\ndtoverlay=vc4-kms-dsi-7inch,invx,invy\narm_freq_min=600\ndtoverlay=disable-bt"
