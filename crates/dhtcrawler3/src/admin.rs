//! Admin subcommands (design §13; 04-operations §3). They connect with the
//! `[database]` credentials, which compose sets to the owner for admin runs.
//!
//! Text that came from visitors or torrents (report messages, contacts,
//! notes) is sanitised before it is printed, so it cannot carry terminal
//! control sequences.

use std::io::Write;

use dc3_core::AnyKey;
use dc3_core::text::sanitize_display;
use dc3_policy::TermMatcher;
use dc3_store::{
    DenyOutcome, DenyReason, MAX_FEED_PAGE, ReportAction, ReportReason, ReportStatus,
    SETTING_AUTOHIDE_PER_HOUR, Store, StoreError,
};

use crate::config::{Config, DbRole};

/// Actor recorded for admin commands.
pub const ADMIN_ACTOR: &str = "cli";
/// Actor recorded for `policy rescan` denials.
pub const RESCAN_ACTOR: &str = "policy-rescan";
/// Note recorded for `policy rescan` denials.
pub const RESCAN_NOTE: &str = "blocked term (policy rescan)";
/// Rows read per `policy rescan` page.
pub const RESCAN_PAGE: i64 = MAX_FEED_PAGE;
/// Default number of rows listed.
pub const DEFAULT_LIST_LIMIT: i64 = 100;
/// Days shown by `stats`.
pub const STATS_DAYS: u32 = 7;
/// Characters of free text shown per field in listings.
pub const LIST_TEXT_CHARS: usize = 200;

/// Why an admin command failed.
#[derive(Debug, thiserror::Error)]
pub enum AdminError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("cannot write output: {0}")]
    Output(#[from] std::io::Error),
    #[error("{0}")]
    Refused(String),
}

type Out<'a> = &'a mut (dyn Write + Send);

/// The raw bytes of a key: 20 for a DHT or v1 key, 32 for a v2 infohash.
pub fn key_bytes(key: &AnyKey) -> Vec<u8> {
    match key {
        AnyKey::V1OrDht(k) => k.0.to_vec(),
        AnyKey::V2(h) => h.0.to_vec(),
    }
}

/// Lowercase hex of a key.
pub fn key_hex(key: &AnyKey) -> String {
    match key {
        AnyKey::V1OrDht(k) => k.to_hex(),
        AnyKey::V2(h) => h.to_hex(),
    }
}

fn clean(text: &str) -> String {
    sanitize_display(text, LIST_TEXT_CHARS)
}

/// `migrate`: applies the migrations and stores `web.autohide_per_hour`.
pub async fn migrate(
    store: &Store,
    autohide_per_hour: u32,
    out: Out<'_>,
) -> Result<(), AdminError> {
    store.migrate().await?;
    let changed = store
        .set_setting(SETTING_AUTOHIDE_PER_HOUR, &autohide_per_hour.to_string())
        .await?;
    tracing::info!(autohide_per_hour, changed, "database migrated");
    writeln!(
        out,
        "migrations applied; {SETTING_AUTOHIDE_PER_HOUR} = {autohide_per_hour}{}",
        if changed { " (updated)" } else { "" }
    )?;
    Ok(())
}

/// `deny add`.
pub async fn deny_add(
    store: &Store,
    key: &AnyKey,
    reason: DenyReason,
    note: Option<&str>,
    out: Out<'_>,
) -> Result<DenyOutcome, AdminError> {
    let outcome = store
        .deny(&key_bytes(key), reason, note, ADMIN_ACTOR)
        .await?;
    writeln!(
        out,
        "{} {} ({reason}); {} torrent(s) removed",
        key_hex(key),
        if outcome.newly_denied {
            "denied"
        } else {
            "was already denied"
        },
        outcome.tombstoned
    )?;
    Ok(outcome)
}

/// `deny remove`. It is an error if the key was not denied.
pub async fn deny_remove(store: &Store, key: &AnyKey, out: Out<'_>) -> Result<(), AdminError> {
    if !store.undeny(&key_bytes(key), ADMIN_ACTOR).await? {
        return Err(AdminError::Refused(format!(
            "{} is not on the denylist",
            key_hex(key)
        )));
    }
    writeln!(
        out,
        "{} removed from the denylist (torrents already removed stay removed until fetched again)",
        key_hex(key)
    )?;
    Ok(())
}

/// `deny list`.
pub async fn deny_list(store: &Store, limit: i64, out: Out<'_>) -> Result<(), AdminError> {
    let entries = store.list_denied(limit, 0).await?;
    writeln!(
        out,
        "{:<64}  {:<9}  {:<20}  {:<14}  note",
        "key", "reason", "created", "by"
    )?;
    for e in &entries {
        writeln!(
            out,
            "{:<64}  {:<9}  {:<20}  {:<14}  {}",
            key_hex(&e.key),
            e.reason.as_str(),
            e.created_at.format("%Y-%m-%d %H:%M:%S").to_string(),
            clean(&e.created_by),
            clean(e.note.as_deref().unwrap_or(""))
        )?;
    }
    writeln!(out, "{} entr{}", entries.len(), plural_y(entries.len()))?;
    Ok(())
}

fn plural_y(n: usize) -> &'static str {
    if n == 1 { "y" } else { "ies" }
}

/// `reports list`.
pub async fn reports_list(
    store: &Store,
    status: Option<ReportStatus>,
    limit: i64,
    out: Out<'_>,
) -> Result<(), AdminError> {
    let reports = store.list_reports(status, limit).await?;
    for r in &reports {
        writeln!(
            out,
            "#{} {} {} {} key={} torrent={}",
            r.id,
            r.created_at.format("%Y-%m-%d %H:%M:%S"),
            r.status,
            r.reason,
            key_hex(&r.key),
            r.torrent_id
                .map_or_else(|| "-".to_owned(), |id| id.to_string())
        )?;
        if !r.message.is_empty() {
            writeln!(out, "    message: {}", clean(&r.message))?;
        }
        if let Some(contact) = r.contact.as_deref().filter(|c| !c.is_empty()) {
            writeln!(out, "    contact: {}", clean(contact))?;
        }
        if let Some(at) = r.resolved_at {
            writeln!(
                out,
                "    resolved {} by {}{}",
                at.format("%Y-%m-%d %H:%M:%S"),
                clean(r.resolved_by.as_deref().unwrap_or("?")),
                r.resolution_note
                    .as_deref()
                    .map(|n| format!(": {}", clean(n)))
                    .unwrap_or_default()
            )?;
        }
    }
    writeln!(out, "{} report(s)", reports.len())?;
    Ok(())
}

/// What `reports resolve` does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// Deny the key; without a reason, one is derived from the report.
    Deny(Option<DenyReason>),
    Dismiss,
}

/// The denial reason that fits a report reason.
pub fn deny_reason_for(report: ReportReason) -> DenyReason {
    match report {
        ReportReason::Csam => DenyReason::Csam,
        ReportReason::Copyright => DenyReason::Dmca,
        ReportReason::Malware => DenyReason::Abuse,
        ReportReason::Other => DenyReason::Other,
    }
}

/// `reports resolve`.
pub async fn reports_resolve(
    store: &Store,
    id: i64,
    resolution: Resolution,
    note: Option<&str>,
    out: Out<'_>,
) -> Result<(), AdminError> {
    let report = store
        .get_report(id)
        .await?
        .ok_or_else(|| AdminError::Refused(format!("report #{id} does not exist")))?;
    let action = match resolution {
        Resolution::Dismiss => ReportAction::Dismiss,
        Resolution::Deny(reason) => {
            ReportAction::Deny(reason.unwrap_or_else(|| deny_reason_for(report.reason)))
        }
    };
    store.resolve_report(id, action, note, ADMIN_ACTOR).await?;
    match action {
        ReportAction::Dismiss => writeln!(out, "report #{id} dismissed")?,
        ReportAction::Deny(reason) => writeln!(
            out,
            "report #{id} actioned: {} denied ({reason})",
            key_hex(&report.key)
        )?,
    }
    Ok(())
}

/// Counts from `policy rescan`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RescanSummary {
    pub scanned: u64,
    pub matched: u64,
    pub newly_denied: u64,
    pub removed: u64,
}

/// `policy rescan`: denies every stored torrent whose name or any stored
/// path matches `policy`.
pub async fn policy_rescan(
    store: &Store,
    policy: &TermMatcher,
    out: Out<'_>,
) -> Result<RescanSummary, AdminError> {
    let mut summary = RescanSummary::default();
    let mut after = 0i64;
    loop {
        let rows = store.scan_live(after, RESCAN_PAGE).await?;
        let Some(last) = rows.last() else { break };
        after = last.id;
        for row in &rows {
            summary.scanned = summary.scanned.saturating_add(1);
            let matched = policy.matches(&row.name) || row.paths.iter().any(|p| policy.matches(p));
            if !matched {
                continue;
            }
            summary.matched = summary.matched.saturating_add(1);
            let outcome = store
                .deny(
                    row.dht_key.as_bytes(),
                    DenyReason::CsamAuto,
                    Some(RESCAN_NOTE),
                    RESCAN_ACTOR,
                )
                .await?;
            if outcome.newly_denied {
                summary.newly_denied = summary.newly_denied.saturating_add(1);
            }
            summary.removed = summary.removed.saturating_add(outcome.tombstoned);
        }
    }
    tracing::info!(
        scanned = summary.scanned,
        matched = summary.matched,
        newly_denied = summary.newly_denied,
        removed = summary.removed,
        "policy rescan finished"
    );
    writeln!(
        out,
        "scanned {} torrent(s); {} matched; {} key(s) newly denied; {} torrent(s) removed",
        summary.scanned, summary.matched, summary.newly_denied, summary.removed
    )?;
    Ok(summary)
}

/// `stats`.
pub async fn stats(store: &Store, out: Out<'_>) -> Result<(), AdminError> {
    let s = store.stats().await?;
    writeln!(out, "torrents      {}", s.torrents)?;
    writeln!(out, "pending       {}", s.pending)?;
    writeln!(out, "gave up       {}", s.gave_up)?;
    writeln!(out, "denylisted    {}", s.denylisted)?;
    writeln!(out, "open reports  {}", s.open_reports)?;
    writeln!(out)?;
    writeln!(
        out,
        "{:<10}  {:>12}  {:>12}  {:>12}  {:>12}",
        "day (UTC)", "discovered", "fetched", "failed", "blocked"
    )?;
    for d in store.daily_stats(STATS_DAYS).await? {
        writeln!(
            out,
            "{:<10}  {:>12}  {:>12}  {:>12}  {:>12}",
            d.day.to_string(),
            d.discovered,
            d.fetched,
            d.fetch_failed,
            d.blocked
        )?;
    }
    Ok(())
}

/// `check-config`: prints the configuration (passwords are never read or
/// shown) and the warnings.
pub fn check_config(cfg: &Config, out: Out<'_>) -> Result<(), AdminError> {
    let text = cfg
        .to_redacted_toml()
        .map_err(|e| AdminError::Refused(e.to_string()))?;
    writeln!(
        out,
        "# Effective configuration (environment overrides applied)"
    )?;
    writeln!(out, "{}", text.trim_end())?;
    writeln!(out)?;
    for (label, role) in [
        ("single-role and admin commands", DbRole::Main),
        ("all: crawl", DbRole::Crawler),
        ("all: index", DbRole::Indexer),
        ("all: web", DbRole::Web),
    ] {
        let (user, password_file) = cfg.credentials(role);
        let password = if password_file.as_os_str().is_empty() {
            "no password file".to_owned()
        } else {
            format!("password from {}", password_file.display())
        };
        writeln!(out, "# database user for {label}: {user} ({password})")?;
    }
    let warnings = cfg.warnings();
    if warnings.is_empty() {
        writeln!(out, "# no warnings")?;
    }
    for w in warnings {
        writeln!(out, "warning: {w}")?;
    }
    Ok(())
}

/// Parses a key given on the command line.
pub fn parse_key(text: &str) -> Result<AnyKey, String> {
    text.trim()
        .parse::<AnyKey>()
        .map_err(|e| format!("not a 40-hex, 32-base32 or 64-hex key: {e}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        let v1 = parse_key(&"AB".repeat(20)).unwrap();
        assert_eq!(key_bytes(&v1), vec![0xab; 20]);
        assert_eq!(key_hex(&v1), "ab".repeat(20));
        let v2 = parse_key(&"cd".repeat(32)).unwrap();
        assert_eq!(key_bytes(&v2).len(), 32);
        assert!(parse_key("xyz").is_err());
        assert!(parse_key(&"a".repeat(41)).is_err());
    }

    #[test]
    fn report_reasons_map_to_denials() {
        assert_eq!(deny_reason_for(ReportReason::Csam), DenyReason::Csam);
        assert_eq!(deny_reason_for(ReportReason::Copyright), DenyReason::Dmca);
        assert_eq!(deny_reason_for(ReportReason::Malware), DenyReason::Abuse);
        assert_eq!(deny_reason_for(ReportReason::Other), DenyReason::Other);
    }

    #[test]
    fn check_config_output() {
        let mut cfg = Config::default();
        cfg.database.url = Some("postgres://u:topsecret@h/db".into());
        let mut out = Vec::new();
        check_config(&cfg, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains("topsecret"), "{text}");
        assert!(text.contains("u:REDACTED@h/db"), "{text}");
        assert!(text.contains("[crawl]"), "{text}");
        assert!(
            text.contains("warning: web.trusted_proxies is empty"),
            "{text}"
        );
        assert!(
            text.contains("password from /run/secrets/db_password"),
            "{text}"
        );
        // The printed configuration parses back to the same values.
        let body: String = text
            .lines()
            .filter(|l| !l.starts_with('#') && !l.starts_with("warning:"))
            .map(|l| format!("{l}\n"))
            .collect();
        let mut parsed = crate::config::from_toml_str(&body, Vec::new()).unwrap();
        parsed.database.url = cfg.database.url.clone();
        assert_eq!(parsed, cfg);
    }

    #[test]
    fn listings_sanitise_text() {
        assert_eq!(clean("a\u{1b}[31mred\u{202e}x\ny"), "a[31mredx y");
        assert_eq!(clean(&"z".repeat(500)).len(), LIST_TEXT_CHARS);
    }
}
