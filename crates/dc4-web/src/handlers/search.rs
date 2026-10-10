//! `GET /search` and the search logic the JSON API shares.
//!
//! Search text is never logged.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::http::{HeaderMap, Uri};
use axum::response::{IntoResponse, Redirect, Response};
use chrono::{DateTime, Utc};
use dc4_search::{
    DEFAULT_PER_PAGE, IndexStamp, MAX_PAGE, MAX_PER_PAGE, MAX_QUERY_CHARS, MAX_TERMS, ParsedQuery,
    QueryError, SEARCH_TIMEOUT, SearchError, SearchQuery, SearchResults, Sort, parse_query,
    seeder_multiplier,
};
use dc4_store::TorrentRecord;
use serde::Deserialize;

use super::{
    Detail, Shown, encode_query_value, fresh_seeders_at, log_backend_error, show_all, torrent_href,
};
use crate::Backend;
use crate::app::{AppState, routes};
use crate::format::{date, grouped, human_size, plural, rfc3339};
use crate::render::{Flavor, error_response, error_with_query, html};
use crate::search_cache::{CacheKey, Flight, PreSearch};
use crate::telemetry::metric_names;
use crate::templates::{ResultRow, SearchPage, SortLink};
use crate::theme::{Theme, next_from_uri};

/// Raw query-string parameters of the search page and the search API.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct SearchParams {
    pub q: Option<String>,
    pub p: Option<String>,
    pub sort: Option<String>,
    pub per_page: Option<String>,
}

/// Sort orders offered on the results page, with their link text.
const SORT_CHOICES: [(Sort, &str); 5] = [
    (Sort::Relevance, "Relevance"),
    (Sort::Newest, "Newest"),
    (Sort::Size, "Largest"),
    (Sort::Seen, "Most seen"),
    (Sort::Seeders, "Most seeders"),
];

/// A parameter that is present but unusable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BadParam {
    Page,
    Sort,
    PerPage,
}

impl BadParam {
    pub(crate) fn message(self) -> String {
        match self {
            BadParam::Page => format!("The page number must be between 1 and {MAX_PAGE}."),
            BadParam::Sort => {
                "The sort order must be relevance, newest, size, seen or seeders.".into()
            }
            BadParam::PerPage => {
                format!("The page size must be between 1 and {MAX_PER_PAGE}.")
            }
        }
    }
}

/// `p`: 1 when absent or empty.
pub(crate) fn parse_page(raw: Option<&str>) -> Result<u32, BadParam> {
    parse_bounded(raw, 1, MAX_PAGE).ok_or(BadParam::Page)
}

/// `per_page`: [`DEFAULT_PER_PAGE`] when absent or empty.
pub(crate) fn parse_per_page(raw: Option<&str>) -> Result<u32, BadParam> {
    parse_bounded(raw, DEFAULT_PER_PAGE, MAX_PER_PAGE).ok_or(BadParam::PerPage)
}

/// `sort`: relevance when absent or empty.
pub(crate) fn parse_sort(raw: Option<&str>) -> Result<Sort, BadParam> {
    match raw.map(str::trim) {
        None | Some("") => Ok(Sort::default()),
        Some(s) => Sort::parse(s).ok_or(BadParam::Sort),
    }
}

fn parse_bounded(raw: Option<&str>, default: u32, max: u32) -> Option<u32> {
    match raw.map(str::trim) {
        None | Some("") => Some(default),
        Some(s) => s.parse::<u32>().ok().filter(|n| (1..=max).contains(n)),
    }
}

/// True if the trimmed query is longer than the search index accepts.
/// Cheap: at most `4 * MAX_QUERY_CHARS` bytes are counted.
pub(crate) fn too_long(q: &str) -> bool {
    if q.len() <= MAX_QUERY_CHARS {
        return false;
    }
    q.len() > MAX_QUERY_CHARS.saturating_mul(4) || q.chars().count() > MAX_QUERY_CHARS
}

/// The message for a query longer than [`MAX_QUERY_CHARS`].
pub(crate) fn too_long_message() -> String {
    format!("Your search is too long. Use at most {MAX_QUERY_CHARS} characters.")
}

/// Why a search produced no page of results.
#[derive(Debug)]
pub(crate) enum Failure {
    /// The query itself is unusable.
    Query(QueryError),
    /// The index is overloaded or too slow.
    Busy,
    /// The index failed.
    Broken,
    /// The database failed.
    Database,
}

impl Failure {
    pub(crate) fn status(&self) -> StatusCode {
        match self {
            Failure::Query(_) => StatusCode::BAD_REQUEST,
            Failure::Busy | Failure::Database => StatusCode::SERVICE_UNAVAILABLE,
            Failure::Broken => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub(crate) fn message(&self) -> String {
        match self {
            Failure::Query(QueryError::TooLong { .. }) => too_long_message(),
            Failure::Query(QueryError::TooManyTerms { .. }) => {
                format!("Your search has too many words. Use at most {MAX_TERMS}.")
            }
            Failure::Query(QueryError::NoTerms) => "Your search has no words to look for. \
                 Words with a minus sign in front only leave results out."
                .into(),
            Failure::Query(QueryError::PageOutOfRange { .. }) => BadParam::Page.message(),
            Failure::Query(QueryError::PerPageOutOfRange { .. }) => BadParam::PerPage.message(),
            Failure::Busy => "Search is busy right now. Please try again in a moment.".into(),
            Failure::Broken => "Search is not working right now. Please try again later.".into(),
            Failure::Database => {
                "The database is not available right now. Please try again later.".into()
            }
        }
    }
}

/// One page of results: the number of visible matches, and the visible
/// torrents on the page in search-hit order.
///
/// `total` is the index match count minus hits hidden on *this* page
/// (missing or deleted during hydration), so a single-page query cannot
/// be compared against its page to reveal the hidden count. It is exact
/// when all matches fit on one page; on multi-page queries hits hidden on
/// other pages are still counted and `total` may differ per page. Pages
/// are not refilled: a short page already reveals per-page filtering, so
/// `total` claims no more.
/// `index_total` is the unadjusted index count and drives pagination
/// (`has_next`): a page that filters heavily must not hide later pages
/// that still have visible hits.
pub(crate) struct Found {
    pub total: u64,
    pub index_total: u64,
    pub torrents: Vec<(TorrentRecord, Shown)>,
}

/// Searches the index, then loads the hits from the database. Hits that the
/// database no longer shows (deleted) are skipped.
///
/// The index search goes through the shared query cache (`cache.md`): hits
/// skip the index entirely, and concurrent identical misses share one search
/// (singleflight). Hydration (`get_many` + `order_page`) still runs on every
/// request — deletions and the seeder boost are time-dependent.
pub(crate) async fn execute<B: Backend>(
    st: &AppState<B>,
    text: &str,
    parsed: &ParsedQuery,
    page: u32,
    per_page: u32,
    sort: Sort,
) -> Result<Found, Failure> {
    let key = CacheKey {
        text: text.to_owned(),
        sort,
        page,
        per_page,
    };
    match st.cache.pre_search(&key, &st.search) {
        // Disabled: today's uncached path, without even a stamp read.
        None => {
            let (results, _) = run_search(&st.search, text, parsed, page, per_page, sort).await?;
            hydrate(st, &results, sort).await
        }
        Some(PreSearch::Hit(results)) => hydrate(st, &results, sort).await,
        Some(PreSearch::Lead(cell)) => {
            let flight = cell
                .get_or_init(|| run_flight(&st.search, text, parsed, page, per_page, sort))
                .await;
            let flight: Arc<Flight> = Arc::clone(flight);
            st.cache.install(&key, &cell, flight.clone());
            match flight.as_ref() {
                Ok((results, _)) => hydrate(st, results, sort).await,
                Err(failure) => Err(clone_failure(failure)),
            }
        }
        Some(PreSearch::Wait(cell)) => {
            metrics::counter!(metric_names::SEARCH_CACHE_COALESCED).increment(1);
            let flight = cell
                .get_or_init(|| run_flight(&st.search, text, parsed, page, per_page, sort))
                .await;
            match flight.as_ref() {
                Ok((results, _)) => hydrate(st, results, sort).await,
                Err(failure) => Err(clone_failure(failure)),
            }
        }
    }
}

/// One index search for `execute()`, recording `SEARCH_SECONDS`.
/// The cache calls this once per flight (leader only); hits and coalesced
/// waits never record index latency.
async fn run_search(
    search: &dc4_search::SearchHandle,
    text: &str,
    parsed: &ParsedQuery,
    page: u32,
    per_page: u32,
    sort: Sort,
) -> Result<(SearchResults, IndexStamp), Failure> {
    let query = SearchQuery {
        text: text.to_owned(),
        sort,
        page,
        per_page,
    };
    let started = Instant::now();
    let result = search
        .search_parsed(query, parsed.clone(), SEARCH_TIMEOUT)
        .await;
    metrics::histogram!(metric_names::SEARCH_SECONDS).record(started.elapsed().as_secs_f64());
    match result {
        Ok(ok) => Ok(ok),
        Err(SearchError::Query(e)) => Err(Failure::Query(e)),
        Err(SearchError::Timeout | SearchError::Task(_)) => Err(Failure::Busy),
        Err(e) => {
            tracing::error!(kind = error_kind(&e), "search index failed");
            Err(Failure::Broken)
        }
    }
}

/// [`run_search`] shared via `Arc`: `Failure` is `Debug`-only on purpose
/// (query text must never be cloned into logs or metrics), so flights share
/// the outcome without cloning it.
async fn run_flight(
    search: &dc4_search::SearchHandle,
    text: &str,
    parsed: &ParsedQuery,
    page: u32,
    per_page: u32,
    sort: Sort,
) -> Arc<Flight> {
    Arc::new(run_search(search, text, parsed, page, per_page, sort).await)
}

/// Rebuilds an owned [`Failure`] from a shared flight outcome.
/// `QueryError` is `Clone`; the other variants are units.
fn clone_failure(failure: &Failure) -> Failure {
    match failure {
        Failure::Query(e) => Failure::Query(e.clone()),
        Failure::Busy => Failure::Busy,
        Failure::Broken => Failure::Broken,
        Failure::Database => Failure::Database,
    }
}

/// Loads the index hits from the database and ranks one page.
/// Runs on every request, cached or not.
async fn hydrate<B: Backend>(
    st: &AppState<B>,
    results: &SearchResults,
    sort: Sort,
) -> Result<Found, Failure> {
    let ids: Vec<i64> = results.hits.iter().map(|hit| hit.id).collect();
    let records = if ids.is_empty() {
        Vec::new()
    } else {
        match st.backend.get_many(&ids).await {
            Ok(records) => records,
            Err(e) => {
                log_backend_error(&e, "search results");
                return Err(Failure::Database);
            }
        }
    };
    let mut by_id: HashMap<i64, TorrentRecord> = records.into_iter().map(|r| (r.id, r)).collect();
    let ordered = ids.iter().filter_map(|id| by_id.remove(id)).collect();
    let Some(mut torrents) = show_all(ordered, Detail::NameOnly).await else {
        tracing::error!("preparing search results failed");
        return Err(Failure::Broken);
    };
    let scores: HashMap<i64, f32> = results.hits.iter().map(|hit| (hit.id, hit.score)).collect();
    order_page(&mut torrents, &scores, sort, st.seeder_freshness);
    // Hits the database no longer shows (deleted) were skipped above;
    // subtract those on this page so a single-page `total` counts visible
    // matches instead of leaking the hidden count. Pagination still uses
    // the index count: hidden-heavy pages must not hide later pages with
    // visible hits.
    let index_total = results.total;
    let dropped =
        u64::try_from(results.hits.len().saturating_sub(torrents.len())).unwrap_or(u64::MAX);
    Ok(Found {
        total: index_total.saturating_sub(dropped),
        index_total,
        torrents,
    })
}

/// Re-sorts one hydrated page for the seeder orderings (bep33.md §7).
///
/// The index has no seeder field (scrape writes must not churn it), so both
/// apply web-side, after hydration, to this page only (approximate across
/// pages):
///
/// * `Seeders`: estimate descending, unknown last.
/// * `Relevance`: each hit's index score times the seeder boost
///   ([`boosted_at`]), fresh estimates only.
/// * Anything else: untouched (the index already ordered it).
pub(crate) fn order_page(
    torrents: &mut Vec<(TorrentRecord, Shown)>,
    scores: &HashMap<i64, f32>,
    sort: Sort,
    freshness: Duration,
) {
    // One clock reading shared by every comparison below. Sampling `now`
    // per comparison would cost a clock call each time and let the
    // freshness boundary move mid-sort, ranking the same record both ways.
    let now = Utc::now();
    match sort {
        Sort::Seeders => {
            torrents.sort_by(|a, b| {
                fresh_seeders_at(&b.0, freshness, now).cmp(&fresh_seeders_at(&a.0, freshness, now))
            });
        }
        Sort::Relevance => {
            torrents.sort_by(|a, b| {
                boosted_at(&b.0, scores.get(&b.0.id), freshness, now).total_cmp(&boosted_at(
                    &a.0,
                    scores.get(&a.0.id),
                    freshness,
                    now,
                ))
            });
        }
        Sort::Newest | Sort::Size | Sort::Seen => {}
    }
}

/// `score` with the web-side seeder boost (bep33.md §7): fresh estimates
/// multiply by `1 + SEEDER_WEIGHT·log10(1+est)`; anything else is unchanged.
/// Takes an explicit clock reading so page-wide ranking can share one `now`
/// with [`fresh_seeders_at`].
pub(crate) fn boosted_at(
    record: &TorrentRecord,
    score: Option<&f32>,
    freshness: Duration,
    now: DateTime<Utc>,
) -> f32 {
    let base = score.copied().unwrap_or(0.0);
    match fresh_seeders_at(record, freshness, now) {
        Some(est) => {
            #[allow(clippy::cast_possible_truncation)]
            let boost = seeder_multiplier(est) as f32;
            base * boost
        }
        None => base,
    }
}

/// A short name for an index failure. The message is not logged, so no
/// part of the query can reach the logs.
fn error_kind(e: &SearchError) -> &'static str {
    match e {
        SearchError::Index(_) => "index",
        SearchError::Directory(_) => "directory",
        SearchError::Io(_) => "io",
        SearchError::SchemaMismatch => "schema-mismatch",
        _ => "other",
    }
}

/// The results-page URL for `q` at `page` in `sort` order with `per_page` results.
pub(crate) fn search_href(q: &str, page: u32, sort: Sort, per_page: u32) -> String {
    format!(
        "{}?q={}&p={page}&sort={}&per_page={per_page}",
        routes::SEARCH,
        encode_query_value(q),
        encode_query_value(sort.as_str())
    )
}

/// `GET /search?q=&p=&sort=`.
pub(crate) async fn search_page<B: Backend>(
    State(st): State<Arc<AppState<B>>>,
    headers: HeaderMap,
    uri: Uri,
    params: Result<Query<SearchParams>, QueryRejection>,
) -> Response {
    let site = &st.site;
    let theme = Theme::from_headers(&headers);
    let next = next_from_uri(&uri);
    let Ok(Query(params)) = params else {
        return error_response(
            site,
            Flavor::Html,
            StatusCode::BAD_REQUEST,
            "The search address could not be read.",
            theme,
            &next,
        );
    };
    let q = params.q.as_deref().unwrap_or_default().trim();
    if q.is_empty() {
        return Redirect::to(routes::HOME).into_response();
    }
    // Checked first, so over-long queries are refused before parsing.
    // The refused query is not echoed.
    if too_long(q) {
        return error_response(
            site,
            Flavor::Html,
            StatusCode::BAD_REQUEST,
            &too_long_message(),
            theme,
            &next,
        );
    }
    let bad_request = |message: &str| {
        error_with_query(
            site,
            Flavor::Html,
            StatusCode::BAD_REQUEST,
            message,
            q,
            theme,
            &next,
        )
    };
    let page_number = match parse_page(params.p.as_deref()) {
        Ok(n) => n,
        Err(e) => return bad_request(&e.message()),
    };
    let sort = match parse_sort(params.sort.as_deref()) {
        Ok(s) => s,
        Err(e) => return bad_request(&e.message()),
    };
    let per_page = match parse_per_page(params.per_page.as_deref()) {
        Ok(n) => n,
        Err(e) => return bad_request(&e.message()),
    };
    let parsed = match parse_query(q) {
        Ok(parsed) => parsed,
        Err(e) => {
            let failure = Failure::Query(e);
            return error_with_query(
                site,
                Flavor::Html,
                failure.status(),
                &failure.message(),
                q,
                theme,
                &next,
            );
        }
    };

    let found = match execute(&st, q, &parsed, page_number, per_page, sort).await {
        Ok(found) => found,
        Err(failure) => {
            return error_with_query(
                site,
                Flavor::Html,
                failure.status(),
                &failure.message(),
                q,
                theme,
                &next,
            );
        }
    };

    let skipped = u64::from(page_number.saturating_sub(1)).saturating_mul(u64::from(per_page));
    // Pagination follows the index count, not the page-adjusted visible
    // count: a heavily filtered page must still offer the next page.
    let has_next =
        page_number < MAX_PAGE && skipped.saturating_add(u64::from(per_page)) < found.index_total;
    let summary = match found.total {
        0 => "No torrents match your search.".to_owned(),
        1 => "1 torrent matches your search.".to_owned(),
        n => format!("{} torrents match your search.", grouped(n)),
    };
    let sort_links = SORT_CHOICES
        .iter()
        .map(|(choice, label)| SortLink {
            label,
            href: search_href(q, 1, *choice, per_page),
            current: *choice == sort,
        })
        .collect();
    let rows = found
        .torrents
        .into_iter()
        .map(|(record, shown)| result_row(&record, shown, st.seeder_freshness))
        .collect();
    let page = SearchPage {
        page: site.page(format!("{q} - search"), q, true, theme, &next),
        summary,
        sort_links,
        rows,
        first_number: skipped.saturating_add(1),
        page_number,
        prev_href: (page_number > 1)
            .then(|| search_href(q, page_number.saturating_sub(1), sort, per_page)),
        next_href: has_next.then(|| search_href(q, page_number.saturating_add(1), sort, per_page)),
    };
    html(StatusCode::OK, &page)
}

fn result_row(record: &TorrentRecord, shown: Shown, freshness: Duration) -> ResultRow {
    ResultRow {
        href: torrent_href(record),
        name: shown.name,
        size_human: human_size(record.total_size),
        size_exact: grouped(record.total_size),
        files: plural(record.file_count, "file", "files"),
        first_seen: date(record.first_seen_at),
        first_seen_iso: rfc3339(record.first_seen_at),
        seen: plural(record.seen_count, "time", "times"),
        seeders: fresh_seeders_at(record, freshness, Utc::now()).map(grouped),
        magnet: shown.magnet,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_parameter() {
        assert_eq!(parse_page(None), Ok(1));
        assert_eq!(parse_page(Some("")), Ok(1));
        assert_eq!(parse_page(Some(" 7 ")), Ok(7));
        assert_eq!(parse_page(Some("50")), Ok(50));
        for bad in ["0", "51", "-1", "x", "1.5", "99999999999"] {
            assert_eq!(parse_page(Some(bad)), Err(BadParam::Page), "{bad}");
        }
    }

    #[test]
    fn per_page_and_sort_parameters() {
        assert_eq!(parse_per_page(None), Ok(DEFAULT_PER_PAGE));
        assert_eq!(parse_per_page(Some("50")), Ok(50));
        assert_eq!(parse_per_page(Some("51")), Err(BadParam::PerPage));
        assert_eq!(parse_per_page(Some("0")), Err(BadParam::PerPage));
        assert_eq!(parse_sort(None), Ok(Sort::Relevance));
        assert_eq!(parse_sort(Some("newest")), Ok(Sort::Newest));
        assert_eq!(parse_sort(Some("size")), Ok(Sort::Size));
        assert_eq!(parse_sort(Some("seen")), Ok(Sort::Seen));
        assert_eq!(parse_sort(Some("seeders")), Ok(Sort::Seeders));
        assert_eq!(parse_sort(Some("NEWEST")), Err(BadParam::Sort));
        assert_eq!(parse_sort(Some("random")), Err(BadParam::Sort));
    }

    #[test]
    fn hrefs_are_encoded() {
        assert_eq!(
            search_href("a&b \"c\"", 2, Sort::Newest, DEFAULT_PER_PAGE),
            "/search?q=a%26b%20%22c%22&p=2&sort=newest&per_page=20"
        );
    }

    #[test]
    fn hrefs_preserve_per_page() {
        assert_eq!(
            search_href("hello", 2, Sort::Relevance, 5),
            "/search?q=hello&p=2&sort=relevance&per_page=5"
        );
    }

    #[test]
    fn failure_mapping() {
        assert_eq!(
            Failure::Query(QueryError::NoTerms).status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(Failure::Busy.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(Failure::Database.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(Failure::Broken.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(too_long(&"x".repeat(MAX_QUERY_CHARS + 1)));
        assert!(!too_long(&"東".repeat(MAX_QUERY_CHARS)));
    }
}
