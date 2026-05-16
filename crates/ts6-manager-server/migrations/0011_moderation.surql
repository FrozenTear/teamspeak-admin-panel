-- =====================================================================
-- TS6 Manager — Phase 9.0 moderation data model (PURA-285).
--
-- One migration for the whole 9.0 moderation surface, per the PURA-262
-- design brief §5. Four tables:
--
--   moderation_case         — one row per actioned subject on a vserver
--   moderation_case_action  — append-only per-case action timeline
--   moderation_note         — free-text moderator notes on a subject UID
--   user_permission         — fine-grained `moderation.*` permission grants
--
-- `user_permission` is shared with the 9.0-rbac workstream (PURA-284):
-- the brief lists it under both the data model (§5) and the rbac
-- workstream (§9). It is **defined here, once** — PURA-284 builds the
-- `RequirePermission` extractor and grant UI on top of this table and
-- `repos::user_permission`, and adds no migration of its own. See the
-- coordination note on PURA-284.
--
-- SurrealDB v3 SCHEMAFULL gotchas (project memory):
--   - `object` fields must be `TYPE option<object> FLEXIBLE` or a
--     SCHEMAFULL table rejects every undeclared nested path.
--   - any `ORDER BY` idiom must appear in the SELECT projection — that
--     constraint is honoured in the repo layer, not here.
--
-- FK convention: spec §4.1 mandates int primary keys, and every existing
-- table (`bot_flow_run.flowId`, `bot_execution.flowId`, …) carries its
-- foreign keys as plain `int`s rather than SurrealDB record links. The
-- brief §5 describes `moderation_case_action.caseId` as a "record link";
-- we deviate to a plain `int` FK for consistency with the rest of the
-- schema and the `repos::record_id_to_i64` helper. Referential integrity
-- is kept by the `moderation_case` DELETE event below.
--
-- Cascade posture:
--   - deleting a `moderation_case` removes its action timeline.
--   - deleting a `user` set-nulls the operator linkage on cases / actions
--     / notes (the moderation record survives — mirrors admin_audit_log),
--     and hard-deletes that user's `user_permission` grants (a grant for
--     a non-existent user is meaningless).
--
-- Note retention / GDPR posture: resolved in docs/admin/moderation-data.md
-- §4. `moderation_note` has no automatic TTL (notes are moderation
-- history held under legitimate interest); subject-scoped export + erasure
-- are served by `repos::moderation_note::{export_for_subject,
-- purge_for_subject}`. Not a schema concern — no TTL column is added.
-- =====================================================================

-- --------------------------------------------------------------------
-- moderation_case
-- --------------------------------------------------------------------
DEFINE SEQUENCE moderation_case_id BATCH 1 START 1;

DEFINE TABLE moderation_case SCHEMAFULL;
DEFINE FIELD serverConfigId          ON moderation_case TYPE int;
DEFINE FIELD virtualServerId         ON moderation_case TYPE int;
-- Durable subject key: a TS6 client UID. Cases key on UID, never on a
-- nickname, so identity churn does not fork a subject's history (§4).
DEFINE FIELD subjectUid              ON moderation_case TYPE string;
DEFINE FIELD subjectNicknameSnapshot ON moderation_case TYPE string;
-- 'operator' | 'complaint' | 'automod'. 'automod' is reserved for 9.1.
DEFINE FIELD origin                  ON moderation_case TYPE string;
-- Free-form backref into the origin surface (e.g. a complaint id).
DEFINE FIELD originRef               ON moderation_case TYPE option<string>;
-- 'open' | 'actioned' | 'resolved'. 'appealed' is reserved for 9.2.
DEFINE FIELD status                  ON moderation_case TYPE string DEFAULT 'open';
DEFINE FIELD reason                  ON moderation_case TYPE string;
DEFINE FIELD resolutionNote          ON moderation_case TYPE option<string>;
DEFINE FIELD openedByUserId          ON moderation_case TYPE option<int>;
DEFINE FIELD openedAt                ON moderation_case TYPE datetime
    VALUE $value OR time::now() READONLY;
DEFINE FIELD updatedAt               ON moderation_case TYPE datetime
    VALUE $value OR time::now();
DEFINE FIELD resolvedAt              ON moderation_case TYPE option<datetime>;

DEFINE INDEX moderation_case_subject_idx ON moderation_case FIELDS subjectUid;
DEFINE INDEX moderation_case_status_idx  ON moderation_case FIELDS status;
DEFINE INDEX moderation_case_vserver_idx ON moderation_case
    FIELDS serverConfigId, virtualServerId;
DEFINE INDEX moderation_case_opened_idx  ON moderation_case FIELDS openedAt;

-- --------------------------------------------------------------------
-- moderation_case_action — append-only timeline
-- --------------------------------------------------------------------
DEFINE SEQUENCE moderation_case_action_id BATCH 1 START 1;

DEFINE TABLE moderation_case_action SCHEMAFULL;
DEFINE FIELD caseId                ON moderation_case_action TYPE int;
DEFINE FIELD actorUserId           ON moderation_case_action TYPE option<int>;
DEFINE FIELD actorUsernameSnapshot ON moderation_case_action TYPE string;
-- 'kick' | 'ban' | 'mute' | 'unmute' | 'note' | 'resolve' | 'reopen'.
DEFINE FIELD actionKind            ON moderation_case_action TYPE string;
DEFINE FIELD reason                ON moderation_case_action TYPE string;
-- TS6 ban-id linkage when the action produced a server-side ban.
DEFINE FIELD tsRef                 ON moderation_case_action TYPE option<string>;
-- FLEXIBLE: per-action-kind detail with arbitrary nested keys. Without
-- FLEXIBLE a SCHEMAFULL table rejects every undeclared nested path.
DEFINE FIELD payload               ON moderation_case_action TYPE option<object> FLEXIBLE;
DEFINE FIELD createdAt             ON moderation_case_action TYPE datetime
    VALUE $value OR time::now() READONLY;

DEFINE INDEX moderation_case_action_case_idx ON moderation_case_action FIELDS caseId;
DEFINE INDEX moderation_case_action_time_idx ON moderation_case_action FIELDS createdAt;

-- --------------------------------------------------------------------
-- moderation_note — free-text notes on a subject, independent of cases
-- --------------------------------------------------------------------
DEFINE SEQUENCE moderation_note_id BATCH 1 START 1;

DEFINE TABLE moderation_note SCHEMAFULL;
DEFINE FIELD subjectUid             ON moderation_note TYPE string;
DEFINE FIELD body                   ON moderation_note TYPE string;
DEFINE FIELD authorUserId           ON moderation_note TYPE option<int>;
DEFINE FIELD authorUsernameSnapshot ON moderation_note TYPE string;
DEFINE FIELD createdAt              ON moderation_note TYPE datetime
    VALUE $value OR time::now() READONLY;
DEFINE FIELD updatedAt              ON moderation_note TYPE datetime
    VALUE $value OR time::now();

DEFINE INDEX moderation_note_subject_idx ON moderation_note FIELDS subjectUid;
DEFINE INDEX moderation_note_time_idx    ON moderation_note FIELDS createdAt;

-- --------------------------------------------------------------------
-- user_permission — fine-grained `moderation.*` grants (shared w/ 9.0-rbac)
-- --------------------------------------------------------------------
DEFINE SEQUENCE user_permission_id BATCH 1 START 1;

DEFINE TABLE user_permission SCHEMAFULL;
-- The user the grant is *for*.
DEFINE FIELD subjectUserId   ON user_permission TYPE int;
-- A permission-catalog key, e.g. 'moderation.action.ban' (brief §6).
DEFINE FIELD permission      ON user_permission TYPE string;
DEFINE FIELD grantedByUserId ON user_permission TYPE option<int>;
DEFINE FIELD grantedAt       ON user_permission TYPE datetime
    VALUE $value OR time::now() READONLY;

-- One grant row per (user, permission) — a re-grant is a no-op, not a
-- duplicate. `repos::user_permission::grant` is written to be idempotent
-- against this index.
DEFINE INDEX user_permission_unique ON user_permission
    FIELDS subjectUserId, permission UNIQUE;
DEFINE INDEX user_permission_subject_idx ON user_permission FIELDS subjectUserId;

-- --------------------------------------------------------------------
-- Cascade events
-- --------------------------------------------------------------------

-- Deleting a case removes its append-only action timeline. The brief
-- keeps the case as the unit of moderation history; an orphan action
-- row would be unreachable from every read path.
DEFINE EVENT moderation_case_cascade ON moderation_case WHEN $event = "DELETE" THEN {
    LET $cid = record::id($before.id);
    DELETE moderation_case_action WHERE caseId = $cid;
};

-- Deleting a user clears operator linkage but keeps the moderation
-- record (mirrors `user_set_null_admin_audit` from 0010). A grant for a
-- deleted user is meaningless, so `user_permission` rows are removed.
DEFINE EVENT user_cascade_moderation ON user WHEN $event = "DELETE" THEN {
    LET $uid = record::id($before.id);
    UPDATE moderation_case        SET openedByUserId = NONE WHERE openedByUserId = $uid;
    UPDATE moderation_case_action SET actorUserId    = NONE WHERE actorUserId = $uid;
    UPDATE moderation_note        SET authorUserId   = NONE WHERE authorUserId = $uid;
    UPDATE user_permission        SET grantedByUserId = NONE WHERE grantedByUserId = $uid;
    DELETE user_permission WHERE subjectUserId = $uid;
};
