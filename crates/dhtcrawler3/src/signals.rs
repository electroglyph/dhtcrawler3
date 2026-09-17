//! Shutdown signals: SIGINT and SIGTERM cancel a token for a graceful stop;
//! a second signal exits at once.

use tokio_util::sync::CancellationToken;

/// Exit code after a second signal.
pub const FORCED_EXIT_CODE: i32 = 1;

/// Cancels `token` on the first SIGINT or SIGTERM, and exits the process on
/// the second. Must be called inside a Tokio runtime.
pub fn spawn_handler(token: CancellationToken) {
    tokio::spawn(async move {
        let mut signals = Signals::new();
        signals.next().await;
        tracing::info!("shutdown requested");
        token.cancel();
        signals.next().await;
        tracing::warn!("second shutdown request; exiting now");
        std::process::exit(FORCED_EXIT_CODE);
    });
}

struct Signals {
    #[cfg(unix)]
    term: Option<tokio::signal::unix::Signal>,
    #[cfg(unix)]
    int: Option<tokio::signal::unix::Signal>,
}

impl Signals {
    fn new() -> Self {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let listen = |kind: SignalKind, name: &str| match signal(kind) {
                Ok(s) => Some(s),
                Err(e) => {
                    tracing::warn!(signal = name, error = %e, "cannot listen for a signal");
                    None
                }
            };
            Self {
                term: listen(SignalKind::terminate(), "SIGTERM"),
                int: listen(SignalKind::interrupt(), "SIGINT"),
            }
        }
        #[cfg(not(unix))]
        {
            Self {}
        }
    }

    /// Waits for the next signal; never returns if none can be received.
    async fn next(&mut self) {
        #[cfg(unix)]
        {
            async fn recv(s: &mut Option<tokio::signal::unix::Signal>) {
                match s {
                    Some(s) => {
                        if s.recv().await.is_none() {
                            std::future::pending::<()>().await;
                        }
                    }
                    None => std::future::pending::<()>().await,
                }
            }
            tokio::select! {
                () = recv(&mut self.term) => {}
                () = recv(&mut self.int) => {}
            }
        }
        #[cfg(not(unix))]
        {
            if tokio::signal::ctrl_c().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }
}
