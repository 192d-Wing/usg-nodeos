use std::{
    fs::File,
    io::BufReader,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{anyhow, Context, Result};
use axum::{
    body::Body,
    extract::State,
    http::{Method, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, put},
    Extension, Json, Router,
};
use der::Decode;
use x509_cert::Certificate;
use hyper_util::{
    rt::{TokioExecutor, TokioIo},
    server::conn::auto::Builder as HyperBuilder,
    service::TowerToHyperService,
};
use rustls::{
    pki_types::{CertificateDer, PrivateKeyDer},
    server::{ClientHello, ResolvesServerCert, WebPkiClientVerifier},
    sign::{CertifiedKey, SigningKey},
    RootCertStore, ServerConfig as TlsServerConfig,
};
use serde::{Deserialize, Serialize};
use tokio::{net::TcpListener, signal};
use tokio_rustls::TlsAcceptor;
use tracing::{info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

mod est;
mod pkcs11;
mod workload;

use pkcs11::{TpmIdentity, TpmPkcs11Config};
use workload::{new_profile, WorkloadHealth, WorkloadProfile};

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Config {
    #[serde(default = "default_listen_addr")]
    listen_addr: SocketAddr,
    node_id: String,
    /// Which workload this node reconciles. Selected at build time (the image's
    /// package set + overlay) and reflected here so `noded` drives the matching
    /// `WorkloadProfile`. The hardened base (boot, identity, mTLS API) is
    /// identical across profiles; only the workload differs.
    #[serde(default)]
    profile: Profile,
    cert_file: PathBuf,
    key_file: PathBuf,
    client_ca: PathBuf,
    enrollment: EnrollmentConfig,
    #[serde(default)]
    authorization: AuthorizationConfig,
    /// Optional TPM-resident node key (tpm2-pkcs11). When present, the EST key
    /// pair is generated in and never leaves the TPM, and `keyFile` is unused.
    #[serde(default)]
    tpm: Option<TpmPkcs11Config>,
}

/// The workload layer this node runs. The base OS is workload-agnostic; the
/// profile selects what `noded` reconciles (and, at build time, which packages
/// and overlay ship in the image).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum Profile {
    /// Kubernetes node: containerd + kubelet, joined to a cluster.
    #[default]
    K8s,
    /// Bare-metal KVM/libvirt hypervisor host.
    Kvm,
}

/// Management-API roles, ordered by privilege. Higher roles inherit the
/// permissions of lower ones (see `Role::satisfies`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Role {
    Viewer,
    Operator,
    Maintainer,
    Breakglass,
}

impl Role {
    fn level(self) -> u8 {
        match self {
            Role::Viewer => 1,
            Role::Operator => 2,
            Role::Maintainer => 3,
            Role::Breakglass => 4,
        }
    }

    /// Whether holding `self` satisfies a requirement for `required`.
    fn satisfies(self, required: Role) -> bool {
        self.level() >= required.level()
    }
}

/// Authorization policy: bindings from client-certificate Common Name to role.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuthorizationConfig {
    #[serde(default)]
    roles: Vec<RoleBinding>,
}

impl AuthorizationConfig {
    /// Highest role bound to the given Common Name, if any.
    fn role_for(&self, common_name: &str) -> Option<Role> {
        self.roles
            .iter()
            .filter(|binding| binding.subjects.iter().any(|s| s == common_name))
            .map(|binding| binding.role)
            .max_by_key(|role| role.level())
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RoleBinding {
    role: Role,
    #[serde(default)]
    subjects: Vec<String>,
}

/// Authenticated client identity derived from the verified mTLS client
/// certificate, injected into each request as an extension.
#[derive(Clone, Debug, Default)]
struct ClientIdentity {
    common_name: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EnrollmentConfig {
    est: EstConfig,
    /// How often the live renewal loop checks the node certificate's expiry.
    #[serde(default = "default_renew_check_interval_secs")]
    renew_check_interval_secs: u64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EstConfig {
    server_url: String,
    bearer_token: String,
    ca_cert_file: PathBuf,
    /// Optional RFC 7030 CA label. When set it becomes the
    /// `/.well-known/est/{label}/...` path segment; when absent the unlabeled
    /// EST endpoints are used.
    #[serde(default)]
    label: Option<String>,
}

#[derive(Clone)]
struct AppState {
    config: Config,
    workload: Arc<dyn WorkloadProfile>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HealthResponse {
    status: &'static str,
    node_id: String,
    unix_time: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusResponse {
    node_id: String,
    os_version: &'static str,
    kernel_version: String,
    immutable_root: bool,
    ssh_present: bool,
    shell_present: bool,
    package_manager_present: bool,
    /// Active workload profile name ("k8s" | "kvm").
    workload_profile: String,
    /// Health of the profile's workload (containerd/kubelet or libvirtd).
    workload_health: WorkloadHealth,
}

fn main() -> Result<()> {
    init_tracing();

    install_crypto_provider()?;

    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/etc/nodeos/noded.yaml".to_string());
    let config = load_config(config_path).context("load config")?;

    // When a TPM-resident key is configured, set tpm2-pkcs11's environment (store
    // path + TCTI) *before* starting the async runtime — process environment must
    // be mutated single-threaded, and the PKCS#11 C library reads these when the
    // token is opened. If the TCTI is not honored the library falls back to a
    // default that can block on the wrong TPM device.
    if let Some(tpm) = &config.tpm {
        std::fs::create_dir_all(&tpm.store)
            .with_context(|| format!("create PKCS#11 store {}", tpm.store.display()))?;
        std::env::set_var("TPM2_PKCS11_STORE", &tpm.store);
        std::env::set_var("TPM2_PKCS11_TCTI", &tpm.tcti);
        info!(store = %tpm.store.display(), tcti = %tpm.tcti, "tpm2-pkcs11 environment configured");
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?
        .block_on(run(config))
}

async fn run(config: Config) -> Result<()> {
    // Open (provisioning on first boot) the TPM-resident node identity if
    // configured. This generates the EST key inside the token, so it must happen
    // before enrollment signs the CSR.
    let tpm: Option<Arc<TpmIdentity>> = match &config.tpm {
        Some(tpm_cfg) => Some(Arc::new(
            pkcs11::open(tpm_cfg)
                .await
                .context("open TPM-resident node key")?,
        )),
        None => None,
    };

    if !node_pki_present(&config) {
        info!(
            node_id = %config.node_id,
            tpm = tpm.is_some(),
            "node PKI not found; starting EST bootstrap enrollment"
        );
        // Retry in-process with backoff: on an IPv6-only node the interface needs
        // a moment to obtain a SLAAC address + default route after coming up, so
        // the first attempts may fail to reach the EST server. The TPM key is
        // already generated (in pkcs11::open), so retries reuse it — no
        // regeneration, and no crash-restart loop while the network settles.
        const BOOTSTRAP_ATTEMPTS: u32 = 30;
        let mut attempt = 0;
        loop {
            attempt += 1;
            match est::bootstrap_enroll(&config, tpm.as_deref()).await {
                Ok(()) => break,
                Err(err) if attempt < BOOTSTRAP_ATTEMPTS => {
                    warn!(
                        attempt,
                        max = BOOTSTRAP_ATTEMPTS,
                        error = format!("{err:#}"),
                        "bootstrap enrollment failed; retrying in 5s (network may be settling)"
                    );
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
                Err(err) => return Err(err).context("EST bootstrap enrollment"),
            }
        }
    } else {
        // PKI exists: renew via simplereenroll if the node cert is near expiry.
        // Renewal is best-effort — a failure must not prevent the node from
        // starting with its still-valid current certificate.
        match est::cert_needs_renewal(&config.cert_file) {
            Ok(true) => {
                info!("node certificate near expiry; attempting EST re-enrollment");
                // Best-effort, but retry briefly to ride out the post-boot IPv6
                // SLAAC settling window (same race as bootstrap). On failure the
                // node keeps its still-valid certificate; the renewal loop retries.
                const REENROLL_ATTEMPTS: u32 = 6;
                let mut attempt = 0;
                loop {
                    attempt += 1;
                    match est::reenroll(&config, tpm.as_deref()).await {
                        Ok(()) => break,
                        Err(err) if attempt < REENROLL_ATTEMPTS => {
                            warn!(
                                attempt,
                                max = REENROLL_ATTEMPTS,
                                error = format!("{err:#}"),
                                "EST re-enrollment failed; retrying in 5s (network may be settling)"
                            );
                            tokio::time::sleep(Duration::from_secs(5)).await;
                        }
                        Err(err) => {
                            warn!(error = %format!("{err:#}"), "EST re-enrollment failed; continuing with existing certificate");
                            break;
                        }
                    }
                }
            }
            Ok(false) => info!("node PKI present and current; skipping enrollment"),
            Err(err) => {
                warn!(error = %err, "could not evaluate certificate expiry; continuing with existing certificate")
            }
        }
    }

    // Swappable server certificate, fed by the live renewal loop below. With a
    // TPM the server signs handshakes from the token; otherwise from the key file.
    let resolver = Arc::new(match &tpm {
        Some(tpm) => ReloadableCert::from_tpm(&config.cert_file, tpm.signing_key())
            .context("load node server certificate (TPM)")?,
        None => ReloadableCert::from_files(&config.cert_file, &config.key_file)
            .context("load node server certificate")?,
    });
    let tls_config = build_server_config(&config, resolver.clone()).context("build TLS config")?;

    tokio::spawn(renewal_loop(config.clone(), resolver, tpm.clone()));

    // Bring the workload to its desired state. Best-effort in Phase 1 (the k8s
    // and kvm reconcilers are no-ops); a failure here must not stop the node from
    // serving its management API, through which an operator can intervene.
    let workload = new_profile(config.profile);
    if let Err(err) = workload.reconcile().await {
        warn!(profile = workload.name(), error = %format!("{err:#}"), "workload reconcile failed; continuing");
    }

    let app = build_router(AppState {
        config: config.clone(),
        workload,
    });

    let listener = TcpListener::bind(config.listen_addr)
        .await
        .with_context(|| format!("bind {}", config.listen_addr))?;
    let acceptor = TlsAcceptor::from(Arc::new(tls_config));

    info!(listen_addr = %config.listen_addr, "noded listening");

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, remote_addr) = accepted.context("accept TCP connection")?;
                let acceptor = acceptor.clone();
                let app = app.clone();

                tokio::spawn(async move {
                    match acceptor.accept(stream).await {
                        Ok(tls_stream) => {
                            // Bind the verified client identity to this connection
                            // so the authorization middleware can resolve its role.
                            let identity = peer_identity(tls_stream.get_ref().1);
                            let conn_app = app.layer(Extension(identity));
                            let service = TowerToHyperService::new(conn_app);
                            if let Err(err) = HyperBuilder::new(TokioExecutor::new())
                                .serve_connection(TokioIo::new(tls_stream), service)
                            .await
                            {
                                warn!(%remote_addr, error = %err, "serve TLS connection failed");
                            }
                        }
                        Err(err) => {
                            warn!(%remote_addr, error = %err, "mTLS handshake failed");
                        }
                    }
                });
            }
            _ = signal::ctrl_c() => {
                info!("shutdown requested");
                break;
            }
        }
    }

    Ok(())
}

async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        node_id: state.config.node_id,
        unix_time: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or_default(),
    })
}

async fn status(State(state): State<AppState>) -> Json<StatusResponse> {
    let workload_health = state.workload.health().await;
    Json(StatusResponse {
        node_id: state.config.node_id,
        os_version: "dev",
        kernel_version: std::env::consts::OS.to_string(),
        immutable_root: false,
        ssh_present: path_exists("/usr/sbin/sshd") || path_exists("/usr/bin/ssh"),
        shell_present: path_exists("/bin/sh") || path_exists("/bin/bash"),
        package_manager_present: path_exists("/usr/bin/apt")
            || path_exists("/usr/bin/dnf")
            || path_exists("/usr/bin/yum")
            || path_exists("/usr/bin/apk"),
        workload_profile: state.workload.name().to_string(),
        workload_health,
    })
}

/// Monotonic per-process request identifier for audit correlation.
static REQUEST_ID: AtomicU64 = AtomicU64::new(1);

/// Build the management API router with the authorization + audit middleware.
fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/v1/healthz", get(health))
        .route("/v1/status", get(status))
        .route("/v1/config", put(put_config))
        .layer(middleware::from_fn_with_state(state.clone(), authorize))
        .with_state(state)
}

async fn put_config() -> impl IntoResponse {
    // Authorization is enforced by the `authorize` middleware; the mutation
    // logic itself is a separate follow-up.
    (
        StatusCode::NOT_IMPLEMENTED,
        "configuration mutation is authorized but not yet implemented",
    )
}

/// The minimum role required for a request, or `None` for endpoints that need
/// only a valid client certificate (mTLS). Unknown routes default to the most
/// privileged role (deny-by-default).
fn required_role(method: &Method, path: &str) -> Option<Role> {
    match (method, path) {
        (&Method::GET, "/v1/healthz") => None,
        (&Method::GET, "/v1/status") => Some(Role::Viewer),
        (&Method::PUT, "/v1/config") => Some(Role::Operator),
        _ => Some(Role::Breakglass),
    }
}

/// Authorize the request against the caller's certificate-derived role and emit
/// an audit event for the decision.
async fn authorize(State(state): State<AppState>, request: Request<Body>, next: Next) -> Response {
    let request_id = REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    let method = request.method().clone();
    let path = request.uri().path().to_string();

    let identity = request
        .extensions()
        .get::<ClientIdentity>()
        .cloned()
        .unwrap_or_default();
    let subject = identity.common_name.clone();
    let granted = subject
        .as_deref()
        .and_then(|cn| state.config.authorization.role_for(cn));
    let required = required_role(&method, &path);

    let (allowed, reason) = match required {
        None => (true, "no role required"),
        Some(req) => match granted {
            Some(role) if role.satisfies(req) => (true, "authorized"),
            Some(_) => (false, "insufficient role"),
            None => (false, "no role assigned to subject"),
        },
    };

    info!(
        target: "audit",
        request_id,
        subject = subject.as_deref().unwrap_or("<none>"),
        method = %method,
        resource = %path,
        required = ?required,
        granted = ?granted,
        decision = if allowed { "allow" } else { "deny" },
        reason,
        "authorization decision"
    );

    if allowed {
        next.run(request).await
    } else {
        (StatusCode::FORBIDDEN, "forbidden: insufficient authorization").into_response()
    }
}

/// Derive the client identity from a connection's verified peer certificate.
fn peer_identity(conn: &rustls::ServerConnection) -> ClientIdentity {
    let common_name = conn
        .peer_certificates()
        .and_then(|certs| certs.first())
        .and_then(|cert_der| Certificate::from_der(cert_der.as_ref()).ok())
        .and_then(|cert| subject_common_name(&cert));
    ClientIdentity { common_name }
}

/// Extract the Common Name (OID 2.5.4.3) from a certificate's subject.
fn subject_common_name(cert: &Certificate) -> Option<String> {
    for atv in cert.tbs_certificate().subject().iter() {
        if atv.oid.to_string() == "2.5.4.3" {
            if let Ok(name) = std::str::from_utf8(atv.value.value()) {
                return Some(name.to_string());
            }
        }
    }
    None
}

fn load_config(path: impl AsRef<Path>) -> Result<Config> {
    let path = path.as_ref();
    let data =
        std::fs::read_to_string(path).with_context(|| format!("read config {}", path.display()))?;
    let is_yaml = matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some("yaml" | "yml")
    );
    parse_config(&data, is_yaml)
}

fn parse_config(data: &str, is_yaml: bool) -> Result<Config> {
    let config: Config = if is_yaml {
        yaml_serde::from_str(data).context("parse YAML config")?
    } else {
        toml::from_str(data).context("parse TOML config")?
    };

    if !config.listen_addr.is_ipv6() {
        return Err(anyhow!(
            "listenAddr must be IPv6-only; use an IPv6 literal such as [::1]:9443"
        ));
    }

    if config.node_id.trim().is_empty() {
        return Err(anyhow!("nodeId is required"));
    }

    validate_est_config(&config.enrollment.est)?;

    Ok(config)
}

fn validate_est_config(config: &EstConfig) -> Result<()> {
    if !config.server_url.starts_with("https://") {
        return Err(anyhow!("enrollment.est.serverUrl must use https"));
    }

    if config.bearer_token.trim().is_empty() {
        return Err(anyhow!("enrollment.est.bearerToken is required"));
    }

    if config.ca_cert_file.as_os_str().is_empty() {
        return Err(anyhow!("enrollment.est.caCertFile is required"));
    }

    Ok(())
}

/// The node has usable PKI when the certificate, the management CA bundle, and
/// the private key are present. With a TPM the private key lives in the token (no
/// key file), so only the certificate and CA bundle are checked. Missing files
/// trigger bootstrap EST enrollment; present-but-invalid files surface later as a
/// TLS load error rather than silently clobbering operator material.
fn node_pki_present(config: &Config) -> bool {
    let key_present = config.tpm.is_some() || config.key_file.exists();
    config.cert_file.exists() && key_present && config.client_ca.exists()
}

/// Where the server's signing key comes from: a PEM key file, or a TPM-resident
/// key behind a rustls PKCS#11 signer (stable across renewals — only the
/// certificate rotates).
enum KeySource {
    File(PathBuf),
    Tpm(Arc<dyn SigningKey>),
}

impl std::fmt::Debug for KeySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeySource::File(path) => f.debug_tuple("File").field(path).finish(),
            KeySource::Tpm(_) => f.write_str("Tpm(..)"),
        }
    }
}

/// A rustls server-certificate resolver whose certificate can be swapped at
/// runtime. The renewal loop installs a freshly issued node certificate here, so
/// new TLS handshakes use it without restarting the server. Established
/// connections keep the certificate they handshook with.
#[derive(Debug)]
struct ReloadableCert {
    cert_path: PathBuf,
    key_source: KeySource,
    current: RwLock<Arc<CertifiedKey>>,
}

impl ReloadableCert {
    fn new(cert_path: PathBuf, key_source: KeySource) -> Result<Self> {
        let certified = Arc::new(build_certified_key(&cert_path, &key_source)?);
        Ok(Self {
            cert_path,
            key_source,
            current: RwLock::new(certified),
        })
    }

    fn from_files(cert_path: &Path, key_path: &Path) -> Result<Self> {
        Self::new(cert_path.to_path_buf(), KeySource::File(key_path.to_path_buf()))
    }

    /// Build a resolver served by a TPM-resident key (the signing key persists
    /// across renewals; only the certificate is reloaded).
    fn from_tpm(cert_path: &Path, signing_key: Arc<dyn SigningKey>) -> Result<Self> {
        Self::new(cert_path.to_path_buf(), KeySource::Tpm(signing_key))
    }

    /// Atomically replace the served certificate with the one now on disk,
    /// reusing the configured key source.
    fn reload(&self) -> Result<()> {
        let certified = Arc::new(build_certified_key(&self.cert_path, &self.key_source)?);
        *self
            .current
            .write()
            .map_err(|_| anyhow!("reloadable certificate lock poisoned"))? = certified;
        Ok(())
    }

    /// DER of the currently served leaf certificate (for tests).
    #[cfg(test)]
    fn leaf_der(&self) -> Vec<u8> {
        self.current.read().unwrap().cert[0].as_ref().to_vec()
    }
}

impl ResolvesServerCert for ReloadableCert {
    fn resolve(&self, _client_hello: ClientHello) -> Option<Arc<CertifiedKey>> {
        self.current.read().ok().map(|guard| guard.clone())
    }
}

/// Load the certificate chain and pair it with the configured signing key (a PEM
/// key via the installed crypto provider, or the TPM-resident rustls signer).
fn build_certified_key(cert_path: &Path, key_source: &KeySource) -> Result<CertifiedKey> {
    let certs = load_certs(cert_path).context("load server certificate")?;
    let signing_key = match key_source {
        KeySource::File(key_path) => {
            let key = load_key(key_path).context("load server private key")?;
            let provider = rustls::crypto::CryptoProvider::get_default()
                .ok_or_else(|| anyhow!("no rustls crypto provider installed"))?;
            provider
                .key_provider
                .load_private_key(key)
                .map_err(|err| anyhow!("load signing key: {err}"))?
        }
        KeySource::Tpm(signing_key) => signing_key.clone(),
    };
    Ok(CertifiedKey::new(certs, signing_key))
}

/// Build the mTLS server config: a fixed client-certificate verifier (the
/// management CA bundle) plus a swappable server-certificate resolver.
fn build_server_config(
    config: &Config,
    resolver: Arc<ReloadableCert>,
) -> Result<TlsServerConfig> {
    let client_roots = load_client_roots(&config.client_ca).context("load client CA")?;

    Ok(TlsServerConfig::builder()
        .with_client_cert_verifier(
            WebPkiClientVerifier::builder(Arc::new(client_roots))
                .build()
                .context("build client cert verifier")?,
        )
        .with_cert_resolver(resolver))
}

/// Background loop: periodically renew the node certificate before it expires
/// and hot-swap the rotated certificate into the running server. Best-effort —
/// failures are logged and retried on the next tick; they never take the server
/// down.
async fn renewal_loop(
    config: Config,
    resolver: Arc<ReloadableCert>,
    tpm: Option<Arc<TpmIdentity>>,
) {
    let interval = Duration::from_secs(config.enrollment.renew_check_interval_secs.max(1));
    loop {
        tokio::time::sleep(interval).await;

        match est::cert_needs_renewal(&config.cert_file) {
            Ok(true) => {
                info!("renewal loop: node certificate near expiry; re-enrolling");
                if let Err(err) = est::reenroll(&config, tpm.as_deref()).await {
                    warn!(error = %err, "renewal loop: re-enrollment failed; will retry next tick");
                    continue;
                }
                match resolver.reload() {
                    Ok(()) => info!("renewal loop: rotated certificate is now served"),
                    Err(err) => {
                        warn!(error = %err, "renewal loop: failed to load rotated certificate")
                    }
                }
            }
            Ok(false) => {}
            Err(err) => warn!(error = %err, "renewal loop: could not evaluate certificate expiry"),
        }
    }
}

fn load_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let mut reader = BufReader::new(File::open(path)?);
    rustls_pemfile::certs(&mut reader)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn load_key(path: &Path) -> Result<PrivateKeyDer<'static>> {
    let mut reader = BufReader::new(File::open(path)?);
    rustls_pemfile::private_key(&mut reader)?
        .ok_or_else(|| anyhow!("no private key found in {}", path.display()))
}

fn load_client_roots(path: &Path) -> Result<RootCertStore> {
    let mut roots = RootCertStore::empty();
    for cert in load_certs(path)? {
        roots
            .add(cert)
            .map_err(|err| anyhow!("invalid client CA certificate: {err}"))?;
    }

    if roots.is_empty() {
        return Err(anyhow!("client CA bundle contains no certificates"));
    }

    Ok(roots)
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().json())
        .init();
}

/// Install the process-wide rustls crypto provider, shared by noded's mTLS
/// server and the EST client's HTTPS transport.
///
/// With the default `fips` feature this installs the aws-lc-rs FIPS module and
/// fails closed: it errors unless the binary is actually linked against the FIPS
/// module, so a misconfigured build cannot silently fall back to non-validated
/// crypto. Without `fips` it installs the standard aws-lc-rs provider.
#[cfg(feature = "fips")]
fn install_crypto_provider() -> Result<()> {
    usg_est_client::fips_tls::install_fips_provider()
        .map_err(|err| anyhow!("install FIPS crypto provider: {err}"))?;
    info!("FIPS crypto provider active (aws-lc-rs FIPS module)");
    Ok(())
}

#[cfg(not(feature = "fips"))]
fn install_crypto_provider() -> Result<()> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    warn!("FIPS feature disabled; using non-validated aws-lc-rs crypto provider");
    Ok(())
}

fn path_exists(path: &str) -> bool {
    Path::new(path).exists()
}

fn default_listen_addr() -> SocketAddr {
    "[::1]:9443"
        .parse()
        .expect("default listen address is valid")
}

/// Default renewal-check cadence: hourly. The renewal trigger itself is
/// expiry-based (two-thirds of lifetime), so this only bounds detection latency.
fn default_renew_check_interval_secs() -> u64 {
    3600
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_YAML: &str = r#"
listenAddr: "[::1]:9443"
nodeId: qemu-node-001
certFile: /etc/nodeos/pki/server.crt
keyFile: /etc/nodeos/pki/server.key
clientCa: /etc/nodeos/pki/client-ca.crt
enrollment:
  est:
    serverUrl: https://[2001:db8::10]
    bearerToken: bootstrap-token
    caCertFile: /etc/nodeos/pki/est-ca.crt
"#;

    #[test]
    fn parses_minimal_yaml_without_label() {
        let config = parse_config(SAMPLE_YAML, true).expect("parse");
        assert_eq!(config.node_id, "qemu-node-001");
        assert!(config.listen_addr.is_ipv6());
        assert_eq!(config.enrollment.est.server_url, "https://[2001:db8::10]");
        assert!(config.enrollment.est.label.is_none());
    }

    #[test]
    fn parses_optional_label() {
        let yaml = format!("{SAMPLE_YAML}    label: issuing-ca\n");
        let config = parse_config(&yaml, true).expect("parse");
        assert_eq!(config.enrollment.est.label.as_deref(), Some("issuing-ca"));
    }

    #[test]
    fn profile_defaults_to_k8s_and_parses_explicit_kvm() {
        // Omitted `profile` defaults to k8s (the namesake), so existing images
        // keep their behavior without touching their config.
        let config = parse_config(SAMPLE_YAML, true).expect("parse");
        assert_eq!(config.profile, Profile::K8s);
        assert_eq!(new_profile(config.profile).name(), "k8s");

        let kvm_yaml = format!("profile: kvm\n{SAMPLE_YAML}");
        let kvm = parse_config(&kvm_yaml, true).expect("parse");
        assert_eq!(kvm.profile, Profile::Kvm);
        assert_eq!(new_profile(kvm.profile).name(), "kvm");
    }

    #[test]
    fn rejects_ipv4_listen_addr() {
        let yaml = SAMPLE_YAML.replace("[::1]:9443", "127.0.0.1:9443");
        let err = parse_config(&yaml, true).expect_err("should reject IPv4");
        assert!(err.to_string().contains("IPv6"));
    }

    #[test]
    fn rejects_non_https_est_url() {
        let yaml = SAMPLE_YAML.replace("https://[2001:db8::10]", "http://[2001:db8::10]");
        let err = parse_config(&yaml, true).expect_err("should reject http");
        assert!(err.to_string().contains("https"));
    }

    /// Under the default `fips` feature the installed crypto provider must be the
    /// aws-lc-rs FIPS module at runtime (proves the binary is FIPS-linked, not
    /// just that it compiled).
    #[cfg(feature = "fips")]
    #[test]
    fn fips_provider_is_active() {
        install_crypto_provider().expect("install FIPS crypto provider");
        assert!(usg_est_client::fips_tls::is_fips_active());
    }

    /// The server's certificate resolver swaps the served leaf when reloaded —
    /// this is what lets the renewal loop rotate the cert without a restart.
    #[test]
    fn reloadable_cert_hot_swaps_leaf() {
        install_crypto_provider().expect("crypto provider");

        let dir = tempfile::tempdir().unwrap();
        let cert = dir.path().join("server.crt");
        let key = dir.path().join("server.key");
        std::fs::write(&cert, include_str!("../tests/fixtures/node.pem")).unwrap();
        std::fs::write(&key, include_str!("../tests/fixtures/node-key.pem")).unwrap();

        let resolver = ReloadableCert::from_files(&cert, &key).expect("initial load");
        let before = resolver.leaf_der();

        std::fs::write(&cert, include_str!("../tests/fixtures/server.pem")).unwrap();
        std::fs::write(&key, include_str!("../tests/fixtures/server-key.pem")).unwrap();
        resolver.reload().expect("reload");

        assert_ne!(before, resolver.leaf_der(), "served leaf must change after reload");
    }

    #[test]
    fn role_hierarchy_is_inclusive() {
        assert!(Role::Breakglass.satisfies(Role::Viewer));
        assert!(Role::Maintainer.satisfies(Role::Operator));
        assert!(Role::Operator.satisfies(Role::Operator));
        assert!(!Role::Viewer.satisfies(Role::Operator));
    }

    #[test]
    fn role_for_picks_highest_binding() {
        let cfg = AuthorizationConfig {
            roles: vec![
                RoleBinding { role: Role::Viewer, subjects: vec!["a".into()] },
                RoleBinding { role: Role::Maintainer, subjects: vec!["a".into()] },
            ],
        };
        assert_eq!(cfg.role_for("a"), Some(Role::Maintainer));
        assert_eq!(cfg.role_for("unbound"), None);
    }

    #[test]
    fn required_role_per_route() {
        assert_eq!(required_role(&Method::GET, "/v1/healthz"), None);
        assert_eq!(required_role(&Method::GET, "/v1/status"), Some(Role::Viewer));
        assert_eq!(required_role(&Method::PUT, "/v1/config"), Some(Role::Operator));
        // deny-by-default for unknown routes
        assert_eq!(required_role(&Method::GET, "/v1/unknown"), Some(Role::Breakglass));
    }

    #[test]
    fn extracts_common_name_from_cert() {
        use der::DecodePem;
        let cert = Certificate::from_pem(include_bytes!("../tests/fixtures/node.pem")).unwrap();
        assert!(subject_common_name(&cert).is_some());
    }

    /// End-to-end authorization through the router: role gating per route and the
    /// 403 (deny) vs handler (allow) outcomes.
    #[tokio::test]
    async fn authorization_enforced_end_to_end() {
        use axum::body::Body;
        use axum::http::{Method, Request};
        use tower::ServiceExt;

        let mut config = parse_config(SAMPLE_YAML, true).unwrap();
        config.authorization = AuthorizationConfig {
            roles: vec![
                RoleBinding { role: Role::Viewer, subjects: vec!["viewer-id".into()] },
                RoleBinding { role: Role::Operator, subjects: vec!["operator-id".into()] },
            ],
        };
        let workload = new_profile(config.profile);
        let state = AppState { config, workload };

        async fn status_for(
            state: &AppState,
            method: Method,
            uri: &str,
            cn: Option<&str>,
        ) -> StatusCode {
            let mut req = Request::builder()
                .method(method)
                .uri(uri)
                .body(Body::empty())
                .unwrap();
            req.extensions_mut().insert(ClientIdentity {
                common_name: cn.map(String::from),
            });
            build_router(state.clone())
                .oneshot(req)
                .await
                .unwrap()
                .status()
        }

        // healthz: any authenticated client, no role required.
        assert_eq!(
            status_for(&state, Method::GET, "/v1/healthz", None).await,
            StatusCode::OK
        );
        // status: viewer allowed; unbound subject denied.
        assert_eq!(
            status_for(&state, Method::GET, "/v1/status", Some("viewer-id")).await,
            StatusCode::OK
        );
        assert_eq!(
            status_for(&state, Method::GET, "/v1/status", Some("nobody")).await,
            StatusCode::FORBIDDEN
        );
        // config: viewer insufficient (403); operator passes authz and reaches
        // the not-yet-implemented handler (501).
        assert_eq!(
            status_for(&state, Method::PUT, "/v1/config", Some("viewer-id")).await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            status_for(&state, Method::PUT, "/v1/config", Some("operator-id")).await,
            StatusCode::NOT_IMPLEMENTED
        );
    }
}
