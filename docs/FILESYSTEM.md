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
- `/var/lib/nodeos`: node identity and management state
- `/var/lib/kubelet`: kubelet state
- `/var/lib/containerd`: container runtime state
- `/var/log`: logs

The first QEMU prototype mounts these writable paths as tmpfs. Later hardware
targets should move durable state to explicit signed/encrypted state
partitions.

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
