-- =====================================================================
-- TS6 Manager — Phase 9.2 appeals data model (PURA-305).
--
-- Workstream 1 of the Phase 9.2 Appeals breakdown (PURA-269 §9). Two
-- tables, both downstream of the 9.0 moderation surface in 0011:
--
--   moderation_report  — report intake queue (pre-case). An operator
--                        triages a report and either promotes it to a
--                        `moderation_case` or dismisses it.
--   moderation_appeal  — one row per appeal lodged against a case.
--
-- The `report_challenge_token` / `appeal_token` store is owned by the
-- 9.2-tokens workstream and is intentionally NOT defined here — it lands
-- in its own follow-up migration so the two workstreams stay decoupled.
--
-- FK convention (see 0011 header): spec §4.1 mandates int primary keys,
-- and every table carries foreign keys as plain `int`s rather than
-- SurrealDB record links. `moderation_appeal.caseId` follows that — it is
-- a plain `int` FK into `moderation_case`, and referential integrity for
-- a deleted case is kept by the cascade event below. `moderation_report`
-- carries an *optional* `caseId`: a report only gains one once it is
-- promoted to a case.
--
-- SurrealDB v3 SCHEMAFULL gotchas (project memory): neither table has an
-- `object` field, so no `TYPE option<object> FLEXIBLE` is needed; any
-- `ORDER BY` idiom is honoured in the repo projection, not here.
--
-- Status columns are plain strings with the value set documented inline
-- (no schema-level `ASSERT`) — this mirrors `moderation_case.status` in
-- 0011 rather than the enumerated `actionKind` in 0012.
--
-- Cascade posture (mirrors `user_cascade_moderation` in 0011):
--   - deleting a `moderation_case` deletes its `moderation_appeal` rows
--     (an appeal is meaningless without the case it contests). A
--     promoted `moderation_report` keeps its now-dangling `caseId` — the
--     intake record is pre-case history and survives the case.
--   - deleting a `user` set-nulls operator linkage on both tables
--     (`moderation_report.triagedByUserId`,
--     `moderation_appeal.decidedByUserId`); the moderation records
--     themselves survive.
--
-- GDPR posture: subject-scoped export + erasure are served by the repo
-- layer (`repos::moderation_reports` / `repos::moderation_appeals`,
-- `export_for_subject` / `purge_for_subject`), mirroring
-- `repos::moderation_notes`. No automatic TTL column is added — reports
-- and appeals are moderation history held under legitimate interest.
-- =====================================================================

-- --------------------------------------------------------------------
-- moderation_report — report intake queue (pre-case)
-- --------------------------------------------------------------------
DEFINE SEQUENCE moderation_report_id BATCH 1 START 1;

DEFINE TABLE moderation_report SCHEMAFULL;
DEFINE FIELD serverConfigId       ON moderation_report TYPE int;
DEFINE FIELD virtualServerId      ON moderation_report TYPE int;
-- The TS6 client UID of the reporter.
DEFINE FIELD reporterUid          ON moderation_report TYPE string;
-- The accused: a TS6 client UID when known, otherwise a free-text
-- nickname. Reports are filed before a case exists, so a durable UID is
-- not always available — unlike `moderation_case.subjectUid`.
DEFINE FIELD subjectUidOrNickname ON moderation_report TYPE string;
-- Free-form report category (e.g. 'harassment', 'spam').
DEFINE FIELD category             ON moderation_report TYPE string;
DEFINE FIELD statement            ON moderation_report TYPE string;
DEFINE FIELD evidenceUrl          ON moderation_report TYPE option<string>;
-- 'pending' | 'promoted' | 'dismissed'.
DEFINE FIELD status               ON moderation_report TYPE string DEFAULT 'pending';
-- Set once the report is promoted to a `moderation_case` (plain int FK).
DEFINE FIELD caseId               ON moderation_report TYPE option<int>;
-- The operator who triaged (promoted / dismissed) the report.
DEFINE FIELD triagedByUserId      ON moderation_report TYPE option<int>;
-- Hashed source IP of the submitter — abuse rate-limiting, never the raw IP.
DEFINE FIELD sourceIpHash         ON moderation_report TYPE string;
DEFINE FIELD createdAt            ON moderation_report TYPE datetime
    VALUE $value OR time::now() READONLY;
DEFINE FIELD updatedAt            ON moderation_report TYPE datetime
    VALUE $value OR time::now();

DEFINE INDEX moderation_report_subject_idx ON moderation_report FIELDS subjectUidOrNickname;
DEFINE INDEX moderation_report_case_idx    ON moderation_report FIELDS caseId;
DEFINE INDEX moderation_report_status_idx  ON moderation_report FIELDS status;
DEFINE INDEX moderation_report_created_idx ON moderation_report FIELDS createdAt;

-- --------------------------------------------------------------------
-- moderation_appeal — one row per appeal lodged against a case
-- --------------------------------------------------------------------
DEFINE SEQUENCE moderation_appeal_id BATCH 1 START 1;

DEFINE TABLE moderation_appeal SCHEMAFULL;
-- Plain int FK into `moderation_case` (FK convention, see 0011 header).
DEFINE FIELD caseId          ON moderation_appeal TYPE int;
-- The TS6 client UID of the appellant.
DEFINE FIELD submitterUid    ON moderation_appeal TYPE string;
-- Free-text proof of identity supplied with the appeal.
DEFINE FIELD identityProof   ON moderation_appeal TYPE string;
DEFINE FIELD statement       ON moderation_appeal TYPE string;
-- 'pending' | 'upheld' | 'overturned' | 'withdrawn'.
DEFINE FIELD status          ON moderation_appeal TYPE string DEFAULT 'pending';
-- The operator who decided the appeal.
DEFINE FIELD decidedByUserId ON moderation_appeal TYPE option<int>;
DEFINE FIELD decisionNote    ON moderation_appeal TYPE option<string>;
-- Hashed source IP of the appellant — abuse rate-limiting, never the raw IP.
DEFINE FIELD sourceIpHash    ON moderation_appeal TYPE string;
DEFINE FIELD createdAt       ON moderation_appeal TYPE datetime
    VALUE $value OR time::now() READONLY;
DEFINE FIELD decidedAt       ON moderation_appeal TYPE option<datetime>;

DEFINE INDEX moderation_appeal_case_idx    ON moderation_appeal FIELDS caseId;
DEFINE INDEX moderation_appeal_subject_idx ON moderation_appeal FIELDS submitterUid;
DEFINE INDEX moderation_appeal_status_idx  ON moderation_appeal FIELDS status;
DEFINE INDEX moderation_appeal_created_idx ON moderation_appeal FIELDS createdAt;

-- --------------------------------------------------------------------
-- Cascade events
-- --------------------------------------------------------------------

-- Deleting a case removes the appeals contesting it. Named distinctly
-- from 0011's `moderation_case_cascade` (which clears the action
-- timeline) — SurrealDB fires every event defined on the table.
DEFINE EVENT moderation_case_cascade_appeal ON moderation_case WHEN $event = "DELETE" THEN {
    LET $cid = record::id($before.id);
    DELETE moderation_appeal WHERE caseId = $cid;
};

-- Deleting a user clears operator linkage on reports and appeals but
-- keeps the moderation records (mirrors `user_cascade_moderation` in
-- 0011). Defined separately from that event so each migration owns its
-- own tables' cascade rules.
DEFINE EVENT user_cascade_moderation_appeal ON user WHEN $event = "DELETE" THEN {
    LET $uid = record::id($before.id);
    UPDATE moderation_report SET triagedByUserId = NONE WHERE triagedByUserId = $uid;
    UPDATE moderation_appeal SET decidedByUserId = NONE WHERE decidedByUserId = $uid;
};
