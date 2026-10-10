//! A TCP listener with a connection cap, a header-read timeout and an idle
//! timeout.
//!
//! `axum::serve` sets no header-read timeout, so a client could otherwise
//! hold a connection open indefinitely. Each accepted connection holds a
//! permit from a semaphore of [`MAX_CONNECTIONS`]; when all are taken, new
//! connections wait in the kernel backlog. A connection fails with
//! `TimedOut`, which closes it, when
//! - a request head (from the connection's start, or from the first byte of
//!   a later request, to the blank line that ends the head) takes longer
//!   than [`HEADER_READ_TIMEOUT`], however steadily its bytes trickle in; or
//! - it makes no read or write progress for [`CONNECTION_IDLE_TIMEOUT`].
//!
//! A request body is bounded by the request timeout of the middleware.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::serve::Listener;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::{Instant, Sleep};

/// Most connections open at once.
pub const MAX_CONNECTIONS: usize = 768;
/// A connection without read or write progress for this long is closed.
/// It is longer than reverse proxies keep idle upstream connections (Caddy:
/// 2 minutes, nginx: 60 s), so the proxy always closes first and never
/// sends a request on a connection this side is closing.
pub const CONNECTION_IDLE_TIMEOUT: Duration = Duration::from_secs(150);
/// A request head must arrive completely within this long.
pub const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);
/// Pause after an `accept` error that is not about a single connection
/// (for example running out of file descriptors).
const ACCEPT_ERROR_PAUSE: Duration = Duration::from_secs(1);

/// A [`TcpListener`] that caps open connections and times out idle ones.
pub(crate) struct GuardedListener {
    inner: TcpListener,
    permits: Arc<Semaphore>,
    idle: Duration,
}

impl GuardedListener {
    pub(crate) fn new(inner: TcpListener, max_connections: usize, idle: Duration) -> Self {
        GuardedListener {
            inner,
            permits: Arc::new(Semaphore::new(max_connections)),
            idle,
        }
    }
}

impl Listener for GuardedListener {
    type Io = GuardedStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            // The semaphore is never closed, so this cannot fail; if it ever
            // did, accepting without a permit is refused below.
            let Ok(permit) = Arc::clone(&self.permits).acquire_owned().await else {
                tokio::time::sleep(ACCEPT_ERROR_PAUSE).await;
                continue;
            };
            match self.inner.accept().await {
                Ok((stream, addr)) => {
                    return (GuardedStream::new(stream, self.idle, Some(permit)), addr);
                }
                Err(e) if is_connection_error(&e) => {}
                Err(e) => {
                    tracing::error!(error = %e, "web listener cannot accept connections");
                    tokio::time::sleep(ACCEPT_ERROR_PAUSE).await;
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

fn is_connection_error(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
    )
}

/// Where the connection is in the HTTP/1 request cycle, as far as the
/// bytes show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Head {
    /// A request head is being read, and its deadline runs. `started` is
    /// set by the first byte other than CR or LF (blank lines before a
    /// request line are skipped by the parser); `newlines` counts line ends
    /// in a row since then, ignoring CR. Two end the head.
    Reading { started: bool, newlines: u8 },
    /// The head is complete and no final response was written yet: bytes
    /// read now belong to the body (or to a pipelined request).
    Body,
    /// A response was written: the next byte read starts a new head.
    Done,
}

/// Start of an interim (1xx) response, which does not end a request.
const INTERIM_RESPONSE: &[u8] = b"HTTP/1.1 1";

/// A stream that fails once a request head takes too long or it has been
/// idle too long, and releases its connection permit when dropped.
pub(crate) struct GuardedStream<S> {
    inner: S,
    idle: Duration,
    deadline: Pin<Box<Sleep>>,
    head: Head,
    head_deadline: Pin<Box<Sleep>>,
    _permit: Option<OwnedSemaphorePermit>,
}

impl<S> GuardedStream<S> {
    pub(crate) fn new(inner: S, idle: Duration, permit: Option<OwnedSemaphorePermit>) -> Self {
        GuardedStream {
            inner,
            idle,
            deadline: Box::pin(tokio::time::sleep(idle)),
            head: Head::Reading {
                started: false,
                newlines: 0,
            },
            head_deadline: Box::pin(tokio::time::sleep(HEADER_READ_TIMEOUT)),
            _permit: permit,
        }
    }

    /// The wrapped stream.
    pub(crate) fn get_ref(&self) -> &S {
        &self.inner
    }

    /// Pushes the deadline back after progress.
    fn touch(&mut self) {
        let next = Instant::now()
            .checked_add(self.idle)
            .unwrap_or_else(Instant::now);
        self.deadline.as_mut().reset(next);
    }

    /// True if a request head is being read and its deadline has passed.
    fn head_expired(&self) -> bool {
        matches!(self.head, Head::Reading { .. }) && Instant::now() >= self.head_deadline.deadline()
    }

    /// Follows the request cycle over bytes just read.
    fn scan_read(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if self.head == Head::Done {
                self.head = Head::Reading {
                    started: false,
                    newlines: 0,
                };
                let next = Instant::now()
                    .checked_add(HEADER_READ_TIMEOUT)
                    .unwrap_or_else(Instant::now);
                self.head_deadline.as_mut().reset(next);
            }
            let Head::Reading { started, newlines } = &mut self.head else {
                return;
            };
            match b {
                b'\r' => {}
                b'\n' if *started => {
                    *newlines = newlines.saturating_add(1);
                    if *newlines >= 2 {
                        self.head = Head::Body;
                    }
                }
                b'\n' => {}
                _ => {
                    *started = true;
                    *newlines = 0;
                }
            }
        }
    }

    /// Follows the request cycle over a write that starts with `first`.
    fn scan_write(&mut self, first: &[u8]) {
        match self.head {
            Head::Reading { .. } => self.head = Head::Done,
            Head::Body if !first.starts_with(INTERIM_RESPONSE) => self.head = Head::Done,
            Head::Body | Head::Done => {}
        }
    }

    /// Called while the stream is not ready: fails once a deadline passed.
    fn poll_idle<T>(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<T>> {
        if matches!(self.head, Head::Reading { .. })
            && self.head_deadline.as_mut().poll(cx).is_ready()
        {
            return Poll::Ready(Err(head_timed_out()));
        }
        match self.deadline.as_mut().poll(cx) {
            Poll::Ready(()) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "connection idle for too long",
            ))),
            Poll::Pending => Poll::Pending,
        }
    }
}

fn head_timed_out() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "request head took too long")
}

impl<S: AsyncRead + Unpin> AsyncRead for GuardedStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.head_expired() {
            return Poll::Ready(Err(head_timed_out()));
        }
        let before = buf.filled().len();
        match Pin::new(&mut this.inner).poll_read(cx, buf) {
            Poll::Ready(result) => {
                this.touch();
                if result.is_ok() {
                    this.scan_read(buf.filled().get(before..).unwrap_or_default());
                }
                Poll::Ready(result)
            }
            Poll::Pending => this.poll_idle(cx),
        }
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for GuardedStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_write(cx, buf) {
            Poll::Ready(result) => {
                this.touch();
                if matches!(result, Ok(n) if n > 0) {
                    this.scan_write(buf);
                }
                Poll::Ready(result)
            }
            Poll::Pending => this.poll_idle(cx),
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_write_vectored(cx, bufs) {
            Poll::Ready(result) => {
                this.touch();
                if matches!(result, Ok(n) if n > 0) {
                    let first = bufs.iter().find(|b| !b.is_empty());
                    this.scan_write(first.map_or(&[], |b| &b[..]));
                }
                Poll::Ready(result)
            }
            Poll::Pending => this.poll_idle(cx),
        }
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_flush(cx) {
            Poll::Ready(result) => Poll::Ready(result),
            Poll::Pending => this.poll_idle(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_shutdown(cx) {
            Poll::Ready(result) => Poll::Ready(result),
            Poll::Pending => this.poll_idle(cx),
        }
    }
}

/// Sets `TCP_NODELAY` on an accepted connection; failure only costs latency.
pub(crate) fn set_nodelay(stream: &mut GuardedStream<TcpStream>) {
    if let Err(e) = stream.get_ref().set_nodelay(true) {
        tracing::debug!(error = %e, "cannot set TCP_NODELAY");
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt as _};

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn idle_stream_times_out() {
        let (client, server) = tokio::io::duplex(64);
        let mut guarded = GuardedStream::new(server, Duration::from_secs(1), None);
        let mut buf = [0u8; 8];
        let err = guarded.read(&mut buf).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        drop(client);
    }

    #[tokio::test(start_paused = true)]
    async fn progress_pushes_the_deadline_back() {
        let (mut client, server) = tokio::io::duplex(64);
        let mut guarded = GuardedStream::new(server, Duration::from_secs(1), None);
        let writer = tokio::spawn(async move {
            for _ in 0..3 {
                tokio::time::sleep(Duration::from_millis(700)).await;
                client.write_all(b"x").await.unwrap();
            }
            client
        });
        let start = Instant::now();
        let mut buf = [0u8; 1];
        for _ in 0..3 {
            guarded.read_exact(&mut buf).await.unwrap();
        }
        // 2.1 s have passed, longer than the idle timeout, without a timeout.
        assert!(start.elapsed() >= Duration::from_millis(2100));
        // The peer stays open but silent: the next read times out.
        let _client = writer.await.unwrap();
        let err = guarded.read(&mut buf).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }

    /// Reads from `guarded` until it fails, within `limit`.
    async fn read_until_error(
        guarded: &mut GuardedStream<tokio::io::DuplexStream>,
        limit: Duration,
    ) -> io::Error {
        let mut buf = [0u8; 64];
        let run = async {
            loop {
                match guarded.read(&mut buf).await {
                    Ok(0) => panic!("unexpected end of stream"),
                    Ok(_) => {}
                    Err(e) => return e,
                }
            }
        };
        tokio::time::timeout(limit, run)
            .await
            .expect("the stream was not closed in time")
    }

    #[tokio::test(start_paused = true)]
    async fn a_trickled_request_head_times_out() {
        let (mut client, server) = tokio::io::duplex(64);
        let mut guarded = GuardedStream::new(server, CONNECTION_IDLE_TIMEOUT, None);
        let writer = tokio::spawn(async move {
            client
                .write_all(b"GET / HTTP/1.1\r\nX-Pad: ")
                .await
                .unwrap();
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if client.write_all(b"x").await.is_err() {
                    break;
                }
            }
        });
        let start = Instant::now();
        let err = read_until_error(&mut guarded, Duration::from_secs(300)).await;
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() <= HEADER_READ_TIMEOUT);
        writer.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn a_silent_new_connection_times_out_like_a_slow_head() {
        let (_client, server) = tokio::io::duplex(64);
        let mut guarded = GuardedStream::new(server, CONNECTION_IDLE_TIMEOUT, None);
        let start = Instant::now();
        let err = read_until_error(&mut guarded, Duration::from_secs(300)).await;
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() <= HEADER_READ_TIMEOUT);
    }

    #[tokio::test(start_paused = true)]
    async fn only_request_heads_are_timed() {
        let (mut client, server) = tokio::io::duplex(256);
        let mut guarded = GuardedStream::new(server, CONNECTION_IDLE_TIMEOUT, None);
        let mut buf = [0u8; 256];
        // Leading blank lines do not end a head.
        client
            .write_all(b"\r\n\r\nPOST / HTTP/1.1\r\n")
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
        client
            .write_all(b"Content-Length: 3\r\n\r\n")
            .await
            .unwrap();
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            let n = guarded.read(&mut buf).await.unwrap();
            head.extend_from_slice(&buf[..n]);
        }
        // A slow body is bounded by the request timeout, not here.
        for _ in 0..3 {
            tokio::time::sleep(Duration::from_secs(20)).await;
            client.write_all(b"b").await.unwrap();
            assert_eq!(guarded.read(&mut buf).await.unwrap(), 1);
        }
        guarded.write_all(b"HTTP/1.1 200 OK\r\n\r\n").await.unwrap();
        // An idle keep-alive connection is closed only by the idle timeout.
        tokio::time::sleep(CONNECTION_IDLE_TIMEOUT - Duration::from_secs(1)).await;
        client.write_all(b"GET / HTTP/1.1\n\n").await.unwrap();
        assert!(guarded.read(&mut buf).await.unwrap() > 0);
        guarded.write_all(b"HTTP/1.1 200 OK\r\n\r\n").await.unwrap();
        // The next head is timed from its first byte.
        tokio::time::sleep(Duration::from_secs(100)).await;
        client.write_all(b"G").await.unwrap();
        let start = Instant::now();
        let err = read_until_error(&mut guarded, Duration::from_secs(300)).await;
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() <= HEADER_READ_TIMEOUT);
    }

    #[tokio::test]
    async fn connections_beyond_the_cap_wait_for_a_permit() {
        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = tcp.local_addr().unwrap();
        let mut listener = GuardedListener::new(tcp, 1, CONNECTION_IDLE_TIMEOUT);
        let _c1 = TcpStream::connect(addr).await.unwrap();
        let _c2 = TcpStream::connect(addr).await.unwrap();
        let (first, _) = listener.accept().await;
        // The second connection is not accepted while the first is open.
        let waiting = tokio::time::timeout(Duration::from_millis(100), listener.accept()).await;
        assert!(waiting.is_err());
        drop(first);
        let second = tokio::time::timeout(Duration::from_secs(5), listener.accept()).await;
        assert!(second.is_ok());
    }
}
