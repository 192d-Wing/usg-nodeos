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
  cross_prefix="${cross_gcc%-gcc}"                  # .../bin/<triple>
  triple="$(basename "$cross_prefix")"              # e.g. x86_64-buildroot-linux-gnu
  host_dir="$(dirname "$(dirname "$cross_gcc")")"   # buildroot host/ (parent of bin/)
  sysroot="$host_dir/$triple/sysroot"

  # The buildroot gcc stays the LINKER and archiver, so the final binary links
  # against the *target* glibc/libgcc (hermetic, host-glibc-independent).
  export AR_x86_64_unknown_linux_gnu="${cross_prefix}-ar"
  export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$cross_gcc"

  if [ "$fips" = "1" ]; then
    # aws-lc's FIPS module is built and CMVP-validated with clang; its delocate /
    # inject-hash integrity tooling rejects GCC codegen (GCC >= 15 emits
    # `.data.rel.ro.local`, which delocate flags as a forbidden .data section).
    # Compile the C/asm with clang, cross-targeted at the buildroot sysroot so we
    # stay on the validated compiler AND keep the hermetic target glibc.
    if ! command -v clang >/dev/null 2>&1; then
      echo "error: FIPS build requires clang (aws-lc-fips delocate is clang-based);" >&2
      echo "       install clang or build with NODEOS_FIPS=0." >&2
      exit 1
    fi
    clang_cross="--target=$triple --sysroot=$sysroot --gcc-toolchain=$host_dir"
    export CC_x86_64_unknown_linux_gnu="clang"
    export CXX_x86_64_unknown_linux_gnu="clang++"
    export CFLAGS_x86_64_unknown_linux_gnu="$clang_cross"
    export CXXFLAGS_x86_64_unknown_linux_gnu="$clang_cross"
    echo "cross-building Rust with clang for the FIPS aws-lc module" \
         "(sysroot=$sysroot), linking with $cross_gcc"
  else
    export CC_x86_64_unknown_linux_gnu="$cross_gcc"
    export CXX_x86_64_unknown_linux_gnu="${cross_prefix}-g++"
    echo "cross-building Rust with $cross_gcc"
  fi
fi

cargo build --workspace --release "${feature_args[@]}"

echo "built (glibc, fips=$fips):"
echo "  $repo_root/target/release/initd"
echo "  $repo_root/target/release/noded"
