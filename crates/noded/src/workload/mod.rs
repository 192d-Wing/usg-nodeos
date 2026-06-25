//! Workload layer: the part of the node that differs by [`Profile`].
//!
//! The hardened base (signed boot, `initd` supervision, TPM-backed EST identity,
//! the mTLS management API) is identical across profiles. Everything that knows
//! *what the node runs* — Kubernetes vs. a KVM hypervisor — lives behind the
//! [`WorkloadProfile`] trait so `noded` stays a single universal agent.
//!
//! Phase 1 establishes the seam: both profiles compile and report status, but
//! reconciliation is a no-op. The k8s workload (containerd + kubelet + cluster
//! join) lands in Phase 2; the kvm workload (libvirtd + guest lifecycle) in
//! Phase 3.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde::Serialize;

use crate::Profile;

mod k8s;
mod kvm;

/// The workload-specific behavior `noded` drives for the active [`Profile`].
#[async_trait]
pub trait WorkloadProfile: Send + Sync {
    /// Stable, lowercase profile name for status reporting and audit logs.
    fn name(&self) -> &'static str;

    /// Bring the workload to its desired state. Must be idempotent — it runs at
    /// startup and again whenever declarative config is applied. A no-op until
    /// the per-profile workload layer is implemented.
    async fn reconcile(&self) -> Result<()>;

    /// Current workload health, surfaced on `GET /v1/status`.
    async fn health(&self) -> WorkloadHealth;
}

/// Workload health for the management API. Generic across profiles: each profile
/// fills `components` with the daemons it manages (containerd/kubelet, libvirtd).
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkloadHealth {
    /// Active profile name ("k8s" | "kvm").
    pub profile: String,
    /// Whether the workload is reconciled and healthy. In Phase 1 the workload
    /// layer is not yet implemented, so this is always `false` (the base node
    /// still enrolls and serves the API regardless).
    pub ready: bool,
    /// Per-component status for the daemons this profile owns.
    pub components: Vec<ComponentHealth>,
}

/// Status of a single workload daemon (e.g. containerd, kubelet, libvirtd).
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ComponentHealth {
    pub name: String,
    /// The managing binary is present in the image.
    pub present: bool,
    /// The component is currently running. Best-effort; authoritative health
    /// reporting lands with each profile's workload implementation.
    pub running: bool,
}

impl ComponentHealth {
    /// Report a component by checking only whether its binary is installed in the
    /// image. `running` is left `false` until the profile manages it for real.
    fn from_binary(name: &str, binary: &str) -> Self {
        ComponentHealth {
            name: name.to_string(),
            present: std::path::Path::new(binary).exists(),
            running: false,
        }
    }
}

/// Construct the workload profile for the node's configured [`Profile`].
pub fn new_profile(profile: Profile) -> Arc<dyn WorkloadProfile> {
    match profile {
        Profile::K8s => Arc::new(k8s::K8sProfile::new()),
        Profile::Kvm => Arc::new(kvm::KvmProfile::new()),
    }
}
