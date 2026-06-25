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
    #
    # ATO / CMVP: the aws-lc-rs FIPS module is validated against a SPECIFIC clang
    # version in its operating environment. Pin it for a reproducible, on-cert
    # build — do NOT build an ATO image with "whatever clang the distro ships":
    #   - NODEOS_FIPS_CC / NODEOS_FIPS_CXX : the exact clang binary (e.g.
    #     clang-19 / clang++-19) matching the CMVP-validated OE for the linked
    #     aws-lc-fips-sys version. Defaults to clang/clang++ for dev builds.
    #   - NODEOS_FIPS_CLANG_VERSION : when set, the build asserts the compiler's
    #     reported version matches and fails closed otherwise (reproducibility).
    # Confirm the validated clang against the aws-lc CMVP certificate and
    # usg-est-client docs/fips-compliance.md before an ATO.
    fips_cc="${NODEOS_FIPS_CC:-clang}"
    fips_cxx="${NODEOS_FIPS_CXX:-clang++}"
    if ! command -v "$fips_cc" >/dev/null 2>&1; then
      echo "error: FIPS build requires clang (aws-lc-fips delocate is clang-based);" >&2
      echo "       install '$fips_cc' (or set NODEOS_FIPS_CC) or build with NODEOS_FIPS=0." >&2
      exit 1
    fi
    fips_cc_version="$("$fips_cc" --version | head -1)"
    echo "FIPS aws-lc compiler: $fips_cc_version"
    # Match the pin against the parsed "clang version X.Y.Z" as a WHOLE version
    # (exact, or a `major[.minor]` prefix) — not a substring. A substring match
    # would let "21.1.8" accept "21.1.80" and "2" accept "21.x", defeating the
    # fail-closed intent.
    fips_cc_actual="$(printf '%s' "$fips_cc_version" |
      sed -n 's/.*clang version \([0-9][0-9.]*\).*/\1/p')"
    if [ -n "${NODEOS_FIPS_CLANG_VERSION:-}" ]; then
      case "$fips_cc_actual" in
        "$NODEOS_FIPS_CLANG_VERSION" | "$NODEOS_FIPS_CLANG_VERSION".*) ;;
        *)
          echo "error: NODEOS_FIPS_CLANG_VERSION pinned to ${NODEOS_FIPS_CLANG_VERSION}," >&2
          echo "       but '$fips_cc' reports ${fips_cc_actual:-<unparsed>} ($fips_cc_version)." >&2
          echo "       Install the CMVP-validated clang or unset the pin to override." >&2
          exit 1
          ;;
      esac
    fi
    clang_cross="--target=$triple --sysroot=$sysroot --gcc-toolchain=$host_dir"
    export CC_x86_64_unknown_linux_gnu="$fips_cc"
    export CXX_x86_64_unknown_linux_gnu="$fips_cxx"
    export CFLAGS_x86_64_unknown_linux_gnu="$clang_cross"
    export CXXFLAGS_x86_64_unknown_linux_gnu="$clang_cross"
    echo "cross-building Rust with $fips_cc for the FIPS aws-lc module" \
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
