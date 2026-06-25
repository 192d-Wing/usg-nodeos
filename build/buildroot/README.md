# Buildroot Skeleton

This directory holds the bootable image pipeline.

The flow (driven by `scripts/build-buildroot-image.sh`) is:

1. Build `initd` and `noded` for the target architecture.
2. Apply the root filesystem overlays (shared base + the selected profile).
3. `post-build.sh` installs `/init` → `initd`, injects `.env` deployment values,
   and runs the forbidden-path + profile-consistency checks.
4. Build a kernel, initramfs, and QEMU disk image.
5. Run image validation checks.

## Workload profiles

One hardened base produces two image variants, selected with
`NODEOS_PROFILE={k8s,kvm}` (default `k8s`):

- **k8s** — Kubernetes node (containerd + kubelet; Phase 2).
- **kvm** — bare-metal KVM/libvirt hypervisor host (Phase 3).

The base OS — signed boot, `initd`, `noded`, the TPM-backed EST identity, and the
mTLS management API — is identical across profiles; only the workload layer
differs. That difference lives entirely in the build inputs:

- `overlay-base/` — files common to every profile (`/etc/hosts`, the EST trust
  anchor). Always applied first.
- `overlay-<profile>/` — the profile's `/etc/nodeos/initd.toml` (mounts +
  supervised services) and `noded.yaml` (carrying `profile: <profile>`). Layered
  on top of the base overlay.
- `qemu-x86_64[-<profile>].defconfig` / `qemu-x86_64-linux[-<profile>].config` —
  the build resolves the `-<profile>` file when present, else the shared base
  file. Identical inputs stay shared (no drift); a profile diverges by adding its
  own copy.

## WSL Notes

Buildroot works best from WSL's native filesystem rather than `/mnt/c`. If build
performance is poor or file permission handling becomes strange, copy the repo
to a path such as:

```bash
~/src/nodeos
```

Then run the scripts from there.
