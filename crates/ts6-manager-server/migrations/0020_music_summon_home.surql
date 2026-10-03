-- Summon home channel, stored on the same row as the summon cap.
--
-- A summon client that is not playing for anyone waits in this channel.
-- No value means none is picked: such a client disconnects instead of
-- waiting in a public channel. Nothing in this migration picks a default.
--
-- A row may now hold a home without a cap, so `cap` becomes optional. A
-- row with no cap is a server with no stored number, the same as no row.

DEFINE FIELD OVERWRITE cap ON music_summon_cap TYPE option<int>;
DEFINE FIELD homeChannel ON music_summon_cap TYPE option<int>;
