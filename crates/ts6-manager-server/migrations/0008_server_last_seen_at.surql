-- =====================================================================
-- TS6 Manager — PURA-222: `lastSeenAt` on `server_connection`.
--
-- Adds an optional timestamp the dashboard handler bumps after a
-- successful WebQuery fetch so the `/servers` index can render a
-- "last successful probe" column without operators having to re-check
-- each row by hand. `option<datetime>` (rather than a sentinel epoch)
-- keeps the "never reachable since add" case truthful — the FE renders
-- "Never" instead of inventing 1970.
--
-- Backfill is intentionally NULL: existing rows haven't been observed
-- since this column landed; the next dashboard fetch sets it. Adding a
-- backfill DEFAULT would lie about uptime to the operator on every row
-- carried forward from before the migration.
-- =====================================================================

DEFINE FIELD lastSeenAt ON server_connection TYPE option<datetime>;
