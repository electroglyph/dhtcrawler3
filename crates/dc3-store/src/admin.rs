//! Admin operations: settings, totals and the audit log. Every method here
//! needs the `dc3_owner` user.

use std::borrow::Cow;

use crate::types::StoreStats;
use crate::{
    MAX_NUMERIC_SETTING, MAX_SETTING_KEY_CHARS, MAX_SETTING_VALUE_CHARS, NUMERIC_SETTINGS, Result,
    SETTINGS_ACTOR, SQLSTATE_CHECK_VIOLATION, Store, StoreError, check_len, db_message, get,
    is_numeric_setting_value, live_torrents, sqlstate, to_u64, write_audit,
};

impl Store {
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
                    (SELECT count(*) FROM pending WHERE gave_up) AS gave_up",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(StoreStats {
            torrents: to_u64("torrents", torrents)?,
            pending: to_u64("pending", get(&row, "pending")?)?,
            gave_up: to_u64("gave_up", get(&row, "gave_up")?)?,
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
    /// Keys in [`crate::NUMERIC_SETTINGS`] take a decimal integer in
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
