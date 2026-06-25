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
output_dir="${BUILDROOT_OUTPUT_DIR:-$build_base/output}"
dl_dir="${BUILDROOT_DL_DIR:-$build_base/dl}"
host_bin="$build_base/host-bin"

# Resolve a profile-specific build input, falling back to the shared base file
# when no per-profile variant exists yet. This keeps identical files shared (no
# drift) while letting a profile diverge by simply adding its own copy.
br="$repo_root/build/buildroot"
pick_profile_file() {
  # $1 = base path (no profile), $2 = profile-suffixed path
  if [ -e "$2" ]; then printf '%s' "$2"; else printf '%s' "$1"; fi
}
defconfig="${DEFCONFIG:-$(pick_profile_file \
  "$br/qemu-x86_64.defconfig" "$br/qemu-x86_64-$profile.defconfig")}"
kernel_config="$(pick_profile_file \
  "$br/qemu-x86_64-linux.config" "$br/qemu-x86_64-linux-$profile.config")"
# The shared base overlay is always applied first; the profile overlay layers on
# top (buildroot accepts a space-separated BR2_ROOTFS_OVERLAY list).
overlays="$br/overlay-base $br/overlay-$profile"
generated_defconfig="$build_base/nodeos.defconfig"

echo "NodeOS build profile: $profile"
echo "  defconfig:     $defconfig"
echo "  kernel config: $kernel_config"
echo "  overlays:      $overlays"

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

sed \
  -e "s|^BR2_ROOTFS_OVERLAY=.*|BR2_ROOTFS_OVERLAY=\"$overlays\"|" \
  -e "s|^BR2_ROOTFS_POST_BUILD_SCRIPT=.*|BR2_ROOTFS_POST_BUILD_SCRIPT=\"$repo_root/build/buildroot/post-build.sh\"|" \
  -e "s|^BR2_LINUX_KERNEL_CUSTOM_CONFIG_FILE=.*|BR2_LINUX_KERNEL_CUSTOM_CONFIG_FILE=\"$kernel_config\"|" \
  "$defconfig" > "$generated_defconfig"

if ! grep -q '^BR2_DL_DIR=' "$generated_defconfig"; then
  echo "BR2_DL_DIR=\"$dl_dir\"" >> "$generated_defconfig"
fi

make -C "$buildroot_dir" O="$output_dir" BR2_DEFCONFIG="$generated_defconfig" defconfig

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
