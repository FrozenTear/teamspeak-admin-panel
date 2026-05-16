-- =====================================================================
-- TS6 Manager — Phase 9.1.4 automod: `unban` action kind (PURA-303).
--
-- The automod review surface adds a one-click *Revert* on a `mute` / `ban`
-- automod action (PURA-297 plan §7). Reverting a `mute` appends an
-- existing `unmute` timeline row; reverting a `ban` issues `bandel` and
-- needs a paired de-escalation row of its own. `unmute` would be the
-- wrong kind — it carries the talker-flag semantics, not a ban lift — so
-- this migration adds a first-class `unban` kind.
--
-- Migration 0012 promoted `moderation_case_action.actionKind` to a
-- schema-level `ASSERT` over an enumerated set; this migration overwrites
-- that field definition to extend the set with `unban`. As with 0012 the
-- enumerated set is the union of every kind written today:
--   - operator case actions (`routes/moderation/actions.rs`):
--     kick, ban, ban_ip, mute, unmute, note
--   - case-status transitions (`routes/moderation/cases.rs`): resolve, reopen
--   - automod effects (`flow/dispatch.rs`): warn
--   - automod revert (this phase, `routes/moderation/automod.rs`): unban
--
-- 0011/0012 are already applied wherever a moderation schema exists, so
-- this is a separate migration rather than an edit — the name-keyed
-- runner never re-runs an applied migration.
-- =====================================================================

DEFINE FIELD OVERWRITE actionKind ON moderation_case_action TYPE string
    ASSERT $value IN [
        'warn', 'kick', 'ban', 'ban_ip', 'mute', 'unmute', 'unban', 'note',
        'resolve', 'reopen'
    ];
