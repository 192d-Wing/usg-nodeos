# Filesystem

The production image uses an immutable root filesystem. Runtime state is
limited to explicit writable locations.

## Immutable

The root filesystem is mounted read-only after early boot. The production image
must not include:

- SSH daemon or client
- interactive shell
- package manager
- compiler toolchain
- debugger
- scripting language runtime unless required by the OS

## Writable Paths

The only expected writable paths are:

- `/run`: tmpfs runtime state
- `/tmp`: tmpfs scratch space
- `/sys/fs/cgroup`: cgroup v2 unified hierarchy (k8s profile)
- `/var/lib/nodeos`: node identity and management state (persistent, encrypted)
- `/var/lib/kubelet`: kubelet state (tmpfs — reconstructible on rejoin)
- `/var/lib/containerd`: container image store + runtime state (k8s profile;
  persistent, encrypted — images survive reboot)
- `/var/log`: logs

Durable state lives on LUKS-encrypted partitions whose keys are sealed in the
TPM to the measured-boot PCRs (see [BOOT.md](BOOT.md)):

- `vda2` (`nodeos-state`) → `/var/lib/nodeos` — both profiles.
- `vda3` (`nodeos-data`) → `/var/lib/containerd` — k8s profile only (the kvm
  profile leaves `vda3` unused).

Ephemeral paths (`/run`, `/tmp`, `/var/lib/kubelet`, `/var/log`) are tmpfs.

## Checks

The image validation step should fail if any of the following exist:

- `/bin/sh`
- `/bin/bash`
- `/usr/bin/ssh`
- `/usr/sbin/sshd`
- `/usr/bin/apt`
- `/usr/bin/dnf`
- `/usr/bin/yum`
- `/usr/bin/apk`
