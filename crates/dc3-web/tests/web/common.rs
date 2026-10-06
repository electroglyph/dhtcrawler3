//! Shared test helpers: an in-memory backend, a metrics recorder, a real
//! search index in a temporary directory, and request helpers.

use std::collections::{BTreeSet, HashMap};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use chrono::{DateTime, TimeZone, Utc};
use dc3_core::{AnyKey, DhtKey, InfoHashV2};
use dc3_policy::TermMatcher;
use dc3_search::{IndexDoc, IndexRoot, SearchHandle};
use dc3_store::{FileRow, NewReport, PublicStats, SubmitOutcome, TorrentRecord};
use dc3_web::{Backend, BackendError, WebConfig, WebDeps, router};
use metrics::{
    Counter, Gauge, Histogram, HistogramFn, Key, KeyName, Metadata, Recorder, SharedString, Unit,
};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use tempfile::TempDir;
use tower::ServiceExt;

pub const BASE_URL: &str = "https://search.example.org";
pub const SITE_NAME: &str = "Test <Search> & Co";
pub const CLIENT: &str = "198.51.100.23:50000";
/// A synthetic blocked term (and a phrase) for the tests.
pub const BLOCKED_TERMS: &str = "zzforbiddenzz\nbad phrase\n";
pub const CSP: &str = "default-src 'none'; style-src 'self'; img-src 'self'; \
form-action 'self'; frame-ancestors 'none'; base-uri 'none'";
const WRITER_HEAP: usize = 20 * 1024 * 1024;
const MAX_TEST_BODY: usize = 64 * 1024 * 1024;

// ------------------------------------------------------------------ serial

static SERIAL: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(|| tokio::sync::Mutex::new(()));

/// Tests share one global metrics recorder, so they run one at a time.
pub async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL.lock().await
}

// ----------------------------------------------------------------- metrics

#[derive(Default)]
pub struct TestRecorder {
    counters: Mutex<HashMap<String, Arc<AtomicU64>>>,
    histograms: Mutex<HashMap<String, Arc<HistogramCount>>>,
}

#[derive(Default)]
struct HistogramCount(AtomicU64);

impl HistogramFn for HistogramCount {
    fn record(&self, _value: f64) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn key_text(key: &Key) -> String {
    let labels: BTreeSet<String> = key
        .labels()
        .map(|l| format!("{}={}", l.key(), l.value()))
        .collect();
    if labels.is_empty() {
        key.name().to_owned()
    } else {
        let joined: Vec<String> = labels.into_iter().collect();
        format!("{}{{{}}}", key.name(), joined.join(","))
    }
}

impl TestRecorder {
    /// The value of a counter, e.g. `dc3_reports_total{reason=csam}`.
    pub fn counter(&self, key: &str) -> u64 {
        self.counters
            .lock()
            .unwrap()
            .get(key)
            .map_or(0, |c| c.load(Ordering::SeqCst))
    }

    /// How many values a histogram has recorded.
    pub fn histogram_count(&self, key: &str) -> u64 {
        self.histograms
            .lock()
            .unwrap()
            .get(key)
            .map_or(0, |h| h.0.load(Ordering::SeqCst))
    }
}

impl Recorder for TestRecorder {
    fn describe_counter(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn describe_gauge(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn describe_histogram(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}

    fn register_counter(&self, key: &Key, _: &Metadata<'_>) -> Counter {
        let counter = Arc::clone(
            self.counters
                .lock()
                .unwrap()
                .entry(key_text(key))
                .or_default(),
        );
        Counter::from_arc(counter)
    }

    fn register_gauge(&self, _: &Key, _: &Metadata<'_>) -> Gauge {
        Gauge::noop()
    }

    fn register_histogram(&self, key: &Key, _: &Metadata<'_>) -> Histogram {
        let histogram = Arc::clone(
            self.histograms
                .lock()
                .unwrap()
                .entry(key_text(key))
                .or_default(),
        );
        Histogram::from_arc(histogram)
    }
}

static RECORDER: OnceLock<Arc<TestRecorder>> = OnceLock::new();

/// The process-wide test recorder, installed on first use.
pub fn metrics() -> &'static TestRecorder {
    RECORDER.get_or_init(|| {
        let recorder = Arc::new(TestRecorder::default());
        metrics::set_global_recorder(Arc::clone(&recorder))
            .unwrap_or_else(|_| panic!("a metrics recorder was already installed"));
        recorder
    })
}

// ----------------------------------------------------------------- backend

/// What the fake backend's `submit_report` does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReportBehaviour {
    #[default]
    Stored,
    Hidden,
    BudgetExhausted,
    Full,
    Invalid,
    Down,
}

#[derive(Default)]
pub struct FakeState {
    pub torrents: Mutex<Vec<TorrentRecord>>,
    pub get_many_calls: AtomicUsize,
    pub get_by_key_calls: AtomicUsize,
    pub reports: Mutex<Vec<NewReport>>,
    pub behaviour: Mutex<ReportBehaviour>,
    pub ping_fails: AtomicBool,
    pub reads_fail: AtomicBool,
    pub stats: Mutex<Option<PublicStats>>,
}

/// An in-memory [`Backend`].
#[derive(Clone, Default)]
pub struct FakeBackend(pub Arc<FakeState>);

impl FakeBackend {
    pub fn with(torrents: Vec<TorrentRecord>) -> FakeBackend {
        let backend = FakeBackend::default();
        *backend.0.torrents.lock().unwrap() = torrents;
        backend
    }

    pub fn set_behaviour(&self, behaviour: ReportBehaviour) {
        *self.0.behaviour.lock().unwrap() = behaviour;
    }

    pub fn reports(&self) -> Vec<NewReport> {
        self.0.reports.lock().unwrap().clone()
    }

    pub fn get_many_calls(&self) -> usize {
        self.0.get_many_calls.load(Ordering::SeqCst)
    }

    pub fn get_by_key_calls(&self) -> usize {
        self.0.get_by_key_calls.load(Ordering::SeqCst)
    }

    /// Hides the torrent with this id, as a CSAM report or denial would.
    pub fn hide(&self, id: i64) {
        for t in self.0.torrents.lock().unwrap().iter_mut() {
            if t.id == id {
                t.hidden_at = Some(Utc::now());
            }
        }
    }

    fn unavailable() -> BackendError {
        BackendError::Unavailable("fake database is down".into())
    }
}

impl Backend for FakeBackend {
    async fn get_by_key(&self, key: AnyKey) -> Result<Option<TorrentRecord>, BackendError> {
        self.0.get_by_key_calls.fetch_add(1, Ordering::SeqCst);
        if self.0.reads_fail.load(Ordering::SeqCst) {
            return Err(FakeBackend::unavailable());
        }
        let torrents = self.0.torrents.lock().unwrap();
        let found = torrents.iter().find(|t| match key {
            AnyKey::V1OrDht(k) => t.dht_key == k || t.info_hash_v1 == Some(k),
            AnyKey::V2(h) => t.info_hash_v2 == Some(h),
        });
        Ok(found.filter(|t| t.is_live()).cloned())
    }

    async fn get_many(&self, ids: &[i64]) -> Result<Vec<TorrentRecord>, BackendError> {
        self.0.get_many_calls.fetch_add(1, Ordering::SeqCst);
        if self.0.reads_fail.load(Ordering::SeqCst) {
            return Err(FakeBackend::unavailable());
        }
        let torrents = self.0.torrents.lock().unwrap();
        Ok(ids
            .iter()
            .filter_map(|id| torrents.iter().find(|t| t.id == *id && t.is_live()))
            .map(|t| {
                let mut t = t.clone();
                t.files.truncate(dc3_store::GET_MANY_MAX_FILES);
                t
            })
            .collect())
    }

    async fn submit_report(&self, report: &NewReport) -> Result<SubmitOutcome, BackendError> {
        let behaviour = *self.0.behaviour.lock().unwrap();
        let outcome = |hidden, budget_exhausted| {
            let mut reports = self.0.reports.lock().unwrap();
            reports.push(report.clone());
            SubmitOutcome {
                report_id: i64::try_from(reports.len()).unwrap(),
                hidden,
                budget_exhausted,
            }
        };
        match behaviour {
            ReportBehaviour::Stored => Ok(outcome(false, false)),
            ReportBehaviour::Hidden => Ok(outcome(true, false)),
            ReportBehaviour::BudgetExhausted => Ok(outcome(false, true)),
            ReportBehaviour::Full => Err(BackendError::ReportsFull),
            ReportBehaviour::Invalid => Err(BackendError::Invalid("rejected".into())),
            ReportBehaviour::Down => Err(FakeBackend::unavailable()),
        }
    }

    async fn public_stats(&self) -> Result<PublicStats, BackendError> {
        (*self.0.stats.lock().unwrap()).ok_or_else(FakeBackend::unavailable)
    }

    async fn ping(&self) -> Result<(), BackendError> {
        if self.0.ping_fails.load(Ordering::SeqCst) {
            Err(FakeBackend::unavailable())
        } else {
            Ok(())
        }
    }
}

// ---------------------------------------------------------------- torrents

pub fn key_for(id: i64) -> DhtKey {
    let mut bytes = [0u8; 20];
    bytes[..8].copy_from_slice(&id.to_be_bytes());
    bytes[19] = 0x5a;
    DhtKey(bytes)
}

pub fn v2_for(id: i64) -> InfoHashV2 {
    let mut bytes = [0u8; 32];
    bytes[..8].copy_from_slice(&id.to_be_bytes());
    bytes[31] = 0xa5;
    InfoHashV2(bytes)
}

pub fn at(day: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, day, 12, 30, 0).unwrap()
}

/// A visible v1 torrent.
pub fn torrent(id: i64, name: &str, files: &[(&str, u64)]) -> TorrentRecord {
    let key = key_for(id);
    TorrentRecord {
        id,
        dht_key: key,
        info_hash_v1: Some(key),
        info_hash_v2: None,
        name: name.to_owned(),
        total_size: files.iter().map(|(_, s)| s).sum(),
        file_count: u64::try_from(files.len()).unwrap(),
        files: files
            .iter()
            .map(|(p, s)| FileRow {
                path: (*p).to_owned(),
                size: *s,
            })
            .collect(),
        files_truncated: false,
        piece_length: Some(16384),
        seen_count: 7,
        first_seen_at: at(1),
        last_seen_at: at(15),
        last_scraped_at: None,
        seeders_est: None,
        change_seq: 99,
        hidden_at: None,
        reviewed_at: None,
        deleted_at: None,
    }
}

/// A hybrid torrent (both hashes).
pub fn hybrid(id: i64, name: &str) -> TorrentRecord {
    let mut t = torrent(id, name, &[("a.bin", 2048)]);
    t.info_hash_v2 = Some(v2_for(id));
    t
}

// ------------------------------------------------------------------- index

/// A real search index in a temporary directory.
pub struct TestIndex {
    pub search: SearchHandle,
    /// Keeps the index directory alive.
    _dir: TempDir,
}

pub fn index_of(torrents: &[TorrentRecord]) -> TestIndex {
    let dir = tempfile::tempdir().unwrap();
    let root = IndexRoot::open(dir.path()).unwrap();
    let index = root.open_current().unwrap();
    let mut writer = index.writer(WRITER_HEAP).unwrap();
    for t in torrents {
        let files: Vec<&str> = t.files.iter().map(|f| f.path.as_str()).collect();
        writer
            .upsert(&IndexDoc {
                id: t.id,
                name: t.name.clone(),
                files: files.join("\n"),
                size: t.total_size,
                created: t.first_seen_at.timestamp(),
                seen: t.seen_count,
                file_count: t.file_count,
            })
            .unwrap();
    }
    writer.commit(1).unwrap();
    writer.wait_merging_threads().unwrap();
    let search = SearchHandle::open(dir.path()).unwrap();
    TestIndex { search, _dir: dir }
}

// --------------------------------------------------------------------- app

pub fn config() -> WebConfig {
    WebConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        base_url: BASE_URL.into(),
        site_name: SITE_NAME.into(),
        contact_email: "abuse@example.org".into(),
        dmca_agent: "Example DMCA Agent\n1 Example Street".into(),
        hsts: true,
        trusted_proxies: Vec::new(),
        seeder_freshness: Duration::from_secs(7 * 24 * 60 * 60),
    }
}

pub fn policy() -> Arc<TermMatcher> {
    Arc::new(TermMatcher::load(BLOCKED_TERMS).unwrap())
}

pub struct TestApp {
    pub router: Router,
    pub backend: FakeBackend,
    /// Keeps the search index alive.
    _index: TestIndex,
}

pub fn app(torrents: Vec<TorrentRecord>) -> TestApp {
    app_with(torrents, config())
}

pub fn app_with(torrents: Vec<TorrentRecord>, cfg: WebConfig) -> TestApp {
    metrics();
    let index = index_of(&torrents);
    let backend = FakeBackend::with(torrents);
    let router = router(
        cfg,
        WebDeps {
            backend: backend.clone(),
            search: index.search.clone(),
            policy: policy(),
        },
    );
    TestApp {
        router,
        backend,
        _index: index,
    }
}

// ---------------------------------------------------------------- requests

pub struct TestResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: String,
}

impl TestResponse {
    pub fn header(&self, name: &str) -> &str {
        self.headers
            .get(name)
            .unwrap_or_else(|| panic!("missing header {name}"))
            .to_str()
            .unwrap()
    }

    pub fn json(&self) -> serde_json::Value {
        assert_eq!(self.header("content-type"), "application/json");
        serde_json::from_str(&self.body).unwrap()
    }
}

/// Percent-encodes a query-string value.
pub fn enc(value: &str) -> String {
    utf8_percent_encode(value, NON_ALPHANUMERIC).to_string()
}

pub fn request(method: Method, uri: &str, body: Body, peer: &str) -> Request<Body> {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .body(body)
        .unwrap();
    req.extensions_mut()
        .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
    req
}

pub fn get(uri: &str) -> Request<Body> {
    request(Method::GET, uri, Body::empty(), CLIENT)
}

pub fn get_from(uri: &str, peer: &str) -> Request<Body> {
    request(Method::GET, uri, Body::empty(), peer)
}

/// A report form post with the given headers and body.
pub fn report_post(key: &str, headers: &[(&str, &str)], body: &str, peer: &str) -> Request<Body> {
    let mut req = request(
        Method::POST,
        &format!("/report/{key}"),
        Body::from(body.to_owned()),
        peer,
    );
    let h = req.headers_mut();
    h.insert(
        "content-type",
        "application/x-www-form-urlencoded".parse().unwrap(),
    );
    h.insert("content-length", body.len().to_string().parse().unwrap());
    for (name, value) in headers {
        h.insert(
            axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            value.parse().unwrap(),
        );
    }
    req
}

/// Headers of a browser same-origin POST: fetch metadata plus origin.
pub const SAME_ORIGIN_HEADERS: [(&str, &str); 2] =
    [("sec-fetch-site", "same-origin"), ("origin", BASE_URL)];

pub async fn send(router: &Router, req: Request<Body>) -> TestResponse {
    let response = router.clone().oneshot(req).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), MAX_TEST_BODY).await.unwrap();
    TestResponse {
        status,
        headers,
        body: String::from_utf8(bytes.to_vec()).unwrap(),
    }
}

// -------------------------------------------------------------- assertions

pub fn assert_security_headers(r: &TestResponse, hsts: bool) {
    let h = &r.headers;
    let context = format!("status {}", r.status);
    assert_eq!(h["content-security-policy"], CSP, "{context}");
    assert_eq!(h["x-content-type-options"], "nosniff", "{context}");
    assert_eq!(h["referrer-policy"], "no-referrer", "{context}");
    assert_eq!(
        h["permissions-policy"], "camera=(), microphone=(), geolocation=()",
        "{context}"
    );
    assert_eq!(h["cross-origin-opener-policy"], "same-origin", "{context}");
    assert_eq!(
        h["cross-origin-resource-policy"], "same-origin",
        "{context}"
    );
    assert_eq!(h["x-frame-options"], "DENY", "{context}");
    if hsts {
        assert_eq!(
            h["strict-transport-security"], "max-age=31536000; includeSubDomains",
            "{context}"
        );
    } else {
        assert!(h.get("strict-transport-security").is_none(), "{context}");
    }
    assert!(h.get("server").is_none(), "{context}");
    assert!(h.get("cache-control").is_some(), "{context}");
    for (name, _) in h {
        assert!(
            !name.as_str().starts_with("access-control-"),
            "CORS header {name} ({context})"
        );
    }
}

/// Tags the templates may produce.
const ALLOWED_TAGS: &[&str] = &[
    "!doctype", "html", "head", "meta", "title", "link", "body", "a", "header", "div", "form",
    "label", "input", "button", "main", "footer", "nav", "ul", "ol", "li", "h1", "h2", "p", "span",
    "time", "bdi", "section", "dl", "dt", "dd", "code", "table", "thead", "tbody", "tr", "th",
    "td", "article", "fieldset", "legend", "textarea",
];

/// Checks that a page contains only the markup the templates produce: known
/// tags, no event-handler or style attributes, no `javascript:` URLs, no
/// bidi overrides, and no `<script`.
pub fn assert_safe_html(body: &str) {
    assert!(
        !body.to_ascii_lowercase().contains("<script"),
        "page contains <script"
    );
    assert!(!body.contains('\u{202E}'), "page contains a bidi override");
    let mut rest = body;
    while let Some(start) = rest.find('<') {
        let after = &rest[start + 1..];
        let end = after.find('>').expect("unterminated tag");
        let tag = &after[..end];
        check_tag(tag);
        rest = &after[end + 1..];
    }
}

fn check_tag(tag: &str) {
    let inner = tag.strip_prefix('/').unwrap_or(tag);
    let name_len = inner
        .find(|c: char| c.is_whitespace())
        .unwrap_or(inner.len());
    let name = inner[..name_len].to_ascii_lowercase();
    assert!(
        ALLOWED_TAGS.contains(&name.as_str()),
        "unexpected tag <{tag}>"
    );
    assert!(
        !tag.to_ascii_lowercase().contains("javascript:"),
        "javascript: URL in <{tag}>"
    );
    for attribute in attribute_names(&inner[name_len..]) {
        let attribute = attribute.to_ascii_lowercase();
        assert!(
            !attribute.starts_with("on"),
            "event handler {attribute} in <{tag}>"
        );
        assert_ne!(attribute, "style", "inline style in <{tag}>");
        assert!(
            attribute
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-'),
            "odd attribute {attribute:?} in <{tag}>"
        );
    }
}

/// Attribute names of a tag body whose values are all double-quoted.
fn attribute_names(mut s: &str) -> Vec<String> {
    let mut names = Vec::new();
    loop {
        s = s.trim_start();
        if s.is_empty() {
            return names;
        }
        let end = s
            .find(|c: char| c == '=' || c.is_whitespace())
            .unwrap_or(s.len());
        names.push(s[..end].to_owned());
        s = &s[end..];
        if let Some(value) = s.strip_prefix('=') {
            let value = value
                .strip_prefix('"')
                .expect("attribute values are double-quoted");
            let close = value.find('"').expect("unterminated attribute value");
            s = &value[close + 1..];
        }
    }
}
