-- One summon cap per TeamSpeak server address.
--
-- The number is not a music-bot row and not one row per quiet-client
-- slot. A server with no row has no stored number. Nothing in this
-- migration inserts a default.

DEFINE TABLE music_summon_cap SCHEMAFULL;
DEFINE FIELD serverAddr ON music_summon_cap TYPE string;
DEFINE FIELD cap ON music_summon_cap TYPE int;
DEFINE INDEX music_summon_cap_server_unique ON music_summon_cap FIELDS serverAddr UNIQUE;
