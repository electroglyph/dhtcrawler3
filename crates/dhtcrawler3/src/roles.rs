//! Runs the long-lived roles (`crawl`, `index`, `web`, `all`) with their
//! metrics listener, and `index --rebuild`.

use std::io::Write;

use anyhow::{Context, anyhow};
use dc3_store::Store;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::config::{Config, DbRole};
use crate::crawl::{self, CrawlOptions};
use crate::index::{self, IndexOptions, REBUILD_GRACE};
use crate::metrics_server::{self, MetricsServer, Readiness};
use crate::{admin, policy, web};

/// Connects a pool with `role`'s credentials.
pub async fn connect(cfg: &Config, role: DbRole) -> anyhow::Result<Store> {
    let options = cfg.connect_options(role)?;
    let (user, _) = cfg.credentials(role);
    Store::connect_with(options, cfg.database.max_connections)
        .await
        .with_context(|| format!("cannot connect to the database as {user}"))
}

async fn start_metrics(cfg: &Config, readiness: Readiness) -> anyhow::Result<MetricsServer> {
    let handle = metrics_server::install_recorder()?;
    Ok(MetricsServer::start(cfg.metrics.listen, handle, readiness).await?)
}

fn log_warnings(cfg: &Config) {
    for warning in cfg.warnings() {
        tracing::warn!(%warning, "configuration");
    }
}

/// The crawl role.
pub async fn crawl(cfg: &Config, cancel: CancellationToken) -> anyhow::Result<()> {
    let readiness = Readiness::new();
    let ready = readiness.register();
    let metrics = start_metrics(cfg, readiness).await?;
    let result: anyhow::Result<()> = async {
        let opts = CrawlOptions::from_config(cfg)?;
        let store = connect(cfg, DbRole::Crawler).await?;
        let policy = policy::load_shared(cfg.policy.terms_file.clone()).await?;
        crawl::run(opts, store, policy, ready, cancel).await?;
        Ok(())
    }
    .await;
    metrics.stop().await;
    result
}

/// The index role.
pub async fn index(cfg: &Config, cancel: CancellationToken) -> anyhow::Result<()> {
    let readiness = Readiness::new();
    let ready = readiness.register();
    let metrics = start_metrics(cfg, readiness).await?;
    let result: anyhow::Result<()> = async {
        let store = connect(cfg, DbRole::Indexer).await?;
        let policy = policy::load_shared(cfg.policy.terms_file.clone()).await?;
        index::run(
            IndexOptions::from_config(&cfg.index),
            store,
            policy,
            ready,
            cancel,
        )
        .await?;
        Ok(())
    }
    .await;
    metrics.stop().await;
    result
}

/// `index --rebuild`, with a summary on `out`.
pub async fn rebuild(
    cfg: &Config,
    cancel: CancellationToken,
    out: &mut (dyn Write + Send),
) -> anyhow::Result<()> {
    let store = connect(cfg, DbRole::Indexer).await?;
    let policy = policy::load_shared(cfg.policy.terms_file.clone()).await?;
    writeln!(
        out,
        "rebuilding the search index in {}",
        cfg.index.path.display()
    )?;
    let s = index::rebuild(
        IndexOptions::from_config(&cfg.index),
        store,
        policy,
        cancel,
        REBUILD_GRACE,
    )
    .await?;
    writeln!(
        out,
        "generation {} replaced generation {} after {:.1} s: {} documents at checkpoint {} \
         ({} rows indexed, {} rows left out)",
        s.generation,
        s.previous,
        s.elapsed.as_secs_f64(),
        s.documents,
        s.checkpoint,
        s.upserts,
        s.deletes
    )?;
    if s.removed.is_empty() {
        writeln!(
            out,
            "no old generation was removed; the next rebuild removes what is left"
        )?;
    } else {
        let removed: Vec<String> = s.removed.iter().map(u64::to_string).collect();
        writeln!(out, "removed generation(s): {}", removed.join(", "))?;
    }
    Ok(())
}

/// The web role.
pub async fn web(cfg: &Config, cancel: CancellationToken) -> anyhow::Result<()> {
    let readiness = Readiness::new();
    let ready = readiness.register();
    let metrics = start_metrics(cfg, readiness).await?;
    let result: anyhow::Result<()> = async {
        log_warnings(cfg);
        let web_cfg = web::web_config(cfg)?;
        let store = connect(cfg, DbRole::Web).await?;
        let policy = policy::load_shared(cfg.policy.terms_file.clone()).await?;
        let search = web::open_search(&cfg.index.path).await?;
        web::run(web_cfg, store, search, policy, ready, cancel).await?;
        Ok(())
    }
    .await;
    metrics.stop().await;
    result
}

/// `all`: crawl, index and web in one process, each with its own pool and
/// credentials. If one role fails, the others are stopped and the error is
/// returned.
pub async fn all(cfg: &Config, migrate: bool, cancel: CancellationToken) -> anyhow::Result<()> {
    let readiness = Readiness::new();
    let crawl_ready = readiness.register();
    let index_ready = readiness.register();
    let web_ready = readiness.register();
    let metrics = start_metrics(cfg, readiness).await?;
    let result = run_all(cfg, migrate, cancel, [crawl_ready, index_ready, web_ready]).await;
    metrics.stop().await;
    result
}

async fn run_all(
    cfg: &Config,
    migrate: bool,
    cancel: CancellationToken,
    [crawl_ready, index_ready, web_ready]: [std::sync::Arc<std::sync::atomic::AtomicBool>; 3],
) -> anyhow::Result<()> {
    log_warnings(cfg);
    if migrate {
        let owner = connect(cfg, DbRole::Main).await?;
        admin::migrate(&owner, &mut std::io::sink()).await?;
        owner.pool().close().await;
    }
    let crawl_opts = CrawlOptions::from_config(cfg)?;
    let index_opts = IndexOptions::from_config(&cfg.index);
    let web_cfg = web::web_config(cfg)?;
    let crawl_store = connect(cfg, DbRole::Crawler).await?;
    let index_store = connect(cfg, DbRole::Indexer).await?;
    let web_store = connect(cfg, DbRole::Web).await?;
    let policy = policy::load_shared(cfg.policy.terms_file.clone()).await?;
    let search = web::open_search(&cfg.index.path).await?;

    let roles = cancel.child_token();
    let mut tasks: JoinSet<(&'static str, anyhow::Result<()>)> = JoinSet::new();
    {
        let (policy, token) = (policy.clone(), roles.clone());
        tasks.spawn(async move {
            let result = crawl::run(crawl_opts, crawl_store, policy, crawl_ready, token).await;
            ("crawl", result.map_err(anyhow::Error::from))
        });
    }
    {
        let (policy, token) = (policy.clone(), roles.clone());
        tasks.spawn(async move {
            let result = index::run(index_opts, index_store, policy, index_ready, token).await;
            ("index", result.map_err(anyhow::Error::from))
        });
    }
    {
        let token = roles.clone();
        tasks.spawn(async move {
            let result = web::run(web_cfg, web_store, search, policy, web_ready, token).await;
            ("web", result.map_err(anyhow::Error::from))
        });
    }

    let mut first_error: Option<anyhow::Error> = None;
    while let Some(joined) = tasks.join_next().await {
        let (role, result) = match joined {
            Ok(done) => done,
            Err(e) => ("unknown", Err(anyhow!("role task failed: {e}"))),
        };
        let result = match result {
            Ok(()) if !roles.is_cancelled() => Err(anyhow!("stopped unexpectedly")),
            other => other,
        };
        if let Err(e) = result {
            tracing::error!(
                role,
                error = %crate::cli::describe_error(&e),
                "role failed; stopping the others"
            );
            if first_error.is_none() {
                first_error = Some(e.context(format!("the {role} role failed")));
            }
            roles.cancel();
        }
    }
    first_error.map_or(Ok(()), Err)
}
