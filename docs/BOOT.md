# Boot

The first bootable milestone is a QEMU image that starts the Rust `initd` as
PID 1 and then launches `noded`.

## Boot Flow

1. Firmware loads the bootloader.
2. Bootloader loads the kernel and initramfs.
3. Kernel starts `/init`.
4. `/init` is the `initd` binary.
5. `initd` creates the required writable directories.
6. `initd` mounts tmpfs-backed runtime and state paths.
7. `initd` remounts `/` read-only.
8. `initd` starts `noded`.
9. `noded` enrolls/renews via EST and exposes the IPv6-only mTLS management API.

## Binaries and the rootfs (glibc)

The buildroot rootfs is **glibc** (`BR2_TOOLCHAIN_BUILDROOT_GLIBC`). `initd` and
`noded` are cross-compiled against the buildroot toolchain (see the glibc
constraint below) and copied into the rootfs by `build/buildroot/post-build.sh`.

- **Host builds and tests use the default `fips` feature** (aws-lc-rs FIPS
  module) and pass with the FIPS provider active at runtime. The build host needs
  the FIPS toolchain (cmake, Go, clang/libclang).
- **The image is currently built non-FIPS** (`NODEOS_FIPS=0`) because the FIPS
  module cannot be built with the buildroot cross-toolchain yet — see below.

### FIPS in the image (currently blocked)

Building `noded` with `fips` for the rootfs fails in `aws-lc-fips-sys` 0.13.14
(the module pinned by `usg-est-client` v2.0.0 → aws-lc-rs FIPS `=1.17.0`). The
FIPS module build runs a **delocate** step that rewrites the module's assembly
(`bcm.o` → `bcm-delocated.S`) into one contiguous, hashable blob for the FIPS
power-on integrity self-test. It aborts:

```text
error while processing "\t.section\t.data.rel.ro.local,\"aw\"\n" ...
  ".data section found in module"
```

The compiler emitted a `.data.rel.ro.local` section (relocatable read-only data,
local linkage) inside the FIPS module; delocate only accepts a module with no
such data. This happens with **both** the musl cross toolchain and the buildroot
**glibc** cross toolchain. It does **not** happen on the dev host with the same
gcc *version* (15.2.0) — so the trigger is the buildroot toolchain's compiled-in
**default specs** (hardened defaults: default-PIE, default-SSP/stack-protector,
CET/`-fcf-protection`), which introduce relro/local data that 0.13.14's delocate
does not recognize. aws-lc's FIPS delocate is primarily developed/validated
against specific clang versions; gcc — especially a custom-hardened gcc — is not
a supported FIPS build compiler for this module version.

**What is needed to fix it (in order of correctness for ATO):**

1. **Build the image's FIPS binary in the CMVP-aligned environment.** FIPS 140
   validation binds to an exact module version + compiler + flags + OS. Build
   `noded` (fips) using the **validated aws-lc-fips module version and its
   validated compiler/flags** (per the CMVP certificate — typically a specific
   clang), in a pinned CI build container, rather than buildroot's stock hardened
   gcc. This is the only path that yields a *validated* FIPS image. Ties to
   "pin the exact CMVP-validated aws-lc version" in the FIPS plan.
2. **Compile aws-lc with the matching clang for the target.** Provide a target
   clang (cross) and point `aws-lc-fips-sys` at it (`CC_x86_64_unknown_linux_gnu`
   = that clang) so delocate sees supported codegen. Still must match the
   validated toolchain to keep the FIPS claim.
3. **Adopt a newer aws-lc-fips** whose delocate handles `.data.rel.ro.local`
   from this toolchain — requires `usg-est-client` to bump its FIPS pin to a
   module version that is *itself* CMVP-validated for the target environment.
4. **Dev-only workaround (NOT FIPS-valid):** compile the module without the
   offending codegen — non-PIE + `-fno-stack-protector -fcf-protection=none`
   (`relocation-model=static`). This clears delocate but disables binary
   hardening *and* deviates from the validated build, so it cannot carry a FIPS
   claim; useful only to prove the link/boot path.

Until one of (1)–(3) is in place, the image ships non-FIPS and FIPS coverage is
provided by the host-built artifacts and the test suite.

> musl is doubly out for FIPS: aws-lc's FIPS module also fails the same delocate
> step under musl, and musl is not a supported FIPS environment.

### glibc version constraint (build environment)

A dynamically linked glibc binary only runs on the image if the **build host's
glibc is <= the buildroot rootfs glibc**. A bleeding-edge host (e.g. Ubuntu
rolling at glibc 2.43) produces binaries that reference symbol versions newer
than buildroot's glibc, so they fail to start on the image. For a runnable image,
either:

- cross-build `noded`/`initd` with the **buildroot cross-toolchain**
  (`$output/host/bin/x86_64-buildroot-linux-gnu-gcc`) so the binary matches the
  target glibc, or
- build in a **CI image whose glibc matches** (is no newer than) the target.

This is why the dev WSL host cannot, by itself, produce a bootable image when its
glibc is newer than buildroot's.

## State and PKI on a read-only root

Because `/` is read-only, the **enrolled** node identity
(`certFile`/`keyFile`/`clientCa`) is written to `/var/lib/nodeos/pki`, which is
on a writable tmpfs mounted by `initd`. Only the baked-in EST trust anchor
(`enrollment.est.caCertFile`) lives under read-only `/etc/nodeos/pki`.

Today `/var/lib/nodeos` is tmpfs, so the node re-enrolls on every boot
(ephemeral identity). Persisting the node identity across reboots needs a durable
state partition — a planned follow-up.

## Early Init Contract

`initd` is intentionally small. It is not a general-purpose service manager.
Its early responsibilities are:

- prepare runtime directories
- mount explicit writable filesystems
- remount root read-only
- start required node daemons
- restart services marked `always`
- fail the boot if a non-restartable required service exits

## QEMU Target

The first QEMU target should prove:

- the kernel boots
- `/init` is executed
- `/` is read-only after early setup
- `/run` and `/tmp` are tmpfs
- `noded` starts
- `noded` listens only on IPv6
- the image contains no SSH daemon, shell, or package manager
