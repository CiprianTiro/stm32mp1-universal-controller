# =============================================================================
# Universal Controller Platform Master Automation Matrix
# Target Architecture: STMicroelectronics STM32MP1 (Cortex-A7 + Cortex-M4)
# =============================================================================

.PHONY: help build-hw build-hw-prod build-qemu shell-yocto clean-yocto build-m4 build-a7 test-qemu verify-qemu flash-hw flash-hw-prod flash-m4 deploy-ui deploy-backend netboot restore-userfs audit

# Zephyr workspace used to build the M4 firmware: a normal `west init` +
# `west update` checkout at the Zephyr version pinned in firmware_m4/west.yml
# (see firmware_m4/README.md). Override per machine, e.g.
# `make build-m4 ZEPHYR_WORKSPACE=/opt/zephyrproject`.
ZEPHYR_WORKSPACE ?= $(HOME)/zephyrproject

# Where `make flash-m4` copies the firmware (SSH as root, key auth -- see the
# ssh-root-key recipe). stm32mp1.local resolves via mDNS; override with an IP
# if it doesn't on your network: `make flash-m4 BOARD_HOST=192.168.1.130`.
BOARD_HOST ?= stm32mp1.local

# Default target when just typing 'make'
help:
	@echo "============================================================================="
	@echo "🛠️  Universal Controller Master Build System"
	@echo "============================================================================="
	@echo "Available Execution Commands:"
	@echo "  make build-hw      - Build EVERYTHING for the DK2: M4 firmware + Yocto image (A7 apps included)"
	@echo "  make flash-hw      - Flash the built image to the DK2 over USB (keeps /usr/local; FULL_FLASH=1 wipes it)"
	@echo "  make build-hw-prod - Build the PRODUCTION image variant (no SSH at all)"
	@echo "  make flash-hw-prod - Flash the production image (the board then has no SSH)"
	@echo "  make restore-userfs - Put the newest /usr/local backup (taken by flash-hw) back on the board"
	@echo "  make flash-m4      - Update ONLY the M4 firmware on a running board over SSH (no reflash)"
	@echo "  make deploy-ui     - Rebuild ONLY the UI and put it on a running board over SSH (dev loop, no reflash)"
	@echo "  make deploy-backend - The same for backend_daemon"
	@echo "  make netboot       - One-shot TFTP/NFS netboot with the latest build, no eMMC changes (see wiki: Network-Boot)"
	@echo "  make build-qemu    - Build Yocto Linux simulation image for QEMU ARM64"
	@echo "  make test-qemu     - Automated headless boot smoke test (systemd/sshd/etc)"
	@echo "  make verify-qemu   - build-qemu + test-qemu in one shot: run BEFORE build-hw"
	@echo "  make shell-yocto   - Enter interactive terminal inside Yocto Docker sandbox"
	@echo "  make clean-yocto   - Wipe local Yocto build caches and configuration layouts"
	@echo "-----------------------------------------------------------------------------"
	@echo "  make build-m4      - Compile only the Cortex-M4 Zephyr firmware"
	@echo "  make build-a7      - Compile Cortex-A7 Linux native daemons (Rust/Cargo)"
	@echo "  make audit         - Check all Rust crates for known vulnerabilities + licenses (cargo audit/deny)"
	@echo "============================================================================="

# -----------------------------------------------------------------------------
# 1. LINUX OS DISTRIBUTION (YOCTO PROJECT & DOCKER SANDBOX)
# -----------------------------------------------------------------------------

# M4 first: the image's m4-firmware recipe packages the ELF build-m4 produces
# (firmware_m4/build/zephyr/zephyr.elf), since Zephyr's toolchain lives on the
# host, not in the Yocto container. The A7 side (kernel, backend_daemon,
# ui_layer) is all built by bitbake inside run_build.sh.
build-hw: build-m4
	@echo "🎬 Invoking Yocto hardware build pipeline..."
	@cd yocto_layers && ./run_build.sh

# The production variant (issue #37): same image without SSH (see
# universal-controller-image-prod.bb). Built next to the development one,
# in the same build directory, so switching between them is quick.
build-hw-prod: build-m4
	@echo "🎬 Invoking Yocto hardware build pipeline (production image)..."
	@cd yocto_layers && ./run_build.sh hardware universal-controller-image-prod

build-qemu:
	@echo "🎬 Invoking Yocto QEMU emulation build pipeline..."
	@cd yocto_layers && ./run_build.sh qemu

shell-yocto:
	@echo "🐳 Entering Yocto Docker workspace portal..."
	@cd yocto_layers && docker compose up -d
	@cd yocto_layers && docker compose exec yocto-builder /bin/bash

clean-yocto:
	@echo "⚠️  Wiping out Yocto generation paths..."
	rm -rf yocto_layers/build/ yocto_layers/build-qemu/

flash-hw:
	@echo "📲 Flashing production image to DK2 over USB DFU..."
	@cd yocto_layers && BOARD_HOST=$(BOARD_HOST) ./flash_hw.sh

# Without SSH on the production image, the automatic /usr/local backup
# can't be taken first (it's skipped with a note); /usr/local is kept anyway.
flash-hw-prod:
	@echo "📲 Flashing PRODUCTION image to DK2 over USB DFU..."
	@cd yocto_layers && IMAGE=universal-controller-image-prod BOARD_HOST=$(BOARD_HOST) ./flash_hw.sh

# Restores the board identity + device registry saved by flash-hw (#56).
restore-userfs:
	@cd yocto_layers && BOARD_HOST=$(BOARD_HOST) ./restore_userfs.sh

# Fast path for M4-only changes: copies the freshly built ELF over the one
# the image installed (/lib/firmware/rproc-m4-fw) and restarts the
# m4-firmware service, which stops the M4 and boots the new firmware. Survives
# reboots; a later flash-hw replaces it with whatever that image contains.
# Depends on build-m4, so `make flash-m4` alone builds and deploys.
# accept-new: after a reflash the board has fresh SSH host keys and flash_hw.sh
# clears the old entry, so the first connection must be allowed to add the new
# one. A key that CHANGED without that cleanup is still rejected.
# Fast dev loop (issue #38): one app rebuilt with bitbake and copied onto a
# running board over SSH, no reflash. See yocto_layers/deploy_app.sh.
deploy-ui:
	@cd yocto_layers && BOARD_HOST=$(BOARD_HOST) ./deploy_app.sh ui-layer

deploy-backend:
	@cd yocto_layers && BOARD_HOST=$(BOARD_HOST) ./deploy_app.sh backend-daemon

flash-m4: build-m4
	@echo "📲 Deploying M4 firmware to $(BOARD_HOST) over SSH..."
	@scp -q -o StrictHostKeyChecking=accept-new firmware_m4/build/zephyr/zephyr.elf root@$(BOARD_HOST):/lib/firmware/rproc-m4-fw
	@ssh -o StrictHostKeyChecking=accept-new root@$(BOARD_HOST) 'systemctl restart m4-firmware && cat /sys/class/remoteproc/remoteproc0/state'

netboot:
	@echo "🔄 Syncing latest build and triggering a one-shot TFTP/NFS netboot..."
	@cd yocto_layers && ./netboot.sh

test-qemu:
	@echo "🧪 Running automated QEMU boot smoke test..."
	@cd yocto_layers && ./test_qemu.sh

# Run this before every build-hw / hardware flash cycle: catches BSP and
# rootfs breakage in a headless boot test instead of on the DK2 board.
verify-qemu: build-qemu test-qemu

# -----------------------------------------------------------------------------
# 2. APPLICATION & MCU COMPONENT STACKS
# -----------------------------------------------------------------------------

# Runs west from inside the Zephyr workspace (west finds its workspace from
# the current directory), pointing it at this repo's app and build dir by
# absolute path ($(CURDIR) = repo root).
build-m4:
	@echo "⚡ Compiling Cortex-M4 Zephyr Firmware..."
	@cd $(ZEPHYR_WORKSPACE) && . .venv/bin/activate && \
		west build -p always -b stm32mp157c_dk2 $(CURDIR)/firmware_m4 -d $(CURDIR)/firmware_m4/build

build-a7:
	@echo "🦀 Compiling Cortex-A7 Application Daemons via Cargo Matrix..."
	@cd linux_a7/backend_daemon && cargo build --release
	@cd linux_a7/ui_layer && cargo build --release

# Dependency audit (issue #37), for every Rust program in the repo:
#   cargo audit -- any crate in Cargo.lock with a known security advisory
#                  (RustSec database, downloaded fresh each run)?
#   cargo deny  -- the same advisories, plus: only allowed licenses, crates
#                  only from crates.io, duplicate versions reported
#                  (policy: linux_a7/deny.toml).
# Fails on any finding. Run before every merge; the result at the time of
# #37 is on the wiki's Hardening page.
# (rust-hello, the toolchain smoke test, has no dependencies: nothing to audit.)
RUST_CRATES := linux_a7/backend_daemon linux_a7/ui_layer
audit:
	@for crate in $(RUST_CRATES); do \
		echo "🔍 $$crate"; \
		(cd $$crate && cargo audit && cargo deny --config $(CURDIR)/linux_a7/deny.toml check) || exit 1; \
	done
	@echo "✅ No known vulnerabilities, all licenses allowed."

# -----------------------------------------------------------------------------
# 3. KERNEL MANIPULATION PIPELINES
# -----------------------------------------------------------------------------

kernel-config:
	@echo "🎬 Spawning interactive devshell kernel config..."
	@cd yocto_layers && ./manage_kernel.sh menuconfig

kernel-save:
	@echo "🎬 Analyzing kernel deltas and exporting permanent layers..."
	@cd yocto_layers && ./manage_kernel.sh save

# -----------------------------------------------------------------------------
# 4. SIMULATION PIPELINES
# -----------------------------------------------------------------------------

run-qemu:
	@cd yocto_layers && ./run_qemu.sh