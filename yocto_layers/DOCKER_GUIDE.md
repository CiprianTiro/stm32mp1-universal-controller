# 🐳 Containerized Development Environment Setup

This project utilizes a containerized Yocto Project workspace wrapped inside a Docker sandbox. This configuration isolates cross-compilation toolchain dependencies, locks in specific package versions (`gcc-multilib`, `python-is-python3`, `qemu`), and guarantees identical compilation outputs across different development workstations or automated CI runners.

---

## 📋 Prerequisites

To run this build framework, your host Linux workstation needs native Docker and Docker Compose engines installed. Do not use the sandboxed Snap version, as it restricts file subsystem mount points.

Note: `docker-compose` (the old hyphenated standalone tool) has been deprecated
since 2023. `docker-compose-v2` is Ubuntu's own packaging of the modern
Compose V2 plugin - no extra apt repository needed - and gives you the
`docker compose` (space) command that every script in this repo now uses.

```bash
# 1. Update your local repository index
sudo apt-get update

# 2. Install the native Linux Docker system utilities
sudo apt-get install -y docker.io docker-compose-v2

# 3. Add your active user to the docker group to bypass typing 'sudo' for commands
sudo usermod -aG docker $USER
```
 ⚠️ **CRITICAL STEP:** After running the group assignment command, you must log out of your Ubuntu desktop session and log back in, or run `newgrp docker` in your current terminal window to apply the security permissions.

---

## 🚀 Manual Container Workflow

### 1. Spin Up the Background Container Engine
Navigate into the Yocto workspace subdirectory and initialize the background daemon layer. This command reads our configurations, sets up folder mounts, and builds the custom Ubuntu compilation image on its first run:

```bash
cd yocto_layers/
docker compose up -d --build
```

### 2. Enter the Running Container Shell
To step inside the active container sandbox portal and interact directly with its isolated tools run the following command or just attach shell straight from VSC editor:

```bash
docker compose exec yocto-builder /bin/bash
```

Your prompt will immediately switch to builder@stm32mp1-yocto-container:~/workspace$, showing you are executing commands inside the isolated sandbox.

### 3. Initialize the BitBake Build Environment (Inside Container)
Once inside the container shell, initialize the standard Yocto path layouts and environmental tracking states:
```bash
source poky/oe-init-build-env build
```

### 4. Exit the Container Portal
To close your active connection portal and drop back down to your host workstation terminal prompt without killing the active compilation background engine, execute:

```bash
exit
```

## ⚡ Automated Build Script Execution

 A master automation tracking script is provided at the repository root. This wrapper handles checking the container status, waking it up if it is sleeping, and passing the required compilation chains down into the sandbox in a single action.

```bash
# From the repository root:
cd yocto_layers/

# Grant the automation scripts execution permissions - required to be executed only once
chmod +x run_build.sh run_qemu.sh

# Automatically compile the custom Linux distribution for the physical hardware target
# (no argument = hardware; MACHINE=stm32mp1, build dir: yocto_layers/build/)
./run_build.sh

# Automatically compile the custom Linux distribution for the local QEMU emulator target
# (MACHINE=qemuarm64, build dir: yocto_layers/build-qemu/)
./run_build.sh qemu
```

Or equivalently, from the repository root: `make build-hw` / `make build-qemu` (see the top-level `Makefile`).

## 🚫 Troubleshooting

### Error: `open [...]/docker-compose.yml: permission denied`
If your terminal outputs an open file restriction error when triggering container commands from secondary hard drive configurations or external storage paths (e.g., `/mnt/Storage/...`), Ubuntu's default **Snap** security sandbox is blocking file operations.

**Resolution:**
Wipe the Snap implementation completely and install the native Linux Docker engines via standard packages:
```bash
sudo snap remove docker docker-compose
sudo apt-get update && sudo apt-get install -y docker.io docker-compose-v2
```

Before trying again to open the container, after re-installing the packages, a restart of the Host station is required.