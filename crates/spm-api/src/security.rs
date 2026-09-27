//! Local API security: the token file, browser sessions (HttpOnly cookie + CSRF value) and the
//! Host/Origin allowlist.
//!
//! * `~/.config/spark-pearl-miner/api-token`: 256 random bits, hex, mode 0600. Whoever can read it
//!   is the owner. `spark-pearl-miner gui` opens the browser with the token in the URL fragment
//!   (never sent to the server); the page exchanges it once on `POST /api/v1/session`.
//! * The session cookie (`spm_session`, HttpOnly, SameSite=Strict, Path=/) is another random 256-bit
//!   value, kept in memory only; a restart logs every browser out.
//! * Each session has its own CSRF value, returned in the JSON body. Every mutation must carry it in
//!   `X-SPM-CSRF`; a cross-site page can neither read it nor set the header without CORS.
//! * The Host header must name the loopback listener (defeats DNS rebinding) and an Origin header, if
//!   present, must be the same origin.
//! * Trusted local user (`api.trust_local_user`, on by default): a loopback connection whose peer
//!   socket belongs to the daemon's own UID (see [`crate::peer`]) may read without a cookie and gets
//!   a session from `GET /api/v1/session` without the token. Mutations still need the cookie and the
//!   CSRF header. Other accounts on the machine, and everything not on loopback, need the token.

use std::collections::HashMap;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::net::{Ipv4Addr, SocketAddr};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::peer::{self, PeerUidResolver};

/// Cookie name.
pub const COOKIE: &str = "spm_session";
/// CSRF header.
pub const CSRF_HEADER: &str = "x-spm-csrf";
/// Idle lifetime of a browser session.
pub const SESSION_IDLE: Duration = Duration::from_secs(24 * 3600);
/// Sessions kept at once (the oldest is dropped).
pub const MAX_SESSIONS: usize = 16;

/// `n` random bytes from the OS, hex encoded.
pub fn random_hex(n: usize) -> io::Result<String> {
    let mut buf = vec![0u8; n];
    getrandom::fill(&mut buf).map_err(|e| io::Error::other(e.to_string()))?;
    Ok(hex::encode(buf))
}

/// Constant-time comparison of two strings (length is not secret).
pub fn ct_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn valid_token(t: &str) -> bool {
    t.len() == 64 && t.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Read the API token, creating it (0600) when missing. A token file readable by others is
/// tightened to 0600 and reported through `warn`.
pub fn load_or_create_token(path: &Path, warn: impl Fn(&str)) -> io::Result<String> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    match fs::read_to_string(path) {
        Ok(text) => {
            let meta = fs::metadata(path)?;
            if meta.permissions().mode() & 0o077 != 0 {
                fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
                warn("the API token file was readable by other users; permissions reset to 0600");
            }
            let t = text.trim().to_string();
            if valid_token(&t) {
                return Ok(t);
            }
            warn("the API token file was malformed; a new token was written");
            write_token(path)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => write_token(path),
        Err(e) => Err(e),
    }
}

fn write_token(path: &Path) -> io::Result<String> {
    let token = random_hex(32)?;
    let tmp = path.with_extension("tmp");
    let _ = fs::remove_file(&tmp);
    let mut f = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
    f.write_all(token.as_bytes())?;
    f.write_all(b"\n")?;
    f.sync_all()?;
    fs::rename(&tmp, path)?;
    Ok(token)
}

#[derive(Debug, Clone)]
struct Session {
    csrf: String,
    last_seen: Instant,
    created: Instant,
    /// Opened for the trusted local user (no token). Evicted before token sessions when full.
    local: bool,
}

/// In-memory security state of one API server.
pub struct Security {
    token: String,
    sessions: Mutex<HashMap<String, Session>>,
    hosts: Vec<String>,
    origins: Vec<String>,
    failures: Mutex<(u32, Option<Instant>)>,
    trust_local_user: bool,
    listener: SocketAddr,
    own_uid: Option<u32>,
    peer_uid: PeerUidResolver,
}

impl fmt::Debug for Security {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Security")
            .field("hosts", &self.hosts)
            .field("trust_local_user", &self.trust_local_user)
            .field("listener", &self.listener)
            .field("own_uid", &self.own_uid)
            .finish_non_exhaustive()
    }
}

impl Security {
    /// `port` is the port the listener really bound (the Host allowlist uses it). The trusted local
    /// user is off until [`Security::trust_local_user`] turns it on.
    pub fn new(token: String, port: u16) -> Self {
        let hosts = vec![format!("127.0.0.1:{port}"), format!("localhost:{port}"), format!("[::1]:{port}")];
        let origins = hosts.iter().map(|h| format!("http://{h}")).collect();
        Security {
            token,
            sessions: Mutex::new(HashMap::new()),
            hosts,
            origins,
            failures: Mutex::new((0, None)),
            trust_local_user: false,
            listener: SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port),
            own_uid: peer::effective_uid(),
            peer_uid: Box::new(peer::proc_peer_uid),
        }
    }

    /// `api.trust_local_user`: let the same user account on this machine in without the token.
    pub fn trust_local_user(mut self, on: bool) -> Self {
        self.trust_local_user = on;
        self
    }

    /// The address the listener really bound (the peer lookup matches connections to it).
    pub fn listener(mut self, addr: SocketAddr) -> Self {
        self.listener = addr;
        self
    }

    /// Replace the peer-UID lookup (default: `/proc/net/tcp*`). For tests.
    #[doc(hidden)]
    pub fn peer_uid_resolver(mut self, f: PeerUidResolver) -> Self {
        self.peer_uid = f;
        self
    }

    /// Whether the switch is on.
    pub fn trusts_local_user(&self) -> bool {
        self.trust_local_user
    }

    /// The connection from `peer` is the daemon's own user on this machine: the switch is on, the
    /// peer is on loopback and its socket belongs to our effective UID. `None` (no peer address
    /// known) is never trusted.
    pub fn is_local_user(&self, peer: Option<SocketAddr>) -> bool {
        let (Some(peer), Some(own)) = (peer, self.own_uid) else {
            return false;
        };
        self.trust_local_user && peer::is_loopback(peer.ip()) && (self.peer_uid)(peer, self.listener) == Some(own)
    }

    /// Host header allowlist.
    pub fn host_allowed(&self, host: Option<&str>) -> bool {
        host.is_some_and(|h| self.hosts.iter().any(|a| a.eq_ignore_ascii_case(h.trim())))
    }

    /// Origin header (absent = same-origin navigation or a non-browser client).
    pub fn origin_allowed(&self, origin: Option<&str>) -> bool {
        match origin {
            None => true,
            Some(o) => self.origins.iter().any(|a| a.eq_ignore_ascii_case(o.trim())),
        }
    }

    /// Delay to apply before answering a login attempt (grows after failures).
    pub fn login_delay(&self) -> Duration {
        let f = self.failures.lock().map(|g| g.0).unwrap_or(0);
        Duration::from_millis(u64::from(f.min(10)) * 200)
    }

    /// Exchange the token for a new session: `(cookie value, csrf value)`.
    pub fn login(&self, token: &str) -> Option<(String, String)> {
        if !ct_eq(token.trim(), &self.token) {
            if let Ok(mut g) = self.failures.lock() {
                g.0 = g.0.saturating_add(1);
                g.1 = Some(Instant::now());
            }
            return None;
        }
        if let Ok(mut g) = self.failures.lock() {
            *g = (0, None);
        }
        self.open_session(false)
    }

    /// A new session. `local` marks one opened without the token (only for a connection
    /// [`Security::is_local_user`] trusts); when the table is full, local sessions go first.
    pub(crate) fn open_session(&self, local: bool) -> Option<(String, String)> {
        let id = random_hex(32).ok()?;
        let csrf = random_hex(32).ok()?;
        let now = Instant::now();
        let mut s = self.sessions.lock().ok()?;
        s.retain(|_, v| now.duration_since(v.last_seen) < SESSION_IDLE);
        while s.len() >= MAX_SESSIONS {
            // A page that keeps opening local sessions must not push out a token login.
            let victim = s
                .iter()
                .filter(|(_, v)| v.local)
                .min_by_key(|(_, v)| v.created)
                .or_else(|| s.iter().min_by_key(|(_, v)| v.created))
                .map(|(k, _)| k.clone());
            match victim {
                Some(k) => s.remove(&k),
                None => break,
            };
        }
        s.insert(id.clone(), Session { csrf: csrf.clone(), last_seen: now, created: now, local });
        Some((id, csrf))
    }

    /// The CSRF value of a live session (and refresh its idle timer).
    pub fn session_csrf(&self, cookie: &str) -> Option<String> {
        let mut s = self.sessions.lock().ok()?;
        let now = Instant::now();
        let key = s.keys().find(|k| ct_eq(k, cookie)).cloned()?;
        let entry = s.get_mut(&key)?;
        if now.duration_since(entry.last_seen) >= SESSION_IDLE {
            s.remove(&key);
            return None;
        }
        entry.last_seen = now;
        Some(entry.csrf.clone())
    }

    /// End a session.
    pub fn logout(&self, cookie: &str) {
        if let Ok(mut s) = self.sessions.lock() {
            s.retain(|k, _| !ct_eq(k, cookie));
        }
    }
}

/// The value of cookie `name` in a `Cookie:` header.
pub fn cookie_value<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    header.split(';').map(str::trim).find_map(|kv| kv.strip_prefix(name).and_then(|r| r.strip_prefix('=')))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_file_is_created_0600_and_reused() {
        let dir = std::env::temp_dir().join(format!("spm-api-token-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("api-token");
        let t1 = load_or_create_token(&path, |_| {}).unwrap();
        assert_eq!(t1.len(), 64);
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let t2 = load_or_create_token(&path, |_| {}).unwrap();
        assert_eq!(t1, t2);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let warned = std::cell::Cell::new(false);
        load_or_create_token(&path, |_| warned.set(true)).unwrap();
        assert!(warned.get());
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn local_sessions_are_evicted_before_token_logins() {
        let tok = "ab".repeat(32);
        let s = Security::new(tok.clone(), 4078);
        let logins: Vec<String> = (0..MAX_SESSIONS - 1).map(|_| s.login(&tok).unwrap().0).collect();
        let (local1, _) = s.open_session(true).unwrap();
        // Table full: the next local session pushes out the oldest LOCAL one, never a token login.
        let (local2, _) = s.open_session(true).unwrap();
        assert!(s.session_csrf(&local1).is_none());
        assert!(s.session_csrf(&local2).is_some());
        assert!(logins.iter().all(|c| s.session_csrf(c).is_some()));
        // With no local session left to evict, the oldest token login goes, as before.
        let (local3, _) = s.open_session(true).unwrap();
        assert!(s.session_csrf(&local2).is_none());
        assert!(s.session_csrf(&local3).is_some());
        let (extra, _) = s.login(&tok).unwrap();
        assert!(s.session_csrf(&local3).is_none(), "a token login evicts the local session first");
        assert!(s.session_csrf(&extra).is_some());
    }

    #[test]
    fn sessions_and_allowlists() {
        let s = Security::new("ab".repeat(32), 4078);
        assert!(s.login("wrong").is_none());
        let (cookie, csrf) = s.login(&"ab".repeat(32)).unwrap();
        assert_eq!(s.session_csrf(&cookie), Some(csrf));
        s.logout(&cookie);
        assert_eq!(s.session_csrf(&cookie), None);
        assert!(s.host_allowed(Some("127.0.0.1:4078")));
        assert!(s.host_allowed(Some("localhost:4078")));
        assert!(!s.host_allowed(Some("evil.example:4078")));
        assert!(!s.host_allowed(Some("127.0.0.1:4079")));
        assert!(!s.host_allowed(None));
        assert!(s.origin_allowed(None));
        assert!(s.origin_allowed(Some("http://localhost:4078")));
        assert!(!s.origin_allowed(Some("http://evil.example")));
        assert_eq!(cookie_value("a=1; spm_session=xyz; b=2", COOKIE), Some("xyz"));
    }
}
