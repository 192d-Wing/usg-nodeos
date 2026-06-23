# Minimal Kubernetes Node OS

This repository is the starting point for a purpose-built Kubernetes node
operating system. It is intentionally not Talos-compatible.

The design goals are:

- Immutable root filesystem
- No SSH daemon
- No interactive shell in the production image
- No package manager in the production image
- API-only node management
- Mutual TLS for every management operation
- Hardened kernel and conservative runtime defaults
- CIS-oriented Kubernetes defaults
- Kubernetes-first operation
- IPv6-only node networking

The first deliverable is a minimal Rust node management daemon, `noded`, plus the
design documents needed to build a reproducible OS image around it. Bootstrap
identity enrollment is EST-based and driven by YAML configuration.

## Components

- `cmd/noded`: local node management API daemon
- `crates/noded`: Rust local node management API daemon
- `crates/initd`: Rust PID 1 / early userspace supervisor
- `api/node/v1`: first version of the management API contract
- `build/buildroot`: first bootable image pipeline skeleton
- `docs`: architecture, security model, build strategy, and roadmap

## Early Boot Shape

The intended boot flow is:

1. Firmware loads a signed bootloader.
2. Bootloader verifies and loads a signed kernel and initramfs.
3. A tiny init process mounts immutable root partitions read-only.
4. `noded` reads `/etc/nodeos/noded.yaml` and enrolls identity through EST.
5. `noded` starts as the only management surface on IPv6 only.
6. `containerd` and `kubelet` are launched from declarative node config.
7. All administrative actions go through the mTLS API.

## Non-Goals

- General-purpose Linux usage
- SSH administration
- Runtime package installation
- Talos API compatibility
- Mutable host configuration
- IPv4 node addressing
