//! The database operations the web role uses.

use std::future::Future;

use dc3_core::AnyKey;
use dc3_store::{PublicStats, Store, StoreError, TorrentRecord};

/// A boxed error from a [`Backend`] implementation.
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Why a [`Backend`] call failed.
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    /// The backend rejected the input ([`StoreError::Invalid`]).
    #[error("invalid input: {0}")]
    Invalid(String),
    /// The backend could not answer, for example because the database is
    /// unreachable.
    #[error("backend unavailable: {0}")]
    Unavailable(#[source] BoxError),
}

impl From<StoreError> for BackendError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::Invalid(message) => BackendError::Invalid(message),
            other => BackendError::Unavailable(Box::new(other)),
        }
    }
}

/// Database access for the web role.
///
/// These four operations are all the `dc3_web` database user may perform
/// (`docs/03-design.md` §10): reads of visible torrents and statistics.
/// The trait is used generically, never as a
/// trait object, so tests can supply an in-memory implementation.
pub trait Backend: Clone + Send + Sync + 'static {
    /// The visible torrent with this key, with every stored file.
    fn get_by_key(
        &self,
        key: AnyKey,
    ) -> impl Future<Output = Result<Option<TorrentRecord>, BackendError>> + Send;

    /// The visible torrents among `ids`, in the order of `ids`. Ids that are
    /// missing, hidden, denied or deleted are skipped.
    fn get_many(
        &self,
        ids: &[i64],
    ) -> impl Future<Output = Result<Vec<TorrentRecord>, BackendError>> + Send;

    /// Totals for the home page.
    fn public_stats(&self) -> impl Future<Output = Result<PublicStats, BackendError>> + Send;

    /// Checks that the backend answers.
    fn ping(&self) -> impl Future<Output = Result<(), BackendError>> + Send;
}

impl Backend for Store {
    async fn get_by_key(&self, key: AnyKey) -> Result<Option<TorrentRecord>, BackendError> {
        Ok(Store::get_by_key(self, &key).await?)
    }

    async fn get_many(&self, ids: &[i64]) -> Result<Vec<TorrentRecord>, BackendError> {
        Ok(Store::get_many(self, ids).await?)
    }

    async fn public_stats(&self) -> Result<PublicStats, BackendError> {
        Ok(Store::public_stats(self).await?)
    }

    async fn ping(&self) -> Result<(), BackendError> {
        Ok(Store::ping(self).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{WebConfig, WebDeps, WebError};

    /// Compiles only if serving the real store can be spawned on a
    /// multi-threaded runtime.
    fn serve_store(
        cfg: WebConfig,
        deps: WebDeps<Store>,
    ) -> impl Future<Output = Result<(), WebError>> + Send + 'static {
        crate::serve(cfg, deps, std::future::pending())
    }

    #[test]
    fn store_can_be_served_from_a_spawned_task() {
        let _ = serve_store;
        let _ = crate::router::<Store>;
    }

    #[test]
    fn store_errors_map_to_backend_errors() {
        assert!(matches!(
            BackendError::from(StoreError::Invalid("x".into())),
            BackendError::Invalid(m) if m == "x"
        ));
        assert!(matches!(
            BackendError::from(StoreError::NotFound),
            BackendError::Unavailable(_)
        ));
    }
}
