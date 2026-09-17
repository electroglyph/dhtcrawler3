//! `dhtcrawler3 healthcheck URL`: a minimal HTTP/1.1 GET for container
//! health checks (the runtime image has no curl). Succeeds only on a
//! `HTTP/1.x 2xx` status line.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Time allowed for the whole check.
pub const HEALTHCHECK_TIMEOUT: Duration = Duration::from_secs(5);
/// Most response bytes read.
pub const MAX_RESPONSE_BYTES: usize = 8 * 1024;
/// Longest URL accepted.
pub const MAX_URL_LEN: usize = 2048;
/// Port used when the URL names none.
pub const DEFAULT_HTTP_PORT: u16 = 80;
const SCHEME: &str = "http://";
const READ_CHUNK: usize = 1024;

/// Why a health check failed.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HealthError {
    #[error("invalid URL: {0}")]
    InvalidUrl(&'static str),
    #[error("cannot connect: {0}")]
    Connect(std::io::ErrorKind),
    #[error("i/o error: {0}")]
    Io(std::io::ErrorKind),
    #[error("timed out")]
    Timeout,
    #[error("not an HTTP/1.x response")]
    BadResponse,
    #[error("HTTP status {0}")]
    Status(u16),
}

/// The parts of a URL the check needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// Host name or address, without brackets.
    pub host: String,
    pub port: u16,
    /// `host[:port]` as written, for the `Host` header.
    pub authority: String,
    /// Path and query, starting with `/`.
    pub path: String,
}

/// Parses an `http://host[:port][/path]` URL.
pub fn parse_url(url: &str) -> Result<Target, HealthError> {
    if url.len() > MAX_URL_LEN {
        return Err(HealthError::InvalidUrl("too long"));
    }
    let rest = url
        .get(..SCHEME.len())
        .filter(|s| s.eq_ignore_ascii_case(SCHEME))
        .and_then(|_| url.get(SCHEME.len()..))
        .ok_or(HealthError::InvalidUrl("only http:// URLs are supported"))?;
    let rest = rest.split_once('#').map_or(rest, |(before, _)| before);
    let (authority, path) = match rest.find(['/', '?']) {
        Some(at) => rest.split_at(at),
        None => (rest, ""),
    };
    let path = match path {
        "" => "/".to_owned(),
        p if p.starts_with('?') => format!("/{p}"),
        p => p.to_owned(),
    };
    if !path.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(HealthError::InvalidUrl(
            "the path has spaces or control characters",
        ));
    }
    if authority.is_empty() {
        return Err(HealthError::InvalidUrl("no host"));
    }
    if authority.contains('@') {
        return Err(HealthError::InvalidUrl("user info is not supported"));
    }
    let (host, port) = match authority.strip_prefix('[') {
        Some(inner) => {
            let (host, after) = inner
                .split_once(']')
                .ok_or(HealthError::InvalidUrl("unterminated IPv6 address"))?;
            if host.parse::<std::net::Ipv6Addr>().is_err() {
                return Err(HealthError::InvalidUrl("invalid IPv6 address"));
            }
            let port = match after {
                "" => None,
                p => Some(
                    p.strip_prefix(':')
                        .ok_or(HealthError::InvalidUrl("junk after the IPv6 address"))?,
                ),
            };
            (host, port)
        }
        None => match authority.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        },
    };
    let host_ok = !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':'));
    if !host_ok {
        return Err(HealthError::InvalidUrl("invalid host"));
    }
    let port = match port {
        None => DEFAULT_HTTP_PORT,
        Some(p) => p
            .parse::<u16>()
            .ok()
            .filter(|p| *p != 0)
            .ok_or(HealthError::InvalidUrl("invalid port"))?,
    };
    Ok(Target {
        host: host.to_owned(),
        port,
        authority: authority.to_owned(),
        path,
    })
}

/// The status code of a `HTTP/1.x NNN ...` status line.
pub fn status_code(response: &[u8]) -> Result<u16, HealthError> {
    let line_end = response
        .iter()
        .position(|b| *b == b'\n')
        .ok_or(HealthError::BadResponse)?;
    let line = response.get(..line_end).ok_or(HealthError::BadResponse)?;
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let rest = line
        .strip_prefix(b"HTTP/1.")
        .ok_or(HealthError::BadResponse)?;
    let (minor, rest) = rest.split_first().ok_or(HealthError::BadResponse)?;
    if !matches!(minor, b'0' | b'1') {
        return Err(HealthError::BadResponse);
    }
    let rest = rest.strip_prefix(b" ").ok_or(HealthError::BadResponse)?;
    let (code, reason) = rest.split_at_checked(3).ok_or(HealthError::BadResponse)?;
    if !code.iter().all(u8::is_ascii_digit) || !(reason.is_empty() || reason.starts_with(b" ")) {
        return Err(HealthError::BadResponse);
    }
    std::str::from_utf8(code)
        .ok()
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or(HealthError::BadResponse)
}

/// GETs `url`; Ok only for a 2xx status within [`HEALTHCHECK_TIMEOUT`].
pub async fn check(url: &str) -> Result<(), HealthError> {
    check_with_timeout(url, HEALTHCHECK_TIMEOUT).await
}

/// [`check`] with another time limit.
pub async fn check_with_timeout(url: &str, limit: Duration) -> Result<(), HealthError> {
    let target = parse_url(url)?;
    let code = tokio::time::timeout(limit, get(&target))
        .await
        .map_err(|_| HealthError::Timeout)??;
    if (200..300).contains(&code) {
        Ok(())
    } else {
        Err(HealthError::Status(code))
    }
}

async fn get(target: &Target) -> Result<u16, HealthError> {
    let mut stream = TcpStream::connect((target.host.as_str(), target.port))
        .await
        .map_err(|e| HealthError::Connect(e.kind()))?;
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        target.path, target.authority
    );
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|e| HealthError::Io(e.kind()))?;
    let mut response = Vec::with_capacity(READ_CHUNK);
    let mut chunk = [0u8; READ_CHUNK];
    while response.len() < MAX_RESPONSE_BYTES && !response.contains(&b'\n') {
        let room = MAX_RESPONSE_BYTES
            .saturating_sub(response.len())
            .min(READ_CHUNK);
        let buf = chunk.get_mut(..room).ok_or(HealthError::BadResponse)?;
        let n = stream
            .read(buf)
            .await
            .map_err(|e| HealthError::Io(e.kind()))?;
        if n == 0 {
            break;
        }
        response.extend_from_slice(buf.get(..n).ok_or(HealthError::BadResponse)?);
    }
    status_code(&response)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use axum::Router;
    use axum::http::StatusCode;
    use axum::routing::get;
    use tokio::net::TcpListener;

    use super::*;

    #[test]
    fn urls() {
        let t = parse_url("http://127.0.0.1:9100/readyz").unwrap();
        assert_eq!(
            t,
            Target {
                host: "127.0.0.1".into(),
                port: 9100,
                authority: "127.0.0.1:9100".into(),
                path: "/readyz".into()
            }
        );
        let t = parse_url("HTTP://localhost").unwrap();
        assert_eq!((t.port, t.path.as_str()), (80, "/"));
        let t = parse_url("http://[::1]:8080?x=1#frag").unwrap();
        assert_eq!((t.host.as_str(), t.port), ("::1", 8080));
        assert_eq!(t.path, "/?x=1");
        assert_eq!(t.authority, "[::1]:8080");
        for bad in [
            "https://127.0.0.1/",
            "127.0.0.1:80",
            "http://",
            "http:///x",
            "http://u:p@host/",
            "http://host:0/",
            "http://host:99999/",
            "http://host:x/",
            "http://[::1/",
            "http://[nope]/",
            "http://ho st/",
            "http://host/a b",
            "http://host/a\r\nX: y",
        ] {
            assert!(parse_url(bad).is_err(), "{bad:?} should be rejected");
        }
        assert!(parse_url(&format!("http://h/{}", "a".repeat(MAX_URL_LEN))).is_err());
    }

    #[test]
    fn status_lines() {
        assert_eq!(status_code(b"HTTP/1.1 200 OK\r\n"), Ok(200));
        assert_eq!(status_code(b"HTTP/1.0 204\r\n\r\n"), Ok(204));
        assert_eq!(status_code(b"HTTP/1.1 503 Service Unavailable\n"), Ok(503));
        for bad in [
            &b"HTTP/2 200 OK\r\n"[..],
            b"HTTP/1.1 200 OK",
            b"HTTP/1.1 20 OK\r\n",
            b"HTTP/1.1 2000 OK\r\n",
            b"HTTP/1.1 abc OK\r\n",
            b"garbage\r\n",
            b"",
            b"HTTP/1.x 200 OK\r\n",
        ] {
            assert_eq!(status_code(bad), Err(HealthError::BadResponse), "{bad:?}");
        }
    }

    async fn serve(app: Router) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });
        format!("http://{addr}")
    }

    /// A raw TCP server that writes `reply` (or nothing) to every client.
    async fn raw(reply: Option<&'static [u8]>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    let _ = stream.read(&mut buf).await;
                    match reply {
                        Some(r) => {
                            let _ = stream.write_all(r).await;
                        }
                        None => tokio::time::sleep(Duration::from_secs(30)).await,
                    }
                });
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn checks_against_local_servers() {
        let app = Router::new()
            .route("/ok", get(|| async { "fine" }))
            .route(
                "/down",
                get(|| async { (StatusCode::SERVICE_UNAVAILABLE, "no") }),
            )
            .route("/created", get(|| async { (StatusCode::CREATED, "made") }));
        let base = serve(app).await;
        assert_eq!(check(&format!("{base}/ok")).await, Ok(()));
        assert_eq!(check(&format!("{base}/created")).await, Ok(()));
        assert_eq!(
            check(&format!("{base}/down")).await,
            Err(HealthError::Status(503))
        );
        assert_eq!(
            check(&format!("{base}/missing")).await,
            Err(HealthError::Status(404))
        );

        let garbage = raw(Some(b"garbage\r\n\r\n")).await;
        assert_eq!(check(&garbage).await, Err(HealthError::BadResponse));
        let empty = raw(Some(b"")).await;
        assert_eq!(check(&empty).await, Err(HealthError::BadResponse));
        let huge = raw(Some(&[b'x'; 10_000])).await;
        assert_eq!(check(&huge).await, Err(HealthError::BadResponse));

        let silent = raw(None).await;
        assert_eq!(
            check_with_timeout(&silent, Duration::from_millis(200)).await,
            Err(HealthError::Timeout)
        );

        let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = closed.local_addr().unwrap();
        drop(closed);
        assert!(matches!(
            check(&format!("http://{addr}/")).await,
            Err(HealthError::Connect(_))
        ));
    }
}
