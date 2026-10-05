//! The JSON API: `GET /api/v1/search` and `GET /api/v1/torrents/{key}`.
//!
//! Responses are built from the types below, never by serialising
//! [`TorrentRecord`], so moderation fields never leak. Errors are
//! `{"error": "..."}`. No CORS headers are sent.

use std::sync::Arc;

use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use dc3_store::TorrentRecord;
use serde::Serialize;

use super::search::{
    SearchParams, execute, expansion_blocked, is_blocked, parse_page, parse_per_page, parse_sort,
    too_long, too_long_message,
};
use super::{Lookup, Shown, lookup, parse_key};
use crate::Backend;
use crate::app::AppState;
use crate::format::rfc3339;
use crate::render::{json, json_error};

/// One torrent in API output.
#[derive(Debug, Serialize)]
pub(crate) struct ApiTorrent {
    pub dht_key: String,
    pub info_hash_v1: Option<String>,
    pub info_hash_v2: Option<String>,
    pub name: String,
    pub size: u64,
    pub file_count: u64,
    pub first_seen: String,
    pub last_seen: String,
    pub seen_count: u64,
    pub magnet: Option<String>,
}

impl ApiTorrent {
    fn new(record: &TorrentRecord, shown: &Shown) -> ApiTorrent {
        ApiTorrent {
            dht_key: record.dht_key.to_hex(),
            info_hash_v1: record.info_hash_v1.map(|k| k.to_hex()),
            info_hash_v2: record.info_hash_v2.map(|h| h.to_hex()),
            name: shown.name.clone(),
            size: record.total_size,
            file_count: record.file_count,
            first_seen: rfc3339(record.first_seen_at),
            last_seen: rfc3339(record.last_seen_at),
            seen_count: record.seen_count,
            magnet: shown.magnet.clone(),
        }
    }
}

#[derive(Debug, Serialize)]
struct ApiFile<'a> {
    path: &'a str,
    size: u64,
}

#[derive(Debug, Serialize)]
struct ApiTorrentDetail<'a> {
    #[serde(flatten)]
    torrent: ApiTorrent,
    files: Vec<ApiFile<'a>>,
    files_truncated: bool,
}

#[derive(Debug, Serialize)]
struct ApiSearch<'a> {
    query: &'a str,
    page: u32,
    per_page: u32,
    total: u64,
    #[serde(skip_serializing_if = "is_false")]
    blocked: bool,
    results: Vec<ApiTorrent>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// `GET /api/v1/search?q=&p=&sort=&per_page=`.
pub(crate) async fn search<B: Backend>(
    State(st): State<Arc<AppState<B>>>,
    params: Result<Query<SearchParams>, QueryRejection>,
) -> Response {
    let Ok(Query(params)) = params else {
        return json_error(
            StatusCode::BAD_REQUEST,
            "the query string could not be read",
        );
    };
    let q = params.q.as_deref().unwrap_or_default().trim();
    if q.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "the q parameter is required");
    }
    // Checked first, so the term matcher never sees more than the limit.
    if too_long(q) {
        return json_error(StatusCode::BAD_REQUEST, &too_long_message());
    }
    // Validated first, so blocked and clean queries answer bad parameters
    // alike: no status oracle for the blocklist.
    let parsed = parse_page(params.p.as_deref()).and_then(|page| {
        let per_page = parse_per_page(params.per_page.as_deref())?;
        let sort = parse_sort(params.sort.as_deref())?;
        Ok((page, per_page, sort))
    });
    let (page, per_page, sort) = match parsed {
        Ok(v) => v,
        Err(e) => return json_error(StatusCode::BAD_REQUEST, &e.message()),
    };
    if is_blocked(&st, q) {
        // The refused query is not echoed back.
        let body = ApiSearch {
            query: "",
            page,
            per_page,
            total: 0,
            blocked: true,
            results: Vec::new(),
        };
        return json(StatusCode::OK, &body);
    }
    if expansion_blocked(&st, q).await {
        // A prefix of a blocked term is blocked like the term itself.
        let body = ApiSearch {
            query: "",
            page,
            per_page,
            total: 0,
            blocked: true,
            results: Vec::new(),
        };
        return json(StatusCode::OK, &body);
    }
    match execute(&st, q, page, per_page, sort).await {
        Ok(found) => {
            let body = ApiSearch {
                query: q,
                page,
                per_page,
                total: found.total,
                blocked: false,
                results: found
                    .torrents
                    .iter()
                    .map(|(record, shown)| ApiTorrent::new(record, shown))
                    .collect(),
            };
            json(StatusCode::OK, &body)
        }
        Err(failure) => json_error(failure.status(), &failure.message()),
    }
}

/// `GET /api/v1/torrents/{key}`.
pub(crate) async fn torrent<B: Backend>(
    State(st): State<Arc<AppState<B>>>,
    key: Result<Path<String>, PathRejection>,
) -> Response {
    let Some(key) = parse_key(key) else {
        return json_error(
            StatusCode::BAD_REQUEST,
            "the key must be 40 hex or 32 base32 characters, or 64 hex characters for a v2 hash",
        );
    };
    match lookup(&st, key).await {
        Lookup::Found {
            record,
            shown,
            permit: _permit,
        } => {
            let files = shown
                .files
                .iter()
                .map(|f| ApiFile {
                    path: &f.path,
                    size: f.size,
                })
                .collect();
            let body = ApiTorrentDetail {
                torrent: ApiTorrent::new(&record, &shown),
                files,
                files_truncated: record.files_truncated || shown.files_cut,
            };
            json(StatusCode::OK, &body)
        }
        Lookup::Missing => json_error(StatusCode::NOT_FOUND, "torrent not found"),
        Lookup::Unavailable => json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "the database is not available right now",
        ),
    }
}
