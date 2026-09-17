//! `GET /t/{key}`: one torrent's details.

use std::sync::Arc;

use axum::extract::rejection::PathRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;

use super::{Lookup, lookup, parse_key};
use crate::Backend;
use crate::app::{AppState, routes};
use crate::format::{date, grouped, human_size, rfc3339};
use crate::render::{Flavor, error_response, html};
use crate::templates::{FileView, TorrentPage, TorrentView};

/// The message for a malformed key.
pub(crate) const BAD_KEY_MESSAGE: &str = "That is not a valid torrent key. A key is 40 \
hexadecimal or 32 base32 characters, or 64 hexadecimal characters for a v2 info hash.";

/// `GET /t/{key}`.
pub(crate) async fn torrent_page<B: Backend>(
    State(st): State<Arc<AppState<B>>>,
    key: Result<Path<String>, PathRejection>,
) -> Response {
    let site = &st.site;
    let Some(key) = parse_key(key) else {
        return error_response(site, Flavor::Html, StatusCode::BAD_REQUEST, BAD_KEY_MESSAGE);
    };
    let found = lookup(&st, key).await;
    let (record, shown, _permit) = match found {
        Lookup::Found {
            record,
            shown,
            permit,
        } => (record, shown, permit),
        Lookup::Missing => {
            return error_response(
                site,
                Flavor::Html,
                StatusCode::NOT_FOUND,
                "No torrent with this key is available.",
            );
        }
        Lookup::Unavailable => {
            return error_response(
                site,
                Flavor::Html,
                StatusCode::SERVICE_UNAVAILABLE,
                "The database is not available right now. Please try again later.",
            );
        }
    };

    let listed = u64::try_from(shown.files.len()).unwrap_or(u64::MAX);
    let files_note = (record.files_truncated || shown.files_cut || listed < record.file_count)
        .then(|| {
            format!(
                "Only the first {} of {} files are listed.",
                grouped(listed),
                grouped(record.file_count.max(listed))
            )
        });
    let dht_key = record.dht_key.to_hex();
    let view = TorrentView {
        report_href: format!("{}{dht_key}", routes::REPORT_PREFIX),
        dht_key,
        info_hash_v1: record.info_hash_v1.map(|k| k.to_hex()),
        info_hash_v2: record.info_hash_v2.map(|h| h.to_hex()),
        size_human: human_size(record.total_size),
        size_exact: grouped(record.total_size),
        file_count: grouped(record.file_count),
        first_seen: date(record.first_seen_at),
        first_seen_iso: rfc3339(record.first_seen_at),
        last_seen: date(record.last_seen_at),
        last_seen_iso: rfc3339(record.last_seen_at),
        seen_count: grouped(record.seen_count),
        magnet: shown.magnet,
        name: shown.name,
    };
    let files = shown
        .files
        .into_iter()
        .map(|f| FileView {
            size_human: human_size(f.size),
            size_exact: grouped(f.size),
            path: f.path,
        })
        .collect();
    let page = TorrentPage {
        page: site.page(view.name.clone(), "", true),
        t: view,
        files,
        files_note,
    };
    html(StatusCode::OK, &page)
}
