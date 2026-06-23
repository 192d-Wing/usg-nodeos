# Buildroot Skeleton

This directory will hold the first bootable image pipeline.

The intended flow is:

1. Build `initd` and `noded` for the target architecture.
2. Copy them into the root filesystem overlay.
3. Install `/init` as the `initd` binary.
4. Install `/etc/nodeos/initd.toml` and `/etc/nodeos/noded.toml`.
5. Build a kernel, initramfs, and QEMU disk image.
6. Run image validation checks.

The files here are scaffolding until Buildroot is vendored or referenced as an
external dependency.

## WSL Notes

Buildroot works best from WSL's native filesystem rather than `/mnt/c`. If build
performance is poor or file permission handling becomes strange, copy the repo
to a path such as:

```bash
~/src/nodeos
```

Then run the scripts from there.
