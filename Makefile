# =============================================================================
# Universal Controller Platform Master Automation Matrix
# Target Architecture: STMicroelectronics STM32MP1 (Cortex-A7 + Cortex-M4)
# =============================================================================

.PHONY: help build-hw build-qemu shell-yocto clean-yocto build-m4 build-a7

# Default target when just typing 'make'
help:
	@echo "============================================================================="
	@echo "🛠️  Universal Controller Master Build System"
	@echo "============================================================================="
	@echo "Available Execution Commands:"
	@echo "  make build-hw      - Build Yocto Linux production image for STM32MP157F-DK2"
#	@echo "  make build-qemu    - Build Yocto Linux simulation image for QEMU ARM64"
	@echo "  make shell-yocto   - Enter interactive terminal inside Yocto Docker sandbox"
	@echo "  make clean-yocto   - Wipe local Yocto build caches and configuration layouts"
	@echo "-----------------------------------------------------------------------------"
	@echo "  make build-m4      - Compile Cortex-M4 MCU firmware (Bare-metal/RTOS C)"
	@echo "  make build-a7      - Compile Cortex-A7 Linux native daemons (Rust/Cargo)"
	@echo "============================================================================="

# -----------------------------------------------------------------------------
# 1. LINUX OS DISTRIBUTION (YOCTO PROJECT & DOCKER SANDBOX)
# -----------------------------------------------------------------------------

build-hw:
	@echo "🎬 Invoking Yocto hardware build pipeline..."
	@cd yocto_layers && ./run_build.sh

#build-qemu:
#	@echo "🎬 Invoking Yocto QEMU emulation build pipeline..."
#	@cd yocto_layers && ./run_build.sh qemu

shell-yocto:
	@echo "🐳 Entering Yocto Docker workspace portal..."
	@cd yocto_layers && docker-compose up -d
	@cd yocto_layers && docker-compose exec yocto-builder /bin/bash

clean-yocto:
	@echo "⚠️  Wiping out Yocto generation paths..."
	rm -rf yocto_layers/build/ yocto_layers/build-qemu/

# -----------------------------------------------------------------------------
# 2. APPLICATION & MCU COMPONENT STACKS
# -----------------------------------------------------------------------------

build-m4:
	@echo "⚡ Compiling Cortex-M4 Embedded Firmware Stack..."
	# Placeholder for your toolchain call (e.g., stm32cubeide or arm-none-eabi-gcc)
	@cd firmware_m4 && echo "Triggering MCU toolchain compilation wrappers here..."

build-a7:
	@echo "🦀 Compiling Cortex-A7 Application Daemons via Cargo Matrix..."
	@cd linux_a7/backend_daemon && cargo build --release
	@cd linux_a7/ui_layer && cargo build --release

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

#run-qemu:
#	@cd yocto_layers && ./run_qemu.sh