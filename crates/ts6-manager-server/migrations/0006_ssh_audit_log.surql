-- =====================================================================
-- TS6 Manager — PURA-79: persistent SSH audit log table.
--
-- Follow-up D under PURA-69 — the structured `tracing::info!` audit slice
-- (see `sshbridge::audit::AuditEntry::emit`) lands in the DB so operators
-- can query the audit trail without scraping log files.
--
-- SecurityEngineer signed off on the field list on PURA-79 with required
-- changes R1–R7 (NIST 800-53 AU-9/AU-11, OWASP A09, STRIDE-T baseline).
-- This migration implements R1 (retention default + override floor) and
-- R7 (`userId` set-null event); the remaining changes live in the repo
-- and writer wiring (R2/R3/R4/R5/R6).
--
-- Cascade posture (matches `bot_execution_log` precedent §4.2.8):
--   - `serverConfigId` is intentionally non-cascading. The audit row
--     survives server-config deletion — that is the entire point of an
--     audit trail. (Note the deliberate omission from the
--     `server_connection_cascade*` events in 0001 / 0004.)
--   - `userId` is set-null on user delete via a dedicated event below
--     (R7). The audit row survives; only the operator linkage drops.
--
-- File numbered 0006 to clear the on-disk collision SecurityEngineer
-- flagged: 0005 is already taken by `0005_ssh_bridge_auth.surql`.
-- =====================================================================

DEFINE SEQUENCE ssh_audit_log_id BATCH 1 START 1;

DEFINE TABLE ssh_audit_log SCHEMAFULL;
DEFINE FIELD serverConfigId  ON ssh_audit_log TYPE int;
DEFINE FIELD virtualServerId ON ssh_audit_log TYPE option<int>;
DEFINE FIELD userId          ON ssh_audit_log TYPE option<int>;
-- First whitespace-delimited token of `commandLine`; cheap group-by key.
DEFINE FIELD command          ON ssh_audit_log TYPE string;
-- Full §10.4-escaped wire line. Audit module invariant: never contains
-- credentials. Repo enforces an 8 KiB cap with truncation sentinel (R2)
-- and a `debug_assert!` denylist for credential-token shapes (R3).
DEFINE FIELD commandLine      ON ssh_audit_log TYPE string;
-- 0 success, upstream `error id=<n>` value otherwise, -1 transport.
DEFINE FIELD exitCode         ON ssh_audit_log TYPE int;
-- 'success' | 'upstream_error' | 'transport'. Derived from `exitCode`;
-- stored explicit so SurrealQL filtering is ergonomic. The canonical
-- source of truth is `exitCode` (see `AuditOutcome` doc comment) — a
-- mismatch between the two fields indicates corruption.
DEFINE FIELD outcome          ON ssh_audit_log TYPE string;
-- Empty on success. Repo enforces a 4 KiB cap with truncation sentinel
-- (R2). Audit module's invariant: caller scrubs key path / agent socket
-- before passing the message in.
DEFINE FIELD errorMsg         ON ssh_audit_log TYPE string DEFAULT '';
-- Wall-clock at which the upstream terminator arrived. UTC. READONLY:
-- supports R4's tamper-resistance posture (no row-level overwrite).
DEFINE FIELD completedAt      ON ssh_audit_log TYPE datetime VALUE $value OR time::now() READONLY;
DEFINE FIELD latencyMs        ON ssh_audit_log TYPE int;
-- Server's DB-write timestamp. Lets forensics detect drift between the
-- operator host and the persistence layer. READONLY for the same reason.
DEFINE FIELD insertedAt       ON ssh_audit_log TYPE datetime VALUE $value OR time::now() READONLY;

DEFINE INDEX ssh_audit_log_server_idx       ON ssh_audit_log FIELDS serverConfigId;
DEFINE INDEX ssh_audit_log_completed_idx    ON ssh_audit_log FIELDS completedAt;
DEFINE INDEX ssh_audit_log_server_time_idx  ON ssh_audit_log FIELDS serverConfigId, completedAt;
DEFINE INDEX ssh_audit_log_user_idx         ON ssh_audit_log FIELDS userId;

-- ---- R7: userId set-null on user delete ------------------------------
-- Mirrors the `music_bot_set_null_playlist` pattern (§4.2.12 / §4.5):
-- the cascade is non-destructive — the audit row stays, the operator
-- linkage clears. SecurityEngineer chose this over extending the
-- existing `user_cascade` (in 0001) so `user_cascade` keeps focusing on
-- hard-cascade tables and the set-null naming convention stays uniform.
DEFINE EVENT user_set_null_ssh_audit ON user WHEN $event = "DELETE" THEN {
    LET $uid = record::id($before.id);
    UPDATE ssh_audit_log SET userId = NONE WHERE userId = $uid;
};

-- ---- R1: retention seed ----------------------------------------------
-- Default 365d. Override semantics enforced by the parser in
-- `crate::sshbridge::retention`:
--   empty / unset       → 365 (default)
--   '0'                 → unbounded (operator opt-in to "never prune"),
--                         WARN logged at boot.
--   '1'..='29'          → clamped to 30, WARN logged at boot.
--   '>= 30'             → honored.
-- A misclick that drops retention to "1 day" otherwise silently destroys
-- forensic evidence — the floor is the soft-fail mode SecurityEngineer
-- closed in R1.
CREATE app_setting:ssh_audit_retention_days CONTENT {
    key: 'ssh_audit_retention_days',
    value: '365'
};
