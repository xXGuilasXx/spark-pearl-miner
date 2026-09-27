//! The local API against a fake daemon: security (Host/Origin allowlist, cookie, CSRF, CSP, the
//! trusted local user), configuration validation (wallet, 3-pool limit, no fee keys), SSE and the
//! embedded GUI.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{header, Request, Response, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use spm_api::config::{find_fee_keys, Config, TlsSetting};
use spm_api::security::Security;
use spm_api::server::{router, webui_size, CSP, WEBUI};
use spm_api::*;
use tokio::sync::broadcast;
use tower::ServiceExt;

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const HOST: &str = "127.0.0.1:4078";
const WALLET: &str = "prl1pxtue3pmxcxjplpe6gsc57ctwv6z8t4lawq2l80wm88rqkyyc6eaqrveydh";

struct Fake {
    cfg: Mutex<Config>,
    events: broadcast::Sender<ApiEvent>,
    controls: Mutex<Vec<ControlOp>>,
}

impl Fake {
    fn new() -> Arc<Fake> {
        let mut cfg = Config::default();
        cfg.miner.wallet = WALLET.into();
        Arc::new(Fake { cfg: Mutex::new(cfg), events: broadcast::channel(16).0, controls: Mutex::new(Vec::new()) })
    }
}

impl Backend for Fake {
    fn status(&self) -> StatusView {
        StatusView { state: "mining".into(), version: "test".into(), ..Default::default() }
    }
    fn pools(&self) -> PoolsView {
        PoolsView::default()
    }
    fn fee(&self) -> FeeView {
        FeeView::constants_only()
    }
    fn gpu(&self) -> GpuView {
        GpuView::default()
    }
    fn about(&self) -> AboutView {
        AboutView::default()
    }
    fn config(&self) -> Config {
        self.cfg.lock().unwrap().clone()
    }
    fn logs(&self, _since: u64, _limit: usize) -> Vec<LogEntry> {
        vec![LogEntry { seq: 1, at_ms: 1, level: "info".into(), target: "t".into(), msg: format!("login {WALLET}.rig") }]
    }
    fn subscribe(&self) -> broadcast::Receiver<ApiEvent> {
        self.events.subscribe()
    }
    fn apply_config(&self, cfg: Config, _source: ChangeSource) -> impl std::future::Future<Output = Result<ApplyOutcome, String>> + Send {
        let changed = self.cfg.lock().unwrap().miner.wallet != cfg.miner.wallet;
        *self.cfg.lock().unwrap() = cfg;
        async move { Ok(ApplyOutcome { applied: true, wallet_changed: changed, restart_required: vec![] }) }
    }
    fn control(&self, op: ControlOp) -> impl std::future::Future<Output = Result<String, String>> + Send {
        self.controls.lock().unwrap().push(op);
        async { Ok("ok".to_string()) }
    }
}

fn app(fake: &Arc<Fake>) -> axum::Router {
    router(fake.clone(), Arc::new(Security::new(TOKEN.into(), 4078)))
}

fn req(method: &str, path: &str) -> axum::http::request::Builder {
    Request::builder().method(method).uri(path).header(header::HOST, HOST)
}

async fn body_json(r: Response<Body>) -> Value {
    let b = r.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&b).unwrap_or(Value::Null)
}

/// Log in: (cookie header value, csrf).
async fn login(app: &axum::Router) -> (String, String) {
    let r = app
        .clone()
        .oneshot(req("POST", "/api/v1/session").header(header::CONTENT_TYPE, "application/json").body(Body::from(json!({ "token": TOKEN }).to_string())).unwrap())
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let set = r.headers().get(header::SET_COOKIE).unwrap().to_str().unwrap().to_string();
    assert!(set.contains("HttpOnly") && set.contains("SameSite=Strict") && set.contains("Path=/"), "{set}");
    let cookie = set.split(';').next().unwrap().to_string();
    let csrf = body_json(r).await["csrf"].as_str().unwrap().to_string();
    (cookie, csrf)
}

async fn put_config(app: &axum::Router, cookie: &str, csrf: &str, body: Value) -> (StatusCode, Value) {
    let r = app
        .clone()
        .oneshot(
            req("PUT", "/api/v1/config")
                .header(header::COOKIE, cookie)
                .header("X-SPM-CSRF", csrf)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let st = r.status();
    (st, body_json(r).await)
}

fn valid_config_json() -> Value {
    let mut c = Config::default();
    c.miner.wallet = WALLET.into();
    c.miner.disclosure_accepted = true;
    serde_json::to_value(c).unwrap()
}

#[tokio::test]
async fn no_cookie_is_401() {
    let fake = Fake::new();
    let r = app(&fake).oneshot(req("GET", "/api/v1/status").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(r.headers().get(header::CONTENT_SECURITY_POLICY).unwrap(), CSP);
    let r = app(&fake).oneshot(req("GET", "/api/v1/config").header(header::COOKIE, "spm_session=forged").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let r = app(&fake)
        .oneshot(req("POST", "/api/v1/session").header(header::CONTENT_TYPE, "application/json").body(Body::from(r#"{"token":"nope"}"#)).unwrap())
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn foreign_host_or_origin_is_403() {
    let fake = Fake::new();
    let app = app(&fake);
    let (cookie, _) = login(&app).await;
    for host in ["evil.example:4078", "127.0.0.1:4079", "192.168.1.10:4078", "localhost"] {
        let r = app
            .clone()
            .oneshot(Request::builder().uri("/api/v1/status").header(header::HOST, host).header(header::COOKIE, &cookie).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN, "{host}");
        assert!(r.headers().contains_key(header::CONTENT_SECURITY_POLICY));
    }
    // The GUI itself is protected the same way (DNS rebinding).
    let r = app.clone().oneshot(Request::builder().uri("/").header(header::HOST, "evil.example:4078").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    let r = app
        .clone()
        .oneshot(req("GET", "/api/v1/status").header(header::COOKIE, &cookie).header(header::ORIGIN, "http://evil.example").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    let r = app
        .clone()
        .oneshot(req("GET", "/api/v1/status").header(header::COOKIE, &cookie).header(header::ORIGIN, "http://localhost:4078").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    // A missing Host header is refused too.
    let r = app.clone().oneshot(Request::builder().uri("/").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn mutations_need_the_csrf_header() {
    let fake = Fake::new();
    let app = app(&fake);
    let (cookie, csrf) = login(&app).await;
    let r = app.clone().oneshot(req("POST", "/api/v1/mining/start").header(header::COOKIE, &cookie).body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(r).await["error"], "csrf");
    let r = app
        .clone()
        .oneshot(req("POST", "/api/v1/mining/start").header(header::COOKIE, &cookie).header("X-SPM-CSRF", "0".repeat(64)).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    let (st, _) = put_config(&app, &cookie, "", valid_config_json()).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert!(fake.controls.lock().unwrap().is_empty());
    let r = app
        .clone()
        .oneshot(req("POST", "/api/v1/mining/start").header(header::COOKIE, &cookie).header("X-SPM-CSRF", &csrf).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let r = app
        .clone()
        .oneshot(req("POST", "/api/v1/pools/2/switch").header(header::COOKIE, &cookie).header("X-SPM-CSRF", &csrf).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let r = app
        .clone()
        .oneshot(
            req("POST", "/api/v1/pools/1/pin")
                .header(header::COOKIE, &cookie)
                .header("X-SPM-CSRF", &csrf)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"pinned":false}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let r = app
        .clone()
        .oneshot(req("POST", "/api/v1/pools/4/switch").header(header::COOKIE, &cookie).header("X-SPM-CSRF", &csrf).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        *fake.controls.lock().unwrap(),
        vec![ControlOp::Start, ControlOp::Switch { slot: 1 }, ControlOp::Pin { slot: None }]
    );
    // Reads do not need CSRF; the session endpoint hands it back after a reload.
    let r = app.clone().oneshot(req("GET", "/api/v1/session").header(header::COOKIE, &cookie).body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(body_json(r).await["csrf"], csrf.as_str());
}

#[tokio::test]
async fn strict_csp_on_every_response() {
    let fake = Fake::new();
    let app = app(&fake);
    for path in ["/", "/js/app.js", "/i18n/en.json", "/api/v1/status", "/nope"] {
        let r = app.clone().oneshot(req("GET", path).body(Body::empty()).unwrap()).await.unwrap();
        let h = r.headers();
        assert_eq!(h.get(header::CONTENT_SECURITY_POLICY).unwrap(), "default-src 'self'; frame-ancestors 'none'", "{path}");
        assert_eq!(h.get(header::X_CONTENT_TYPE_OPTIONS).unwrap(), "nosniff");
        assert_eq!(h.get(header::X_FRAME_OPTIONS).unwrap(), "DENY");
    }
    let r = app.clone().oneshot(req("GET", "/").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers().get(header::CONTENT_TYPE).unwrap(), "text/html; charset=utf-8");
    let html = String::from_utf8(r.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    assert!(!html.contains("<script>") && !html.contains("style="), "no inline script or style (CSP)");
    assert!(!html.contains("http://") && !html.contains("https://"), "no external resources");
}

#[tokio::test]
async fn config_validation_through_the_api() {
    let fake = Fake::new();
    let app = app(&fake);
    let (cookie, csrf) = login(&app).await;

    // Invalid address.
    let mut bad = valid_config_json();
    bad["miner"]["wallet"] = json!("prl1pxtue3pmxcxjplpe6gsc57ctwv6z8t4lawq2l80wm88rqkyyc6eaqrveydj");
    let (st, v) = put_config(&app, &cookie, &csrf, bad).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(v["fields"][0]["code"], "wallet_invalid");
    for w in ["bc1pxtue3pmxcxjplpe6gsc57ctwv6z8t4lawq2l80wm88rqkyyc6eaqrveydh", "prl1qw508d6qejxtdg4y5r3zarvary0c5xw7k", ""] {
        let mut bad = valid_config_json();
        bad["miner"]["wallet"] = json!(w);
        let (st, _) = put_config(&app, &cookie, &csrf, bad).await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{w}");
    }

    // A 4th pool.
    let mut four = valid_config_json();
    let p = four["pools"][0].clone();
    four["pools"].as_array_mut().unwrap().push(p);
    let (st, v) = put_config(&app, &cookie, &csrf, four).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(v["fields"][0]["code"], "too_many_pools");

    // Any fee-looking key, anywhere.
    for (path, key) in [("", "fee"), ("", "dev_fee"), ("miner", "fee_wallet"), ("miner", "dev_wallet"), ("api", "donation_pct"), ("worker", "devfee")] {
        let mut v = valid_config_json();
        let target = if path.is_empty() { &mut v } else { &mut v[path] };
        target.as_object_mut().unwrap().insert(key.into(), json!(0));
        let (st, body) = put_config(&app, &cookie, &csrf, v).await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{key}");
        assert_eq!(body["error"], "fee_not_configurable", "{key}");
    }
    let mut v = valid_config_json();
    v["pools"][0]["dev_pool"] = json!("evil:1");
    assert_eq!(put_config(&app, &cookie, &csrf, v).await.1["error"], "fee_not_configurable");

    // Unknown keys are refused too; bad worker names as well.
    let mut v = valid_config_json();
    v["miner"]["color"] = json!("blue");
    assert_eq!(put_config(&app, &cookie, &csrf, v).await.0, StatusCode::BAD_REQUEST);
    let mut v = valid_config_json();
    v["miner"]["worker"] = json!("rig 1");
    assert_eq!(put_config(&app, &cookie, &csrf, v).await.1["fields"][0]["code"], "worker_invalid");
    let mut v = valid_config_json();
    v["api"]["lan"] = json!(true);
    assert_eq!(put_config(&app, &cookie, &csrf, v).await.1["fields"][0]["code"], "lan_requires_tls");
    let mut v = valid_config_json();
    v["power"]["profile"] = json!("max");
    assert_eq!(put_config(&app, &cookie, &csrf, v).await.1["fields"][0]["code"], "max_not_acknowledged");

    // A valid change is applied, with its TLS mode intact.
    let mut v = valid_config_json();
    v["pools"][0]["tls"] = json!("off");
    v["pools"].as_array_mut().unwrap().truncate(2);
    let (st, out) = put_config(&app, &cookie, &csrf, v).await;
    assert_eq!(st, StatusCode::OK, "{out}");
    assert_eq!(out["applied"], true);
    let now = fake.config();
    assert_eq!(now.pools.len(), 2);
    assert_eq!(now.pools[0].tls, TlsSetting::Off);
    let r = app.clone().oneshot(req("GET", "/api/v1/config").header(header::COOKIE, &cookie).body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(body_json(r).await["pools"][0]["tls"], "off");
}

/// What the Settings dialog does: GET a fresh config, change the wallet, the worker, the language
/// and one pool row, PUT the result. The hidden settings of the rows the user did not touch
/// (Kryptex's dialect and explicit TLS, LuckyPool's pinned key) must survive, in JSON and in the
/// file.
#[tokio::test]
async fn a_settings_save_keeps_the_hidden_pool_settings() {
    let fake = Fake::new();
    let app = app(&fake);
    let (cookie, csrf) = login(&app).await;
    let r = app.clone().oneshot(req("GET", "/api/v1/config").header(header::COOKIE, &cookie).body(Body::empty()).unwrap()).await.unwrap();
    let mut cfg = body_json(r).await;
    cfg["miner"]["disclosure_accepted"] = json!(true);
    cfg["miner"]["worker"] = json!("rig-2");
    cfg["gui"]["language"] = json!("pt-BR");
    // Backup 1 retyped as a custom host:port (poolrows.js: custom defaults).
    cfg["pools"][1] = json!({
        "name": "pool.example.com", "host": "pool.example.com", "port": 3333, "tls": "auto", "spki_pin": "",
        "dialect": "auto", "jsonrpc": "auto", "proof": "auto", "password": "x", "pattern": "auto", "enabled": true
    });
    let (st, out) = put_config(&app, &cookie, &csrf, cfg).await;
    assert_eq!(st, StatusCode::OK, "{out}");
    let now = fake.config();
    let (k, custom, lucky) = (&now.pools[0], &now.pools[1], &now.pools[2]);
    assert_eq!((k.host.as_str(), k.tls, k.dialect), ("prl-br.kryptex.network", TlsSetting::On, spm_api::config::DialectSetting::Kryptex));
    assert_eq!((custom.host.as_str(), custom.port, custom.tls), ("pool.example.com", 3333, TlsSetting::Auto));
    assert_eq!((lucky.tls, lucky.spki_pin.as_str()), (TlsSetting::Pinned, "d0ehDQxaU5IUv4UHWXItQKqdJ8anqZclQXcoIjwF/mk="));
    assert_eq!(lucky.jsonrpc, spm_api::config::JsonRpcSetting::On);
    assert_eq!((now.miner.worker.as_str(), now.gui.language.as_str()), ("rig-2", "pt-BR"));
    assert_eq!(Config::from_toml(&now.to_toml()).unwrap(), now, "the commented file keeps them too");
}

#[test]
fn the_config_schema_has_no_fee_keys() {
    let v = serde_json::to_value(Config::default()).unwrap();
    assert!(find_fee_keys(&v).is_empty());
    let text = Config::default().to_toml().to_lowercase();
    for line in text.lines().filter(|l| !l.trim_start().starts_with('#')) {
        assert!(!line.contains("fee") && !line.contains("dev_"), "fee-like key in the schema: {line}");
    }
}

#[tokio::test]
async fn sse_emits_stats_and_events() {
    let fake = Fake::new();
    let app = app(&fake);
    let (cookie, _) = login(&app).await;
    let r = app.clone().oneshot(req("GET", "/api/v1/events").header(header::COOKIE, &cookie).body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers().get(header::CONTENT_TYPE).unwrap(), "text/event-stream");
    let mut body = r.into_body();
    let mut seen = String::new();
    let events = fake.events.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let _ = events.send(ApiEvent::Alert(AlertView { at_ms: 1, level: "warn".into(), msg: "pool 1: connection refused".into() }));
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !(seen.contains("event: stats") && seen.contains("event: alert")) {
        let frame = tokio::time::timeout_at(deadline, body.frame()).await.expect("SSE frames in time").unwrap().unwrap();
        if let Ok(data) = frame.into_data() {
            seen.push_str(&String::from_utf8_lossy(&data));
        }
    }
    assert!(seen.contains("\"state\":\"mining\""), "{seen}");
    assert!(seen.contains("connection refused"), "{seen}");
}

#[tokio::test]
async fn logs_can_be_exported_redacted() {
    let fake = Fake::new();
    let app = app(&fake);
    let (cookie, _) = login(&app).await;
    let r = app.clone().oneshot(req("GET", "/api/v1/logs?redact=1").header(header::COOKIE, &cookie).body(Body::empty()).unwrap()).await.unwrap();
    let v = body_json(r).await;
    assert_eq!(v[0]["msg"], "login prl1…eydh.rig");
}

#[tokio::test]
async fn fee_endpoint_is_read_only_constants() {
    let fake = Fake::new();
    let app = app(&fake);
    let (cookie, csrf) = login(&app).await;
    let r = app.clone().oneshot(req("GET", "/api/v1/fee").header(header::COOKIE, &cookie).body(Body::empty()).unwrap()).await.unwrap();
    let v = body_json(r).await;
    assert_eq!(v["constants"]["dev_wallet"], spm_fee::DEV_WALLET);
    assert_eq!(v["constants"]["fee_bps"], 200);
    assert_eq!(v["constants_hash"], spm_fee::constants_hash());
    // There is no way to write it.
    for m in ["PUT", "POST", "DELETE"] {
        let r = app
            .clone()
            .oneshot(req(m, "/api/v1/fee").header(header::COOKIE, &cookie).header("X-SPM-CSRF", &csrf).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::METHOD_NOT_ALLOWED, "{m}");
    }
}

#[tokio::test]
async fn pool_test_is_dns_tcp_tls_unless_confirmed() {
    let pool = spm_mockpool::MockPool::start(spm_mockpool::MockConfig::trivial()).await.unwrap();
    let fake = Fake::new();
    let app = app(&fake);
    let (cookie, csrf) = login(&app).await;
    let mut p = serde_json::to_value(spm_api::config::PoolEntry::new("mock", "127.0.0.1", pool.port(), TlsSetting::Off)).unwrap();
    p["dialect"] = json!("object");
    let run = |confirm: bool| {
        let app = app.clone();
        let (cookie, csrf, p) = (cookie.clone(), csrf.clone(), p.clone());
        async move {
            let r = app
                .oneshot(
                    req("POST", "/api/v1/pools/test")
                        .header(header::COOKIE, cookie)
                        .header("X-SPM-CSRF", csrf)
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(json!({ "pool": p, "confirm": confirm }).to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            body_json(r).await
        }
    };
    let v = run(false).await;
    assert_eq!(v["ok"], true, "{v}");
    let steps: Vec<&str> = v["steps"].as_array().unwrap().iter().map(|s| s["step"].as_str().unwrap()).collect();
    assert_eq!(steps, ["dns", "tcp"]);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(pool.stats().authorizes, 0, "no login without confirmation");
    let v = run(true).await;
    assert_eq!(v["ok"], true, "{v}");
    let steps: Vec<&str> = v["steps"].as_array().unwrap().iter().map(|s| s["step"].as_str().unwrap()).collect();
    assert_eq!(steps, ["dns", "tcp", "authorize", "job"]);
    assert_eq!(pool.stats().authorizes, 1);
    assert_eq!(pool.stats().submits, 0, "a test never submits");
    // A closed port.
    drop(pool);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let v = run(false).await;
    assert_eq!(v["ok"], false);
    assert_eq!(v["error_code"], "connect_refused");
}

// ----- the embedded GUI -----

fn i18n(lang: &str) -> serde_json::Map<String, Value> {
    let f = WEBUI.get_file(format!("i18n/{lang}.json")).unwrap_or_else(|| panic!("i18n/{lang}.json embedded"));
    match serde_json::from_slice::<Value>(f.contents()).expect("valid JSON") {
        Value::Object(m) => m,
        _ => panic!("i18n/{lang}.json must be an object"),
    }
}

#[test]
fn webui_is_small_and_self_contained() {
    let size = webui_size();
    assert!(size < 200 * 1024, "the GUI is {size} bytes (limit 200 KB)");
    assert!(WEBUI.get_file("index.html").is_some());
    fn walk(d: &include_dir::Dir<'_>, out: &mut Vec<(String, String)>) {
        for f in d.files() {
            out.push((f.path().display().to_string(), String::from_utf8_lossy(f.contents()).to_string()));
        }
        for sub in d.dirs() {
            walk(sub, out);
        }
    }
    let mut files = Vec::new();
    walk(&WEBUI, &mut files);
    for (path, text) in &files {
        if path.ends_with(".js") {
            assert!(!text.contains("innerHTML") && !text.contains("eval(") && !text.contains("new Function"), "{path}: unsafe DOM/JS API");
        }
        if path.ends_with(".js") || path.ends_with(".html") || path.ends_with(".css") {
            for cdn in ["https://", "http://", "//cdn", "googleapis"] {
                let hits: Vec<&str> = text.lines().filter(|l| l.contains(cdn) && !l.trim_start().starts_with("//")).collect();
                assert!(hits.is_empty(), "{path}: external reference {hits:?}");
            }
        }
    }
}

#[test]
fn i18n_files_are_valid_with_identical_keys() {
    let en = i18n("en");
    let pt = i18n("pt-BR");
    let ek: BTreeSet<&String> = en.keys().collect();
    let pk: BTreeSet<&String> = pt.keys().collect();
    assert_eq!(ek.difference(&pk).collect::<Vec<_>>(), Vec::<&&String>::new(), "missing in pt-BR");
    assert_eq!(pk.difference(&ek).collect::<Vec<_>>(), Vec::<&&String>::new(), "missing in en");
    for (k, v) in en.iter().chain(pt.iter()) {
        let s = v.as_str().unwrap_or_else(|| panic!("{k} is not a string"));
        assert!(!s.trim().is_empty(), "{k} is empty");
    }
    // Placeholders match between languages.
    let ph = |s: &str| s.split('{').skip(1).filter_map(|p| p.split('}').next()).map(str::to_string).collect::<BTreeSet<_>>();
    for k in en.keys() {
        assert_eq!(ph(en[k].as_str().unwrap()), ph(pt[k].as_str().unwrap()), "placeholders of {k}");
    }
}

#[test]
fn every_key_the_gui_uses_is_translated() {
    let en = i18n("en");
    let mut missing = Vec::new();
    for f in WEBUI.get_dir("js").unwrap().files() {
        let src = String::from_utf8_lossy(f.contents()).to_string();
        for call in ["t('", "tt('"] {
            let mut from = 0;
            while let Some(i) = src[from..].find(call).map(|i| i + from) {
                from = i + call.len();
                // A call of t()/tt(), not get('…'), put('…'), post('…').
                let before = src[..i].chars().next_back().unwrap_or(' ');
                if before.is_ascii_alphanumeric() || before == '_' || before == '.' || before == '$' {
                    continue;
                }
                let key = src[from..].split('\'').next().unwrap_or("");
                if !key.is_empty() && !en.contains_key(key) {
                    missing.push(format!("{}: {key}", f.path().display()));
                }
            }
        }
    }
    // Codes the daemon sends, translated through dynamic keys.
    for (prefix, codes) in DYNAMIC {
        for c in *codes {
            let k = format!("{prefix}.{c}");
            if !en.contains_key(&k) {
                missing.push(k);
            }
        }
    }
    assert!(missing.is_empty(), "untranslated keys: {missing:#?}");
}

/// Keys built at run time (`t(`${prefix}.${code}`)`) from codes the daemon sends.
const DYNAMIC: &[(&str, &[&str])] = &[
    ("state", &["setup_required", "stopped", "starting", "mining", "failing_over", "all_down", "paused", "offline"]),
    ("worker", &["absent", "starting", "ready", "hashing", "paused", "backoff", "waiting_external", "faulted", "unavailable"]),
    ("pause", &[
        "hardware_fault", "power_fault", "power_trip", "no_telemetry", "memory", "user", "user_stop", "yield", "update_required",
        "reject_everywhere", "health",
    ]),
    ("mgr", &["starting", "mining", "failing_over", "reconnecting", "all_down", "paused"]),
    ("trip", &["over_power", "gpu_over_temp", "acpitz_over_temp", "fault"]),
    ("mem", &["refused", "exit_low_memory", "exit_pressure", "unreadable"]),
    ("fault", &["usb_pd", "safety_mode", "thermal_cap_100w"]),
    ("pwrstate", &["off", "no_telemetry", "idle", "running", "tripped", "fault"]),
    ("pwr.profile", &["eco", "balanced", "max"]),
    ("wallet.err", &["empty", "mixed_case", "too_long", "format", "charset", "checksum", "hrp", "version", "length"]),
    ("set.row", &["0", "1", "2"]),
    ("wizard.presets", &["pools", "power", "gpu", "later"]),
    ("cfgerr", &[
        "wallet_required", "wallet_invalid", "worker_invalid", "pools_empty", "too_many_pools", "host_invalid", "port_invalid",
        "pin_invalid", "pin_unused", "password_invalid", "name_invalid", "out_of_range", "max_not_acknowledged",
        "metrics_url_invalid", "bind_not_loopback", "bind_invalid", "lan_requires_tls", "language_invalid", "schema_version",
        "parse", "fee_not_configurable",
    ]),
    ("err", &[
        "dns_failed", "dns_timeout", "connect_refused", "connect_failed", "connect_timeout", "tls_certificate", "tls_pin_mismatch",
        "tls_protocol", "tls_timeout", "tls_config", "auth_rejected", "auth_timeout", "no_job", "eof", "io", "line_too_long",
        "protocol", "closed", "reject_storm", "stale_shares", "ack_timeouts", "banned", "stall", "update_required", "host_invalid",
        "wallet_invalid",
    ]),
];

/// The GUI was cut down to the wizard, the dashboard and the Settings dialog: a key that no
/// script uses any more must go from both language files.
#[test]
fn every_translated_key_is_used() {
    let en = i18n("en");
    let mut src = String::new();
    for f in WEBUI.get_dir("js").unwrap().files() {
        src.push_str(&String::from_utf8_lossy(f.contents()));
    }
    let unused: Vec<&String> = en
        .keys()
        .filter(|k| !src.contains(&format!("'{k}'")))
        .filter(|k| !DYNAMIC.iter().any(|(prefix, codes)| codes.iter().any(|c| **k == format!("{prefix}.{c}"))))
        .collect();
    assert!(unused.is_empty(), "keys no script uses: {unused:#?}");
}

#[test]
fn the_removed_screens_are_gone() {
    for gone in ["js/pooleditor.js", "js/screens.js"] {
        assert!(WEBUI.get_file(gone).is_none(), "{gone} is still embedded");
    }
    for f in WEBUI.get_dir("js").unwrap().files() {
        let src = String::from_utf8_lossy(f.contents());
        for route in ["#/pools", "#/failover", "#/power", "#/fee", "#/logs", "#/about", "#/setup", "FAILOVER_DEFAULTS"] {
            assert!(!src.contains(route), "{}: {route}", f.path().display());
        }
    }
}

// ---- Trusted local user (api.trust_local_user) ----

/// A loopback peer as `into_make_service_with_connect_info` would report it.
fn peer() -> ConnectInfo<SocketAddr> {
    ConnectInfo("127.0.0.1:54321".parse().unwrap())
}

fn own_uid() -> u32 {
    spm_api::peer::effective_uid().expect("/proc/self/status")
}

/// A router whose peer lookup answers `uid` for every connection.
fn local_app(fake: &Arc<Fake>, trust: bool, uid: Option<u32>) -> axum::Router {
    let sec = Security::new(TOKEN.into(), 4078).trust_local_user(trust).peer_uid_resolver(Box::new(move |_, _| uid));
    router(fake.clone(), Arc::new(sec))
}

fn session_cookie(r: &Response<Body>) -> Option<String> {
    let set = r.headers().get(header::SET_COOKIE)?.to_str().ok()?;
    assert!(set.contains("HttpOnly") && set.contains("SameSite=Strict") && set.contains("Path=/") && set.contains("Max-Age=86400"), "{set}");
    Some(set.split(';').next()?.to_string())
}

#[tokio::test]
async fn trusted_local_get_session_logs_in() {
    let fake = Fake::new();
    let app = local_app(&fake, true, Some(own_uid()));
    // Without Fetch Metadata (curl, or a browser without it) no session is opened: reads work
    // without one, and a cross-site GET from an old browser must not create sessions.
    let r = app.clone().oneshot(req("GET", "/api/v1/session").extension(peer()).body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    assert!(r.headers().get(header::SET_COOKIE).is_none());
    // The GUI's own fetch says `same-origin`.
    let r = app.clone().oneshot(req("GET", "/api/v1/session").extension(peer()).header("sec-fetch-site", "same-origin").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let cookie = session_cookie(&r).expect("a session cookie");
    let csrf = body_json(r).await["csrf"].as_str().unwrap().to_string();
    assert_eq!(csrf.len(), 64);
    // The session is a normal one: with cookie + CSRF, mutations work.
    let r = app
        .clone()
        .oneshot(req("POST", "/api/v1/mining/start").extension(peer()).header(header::COOKIE, &cookie).header("X-SPM-CSRF", &csrf).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    // With a valid cookie, GET /session behaves as before: same CSRF, no new cookie.
    let r = app.clone().oneshot(req("GET", "/api/v1/session").extension(peer()).header(header::COOKIE, &cookie).body(Body::empty()).unwrap()).await.unwrap();
    assert!(r.headers().get(header::SET_COOKIE).is_none());
    assert_eq!(body_json(r).await["csrf"], csrf.as_str());
    // A stale cookie (daemon restarted) gets a fresh session.
    let r = app.clone().oneshot(req("GET", "/api/v1/session").extension(peer()).header("sec-fetch-site", "same-origin").header(header::COOKIE, "spm_session=stale").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_ne!(session_cookie(&r).unwrap(), cookie);
}

#[tokio::test]
async fn trusted_local_reads_without_a_cookie() {
    let fake = Fake::new();
    let app = local_app(&fake, true, Some(own_uid()));
    for path in ["/api/v1/status", "/api/v1/pools", "/api/v1/fee", "/api/v1/gpu", "/api/v1/about", "/api/v1/logs"] {
        let r = app.clone().oneshot(req("GET", path).extension(peer()).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK, "{path}");
        assert!(r.headers().get(header::SET_COOKIE).is_none(), "{path}");
    }
    // Typed in the address bar / fetched by the GUI itself.
    for site in ["none", "same-origin"] {
        let r = app.clone().oneshot(req("GET", "/api/v1/status").extension(peer()).header("sec-fetch-site", site).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK, "{site}");
    }
}

#[tokio::test]
async fn trusted_local_mutations_still_need_cookie_and_csrf() {
    let fake = Fake::new();
    let app = local_app(&fake, true, Some(own_uid()));
    for (m, path) in [("POST", "/api/v1/mining/start"), ("POST", "/api/v1/wallet/ack"), ("DELETE", "/api/v1/session"), ("PUT", "/api/v1/config")] {
        let r = app.clone().oneshot(req(m, path).extension(peer()).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "{m} {path}");
    }
    // A session but no CSRF header: 403, as for everyone.
    let r = app.clone().oneshot(req("GET", "/api/v1/session").extension(peer()).header("sec-fetch-site", "same-origin").body(Body::empty()).unwrap()).await.unwrap();
    let cookie = session_cookie(&r).unwrap();
    let r = app.clone().oneshot(req("POST", "/api/v1/mining/start").extension(peer()).header(header::COOKIE, &cookie).body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    let (st, _) = put_config(&app, &cookie, "wrong", valid_config_json()).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert!(fake.controls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn trusted_local_refuses_other_sites_and_proxies() {
    let fake = Fake::new();
    let app = local_app(&fake, true, Some(own_uid()));
    for path in ["/api/v1/session", "/api/v1/status"] {
        for site in ["cross-site", "same-site", "bogus"] {
            let r = app.clone().oneshot(req("GET", path).extension(peer()).header("sec-fetch-site", site).body(Body::empty()).unwrap()).await.unwrap();
            assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "{path} {site}");
            assert!(r.headers().get(header::SET_COOKIE).is_none());
        }
        for (h, v) in [("x-forwarded-for", "203.0.113.9"), ("forwarded", "for=203.0.113.9"), ("x-real-ip", "203.0.113.9")] {
            let r = app.clone().oneshot(req("GET", path).extension(peer()).header(h, v).body(Body::empty()).unwrap()).await.unwrap();
            assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "{path} {h}");
        }
        // The Host/Origin allowlist still comes first.
        let r = app.clone().oneshot(req("GET", path).extension(peer()).header(header::ORIGIN, "http://evil.example").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN, "{path}");
        let r = app.clone().oneshot(Request::builder().uri(path).header(header::HOST, "evil.example:4078").extension(peer()).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN, "{path}");
    }
}

#[tokio::test]
async fn other_users_and_unknown_peers_are_401() {
    let fake = Fake::new();
    let other = own_uid().wrapping_add(1);
    let unauthorized = |app: axum::Router, peer: Option<ConnectInfo<SocketAddr>>| async move {
        for path in ["/api/v1/session", "/api/v1/status"] {
            let mut b = req("GET", path);
            if let Some(p) = peer {
                b = b.extension(p);
            }
            let r = app.clone().oneshot(b.body(Body::empty()).unwrap()).await.unwrap();
            assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "{path}");
            assert!(r.headers().get(header::SET_COOKIE).is_none(), "{path}");
        }
    };
    // Another account on this machine.
    unauthorized(local_app(&fake, true, Some(other)), Some(peer())).await;
    // No socket found for the peer.
    unauthorized(local_app(&fake, true, None), Some(peer())).await;
    // No peer address at all (tower oneshot without ConnectInfo).
    unauthorized(local_app(&fake, true, Some(own_uid())), None).await;
    // A peer that is not on loopback, even if a lookup claimed our UID.
    unauthorized(local_app(&fake, true, Some(own_uid())), Some(ConnectInfo("100.64.0.7:40000".parse().unwrap()))).await;
    // The switch is off.
    unauthorized(local_app(&fake, false, Some(own_uid())), Some(peer())).await;
    // The token login still works for them.
    let (cookie, _) = login(&local_app(&fake, false, Some(own_uid()))).await;
    assert!(cookie.starts_with("spm_session="));
}

/// One raw HTTP/1.1 exchange over a real TCP connection; returns (status line, head, body).
async fn raw_http(port: u16, extra: &str) -> (String, String, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let request = format!("GET /api/v1/session HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{extra}Connection: close\r\n\r\n");
    s.write_all(request.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), s.read_to_end(&mut buf)).await.expect("response in time").unwrap();
    let text = String::from_utf8_lossy(&buf).to_string();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    (head.lines().next().unwrap_or("").to_string(), head.to_ascii_lowercase(), body.to_string())
}

#[tokio::test]
async fn real_tcp_same_user_gets_a_session_from_proc() {
    use spm_api::server::{bind, serve, ServeOptions};
    for trust in [true, false] {
        let (listener, port) = bind(&ServeOptions { bind: "127.0.0.1".parse().unwrap(), port: 0, lan: false }).await.unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(serve(listener, Fake::new(), TOKEN.into(), trust, async move {
            let _ = stopped.await;
        }));
        // The real /proc/net/tcp lookup: this test process is the same UID as the "daemon".
        for extra in ["Sec-Fetch-Site: same-origin\r\n", "Sec-Fetch-Site: same-origin\r\nX-Test: 1\r\n"] {
            let (status, head, body) = raw_http(port, extra).await;
            if trust {
                assert!(status.contains(" 200 "), "{status}\n{head}");
                assert!(head.contains("set-cookie: spm_session="), "{head}");
                assert!(body.contains("\"csrf\""), "{body}");
            } else {
                assert!(status.contains(" 401 "), "{status}");
                assert!(!head.contains("set-cookie"), "{head}");
            }
        }
        // Another site in the same user's browser is refused even over a real connection, and a
        // bare client without Fetch Metadata (curl) gets no session either.
        for extra in ["Sec-Fetch-Site: cross-site\r\n", ""] {
            let (status, head, _) = raw_http(port, extra).await;
            assert!(status.contains(" 401 "), "{extra:?}: {status}");
            assert!(!head.contains("set-cookie"), "{head}");
        }
        let _ = stop.send(());
        server.await.unwrap().unwrap();
    }
}
