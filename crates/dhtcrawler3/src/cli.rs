//! The command line (design §13).
//!
//! Exit codes: 0 on success, 1 on error, 2 on a usage error.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, anyhow};
use clap::{Parser, Subcommand};
use tokio_util::sync::CancellationToken;

use crate::admin;
use crate::config::{self, Config, DbRole};
use crate::{healthcheck, logging, roles, signals};

/// Exit code of a usage error.
pub const EXIT_USAGE: u8 = 2;
/// How long the runtime waits for blocking tasks at exit.
pub const RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// dhtcrawler3: a BitTorrent DHT search engine.
#[derive(Debug, Parser)]
#[command(name = "dhtcrawler3", version, propagate_version = true)]
pub struct Cli {
    /// Configuration file. Without this option,
    /// /etc/dhtcrawler3/dhtcrawler3.toml is read if it exists.
    #[arg(long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

/// The subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Apply database migrations and store settings (owner credentials).
    Migrate,
    /// Run the crawl role.
    Crawl,
    /// Run the index role.
    Index {
        /// Build a new index generation from scratch, switch to it, and
        /// remove the old one. Stop the index role first.
        #[arg(long)]
        rebuild: bool,
    },
    /// Run the web role.
    Web,
    /// Run crawl, index and web in one process.
    All {
        /// Apply migrations first (the database section must name the owner).
        #[arg(long)]
        migrate: bool,
    },
    /// Show totals and the counters of the last days.
    Stats,
    /// Check the configuration and print it without secrets.
    CheckConfig,
    /// GET an http:// URL and exit 0 on a 2xx answer.
    Healthcheck {
        /// For example http://127.0.0.1:9100/readyz
        url: String,
    },
}

/// An error and its causes on one line. A cause whose text the message so
/// far already ends with is skipped (many errors quote their source).
pub fn describe_error(e: &anyhow::Error) -> String {
    let mut out = String::new();
    for cause in e.chain() {
        let text = cause.to_string();
        if out.ends_with(&text) {
            continue;
        }
        if !out.is_empty() {
            out.push_str(": ");
        }
        out.push_str(&text);
    }
    out
}

/// The program's entry point.
pub fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            // Printing can only fail if stderr is closed; the code still tells.
            let _ = e.print();
            return ExitCode::from(u8::try_from(e.exit_code()).unwrap_or(EXIT_USAGE));
        }
    };
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("dhtcrawler3: {}", describe_error(&e));
            ExitCode::FAILURE
        }
    }
}

/// Runs a parsed command, printing results on stdout.
pub fn run(cli: Cli) -> anyhow::Result<()> {
    run_to(cli, &mut std::io::stdout())
}

/// Runs a parsed command, printing results on `out`.
pub fn run_to(cli: Cli, out: &mut (dyn Write + Send)) -> anyhow::Result<()> {
    if let Command::Healthcheck { url } = &cli.command {
        // No configuration and no logging: this runs every few seconds.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("cannot start the async runtime")?;
        return runtime
            .block_on(healthcheck::check(url))
            .map_err(|e| anyhow!("health check failed: {e}"));
    }
    let cfg = config::load(cli.config.as_deref(), std::env::vars_os())?;
    if let Command::CheckConfig = cli.command {
        admin::check_config(&cfg, out)?;
        return Ok(());
    }
    logging::init(cfg.log.format, cfg.log.level)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("dhtcrawler3")
        .build()
        .context("cannot start the async runtime")?;
    let result = runtime.block_on(dispatch(cfg, cli.command, out));
    runtime.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);
    result
}

/// Logs a role's failure before it is printed.
fn logged(result: anyhow::Result<()>) -> anyhow::Result<()> {
    if let Err(e) = &result {
        tracing::error!(error = %describe_error(e), "stopped with an error");
    }
    result
}

fn shutdown_token() -> CancellationToken {
    let token = CancellationToken::new();
    signals::spawn_handler(token.clone());
    token
}

async fn dispatch(
    cfg: Config,
    command: Command,
    out: &mut (dyn Write + Send),
) -> anyhow::Result<()> {
    match command {
        Command::Crawl => logged(roles::crawl(&cfg, shutdown_token()).await),
        Command::Index { rebuild: false } => logged(roles::index(&cfg, shutdown_token()).await),
        Command::Index { rebuild: true } => {
            logged(roles::rebuild(&cfg, shutdown_token(), out).await)
        }
        Command::Web => logged(roles::web(&cfg, shutdown_token()).await),
        Command::All { migrate } => logged(roles::all(&cfg, migrate, shutdown_token()).await),
        Command::Migrate => {
            let store = roles::connect(&cfg, DbRole::Main).await?;
            admin::migrate(&store, out).await?;
            Ok(())
        }
        Command::Stats => {
            let store = roles::connect(&cfg, DbRole::Main).await?;
            admin::stats(&store, out).await?;
            Ok(())
        }
        Command::CheckConfig => {
            admin::check_config(&cfg, out)?;
            Ok(())
        }
        Command::Healthcheck { url } => healthcheck::check(&url)
            .await
            .map_err(|e| anyhow!("health check failed: {e}")),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("dhtcrawler3").chain(args.iter().copied()))
    }

    #[test]
    fn definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn commands_parse() {
        let cli = parse(&["--config", "/tmp/x.toml", "crawl"]).unwrap();
        assert_eq!(cli.config, Some(PathBuf::from("/tmp/x.toml")));
        assert!(matches!(cli.command, Command::Crawl));
        // The global option also works after the subcommand.
        let cli = parse(&["index", "--rebuild", "--config", "c.toml"]).unwrap();
        assert!(matches!(cli.command, Command::Index { rebuild: true }));
        assert!(matches!(
            parse(&["index"]).unwrap().command,
            Command::Index { rebuild: false }
        ));
        assert!(matches!(
            parse(&["all", "--migrate"]).unwrap().command,
            Command::All { migrate: true }
        ));
        for simple in ["migrate", "web", "stats", "check-config"] {
            assert!(parse(&[simple]).is_ok(), "{simple}");
        }
        match parse(&["healthcheck", "http://127.0.0.1:9100/readyz"])
            .unwrap()
            .command
        {
            Command::Healthcheck { url } => assert_eq!(url, "http://127.0.0.1:9100/readyz"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn usage_errors_exit_with_2() {
        for args in [
            vec![],
            vec!["frobnicate"],
            vec!["deny"],
            vec!["policy"],
            vec!["index", "--rebuild=maybe"],
            vec!["healthcheck"],
        ] {
            let err = parse(&args).unwrap_err();
            assert_eq!(err.exit_code(), 2, "{args:?}: {err}");
        }
        assert_eq!(parse(&["--help"]).unwrap_err().exit_code(), 0);
        assert_eq!(parse(&["--version"]).unwrap_err().exit_code(), 0);
    }

    #[test]
    fn error_chains_are_not_repeated() {
        let io = std::io::Error::other("disk on fire");
        let e = anyhow::Error::from(admin::AdminError::from(io)).context("cannot print");
        assert_eq!(
            describe_error(&e),
            "cannot print: cannot write output: disk on fire"
        );
        let plain = anyhow!("inner").context("outer");
        assert_eq!(describe_error(&plain), "outer: inner");
    }

    #[test]
    fn explicit_missing_config_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let cli = parse(&[
            "--config",
            dir.path().join("missing.toml").to_str().unwrap(),
            "stats",
        ])
        .unwrap();
        let err = describe_error(&run_to(cli, &mut Vec::new()).unwrap_err());
        assert!(err.starts_with("cannot read config file"), "{err}");
        assert_eq!(err.matches("No such file").count(), 1, "{err}");
    }

    #[test]
    fn check_config_runs_without_a_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.toml");
        std::fs::write(&path, "[web]\nsite_name = \"test site\"\n").unwrap();
        let cli = parse(&["--config", path.to_str().unwrap(), "check-config"]).unwrap();
        let mut out = Vec::new();
        run_to(cli, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("site_name = \"test site\""), "{text}");
        let bad = dir.path().join("bad.toml");
        std::fs::write(&bad, "[web]\nnope = 1\n").unwrap();
        let cli = parse(&["--config", bad.to_str().unwrap(), "check-config"]).unwrap();
        assert!(run_to(cli, &mut Vec::new()).is_err());
    }
}
