//! [`SearchHandle`]: the web role's view of the live index generation.

use std::future::Future;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use tokio::sync::Semaphore;

use crate::generations::IndexRoot;
use crate::index::{MAX_CONCURRENT_SEARCHES, Result, SearchIndex, SearchResults};
use crate::query::SearchQuery;

/// Shortest poll interval [`SearchHandle::watch`] uses.
pub const MIN_WATCH_INTERVAL: Duration = Duration::from_millis(10);

/// A cheap-to-clone handle that always searches the live generation.
///
/// All clones share one search limit of [`MAX_CONCURRENT_SEARCHES`], which
/// carries over when the handle switches generation. A search that started
/// on the old generation finishes there.
#[derive(Clone)]
pub struct SearchHandle {
    inner: Arc<Inner>,
}

struct Inner {
    root: IndexRoot,
    live: RwLock<Arc<SearchIndex>>,
    permits: Arc<Semaphore>,
    /// Serialises refreshes so an older `CURRENT` never replaces a newer one.
    refreshing: Mutex<()>,
}

impl std::fmt::Debug for SearchHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchHandle")
            .field("generation", &self.current_generation())
            .finish_non_exhaustive()
    }
}

impl SearchHandle {
    /// Opens the live generation under `root`, initialising the root if it
    /// is new (see [`IndexRoot::open`]). Blocking.
    pub fn open(root: &Path) -> Result<SearchHandle> {
        let root = IndexRoot::open(root)?;
        let permits = Arc::new(Semaphore::new(MAX_CONCURRENT_SEARCHES));
        let (generation, _) = root.current()?;
        let index = root.open_generation_with_permits(generation, Arc::clone(&permits))?;
        Ok(SearchHandle {
            inner: Arc::new(Inner {
                root,
                live: RwLock::new(Arc::new(index)),
                permits,
                refreshing: Mutex::new(()),
            }),
        })
    }

    /// The index searches currently use.
    pub fn index(&self) -> Arc<SearchIndex> {
        // The lock only guards an `Arc` swap, so a poisoned value is intact.
        let live = self
            .inner
            .live
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        Arc::clone(&live)
    }

    /// Searches the live generation; see [`SearchIndex::search_async`].
    pub async fn search(&self, q: SearchQuery, timeout: Duration) -> Result<SearchResults> {
        let index = self.index();
        index.search_async(q, timeout).await
    }

    /// The generation searches currently use.
    pub fn current_generation(&self) -> u64 {
        // Indexes opened through an `IndexRoot` always carry a generation.
        self.index().generation().unwrap_or_default()
    }

    /// Number of live documents in the current generation.
    pub fn doc_count(&self) -> u64 {
        self.index().doc_count()
    }

    /// Reads `CURRENT` once: switches to a newly promoted generation, or
    /// reloads the current one if its commits changed. Returns the live
    /// generation. On error the previous state stays in use. Blocking.
    pub fn refresh(&self) -> Result<u64> {
        let _serial = self
            .inner
            .refreshing
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let (generation, _) = self.inner.root.current()?;
        let live = self.index();
        if live.generation() == Some(generation) {
            live.reload_if_changed()?;
            return Ok(generation);
        }
        let next = self
            .inner
            .root
            .open_generation_with_permits(generation, Arc::clone(&self.inner.permits))?;
        *self
            .inner
            .live
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Arc::new(next);
        tracing::info!(
            generation,
            previous = ?live.generation(),
            "search index switched generation"
        );
        Ok(generation)
    }

    /// Calls [`refresh`](Self::refresh) every `interval` (at least
    /// [`MIN_WATCH_INTERVAL`]) until `cancel` completes. Failures are logged
    /// when they start and when they stop; the handle keeps serving.
    pub async fn watch(self, interval: Duration, cancel: impl Future<Output = ()>) {
        let interval = interval.max(MIN_WATCH_INTERVAL);
        let mut cancel = std::pin::pin!(cancel);
        let mut last_error: Option<String> = None;
        loop {
            tokio::select! {
                () = &mut cancel => break,
                () = tokio::time::sleep(interval) => {}
            }
            let this = self.clone();
            let error = match tokio::task::spawn_blocking(move || this.refresh()).await {
                Ok(Ok(_)) => None,
                Ok(Err(e)) => Some(e.to_string()),
                Err(e) => Some(format!("refresh task failed: {e}")),
            };
            if error != last_error {
                match &error {
                    Some(e) => tracing::warn!(
                        error = %e,
                        "search index refresh failed; serving the previous state"
                    ),
                    None => tracing::info!("search index refresh recovered"),
                }
            }
            last_error = error;
        }
        tracing::debug!("search index watch stopped");
    }
}
