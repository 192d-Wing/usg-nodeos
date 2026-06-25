//! KVM hypervisor workload profile: libvirtd + guest (domain) lifecycle.
//!
//! Phase 1 is a placeholder so the kvm image builds and boots on the shared base.
//! The real reconcile (supervise libvirtd, reconcile declarative VM domains
//! delivered via `/v1/config`, report per-domain state) lands in Phase 3.

use anyhow::Result;
use async_trait::async_trait;

use super::{ComponentHealth, WorkloadHealth, WorkloadProfile};

pub struct KvmProfile;

impl KvmProfile {
    pub fn new() -> Self {
        KvmProfile
    }
}

#[async_trait]
impl WorkloadProfile for KvmProfile {
    fn name(&self) -> &'static str {
        "kvm"
    }

    async fn reconcile(&self, _desired: Option<&super::NodeIntent>) -> Result<()> {
        // Phase 3: ensure libvirtd is supervised and reconcile declarative VM
        // domains to their desired run state. The k8s-shaped NodeIntent is ignored
        // until Phase 3 generalizes the declarative intent per profile. No-op now.
        Ok(())
    }

    async fn health(&self) -> WorkloadHealth {
        let components = vec![ComponentHealth::from_binary("libvirtd", "/usr/sbin/libvirtd")];
        WorkloadHealth {
            profile: self.name().to_string(),
            ready: false,
            components,
        }
    }
}
