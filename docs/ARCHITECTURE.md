# Architecture

The OS is a minimal, immutable Kubernetes node platform with a local management
daemon as the only supported administrative interface.

## System Model

The node is split into four layers:

1. **Boot layer**: signed bootloader, signed kernel, signed initramfs.
2. **Base OS layer**: immutable root filesystem with kernel, core userspace,
   `containerd`, `kubelet`, CNI assets, and `noded`.
3. **State layer**: writable partitions for machine identity, Kubernetes state,
   container images, logs, and crash/debug bundles.
4. **Management layer**: local HTTPS API with mandatory mTLS.

## Filesystem Layout

Production images should use a read-only root filesystem.

Writable paths are explicit:

- `/var/lib/nodeos`: machine identity and node state
- `/var/lib/containerd`: container runtime state
- `/var/lib/kubelet`: kubelet state
- `/var/log`: logs
- `/run`: tmpfs runtime state
- `/tmp`: tmpfs scratch space

The production image must not include a package manager, SSH daemon, or
interactive shell.

## Node Agent

`noded` owns host-level reconciliation. It should:

- Expose a local management API
- Bind only to IPv6 addresses
- Require client certificates
- Validate and apply declarative node configuration
- Manage kubelet and containerd lifecycle
- Report health, versions, boot state, and compliance posture
- Stage OS updates atomically
- Reboot only after explicit API requests and policy checks

## Kubernetes

Kubernetes is the primary workload. The OS should run:

- `containerd`
- `kubelet`
- CNI configuration/assets
- static bootstrap manifests only when explicitly configured

Control plane construction belongs to the external fleet controller, not hidden
host-side magic.

## Networking

The OS is IPv6-only at the host and management layers. Node addresses,
management API listeners, kubelet node IPs, and cluster infrastructure should be
IPv6.

IPv4 service exposure is handled above the node layer. In our target clusters,
Cilium advertises IPv4 load balancer VIPs from the IPv6-only fabric using BGP
extended next hop. This keeps the node OS free of IPv4 addressing while still
allowing IPv4 clients to reach load balancer services.
