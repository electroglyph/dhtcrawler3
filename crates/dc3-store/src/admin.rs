//! Admin operations: denylist management, report review, settings, totals
//! and the audit log. Every method here needs the `dc3_owner` user.

use std::borrow::Cow;

use crate::crawler::deny_in_tx;
use crate::types::{DenyEntry, Report, ReportAction, ReportReason, ReportStatus, StoreStats};
use crate::{
    MAX_NUMERIC_SETTING, MAX_PAGE, MAX_SETTING_KEY_CHARS, MAX_SETTING_VALUE_CHARS,
    NUMERIC_SETTINGS, Result, SETTINGS_ACTOR, SQLSTATE_CHECK_VIOLATION, Store, StoreError,
    check_actor, check_key_len, check_len, check_note, db_message, get, hex_of,
    is_numeric_setting_value, live_torrents, lock_change_shared, sqlstate, to_u64, write_audit,
};

/// `SELECT` of every [`Report`] column.
macro_rules! report_select {
    () => {
        "SELECT id, torrent_id, dht_key, reason, message, contact, created_at, status, \
         resolved_at, resolved_by, resolution_note FROM reports"
    };
}

impl Store {
    /// Removes `key` from the denylist and writes the audit log. Returns false
    /// when the key was not denied.
    ///
    /// Torrents tombstoned by the denial stay tombstoned (their content was
    /// wiped); they are restored only if the crawler fetches them again.
    pub async fn undeny(&self, key: &[u8], actor: &str) -> Result<bool> {
        check_key_len(key)?;
        check_actor(actor)?;
        let mut tx = self.pool.begin().await?;
        let removed: Option<String> =
            sqlx::query_scalar("DELETE FROM denylist WHERE key = $1 RETURNING reason")
                .bind(key)
                .fetch_optional(&mut *tx)
                .await?;
        let detail = serde_json::json!({ "removed": removed.is_some(), "reason": removed });
        write_audit(&mut tx, actor, "undeny", &hex_of(key), &detail).await?;
        tx.commit().await?;
        Ok(removed.is_some())
    }

    /// Denylist entries, newest first.
    pub async fn list_denied(&self, limit: i64, offset: i64) -> Result<Vec<DenyEntry>> {
        if limit <= 0 {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(
            "SELECT key, reason, note, created_at, created_by FROM denylist \
             ORDER BY created_at DESC, key LIMIT $1 OFFSET $2",
        )
        .bind(limit.min(MAX_PAGE))
        .bind(offset.max(0))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(DenyEntry::from_row).collect()
    }

    /// Reports with the given status (all when `None`), oldest first.
    pub async fn list_reports(
        &self,
        status: Option<ReportStatus>,
        limit: i64,
    ) -> Result<Vec<Report>> {
        if limit <= 0 {
            return Ok(Vec::new());
        }
        let sql = match status {
            Some(_) => concat!(
                report_select!(),
                " WHERE status = $1 ORDER BY created_at, id LIMIT $2"
            ),
            None => concat!(
                report_select!(),
                " WHERE $1::text IS NULL ORDER BY created_at, id LIMIT $2"
            ),
        };
        let rows = sqlx::query(sql)
            .bind(status.map(|s| s.as_str()))
            .bind(limit.min(MAX_PAGE))
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(Report::from_row).collect()
    }

    /// One report by id.
    pub async fn get_report(&self, id: i64) -> Result<Option<Report>> {
        let row = sqlx::query(concat!(report_select!(), " WHERE id = $1"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(Report::from_row).transpose()
    }

    /// Resolves an open report in one transaction.
    ///
    /// * [`ReportAction::Deny`] denies the reported key (as [`Store::deny`])
    ///   and marks the report actioned.
    /// * [`ReportAction::Dismiss`] marks it dismissed. If the report is about
    ///   a stored torrent, sets the torrent's `reviewed_at` (so it is not
    ///   hidden again automatically) and un-hides it unless another CSAM
    ///   report about it is still open.
    ///
    /// Fails with [`StoreError::NotFound`] or [`StoreError::ReportNotOpen`].
    pub async fn resolve_report(
        &self,
        id: i64,
        action: ReportAction,
        note: Option<&str>,
        actor: &str,
    ) -> Result<()> {
        check_note(note)?;
        check_actor(actor)?;
        let mut tx = self.pool.begin().await?;
        // Both actions may bump change_seq, so the change lock comes first.
        lock_change_shared(&mut tx).await?;
        let row =
            sqlx::query("SELECT status, torrent_id, dht_key FROM reports WHERE id = $1 FOR UPDATE")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(StoreError::NotFound)?;
        let status: String = get(&row, "status")?;
        if status != ReportStatus::Open.as_str() {
            return Err(StoreError::ReportNotOpen(id));
        }
        let torrent_id: Option<i64> = get(&row, "torrent_id")?;
        let key: Vec<u8> = get(&row, "dht_key")?;

        let new_status = match action {
            ReportAction::Deny(reason) => {
                deny_in_tx(&mut tx, &key, reason, note, actor).await?;
                ReportStatus::Actioned
            }
            ReportAction::Dismiss => ReportStatus::Dismissed,
        };
        sqlx::query(
            "UPDATE reports SET status = $2, resolved_at = now(), resolved_by = $3, resolution_note = $4 \
             WHERE id = $1",
        )
        .bind(id)
        .bind(new_status.as_str())
        .bind(actor)
        .bind(note)
        .execute(&mut *tx)
        .await?;

        let mut unhidden = false;
        let mut reviewed = false;
        if let (ReportAction::Dismiss, Some(tid)) = (action, torrent_id) {
            // Lock the torrent first: a concurrent submit_report holds this
            // row while it inserts, so the check below sees its report.
            let hidden: Option<Option<chrono::DateTime<chrono::Utc>>> =
                sqlx::query_scalar("SELECT hidden_at FROM torrents WHERE id = $1 FOR UPDATE")
                    .bind(tid)
                    .fetch_optional(&mut *tx)
                    .await?;
            if let Some(hidden_at) = hidden {
                let other_open: bool = sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM reports r WHERE r.torrent_id = $1 \
                     AND r.status = 'open' AND r.reason = $2 AND r.id <> $3)",
                )
                .bind(tid)
                .bind(ReportReason::Csam.as_str())
                .bind(id)
                .fetch_one(&mut *tx)
                .await?;
                if hidden_at.is_some() && !other_open {
                    sqlx::query(
                        "UPDATE torrents SET hidden_at = NULL, change_seq = nextval('change_seq') \
                         WHERE id = $1",
                    )
                    .bind(tid)
                    .execute(&mut *tx)
                    .await?;
                    unhidden = true;
                }
                sqlx::query("UPDATE torrents SET reviewed_at = now() WHERE id = $1")
                    .bind(tid)
                    .execute(&mut *tx)
                    .await?;
                reviewed = true;
            }
        }

        let action_name = match action {
            ReportAction::Deny(_) => "report-deny",
            ReportAction::Dismiss => "report-dismiss",
        };
        let detail = serde_json::json!({
            "report_id": id,
            "torrent_id": torrent_id,
            "status": new_status.as_str(),
            "deny_reason": match action { ReportAction::Deny(r) => Some(r.as_str()), ReportAction::Dismiss => None },
            "unhidden": unhidden,
            "reviewed": reviewed,
            "note": note,
        });
        write_audit(
            &mut tx,
            actor,
            action_name,
            &format!("report:{id}"),
            &detail,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Appends an audit-log entry.
    pub async fn audit(
        &self,
        actor: &str,
        action: &str,
        subject: &str,
        detail: &serde_json::Value,
    ) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        write_audit(&mut conn, actor, action, subject, detail).await
    }

    /// Totals for the `stats` command. `torrents` is exact while the planner
    /// estimates at most [`crate::EXACT_COUNT_THRESHOLD`] rows and the
    /// estimate above that; the other counts are exact.
    pub async fn stats(&self) -> Result<StoreStats> {
        let torrents = live_torrents(&self.pool).await?;
        let row = sqlx::query(
            "SELECT (SELECT count(*) FROM pending WHERE NOT gave_up) AS pending, \
                    (SELECT count(*) FROM pending WHERE gave_up) AS gave_up, \
                    (SELECT count(*) FROM denylist) AS denylisted, \
                    (SELECT count(*) FROM reports WHERE status = 'open') AS open_reports",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(StoreStats {
            torrents: to_u64("torrents", torrents)?,
            pending: to_u64("pending", get(&row, "pending")?)?,
            gave_up: to_u64("gave_up", get(&row, "gave_up")?)?,
            denylisted: to_u64("denylisted", get(&row, "denylisted")?)?,
            open_reports: to_u64("open_reports", get(&row, "open_reports")?)?,
        })
    }

    /// The value of a setting, or `None` when it is not set.
    pub async fn get_setting(&self, key: &str) -> Result<Option<String>> {
        let value: Option<String> = sqlx::query_scalar("SELECT value FROM settings WHERE key = $1")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;
        Ok(value)
    }

    /// Sets a setting and, when the value changes, writes a `set-setting`
    /// audit row with the old and new values. Returns whether it changed.
    ///
    /// [`crate::SETTING_AUTOHIDE_PER_HOUR`] and
    /// [`crate::SETTING_OPEN_REPORTS_CAP`] take a decimal integer in
    /// `0..=MAX_NUMERIC_SETTING`, without sign or leading spaces.
    pub async fn set_setting(&self, key: &str, value: &str) -> Result<bool> {
        check_len("setting key", key, 1, MAX_SETTING_KEY_CHARS)?;
        check_len("setting value", value, 0, MAX_SETTING_VALUE_CHARS)?;
        if NUMERIC_SETTINGS.contains(&key) && !is_numeric_setting_value(value) {
            return Err(StoreError::Invalid(format!(
                "setting {key} must be a whole number in 0..={MAX_NUMERIC_SETTING}"
            )));
        }
        let mut tx = self.pool.begin().await?;
        let old: Option<String> =
            sqlx::query_scalar("SELECT value FROM settings WHERE key = $1 FOR UPDATE")
                .bind(key)
                .fetch_optional(&mut *tx)
                .await?;
        if old.as_deref() == Some(value) {
            tx.rollback().await?;
            return Ok(false);
        }
        sqlx::query(
            "INSERT INTO settings (key, value) VALUES ($1, $2) \
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(key)
        .bind(value)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            let code = sqlstate(&e).map(Cow::into_owned);
            if code.as_deref() == Some(SQLSTATE_CHECK_VIOLATION) {
                StoreError::Invalid(db_message(&e))
            } else {
                StoreError::Database(e)
            }
        })?;
        let detail = serde_json::json!({ "old": old, "new": value });
        write_audit(&mut tx, SETTINGS_ACTOR, "set-setting", key, &detail).await?;
        tx.commit().await?;
        Ok(true)
    }
}
