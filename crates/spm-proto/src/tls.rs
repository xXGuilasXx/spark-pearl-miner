//! Transport: plain TCP or TLS (rustls, ring provider, webpki roots, SNI), per pool `TlsMode`.
//!
//! - `Off`: plain TCP.
//! - `On`: TLS with public-CA verification; any failure is an error.
//! - `Auto`: TLS first. Only a TLS *protocol* failure (the peer does not speak TLS: garbage
//!   record, EOF or silence during the handshake) falls back to plain TCP. A *certificate*
//!   failure is never downgraded. The outcome is cached per endpoint (host, port).
//! - `Pinned`: TLS where the only check is SHA-256(SubjectPublicKeyInfo) of the server's
//!   end-entity certificate against a base64 pin (LuckyPool's self-signed `*.luckypool.io`).
//!   Handshake signatures are still verified, so the peer must hold the pinned key.
use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{ring, verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::{CertificateError, ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use rustls_pki_types::{CertificateDer, ServerName, UnixTime};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

/// LuckyPool's self-signed `CN=*.luckypool.io` key (captured 2026-09-26, cert valid to 2036-02-05).
pub const LUCKYPOOL_SPKI_SHA256_B64: &str = "d0ehDQxaU5IUv4UHWXItQKqdJ8anqZclQXcoIjwF/mk=";

pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TlsMode {
    Off,
    On,
    #[default]
    Auto,
    Pinned { spki_sha256_b64: String },
}

impl TlsMode {
    pub fn luckypool() -> Self {
        TlsMode::Pinned { spki_sha256_b64: LUCKYPOOL_SPKI_SHA256_B64.to_string() }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    Plain,
    Tls,
}

/// A bidirectional byte stream to a pool (plain TCP or TLS over TCP).
pub trait PoolStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> PoolStream for T {}

pub struct Connected {
    pub stream: Box<dyn PoolStream>,
    pub transport: Transport,
    /// SHA-256 of the server's SubjectPublicKeyInfo, base64 (TLS only).
    pub peer_spki_sha256_b64: Option<String>,
    /// `Auto` ended in plain TCP (either now or from the per-endpoint cache).
    pub plain_fallback: bool,
}

impl std::fmt::Debug for Connected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connected")
            .field("transport", &self.transport)
            .field("peer_spki_sha256_b64", &self.peer_spki_sha256_b64)
            .field("plain_fallback", &self.plain_fallback)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error("connect to {endpoint}: {source}")]
    Tcp { endpoint: String, source: io::Error },
    #[error("connect to {endpoint}: timed out")]
    Timeout { endpoint: String },
    #[error("TLS certificate of {endpoint} rejected: {reason}")]
    Certificate { endpoint: String, reason: String },
    #[error("TLS key of {endpoint} does not match the pin (expected {expected}, got {got}); the pool changed its key or someone is in the middle")]
    PinMismatch { endpoint: String, expected: String, got: String },
    #[error("TLS handshake with {endpoint} failed: {reason}")]
    TlsProtocol { endpoint: String, reason: String },
    #[error("invalid server name {0:?}")]
    ServerName(String),
    #[error("TLS configuration: {0}")]
    Config(String),
}

impl ConnectError {
    /// Certificate problems (including a pin mismatch) are never retried in plain text.
    pub fn is_certificate(&self) -> bool {
        matches!(self, ConnectError::Certificate { .. } | ConnectError::PinMismatch { .. })
    }
}

/// Per-endpoint memory of what `Auto` found. Clone-able handle; share one per process.
#[derive(Debug, Clone, Default)]
pub struct TlsCache {
    inner: Arc<Mutex<HashMap<(String, u16), Transport>>>,
}

impl TlsCache {
    fn key(host: &str, port: u16) -> (String, u16) {
        (host.to_ascii_lowercase(), port)
    }
    pub fn get(&self, host: &str, port: u16) -> Option<Transport> {
        self.inner.lock().ok().and_then(|m| m.get(&Self::key(host, port)).copied())
    }
    pub fn set(&self, host: &str, port: u16, t: Transport) {
        if let Ok(mut m) = self.inner.lock() {
            m.insert(Self::key(host, port), t);
        }
    }
    /// Forget an endpoint so the next `Auto` connect probes TLS again.
    pub fn forget(&self, host: &str, port: u16) {
        if let Ok(mut m) = self.inner.lock() {
            m.remove(&Self::key(host, port));
        }
    }
}

enum Verify {
    WebPki,
    Pin([u8; 32]),
}

enum TlsAttemptError {
    Connect(ConnectError),
    Certificate(String),
    PinMismatch { got: String },
    /// The peer does not speak TLS (or not a version/suite we accept).
    Protocol(String),
}

/// Opens pool connections according to a `TlsMode`.
#[derive(Debug, Clone)]
pub struct Connector {
    cache: TlsCache,
    extra_roots: Vec<CertificateDer<'static>>,
    connect_timeout: Duration,
    handshake_timeout: Duration,
}

impl Default for Connector {
    fn default() -> Self {
        Self::new(TlsCache::default())
    }
}

impl Connector {
    pub fn new(cache: TlsCache) -> Self {
        Connector {
            cache,
            extra_roots: Vec::new(),
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
        }
    }

    /// Trust one more root for `On`/`Auto` (tests, private pools).
    pub fn with_extra_root(mut self, der: CertificateDer<'static>) -> Self {
        self.extra_roots.push(der);
        self
    }

    pub fn with_timeouts(mut self, connect: Duration, handshake: Duration) -> Self {
        self.connect_timeout = connect;
        self.handshake_timeout = handshake;
        self
    }

    pub fn cache(&self) -> &TlsCache {
        &self.cache
    }

    pub async fn connect(&self, host: &str, port: u16, mode: &TlsMode) -> Result<Connected, ConnectError> {
        let endpoint = format!("{host}:{port}");
        match mode {
            TlsMode::Off => self.plain(host, port, &endpoint, false).await,
            TlsMode::On => self.tls(host, port, &endpoint, Verify::WebPki).await.map_err(|e| e.into_error(&endpoint, None)),
            TlsMode::Pinned { spki_sha256_b64 } => {
                let pin = decode_pin(spki_sha256_b64)?;
                self.tls(host, port, &endpoint, Verify::Pin(pin))
                    .await
                    .map_err(|e| e.into_error(&endpoint, Some(spki_sha256_b64)))
            }
            TlsMode::Auto => {
                if self.cache.get(host, port) == Some(Transport::Plain) {
                    return self.plain(host, port, &endpoint, true).await;
                }
                match self.tls(host, port, &endpoint, Verify::WebPki).await {
                    Ok(c) => {
                        self.cache.set(host, port, Transport::Tls);
                        Ok(c)
                    }
                    Err(TlsAttemptError::Protocol(reason)) => {
                        tracing::info!(%endpoint, %reason, "TLS not spoken here; falling back to plain TCP");
                        let c = self.plain(host, port, &endpoint, true).await?;
                        self.cache.set(host, port, Transport::Plain);
                        Ok(c)
                    }
                    Err(e) => Err(e.into_error(&endpoint, None)),
                }
            }
        }
    }

    async fn tcp(&self, host: &str, port: u16, endpoint: &str) -> Result<TcpStream, ConnectError> {
        let s = tokio::time::timeout(self.connect_timeout, TcpStream::connect((host, port)))
            .await
            .map_err(|_| ConnectError::Timeout { endpoint: endpoint.to_string() })?
            .map_err(|source| ConnectError::Tcp { endpoint: endpoint.to_string(), source })?;
        // Latency matters more than packet count for submits.
        let _ = s.set_nodelay(true);
        Ok(s)
    }

    async fn plain(&self, host: &str, port: u16, endpoint: &str, plain_fallback: bool) -> Result<Connected, ConnectError> {
        let s = self.tcp(host, port, endpoint).await?;
        Ok(Connected { stream: Box::new(s), transport: Transport::Plain, peer_spki_sha256_b64: None, plain_fallback })
    }

    fn client_config(&self, verify: &Verify, seen: &Arc<Mutex<Option<String>>>) -> Result<Arc<ClientConfig>, ConnectError> {
        let provider = Arc::new(ring::default_provider());
        let builder = ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .map_err(|e| ConnectError::Config(e.to_string()))?;
        let cfg = match verify {
            Verify::WebPki => {
                let mut roots = RootCertStore::empty();
                roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
                for r in &self.extra_roots {
                    roots.add(r.clone()).map_err(|e| ConnectError::Config(e.to_string()))?;
                }
                builder.with_root_certificates(roots).with_no_client_auth()
            }
            Verify::Pin(pin) => builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(SpkiPinVerifier { pin: *pin, provider, seen: seen.clone() }))
                .with_no_client_auth(),
        };
        Ok(Arc::new(cfg))
    }

    async fn tls(&self, host: &str, port: u16, endpoint: &str, verify: Verify) -> Result<Connected, TlsAttemptError> {
        let seen = Arc::new(Mutex::new(None));
        let cfg = self.client_config(&verify, &seen).map_err(TlsAttemptError::Connect)?;
        let name = ServerName::try_from(host.to_string()).map_err(|_| TlsAttemptError::Connect(ConnectError::ServerName(host.to_string())))?;
        let tcp = self.tcp(host, port, endpoint).await.map_err(TlsAttemptError::Connect)?;
        let hs = tokio::time::timeout(self.handshake_timeout, TlsConnector::from(cfg).connect(name, tcp)).await;
        match hs {
            Err(_) => Err(TlsAttemptError::Protocol("handshake timed out".into())),
            Ok(Err(e)) => {
                let observed = seen.lock().ok().and_then(|s| s.clone());
                Err(match (classify(&e), &verify, observed) {
                    (HandshakeFailure::Certificate(_), Verify::Pin(_), Some(got)) => TlsAttemptError::PinMismatch { got },
                    (HandshakeFailure::Certificate(r), _, _) => TlsAttemptError::Certificate(r),
                    (HandshakeFailure::Protocol(r), _, _) => TlsAttemptError::Protocol(r),
                })
            }
            Ok(Ok(stream)) => {
                let spki = stream
                    .get_ref()
                    .1
                    .peer_certificates()
                    .and_then(|c| c.first())
                    .and_then(|c| spki_sha256_b64(c.as_ref()));
                Ok(Connected { stream: Box::new(stream), transport: Transport::Tls, peer_spki_sha256_b64: spki, plain_fallback: false })
            }
        }
    }
}

impl TlsAttemptError {
    fn into_error(self, endpoint: &str, pin: Option<&str>) -> ConnectError {
        let endpoint = endpoint.to_string();
        match self {
            TlsAttemptError::Connect(e) => e,
            TlsAttemptError::Certificate(reason) => ConnectError::Certificate { endpoint, reason },
            TlsAttemptError::PinMismatch { got } => {
                ConnectError::PinMismatch { endpoint, expected: pin.unwrap_or_default().to_string(), got }
            }
            TlsAttemptError::Protocol(reason) => ConnectError::TlsProtocol { endpoint, reason },
        }
    }
}

enum HandshakeFailure {
    Certificate(String),
    Protocol(String),
}

fn classify(e: &io::Error) -> HandshakeFailure {
    if let Some(r) = e.get_ref().and_then(|i| i.downcast_ref::<rustls::Error>()) {
        return match r {
            rustls::Error::InvalidCertificate(_)
            | rustls::Error::NoCertificatesPresented
            | rustls::Error::InvalidCertRevocationList(_)
            | rustls::Error::UnsupportedNameType => HandshakeFailure::Certificate(r.to_string()),
            other => HandshakeFailure::Protocol(other.to_string()),
        };
    }
    // EOF/reset while handshaking: the peer hung up on our ClientHello.
    HandshakeFailure::Protocol(e.to_string())
}

fn decode_pin(b64: &str) -> Result<[u8; 32], ConnectError> {
    STANDARD
        .decode(b64.trim())
        .ok()
        .and_then(|v| <[u8; 32]>::try_from(v).ok())
        .ok_or_else(|| ConnectError::Config(format!("SPKI pin {b64:?} is not base64 of 32 bytes")))
}

/// Accepts exactly the server key whose SPKI hashes to `pin`; ignores CA chain, name and dates.
#[derive(Debug)]
struct SpkiPinVerifier {
    pin: [u8; 32],
    provider: Arc<CryptoProvider>,
    seen: Arc<Mutex<Option<String>>>,
}

impl ServerCertVerifier for SpkiPinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let spki = spki_der(end_entity.as_ref()).ok_or(rustls::Error::InvalidCertificate(CertificateError::BadEncoding))?;
        let digest: [u8; 32] = Sha256::digest(spki).into();
        if let Ok(mut s) = self.seen.lock() {
            *s = Some(STANDARD.encode(digest));
        }
        if digest == self.pin {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(CertificateError::ApplicationVerificationFailure))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// base64(SHA-256(SubjectPublicKeyInfo DER)) of an X.509 certificate, the usual "SPKI pin".
pub fn spki_sha256_b64(cert_der: &[u8]) -> Option<String> {
    spki_der(cert_der).map(|spki| STANDARD.encode(Sha256::digest(spki)))
}

/// The complete DER encoding (tag, length and value) of the certificate's SubjectPublicKeyInfo.
///
/// Certificate ::= SEQUENCE { tbsCertificate SEQUENCE { [0] version OPTIONAL, serialNumber,
/// signature, issuer, validity, subject, subjectPublicKeyInfo, ... }, ... }
pub fn spki_der(cert: &[u8]) -> Option<&[u8]> {
    const SEQUENCE: u8 = 0x30;
    let cert = Tlv::parse(cert).filter(|t| t.tag == SEQUENCE)?;
    let tbs = Tlv::parse(cert.value).filter(|t| t.tag == SEQUENCE)?;
    let mut rest = tbs.value;
    let first = Tlv::parse(rest)?;
    if first.tag == 0xa0 {
        rest = first.rest; // explicit [0] version
    }
    // serialNumber INTEGER, signature SEQUENCE, issuer SEQUENCE, validity SEQUENCE, subject SEQUENCE
    for expected in [0x02, SEQUENCE, SEQUENCE, SEQUENCE, SEQUENCE] {
        rest = Tlv::parse(rest).filter(|t| t.tag == expected)?.rest;
    }
    Tlv::parse(rest).filter(|t| t.tag == SEQUENCE).map(|t| t.whole)
}

/// One DER TLV. Definite lengths only, low tag numbers only.
struct Tlv<'a> {
    tag: u8,
    value: &'a [u8],
    /// Tag, length and value.
    whole: &'a [u8],
    /// Bytes after this TLV.
    rest: &'a [u8],
}

impl<'a> Tlv<'a> {
    fn parse(input: &'a [u8]) -> Option<Tlv<'a>> {
        let (&tag, after_tag) = input.split_first()?;
        if tag & 0x1f == 0x1f {
            return None;
        }
        let (&first, mut after_len) = after_tag.split_first()?;
        let len = if first < 0x80 {
            first as usize
        } else {
            let n = (first & 0x7f) as usize;
            if n == 0 || n > 4 || after_len.len() < n {
                return None;
            }
            let (len_bytes, after) = after_len.split_at(n);
            after_len = after;
            len_bytes.iter().fold(0usize, |acc, &b| (acc << 8) | b as usize)
        };
        if after_len.len() < len {
            return None;
        }
        let header = input.len() - after_len.len();
        let (value, rest) = after_len.split_at(len);
        Some(Tlv { tag, value, whole: &input[..header + len], rest })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spki_matches_the_key_pair_that_signed_the_certificate() {
        for _ in 0..3 {
            let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
            let der = ck.cert.der();
            let spki = spki_der(der.as_ref()).expect("spki");
            assert_eq!(spki, &ck.key_pair.public_key_der()[..]);
            let expect = STANDARD.encode(Sha256::digest(ck.key_pair.public_key_der()));
            assert_eq!(spki_sha256_b64(der.as_ref()), Some(expect));
        }
    }

    #[test]
    fn der_parser_refuses_truncated_or_garbage_input() {
        let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let der = ck.cert.der().as_ref().to_vec();
        for cut in [0, 1, 2, 10, der.len() / 2, der.len() - 1] {
            assert_eq!(spki_der(&der[..cut]), None, "cut at {cut}");
        }
        assert_eq!(spki_der(b"\x30\x84\xff\xff\xff\xff"), None);
        assert_eq!(spki_der(b"\x30\x80\x00\x00"), None);
        assert_eq!(spki_der(b"hello"), None);
    }

    #[test]
    fn luckypool_pin_is_32_bytes_and_bad_pins_are_config_errors() {
        assert_eq!(decode_pin(LUCKYPOOL_SPKI_SHA256_B64).unwrap().len(), 32);
        assert!(matches!(decode_pin("AAAA"), Err(ConnectError::Config(_))));
        assert!(matches!(decode_pin("not base64"), Err(ConnectError::Config(_))));
        assert_eq!(TlsMode::luckypool(), TlsMode::Pinned { spki_sha256_b64: LUCKYPOOL_SPKI_SHA256_B64.into() });
    }

    #[test]
    fn tls_mode_serde_shape() {
        assert_eq!(serde_json::to_string(&TlsMode::Auto).unwrap(), "\"auto\"");
        let p: TlsMode = serde_json::from_str(r#"{"pinned":{"spki_sha256_b64":"x"}}"#).unwrap();
        assert_eq!(p, TlsMode::Pinned { spki_sha256_b64: "x".into() });
    }

    #[test]
    fn cache_is_case_insensitive_and_forgettable() {
        let c = TlsCache::default();
        c.set("Pool.Example", 1200, Transport::Plain);
        assert_eq!(c.get("pool.example", 1200), Some(Transport::Plain));
        assert_eq!(c.get("pool.example", 1201), None);
        c.forget("POOL.example", 1200);
        assert_eq!(c.get("pool.example", 1200), None);
    }
}
