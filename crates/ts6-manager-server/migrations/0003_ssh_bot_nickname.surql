-- =====================================================================
-- Adds `sshBotNickname` to server_connection (spec §4.2.4 + §4.4).
--
-- Second of the two nickname-field migrations from the reference. Used
-- when a separate SSH bot session needs a distinct nickname from the
-- WebQuery query-bot session.
-- =====================================================================

DEFINE FIELD sshBotNickname ON server_connection TYPE option<string>;
