//! The axum router: security guard, REST endpoints, SSE and the embedded GUI.

use std::convert::Infallible;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{ConnectInfo, Path, Query, Request, State};
use axum::http::header::{self, HeaderValue};
use axum::http::{Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::Stream;
use include_dir::{include_dir, Dir};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::broadcast::error::RecvError;

use crate::config::{Config, ConfigError, Strictness, MAX_POOLS};
use crate::pooltest::{self, PoolTestRequest};
use crate::security::{cookie_value, ct_eq, Security, COOKIE, CSRF_HEADER};
use crate::views::redact;
use crate::{Backend, ChangeSource, ControlOp};

/// The web GUI, embedded at build time (no build step, no CDN).
pub static WEBUI: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../../webui");

/// Exactly the policy from the architecture: nothing but this origin, never framed.
pub const CSP: &str = "default-src 'self'; frame-ancestors 'none'";

struct AppState<B> {
    backend: Arc<B>,
    sec: Arc<Security>,
}

/// Build the router (tests drive it with `tower::ServiceExt::oneshot`).
pub fn router<B: Backend>(backend: Arc<B>, sec: Arc<Security>) -> Router {
    let st = Arc::new(AppState { backend, sec });
    Router::new()
        .route("/api/v1/session", post(login::<B>).get(session_csrf::<B>).delete(logout::<B>))
        .route("/api/v1/status", get(status::<B>))
        .route("/api/v1/config", get(get_config::<B>).put(put_config::<B>))
        .route("/api/v1/pools", get(pools::<B>))
        .route("/api/v1/pools/test", post(pool_test::<B>))
        .route("/api/v1/pools/{i}/switch", post(pool_switch::<B>))
        .route("/api/v1/pools/{i}/pin", post(pool_pin::<B>))
        .route("/api/v1/fee", get(fee::<B>))
        .route("/api/v1/gpu", get(gpu::<B>))
        .route("/api/v1/about", get(about::<B>))
        .route("/api/v1/logs", get(logs::<B>))
        .route("/api/v1/events", get(events::<B>))
        .route("/api/v1/mining/{op}", post(mining::<B>))
        .route("/api/v1/wallet/ack", post(wallet_ack::<B>))
        .fallback(static_or_404)
        .layer(middleware::from_fn_with_state(st.clone(), guard::<B>))
        .with_state(st)
}

fn err(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": code, "message": message.into() }))).into_response()
}

fn is_mutation(m: &Method) -> bool {
    !matches!(*m, Method::GET | Method::HEAD | Method::OPTIONS)
}

fn header_str(req: &Request, name: impl header::AsHeaderName) -> Option<&str> {
    req.headers().get(name).and_then(|v| v.to_str().ok())
}

fn set_security_headers(resp: &mut Response, api: bool) {
    let h = resp.headers_mut();
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert("cross-origin-opener-policy", HeaderValue::from_static("same-origin"));
    h.insert("cross-origin-resource-policy", HeaderValue::from_static("same-origin"));
    if api {
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
}

/// Marks a request let through without a session because it comes from the daemon's own user on
/// this machine (`GET /api/v1/session` then opens a session without the token).
#[derive(Debug, Clone, Copy)]
struct LocalUser;

/// Host/Origin allowlist, session cookie and CSRF, then the security headers on every response.
async fn guard<B: Backend>(State(st): State<Arc<AppState<B>>>, mut req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    let api = path.starts_with("/api/");
    let mut resp = match check(&st.sec, &req, api, &path) {
        Access::Refused(refused) => refused,
        access => {
            // Only `check` decides; never keep a marker that was already on the request.
            req.extensions_mut().remove::<LocalUser>();
            if matches!(access, Access::LocalUser) {
                req.extensions_mut().insert(LocalUser);
            }
            next.run(req).await
        }
    };
    set_security_headers(&mut resp, api);
    resp
}

/// What the guard decided.
enum Access {
    /// Not an API path, the token login, or a valid session (with CSRF on mutations).
    Open,
    /// No session, but a read from the daemon's own user on this machine.
    LocalUser,
    Refused(Response),
}

fn check(sec: &Security, req: &Request, api: bool, path: &str) -> Access {
    if !sec.host_allowed(header_str(req, header::HOST)) {
        return Access::Refused(err(StatusCode::FORBIDDEN, "forbidden_host", "requests must address 127.0.0.1 or localhost on the API port"));
    }
    if !sec.origin_allowed(header_str(req, header::ORIGIN)) {
        return Access::Refused(err(StatusCode::FORBIDDEN, "forbidden_origin", "cross-origin requests are refused"));
    }
    if !api {
        return Access::Open;
    }
    let login = path == "/api/v1/session" && req.method() == Method::POST;
    if login {
        return Access::Open;
    }
    let cookie = header_str(req, header::COOKIE).and_then(|c| cookie_value(c, COOKIE));
    if let Some(csrf) = cookie.and_then(|c| sec.session_csrf(c)) {
        if is_mutation(req.method()) {
            let sent = header_str(req, CSRF_HEADER);
            if !sent.is_some_and(|s| ct_eq(s, &csrf)) {
                return Access::Refused(err(StatusCode::FORBIDDEN, "csrf", "missing or wrong X-SPM-CSRF header"));
            }
        }
        return Access::Open;
    }
    // No session. Reads (and the session bootstrap) are open to the same user on this machine;
    // mutations never are: a hostile page in that user's own browser runs under the same UID, so
    // the cookie + CSRF pair stays the only way to change anything.
    if !is_mutation(req.method()) && local_user(sec, req) {
        return Access::LocalUser;
    }
    Access::Refused(err(StatusCode::UNAUTHORIZED, "unauthorized", "log in with the API token (spark-pearl-miner gui)"))
}

/// The request comes from the daemon's own user on this machine and not from another site in
/// their browser, nor through a proxy.
fn local_user(sec: &Security, req: &Request) -> bool {
    if !sec.trusts_local_user() {
        return false;
    }
    // Browsers say where a request comes from: only the GUI itself ("same-origin") or an address
    // typed by the user ("none") qualify.
    if header_str(req, "sec-fetch-site").is_some_and(|v| !matches!(v.trim(), "same-origin" | "none")) {
        return false;
    }
    // A reverse proxy run by the same user would make every remote client look local.
    if ["forwarded", "x-forwarded-for", "x-real-ip"].iter().any(|h| req.headers().contains_key(*h)) {
        return false;
    }
    let peer = req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0);
    sec.is_local_user(peer)
}

#[derive(Deserialize)]
struct LoginBody {
    token: String,
}

async fn login<B: Backend>(State(st): State<Arc<AppState<B>>>, body: Option<Json<LoginBody>>) -> Response {
    tokio::time::sleep(st.sec.login_delay()).await;
    let Some(Json(body)) = body else {
        return err(StatusCode::BAD_REQUEST, "bad_request", "expected {\"token\": \"…\"}");
    };
    match st.sec.login(&body.token) {
        Some((cookie, csrf)) => {
            tracing::info!("API: browser session opened");
            session_opened(&cookie, &csrf)
        }
        None => {
            tracing::warn!("API: login with a wrong token");
            err(StatusCode::UNAUTHORIZED, "bad_token", "wrong API token")
        }
    }
}

/// `{"csrf"}` plus the session cookie.
fn session_opened(cookie: &str, csrf: &str) -> Response {
    let mut resp = Json(json!({ "csrf": csrf })).into_response();
    let c = format!("{COOKIE}={cookie}; HttpOnly; SameSite=Strict; Path=/; Max-Age=86400");
    if let Ok(v) = HeaderValue::from_str(&c) {
        resp.headers_mut().insert(header::SET_COOKIE, v);
    }
    resp
}

fn request_cookie(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|c| cookie_value(c, COOKIE))
        .map(str::to_string)
}

/// The CSRF value of the current session. For the trusted local user without a session, this
/// opens one exactly as a token login would, so the GUI needs no login screen on this machine.
async fn session_csrf<B: Backend>(State(st): State<Arc<AppState<B>>>, req: Request) -> Response {
    if let Some(csrf) = request_cookie(req.headers()).and_then(|c| st.sec.session_csrf(&c)) {
        return Json(json!({ "csrf": csrf })).into_response();
    }
    // Only the GUI's own fetch carries `Sec-Fetch-Site: same-origin`; a browser without Fetch
    // Metadata, or a bare `curl`, gets no session (reads work without one anyway).
    let same_origin = header_str(&req, "sec-fetch-site").is_some_and(|v| v.trim() == "same-origin");
    if same_origin && req.extensions().get::<LocalUser>().is_some() {
        if let Some((cookie, csrf)) = st.sec.open_session(true) {
            tracing::info!("API: browser session opened for the local user (same UID, no token)");
            return session_opened(&cookie, &csrf);
        }
    }
    err(StatusCode::UNAUTHORIZED, "unauthorized", "no session")
}

async fn logout<B: Backend>(State(st): State<Arc<AppState<B>>>, headers: axum::http::HeaderMap) -> Response {
    if let Some(c) = request_cookie(&headers) {
        st.sec.logout(&c);
    }
    let mut resp = Json(json!({ "ok": true })).into_response();
    let c = format!("{COOKIE}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0");
    if let Ok(v) = HeaderValue::from_str(&c) {
        resp.headers_mut().insert(header::SET_COOKIE, v);
    }
    resp
}

async fn status<B: Backend>(State(st): State<Arc<AppState<B>>>) -> Response {
    Json(st.backend.status()).into_response()
}

async fn get_config<B: Backend>(State(st): State<Arc<AppState<B>>>) -> Response {
    Json(st.backend.config()).into_response()
}

fn config_error(e: &ConfigError) -> Response {
    let code = match e {
        ConfigError::FeeKey(_) => "fee_not_configurable",
        ConfigError::Parse(_) => "parse",
        ConfigError::Invalid(_) => "invalid",
    };
    let status = match e {
        ConfigError::Parse(_) => StatusCode::BAD_REQUEST,
        _ => StatusCode::UNPROCESSABLE_ENTITY,
    };
    (status, Json(json!({ "error": code, "message": e.to_string(), "fields": e.fields() }))).into_response()
}

/// Validate a submitted configuration (also used by the daemon's CLI path).
pub fn validate_submitted(body: Value) -> Result<Config, ConfigError> {
    if let Some(n) = body.get("pools").and_then(Value::as_array).map(Vec::len) {
        if n > MAX_POOLS {
            // Checked before parsing so the message is the specific one.
            return Err(ConfigError::Invalid(vec![crate::config::FieldError {
                path: "pools".into(),
                code: "too_many_pools".into(),
                message: format!("at most {MAX_POOLS} pools (got {n})"),
            }]));
        }
    }
    let cfg = Config::from_json_value(body)?;
    cfg.validate(Strictness::Submit)?;
    Ok(cfg)
}

async fn put_config<B: Backend>(State(st): State<Arc<AppState<B>>>, body: Result<Json<Value>, axum::extract::rejection::JsonRejection>) -> Response {
    let Ok(Json(body)) = body else {
        return err(StatusCode::BAD_REQUEST, "parse", "expected a JSON configuration");
    };
    let fee_keys = crate::config::find_fee_keys(&body);
    if !fee_keys.is_empty() {
        tracing::warn!(keys = ?fee_keys, "API: refused a configuration with fee keys");
    }
    match validate_submitted(body) {
        Ok(cfg) => match st.backend.apply_config(cfg, ChangeSource::Api).await {
            Ok(outcome) => Json(outcome).into_response(),
            Err(e) => err(StatusCode::CONFLICT, "apply_failed", e),
        },
        Err(e) => config_error(&e),
    }
}

async fn pools<B: Backend>(State(st): State<Arc<AppState<B>>>) -> Response {
    Json(st.backend.pools()).into_response()
}

async fn pool_test<B: Backend>(State(st): State<Arc<AppState<B>>>, body: Result<Json<PoolTestRequest>, axum::extract::rejection::JsonRejection>) -> Response {
    let req = match body {
        Ok(Json(r)) => r,
        Err(e) => return err(StatusCode::BAD_REQUEST, "parse", e.body_text()),
    };
    let cfg = st.backend.config();
    tracing::info!(host = %req.pool.host, port = req.pool.port, authorize = req.confirm, "API: testing a pool connection");
    let r = pooltest::run(req, &cfg.miner.wallet, &cfg.miner.worker).await;
    Json(r).into_response()
}

fn slot_param(i: &str) -> Option<u8> {
    let n: u8 = i.parse().ok()?;
    (1..=MAX_POOLS as u8).contains(&n).then(|| n - 1)
}

async fn run_control<B: Backend>(st: &AppState<B>, op: ControlOp) -> Response {
    match st.backend.control(op).await {
        Ok(msg) => Json(json!({ "ok": true, "message": msg })).into_response(),
        Err(e) => err(StatusCode::CONFLICT, "refused", e),
    }
}

async fn pool_switch<B: Backend>(State(st): State<Arc<AppState<B>>>, Path(i): Path<String>) -> Response {
    match slot_param(&i) {
        Some(slot) => run_control(&st, ControlOp::Switch { slot }).await,
        None => err(StatusCode::NOT_FOUND, "no_such_pool", "pools are numbered 1 to 3"),
    }
}

#[derive(Deserialize)]
struct PinBody {
    pinned: bool,
}

async fn pool_pin<B: Backend>(State(st): State<Arc<AppState<B>>>, Path(i): Path<String>, body: Option<Json<PinBody>>) -> Response {
    let Some(slot) = slot_param(&i) else {
        return err(StatusCode::NOT_FOUND, "no_such_pool", "pools are numbered 1 to 3");
    };
    let pinned = body.map(|Json(b)| b.pinned).unwrap_or(true);
    run_control(&st, ControlOp::Pin { slot: pinned.then_some(slot) }).await
}

async fn mining<B: Backend>(State(st): State<Arc<AppState<B>>>, Path(op): Path<String>) -> Response {
    let op = match op.as_str() {
        "start" => ControlOp::Start,
        "stop" => ControlOp::Stop,
        "pause" => ControlOp::Pause,
        "resume" => ControlOp::Resume,
        _ => return err(StatusCode::NOT_FOUND, "no_such_op", "start, stop, pause or resume"),
    };
    run_control(&st, op).await
}

async fn wallet_ack<B: Backend>(State(st): State<Arc<AppState<B>>>) -> Response {
    run_control(&st, ControlOp::AckWallet).await
}

async fn fee<B: Backend>(State(st): State<Arc<AppState<B>>>) -> Response {
    Json(st.backend.fee()).into_response()
}

async fn gpu<B: Backend>(State(st): State<Arc<AppState<B>>>) -> Response {
    Json(st.backend.gpu()).into_response()
}

async fn about<B: Backend>(State(st): State<Arc<AppState<B>>>) -> Response {
    Json(st.backend.about()).into_response()
}

#[derive(Deserialize)]
struct LogQuery {
    #[serde(default)]
    since: u64,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    redact: Option<String>,
}

async fn logs<B: Backend>(State(st): State<Arc<AppState<B>>>, Query(q): Query<LogQuery>) -> Response {
    let mut lines = st.backend.logs(q.since, q.limit.unwrap_or(500).min(5000));
    if q.redact.as_deref().is_some_and(|r| r == "1" || r == "true") {
        for l in &mut lines {
            l.msg = redact(&l.msg);
        }
    }
    Json(lines).into_response()
}

/// SSE: a `stats` event every second plus every daemon event as it happens.
async fn events<B: Backend>(State(st): State<Arc<AppState<B>>>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = st.backend.subscribe();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let stream = futures_util::stream::unfold((st, rx, tick), |(st, mut rx, mut tick)| async move {
        loop {
            let ev = tokio::select! {
                _ = tick.tick() => Event::default().event("stats").json_data(st.backend.status()).ok(),
                r = rx.recv() => match r {
                    Ok(e) => Event::default().event(e.name()).json_data(&e).ok(),
                    Err(RecvError::Lagged(n)) => Event::default().event("lagged").json_data(json!({ "missed": n })).ok(),
                    Err(RecvError::Closed) => return None,
                },
            };
            if let Some(ev) = ev {
                return Some((Ok(ev), (st, rx, tick)));
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "txt" | "md" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

async fn static_or_404(req: Request) -> Response {
    let path = req.uri().path();
    if path.starts_with("/api/") {
        return err(StatusCode::NOT_FOUND, "not_found", "no such endpoint");
    }
    if req.method() != Method::GET && req.method() != Method::HEAD {
        return err(StatusCode::METHOD_NOT_ALLOWED, "method", "GET only");
    }
    let rel = path.trim_start_matches('/');
    let rel = if rel.is_empty() { "index.html" } else { rel };
    if rel.split('/').any(|seg| seg == ".." || seg.starts_with('.')) {
        return err(StatusCode::NOT_FOUND, "not_found", "no such file");
    }
    match WEBUI.get_file(rel) {
        Some(f) => {
            let mut resp = Response::new(Body::from(f.contents()));
            resp.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type(rel)));
            resp.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
            resp
        }
        None => err(StatusCode::NOT_FOUND, "not_found", "no such file"),
    }
}

/// Where and how to listen.
#[derive(Debug, Clone)]
pub struct ServeOptions {
    pub bind: IpAddr,
    /// 0 picks a free port (tests).
    pub port: u16,
    /// LAN access. Needs TLS, which is not implemented: refused.
    pub lan: bool,
}

/// Bind the listener, refusing anything but loopback.
pub async fn bind(opts: &ServeOptions) -> io::Result<(TcpListener, u16)> {
    if opts.lan || !opts.bind.is_loopback() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "LAN access to the API requires TLS, which this build does not implement; it listens on loopback only (use ssh -L 4078:127.0.0.1:4078 for remote access)",
        ));
    }
    let listener = TcpListener::bind(SocketAddr::new(opts.bind, opts.port)).await?;
    let port = listener.local_addr()?.port();
    Ok((listener, port))
}

/// Serve until `shutdown` resolves. `trust_local_user` is `api.trust_local_user`.
pub async fn serve<B: Backend>(
    listener: TcpListener,
    backend: Arc<B>,
    token: String,
    trust_local_user: bool,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> io::Result<()> {
    let local = listener.local_addr()?;
    let port = local.port();
    let sec = Arc::new(Security::new(token, port).listener(local).trust_local_user(trust_local_user));
    let app = router(backend, sec);
    tracing::info!(%port, trust_local_user, "API and GUI on http://127.0.0.1:{port}/");
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).with_graceful_shutdown(shutdown).await
}

/// Total size of the embedded GUI, in bytes.
pub fn webui_size() -> usize {
    fn walk(d: &Dir<'_>) -> usize {
        d.files().map(|f| f.contents().len()).sum::<usize>() + d.dirs().map(walk).sum::<usize>()
    }
    walk(&WEBUI)
}
