//! Metrics of the web role (`docs/04-operations.md` §6).
//!
//! The binary installs the recorder; this crate only emits.

use metrics::{Unit, describe_counter, describe_histogram};

/// Metric names.
pub mod metric_names {
    /// Counter `{route, status}`: responses by matched route template.
    pub const HTTP_REQUESTS: &str = "dc3_http_requests_total";
    /// Histogram: time spent in the search index per search, in seconds.
    pub const SEARCH_SECONDS: &str = "dc3_search_seconds";
    /// Counter `{route}`: requests refused by the rate limiter.
    pub const RATE_LIMITED: &str = "dc3_rate_limited_total";
    /// Counter: searches refused by the blocked-term policy.
    pub const BLOCKED_QUERIES: &str = "dc3_blocked_queries_total";
    /// Counter `{reason}`: reports stored.
    pub const REPORTS: &str = "dc3_reports_total";
    /// Counter: torrents hidden automatically by a CSAM report.
    pub const AUTOHIDE: &str = "dc3_autohide_total";
    /// Counter: CSAM reports that would have hidden a torrent but found the
    /// hourly budget used up.
    pub const AUTOHIDE_BUDGET_EXHAUSTED: &str = "dc3_autohide_budget_exhausted_total";
}

/// Registers help texts for the metrics above with the installed recorder.
pub fn describe_metrics() {
    use metric_names::*;
    describe_counter!(
        HTTP_REQUESTS,
        Unit::Count,
        "HTTP responses by route template and status"
    );
    describe_histogram!(
        SEARCH_SECONDS,
        Unit::Seconds,
        "Time spent searching the index"
    );
    describe_counter!(
        RATE_LIMITED,
        Unit::Count,
        "Requests refused by the rate limiter, by route template"
    );
    describe_counter!(
        BLOCKED_QUERIES,
        Unit::Count,
        "Searches refused because they contain a blocked term"
    );
    describe_counter!(REPORTS, Unit::Count, "Reports stored, by reason");
    describe_counter!(
        AUTOHIDE,
        Unit::Count,
        "Torrents hidden automatically by a CSAM report"
    );
    describe_counter!(
        AUTOHIDE_BUDGET_EXHAUSTED,
        Unit::Count,
        "CSAM reports that did not hide a torrent because the hourly budget was used up"
    );
}
