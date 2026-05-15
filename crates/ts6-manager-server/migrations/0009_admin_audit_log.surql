-- =====================================================================
-- TS6 Manager — Phase 7 admin audit log (PURA-228 / PURA-234).
--
-- Append-only per-event row for admin-mutating actions on the user
-- surface and adjacent areas. Mirrors the ssh_audit_log shape from
-- 0006_ssh_audit_log.surql (PURA-79).
--
-- No user-table changes accompany this migration. architecture.md §4.1
-- ("What does NOT change") is explicit: the existing `enabled: bool`
-- flag covers the disable surface, and the audit row itself retains
-- the timestamp via the `userDisabled` event. The ticket's open
-- implementation question resolves to "no user-table delta".
--
-- Cascade posture:
--   - actorUserId set-null on user delete (mirrors ssh_audit_log R7).
--   - targetId set-null on user delete when targetKind = 'user'.
-- Other targetKind values cascade per their own surface (none for v1.1).
--
-- Retention: row-cap + TTL, enforced by a Tokio janitor (see
-- docs/admin/audit-shape.md §3.3 and §3.4). The parser semantics
-- (default 365, '0' = unbounded, '1'..='29' clamped to 30, '>= 30'
-- honored) live in crate::audit::retention and mirror the SSH path.
-- =====================================================================

DEFINE SEQUENCE admin_audit_log_id BATCH 1 START 1;

DEFINE TABLE admin_audit_log SCHEMAFULL;
DEFINE FIELD actorUserId      ON admin_audit_log TYPE option<int>;
DEFINE FIELD actorUsername    ON admin_audit_log TYPE string;     -- snapshot at event time
DEFINE FIELD kind             ON admin_audit_log TYPE string;     -- event discriminant
DEFINE FIELD targetKind       ON admin_audit_log TYPE option<string>;
DEFINE FIELD targetId         ON admin_audit_log TYPE option<int>;
DEFINE FIELD targetLabel      ON admin_audit_log TYPE option<string>; -- snapshot
-- FLEXIBLE: `payload` holds per-event-kind objects with arbitrary nested
-- keys (audit-shape.md §2.2 — `{fields:[...]}`, `{from,to}`, `{family,
-- rowsDeleted}`, …). Without FLEXIBLE a SCHEMAFULL table rejects every
-- undeclared nested path (`payload.fields` etc.).
DEFINE FIELD payload          ON admin_audit_log TYPE option<object> FLEXIBLE; -- redacted detail
DEFINE FIELD outcome          ON admin_audit_log TYPE string;     -- 'success' | 'failure'
DEFINE FIELD errorMsg         ON admin_audit_log TYPE option<string>;
DEFINE FIELD requestIp        ON admin_audit_log TYPE option<string>;
DEFINE FIELD requestUserAgent ON admin_audit_log TYPE option<string>;
DEFINE FIELD occurredAt       ON admin_audit_log TYPE datetime
    VALUE $value OR time::now() READONLY;
DEFINE FIELD insertedAt       ON admin_audit_log TYPE datetime
    VALUE $value OR time::now() READONLY;

DEFINE INDEX admin_audit_log_occurred_idx ON admin_audit_log FIELDS occurredAt;
DEFINE INDEX admin_audit_log_actor_idx    ON admin_audit_log FIELDS actorUserId;
DEFINE INDEX admin_audit_log_kind_idx     ON admin_audit_log FIELDS kind;
DEFINE INDEX admin_audit_log_target_idx   ON admin_audit_log FIELDS targetKind, targetId;

-- Mirrors `user_set_null_ssh_audit` from 0006_ssh_audit_log.surql (R7).
-- The audit row survives user-delete cascades; only the operator
-- linkage clears.
DEFINE EVENT user_set_null_admin_audit ON user WHEN $event = "DELETE" THEN {
    LET $uid = record::id($before.id);
    UPDATE admin_audit_log SET actorUserId = NONE
        WHERE actorUserId = $uid;
    UPDATE admin_audit_log SET targetId = NONE
        WHERE targetKind = 'user' AND targetId = $uid;
};

-- Retention default. Override semantics enforced by the parser in
-- crate::audit::retention (mirrors crate::sshbridge::retention):
--   empty / unset → 365 (default)
--   '0'           → unbounded, WARN at boot
--   '1'..='29'    → clamped to 30, WARN at boot
--   '>= 30'       → honored
CREATE app_setting:admin_audit_retention_days CONTENT {
    key: 'admin_audit_retention_days',
    value: '365'
};
