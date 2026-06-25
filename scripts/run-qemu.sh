#!/usr/bin/env bash
set -euo pipefail

export PATH="/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
build_base="${NODEOS_BUILD_BASE:-$HOME/.cache/nodeos-buildroot}"
images_dir="${IMAGES_DIR:-$build_base/output/images}"
kernel="$images_dir/bzImage"
initramfs="$images_dir/rootfs.cpio"

if [ ! -f "$kernel" ]; then
  echo "missing kernel: $kernel" >&2
  exit 1
fi

if [ ! -f "$initramfs" ]; then
  echo "missing initramfs: $initramfs" >&2
  exit 1
fi

# Networking is opt-in (NODEOS_NET=1) to keep the default boot offline. When on,
# attach an IPv6 user-mode NIC and let the kernel autoconfigure it via SLAAC
# (auto6) so the IPv6-only node can reach the EST server. Reaching an external
# IPv6 host additionally requires IPv6 egress on the build host (e.g. WSL2
# mirrored networking).
net_args=()
append="console=ttyS0 panic=-1 ipv6.disable=0 ip=off rdinit=/usr/bin/initd"
if [ "${NODEOS_NET:-0}" = "1" ]; then
  # IPv6 user-mode NIC; initd brings eth0 up and the kernel does SLAAC from
  # slirp's router advertisements (no kernel ip= autoconfig needed).
  net_args=(-netdev "user,id=n0,ipv6=on" -device "virtio-net-pci,netdev=n0")
  if [ -n "${NODEOS_PCAP:-}" ]; then
    net_args+=(-object "filter-dump,id=d0,netdev=n0,file=${NODEOS_PCAP}")
  fi
fi

# UEFI boot+state disk (GPT: ESP with the EFI-stub kernel + LUKS state on vda2).
# OVMF measures the kernel it loads into the TPM PCRs (measured boot). The disk
# is built once and only its ESP kernel is refreshed on rebuilds, so the
# encrypted state partition persists. Delete it (and the tpm state) to re-enroll.
disk_img="${NODEOS_DISK:-$build_base/nodeos-uefi.img}"
bash "$repo_root/scripts/make-uefi-disk.sh" "$disk_img" "$kernel"

# OVMF firmware: read-only CODE + a writable per-node VARS copy.
ovmf_code="${NODEOS_OVMF_CODE:-/usr/share/OVMF/OVMF_CODE_4M.fd}"
ovmf_vars="$build_base/OVMF_VARS.fd"
[ -f "$ovmf_vars" ] || cp "${NODEOS_OVMF_VARS:-/usr/share/OVMF/OVMF_VARS_4M.fd}" "$ovmf_vars"

# Virtual TPM 2.0 (swtpm) as a TPM-CRB device (-> /dev/tpm0). State persists, so
# the sealed LUKS key / NV indices survive reboots. (Set NODEOS_TPM=0 to skip.)
tpm_args=()
if [ "${NODEOS_TPM:-1}" = "1" ]; then
  command -v swtpm >/dev/null || { echo "swtpm not installed (apt install swtpm)" >&2; exit 1; }
  tpm_state="${NODEOS_TPM_STATE:-$build_base/tpmstate}"
  tpm_sock="$tpm_state/swtpm-sock"
  mkdir -p "$tpm_state"
  pkill -f "swtpm socket.*$tpm_sock" 2>/dev/null || true
  sleep 0.3
  swtpm socket --tpm2 --tpmstate "dir=$tpm_state" \
    --ctrl "type=unixio,path=$tpm_sock" --log level=0 --daemon
  tpm_args=(-chardev "socket,id=chrtpm,path=$tpm_sock" \
    -tpmdev "emulator,id=tpm0,chardev=chrtpm" \
    -device "tpm-crb,tpmdev=tpm0")
fi

# UEFI boot: no -kernel/-initrd/-append; OVMF loads EFI/BOOT/BOOTX64.EFI from the
# ESP and the kernel uses its embedded cmdline (CONFIG_CMDLINE).
exec qemu-system-x86_64 \
  -machine q35 \
  -m 1024 \
  -nographic \
  -drive "if=pflash,format=raw,unit=0,readonly=on,file=$ovmf_code" \
  -drive "if=pflash,format=raw,unit=1,file=$ovmf_vars" \
  -drive "file=$disk_img,if=virtio,format=raw" \
  "${tpm_args[@]}" \
  "${net_args[@]}"
