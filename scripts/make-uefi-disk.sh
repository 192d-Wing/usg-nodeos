#!/usr/bin/env bash
# Build (or refresh) the node's UEFI boot+state disk:
#   GPT: vda1 = ESP (FAT, holds the EFI-stub kernel as EFI/BOOT/BOOTX64.EFI)
#        vda2 = LUKS-encrypted node state (initd formats it on first boot)
# On an existing disk only the ESP kernel is refreshed, so the encrypted state
# partition (and thus the enrolled identity) persists across kernel rebuilds.
set -euo pipefail

export PATH="$HOME/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"

img="${1:?usage: make-uefi-disk.sh <image> <bzImage> [size_mb]}"
bzimage="${2:?usage: make-uefi-disk.sh <image> <bzImage> [size_mb]}"
size_mb="${3:-512}"

[ -f "$bzimage" ] || { echo "missing kernel image: $bzimage" >&2; exit 1; }

refresh_esp() {
  local loop mnt
  loop="$(sudo losetup -fP --show "$img")"
  mnt="$(mktemp -d)"
  sudo mount "${loop}p1" "$mnt"
  sudo mkdir -p "$mnt/EFI/BOOT"
  sudo cp "$bzimage" "$mnt/EFI/BOOT/BOOTX64.EFI"
  sudo umount "$mnt"
  rmdir "$mnt"
  sudo losetup -d "$loop"
}

if [ ! -f "$img" ]; then
  echo "creating UEFI disk: $img (${size_mb}M)"
  truncate -s "${size_mb}M" "$img"
  sgdisk --zap-all "$img" >/dev/null
  sgdisk -n 1:0:+64M -t 1:ef00 -c 1:ESP "$img" >/dev/null
  sgdisk -n 2:0:0 -t 2:8309 -c 2:nodeos-state "$img" >/dev/null
  loop="$(sudo losetup -fP --show "$img")"
  sudo mkfs.vfat -F 32 "${loop}p1" >/dev/null
  sudo losetup -d "$loop"
  refresh_esp
  echo "UEFI disk created (ESP + blank LUKS state partition)"
else
  echo "refreshing kernel on existing UEFI disk: $img"
  refresh_esp
fi
