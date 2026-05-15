-- =====================================================================
-- TS6 Manager — PURA-241 (v1.1 flow engine): `bot_flow_run` table.
--
-- Persistent run-history record for the v1.1 flow engine. Brief §5.2 /
-- §5.3 / §5.4 — every trigger fire produces one row, regardless of
-- whether the actions actually ran (`status = 'skipped_disabled'` rows
-- are part of the audit trail). The per-flow cap of 200 rows + the
-- 30-day global TTL keep the table bounded; both are enforced from the
-- `repos::bot_flow_runs` module rather than from SurrealDB events.
--
-- `trigger` and `actionResults` carry JSON documents whose shape depends
-- on the trigger / action discriminants. Mirroring `bot_flow.flowData`,
-- we store them as `string` (JSON-encoded) so the table stays SCHEMAFULL
-- without committing to a per-discriminant column layout — the
-- typed deserialisation happens in the repo layer using `serde_json`
-- against `ts6_manager_shared::flows::{Action, FlowRunStatus, ...}`.
--
-- Cascade: deleting a `bot_flow` removes its run history. The chapter-4
-- `bot_flow_cascade` event in `0004_chapter4_remaining_entities` predates
-- this table — we add a sibling event scoped specifically to
-- `bot_flow_run`, keeping the chapter-4 event's name + ownership intact
-- (same containment trick `0007_video_source.surql` used for the Phase 5
-- entities).
-- =====================================================================

DEFINE SEQUENCE bot_flow_run_id BATCH 1 START 1;

DEFINE TABLE bot_flow_run SCHEMAFULL;
DEFINE FIELD flowId        ON bot_flow_run TYPE int;
-- JSON-encoded trigger document. `manualFire` carries the operator's
-- optional context; `ts6ClientJoined` carries the resolved event fields;
-- `cron` carries the tick instant.
DEFINE FIELD trigger       ON bot_flow_run TYPE string DEFAULT '{"kind":"manualFire"}';
-- 'in_flight' | 'ok' | 'errored' | 'interrupted' | 'skipped_disabled'.
-- Brief §6 lifecycle. The boot-time interrupted sweep rewrites any
-- straggling 'in_flight' rows to 'interrupted' before the engine starts
-- accepting new triggers.
DEFINE FIELD status        ON bot_flow_run TYPE string DEFAULT 'in_flight';
DEFINE FIELD startedAt     ON bot_flow_run TYPE datetime VALUE $value OR time::now() READONLY;
DEFINE FIELD finishedAt    ON bot_flow_run TYPE option<datetime>;
-- Capped at 2 KiB by the repo layer (brief §5.2). NULL for `ok` and
-- `skipped_disabled` rows.
DEFINE FIELD error         ON bot_flow_run TYPE option<string>;
-- JSON-encoded `Vec<ActionResult>`. Defaults to '[]' so a freshly
-- inserted in-flight row round-trips through serde without a null check.
DEFINE FIELD actionResults ON bot_flow_run TYPE string DEFAULT '[]';

-- Index supports the brief §5.2 run-history list query:
--   SELECT ... WHERE flowId = $fid ORDER BY startedAt DESC LIMIT 25.
DEFINE INDEX bot_flow_run_flow_started_idx ON bot_flow_run FIELDS flowId, startedAt;
-- TTL janitor scans by `finishedAt`; supporting index keeps the global
-- sweep cheap even on a hot table.
DEFINE INDEX bot_flow_run_finished_idx ON bot_flow_run FIELDS finishedAt;

-- ---- Cascade: bot_flow delete removes its run rows -------------------
DEFINE EVENT bot_flow_cascade_runs ON bot_flow WHEN $event = "DELETE" THEN {
    LET $fid = record::id($before.id);
    DELETE bot_flow_run WHERE flowId = $fid;
};
