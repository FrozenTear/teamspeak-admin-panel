//! `ssh_audit_log` retention policy parser + periodic sweep task (PURA-79
//! R1 / R6).
//!
//! Operator-tunable via `app_setting:ssh_audit_retention_days` (seeded by
//! migration `0006_ssh_audit_log.surql`). Parser semantics — exactly the
//! shape SecurityEngineer signed off on:
//!
//! | Raw value          | Behaviour                                   |
//! |--------------------|---------------------------------------------|
//! | empty / unset      | [`DEFAULT_RETENTION_DAYS`] (365)            |
//! | `'0'`              | unbounded; startup `WARN` logged            |
//! | `'1'..='29'`       | clamp to [`RETENTION_FLOOR_DAYS`] (30) + WARN |
//! | `>= 30`            | honored                                      |
//! | unparseable / `<0` | fall back to default + WARN                  |
//!
//! Why a 30-day floor: a misclick to "1 day" otherwise silently destroys
//! forensic evidence — that's the soft-fail mode SecurityEngineer closed
//! in R1. `0` stays available as the explicit "never prune" opt-in for
//! operators who want unbounded retention.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio::time::MissedTickBehavior;

use crate::db::Database;
use crate::repos::{app_settings, ssh_audit_log};

pub const APP_SETTING_KEY: &str = "ssh_audit_retention_days";
pub const DEFAULT_RETENTION_DAYS: u32 = 365;
pub const RETENTION_FLOOR_DAYS: u32 = 30;

/// One sweep per hour. Sweep is idempotent and cheap when there's nothing
/// to prune (single SELECT with `LIMIT 1000` returning zero rows).
const SWEEP_INTERVAL: Duration = Duration::from_secs(3600);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetentionPolicy {
    /// Prune everything older than `days`. Always >= [`RETENTION_FLOOR_DAYS`]
    /// (or [`DEFAULT_RETENTION_DAYS`] when unset).
    Days(u32),
    /// Operator opted into "never prune" by setting the value to `'0'`.
    Unbounded,
}

/// Apply the R1 parser semantics. Logs the operator-visible soft-fail modes
/// (clamped, unbounded, parse-failed) so an operator who hits one knows.
pub fn parse_retention(raw: Option<&str>) -> RetentionPolicy {
    let trimmed = raw.unwrap_or("").trim();
    if trimmed.is_empty() {
        return RetentionPolicy::Days(DEFAULT_RETENTION_DAYS);
    }
    match trimmed.parse::<i64>() {
        Ok(0) => {
            tracing::warn!(
                target: "sshbridge::audit::retention",
                raw = %trimmed,
                "ssh_audit_retention_days = 0 — audit log will never be pruned. \
                 Set a positive integer (>= {}) to re-enable pruning.",
                RETENTION_FLOOR_DAYS
            );
            RetentionPolicy::Unbounded
        }
        Ok(n) if n < 0 => {
            tracing::warn!(
                target: "sshbridge::audit::retention",
                raw = %trimmed,
                "ssh_audit_retention_days < 0 — falling back to default {} days",
                DEFAULT_RETENTION_DAYS
            );
            RetentionPolicy::Days(DEFAULT_RETENTION_DAYS)
        }
        Ok(n) if (n as u32) < RETENTION_FLOOR_DAYS => {
            tracing::warn!(
                target: "sshbridge::audit::retention",
                raw = %trimmed,
                clamped_to = RETENTION_FLOOR_DAYS,
                "ssh_audit_retention_days = {n} < floor {} days — clamping to floor. \
                 A retention < 30d silently destroys forensic evidence; use 0 if you \
                 explicitly want unbounded retention.",
                RETENTION_FLOOR_DAYS
            );
            RetentionPolicy::Days(RETENTION_FLOOR_DAYS)
        }
        Ok(n) => RetentionPolicy::Days(n as u32),
        Err(e) => {
            tracing::warn!(
                target: "sshbridge::audit::retention",
                raw = %trimmed,
                error = %e,
                "ssh_audit_retention_days unparseable — falling back to default {} days",
                DEFAULT_RETENTION_DAYS
            );
            RetentionPolicy::Days(DEFAULT_RETENTION_DAYS)
        }
    }
}

pub async fn load_policy(db: &Database) -> RetentionPolicy {
    let raw = match app_settings::get(db, APP_SETTING_KEY).await {
        Ok(Some(row)) => Some(row.value),
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(
                target: "sshbridge::audit::retention",
                error = %e,
                "failed to read app_setting:{APP_SETTING_KEY} — falling back to default"
            );
            None
        }
    };
    parse_retention(raw.as_deref())
}

/// Spawn the hourly retention sweep. The first tick fires immediately, so
/// the boot path logs the operator-set policy and clears any stale rows
/// from a previous incarnation.
pub fn spawn_sweep(db: Arc<Database>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(SWEEP_INTERVAL);
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            let policy = load_policy(&db).await;
            let cutoff_days = match policy {
                RetentionPolicy::Days(d) => d,
                RetentionPolicy::Unbounded => continue,
            };
            let cutoff = Utc::now() - chrono::Duration::days(cutoff_days as i64);
            match ssh_audit_log::prune_older_than(&db, cutoff).await {
                Ok(n) => {
                    if n > 0 {
                        tracing::info!(
                            target: "sshbridge::audit::retention",
                            pruned = n,
                            days = cutoff_days,
                            "ssh_audit_log retention sweep pruned {n} row(s)"
                        );
                    }
                }
                Err(e) => tracing::warn!(
                    target: "sshbridge::audit::retention",
                    error = %e,
                    "ssh_audit_log retention sweep failed"
                ),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_retention_unset_uses_default() {
        assert_eq!(
            parse_retention(None),
            RetentionPolicy::Days(DEFAULT_RETENTION_DAYS)
        );
        assert_eq!(
            parse_retention(Some("")),
            RetentionPolicy::Days(DEFAULT_RETENTION_DAYS)
        );
        assert_eq!(
            parse_retention(Some("   ")),
            RetentionPolicy::Days(DEFAULT_RETENTION_DAYS)
        );
    }

    #[test]
    fn parse_retention_zero_is_unbounded() {
        assert_eq!(parse_retention(Some("0")), RetentionPolicy::Unbounded);
    }

    #[test]
    fn parse_retention_below_floor_is_clamped() {
        // SecurityEngineer's R1: 1..=29 → clamp to 30.
        assert_eq!(
            parse_retention(Some("1")),
            RetentionPolicy::Days(RETENTION_FLOOR_DAYS)
        );
        assert_eq!(
            parse_retention(Some("29")),
            RetentionPolicy::Days(RETENTION_FLOOR_DAYS)
        );
    }

    #[test]
    fn parse_retention_at_or_above_floor_is_honored() {
        assert_eq!(parse_retention(Some("30")), RetentionPolicy::Days(30));
        assert_eq!(parse_retention(Some("365")), RetentionPolicy::Days(365));
        assert_eq!(parse_retention(Some("3650")), RetentionPolicy::Days(3650));
    }

    #[test]
    fn parse_retention_negative_falls_back_to_default() {
        assert_eq!(
            parse_retention(Some("-5")),
            RetentionPolicy::Days(DEFAULT_RETENTION_DAYS)
        );
    }

    #[test]
    fn parse_retention_unparseable_falls_back_to_default() {
        assert_eq!(
            parse_retention(Some("forever")),
            RetentionPolicy::Days(DEFAULT_RETENTION_DAYS)
        );
        assert_eq!(
            parse_retention(Some("1.5")),
            RetentionPolicy::Days(DEFAULT_RETENTION_DAYS)
        );
    }
}
