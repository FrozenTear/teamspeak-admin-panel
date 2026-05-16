-- =====================================================================
-- TS6 Manager — Phase 9.1.2 automod: `warn` action kind (PURA-301).
--
-- The Phase 9.1 automod brief (PURA-297 §4.3 / §10, board decision) adds
-- `warn` as a first-class `moderation_case_action.actionKind`: a private
-- `sendtextmessage` to the subject is a distinct, operator-visible
-- moderation step, cleaner for per-rule history and metrics than reusing
-- `note` with a payload tag.
--
-- Migration 0011 defined `actionKind` as a bare `TYPE string` with the
-- valid set living only in a comment and the `repos::moderation_case_action`
-- `ACTION_KINDS` const. This migration promotes that set to a schema-level
-- `ASSERT` so an out-of-vocabulary actionKind is rejected at write time,
-- and adds `warn` to it.
--
-- The enumerated set is the union of every kind written today:
--   - operator case actions (`routes/moderation/actions.rs`):
--     kick, ban, ban_ip, mute, unmute, note
--   - case-status transitions (`routes/moderation/cases.rs`): resolve, reopen
--   - automod (this phase): warn
--
-- 0011 is already applied in every environment with a moderation schema,
-- so this is a separate migration rather than an edit to 0011 — the
-- name-keyed runner never re-runs an applied migration.
-- =====================================================================

DEFINE FIELD OVERWRITE actionKind ON moderation_case_action TYPE string
    ASSERT $value IN [
        'warn', 'kick', 'ban', 'ban_ip', 'mute', 'unmute', 'note', 'resolve', 'reopen'
    ];
