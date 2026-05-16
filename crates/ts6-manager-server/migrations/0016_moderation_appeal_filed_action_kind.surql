-- =====================================================================
-- TS6 Manager — Phase 9.2 appeals: `appeal_filed` action kind (PURA-307).
--
-- The public appeals surface (`POST /api/public/moderation/appeals`,
-- workstream `9.2-public-routes` of PURA-269 §9) records that an appeal
-- was *filed* against a case as a `moderation_case_action` timeline row
-- with `actionKind = 'appeal_filed'` and `actorUserId = NULL` (the
-- appellant is not an operator). The case timeline records only the
-- filed/decided event, never the raw external appeal prose — that stays
-- quarantined in `moderation_appeal` (plan §4.7).
--
-- Migrations 0012 / 0013 promoted `moderation_case_action.actionKind` to
-- a schema-level `ASSERT` over an enumerated set; this migration
-- overwrites that field definition to extend the set with `appeal_filed`.
-- `appeal_decided` is intentionally NOT added here — it is written by the
-- 9.2-operator-ui decision workstream and lands in that workstream's own
-- migration, keeping each workstream's schema change self-contained.
--
-- 0011/0012/0013 are already applied wherever a moderation schema exists,
-- so this is a separate migration rather than an edit — the name-keyed
-- runner never re-runs an applied migration.
-- =====================================================================

DEFINE FIELD OVERWRITE actionKind ON moderation_case_action TYPE string
    ASSERT $value IN [
        'warn', 'kick', 'ban', 'ban_ip', 'mute', 'unmute', 'unban', 'note',
        'resolve', 'reopen', 'appeal_filed'
    ];
