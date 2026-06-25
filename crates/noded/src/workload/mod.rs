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

use std::net::{IpAddr, Ipv6Addr};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use der::DecodePem;
use serde::{Deserialize, Serialize};
use x509_cert::Certificate;

use crate::Profile;

mod k8s;
mod kvm;

/// Declarative node-join intent delivered via `PUT /v1/config` (and persisted so
/// it is re-applied at boot). Currently shaped for the k8s profile (cluster join);
/// the kvm profile ignores it until Phase 3 generalizes this into a per-profile
/// intent. Validated by [`NodeIntent::validate`].
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeIntent {
    /// Kubernetes API server URL — IPv6 literal, https (e.g.
    /// `https://[2001:db8::1]:6443`).
    pub api_server: String,
    /// Cluster CA bundle (PEM) that signs the API server certificate.
    pub cluster_ca: String,
    /// Bootstrap token (`[a-z0-9]{6}.[a-z0-9]{16}`) for kubelet TLS bootstrap.
    pub bootstrap_token: String,
    /// In-cluster DNS service addresses (IPv6 for an IPv6-only cluster).
    #[serde(default)]
    pub cluster_dns: Vec<String>,
    /// Cluster DNS domain.
    #[serde(default = "default_cluster_domain")]
    pub cluster_domain: String,
    /// Optional node labels (`key=value`). Not yet wired to kubelet flags — see
    /// the node-ip/labels note in the Stage 2b plan.
    #[serde(default)]
    pub node_labels: Vec<String>,
    /// Optional node taints (`key=value:Effect`).
    #[serde(default)]
    pub node_taints: Vec<String>,
}

fn default_cluster_domain() -> String {
    "cluster.local".to_string()
}

impl NodeIntent {
    /// Semantic validation beyond serde's structural parse. Mirrors the EST
    /// config checks: IPv6-only + https for the API server, a well-formed
    /// bootstrap token, and a parseable CA bundle.
    pub fn validate(&self) -> Result<()> {
        // IPv6-only node: the API server must be https with a *parseable*
        // bracketed IPv6 literal authority (not just the prefix). Validating the
        // host also keeps it safe to interpolate into the rendered kubeconfig.
        validate_api_server(&self.api_server)?;
        if !valid_bootstrap_token(&self.bootstrap_token) {
            return Err(anyhow!(
                "bootstrapToken must match [a-z0-9]{{6}}.[a-z0-9]{{16}}"
            ));
        }
        // Real PEM/DER parse, not a substring check, so a corrupt bundle is
        // rejected at the API boundary rather than failing kubelet's TLS later.
        Certificate::from_pem(self.cluster_ca.as_bytes())
            .map_err(|err| anyhow!("clusterCa is not a valid PEM certificate: {err}"))?;
        // A node needs in-cluster DNS; an empty list would silently disable it.
        if self.cluster_dns.is_empty() {
            return Err(anyhow!("clusterDNS must list at least one DNS address"));
        }
        for addr in &self.cluster_dns {
            addr.parse::<IpAddr>()
                .map_err(|_| anyhow!("clusterDNS entry is not an IP address: {addr}"))?;
        }
        if self.cluster_domain.is_empty()
            || !self
                .cluster_domain
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        {
            return Err(anyhow!("clusterDomain must be a DNS name"));
        }
        Ok(())
    }
}

/// Validate the API server URL: `https://[<ipv6>]` with an optional `:port`. The
/// host must parse as an IPv6 address (the node is IPv6-only).
fn validate_api_server(url: &str) -> Result<()> {
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| anyhow!("apiServer must use https"))?;
    if !rest.starts_with('[') {
        return Err(anyhow!(
            "apiServer host must be a bracketed IPv6 literal, e.g. https://[2001:db8::1]:6443"
        ));
    }
    let close = rest
        .find(']')
        .ok_or_else(|| anyhow!("apiServer IPv6 literal is missing its closing ']'"))?;
    rest[1..close]
        .parse::<Ipv6Addr>()
        .map_err(|_| anyhow!("apiServer host is not a valid IPv6 address: {}", &rest[1..close]))?;
    // Anything after ']' must be an optional ":<port>".
    let tail = &rest[close + 1..];
    if !tail.is_empty() && !tail.starts_with(':') {
        return Err(anyhow!("apiServer has unexpected data after the IPv6 host"));
    }
    Ok(())
}

/// A kubeadm-style bootstrap token: `<6 lowercase-alnum>.<16 lowercase-alnum>`.
fn valid_bootstrap_token(token: &str) -> bool {
    let alnum = |s: &str, n: usize| {
        s.len() == n && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    };
    match token.split_once('.') {
        Some((id, secret)) => alnum(id, 6) && alnum(secret, 16),
        None => false,
    }
}

/// The workload-specific behavior `noded` drives for the active [`Profile`].
#[async_trait]
pub trait WorkloadProfile: Send + Sync {
    /// Stable, lowercase profile name for status reporting and audit logs.
    fn name(&self) -> &'static str;

    /// Bring the workload to its desired state for the given declarative intent
    /// (`None` when the node has not been given join config yet). Must be
    /// idempotent — it runs at startup with the persisted intent and again on
    /// every `PUT /v1/config`.
    async fn reconcile(&self, desired: Option<&NodeIntent>) -> Result<()>;

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
            present: crate::path_exists(binary),
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
