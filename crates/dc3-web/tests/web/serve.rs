//! `serve()` on a real loopback socket.

use std::net::SocketAddr;
use std::time::Duration;

use dc3_store::PublicStats;
use dc3_web::{WebDeps, WebError, serve};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::common::*;

const CONNECT_ATTEMPTS: usize = 200;
const CONNECT_PAUSE: Duration = Duration::from_millis(10);
const STOP_TIMEOUT: Duration = Duration::from_secs(30);

fn free_addr() -> SocketAddr {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    probe.local_addr().unwrap()
}

async fn wait_for(addr: SocketAddr) {
    for _ in 0..CONNECT_ATTEMPTS {
        if TcpStream::connect(addr).await.is_ok() {
            return;
        }
        tokio::time::sleep(CONNECT_PAUSE).await;
    }
    panic!("server did not start listening on {addr}");
}

struct RawResponse {
    status: u16,
    head: String,
    body: String,
}

/// A minimal HTTP/1.1 GET over a fresh connection.
async fn http_get(addr: SocketAddr, path: &str, extra_headers: &str) -> RawResponse {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n{extra_headers}\r\n"
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8(raw).unwrap();
    let (head, body) = text.split_once("\r\n\r\n").unwrap();
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    RawResponse {
        status,
        head: head.to_ascii_lowercase(),
        body: body.to_owned(),
    }
}

#[tokio::test]
async fn serve_loads_statistics_serves_and_shuts_down() {
    let _serial = serial().await;
    metrics();
    let torrents = vec![torrent(1, "served torrent", &[("a.txt", 1)])];
    let index = index_of(&torrents);
    let backend = FakeBackend::with(torrents);
    *backend.0.stats.lock().unwrap() = Some(PublicStats {
        torrents: 1_234_567,
        added_today: 89,
        added_yesterday: 1_000,
    });
    let addr = free_addr();
    let mut cfg = config();
    cfg.listen = addr;
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let deps = WebDeps {
        backend: backend.clone(),
        search: index.search.clone(),
        policy: policy(),
    };
    let server = tokio::spawn(serve(cfg, deps, async move {
        let _ = stop_rx.await;
    }));
    wait_for(addr).await;

    let home = http_get(addr, "/", "").await;
    assert_eq!(home.status, 200);
    assert!(home.body.contains("<dd>1,234,567</dd>"), "{}", home.body);
    assert!(home.body.contains("<dd>89</dd>"));
    assert!(home.body.contains("<dd>1,000</dd>"));
    assert!(home.head.contains(&format!(
        "\r\ncontent-security-policy: {}",
        CSP.to_ascii_lowercase()
    )));
    assert!(home.head.contains("\r\nx-frame-options: deny"));
    assert!(
        home.head
            .contains("\r\nstrict-transport-security: max-age=31536000")
    );
    assert!(!home.head.contains("\r\nserver:"));

    let results = http_get(addr, "/search?q=served", "").await;
    assert_eq!(results.status, 200);
    assert!(results.body.contains("<bdi>served torrent</bdi>"));

    // The peer address reaches the rate limiter: a forwarded header from an
    // untrusted peer changes nothing, and the 31st page is refused.
    let mut statuses = Vec::new();
    for i in 0..31 {
        let spoof = format!("X-Forwarded-For: 203.0.113.{i}\r\n");
        statuses.push(http_get(addr, "/healthz", &spoof).await.status);
    }
    assert!(statuses.contains(&429), "{statuses:?}");

    stop_tx.send(()).unwrap();
    let result = tokio::time::timeout(STOP_TIMEOUT, server)
        .await
        .expect("serve did not stop")
        .unwrap();
    assert!(result.is_ok(), "{result:?}");
    assert!(TcpStream::connect(addr).await.is_err());
}

#[tokio::test]
async fn serve_keeps_running_without_statistics() {
    let _serial = serial().await;
    metrics();
    let index = index_of(&[]);
    let backend = FakeBackend::default();
    let addr = free_addr();
    let mut cfg = config();
    cfg.listen = addr;
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let deps = WebDeps {
        backend,
        search: index.search.clone(),
        policy: policy(),
    };
    let server = tokio::spawn(serve(cfg, deps, async move {
        let _ = stop_rx.await;
    }));
    wait_for(addr).await;
    let home = http_get(addr, "/", "").await;
    assert_eq!(home.status, 200);
    assert!(
        home.body
            .contains("Index statistics are not available yet.")
    );
    drop(stop_tx);
    let result = tokio::time::timeout(STOP_TIMEOUT, server)
        .await
        .expect("serve did not stop")
        .unwrap();
    assert!(result.is_ok());
}

#[tokio::test]
async fn serve_configuration_and_bind_errors() {
    let _serial = serial().await;
    metrics();
    let index = index_of(&[]);
    let deps = || WebDeps {
        backend: FakeBackend::default(),
        search: index.search.clone(),
        policy: policy(),
    };

    let mut bad = config();
    bad.base_url = "search.example.org/path".into();
    let err = serve(bad, deps(), std::future::pending())
        .await
        .unwrap_err();
    assert!(matches!(err, WebError::Config(_)), "{err}");

    let mut unnamed = config();
    unnamed.site_name = " ".into();
    let err = serve(unnamed, deps(), std::future::pending())
        .await
        .unwrap_err();
    assert!(matches!(err, WebError::Config(_)), "{err}");

    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let mut cfg = config();
    cfg.listen = taken.local_addr().unwrap();
    let err = serve(cfg, deps(), std::future::pending())
        .await
        .unwrap_err();
    match err {
        WebError::Bind { addr, .. } => assert_eq!(addr, taken.local_addr().unwrap()),
        other => panic!("expected a bind error, got {other}"),
    }
    assert!(config().validate().is_ok());
}
