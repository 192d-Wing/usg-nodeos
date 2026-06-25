//! Kubernetes workload profile: containerd + kubelet, joined to a cluster.
//!
//! Phase 1 is a placeholder that reports the components it will manage. The real
//! reconcile (write kubelet config + kubeconfig from the TPM-backed EST identity,
//! supervise containerd/kubelet, join the cluster) lands in Phase 2.

use anyhow::Result;
use async_trait::async_trait;

use super::{ComponentHealth, WorkloadHealth, WorkloadProfile};

pub struct K8sProfile;

impl K8sProfile {
    pub fn new() -> Self {
        K8sProfile
    }
}

#[async_trait]
impl WorkloadProfile for K8sProfile {
    fn name(&self) -> &'static str {
        "k8s"
    }

    async fn reconcile(&self) -> Result<()> {
        // Phase 2: render kubelet config + kubeconfig (kubelet client identity =
        // the TPM-backed EST cert), ensure containerd/kubelet are supervised, and
        // join the cluster from declarative config. No-op for now.
        Ok(())
    }

    async fn health(&self) -> WorkloadHealth {
        let components = vec![
            ComponentHealth::from_binary("containerd", "/usr/bin/containerd"),
            ComponentHealth::from_binary("kubelet", "/usr/bin/kubelet"),
        ];
        WorkloadHealth {
            profile: self.name().to_string(),
            ready: false,
            components,
        }
    }
}
