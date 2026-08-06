//! TLS identity for kolorinko. Two distinct certs, on purpose:
//!
//! - **TCP bootstrap** ([`crate::web`], `https://` page load) uses the
//!   **long-lived, browser-trusted** cert from [`load_cert_key`] — mkcert by
//!   default. Browsers honor a locally-installed CA *for TCP*, so the page gets
//!   a real lock icon.
//! - **QUIC/WebTransport** ([`crate::server`]) uses a **short-lived self-signed**
//!   cert from [`wt_cert`] (see [`wt_identity`]). Browsers do NOT honor a local
//!   CA for the WebTransport QUIC handshake, so instead the client pins this
//!   cert's SHA-256 via WebTransport's `serverCertificateHashes` option — which
//!   requires the cert to be valid ≤ ~14 days. The page is told the hash
//!   ([`wt_cert_hash`]) so the wasm client can pass it to `new WebTransport`.
//!
//! This splits trust cleanly: the page is CA-trusted (lock icon), the data
//! channel is hash-pinned (no CA setup, no browser flags).

use std::{
    io,
    path::PathBuf,
    sync::{Arc, OnceLock},
};

use compio::rustls::ServerConfig;
use log::{info, warn};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};

/// `Some((cert, key))` when both PEM paths exist on disk, else `None`.
fn existing_pair(cert: &str, key: &str) -> Option<(PathBuf, PathBuf)> {
    let (cert, key) = (PathBuf::from(cert), PathBuf::from(key));
    (cert.is_file() && key.is_file()).then_some((cert, key))
}

/// Default, project-local mkcert cert pair (repo-root relative, resolved
/// against the crate so it works from any CWD). Generated once via:
/// `mkcert localhost 127.0.0.1 ::1` and renamed into `.certs/`.
fn default_mkcert_pair() -> Option<(PathBuf, PathBuf)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.certs");
    existing_pair(
        &dir.join("localhost.pem").to_string_lossy(),
        &dir.join("localhost-key.pem").to_string_lossy(),
    )
}

/// Configured browser-trusted cert+key paths, set once at startup from the
/// `.toml` config. `load_cert_key` prefers these, then the default `.certs/`
/// pair, then a self-signed fallback.
static CERT_PATHS: OnceLock<(Option<PathBuf>, Option<PathBuf>)> = OnceLock::new();

/// Set the cert+key paths from config. Called once from `main`; later calls
/// are ignored (the process has one TLS identity).
pub(crate) fn set_cert_paths(cert: Option<PathBuf>, key: Option<PathBuf>) {
    let _ = CERT_PATHS.set((cert, key));
}

/// Load the server cert + key.
pub(crate) fn load_cert_key() -> io::Result<(
    Vec<compio::rustls::pki_types::CertificateDer<'static>>,
    compio::rustls::pki_types::PrivateKeyDer<'static>,
)> {
    let (cert_cfg, key_cfg) = CERT_PATHS.get().cloned().unwrap_or((None, None));
    let paths = match (cert_cfg, key_cfg) {
        (Some(cert), Some(key)) => Some((cert, key)),
        _ => default_mkcert_pair(),
    };
    match paths {
        Some((cert_path, key_path)) => {
            let cert_pem = std::fs::read(&cert_path)?;
            let key_pem = std::fs::read(&key_path)?;
            let certs: Vec<_> = CertificateDer::pem_slice_iter(cert_pem.as_slice())
                .collect::<Result<_, _>>()
                .map_err(|e| io::Error::other(format!("parse cert PEM: {e}")))?;
            let key = PrivateKeyDer::from_pem_slice(key_pem.as_slice())
                .map_err(|e| io::Error::other(format!("parse key PEM: {e}")))?;
            // Log the leaf cert's SHA-256 so the TCP bootstrap and the QUIC
            // (H3/WT) endpoint can be confirmed to present the *same* cert —
            // both paths call this fn, so the fingerprint must match across all
            // log lines. (mkcert's leaf file carries no chain: `certs.len()` is
            // 1, the leaf signed directly by the trusted root CA.)
            if let Some(leaf) = certs.first() {
                let d = ring::digest::digest(&ring::digest::SHA256, leaf.as_ref());
                let fp: String = d.as_ref().iter().map(|b| format!("{b:02x}")).collect();
                info!(
                    "using TLS cert {}, key {} ({} cert(s) in chain, leaf sha256 = {fp})",
                    cert_path.display(),
                    key_path.display(),
                    certs.len(),
                );
            } else {
                info!(
                    "using TLS cert {}, key {} (NO certs in chain!)",
                    cert_path.display(),
                    key_path.display(),
                );
            }
            Ok((certs, key))
        }
        None => {
            warn!(
                "no TLS cert found — set [server] cert_file/key_file in the config or place the \
                 mkcert pair at .certs/{{localhost.pem,localhost-key.pem}}. Generating a \
                 self-signed cert, which browsers will refuse."
            );
            let rcgen::CertifiedKey { cert, signing_key } =
                rcgen::generate_simple_self_signed(vec!["localhost".into()])
                    .map_err(|e| io::Error::other(format!("rcgen: {e}")))?;
            let cert = cert.der().clone();
            let key_der = signing_key
                .serialize_der()
                .try_into()
                .map_err(|_| io::Error::other("rcgen: bad key DER"))?;
            Ok((vec![cert], key_der))
        }
    }
}

/// A `rustls` `ServerConfig` for the TCP TLS bootstrap (HTTPS / HTTP/1.1).
/// ALPN is `http/1.1`; the `Alt-Svc` header (sent by the application) is what
/// tells the browser to try HTTP/3 on the QUIC side.
pub(crate) fn https_server_config() -> io::Result<Arc<ServerConfig>> {
    let (certs, key) = load_cert_key()?;
    let mut cfg = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| io::Error::other(format!("rustls server config: {e}")))?;
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(cfg))
}

// ---- WebTransport self-signed identity (hash-pinned in the browser) ---------
//
// Browsers won't honor a local CA for the WebTransport QUIC handshake, so the
// client pins this cert by SHA-256 via `serverCertificateHashes`. Browsers
// require that pinned cert to be short-lived (≤ ~14 days), so it's generated
// fresh (self-signed) each server start and regenerated implicitly on restart.
// The page is told its hash ([`wt_cert_hash`]) to hand to `new WebTransport`.

/// The throwaway WebTransport identity (cert chain + key + cert hash), built
/// once per process.
struct WtIdentity {
    certs: Vec<compio::rustls::pki_types::CertificateDer<'static>>,
    key: compio::rustls::pki_types::PrivateKeyDer<'static>,
    hash: [u8; 32],
}

static WT: OnceLock<WtIdentity> = OnceLock::new();

/// The WebTransport self-signed identity, generating it on first use. Panicking
/// here is correct: a server that can't mint a WT cert can't serve the data
/// channel at all.
pub(crate) fn wt_identity() -> &'static WtIdentity {
    WT.get_or_init(|| {
        let key = rcgen::KeyPair::generate().expect("rcgen: WT keypair");
        let now = time::OffsetDateTime::now_utc();
        let mut params =
            rcgen::CertificateParams::new(vec!["localhost".to_string()]).expect("rcgen: WT params");
        // Short validity (browsers require ≤ ~14 days for `serverCertificateHashes`);
        // backdate the start slightly to tolerate any clock skew on localhost.
        params.not_before = now - time::Duration::hours(1);
        params.not_after = now + time::Duration::days(7);
        params
            .subject_alt_names
            .push(rcgen::SanType::IpAddress(std::net::IpAddr::V4(
                std::net::Ipv4Addr::new(127, 0, 0, 1),
            )));
        params
            .subject_alt_names
            .push(rcgen::SanType::IpAddress(std::net::IpAddr::V6(
                std::net::Ipv6Addr::LOCALHOST,
            )));
        let cert = params.self_signed(&key).expect("rcgen: WT self_signed");
        let der = cert.der().clone();
        let digest = ring::digest::digest(&ring::digest::SHA256, der.as_ref());
        let mut hash = [0u8; 32];
        hash.copy_from_slice(digest.as_ref());
        info!(
            "generated short-lived WebTransport cert (valid 7d); sha256 cert hash = {}",
            hash.iter()
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join("")
        );
        WtIdentity {
            certs: vec![der],
            key: key.serialize_der().try_into().expect("rcgen: WT key DER"),
            hash,
        }
    })
}

/// The self-signed cert + key presented on the QUIC/WebTransport endpoint.
pub(crate) fn wt_cert() -> (
    Vec<compio::rustls::pki_types::CertificateDer<'static>>,
    compio::rustls::pki_types::PrivateKeyDer<'static>,
) {
    let id = wt_identity();
    (id.certs.clone(), id.key.clone_key())
}

/// SHA-256 of the WebTransport leaf cert's DER — the value the browser pins
/// via `serverCertificateHashes`.
pub(crate) fn wt_cert_hash() -> &'static [u8] {
    &wt_identity().hash
}
