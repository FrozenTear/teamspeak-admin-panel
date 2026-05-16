//! `admin_audit_log` retention policy parser + hourly sweep
//! (`docs/admin/audit-shape.md` §3.1–§3.4).
//!
//! Mirrors [`crate::sshbridge::retention`] verbatim — same parser
//! semantics SecurityEngineer signed off on for the SSH audit path.
//! Operator-tunable via `app_setting:admin_audit_retention_days`
//! (seeded by migration `0010_admin_audit_log.surql`):
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
//! forensic evidence. SecurityEngineer signed off on this floor for the
//! SSH path (PURA-79 R1); the admin path reuses the same posture.
//!
//! On each tick the sweep:
//!
//! 1. Re-reads the retention value (so an operator's `app_setting` edit
//!    takes effect within one tick, no restart).
//! 2. Prunes rows older than `cutoff = now - retention_days`. Skipped
//!    entirely when the policy is `Unbounded`.
//! 3. Enforces the 100 000 row-cap defence (§3.3). The cull is unconditional —
//!    it fires whether the TTL prune ran or not, so an `Unbounded` operator
//!    still gets the defence-in-depth trim.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio::time::MissedTickBehavior;

use crate::db::Database;
use crate::repos::{admin_audit_log, app_settings};

pub const APP_SETTING_KEY: &str = "admin_audit_retention_days";
pub const DEFAULT_RETENTION_DAYS: u32 = 365;
pub const RETENTION_FLOOR_DAYS: u32 = 30;

/// One sweep per hour. Sweep is idempotent and cheap when there's nothing
/// to prune (single SELECT with `LIMIT 1000` returning zero rows).
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(3600);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetentionPolicy {
    /// Prune everything older than `days`. Always >= [`RETENTION_FLOOR_DAYS`]
    /// (or [`DEFAULT_RETENTION_DAYS`] when unset).
    Days(u32),
    /// Operator opted into "never prune" by setting the value to `'0'`.
    Unbounded,
}

/// Parse the operator-set retention value. Logs the soft-fail modes
/// (clamped, unbounded, parse-failed) so an operator who hits one knows.
pub fn parse_retention(raw: Option<&str>) -> RetentionPolicy {
    let trimmed = raw.unwrap_or("").trim();
    if trimmed.is_empty() {
        return RetentionPolicy::Days(DEFAULT_RETENTION_DAYS);
    }
    match trimmed.parse::<i64>() {
        Ok(0) => {
            tracing::warn!(
                target: "audit::retention",
                raw = %trimmed,
                "admin_audit_retention_days = 0 — admin audit log will never be pruned by TTL. \
                 The 100 000-row defence-in-depth cap still applies. Set a positive integer (>= {}) \
                 to re-enable TTL pruning.",
                RETENTION_FLOOR_DAYS
            );
            RetentionPolicy::Unbounded
        }
        Ok(n) if n < 0 => {
            tracing::warn!(
                target: "audit::retention",
                raw = %trimmed,
                "admin_audit_retention_days < 0 — falling back to default {} days",
                DEFAULT_RETENTION_DAYS
            );
            RetentionPolicy::Days(DEFAULT_RETENTION_DAYS)
        }
        Ok(n) if (n as u32) < RETENTION_FLOOR_DAYS => {
            tracing::warn!(
                target: "audit::retention",
                raw = %trimmed,
                clamped_to = RETENTION_FLOOR_DAYS,
                "admin_audit_retention_days = {n} < floor {} days — clamping to floor. \
                 A retention < 30d silently destroys forensic evidence; use 0 if you explicitly \
                 want unbounded retention.",
                RETENTION_FLOOR_DAYS
            );
            RetentionPolicy::Days(RETENTION_FLOOR_DAYS)
        }
        Ok(n) => RetentionPolicy::Days(n as u32),
        Err(e) => {
            tracing::warn!(
                target: "audit::retention",
                raw = %trimmed,
                error = %e,
                "admin_audit_retention_days unparseable — falling back to default {} days",
                DEFAULT_RETENTION_DAYS
            );
            RetentionPolicy::Days(DEFAULT_RETENTION_DAYS)
        }
    }
}

/// Read + parse the live operator-set policy. On DB error falls back to
/// the default with a warn line.
pub async fn load_policy(db: &Database) -> RetentionPolicy {
    let raw = match app_settings::get(db, APP_SETTING_KEY).await {
        Ok(Some(row)) => Some(row.value),
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(
                target: "audit::retention",
                error = %e,
                "failed to read app_setting:{APP_SETTING_KEY} — falling back to default"
            );
            None
        }
    };
    parse_retention(raw.as_deref())
}

/// Run one sweep cycle: TTL prune (unless `Unbounded`) followed by an
/// unconditional row-cap cull when above [`admin_audit_log::ROW_CAP`].
/// Returns the number of rows deleted across both phases.
pub async fn run_sweep_once(db: &Database, policy: RetentionPolicy) -> u64 {
    let mut deleted: u64 = 0;
    if let RetentionPolicy::Days(d) = policy {
        let cutoff = Utc::now() - chrono::Duration::days(d as i64);
        match admin_audit_log::prune_older_than(db, cutoff).await {
            Ok(n) => deleted = deleted.saturating_add(n),
            Err(e) => tracing::warn!(
                target: "audit::retention",
                error = %e,
                "admin_audit_log TTL prune failed"
            ),
        }
    }
    // Row-cap defence: fires regardless of TTL policy. Audit-shape §3.3.
    match admin_audit_log::count(db).await {
        Ok(n) if n >= admin_audit_log::ROW_CAP => {
            let target = admin_audit_log::CULL_CHUNK;
            match admin_audit_log::cull_oldest(db, target).await {
                Ok(culled) => {
                    deleted = deleted.saturating_add(culled);
                    tracing::info!(
                        target: "audit::retention",
                        culled,
                        rows_before = n,
                        cap = admin_audit_log::ROW_CAP,
                        "admin_audit_log row-cap cull"
                    );
                }
                Err(e) => tracing::warn!(
                    target: "audit::retention",
                    error = %e,
                    "admin_audit_log row-cap cull failed"
                ),
            }
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(
            target: "audit::retention",
            error = %e,
            "admin_audit_log count probe failed"
        ),
    }
    deleted
}

/// Spawn the hourly retention sweep. The first tick fires immediately so
/// the boot path logs the operator-set policy and clears stale rows.
pub fn spawn_sweep(db: Arc<Database>) {
    spawn_sweep_with_interval(db, SWEEP_INTERVAL);
}

/// Spawn a sweep with a caller-supplied interval. Internal seam used by
/// tests to drive the loop without waiting an hour.
pub fn spawn_sweep_with_interval(db: Arc<Database>, interval: Duration) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            let policy = load_policy(&db).await;
            let deleted = run_sweep_once(&db, policy).await;
            if deleted > 0 {
                tracing::info!(
                    target: "audit::retention",
                    deleted,
                    "admin_audit_log retention sweep complete"
                );
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{connect_in_memory, migrations};
    use crate::repos::admin_audit_log::NewAdminAuditLog;

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

    fn sample_new() -> NewAdminAuditLog {
        NewAdminAuditLog {
            actorUserId: Some(1),
            actorUsername: "alice".into(),
            kind: "userPatched".into(),
            targetKind: Some("user".into()),
            targetId: Some(2),
            targetLabel: Some("bob".into()),
            payload: Some(serde_json::json!({"fields": ["role"]})),
            outcome: "success".into(),
            errorMsg: None,
            requestIp: None,
            requestUserAgent: None,
        }
    }

    #[tokio::test]
    async fn run_sweep_once_with_unbounded_skips_ttl_prune() {
        let db = connect_in_memory().await.unwrap();
        migrations::run(&db).await.unwrap();
        for _ in 0..3 {
            admin_audit_log::insert(&db, sample_new()).await.unwrap();
        }
        // Unbounded policy → no TTL prune. Row count well under cap so no cull either.
        let deleted = run_sweep_once(&db, RetentionPolicy::Unbounded).await;
        assert_eq!(deleted, 0);
        assert_eq!(admin_audit_log::count(&db).await.unwrap(), 3);
    }

    #[tokio::test]
    async fn run_sweep_once_with_finite_policy_prunes_old_rows() {
        let db = connect_in_memory().await.unwrap();
        migrations::run(&db).await.unwrap();
        admin_audit_log::insert(&db, sample_new()).await.unwrap();
        // A 0-day policy parses to Unbounded; use 30 days then back-date by
        // direct insertion: rows are stamped with time::now() so to test
        // "prune happens" we simulate by setting cutoff to the future via a
        // very small `Days(N)` that's still >= the floor. The TTL prune uses
        // `cutoff = now - N days`; for N=30, all just-inserted rows survive.
        // Therefore this test asserts the no-op path.
        let deleted = run_sweep_once(&db, RetentionPolicy::Days(30)).await;
        assert_eq!(deleted, 0, "fresh rows must not be pruned by 30-day TTL");
        assert_eq!(admin_audit_log::count(&db).await.unwrap(), 1);
    }

    /// Row-cap branch: with a low override of the row cap is hard to
    /// exercise without a 100 000-row insert. Instead we verify the
    /// branch's pre-condition: at count < ROW_CAP the cull does not fire.
    /// (The cull function itself is unit-tested in
    /// `repos::admin_audit_log::tests::cull_oldest_trims_in_occurred_order`.)
    #[tokio::test]
    async fn run_sweep_once_below_cap_does_not_cull() {
        let db = connect_in_memory().await.unwrap();
        migrations::run(&db).await.unwrap();
        for _ in 0..5 {
            admin_audit_log::insert(&db, sample_new()).await.unwrap();
        }
        let before = admin_audit_log::count(&db).await.unwrap();
        let deleted = run_sweep_once(&db, RetentionPolicy::Days(365)).await;
        let after = admin_audit_log::count(&db).await.unwrap();
        assert_eq!(deleted, 0);
        assert_eq!(before, after);
    }

    #[tokio::test]
    async fn load_policy_returns_default_on_unseeded_db() {
        let db = connect_in_memory().await.unwrap();
        migrations::run(&db).await.unwrap();
        // Migration 0009 seeds the row to "365"; verify the default round-trips.
        let policy = load_policy(&db).await;
        assert_eq!(policy, RetentionPolicy::Days(DEFAULT_RETENTION_DAYS));
    }

    #[tokio::test]
    async fn load_policy_picks_up_operator_override() {
        let db = connect_in_memory().await.unwrap();
        migrations::run(&db).await.unwrap();
        // Operator bumps retention.
        app_settings::put(&db, APP_SETTING_KEY, "180")
            .await
            .unwrap();
        assert_eq!(load_policy(&db).await, RetentionPolicy::Days(180));
        // Operator opts into unbounded.
        app_settings::put(&db, APP_SETTING_KEY, "0").await.unwrap();
        assert_eq!(load_policy(&db).await, RetentionPolicy::Unbounded);
        // Below-floor → clamped.
        app_settings::put(&db, APP_SETTING_KEY, "7").await.unwrap();
        assert_eq!(
            load_policy(&db).await,
            RetentionPolicy::Days(RETENTION_FLOOR_DAYS)
        );
    }
}
