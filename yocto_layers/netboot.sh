#!/usr/bin/env bash
# =============================================================================
# Syncs the latest hardware build output into /srv/tftp + /srv/nfs, then
# triggers a one-shot netboot (see wiki: Network-Boot). Run after every
# `make build-hw` you want the board to actually pick up -- nothing does
# this automatically, bitbake has no idea these directories exist.
# =============================================================================
set -euo pipefail

DEPLOY_DIR="$(dirname "$0")/build/tmp/deploy/images/stm32mp1"
TFTP_DIR="/srv/tftp"
NFS_DIR="/srv/nfs/stm32mp1-rootfs"

BOOTFS_TAR="${DEPLOY_DIR}/universal-controller-image-stm32mp1.splitted-bootfs.tar.xz"
ROOTFS_TAR="${DEPLOY_DIR}/universal-controller-image-stm32mp1.rootfs.tar.xz"

for f in "${BOOTFS_TAR}" "${ROOTFS_TAR}"; do
    if [ ! -f "${f}" ]; then
        echo "Error: ${f} not found. Run 'make build-hw' first."
        exit 1
    fi
done

echo "  -> Extracting uImage + device tree into ${TFTP_DIR}"
tar -xJf "${BOOTFS_TAR}" -C "${TFTP_DIR}" ./uImage ./uImage-* ./stm32mp157f-dk2.dtb 2>/dev/null || \
    tar -xJf "${BOOTFS_TAR}" -C "${TFTP_DIR}" --wildcards ./uImage ./uImage-* ./stm32mp157f-dk2.dtb

echo "  -> Syncing rootfs into ${NFS_DIR} (this deletes anything in there not from the build)"
# The rootfs tarball has real root-owned content (mode 700 /root/.ssh, mode
# 000 systemd credstore, etc.) -- both the extraction and the rsync into
# /srv/nfs need root to reproduce that ownership, so this needs sudo.
#
# --checksum is NOT optional here: Yocto's reproducible builds stamp every
# file with the same fixed mtime (SOURCE_DATE_EPOCH) across every rebuild,
# and a recompiled binary can easily land at the exact same byte size too.
# rsync's default quick-check only compares mtime+size, so without
# --checksum it silently skips re-copying a changed file whose mtime/size
# happen to match the stale copy already sitting in NFS_DIR -- which is
# exactly what was happening: every sync "succeeded" while quietly copying
# nothing, forever, no matter how many times the image was rebuilt.
TMP_EXTRACT="$(mktemp -d)"
trap 'sudo rm -rf "${TMP_EXTRACT}"' EXIT
sudo tar -xJf "${ROOTFS_TAR}" -C "${TMP_EXTRACT}" --numeric-owner
# --exclude on the SSH host keys, on purpose: each fresh tarball extraction
# ships brand-new host keys (they're generated once at image-build time,
# baked into the tarball like any other /etc file), so a plain sync would
# give the board a new SSH identity -- and therefore a new
# "REMOTE HOST IDENTIFICATION HAS CHANGED" warning -- on every single
# rebuild. Excluding them here means whatever keys are already sitting in
# NFS_DIR (from the very first sync) just stay put: rsync's --delete does
# NOT remove excluded destination files by default, so this is both "don't
# overwrite" and "don't delete" in one flag. The board's SSH identity is
# then stable across every future rebuild+resync, and ssh-keygen -R is only
# ever needed once, the first time you switch to netboot at all.
sudo rsync -a --delete --numeric-ids --checksum --exclude 'etc/ssh/ssh_host_*' "${TMP_EXTRACT}/" "${NFS_DIR}/"

# Trigger a one-shot netboot with the freshly synced files. This is RAM-only
# on the board (see apply_netboot_env.sh) -- bootcmd is never touched and
# nothing is saved to eMMC, so the board always falls back to normal eMMC
# boot on its own once you're done, and a broken netboot never requires a
# `make flash-hw` to recover.
"$(dirname "$0")/apply_netboot_env.sh"

echo "  -> Done."
