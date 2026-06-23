//! Optional TPM-resident node identity via tpm2-pkcs11.
//!
//! When `tpm` is configured, the node's EST key pair is generated **inside** the
//! TPM and never touches disk: the CSR is signed by the token, the mTLS server
//! serves with a token-backed [`rustls`] signer, and EST `simplereenroll`
//! authenticates with a token-backed client certificate. The PKCS#11 store and
//! the token PIN live on the LUKS+PCR-sealed state volume, so the key is usable
//! only after an untampered measured boot.
//!
//! The image ships `libtpm2_pkcs11.so` but no Python/`tpm2_ptool`, and the
//! crate's [`Pkcs11KeyProvider::new`] assumes an *already-initialized* token, so
//! first-boot provisioning is done here directly over the PKCS#11 API
//! (`C_InitToken` + `C_InitPIN`).

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{anyhow, Context, Result};
use cryptoki::context::{CInitializeArgs, CInitializeFlags, Pkcs11};
use cryptoki::session::UserType;
use cryptoki::types::AuthPin;
use rand::Rng;
use rustls::client::ResolvesClientCert;
use rustls::pki_types::CertificateDer;
use rustls::sign::SigningKey;
use serde::Deserialize;
use tracing::info;
use usg_est_client::hsm::{
    KeyAlgorithm, KeyHandle, KeyProvider, Pkcs11ClientCertResolver, Pkcs11KeyProvider,
    Pkcs11SigningKey,
};

/// Configuration for a TPM-resident node key (the `tpm` block in noded.yaml).
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TpmPkcs11Config {
    /// Path to the tpm2-pkcs11 module.
    #[serde(default = "default_library")]
    pub library: PathBuf,
    /// tpm2-pkcs11 token store; must be on the encrypted state volume.
    #[serde(default = "default_store")]
    pub store: PathBuf,
    /// Token label set at `C_InitToken`.
    #[serde(default = "default_token_label")]
    pub token_label: String,
    /// CKA_LABEL of the node key pair within the token.
    #[serde(default = "default_key_label")]
    pub key_label: String,
    /// File holding the token user PIN (0600, on the encrypted volume). Its
    /// presence also marks the token as already provisioned.
    #[serde(default = "default_pin_file")]
    pub pin_file: PathBuf,
    /// TCTI for tpm2-pkcs11 (kernel resource manager by default).
    #[serde(default = "default_tcti")]
    pub tcti: String,
}

fn default_library() -> PathBuf {
    PathBuf::from("/usr/lib/libtpm2_pkcs11.so")
}
fn default_store() -> PathBuf {
    PathBuf::from("/var/lib/nodeos/pkcs11")
}
fn default_token_label() -> String {
    "nodeos".to_string()
}
fn default_key_label() -> String {
    "nodeos-est-key".to_string()
}
fn default_pin_file() -> PathBuf {
    PathBuf::from("/var/lib/nodeos/pki/pkcs11.pin")
}
fn default_tcti() -> String {
    "device:/dev/tpmrm0".to_string()
}

/// An opened TPM-resident node identity: the PKCS#11 provider plus the handle to
/// the node key. Cheap to clone-share via `Arc`.
pub(crate) struct TpmIdentity {
    provider: Arc<Pkcs11KeyProvider>,
    handle: KeyHandle,
}

impl TpmIdentity {
    /// Borrow the key provider (for CSR signing via `HsmCsrBuilder`).
    pub fn provider(&self) -> &Pkcs11KeyProvider {
        &self.provider
    }

    /// The node key handle.
    pub fn handle(&self) -> &KeyHandle {
        &self.handle
    }

    /// A rustls signing key for the mTLS *server* (key stays in the token).
    pub fn signing_key(&self) -> Arc<dyn SigningKey> {
        Arc::new(Pkcs11SigningKey::new(self.provider.clone(), self.handle.clone()))
    }

    /// A rustls client-cert resolver for EST `simplereenroll` mutual TLS,
    /// presenting `cert_chain` (the node's current certificate, leaf first).
    pub fn client_resolver(
        &self,
        cert_chain: Vec<CertificateDer<'static>>,
    ) -> Arc<dyn ResolvesClientCert> {
        Arc::new(Pkcs11ClientCertResolver::new(
            self.provider.clone(),
            self.handle.clone(),
            cert_chain,
        ))
    }
}

/// Open (provisioning on first boot) the TPM-resident node identity.
///
/// On first boot the token is initialized over the PKCS#11 API and the node key
/// pair is generated inside it; on later boots the persisted token + PIN (on the
/// sealed volume) are reused.
pub(crate) async fn open(cfg: &TpmPkcs11Config) -> Result<TpmIdentity> {
    // TPM2_PKCS11_STORE / TPM2_PKCS11_TCTI and the store directory are configured
    // by `main` before the async runtime starts (env must be mutated
    // single-threaded), so they are already in effect here.
    let user_pin = if cfg.pin_file.exists() {
        info!("tpm2-pkcs11 token already provisioned; opening");
        read_pin(&cfg.pin_file)?
    } else {
        info!(store = %cfg.store.display(), "provisioning tpm2-pkcs11 token (first boot)");
        provision_token(cfg)?
    };

    // Open the crate provider against the now-initialized token.
    info!(library = %cfg.library.display(), "opening tpm2-pkcs11 provider");
    let provider = Arc::new(
        Pkcs11KeyProvider::new(&cfg.library, None, &user_pin)
            .map_err(|e| anyhow!("open PKCS#11 token: {e}"))?,
    );

    let handle = ensure_key(&provider, &cfg.key_label).await?;
    info!(key_label = %cfg.key_label, "TPM-resident node key ready");

    Ok(TpmIdentity { provider, handle })
}

/// Initialize a fresh tpm2-pkcs11 token over the PKCS#11 API (no `tpm2_ptool`):
/// `C_InitToken` (SO PIN) then `C_InitPIN` (user PIN). Returns the user PIN and
/// persists it (0600) on the encrypted volume.
///
/// The library is finalized (the `Pkcs11` is dropped) before the caller reopens
/// it through [`Pkcs11KeyProvider::new`], so the two initializations don't clash.
fn provision_token(cfg: &TpmPkcs11Config) -> Result<String> {
    let so_pin = random_pin();
    let user_pin = random_pin();

    let pkcs11 = Pkcs11::new(&cfg.library)
        .map_err(|e| anyhow!("load PKCS#11 module {}: {e}", cfg.library.display()))?;
    pkcs11
        .initialize(CInitializeArgs::new(CInitializeFlags::OS_LOCKING_OK))
        .map_err(|e| anyhow!("C_Initialize: {e}"))?;
    info!("provision: PKCS#11 library initialized");

    // tpm2-pkcs11 exposes an uninitialized token as a slot-with-token.
    let mut slots = pkcs11
        .get_slots_with_token()
        .map_err(|e| anyhow!("get slots with token: {e}"))?;
    if slots.is_empty() {
        slots = pkcs11
            .get_all_slots()
            .map_err(|e| anyhow!("get all slots: {e}"))?;
    }
    let slot = *slots
        .first()
        .ok_or_else(|| anyhow!("no PKCS#11 slot available to initialize"))?;
    info!(slot = slot.id(), "provision: initializing token");

    let so = AuthPin::new(so_pin.into_boxed_str());
    let user = AuthPin::new(user_pin.clone().into_boxed_str());

    pkcs11
        .init_token(slot, &so, &cfg.token_label)
        .map_err(|e| anyhow!("C_InitToken: {e}"))?;
    info!("provision: C_InitToken complete");
    let session = pkcs11
        .open_rw_session(slot)
        .map_err(|e| anyhow!("open rw session: {e}"))?;
    session
        .login(UserType::So, Some(&so))
        .map_err(|e| anyhow!("login SO: {e}"))?;
    session
        .init_pin(&user)
        .map_err(|e| anyhow!("C_InitPIN: {e}"))?;
    session.logout().map_err(|e| anyhow!("logout SO: {e}"))?;
    info!("provision: C_InitPIN complete");

    // Persist the user PIN before the provider reopens the token. The SO PIN is
    // intentionally discarded — re-administration re-provisions a wiped store.
    write_pin(&cfg.pin_file, &user_pin)?;

    // Finalize this library handle so the provider can initialize cleanly.
    drop(session);
    drop(pkcs11);

    Ok(user_pin)
}

/// Find the node key in the token by label, or generate a non-extractable
/// EC P-256 key pair if it does not yet exist.
async fn ensure_key(provider: &Pkcs11KeyProvider, label: &str) -> Result<KeyHandle> {
    if let Some(handle) = provider
        .find_key(label)
        .await
        .map_err(|e| anyhow!("look up token key: {e}"))?
    {
        info!(key_label = %label, "reusing existing TPM node key");
        return Ok(handle);
    }

    info!(key_label = %label, "generating EC P-256 node key in TPM");
    provider
        .generate_key_pair(KeyAlgorithm::EcdsaP256, Some(label))
        .await
        .map_err(|e| anyhow!("generate token key: {e}"))
}

/// Generate a 24-character alphanumeric PIN.
fn random_pin() -> String {
    let mut rng = rand::rng();
    (0..24)
        .map(|_| {
            const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
            CHARSET[rng.random_range(0..CHARSET.len())] as char
        })
        .collect()
}

fn read_pin(path: &Path) -> Result<String> {
    let pin = std::fs::read_to_string(path)
        .with_context(|| format!("read token PIN {}", path.display()))?;
    Ok(pin.trim().to_string())
}

/// Write the token PIN with 0600 permissions (it sits on the encrypted volume).
fn write_pin(path: &Path, pin: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create PIN directory {}", parent.display()))?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(path)
        .with_context(|| format!("create PIN file {}", path.display()))?;
    use std::io::Write as _;
    file.write_all(pin.as_bytes())
        .with_context(|| format!("write PIN file {}", path.display()))?;
    file.sync_all().ok();
    Ok(())
}

/// Parse a PEM certificate file into a DER chain (leaf first) for the client
/// resolver / server `CertifiedKey`.
pub(crate) fn cert_chain_from_pem(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let mut reader = std::io::BufReader::new(
        std::fs::File::open(path).with_context(|| format!("open certificate {}", path.display()))?,
    );
    let chain = rustls_pemfile::certs(&mut reader)
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("parse certificates {}", path.display()))?;
    if chain.is_empty() {
        return Err(anyhow!("no certificates in {}", path.display()));
    }
    Ok(chain)
}
