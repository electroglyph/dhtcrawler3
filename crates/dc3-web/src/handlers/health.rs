//! `GET /healthz` and `GET /readyz`.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use dc3_search::FIRST_GENERATION;

use crate::app::AppState;
use crate::render::text;
use crate::{Backend, READY_TIMEOUT};

/// Liveness: the process answers.
pub(crate) async fn healthz() -> Response {
    text(StatusCode::OK, "ok")
}

/// Readiness: the database answers and a search index is open.
pub(crate) async fn readyz<B: Backend>(State(st): State<Arc<AppState<B>>>) -> Response {
    let index_open = st.search.current_generation() >= FIRST_GENERATION;
    let database = match tokio::time::timeout(READY_TIMEOUT, st.backend.ping()).await {
        Ok(Ok(())) => true,
        Ok(Err(e)) => {
            tracing::debug!(error = %e, "readiness: database ping failed");
            false
        }
        Err(_) => {
            tracing::debug!("readiness: database ping timed out");
            false
        }
    };
    if index_open && database {
        text(StatusCode::OK, "ready")
    } else {
        text(StatusCode::SERVICE_UNAVAILABLE, "not ready")
    }
}
