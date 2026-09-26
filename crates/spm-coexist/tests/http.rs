//! The minimal HTTP client against local one-shot servers (no vLLM needed).

use std::time::Duration;

use spm_coexist::http::{fetch_vllm_load, get, parse_response, FetchError, HttpError, HttpUrl};
use spm_coexist::VllmLoad;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const BODY: &str = "# TYPE vllm:num_requests_running gauge\n\
                    vllm:num_requests_running{engine=\"0\"} 1.0\n\
                    vllm:num_requests_waiting{engine=\"0\"} 0.0\n";
const TIMEOUT: Duration = Duration::from_secs(2);

/// Serves one connection: reads the request head, then writes `parts` with small pauses so the
/// client sees split reads. Returns the URL and a handle yielding the request text.
async fn serve_once(
    parts: Vec<Vec<u8>>,
    close: bool,
) -> (HttpUrl, tokio::task::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut req = Vec::new();
        let mut buf = [0u8; 1024];
        while !req.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = sock.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            req.extend_from_slice(&buf[..n]);
        }
        for p in parts {
            sock.write_all(&p).await.unwrap();
            sock.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        if !close {
            // Keep the socket open: the client must stop on Content-Length alone.
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
        String::from_utf8(req).unwrap()
    });
    let url = HttpUrl::parse(&format!("http://127.0.0.1:{port}/metrics")).unwrap();
    (url, handle)
}

fn content_length_response(body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\nserver: uvicorn\r\ncontent-length: {}\r\n\
         content-type: text/plain; version=1.0.0; charset=utf-8\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

#[tokio::test]
async fn content_length_response_split_across_reads_without_close() {
    let full = content_length_response(BODY);
    let (a, b) = full.split_at(40);
    let (url, server) = serve_once(vec![a.to_vec(), b.to_vec()], false).await;
    let started = std::time::Instant::now();
    let load = fetch_vllm_load(&url, TIMEOUT).await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(1), "waited for the close");
    assert_eq!(load, VllmLoad { running: 1.0, waiting: 0.0 });
    server.abort();
}

#[tokio::test]
async fn request_is_a_plain_get_with_host_and_close() {
    let (url, server) = serve_once(vec![content_length_response(BODY)], true).await;
    get(&url, TIMEOUT).await.unwrap();
    let req = server.await.unwrap();
    assert!(req.starts_with("GET /metrics HTTP/1.1\r\n"), "{req}");
    assert!(req.contains(&format!("Host: 127.0.0.1:{}\r\n", url.port)));
    assert!(req.contains("Connection: close\r\n"));
}

#[tokio::test]
async fn chunked_response() {
    let head = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
    let (first, second) = BODY.split_at(30);
    let chunk = |s: &str| format!("{:x};ext=1\r\n{s}\r\n", s.len()).into_bytes();
    let parts = vec![head, chunk(first), chunk(second), b"0\r\n\r\n".to_vec()];
    let (url, server) = serve_once(parts, false).await;
    let body = get(&url, TIMEOUT).await.unwrap();
    assert_eq!(body, BODY.as_bytes());
    server.abort();
}

#[tokio::test]
async fn body_until_close_without_length() {
    let resp = format!("HTTP/1.0 200 OK\r\nContent-Type: text/plain\r\n\r\n{BODY}").into_bytes();
    let (url, _server) = serve_once(vec![resp], true).await;
    assert_eq!(get(&url, TIMEOUT).await.unwrap(), BODY.as_bytes());
}

#[tokio::test]
async fn non_2xx_status_is_an_error() {
    let resp = b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 4\r\n\r\nbusy".to_vec();
    let (url, _server) = serve_once(vec![resp], true).await;
    assert!(matches!(get(&url, TIMEOUT).await, Err(HttpError::Status(503))));
}

#[tokio::test]
async fn silent_server_times_out() {
    let (url, server) = serve_once(vec![], false).await;
    let r = get(&url, Duration::from_millis(150)).await;
    assert!(matches!(r, Err(HttpError::Timeout)), "{r:?}");
    server.abort();
}

#[tokio::test]
async fn refused_connection_is_an_io_error() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let url = HttpUrl::parse(&format!("http://127.0.0.1:{port}/metrics")).unwrap();
    assert!(matches!(
        fetch_vllm_load(&url, TIMEOUT).await,
        Err(FetchError::Http(HttpError::Io(_)))
    ));
}

#[tokio::test]
async fn not_a_vllm_is_a_metrics_error() {
    let (url, _server) = serve_once(vec![content_length_response("hello\n")], true).await;
    assert!(matches!(fetch_vllm_load(&url, TIMEOUT).await, Err(FetchError::Metrics(_))));
}

#[test]
fn url_parsing() {
    let u = HttpUrl::parse("http://127.0.0.1:8001/metrics").unwrap();
    assert_eq!((u.host.as_str(), u.port, u.path.as_str()), ("127.0.0.1", 8001, "/metrics"));
    let u = HttpUrl::parse("http://localhost").unwrap();
    assert_eq!((u.host.as_str(), u.port, u.path.as_str()), ("localhost", 80, "/"));
    let u = HttpUrl::parse("http://[::1]:8001/metrics?x=1").unwrap();
    assert_eq!((u.host.as_str(), u.port, u.path.as_str()), ("::1", 8001, "/metrics?x=1"));
    for bad in
        ["https://127.0.0.1/metrics", "127.0.0.1:8001", "http://:80/", "http://h:x/", "http://u@h/"]
    {
        assert!(matches!(HttpUrl::parse(bad), Err(HttpError::BadUrl(_))), "{bad}");
    }
}

#[test]
fn response_parser_edge_cases() {
    assert!(parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nab", false)
        .unwrap()
        .is_none());
    assert!(matches!(
        parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nab", true),
        Err(HttpError::Malformed(_))
    ));
    assert!(matches!(
        parse_response(b"SSH-2.0-OpenSSH\r\n\r\n", true),
        Err(HttpError::Malformed(_))
    ));
    assert!(matches!(
        parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 999999999\r\n\r\n", false),
        Err(HttpError::TooLarge)
    ));
    let chunked_bad =
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabcX\r\n0\r\n\r\n";
    assert!(matches!(parse_response(chunked_bad, true), Err(HttpError::Malformed(_))));
    let with_trailer =
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\nX-T: 1\r\n\r\n";
    assert_eq!(parse_response(with_trailer, false).unwrap().unwrap().body, b"abc");
}
