//! The index role (design §11): projects the database change feed into the
//! live search index generation, and `index --rebuild` builds a new one.
//!
//! Each poll:
//! 1. take the high-water mark (keep the previous one if the lock was busy);
//! 2. read pages with `checkpoint < change_seq <= mark`;
//! 3. upsert visible rows, delete the rest, and commit with the page's
//!    last `change_seq`;
//! 4. an empty page means the range is drained: commit with the mark (a
//!    short page proves nothing);
//! 5. check that `CURRENT` still names our generation, then sleep.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use dc3_search::{IndexDoc, IndexRoot, IndexWriterHandle, SearchError, SearchIndex};
use dc3_store::{IndexRow, StoreError};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::config::IndexConfig;
use crate::stores::ChangeFeed;

/// The index role is ready while it caught up this recently.
pub const READY_MAX_LAG: Duration = Duration::from_secs(300);
/// Time a retired generation is kept after a rebuild (the web role's
/// 5 s watch plus the 5 s search timeout, with margin).
pub const REBUILD_GRACE: Duration = Duration::from_secs(30);
/// Extra sleep before the cleanup, so the retired generation is surely old
/// enough.
const GRACE_MARGIN: Duration = Duration::from_millis(50);

const METRIC_LAG: &str = "dc3_index_lag";
const METRIC_LAG_SECONDS: &str = "dc3_index_lag_seconds";
const METRIC_DOCS: &str = "dc3_index_docs";

/// Settings of the index role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexOptions {
    pub path: PathBuf,
    pub writer_heap_bytes: usize,
    pub batch_size: i64,
    pub poll_interval: Duration,
    pub ready_max_lag: Duration,
}

impl IndexOptions {
    /// Options from `[index]`.
    pub fn from_config(cfg: &IndexConfig) -> Self {
        Self {
            path: cfg.path.clone(),
            writer_heap_bytes: cfg.writer_heap_bytes,
            batch_size: cfg.batch_size,
            poll_interval: Duration::from_millis(cfg.poll_interval_ms),
            ready_max_lag: READY_MAX_LAG,
        }
    }
}

/// Why the index role stopped.
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("search index error: {0}")]
    Search(#[from] SearchError),
    #[error("database error: {0}")]
    Store(#[from] StoreError),
    #[error("index generation changed from {expected} to {found}; restart the index role")]
    GenerationChanged { expected: u64, found: u64 },
    #[error(
        "the index checkpoint {checkpoint} is ahead of the database change feed ({mark}); \
         run `index --rebuild`"
    )]
    AheadOfDatabase { checkpoint: i64, mark: i64 },
    #[error(
        "the live index generation {0} has a running writer; stop the index role before rebuilding"
    )]
    WriterLocked(u64),
    #[error("the change feed returned position {got} at checkpoint {checkpoint}")]
    FeedOrder { checkpoint: i64, got: i64 },
    #[error("interrupted")]
    Cancelled,
    #[error("index task failed: {0}")]
    Task(String),
}

async fn blocking<T, F>(f: F) -> Result<T, IndexError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, IndexError> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| IndexError::Task(e.to_string()))?
}

/// The search document for a row.
pub fn index_doc(row: &IndexRow) -> IndexDoc {
    IndexDoc {
        id: row.id,
        name: row.name.clone(),
        files: row.files_text.clone(),
        size: row.total_size,
        created: row.first_seen_at.timestamp(),
        seen: row.seen_count,
        file_count: row.file_count,
    }
}

/// Applies rows and commits; returns (upserts, deletes).
fn apply_rows(
    writer: &mut IndexWriterHandle,
    rows: &[IndexRow],
    commit_to: i64,
) -> Result<(u64, u64), SearchError> {
    let (mut upserts, mut deletes) = (0u64, 0u64);
    for row in rows {
        if row.visible {
            writer.upsert(&index_doc(row))?;
            upserts = upserts.saturating_add(1);
        } else {
            writer.delete(row.id)?;
            deletes = deletes.saturating_add(1);
        }
    }
    writer.commit(commit_to)?;
    Ok((upserts, deletes))
}

/// One generation being written.
struct Projector {
    index: Arc<SearchIndex>,
    writer: Option<IndexWriterHandle>,
    generation: u64,
    checkpoint: i64,
    upserts: u64,
    deletes: u64,
}

impl Projector {
    async fn open(index: SearchIndex, generation: u64, heap: usize) -> Result<Self, IndexError> {
        let index = Arc::new(index);
        let for_writer = Arc::clone(&index);
        let writer = blocking(move || Ok(for_writer.writer(heap)?)).await?;
        let checkpoint = writer.checkpoint();
        Ok(Self {
            index,
            writer: Some(writer),
            generation,
            checkpoint,
            upserts: 0,
            deletes: 0,
        })
    }

    async fn apply(&mut self, rows: Vec<IndexRow>, commit_to: i64) -> Result<(), IndexError> {
        let mut writer = self
            .writer
            .take()
            .ok_or_else(|| IndexError::Task("the index writer was lost".into()))?;
        let (writer, result) = tokio::task::spawn_blocking(move || {
            let result = apply_rows(&mut writer, &rows, commit_to);
            (writer, result)
        })
        .await
        .map_err(|e| IndexError::Task(e.to_string()))?;
        self.writer = Some(writer);
        let (upserts, deletes) = result?;
        self.checkpoint = commit_to;
        self.upserts = self.upserts.saturating_add(upserts);
        self.deletes = self.deletes.saturating_add(deletes);
        Ok(())
    }

    fn doc_count(&self) -> u64 {
        self.index.doc_count()
    }

    /// Waits for merges and releases the writer lock.
    async fn close(mut self) -> Result<(), IndexError> {
        if let Some(writer) = self.writer.take() {
            blocking(move || Ok(writer.wait_merging_threads()?)).await?;
        }
        Ok(())
    }
}

/// Reads and applies pages until `checkpoint >= mark`. Returns the number
/// of pages read.
async fn drain<F: ChangeFeed>(
    proj: &mut Projector,
    feed: &F,
    mark: i64,
    batch: i64,
    cancel: &CancellationToken,
) -> Result<usize, IndexError> {
    let mut pages = 0usize;
    while proj.checkpoint < mark {
        if cancel.is_cancelled() {
            return Err(IndexError::Cancelled);
        }
        let rows = feed.changes_since(proj.checkpoint, mark, batch).await?;
        pages = pages.saturating_add(1);
        let drained = rows.is_empty();
        let commit_to = match rows.last() {
            None => mark,
            Some(last) if last.change_seq > proj.checkpoint => last.change_seq,
            Some(last) => {
                return Err(IndexError::FeedOrder {
                    checkpoint: proj.checkpoint,
                    got: last.change_seq,
                });
            }
        };
        proj.apply(rows, commit_to).await?;
        if drained {
            break;
        }
    }
    Ok(pages)
}

fn i64_gauge(v: i64) -> f64 {
    v as f64
}

/// Logs change-feed failures when they start and when they stop, rather
/// than on every poll.
#[derive(Debug, Default)]
struct FeedErrors {
    failing: bool,
}

impl FeedErrors {
    fn record(&mut self, error: Option<&StoreError>) {
        match (error, self.failing) {
            (Some(e), false) => {
                tracing::warn!(error = %e, "reading the change feed failed; retrying");
                self.failing = true;
            }
            (Some(e), true) => tracing::debug!(error = %e, "the change feed still fails"),
            (None, true) => {
                tracing::info!("the change feed answers again");
                self.failing = false;
            }
            (None, false) => {}
        }
    }
}

/// Runs the index role on the live generation until `cancel` is cancelled.
pub async fn run<F: ChangeFeed>(
    opts: IndexOptions,
    feed: F,
    ready: Arc<AtomicBool>,
    cancel: CancellationToken,
) -> Result<(), IndexError> {
    let path = opts.path.clone();
    let (root, generation, index) = blocking(move || {
        let root = IndexRoot::open(&path)?;
        let (generation, _) = root.current()?;
        let index = root.open_generation(generation)?;
        Ok((root, generation, index))
    })
    .await?;
    let mut proj = Projector::open(index, generation, opts.writer_heap_bytes).await?;
    tracing::info!(
        generation,
        checkpoint = proj.checkpoint,
        documents = proj.doc_count(),
        "indexer started"
    );
    let result = follow(&opts, &feed, &ready, &cancel, &root, &mut proj).await;
    ready.store(false, Ordering::Relaxed);
    let closed = proj.close().await;
    result?;
    closed?;
    tracing::info!("indexer stopped");
    Ok(())
}

async fn follow<F: ChangeFeed>(
    opts: &IndexOptions,
    feed: &F,
    ready: &AtomicBool,
    cancel: &CancellationToken,
    root: &IndexRoot,
    proj: &mut Projector,
) -> Result<(), IndexError> {
    let mut mark = proj.checkpoint;
    let mut caught_up_at = Instant::now();
    let mut feed_errors = FeedErrors::default();
    loop {
        if cancel.is_cancelled() {
            return Ok(());
        }
        let mut failure: Option<StoreError> = None;
        let fresh = match feed.high_water_mark().await {
            Ok(Some(m)) => {
                if m < proj.checkpoint {
                    return Err(IndexError::AheadOfDatabase {
                        checkpoint: proj.checkpoint,
                        mark: m,
                    });
                }
                mark = m;
                true
            }
            Ok(None) => false,
            Err(e) => {
                failure = Some(e);
                false
            }
        };
        if proj.checkpoint < mark {
            match drain(proj, feed, mark, opts.batch_size, cancel).await {
                Ok(_) => {}
                Err(IndexError::Cancelled) => return Ok(()),
                Err(IndexError::Store(e)) => failure = Some(e),
                Err(e) => return Err(e),
            }
        }
        feed_errors.record(failure.as_ref());
        let now = Instant::now();
        if fresh && proj.checkpoint >= mark {
            caught_up_at = now;
        }
        let lag = mark.saturating_sub(proj.checkpoint).max(0);
        let lag_time = now.saturating_duration_since(caught_up_at);
        metrics::gauge!(METRIC_LAG).set(i64_gauge(lag));
        metrics::gauge!(METRIC_LAG_SECONDS).set(lag_time.as_secs_f64());
        metrics::gauge!(METRIC_DOCS).set(proj.doc_count() as f64);
        ready.store(lag_time < opts.ready_max_lag, Ordering::Relaxed);

        let for_check = root.clone();
        let (current, _) = blocking(move || Ok(for_check.current()?)).await?;
        if current != proj.generation {
            return Err(IndexError::GenerationChanged {
                expected: proj.generation,
                found: current,
            });
        }
        tokio::select! {
            () = cancel.cancelled() => return Ok(()),
            () = tokio::time::sleep(opts.poll_interval) => {}
        }
    }
}

/// What `index --rebuild` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebuildSummary {
    /// The generation that was live before.
    pub previous: u64,
    /// The generation built and promoted.
    pub generation: u64,
    /// Its checkpoint.
    pub checkpoint: i64,
    /// Documents in it.
    pub documents: u64,
    /// Rows written and rows removed while building.
    pub upserts: u64,
    pub deletes: u64,
    pub elapsed: Duration,
    /// Generations deleted after the grace period.
    pub removed: Vec<u64>,
}

/// Builds a new generation from checkpoint 0, promotes it once it has
/// caught up, waits `grace`, then deletes retired generations.
pub async fn rebuild<F: ChangeFeed>(
    opts: IndexOptions,
    feed: F,
    cancel: CancellationToken,
    grace: Duration,
) -> Result<RebuildSummary, IndexError> {
    let started = Instant::now();
    let path = opts.path.clone();
    let (root, previous, generation, index) = blocking(move || {
        let root = IndexRoot::open(&path)?;
        let (current, _) = root.current()?;
        if root.writer_locked(current)? {
            return Err(IndexError::WriterLocked(current));
        }
        let (generation, index) = root.create_next()?;
        Ok((root, current, generation, index))
    })
    .await?;
    tracing::info!(previous, generation, "index rebuild started");
    let mut proj = Projector::open(index, generation, opts.writer_heap_bytes).await?;
    let caught_up = catch_up(&opts, &feed, &cancel, &mut proj).await;
    let (checkpoint, documents) = (proj.checkpoint, proj.doc_count());
    let (upserts, deletes) = (proj.upserts, proj.deletes);
    let closed = proj.close().await;
    caught_up?;
    closed?;

    let for_promote = root.clone();
    blocking(move || {
        let (current, _) = for_promote.current()?;
        if current != previous {
            return Err(IndexError::GenerationChanged {
                expected: previous,
                found: current,
            });
        }
        for_promote.promote(generation)?;
        Ok(())
    })
    .await?;
    tracing::info!(generation, documents, "index generation promoted");

    let removed = tokio::select! {
        () = cancel.cancelled() => {
            tracing::warn!("interrupted before removing the retired generation");
            Vec::new()
        }
        () = tokio::time::sleep(grace.saturating_add(GRACE_MARGIN)) => {
            let for_cleanup = root.clone();
            // The new generation is live either way; a failed cleanup only
            // leaves disk space for the next rebuild to reclaim.
            match blocking(move || Ok(for_cleanup.cleanup(&[generation], grace)?)).await {
                Ok(removed) => removed,
                Err(e) => {
                    tracing::warn!(error = %e, "removing retired index generations failed");
                    Vec::new()
                }
            }
        }
    };
    Ok(RebuildSummary {
        previous,
        generation,
        checkpoint,
        documents,
        upserts,
        deletes,
        elapsed: started.elapsed(),
        removed,
    })
}

/// A fresh high-water mark, waiting while the lock is busy.
async fn fresh_mark<F: ChangeFeed>(
    feed: &F,
    retry: Duration,
    cancel: &CancellationToken,
) -> Result<i64, IndexError> {
    loop {
        if cancel.is_cancelled() {
            return Err(IndexError::Cancelled);
        }
        match feed.high_water_mark().await? {
            Some(mark) => return Ok(mark),
            None => {
                tokio::select! {
                    () = cancel.cancelled() => return Err(IndexError::Cancelled),
                    () = tokio::time::sleep(retry) => {}
                }
            }
        }
    }
}

/// Drains until a fresh mark is reached. Under constant writes the gap
/// never closes completely, so once a drain needed at most one page, one
/// more fresh mark is drained and the build counts as caught up; the index
/// role continues from its checkpoint.
async fn catch_up<F: ChangeFeed>(
    opts: &IndexOptions,
    feed: &F,
    cancel: &CancellationToken,
    proj: &mut Projector,
) -> Result<(), IndexError> {
    loop {
        let mark = fresh_mark(feed, opts.poll_interval, cancel).await?;
        if proj.checkpoint >= mark {
            return Ok(());
        }
        let pages = drain(proj, feed, mark, opts.batch_size, cancel).await?;
        tracing::info!(
            checkpoint = proj.checkpoint,
            documents = proj.doc_count(),
            "index rebuild progress"
        );
        if pages <= 1 {
            let mark = fresh_mark(feed, opts.poll_interval, cancel).await?;
            if proj.checkpoint < mark {
                drain(proj, feed, mark, opts.batch_size, cancel).await?;
            }
            return Ok(());
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use std::sync::Mutex;

    use chrono::{TimeZone, Utc};
    use dc3_core::DhtKey;
    use dc3_search::{SearchHandle, SearchQuery};

    use super::*;

    #[derive(Clone, Default)]
    struct FakeFeed {
        inner: Arc<Mutex<FeedState>>,
    }

    #[derive(Default)]
    struct FeedState {
        rows: Vec<IndexRow>,
        seq: i64,
        busy: bool,
        failing: bool,
        /// Rows per page at most, whatever the limit (like the byte budget).
        page_cap: usize,
        pages: usize,
    }

    impl FakeFeed {
        fn new(page_cap: usize) -> Self {
            let feed = Self::default();
            feed.inner.lock().unwrap().page_cap = page_cap;
            feed
        }

        /// Adds or replaces a row, giving it the next sequence number.
        fn put(&self, id: i64, name: &str, files: &str, visible: bool) {
            let mut s = self.inner.lock().unwrap();
            s.seq += 1;
            let seq = s.seq;
            s.rows.retain(|r| r.id != id);
            s.rows.push(IndexRow {
                id,
                change_seq: seq,
                dht_key: DhtKey([u8::try_from(id).unwrap(); 20]),
                info_hash_v1: None,
                info_hash_v2: None,
                name: name.into(),
                files_text: files.into(),
                total_size: 10,
                file_count: 1,
                first_seen_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
                seen_count: 1,
                last_scraped_at: None,
                seeders_est: None,
                visible,
            });
        }

        /// Uses up sequence numbers without rows (rolled-back writers).
        fn skip(&self, n: i64) {
            self.inner.lock().unwrap().seq += n;
        }

        fn set_busy(&self, busy: bool) {
            self.inner.lock().unwrap().busy = busy;
        }

        fn set_failing(&self, failing: bool) {
            self.inner.lock().unwrap().failing = failing;
        }

        fn pages(&self) -> usize {
            self.inner.lock().unwrap().pages
        }
    }

    impl ChangeFeed for FakeFeed {
        async fn high_water_mark(&self) -> dc3_store::Result<Option<i64>> {
            let s = self.inner.lock().unwrap();
            if s.failing {
                return Err(StoreError::Invalid("injected feed failure".into()));
            }
            Ok((!s.busy).then_some(s.seq))
        }

        async fn changes_since(
            &self,
            after: i64,
            upto: i64,
            limit: i64,
        ) -> dc3_store::Result<Vec<IndexRow>> {
            let mut s = self.inner.lock().unwrap();
            if s.failing {
                return Err(StoreError::Invalid("injected feed failure".into()));
            }
            s.pages += 1;
            let mut rows: Vec<IndexRow> = s
                .rows
                .iter()
                .filter(|r| r.change_seq > after && r.change_seq <= upto)
                .cloned()
                .collect();
            rows.sort_by_key(|r| r.change_seq);
            rows.truncate(usize::try_from(limit).unwrap().min(s.page_cap));
            Ok(rows)
        }
    }

    fn options(dir: &std::path::Path) -> IndexOptions {
        IndexOptions {
            path: dir.to_path_buf(),
            writer_heap_bytes: dc3_search::WRITER_PROBE_HEAP_BYTES * 2,
            batch_size: 1000,
            poll_interval: Duration::from_millis(20),
            ready_max_lag: READY_MAX_LAG,
        }
    }

    #[test]
    fn documents() {
        let feed = FakeFeed::new(10);
        feed.put(1, "debian", "a/b.iso\nc.txt", true);
        feed.put(2, "debian", "iso/debian.iso", true);
        let rows = feed.inner.lock().unwrap().rows.clone();
        let doc = index_doc(&rows[1]);
        assert_eq!(doc.id, 2);
        assert_eq!(doc.created, 1_700_000_000);
        assert_eq!(doc.files, "iso/debian.iso");
    }

    async fn ids(handle: &SearchHandle, text: &str) -> Vec<i64> {
        let h = handle.clone();
        tokio::task::spawn_blocking(move || h.refresh())
            .await
            .unwrap()
            .unwrap();
        let mut ids: Vec<i64> = handle
            .search(SearchQuery::new(text), Duration::from_secs(5))
            .await
            .unwrap()
            .0
            .hits
            .iter()
            .map(|h| h.id)
            .collect();
        ids.sort_unstable();
        ids
    }

    async fn checkpoint(dir: &std::path::Path) -> i64 {
        let dir = dir.to_path_buf();
        tokio::task::spawn_blocking(move || {
            IndexRoot::open(&dir)
                .unwrap()
                .open_current()
                .unwrap()
                .checkpoint()
                .unwrap()
        })
        .await
        .unwrap()
    }

    async fn wait_for<Fut: std::future::Future<Output = bool>>(
        what: &str,
        mut check: impl FnMut() -> Fut,
    ) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !check().await {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn follows_the_feed_and_stops_on_a_generation_change() {
        let dir = tempfile::tempdir().unwrap();
        // Two rows per page, so short pages happen before the range is drained.
        let feed = FakeFeed::new(2);
        feed.put(1, "ubuntu desktop", "ubuntu.iso", true);
        feed.put(2, "ubuntu docs", "x.iso", true);
        feed.put(3, "ubuntu server", "images/x.iso", true);
        feed.put(4, "ubuntu hidden", "y.iso", false);
        feed.put(5, "ubuntu core", "core.iso", true);
        feed.skip(3);
        let ready = Arc::new(AtomicBool::new(false));
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run(
            options(dir.path()),
            feed.clone(),
            Arc::clone(&ready),
            cancel.clone(),
        ));
        let path = dir.path().to_path_buf();
        wait_for("the first pass", || {
            let path = path.clone();
            async move { checkpoint(&path).await == 8 }
        })
        .await;
        // Readiness is updated just after the pass that reached the mark.
        wait_for("readiness", || {
            let ready = Arc::clone(&ready);
            async move { ready.load(Ordering::Relaxed) }
        })
        .await;
        // Pages of 2, 2 and 1 rows, then an empty page ends the range.
        assert!(feed.pages() >= 4, "{} pages", feed.pages());
        let handle = SearchHandle::open(dir.path()).unwrap();
        assert_eq!(ids(&handle, "ubuntu").await, vec![1, 2, 3, 5]);

        // Later changes: a hide and a new row.
        feed.set_busy(true);
        feed.put(1, "ubuntu desktop", "ubuntu.iso", false);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            checkpoint(dir.path()).await,
            8,
            "a busy lock keeps the mark"
        );
        feed.set_busy(false);
        feed.put(6, "ubuntu cloud", "cloud.iso", true);
        let path = dir.path().to_path_buf();
        wait_for("the second pass", || {
            let path = path.clone();
            async move { checkpoint(&path).await == 10 }
        })
        .await;
        assert_eq!(ids(&handle, "ubuntu").await, vec![2, 3, 5, 6]);

        // Another process promotes a new generation: the indexer stops.
        let root = IndexRoot::open(dir.path()).unwrap();
        let (next, _) = root.create_next().unwrap();
        root.promote(next).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(
                result,
                Err(IndexError::GenerationChanged { expected: 1, found }) if found == next
            ),
            "{result:?}"
        );
        assert!(!ready.load(Ordering::Relaxed));
        drop(cancel);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stops_on_cancel_and_refuses_a_feed_behind_the_index() {
        let dir = tempfile::tempdir().unwrap();
        let feed = FakeFeed::new(10);
        feed.put(1, "one", "", true);
        let cancel = CancellationToken::new();
        let ready = Arc::new(AtomicBool::new(false));
        let task = tokio::spawn(run(
            options(dir.path()),
            feed.clone(),
            Arc::clone(&ready),
            cancel.clone(),
        ));
        let path = dir.path().to_path_buf();
        wait_for("the first pass", || {
            let path = path.clone();
            async move { checkpoint(&path).await == 1 }
        })
        .await;
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        // A database whose sequence restarted is refused.
        let empty = FakeFeed::new(10);
        let result = run(
            options(dir.path()),
            empty,
            Arc::new(AtomicBool::new(false)),
            CancellationToken::new(),
        )
        .await;
        assert!(
            matches!(
                result,
                Err(IndexError::AheadOfDatabase {
                    checkpoint: 1,
                    mark: 0
                })
            ),
            "{result:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn survives_feed_errors() {
        let dir = tempfile::tempdir().unwrap();
        let feed = FakeFeed::new(10);
        feed.set_failing(true);
        feed.put(1, "arch linux", "", true);
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run(
            options(dir.path()),
            feed.clone(),
            Arc::new(AtomicBool::new(false)),
            cancel.clone(),
        ));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !task.is_finished(),
            "a feed error must not stop the indexer"
        );
        assert_eq!(checkpoint(dir.path()).await, 0);
        feed.set_failing(false);
        let path = dir.path().to_path_buf();
        wait_for("recovery", || {
            let path = path.clone();
            async move { checkpoint(&path).await == 1 }
        })
        .await;
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rebuild_builds_promotes_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let feed = FakeFeed::new(1);
        for id in 1..=5 {
            feed.put(id, &format!("fedora {id}"), "", id != 3);
        }
        // An old generation with stale content.
        {
            let root = IndexRoot::open(dir.path()).unwrap();
            let index = root.open_current().unwrap();
            let mut w = index.writer(dc3_search::WRITER_PROBE_HEAP_BYTES).unwrap();
            w.upsert(&IndexDoc {
                id: 99,
                name: "fedora stale".into(),
                ..IndexDoc::default()
            })
            .unwrap();
            w.commit(2).unwrap();
            w.wait_merging_threads().unwrap();
        }
        let handle = SearchHandle::open(dir.path()).unwrap();
        assert_eq!(ids(&handle, "fedora").await, vec![99]);

        // Refused while a writer holds the live generation.
        {
            let root = IndexRoot::open(dir.path()).unwrap();
            let index = root.open_current().unwrap();
            let _writer = index.writer(dc3_search::WRITER_PROBE_HEAP_BYTES).unwrap();
            let result = rebuild(
                options(dir.path()),
                feed.clone(),
                CancellationToken::new(),
                Duration::from_millis(50),
            )
            .await;
            assert!(
                matches!(result, Err(IndexError::WriterLocked(1))),
                "{result:?}"
            );
        }

        let summary = rebuild(
            options(dir.path()),
            feed.clone(),
            CancellationToken::new(),
            Duration::from_millis(100),
        )
        .await
        .unwrap();
        assert_eq!(summary.previous, 1);
        assert_eq!(summary.generation, 2);
        assert_eq!(summary.checkpoint, 5);
        assert_eq!(summary.documents, 4);
        assert_eq!((summary.upserts, summary.deletes), (4, 1));
        assert_eq!(summary.removed, vec![1]);
        assert_eq!(ids(&handle, "fedora").await, vec![1, 2, 4, 5]);
        assert_eq!(handle.current_generation(), 2);
        let root = IndexRoot::open(dir.path()).unwrap();
        assert_eq!(root.generations().unwrap(), vec![2]);
        assert_eq!(checkpoint(dir.path()).await, 5);
    }
}
