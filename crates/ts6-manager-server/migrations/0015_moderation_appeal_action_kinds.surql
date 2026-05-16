-- =====================================================================
-- TS6 Manager — Phase 9.2 appeals: `appeal_filed` / `appeal_decided`
-- action kinds (PURA-308).
--
-- Workstream 4 of the Phase 9.2 Appeals breakdown (PURA-269 §9). The
-- appeal flow appends two new `moderation_case_action.actionKind` values
-- to a case timeline:
--   - `appeal_filed`   — written by the public appeal-submit handler
--                        (9.2-public-routes, PURA-307) when a subject
--                        lodges an appeal: `actioned → appealed`.
--   - `appeal_decided` — written by the operator uphold/overturn
--                        endpoints (this workstream): `appealed →
--                        resolved`.
--
-- The Phase 9.2 design brief (PURA-269 §5) assumed `actionKind` was a
-- bare `TYPE string` and called these values "additive, zero migration".
-- That is now stale: migration 0012 promoted `actionKind` to a
-- schema-level `ASSERT` over an enumerated set, so a new convention value
-- is rejected at write time unless the ASSERT is extended. This migration
-- does that.
--
-- It also folds in `unban` — the automod-revert kind added by the
-- concurrent PURA-303 migration (`0013_moderation_unban_action_kind`).
-- The operator overturn endpoint here records a ban reversal as an
-- `unban` timeline row, so this branch needs `unban` to be a valid kind
-- independently of whether 0013 has landed yet. `DEFINE FIELD OVERWRITE`
-- is last-writer-wins by migration name order: 0013 (if present) then
-- 0015, so this enumerated set — the superset of both — is the one that
-- survives fan-in.
--
-- The enumerated set is the union of every kind written today:
--   - operator case actions (`routes/moderation/actions.rs`):
--     kick, ban, ban_ip, mute, unmute, note
--   - case-status transitions (`routes/moderation/cases.rs`): resolve, reopen
--   - automod effect (`flow/dispatch.rs`): warn
--   - automod revert (PURA-303, `routes/moderation/automod.rs`): unban
--   - appeal flow (Phase 9.2): appeal_filed, appeal_decided
--
-- 0011/0012 are already applied wherever a moderation schema exists, so
-- this is a separate migration rather than an edit — the name-keyed
-- runner never re-runs an applied migration.
-- =====================================================================

DEFINE FIELD OVERWRITE actionKind ON moderation_case_action TYPE string
    ASSERT $value IN [
        'warn', 'kick', 'ban', 'ban_ip', 'mute', 'unmute', 'unban', 'note',
        'resolve', 'reopen', 'appeal_filed', 'appeal_decided'
    ];
