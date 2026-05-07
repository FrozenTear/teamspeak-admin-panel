-- =====================================================================
-- Adds `queryBotNickname` to server_connection (spec §4.2.4 + §4.4).
--
-- This is one of the two nickname-field migrations the reference repo
-- shipped at the pinned commit (the other is 0003). Kept as a separate
-- migration so the implementation's migration history mirrors the
-- reference's append-only shape — useful when an operator's existing
-- DB is replayed forward.
-- =====================================================================

DEFINE FIELD queryBotNickname ON server_connection TYPE option<string>;
