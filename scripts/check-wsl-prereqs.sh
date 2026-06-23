#!/usr/bin/env bash
set -euo pipefail

export PATH="$HOME/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"

missing=0

for tool in \
  bash \
  bc \
  bison \
  cargo \
  clang \
  cmake \
  cpio \
  file \
  flex \
  gcc \
  git \
  go \
  gzip \
  make \
  patch \
  perl \
  python3 \
  qemu-system-x86_64 \
  rsync \
  rustc \
  tar \
  unzip \
  wget
do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "missing: $tool"
    missing=1
  fi
done

if [ "$missing" -ne 0 ]; then
  cat <<EOF

Install the WSL build prerequisites with:

  sudo apt update
  sudo apt install -y \\
    bc bison build-essential clang cmake cpio file flex git golang-go gzip \\
    libclang-dev libncurses-dev make patch perl python3 \\
    qemu-system-x86 rsync rustc cargo tar unzip wget

clang/cmake/go are required to build the aws-lc-rs FIPS module for the default
'fips' feature.

NOTE: the rootfs is glibc; a glibc binary built here only runs on the image if
this host's glibc (ldd --version) is <= the buildroot glibc. If newer, cross-build
with the buildroot toolchain or build in a matched-glibc CI image.

EOF
  exit 1
fi

echo "all required WSL build prerequisites are present"
