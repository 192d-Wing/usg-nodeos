# Architecture

The OS is a minimal, immutable node platform with a local management daemon as
the only supported administrative interface. One hardened base supports multiple
**workload profiles**, selected at build time (`NODEOS_PROFILE`) and reconciled
at runtime by `noded`:

- **k8s** — Kubernetes node (containerd + kubelet).
- **kvm** — bare-metal KVM/libvirt hypervisor host.

## System Model

The node is split into five layers:

1. **Boot layer**: signed bootloader, signed kernel, signed initramfs.
2. **Base OS layer**: immutable root filesystem with kernel, core userspace, the
   TPM/LUKS stack, and `noded`. Identical across profiles — it carries no
   workload assumptions.
3. **Workload layer**: the profile-specific payload — `containerd`/`kubelet` +
   CNI for k8s, `libvirtd` + QEMU for kvm — supervised by `initd` and reconciled
   by `noded`'s `WorkloadProfile`.
4. **State layer**: writable partitions for machine identity, workload state,
   logs, and crash/debug bundles.
5. **Management layer**: local HTTPS API with mandatory mTLS (shared by all
   profiles).

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
- Reconcile the active workload profile (k8s: kubelet + containerd; kvm:
  libvirtd + guest domains) via its `WorkloadProfile` implementation
- Report health, versions, boot state, workload profile, and compliance posture
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
