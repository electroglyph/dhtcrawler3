//! Page templates (askama, HTML auto-escaping).
//!
//! Every value reaches the page through `{{ … }}` and is escaped. No
//! template marks anything as safe, contains script, or uses inline styles.

use askama::Template;

/// What the shared layout needs.
pub(crate) struct PageMeta<'a> {
    pub site_name: &'a str,
    pub style_path: &'static str,
    /// The `<title>` before the site name.
    pub title: String,
    /// Prefills the header search box.
    pub query: &'a str,
    /// Show the search box in the header.
    pub header_search: bool,
    /// `maxlength` of search boxes.
    pub max_query_chars: usize,
}

/// Home-page totals, already formatted.
pub(crate) struct StatsView {
    pub torrents: String,
    pub added_today: String,
    pub added_yesterday: String,
}

#[derive(Template)]
#[template(path = "home.html")]
pub(crate) struct HomePage<'a> {
    pub page: PageMeta<'a>,
    pub stats: Option<StatsView>,
}

/// One search result.
pub(crate) struct ResultRow {
    pub href: String,
    pub name: String,
    pub size_human: String,
    pub size_exact: String,
    pub files: String,
    pub first_seen: String,
    pub first_seen_iso: String,
    pub seen: String,
    /// Fresh seeder estimate, if any (stale estimates are hidden).
    pub seeders: Option<String>,
    pub magnet: Option<String>,
}

/// A link that re-runs the search in another order.
pub(crate) struct SortLink {
    pub label: &'static str,
    pub href: String,
    pub current: bool,
}

#[derive(Template)]
#[template(path = "search.html")]
pub(crate) struct SearchPage<'a> {
    pub page: PageMeta<'a>,
    pub summary: String,
    pub sort_links: Vec<SortLink>,
    pub rows: Vec<ResultRow>,
    pub first_number: u64,
    pub page_number: u32,
    pub prev_href: Option<String>,
    pub next_href: Option<String>,
}

#[derive(Template)]
#[template(path = "blocked.html")]
pub(crate) struct BlockedPage<'a> {
    pub page: PageMeta<'a>,
}

/// The facts shown about one torrent.
pub(crate) struct TorrentView {
    pub name: String,
    pub dht_key: String,
    pub info_hash_v1: Option<String>,
    pub info_hash_v2: Option<String>,
    pub size_human: String,
    pub size_exact: String,
    pub file_count: String,
    pub first_seen: String,
    pub first_seen_iso: String,
    pub last_seen: String,
    pub last_seen_iso: String,
    pub seen_count: String,
    /// Fresh seeder estimate, if any (stale estimates are hidden).
    pub seeders: Option<String>,
    /// When the shown estimate was scraped, if one is shown.
    pub scraped: Option<String>,
    pub scraped_iso: Option<String>,
    pub magnet: Option<String>,
    pub report_href: String,
}

/// One row of a torrent's file list.
pub(crate) struct FileView {
    pub path: String,
    pub size_human: String,
    pub size_exact: String,
}

#[derive(Template)]
#[template(path = "torrent.html")]
pub(crate) struct TorrentPage<'a> {
    pub page: PageMeta<'a>,
    pub t: TorrentView,
    pub files: Vec<FileView>,
    pub files_note: Option<String>,
}

/// A report reason radio button.
pub(crate) struct ReasonChoice {
    pub value: &'static str,
    pub label: &'static str,
    pub checked: bool,
}

#[derive(Template)]
#[template(path = "report.html")]
pub(crate) struct ReportPage<'a> {
    pub page: PageMeta<'a>,
    pub key: String,
    pub action: String,
    pub torrent_href: String,
    pub reasons: Vec<ReasonChoice>,
    pub message: &'a str,
    pub contact: &'a str,
    pub error: Option<&'a str>,
    pub message_max: usize,
    pub contact_max: usize,
}

#[derive(Template)]
#[template(path = "report_done.html")]
pub(crate) struct ReportDonePage<'a> {
    pub page: PageMeta<'a>,
    pub csam: bool,
}

#[derive(Template)]
#[template(path = "about.html")]
pub(crate) struct AboutPage<'a> {
    pub page: PageMeta<'a>,
}

/// A contact address as a `mailto:` link.
pub(crate) struct EmailView {
    pub href: String,
    pub text: String,
}

#[derive(Template)]
#[template(path = "legal.html")]
pub(crate) struct LegalPage<'a> {
    pub page: PageMeta<'a>,
    pub contact: Option<EmailView>,
    pub dmca_agent: Option<&'a str>,
}

#[derive(Template)]
#[template(path = "privacy.html")]
pub(crate) struct PrivacyPage<'a> {
    pub page: PageMeta<'a>,
    pub stored_files: String,
    pub rate_limit_minutes: u64,
}

#[derive(Template)]
#[template(path = "error.html")]
pub(crate) struct ErrorPage<'a> {
    pub page: PageMeta<'a>,
    pub heading: &'a str,
    pub message: &'a str,
}
