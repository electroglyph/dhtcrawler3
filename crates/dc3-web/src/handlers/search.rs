//! `GET /search` and the search logic the JSON API shares.
//!
//! Search text is never logged. A query containing a blocked term is not
//! searched at all.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use dc3_search::{
    DEFAULT_PER_PAGE, MAX_PAGE, MAX_PER_PAGE, MAX_QUERY_CHARS, MAX_TERMS, ParsedQuery, QueryError,
    SEARCH_TIMEOUT, SearchError, SearchQuery, Sort, parse_query,
};
use dc3_store::TorrentRecord;
use serde::Deserialize;

use super::{Detail, Shown, encode_query_value, log_backend_error, show_all, torrent_href};
use crate::Backend;
use crate::app::{AppState, routes};
use crate::format::{date, grouped, human_size, plural, rfc3339};
use crate::render::{Flavor, error_response, error_with_query, html};
use crate::telemetry::metric_names;
use crate::templates::{BlockedPage, ResultRow, SearchPage, SortLink};

/// Raw query-string parameters of the search page and the search API.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct SearchParams {
    pub q: Option<String>,
    pub p: Option<String>,
    pub sort: Option<String>,
    pub per_page: Option<String>,
}

/// Sort orders offered on the results page, with their link text.
const SORT_CHOICES: [(Sort, &str); 4] = [
    (Sort::Relevance, "Relevance"),
    (Sort::Newest, "Newest"),
    (Sort::Size, "Largest"),
    (Sort::Seen, "Most seen"),
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
            BadParam::Sort => "The sort order must be relevance, newest, size or seen.".into(),
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

/// True (and counted) if `q` contains a blocked term.
pub(crate) fn is_blocked<B>(st: &AppState<B>, q: &str) -> bool {
    let blocked = st.policy.matches(q);
    if blocked {
        metrics::counter!(metric_names::BLOCKED_QUERIES).increment(1);
    }
    blocked
}

/// True (and counted) when the query's trailing prefix word expands to an
/// indexed term the whole-token gate cannot see but the policy denies.
/// Only single-token prefixes are enumerated; longer words search as
/// phrases. Runs after parameter validation, so blocked and clean queries
/// answer bad parameters alike. A broken index is not a block: the search
/// itself reports it.
pub(crate) async fn expansion_blocked<B: Backend>(st: &AppState<B>, parsed: &ParsedQuery) -> bool {
    let single = match parsed.words.last() {
        Some(last) if last.prefix => match last.tokens.as_slice() {
            [single] => single.clone(),
            _ => return false,
        },
        _ => return false,
    };
    let expansions = match st.search.prefix_expansions(single.clone(), SEARCH_TIMEOUT).await {
        Ok(expansions) => expansions,
        Err(_) => return false,
    };
    let blocked = expansions
        .iter()
        .any(|term| st.policy.matches_affixed(term))
        || st.policy.matches_affixed(&single);
    if blocked {
        metrics::counter!(metric_names::BLOCKED_QUERIES).increment(1);
    }
    blocked
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

/// One page of results: the total number of matches, and the visible
/// torrents on the page in search-hit order.
pub(crate) struct Found {
    pub total: u64,
    pub torrents: Vec<(TorrentRecord, Shown)>,
}

/// Searches the index, then loads the hits from the database. Hits that the
/// database no longer shows (hidden, denied, deleted) or that contain a
/// blocked term are skipped.
pub(crate) async fn execute<B: Backend>(
    st: &AppState<B>,
    text: &str,
    parsed: &ParsedQuery,
    page: u32,
    per_page: u32,
    sort: Sort,
) -> Result<Found, Failure> {
    let query = SearchQuery {
        text: text.to_owned(),
        sort,
        page,
        per_page,
    };
    let started = Instant::now();
    let result = st
        .search
        .search_parsed(query, parsed.clone(), SEARCH_TIMEOUT)
        .await;
    metrics::histogram!(metric_names::SEARCH_SECONDS).record(started.elapsed().as_secs_f64());
    let results = match result {
        Ok(results) => results,
        Err(SearchError::Query(e)) => return Err(Failure::Query(e)),
        Err(SearchError::Timeout | SearchError::Task(_)) => return Err(Failure::Busy),
        Err(e) => {
            tracing::error!(kind = error_kind(&e), "search index failed");
            return Err(Failure::Broken);
        }
    };

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
    let Some(torrents) = show_all(ordered, &st.policy, Detail::NameOnly).await else {
        tracing::error!("preparing search results failed");
        return Err(Failure::Broken);
    };
    Ok(Found {
        total: results.total,
        torrents,
    })
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

/// The results-page URL for `q` at `page` in `sort` order.
pub(crate) fn search_href(q: &str, page: u32, sort: Sort) -> String {
    format!(
        "{}?q={}&p={page}&sort={}",
        routes::SEARCH,
        encode_query_value(q),
        encode_query_value(sort.as_str())
    )
}

/// `GET /search?q=&p=&sort=`.
pub(crate) async fn search_page<B: Backend>(
    State(st): State<Arc<AppState<B>>>,
    params: Result<Query<SearchParams>, QueryRejection>,
) -> Response {
    let site = &st.site;
    let Ok(Query(params)) = params else {
        return error_response(
            site,
            Flavor::Html,
            StatusCode::BAD_REQUEST,
            "The search address could not be read.",
        );
    };
    let q = params.q.as_deref().unwrap_or_default().trim();
    if q.is_empty() {
        return Redirect::to(routes::HOME).into_response();
    }
    // Checked first, so the term matcher never sees more than the limit.
    // The refused query is not echoed: it has not been matched.
    if too_long(q) {
        return error_response(
            site,
            Flavor::Html,
            StatusCode::BAD_REQUEST,
            &too_long_message(),
        );
    }
    // Parameters validate before either gate, so blocked and clean queries
    // answer bad parameters alike. The refused query is not echoed.
    let bad_request =
        |message: &str| error_with_query(site, Flavor::Html, StatusCode::BAD_REQUEST, message, q);
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
    if is_blocked(&st, q) {
        let page = BlockedPage {
            page: site.page("Search blocked", "", true),
        };
        return html(StatusCode::OK, &page);
    }
    // Parsed once and shared by the prefix gate and the search (B-005);
    // a query that fails to parse is answered exactly as `execute` answered
    // it before (the gate treats it as not blocked).
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
            );
        }
    };
    if expansion_blocked(&st, &parsed).await {
        let page = BlockedPage {
            page: site.page("Search blocked", "", true),
        };
        return html(StatusCode::OK, &page);
    }

    let found = match execute(&st, q, &parsed, page_number, per_page, sort).await {
        Ok(found) => found,
        Err(failure) => {
            return error_with_query(site, Flavor::Html, failure.status(), &failure.message(), q);
        }
    };

    let skipped = u64::from(page_number.saturating_sub(1)).saturating_mul(u64::from(per_page));
    let has_next =
        page_number < MAX_PAGE && skipped.saturating_add(u64::from(per_page)) < found.total;
    let summary = match found.total {
        0 => "No torrents match your search.".to_owned(),
        1 => "1 torrent matches your search.".to_owned(),
        n => format!("{} torrents match your search.", grouped(n)),
    };
    let sort_links = SORT_CHOICES
        .iter()
        .map(|(choice, label)| SortLink {
            label,
            href: search_href(q, 1, *choice),
            current: *choice == sort,
        })
        .collect();
    let rows = found
        .torrents
        .into_iter()
        .map(|(record, shown)| result_row(&record, shown))
        .collect();
    let page = SearchPage {
        page: site.page(format!("{q} - search"), q, true),
        summary,
        sort_links,
        rows,
        first_number: skipped.saturating_add(1),
        page_number,
        prev_href: (page_number > 1).then(|| search_href(q, page_number.saturating_sub(1), sort)),
        next_href: has_next.then(|| search_href(q, page_number.saturating_add(1), sort)),
    };
    html(StatusCode::OK, &page)
}

fn result_row(record: &TorrentRecord, shown: Shown) -> ResultRow {
    ResultRow {
        href: torrent_href(record),
        name: shown.name,
        size_human: human_size(record.total_size),
        size_exact: grouped(record.total_size),
        files: plural(record.file_count, "file", "files"),
        first_seen: date(record.first_seen_at),
        first_seen_iso: rfc3339(record.first_seen_at),
        seen: plural(record.seen_count, "time", "times"),
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
        assert_eq!(parse_sort(Some("NEWEST")), Err(BadParam::Sort));
        assert_eq!(parse_sort(Some("random")), Err(BadParam::Sort));
    }

    #[test]
    fn hrefs_are_encoded() {
        assert_eq!(
            search_href("a&b \"c\"", 2, Sort::Newest),
            "/search?q=a%26b%20%22c%22&p=2&sort=newest"
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
