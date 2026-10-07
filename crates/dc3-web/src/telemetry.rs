//! Metrics of the web role (`docs/04-operations.md` §6).
//!
//! The binary installs the recorder; this crate only emits.

use metrics::{Unit, describe_counter, describe_gauge, describe_histogram};

/// Metric names.
pub mod metric_names {
    /// Counter `{route, status}`: responses by matched route template.
    pub const HTTP_REQUESTS: &str = "dc3_http_requests_total";
    /// Histogram: time spent in the search index per search, in seconds.
    pub const SEARCH_SECONDS: &str = "dc3_search_seconds";
    /// Counter: search cache lookups served from the cache.
    pub const SEARCH_CACHE_HITS: &str = "dc3_search_cache_hits_total";
    /// Counter `{reason}`: search cache lookups that ran a search
    /// (`absent`, `expired`, or `stamp` for index-commit invalidation).
    pub const SEARCH_CACHE_MISSES: &str = "dc3_search_cache_misses_total";
    /// Counter: requests that shared another request's index search.
    pub const SEARCH_CACHE_COALESCED: &str = "dc3_search_cache_coalesced_total";
    /// Gauge: entries physically in the search cache (may briefly include
    /// lazily-expired-but-unvisited ones).
    pub const SEARCH_CACHE_ENTRIES: &str = "dc3_search_cache_entries";
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
        SEARCH_CACHE_HITS,
        Unit::Count,
        "Search cache lookups served from the cache"
    );
    describe_counter!(
        SEARCH_CACHE_MISSES,
        Unit::Count,
        "Search cache lookups that ran a search, by reason"
    );
    describe_counter!(
        SEARCH_CACHE_COALESCED,
        Unit::Count,
        "Requests that shared another request's index search"
    );
    describe_gauge!(
        SEARCH_CACHE_ENTRIES,
        Unit::Count,
        "Entries physically in the search cache"
    );
    describe_counter!(
        RATE_LIMITED,
        Unit::Count,
        "Requests refused by the rate limiter, by route template"
    );
}
