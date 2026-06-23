#!/usr/bin/env bash
set -euo pipefail

export PATH="$HOME/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

cd "$repo_root"

# The buildroot rootfs is glibc. Build glibc binaries (FIPS on by default;
# NODEOS_FIPS=0 to disable).
#
# IMPORTANT: a dynamically linked glibc binary only runs on the target if the
# build host's glibc is <= the target rootfs glibc. If this host's glibc is
# newer than buildroot's (check `ldd --version` vs the buildroot glibc), build
# with the buildroot cross-toolchain instead by setting NODEOS_CC / linker to
# `$output/host/bin/x86_64-buildroot-linux-gnu-gcc`, or build in a CI image whose
# glibc matches the target.
fips="${NODEOS_FIPS:-1}"

feature_args=()
if [ "$fips" != "1" ]; then
  feature_args=(--no-default-features)
fi

# Cross-build against the buildroot glibc toolchain when NODEOS_CROSS_GCC is set
# (host target triple, but the buildroot compiler/linker so the binary — and the
# aws-lc FIPS C code — link against the *target* glibc). This is required when the
# build host's glibc is newer than the rootfs glibc.
if [ -n "${NODEOS_CROSS_GCC:-}" ]; then
  cross_gcc="$NODEOS_CROSS_GCC"
  cross_prefix="${cross_gcc%-gcc}"
  export CC_x86_64_unknown_linux_gnu="$cross_gcc"
  export CXX_x86_64_unknown_linux_gnu="${cross_prefix}-g++"
  export AR_x86_64_unknown_linux_gnu="${cross_prefix}-ar"
  export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$cross_gcc"
  echo "cross-building Rust with $cross_gcc"
fi

cargo build --workspace --release "${feature_args[@]}"

echo "built (glibc, fips=$fips):"
echo "  $repo_root/target/release/initd"
echo "  $repo_root/target/release/noded"
