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
}
