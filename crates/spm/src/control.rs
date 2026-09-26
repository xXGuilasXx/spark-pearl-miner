//! Control socket for the CLI: `$XDG_RUNTIME_DIR/spark-pearl-miner/control.sock` (0600 in a 0700
//! directory). One JSON request line, one JSON response line. Connections from other users are
//! refused after an `SO_PEERCRED` check (read through tokio's safe `peer_cred`).

use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use spm_api::ControlOp;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::daemon::ApiBridge;

/// A CLI request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Status,
    Pools,
    Control { op: ControlOp },
    Api,
}

pub fn bind(path: &Path) -> io::Result<UnixListener> {
    let _ = std::fs::remove_file(path);
    let l = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(l)
}

/// Serve requests until the task is aborted.
pub async fn serve(listener: UnixListener, bridge: ApiBridge, api_port: Option<u16>) {
    let owner = listener
        .local_addr()
        .ok()
        .and_then(|a| a.as_pathname().map(Path::to_path_buf))
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.uid());
    loop {
        let Ok((stream, _)) = listener.accept().await else { continue };
        if let (Ok(cred), Some(uid)) = (stream.peer_cred(), owner) {
            if cred.uid() != uid {
                tracing::warn!(peer_uid = cred.uid(), "control socket: refused a process of another user");
                continue;
            }
        }
        let b = bridge.clone();
        tokio::spawn(async move {
            let _ = handle(stream, b, api_port).await;
        });
    }
}

async fn handle(stream: UnixStream, bridge: ApiBridge, api_port: Option<u16>) -> io::Result<()> {
    let (r, mut w) = stream.into_split();
    let mut line = String::new();
    let mut r = BufReader::new(r.take(64 * 1024));
    tokio::time::timeout(Duration::from_secs(5), r.read_line(&mut line))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "request timeout"))??;
    let reply: Value = match serde_json::from_str::<Request>(line.trim()) {
        Ok(Request::Status) => json!({ "ok": true, "status": bridge.snapshot().status }),
        Ok(Request::Pools) => json!({ "ok": true, "pools": bridge.snapshot().pools }),
        Ok(Request::Api) => json!({ "ok": true, "port": api_port }),
        Ok(Request::Control { op }) => match bridge.control_op(op).await {
            Ok(m) => json!({ "ok": true, "message": m }),
            Err(e) => json!({ "ok": false, "error": e }),
        },
        Err(e) => json!({ "ok": false, "error": format!("bad request: {e}") }),
    };
    let mut out = serde_json::to_vec(&reply).unwrap_or_default();
    out.push(b'\n');
    w.write_all(&out).await?;
    w.shutdown().await
}

/// Send one request to a running daemon.
pub async fn request(path: &Path, req: &Request) -> io::Result<Value> {
    let stream = tokio::time::timeout(Duration::from_secs(3), UnixStream::connect(path))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "connect timeout"))??;
    let (r, mut w) = stream.into_split();
    let mut msg = serde_json::to_vec(req)?;
    msg.push(b'\n');
    w.write_all(&msg).await?;
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(10), BufReader::new(r).read_line(&mut line))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "response timeout"))??;
    serde_json::from_str(&line).map_err(io::Error::other)
}
