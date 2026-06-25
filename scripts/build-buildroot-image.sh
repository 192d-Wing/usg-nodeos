#!/usr/bin/env bash
set -euo pipefail

export PATH="$HOME/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Load deployment-specific lab values (EST server host/IP, node FQDN, bootstrap
# token) from a gitignored .env. post-build.sh substitutes these into the image
# config, replacing the documentation placeholders in the committed overlay.
if [ -f "$repo_root/.env" ]; then
  set -a
  # shellcheck disable=SC1091
  . "$repo_root/.env"
  set +a
fi

# Workload profile selects the image variant from one shared tree: k8s
# (Kubernetes node) or kvm (bare-metal KVM/libvirt hypervisor). It picks the
# rootfs overlay and — when they diverge — the defconfig and kernel config. The
# hardened base (boot, identity, mTLS API) is identical across profiles.
profile="${NODEOS_PROFILE:-k8s}"
case "$profile" in
  k8s | kvm) ;;
  *)
    echo "error: NODEOS_PROFILE must be 'k8s' or 'kvm' (got '$profile')" >&2
    exit 1
    ;;
esac

build_base="${NODEOS_BUILD_BASE:-$HOME/.cache/nodeos-buildroot}"
buildroot_dir="${BUILDROOT_DIR:-$build_base/source}"
# Per-profile output tree: buildroot's target dir is incremental and does NOT
# remove files from packages you de-select, so building two profiles into one
# tree leaks (e.g.) the k8s runtime into the kvm image. Isolate them. The source
# clone + download cache are shared; only the build/target/images are per-profile.
output_dir="${BUILDROOT_OUTPUT_DIR:-$build_base/output-$profile}"
dl_dir="${BUILDROOT_DL_DIR:-$build_base/dl}"
host_bin="$build_base/host-bin"

# Profiles share ONE base defconfig + ONE base kernel config; a profile diverges
# only by an optional *fragment* merged on top (no full-file copies, so shared
# settings cannot drift). The rootfs overlay is shared base + profile overlay.
br="$repo_root/build/buildroot"
defconfig="${DEFCONFIG:-$br/qemu-x86_64.defconfig}"
kernel_config="$br/qemu-x86_64-linux.config"
overlays="$br/overlay-base $br/overlay-$profile"
# Optional per-profile fragments (appended/merged only when present).
defconfig_fragment="$br/qemu-x86_64-$profile.fragment"          # BR2_* deltas
kernel_fragment="$br/qemu-x86_64-linux-$profile.fragment"       # CONFIG_* deltas
# BR2_EXTERNAL tree carrying NodeOS custom packages (e.g. the k8s kubelet/CNI
# binaries). Always passed; packages are enabled only by a profile's fragment.
br2_external="$br/external"
generated_defconfig="$build_base/nodeos.defconfig"

echo "NodeOS build profile: $profile"
echo "  defconfig:     $defconfig"
echo "  kernel config: $kernel_config"
echo "  overlays:      $overlays"
[ -f "$defconfig_fragment" ] && echo "  defconfig frag: $defconfig_fragment"
[ -f "$kernel_fragment" ] && echo "  kernel frag:   $kernel_fragment"
[ -d "$br2_external" ] && echo "  br2_external:  $br2_external"

mkdir -p "$host_bin" "$output_dir" "$dl_dir"
if [ -x /usr/bin/gnuinstall ]; then
  ln -sf /usr/bin/gnuinstall "$host_bin/install"
fi

export PATH="$host_bin:$PATH"

if [ ! -d "$buildroot_dir" ]; then
  cat >&2 <<EOF
Buildroot is not present at:
  $buildroot_dir

Clone it first, for example:
  mkdir -p "$build_base"
  git clone --depth 1 https://github.com/buildroot/buildroot.git "$buildroot_dir"
EOF
  exit 1
fi

"$repo_root/scripts/check-wsl-prereqs.sh"

# The defconfig pins a custom kernel version. Buildroot normally exempts the
# kernel from hash checking (BR_NO_CHECK_HASH_FOR in linux/linux.mk), but
# BR2_DOWNLOAD_FORCE_CHECK_HASHES overrides that and demands a hash — and ships
# none for a custom version, so a clean build fails ("No hash found for
# linux-<ver>.tar.xz"). Register the NodeOS-pinned hash (the linux package's hash
# file is linux/linux.hash and does not exist by default, so create it). Keep
# hash-checking ON for reproducibility. Keep $nodeos_kernel_version in sync with
# BR2_LINUX_KERNEL_CUSTOM_VERSION_VALUE in the base defconfig.
# NOTE (ATO): cross-check this sha256 against kernel.org's published hash.
nodeos_kernel_version="7.1"
nodeos_kernel_sha256="691f44797fbe790dc8a321604c927087526ad27b6d649925d60f8eed0a2564a0"
linux_hash_file="$buildroot_dir/linux/linux.hash"
if ! grep -qs "linux-$nodeos_kernel_version.tar.xz" "$linux_hash_file"; then
  echo "registering NodeOS-pinned hash for linux-$nodeos_kernel_version.tar.xz"
  {
    echo "# NodeOS-pinned custom kernel (registered by build-buildroot-image.sh)"
    echo "sha256  $nodeos_kernel_sha256  linux-$nodeos_kernel_version.tar.xz"
  } >> "$linux_hash_file"
fi

sed \
  -e "s|^BR2_ROOTFS_OVERLAY=.*|BR2_ROOTFS_OVERLAY=\"$overlays\"|" \
  -e "s|^BR2_ROOTFS_POST_BUILD_SCRIPT=.*|BR2_ROOTFS_POST_BUILD_SCRIPT=\"$repo_root/build/buildroot/post-build.sh\"|" \
  -e "s|^BR2_LINUX_KERNEL_CUSTOM_CONFIG_FILE=.*|BR2_LINUX_KERNEL_CUSTOM_CONFIG_FILE=\"$kernel_config\"|" \
  "$defconfig" > "$generated_defconfig"

if ! grep -q '^BR2_DL_DIR=' "$generated_defconfig"; then
  echo "BR2_DL_DIR=\"$dl_dir\"" >> "$generated_defconfig"
fi

# Merge the profile's defconfig fragment (BR2_* deltas) — appended last so its
# KEY=value lines win over the base. Point the kernel at the profile's config
# fragment (buildroot merges it onto the base linux.config).
if [ -f "$defconfig_fragment" ]; then
  printf '\n# --- %s profile fragment ---\n' "$profile" >> "$generated_defconfig"
  cat "$defconfig_fragment" >> "$generated_defconfig"
fi
if [ -f "$kernel_fragment" ]; then
  echo "BR2_LINUX_KERNEL_CONFIG_FRAGMENT_FILES=\"$kernel_fragment\"" >> "$generated_defconfig"
fi

# BR2_EXTERNAL lets buildroot find NodeOS custom packages (kubelet/CNI for k8s).
make_args=(BR2_DEFCONFIG="$generated_defconfig")
[ -d "$br2_external" ] && make_args+=(BR2_EXTERNAL="$br2_external")

make -C "$buildroot_dir" O="$output_dir" "${make_args[@]}" defconfig

# Build the cross toolchain first, then cross-compile the Rust binaries against
# the *target* glibc so they run on the rootfs regardless of the build host's
# (possibly newer) glibc.
make -C "$output_dir" toolchain

cross_gcc="$output_dir/host/bin/x86_64-buildroot-linux-gnu-gcc"
if [ ! -x "$cross_gcc" ]; then
  echo "buildroot cross gcc not found at $cross_gcc" >&2
  exit 1
fi
export NODEOS_CROSS_GCC="$cross_gcc"
"$repo_root/scripts/build-rust-linux.sh"

export NODEOS_INITD="$repo_root/target/release/initd"
export NODEOS_NODED="$repo_root/target/release/noded"
# post-build.sh runs profile-specific finalization checks.
export NODEOS_PROFILE="$profile"

# Force target-finalize so the overlay copy + post-build (which inject the .env
# values into the image config) always re-run. Buildroot otherwise skips finalize
# on a config-only rebuild (no package changed), leaving a stale token/host baked
# into the image. target-finalize is phony, so this re-applies every build.
make -C "$output_dir" target-finalize
make -C "$output_dir"

echo "Buildroot output:"
echo "  $output_dir/images"
