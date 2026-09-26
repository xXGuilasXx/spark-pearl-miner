//! TlsMode behaviour against local servers: pinned, public-CA (with a test root), auto fallback
//! only on protocol errors, never on certificate errors, and the per-endpoint cache.
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
use rustls::crypto::ring;
use rustls::ServerConfig;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use spm_proto::codec::{write_line, NdjsonReader};
use spm_proto::tls::{spki_sha256_b64, ConnectError, Connector, TlsCache, TlsMode, Transport, LUCKYPOOL_SPKI_SHA256_B64};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

struct TestPki {
    ca_der: CertificateDer<'static>,
    leaf_der: CertificateDer<'static>,
    leaf_key: Vec<u8>,
}

fn pki() -> TestPki {
    let ca_key = KeyPair::generate().unwrap();
    let mut ca = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca_cert = ca.self_signed(&ca_key).unwrap();
    let leaf_key = KeyPair::generate().unwrap();
    let leaf = CertificateParams::new(vec!["localhost".to_string()]).unwrap();
    let leaf_cert = leaf.signed_by(&leaf_key, &ca_cert, &ca_key).unwrap();
    TestPki { ca_der: ca_cert.der().clone(), leaf_der: leaf_cert.der().clone(), leaf_key: leaf_key.serialize_der() }
}

fn short_timeouts(c: Connector) -> Connector {
    c.with_timeouts(Duration::from_secs(2), Duration::from_secs(2))
}

/// TLS echo server: every line received is echoed back.
async fn tls_server(p: &TestPki) -> u16 {
    let cfg = ServerConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![p.leaf_der.clone()], PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(p.leaf_key.clone())))
        .unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(cfg));
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((s, _)) = l.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(s).await else { return };
                let (r, mut w) = tokio::io::split(tls);
                let mut r = NdjsonReader::new(r);
                while let Ok(Some(line)) = r.next_line().await {
                    if write_line(&mut w, &line).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    port
}

#[derive(Clone, Copy)]
enum OnHello {
    /// Answer the ClientHello like a JSON pool would, then hang up.
    JsonError,
    /// Hang up without a word.
    Close,
}

/// Plain NDJSON echo server. Counts TLS ClientHellos it sees (first byte 0x16).
async fn plain_server(on_hello: OnHello) -> (u16, Arc<AtomicUsize>) {
    let hellos = Arc::new(AtomicUsize::new(0));
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let h = hellos.clone();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            let h = h.clone();
            tokio::spawn(async move {
                let mut first = [0u8; 1];
                if s.peek(&mut first).await.unwrap_or(0) == 0 {
                    return;
                }
                if first[0] == 0x16 {
                    h.fetch_add(1, Ordering::SeqCst);
                    let mut junk = [0u8; 4096];
                    let _ = s.read(&mut junk).await;
                    if let OnHello::JsonError = on_hello {
                        let _ = s.write_all(b"{\"id\":null,\"result\":null,\"error\":\"Malformed JSON\"}\n").await;
                    }
                    return;
                }
                let (r, mut w) = s.into_split();
                let mut r = NdjsonReader::new(r);
                while let Ok(Some(line)) = r.next_line().await {
                    if write_line(&mut w, &line).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    (port, hellos)
}

async fn echo_roundtrip(c: spm_proto::tls::Connected) {
    let (r, mut w) = tokio::io::split(c.stream);
    write_line(&mut w, r#"{"ping":1}"#).await.unwrap();
    let mut r = NdjsonReader::new(r);
    let got = tokio::time::timeout(Duration::from_secs(2), r.next_line()).await.unwrap().unwrap();
    assert_eq!(got.as_deref(), Some(r#"{"ping":1}"#));
}

#[tokio::test]
async fn pinned_accepts_the_pinned_key_and_reports_it() {
    let p = pki();
    let port = tls_server(&p).await;
    let pin = spki_sha256_b64(p.leaf_der.as_ref()).unwrap();
    let c = short_timeouts(Connector::default())
        .connect("127.0.0.1", port, &TlsMode::Pinned { spki_sha256_b64: pin.clone() })
        .await
        .unwrap();
    assert_eq!(c.transport, Transport::Tls);
    assert_eq!(c.peer_spki_sha256_b64.as_deref(), Some(pin.as_str()));
    echo_roundtrip(c).await;
}

#[tokio::test]
async fn pinned_rejects_any_other_key_hard() {
    let p = pki();
    let port = tls_server(&p).await;
    let actual = spki_sha256_b64(p.leaf_der.as_ref()).unwrap();
    let e = short_timeouts(Connector::default()).connect("localhost", port, &TlsMode::luckypool()).await.unwrap_err();
    match &e {
        ConnectError::PinMismatch { expected, got, .. } => {
            assert_eq!(expected, LUCKYPOOL_SPKI_SHA256_B64);
            assert_eq!(got, &actual);
        }
        other => panic!("expected a pin mismatch, got {other:?}"),
    }
    assert!(e.is_certificate());
}

#[tokio::test]
async fn on_rejects_an_untrusted_certificate() {
    let p = pki();
    let port = tls_server(&p).await;
    let e = short_timeouts(Connector::default()).connect("localhost", port, &TlsMode::On).await.unwrap_err();
    assert!(matches!(e, ConnectError::Certificate { .. }), "{e:?}");
}

#[tokio::test]
async fn on_accepts_a_trusted_chain_with_sni() {
    let p = pki();
    let port = tls_server(&p).await;
    let conn = short_timeouts(Connector::default().with_extra_root(p.ca_der.clone()));
    let c = conn.connect("localhost", port, &TlsMode::On).await.unwrap();
    assert_eq!(c.transport, Transport::Tls);
    echo_roundtrip(c).await;
    // The certificate names "localhost", not "127.0.0.1": name checks are enforced.
    let e = conn.connect("127.0.0.1", port, &TlsMode::On).await.unwrap_err();
    assert!(matches!(e, ConnectError::Certificate { .. }), "{e:?}");
}

#[tokio::test]
async fn on_against_a_plain_server_is_a_protocol_error() {
    let (port, _) = plain_server(OnHello::JsonError).await;
    let e = short_timeouts(Connector::default()).connect("127.0.0.1", port, &TlsMode::On).await.unwrap_err();
    assert!(matches!(e, ConnectError::TlsProtocol { .. }), "{e:?}");
    assert!(!e.is_certificate());
}

#[tokio::test]
async fn auto_never_downgrades_on_a_certificate_error() {
    let p = pki();
    let port = tls_server(&p).await;
    let cache = TlsCache::default();
    let e = short_timeouts(Connector::new(cache.clone())).connect("localhost", port, &TlsMode::Auto).await.unwrap_err();
    assert!(matches!(e, ConnectError::Certificate { .. }), "{e:?}");
    assert_eq!(cache.get("localhost", port), None);
}

#[tokio::test]
async fn auto_prefers_tls_and_caches_it() {
    let p = pki();
    let port = tls_server(&p).await;
    let cache = TlsCache::default();
    let conn = short_timeouts(Connector::new(cache.clone()).with_extra_root(p.ca_der.clone()));
    let c = conn.connect("localhost", port, &TlsMode::Auto).await.unwrap();
    assert_eq!((c.transport, c.plain_fallback), (Transport::Tls, false));
    assert_eq!(cache.get("localhost", port), Some(Transport::Tls));
    echo_roundtrip(c).await;
}

#[tokio::test]
async fn auto_falls_back_to_plain_on_protocol_errors_and_caches_it() {
    for on_hello in [OnHello::JsonError, OnHello::Close] {
        let (port, hellos) = plain_server(on_hello).await;
        let cache = TlsCache::default();
        let conn = short_timeouts(Connector::new(cache.clone()));
        let c = conn.connect("127.0.0.1", port, &TlsMode::Auto).await.unwrap();
        assert_eq!((c.transport, c.plain_fallback), (Transport::Plain, true));
        assert_eq!(cache.get("127.0.0.1", port), Some(Transport::Plain));
        assert_eq!(hellos.load(Ordering::SeqCst), 1);
        echo_roundtrip(c).await;
        // Second connect goes straight to plain: no new ClientHello.
        let c = conn.connect("127.0.0.1", port, &TlsMode::Auto).await.unwrap();
        assert_eq!(c.transport, Transport::Plain);
        assert_eq!(hellos.load(Ordering::SeqCst), 1);
        echo_roundtrip(c).await;
        // Forgetting the endpoint probes TLS again.
        cache.forget("127.0.0.1", port);
        let _ = conn.connect("127.0.0.1", port, &TlsMode::Auto).await.unwrap();
        assert_eq!(hellos.load(Ordering::SeqCst), 2);
    }
}

#[tokio::test]
async fn off_is_plain_and_refused_is_a_tcp_error() {
    let (port, hellos) = plain_server(OnHello::Close).await;
    let conn = short_timeouts(Connector::default());
    let c = conn.connect("127.0.0.1", port, &TlsMode::Off).await.unwrap();
    assert_eq!((c.transport, c.plain_fallback), (Transport::Plain, false));
    echo_roundtrip(c).await;
    assert_eq!(hellos.load(Ordering::SeqCst), 0);
    // A port with no listener.
    let free = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead = free.local_addr().unwrap().port();
    drop(free);
    for mode in [TlsMode::Off, TlsMode::Auto, TlsMode::On] {
        let e = conn.connect("127.0.0.1", dead, &mode).await.unwrap_err();
        assert!(matches!(e, ConnectError::Tcp { .. }), "{mode:?}: {e:?}");
    }
}
