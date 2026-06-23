//! EST (RFC 7030) enrollment and renewal for the node.
//!
//! This is a thin integration layer over the shared `usg-est-client` crate
//! (github.com/192d-Wing/usg-est-client) — we deliberately do not maintain a
//! second EST implementation here. This module:
//!
//! 1. translates `noded`'s YAML config into an `EstClientConfig`,
//! 2. drives `cacerts` + `simpleenroll` for first-time bootstrap (authenticated
//!    with the bootstrap bearer token), and `simplereenroll` for renewal
//!    (authenticated with the current node certificate via mTLS — no token), and
//! 3. persists the resulting PKI atomically on disk.
//!
//! The bearer token is bootstrap-only: it is sent solely to EST endpoints and is
//! never used once the node holds a certificate. Renewal uses the node identity.

use std::{
    fs::OpenOptions,
    io::Write as _,
    path::Path,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, Context, Result};
use der::{pem::LineEnding, DecodePem, EncodePem};
use rustls::client::ResolvesClientCert;
use tracing::info;
use usg_est_client::{
    csr::{CsrBuilder, HsmCsrBuilder},
    Certificate, EnrollmentResponse, EstClient, EstClientConfig,
};
use x509_cert::time::Time;

use crate::pkcs11::TpmIdentity;
use crate::Config;

/// How the EST client authenticates to the server.
enum Auth<'a> {
    /// Bootstrap: `Authorization: Bearer <token>` (first enrollment only).
    Bearer(&'a str),
    /// Renewal: mTLS using the current node certificate and key (PEM).
    ClientCert { cert_pem: Vec<u8>, key_pem: Vec<u8> },
    /// Renewal with a TPM-resident key: mTLS via a token-backed client-cert
    /// resolver (the private key never leaves the token).
    ClientResolver(Arc<dyn ResolvesClientCert>),
}

/// Atomically write `data` to `path` (temp file in the same directory, fsync,
/// then rename). Key material is created with mode `0600` on Unix.
fn write_atomic(path: &Path, data: &[u8], private: bool) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("path has no parent directory: {}", path.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create directory {}", parent.display()))?;

    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow!("path has no file name: {}", path.display()))?
        .to_string_lossy();
    let tmp = parent.join(format!(".{file_name}.tmp"));

    let mut opts = OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(if private { 0o600 } else { 0o644 });
    }

    let mut file = opts
        .open(&tmp)
        .with_context(|| format!("open temp file {}", tmp.display()))?;
    file.write_all(data)
        .with_context(|| format!("write {}", tmp.display()))?;
    file.sync_all()
        .with_context(|| format!("fsync {}", tmp.display()))?;
    drop(file);

    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Build the EST client configuration from `noded`'s config.
///
/// Trust is pinned to the single EST CA shipped in the node image. The
/// authentication method depends on whether this is a bootstrap (bearer token)
/// or a renewal (node-certificate mTLS).
fn build_est_config(config: &Config, auth: Auth) -> Result<EstClientConfig> {
    let est = &config.enrollment.est;

    let ca_pem = std::fs::read(&est.ca_cert_file)
        .with_context(|| format!("read EST CA {}", est.ca_cert_file.display()))?;

    let mut builder = EstClientConfig::builder()
        .server_url(&est.server_url)
        .with_context(|| format!("parse EST serverUrl {}", est.server_url))?
        .trust_explicit(vec![ca_pem]);

    builder = match auth {
        Auth::Bearer(token) => builder.add_header("Authorization", format!("Bearer {token}")),
        Auth::ClientCert { cert_pem, key_pem } => builder.client_identity_pem(cert_pem, key_pem),
        Auth::ClientResolver(resolver) => builder.client_identity_resolver(resolver),
    };

    // An optional RFC 7030 CA label maps to the `/.well-known/est/{label}/...`
    // path segment. When absent the unlabeled endpoints are used.
    if let Some(label) = est.label.as_deref().filter(|l| !l.is_empty()) {
        builder = builder.ca_label(label);
    }

    builder
        .build()
        .map_err(|err| anyhow!("build EST client config: {err}"))
}

/// Generate the node key pair and a PKCS#10 CSR for the node identity. The node
/// certificate is used as the mTLS *server* certificate, so request both server
/// and client auth. Returns `(csr_der, private_key_pem)`.
fn generate_node_csr(node_id: &str) -> Result<(Vec<u8>, String)> {
    let (csr_der, key_pair) = CsrBuilder::new()
        .common_name(node_id)
        .san_dns(node_id)
        .key_usage_digital_signature()
        .key_usage_key_agreement()
        .extended_key_usage_server_auth()
        .extended_key_usage_client_auth()
        .build()
        .map_err(|err| anyhow!("generate CSR: {err}"))?;
    Ok((csr_der, key_pair.serialize_pem()))
}

/// Generate a PKCS#10 CSR for the node identity signed by the **TPM-resident**
/// key (the private key never leaves the token). Mirrors the key usages of the
/// software path. Returns the DER-encoded CSR; there is no private-key PEM.
async fn generate_node_csr_tpm(node_id: &str, tpm: &TpmIdentity) -> Result<Vec<u8>> {
    HsmCsrBuilder::new()
        .common_name(node_id)
        .san_dns(node_id)
        .key_usage_digital_signature()
        .key_usage_key_agreement()
        .extended_key_usage_server_auth()
        .extended_key_usage_client_auth()
        .build_with_provider(tpm.provider(), tpm.handle())
        .await
        .map_err(|err| anyhow!("generate TPM CSR: {err}"))
}

/// Atomically write the node certificate only (TPM path: the private key lives
/// in the token, never on disk).
fn write_cert_only(config: &Config, cert: &Certificate) -> Result<()> {
    let cert_pem = cert
        .to_pem(LineEnding::LF)
        .map_err(|err| anyhow!("encode node certificate PEM: {err}"))?;
    write_atomic(&config.cert_file, cert_pem.as_bytes(), false).context("write node certificate")?;
    Ok(())
}

/// Unwrap an enrollment response into the issued certificate, mapping a
/// "pending manual approval" result to a descriptive error.
fn issued_cert(response: EnrollmentResponse, op: &str) -> Result<Box<Certificate>> {
    match response {
        EnrollmentResponse::Issued { certificate } => Ok(certificate),
        EnrollmentResponse::Pending { retry_after } => Err(anyhow!(
            "EST {op} pending manual approval; retry after {retry_after}s"
        )),
    }
}

/// Atomically write the node identity (certificate + private key).
fn write_identity(config: &Config, cert: &Certificate, key_pem: &str) -> Result<()> {
    let cert_pem = cert
        .to_pem(LineEnding::LF)
        .map_err(|err| anyhow!("encode node certificate PEM: {err}"))?;
    write_atomic(&config.cert_file, cert_pem.as_bytes(), false)
        .context("write node certificate")?;
    write_atomic(&config.key_file, key_pem.as_bytes(), true).context("write node key")?;
    Ok(())
}

/// Perform the full bootstrap EST enrollment and persist the resulting PKI
/// (node identity + the management CA bundle used as the mTLS client verifier).
pub(crate) async fn bootstrap_enroll(config: &Config, tpm: Option<&TpmIdentity>) -> Result<()> {
    let est_config = build_est_config(config, Auth::Bearer(&config.enrollment.est.bearer_token))?;
    let client = EstClient::new(est_config)
        .await
        .map_err(|err| anyhow!("create EST client: {err}"))?;

    // 1. Fetch the management CA bundle (mTLS client trust anchor).
    info!(server = %config.enrollment.est.server_url, "EST: fetching CA certificates");
    let ca_certs = client
        .get_ca_certs()
        .await
        .map_err(|err| anyhow!("EST cacerts failed: {err}"))?;
    if ca_certs.is_empty() {
        return Err(anyhow!("EST cacerts returned no certificates"));
    }

    // 2. Generate key + CSR, then enroll. With a TPM the key is generated in the
    //    token and only the issued certificate is persisted; otherwise the key
    //    pair and certificate are both written to disk.
    info!(node_id = %config.node_id, tpm = tpm.is_some(), "EST: submitting CSR (simpleenroll)");
    if let Some(tpm) = tpm {
        let csr_der = generate_node_csr_tpm(&config.node_id, tpm).await?;
        let node_cert = issued_cert(
            client
                .simple_enroll(&csr_der)
                .await
                .map_err(|err| anyhow!("EST simpleenroll failed: {err}"))?,
            "enrollment",
        )?;
        write_cert_only(config, &node_cert)?;
    } else {
        let (csr_der, key_pem) = generate_node_csr(&config.node_id)?;
        let node_cert = issued_cert(
            client
                .simple_enroll(&csr_der)
                .await
                .map_err(|err| anyhow!("EST simpleenroll failed: {err}"))?,
            "enrollment",
        )?;
        write_identity(config, &node_cert, &key_pem)?;
    }

    // 3. Persist the management CA bundle (mTLS client verifier).
    let mut client_ca_pem = String::new();
    for cert in ca_certs.iter() {
        let pem = cert
            .to_pem(LineEnding::LF)
            .map_err(|err| anyhow!("encode CA certificate PEM: {err}"))?;
        client_ca_pem.push_str(&pem);
    }
    write_atomic(&config.client_ca, client_ca_pem.as_bytes(), false)
        .context("write client CA bundle")?;

    info!(
        cert = %config.cert_file.display(),
        client_ca = %config.client_ca.display(),
        ca_count = ca_certs.len(),
        tpm = tpm.is_some(),
        "EST: enrollment complete; PKI written"
    );
    Ok(())
}

/// Renew the node certificate via `simplereenroll`, authenticating with the
/// current node identity over mTLS (no bearer token). The management CA bundle
/// (`clientCa`) is left untouched — renewal rotates the leaf only.
///
/// Software path: a fresh key pair is generated and the new identity (cert+key)
/// replaces the old atomically. TPM path: the existing token key is reused
/// (proof-of-possession via the resolver), the CSR is signed by the token, and
/// only the rotated certificate is written.
pub(crate) async fn reenroll(config: &Config, tpm: Option<&TpmIdentity>) -> Result<()> {
    if let Some(tpm) = tpm {
        // mTLS client auth via the token-backed resolver presenting the current
        // certificate; the private key never leaves the TPM.
        let cert_chain = crate::pkcs11::cert_chain_from_pem(&config.cert_file)?;
        let est_config = build_est_config(config, Auth::ClientResolver(tpm.client_resolver(cert_chain)))?;
        let client = EstClient::new(est_config)
            .await
            .map_err(|err| anyhow!("create EST client: {err}"))?;

        let csr_der = generate_node_csr_tpm(&config.node_id, tpm).await?;
        info!(node_id = %config.node_id, tpm = true, "EST: submitting CSR (simplereenroll)");
        let node_cert = issued_cert(
            client
                .simple_reenroll(&csr_der)
                .await
                .map_err(|err| anyhow!("EST simplereenroll failed: {err}"))?,
            "re-enrollment",
        )?;
        write_cert_only(config, &node_cert)?;
    } else {
        let cert_pem = std::fs::read(&config.cert_file)
            .with_context(|| format!("read node certificate {}", config.cert_file.display()))?;
        let key_pem = std::fs::read(&config.key_file)
            .with_context(|| format!("read node key {}", config.key_file.display()))?;

        let est_config = build_est_config(config, Auth::ClientCert { cert_pem, key_pem })?;
        let client = EstClient::new(est_config)
            .await
            .map_err(|err| anyhow!("create EST client: {err}"))?;

        let (csr_der, key_pem) = generate_node_csr(&config.node_id)?;
        info!(node_id = %config.node_id, tpm = false, "EST: submitting CSR (simplereenroll)");
        let node_cert = issued_cert(
            client
                .simple_reenroll(&csr_der)
                .await
                .map_err(|err| anyhow!("EST simplereenroll failed: {err}"))?,
            "re-enrollment",
        )?;
        write_identity(config, &node_cert, &key_pem)?;
    }

    info!(
        cert = %config.cert_file.display(),
        tpm = tpm.is_some(),
        "EST: re-enrollment complete; node identity rotated"
    );
    Ok(())
}

/// Convert an X.509 `Time` to a duration since the Unix epoch.
fn time_to_unix(time: &Time) -> Duration {
    match time {
        Time::UtcTime(t) => t.to_unix_duration(),
        Time::GeneralTime(t) => t.to_unix_duration(),
    }
}

/// Decide whether a certificate should be renewed: once it is past two-thirds of
/// its validity lifetime (or already expired). This adapts to short-lived certs
/// without needing a configured window.
fn needs_renewal(not_before: Duration, not_after: Duration, now: Duration) -> bool {
    if now >= not_after {
        return true;
    }
    let lifetime = not_after.saturating_sub(not_before);
    let remaining = not_after.saturating_sub(now);
    remaining < lifetime / 3
}

/// Read the node certificate and decide whether it is due for renewal.
pub(crate) fn cert_needs_renewal(cert_path: &Path) -> Result<bool> {
    let pem = std::fs::read(cert_path)
        .with_context(|| format!("read node certificate {}", cert_path.display()))?;
    let cert = Certificate::from_pem(&pem)
        .map_err(|err| anyhow!("parse node certificate {}: {err}", cert_path.display()))?;
    let validity = cert.tbs_certificate().validity();
    let not_before = time_to_unix(&validity.not_before);
    let not_after = time_to_unix(&validity.not_after);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock before Unix epoch")?;
    Ok(needs_renewal(not_before, not_after, now))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use std::path::PathBuf;

    use crate::{EnrollmentConfig, EstConfig};

    fn test_config(server_url: &str, label: Option<&str>) -> Config {
        Config {
            listen_addr: "[::1]:9443".parse::<SocketAddr>().unwrap(),
            node_id: "qemu-node-001".to_string(),
            cert_file: PathBuf::from("/etc/nodeos/pki/server.crt"),
            key_file: PathBuf::from("/etc/nodeos/pki/server.key"),
            client_ca: PathBuf::from("/etc/nodeos/pki/client-ca.crt"),
            enrollment: EnrollmentConfig {
                est: EstConfig {
                    server_url: server_url.to_string(),
                    bearer_token: "bootstrap-token".to_string(),
                    ca_cert_file: PathBuf::from("/etc/nodeos/pki/est-ca.crt"),
                    label: label.map(str::to_string),
                },
                renew_check_interval_secs: 3600,
            },
            authorization: Default::default(),
            tpm: None,
        }
    }

    /// The EST CA file is read while building the config; point it at a missing
    /// path so we exercise URL handling without needing a real cert fixture.
    #[test]
    fn build_est_config_requires_readable_ca() {
        let config = test_config("https://[2001:db8::10]", None);
        let err = build_est_config(&config, Auth::Bearer("t")).unwrap_err();
        assert!(err.to_string().contains("read EST CA"));
    }

    #[test]
    fn needs_renewal_past_two_thirds_of_lifetime() {
        let nb = Duration::from_secs(1_000);
        let na = Duration::from_secs(1_900); // 900s lifetime; renew window = last 300s
        assert!(!needs_renewal(nb, na, Duration::from_secs(1_000))); // fresh
        assert!(!needs_renewal(nb, na, Duration::from_secs(1_600))); // exactly 2/3 (300 left)
        assert!(needs_renewal(nb, na, Duration::from_secs(1_700))); // past 2/3 (200 left)
        assert!(needs_renewal(nb, na, Duration::from_secs(2_000))); // expired
    }

    /// End-to-end bootstrap against a mock EST server: drives cacerts -> CSR ->
    /// simpleenroll using the crate's own PKCS#7 fixtures, and asserts noded
    /// parses the responses and writes a usable PKI on disk. This covers the
    /// orchestration + parsing + atomic-write path; the TLS/trust path is
    /// covered by usg-est-client's own tests, so the mock speaks plain HTTP.
    #[tokio::test]
    async fn enroll_against_mock_writes_pki() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // Ensure a rustls crypto provider is installed before the EST client
        // builds its transport (mirrors what `main` does).
        let _ = crate::install_crypto_provider();

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.well-known/est/cacerts"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/pkcs7-mime")
                    .set_body_string(include_str!("../tests/fixtures/valid-cacerts.b64")),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/.well-known/est/simpleenroll"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/pkcs7-mime")
                    .set_body_string(include_str!("../tests/fixtures/valid-enroll.b64")),
            )
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let ca_path = dir.path().join("est-ca.pem");
        std::fs::write(&ca_path, include_str!("../tests/fixtures/est-ca.pem")).unwrap();

        let mut config = test_config(&server.uri(), None);
        config.cert_file = dir.path().join("server.crt");
        config.key_file = dir.path().join("server.key");
        config.client_ca = dir.path().join("client-ca.crt");
        config.enrollment.est.bearer_token = "test-token".to_string();
        config.enrollment.est.ca_cert_file = ca_path;

        bootstrap_enroll(&config, None).await.expect("enrollment succeeds");

        let cert = std::fs::read_to_string(&config.cert_file).unwrap();
        assert!(cert.contains("BEGIN CERTIFICATE"), "node cert written");
        let key = std::fs::read_to_string(&config.key_file).unwrap();
        assert!(key.contains("PRIVATE KEY"), "node key written");
        let ca = std::fs::read_to_string(&config.client_ca).unwrap();
        assert!(ca.contains("BEGIN CERTIFICATE"), "client CA bundle written");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&config.key_file)
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "key file must be 0600");
        }
    }

    /// End-to-end renewal against a mock EST server: the existing node identity
    /// is presented as the mTLS client credential (no bearer token) and
    /// simplereenroll rotates the leaf, replacing cert + key on disk.
    #[tokio::test]
    async fn reenroll_against_mock_rotates_identity() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let _ = crate::install_crypto_provider();

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/.well-known/est/simplereenroll"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/pkcs7-mime")
                    .set_body_string(include_str!("../tests/fixtures/valid-enroll.b64")),
            )
            .mount(&server)
            .await;

        // Seed the tempdir with the current node identity (operate on copies so
        // the fixtures are never overwritten).
        let dir = tempfile::tempdir().unwrap();
        let ca_path = dir.path().join("est-ca.pem");
        std::fs::write(&ca_path, include_str!("../tests/fixtures/est-ca.pem")).unwrap();
        let original_key = include_str!("../tests/fixtures/node-key.pem");
        std::fs::write(dir.path().join("server.crt"), include_str!("../tests/fixtures/node.pem"))
            .unwrap();
        std::fs::write(dir.path().join("server.key"), original_key).unwrap();

        let mut config = test_config(&server.uri(), None);
        config.cert_file = dir.path().join("server.crt");
        config.key_file = dir.path().join("server.key");
        config.client_ca = dir.path().join("client-ca.crt");
        config.enrollment.est.ca_cert_file = ca_path;

        reenroll(&config, None).await.expect("re-enrollment succeeds");

        let cert = std::fs::read_to_string(&config.cert_file).unwrap();
        assert!(cert.contains("BEGIN CERTIFICATE"), "rotated cert written");
        let key = std::fs::read_to_string(&config.key_file).unwrap();
        assert!(key.contains("PRIVATE KEY"), "rotated key written");
        assert_ne!(key, original_key, "renewal must generate a fresh key");
    }
}
