//! Kubernetes workload profile: containerd + kubelet, joined to a cluster.
//!
//! Stage 2b: `reconcile` renders kubelet's bootstrap-kubeconfig +
//! KubeletConfiguration from the declarative [`NodeIntent`] onto the encrypted
//! state volume. `initd` then starts kubelet (gated on the rendered
//! bootstrap-kubeconfig appearing — see `startWhen` in the k8s overlay), and
//! kubelet TLS-bootstraps its own file-based client cert and registers. The
//! TPM-backed key cannot be used by kubelet directly; the TPM/EST identity's role
//! is to gate delivery of the bootstrap token over the mTLS management API.

use std::path::Path;

use anyhow::{Context, Result};
use async_trait::async_trait;
use tracing::info;

use crate::est::write_atomic;

use super::{ComponentHealth, NodeIntent, WorkloadHealth, WorkloadProfile};

/// Directory (on the persistent encrypted state volume) holding the rendered
/// kubelet join material.
const KUBELET_DIR: &str = "/var/lib/nodeos/k8s/kubelet";
const CLUSTER_CA_PATH: &str = "/var/lib/nodeos/k8s/kubelet/cluster-ca.pem";
const BOOTSTRAP_KUBECONFIG_PATH: &str = "/var/lib/nodeos/k8s/kubelet/bootstrap-kubeconfig";
const KUBELET_CONFIG_PATH: &str = "/var/lib/nodeos/k8s/kubelet/config.yaml";
/// kubelet writes this once it has bootstrapped a client cert and registered.
const KUBELET_CLIENT_CERT: &str = "/var/lib/kubelet/pki/kubelet-client-current.pem";

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

    async fn reconcile(&self, desired: Option<&NodeIntent>) -> Result<()> {
        let intent = match desired {
            Some(intent) => intent,
            // No join config yet: nothing to render, so kubelet stays deferred
            // (its startWhen marker never appears). The node still serves its
            // management API, through which an operator can PUT the join config.
            None => {
                info!("k8s reconcile: no node intent yet; kubelet remains deferred");
                return Ok(());
            }
        };
        intent.validate().context("invalid node intent")?;

        // Render the CA file first (referenced by path from both kubeconfigs), then
        // the kubelet config, then the bootstrap-kubeconfig LAST — the last write
        // is the startWhen marker initd waits for, so kubelet never starts against
        // a half-rendered config set.
        write_atomic(Path::new(CLUSTER_CA_PATH), intent.cluster_ca.as_bytes(), false)
            .context("write cluster CA")?;
        write_atomic(
            Path::new(KUBELET_CONFIG_PATH),
            render_kubelet_config(intent).as_bytes(),
            false,
        )
        .context("write kubelet config.yaml")?;
        write_atomic(
            Path::new(BOOTSTRAP_KUBECONFIG_PATH),
            render_bootstrap_kubeconfig(intent).as_bytes(),
            true,
        )
        .context("write bootstrap-kubeconfig")?;

        info!(
            api_server = %intent.api_server,
            dir = KUBELET_DIR,
            "k8s reconcile: rendered kubelet join config"
        );
        Ok(())
    }

    async fn health(&self) -> WorkloadHealth {
        let mut containerd = ComponentHealth::from_binary("containerd", "/usr/bin/containerd");
        containerd.running = crate::path_exists("/run/containerd/containerd.sock");
        let mut kubelet = ComponentHealth::from_binary("kubelet", "/usr/bin/kubelet");
        // kubelet writes its bootstrapped client cert once it has registered.
        kubelet.running = crate::path_exists(KUBELET_CLIENT_CERT);

        let ready = containerd.running && kubelet.running;
        WorkloadHealth {
            profile: self.name().to_string(),
            ready,
            components: vec![containerd, kubelet],
        }
    }
}

/// Render kubelet's `--bootstrap-kubeconfig`: the API server + cluster CA (by
/// path) + the bootstrap token. kubelet uses this only until it has a rotated
/// client cert, then switches to `--kubeconfig`.
fn render_bootstrap_kubeconfig(intent: &NodeIntent) -> String {
    format!(
        "apiVersion: v1\n\
         kind: Config\n\
         clusters:\n\
         - name: default\n\
         \x20 cluster:\n\
         \x20   certificate-authority: {ca}\n\
         \x20   server: {server}\n\
         contexts:\n\
         - name: default\n\
         \x20 context:\n\
         \x20   cluster: default\n\
         \x20   user: kubelet-bootstrap\n\
         current-context: default\n\
         users:\n\
         - name: kubelet-bootstrap\n\
         \x20 user:\n\
         \x20   token: {token}\n",
        ca = CLUSTER_CA_PATH,
        server = intent.api_server,
        token = intent.bootstrap_token,
    )
}

/// Render a minimal `KubeletConfiguration`. cgroupfs (no systemd), the containerd
/// CRI endpoint, anonymous-off + webhook auth, and cert rotation. CIS hardening
/// (read-only port off, protectKernelDefaults, FIPS cipher suites) is Stage 2c.
fn render_kubelet_config(intent: &NodeIntent) -> String {
    let mut dns = String::new();
    for addr in &intent.cluster_dns {
        dns.push_str(&format!("  - {addr}\n"));
    }
    format!(
        "apiVersion: kubelet.config.k8s.io/v1beta1\n\
         kind: KubeletConfiguration\n\
         cgroupDriver: cgroupfs\n\
         containerRuntimeEndpoint: unix:///run/containerd/containerd.sock\n\
         clusterDomain: {domain}\n\
         clusterDNS:\n{dns}\
         rotateCertificates: true\n\
         authentication:\n\
         \x20 anonymous:\n\
         \x20   enabled: false\n\
         \x20 webhook:\n\
         \x20   enabled: true\n\
         \x20 x509:\n\
         \x20   clientCAFile: {ca}\n\
         authorization:\n\
         \x20 mode: Webhook\n",
        domain = intent.cluster_domain,
        dns = dns,
        ca = CLUSTER_CA_PATH,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_intent() -> NodeIntent {
        NodeIntent {
            api_server: "https://[2001:db8::1]:6443".to_string(),
            cluster_ca: "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n"
                .to_string(),
            bootstrap_token: "abcdef.0123456789abcdef".to_string(),
            cluster_dns: vec!["fd00::a".to_string()],
            cluster_domain: "cluster.local".to_string(),
            node_labels: vec![],
            node_taints: vec![],
        }
    }

    #[test]
    fn intent_validates_ipv6_https_and_token() {
        sample_intent().validate().expect("valid intent");

        let mut bad = sample_intent();
        bad.api_server = "https://apiserver:6443".to_string();
        assert!(bad.validate().is_err(), "ipv4/hostname apiServer rejected");

        let mut bad = sample_intent();
        bad.api_server = "http://[2001:db8::1]:6443".to_string();
        assert!(bad.validate().is_err(), "http rejected");

        let mut bad = sample_intent();
        bad.bootstrap_token = "ABCDEF.0123456789abcdef".to_string();
        assert!(bad.validate().is_err(), "uppercase token rejected");

        let mut bad = sample_intent();
        bad.bootstrap_token = "abc.0123456789abcdef".to_string();
        assert!(bad.validate().is_err(), "short token id rejected");
    }

    #[test]
    fn bootstrap_kubeconfig_has_server_token_and_ca() {
        let cfg = render_bootstrap_kubeconfig(&sample_intent());
        assert!(cfg.contains("server: https://[2001:db8::1]:6443"));
        assert!(cfg.contains("token: abcdef.0123456789abcdef"));
        assert!(cfg.contains(&format!("certificate-authority: {CLUSTER_CA_PATH}")));
        assert!(cfg.contains("user: kubelet-bootstrap"));
    }

    #[test]
    fn kubelet_config_is_cgroupfs_with_dns_and_runtime() {
        let cfg = render_kubelet_config(&sample_intent());
        assert!(cfg.contains("cgroupDriver: cgroupfs"));
        assert!(cfg.contains("containerRuntimeEndpoint: unix:///run/containerd/containerd.sock"));
        assert!(cfg.contains("  - fd00::a"));
        assert!(cfg.contains("clusterDomain: cluster.local"));
        assert!(cfg.contains("enabled: false")); // anonymous off
    }
}
