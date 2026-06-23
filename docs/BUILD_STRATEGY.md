# Build Strategy

The first practical build should be boring and reproducible.

## Recommended Path

Use a staged image pipeline:

1. Build a pinned Linux kernel with hardened config.
2. Build Rust host binaries, including `noded`.
3. Assemble an initramfs with only the early boot tools required.
4. Assemble an immutable root filesystem.
5. Produce disk, ISO, and PXE artifacts.
6. Generate SBOMs and provenance attestations.
7. Sign all boot and OS artifacts.

## WSL Build Prerequisites

For the first QEMU image, Ubuntu WSL needs the normal Buildroot toolchain
dependencies plus Rust and QEMU:

```bash
sudo apt update
sudo apt install -y \
  bc bison build-essential cpio file flex git gzip libncurses-dev \
  make patch perl python3 qemu-system-x86 rsync rustc cargo tar unzip wget
```

Then run:

```bash
scripts/check-wsl-prereqs.sh
scripts/build-rust-linux.sh
```

The Buildroot image flow is scaffolded behind:

```bash
scripts/build-buildroot-image.sh
scripts/run-qemu.sh
```

## Candidate Tooling

Good options:

- Buildroot for minimal rootfs construction
- Yocto if government certification/custom hardware support dominates
- Custom Rust initramfs if we want maximal control

For the first prototype, Buildroot is the simplest serious choice.

## Kernel Hardening Targets

Initial kernel config goals:

- signed kernel modules only
- disable unsigned module loading
- lockdown mode where supported
- disable unnecessary filesystems
- disable legacy network protocols
- disable debug interfaces in production
- restrict BPF defaults
- enable seccomp
- enable AppArmor or SELinux
- enable IMA/EVM evaluation path
- disable IPv4 host addressing in the production image

## Artifact Outputs

The pipeline should eventually emit:

- raw disk image
- installer ISO
- PXE kernel/initramfs/rootfs
- SBOM
- vulnerability report
- provenance attestation
- signature bundle
