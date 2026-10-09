//! Logging (R19): structured logs to stderr, JSON or human-readable.
//!
//! Logs never carry peer, node or visitor addresses, search text or
//! secrets (R11). The noisy libraries are held at `warn`, and release
//! builds never log at `trace` (the only level allowed to show addresses).
//! Panics are logged with their location only: a panic message could quote
//! untrusted data.

use std::io::IsTerminal;

use tracing::Level;
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::prelude::*;

use crate::config::{LogFormat, LogLevel};

/// Libraries logged at `warn` at most.
pub const QUIET_TARGETS: [&str; 3] = ["tantivy", "sqlx", "hyper"];

/// Why logging could not be set up.
#[derive(Debug, thiserror::Error)]
#[error("cannot set up logging: {0}")]
pub struct LoggingError(String);

/// The level actually used: `trace` is lowered to `debug` in release builds.
pub fn effective_level(level: LogLevel) -> Level {
    match level {
        LogLevel::Trace if cfg!(debug_assertions) => Level::TRACE,
        LogLevel::Trace | LogLevel::Debug => Level::DEBUG,
        LogLevel::Info => Level::INFO,
        LogLevel::Warn => Level::WARN,
        LogLevel::Error => Level::ERROR,
    }
}

/// The filter for `level`, with the quiet libraries capped at `warn`.
pub fn filter(level: LogLevel) -> Targets {
    let level = LevelFilter::from_level(effective_level(level));
    let quiet = level.min(LevelFilter::WARN);
    QUIET_TARGETS
        .iter()
        .fold(Targets::new().with_default(level), |t, target| {
            t.with_target(*target, quiet)
        })
}

/// Installs the global subscriber and the panic hook. Call once.
pub fn init(format: LogFormat, level: LogLevel) -> Result<(), LoggingError> {
    let filter = filter(level);
    let result = match format {
        LogFormat::Json => tracing_subscriber::registry()
            .with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_current_span(false)
                    .with_span_list(false)
                    .with_writer(std::io::stderr)
                    .with_filter(filter),
            )
            .try_init(),
        LogFormat::Pretty => tracing_subscriber::registry()
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(std::io::stderr().is_terminal())
                    .with_writer(std::io::stderr)
                    .with_filter(filter),
            )
            .try_init(),
    };
    result.map_err(|e| LoggingError(e.to_string()))?;
    install_panic_hook();
    Ok(())
}

/// Logs panics with their location but without their message.
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_default();
        tracing::error!(location = %location, "panic");
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_and_quiet_targets() {
        assert_eq!(effective_level(LogLevel::Info), Level::INFO);
        assert_eq!(effective_level(LogLevel::Error), Level::ERROR);
        #[cfg(debug_assertions)]
        assert_eq!(effective_level(LogLevel::Trace), Level::TRACE);
        #[cfg(not(debug_assertions))]
        assert_eq!(effective_level(LogLevel::Trace), Level::DEBUG);

        let f = filter(LogLevel::Debug);
        assert!(f.would_enable("dhtcrawler4::crawl", &Level::DEBUG));
        assert!(!f.would_enable("tantivy::indexer", &Level::INFO));
        assert!(f.would_enable("tantivy::indexer", &Level::WARN));
        assert!(!f.would_enable("sqlx::query", &Level::INFO));
        assert!(!f.would_enable("hyper::proto", &Level::DEBUG));

        let f = filter(LogLevel::Error);
        assert!(!f.would_enable("dhtcrawler4", &Level::WARN));
        assert!(!f.would_enable("tantivy", &Level::WARN));
        assert!(f.would_enable("tantivy", &Level::ERROR));
    }
}
