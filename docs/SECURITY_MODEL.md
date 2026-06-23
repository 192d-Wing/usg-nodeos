# Security Model

The node assumes the management network is hostile. Every management request
must be authenticated, authorized, logged, and designed to be replay-safe where
practical.

## Baseline Controls

- No SSH daemon
- No interactive shell in production images
- No package manager
- Read-only root filesystem
- Mandatory mTLS for management
- Signed boot assets
- Signed OS artifacts
- Kernel lockdown where supported
- Kernel module signature enforcement
- Minimal Linux capabilities for host processes
- Kubernetes Pod Security Admission defaults
- Kubernetes secrets encryption at rest by default for control plane clusters
- IPv6-only host networking; IPv4 VIPs are advertised by the cluster network
  layer, not assigned to nodes

## Identity

Each node has:

- A node identity certificate
- A management server certificate
- A trusted management CA bundle
- A cluster assignment, if provisioned

Certificates should be short-lived where possible and rotatable through the
management API.

## Bootstrap Enrollment

Nodes use Enrollment over Secure Transport (EST) for initial certificate
enrollment and renewal. The bootstrap image carries only the EST trust anchor
and a short-lived bearer token in `/etc/nodeos/noded.yaml`.

The bearer token is only valid for EST bootstrap. It must not authorize normal
node management APIs, and it should be replaced by certificate identity as soon
as enrollment succeeds.

## Authorization

Role-based authorization is enforced on every management request. The
authenticated identity is the **Common Name** of the verified mTLS client
certificate; `authorization.roles` in `noded.yaml` binds Common Names to roles.

Roles are hierarchical — a higher role satisfies any requirement of a lower one:

- `viewer`: read health and status
- `operator`: apply safe config and restart services
- `maintainer`: stage updates and reboot
- `breakglass`: destructive repair actions with strong audit requirements

Enforcement is **deny-by-default**: a subject with no role binding is refused
anything that requires a role, and unknown routes require the most privileged
role. Endpoints declare their minimum role centrally:

- `GET /v1/healthz` — no role (any valid mTLS client; liveness)
- `GET /v1/status` — `viewer`
- `PUT /v1/config` — `operator` (mutation logic itself is a pending follow-up;
  authorized callers currently receive `501 Not Implemented`)

Example policy:

```yaml
authorization:
  roles:
    - role: operator
      subjects: ["mgmt-operator-01"]
    - role: breakglass
      subjects: ["break-glass-01"]
```

## Audit

Every management request produces a structured audit event (tracing target
`audit`) containing:

- timestamp
- client certificate subject (Common Name)
- request ID (per-process monotonic counter)
- action (HTTP method)
- target resource (request path)
- required role and granted role
- decision (`allow` / `deny`)
- reason

## Production Image Exclusions

The following must not ship in production images:

- `/bin/sh`, `/bin/bash`, or equivalent interactive shells
- SSH client or server
- package managers
- compilers
- debuggers
- interpreters not required by the runtime
