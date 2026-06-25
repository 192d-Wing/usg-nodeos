# Roadmap

## Phase 0: Shape the System

- Define OS goals and non-goals
- Define management API
- Create Rust `noded` skeleton
- Create Rust `initd` skeleton
- Document security model
- Choose image build system

## Phase 1: Local Node Prototype

- Start `noded` with mandatory mTLS
- Start `noded` from `initd`
- Add health and status endpoints
- Load declarative node config from disk
- Add audit logging
- Add service supervision abstraction
- Run under a normal Linux host for development

## Phase 2: Bootable Image

- Build minimal kernel/rootfs
- Boot in QEMU
- Start `initd` as PID 1
- Mount immutable root read-only
- Persist only explicit writable state
- Verify no SSH, shell, or package manager exists in production image

## Phase 3: Kubernetes Node (k8s profile)

- Add containerd
- Add kubelet
- Join a Kubernetes cluster from declarative config
- Enforce CIS-oriented kubelet defaults
- Export compliance evidence from the node API

## Phase 3b: KVM Hypervisor Node (kvm profile)

- Factor the workload layer out of the shared base (profile seam — done)
- Add libvirt + QEMU/KVM to the kvm image
- Reconcile declarative VM domains via the node API
- Report libvirtd + per-domain state through `/v1/status`
- Same TPM-backed enrollment, mTLS API, and immutable-root posture as k8s

## Phase 4: Fleet Controller

- Define cluster and machine resources
- Generate node configs and certificates
- Bootstrap control plane clusters
- Manage upgrades and reboots
- Provide audit trail and approval workflow

## Phase 5: Government Hardening

- Airgapped update channels
- FIPS posture
- STIG/CIS evidence exports
- TPM-backed identity path
- Secure Boot signing path
- Incident support bundle workflow
