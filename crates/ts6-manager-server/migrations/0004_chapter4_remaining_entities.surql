-- =====================================================================
-- TS6 Manager — Phase 1 DATA slice 2 (PURA-12)
--
-- Spec: study-documents/ts6-manager-spec.md Chapter 4
--   §4.2.5  BotFlow             §4.2.12 Playlist
--   §4.2.6  BotVariable         §4.2.13 PlaylistSong
--   §4.2.7  BotExecution        §4.2.14 RadioStation
--   §4.2.8  BotExecutionLog     §4.2.15 Widget
--   §4.2.9  AppSetting          §4.2.16 MusicRequest
--   §4.2.10 MusicBot            §4.2.17 StreamSession
--   §4.2.11 Song
--   §4.2 cascade rules (incl. §4.5 the no-cascade Playlist case)
--
-- Continues the slice-1 conventions: sequence-driven int PKs (one
-- SEQUENCE per int-PK table, START 1), camelCase document field names
-- matching the spec / Chapter 7 wire shape verbatim, and cascading
-- delete via DEFINE EVENT instead of SQL FK CASCADE (D8 deviation).
-- AppSetting keeps a string PK (record id == key) per §4.2.9.
-- =====================================================================

-- ---- Sequences --------------------------------------------------------
DEFINE SEQUENCE bot_flow_id          BATCH 1 START 1;
DEFINE SEQUENCE bot_variable_id      BATCH 1 START 1;
DEFINE SEQUENCE bot_execution_id     BATCH 1 START 1;
DEFINE SEQUENCE bot_execution_log_id BATCH 1 START 1;
DEFINE SEQUENCE music_bot_id         BATCH 1 START 1;
DEFINE SEQUENCE song_id              BATCH 1 START 1;
DEFINE SEQUENCE playlist_id          BATCH 1 START 1;
DEFINE SEQUENCE playlist_song_id     BATCH 1 START 1;
DEFINE SEQUENCE radio_station_id     BATCH 1 START 1;
DEFINE SEQUENCE widget_id            BATCH 1 START 1;
DEFINE SEQUENCE music_request_id     BATCH 1 START 1;
DEFINE SEQUENCE stream_session_id    BATCH 1 START 1;

-- ---- BotFlow (§4.2.5) -------------------------------------------------
DEFINE TABLE bot_flow SCHEMAFULL;
DEFINE FIELD name            ON bot_flow TYPE string;
DEFINE FIELD description     ON bot_flow TYPE option<string>;
-- flowData defaults to the empty FlowDefinition shape from §12.1 so a
-- freshly-created flow round-trips through the FlowDefinition codec.
DEFINE FIELD flowData        ON bot_flow TYPE string DEFAULT '{"nodes":[],"edges":[]}';
DEFINE FIELD serverConfigId  ON bot_flow TYPE int;
DEFINE FIELD virtualServerId ON bot_flow TYPE int  DEFAULT 1;
DEFINE FIELD enabled         ON bot_flow TYPE bool DEFAULT false;
DEFINE FIELD createdAt       ON bot_flow TYPE datetime VALUE $value OR time::now() READONLY;
DEFINE FIELD updatedAt       ON bot_flow TYPE datetime VALUE time::now();
DEFINE INDEX bot_flow_server_idx ON bot_flow FIELDS serverConfigId;

-- ---- BotVariable (§4.2.6) --------------------------------------------
DEFINE TABLE bot_variable SCHEMAFULL;
DEFINE FIELD flowId ON bot_variable TYPE int;
DEFINE FIELD name   ON bot_variable TYPE string;
DEFINE FIELD value  ON bot_variable TYPE string;
DEFINE FIELD scope  ON bot_variable TYPE string DEFAULT 'flow';
DEFINE INDEX bot_variable_unique  ON bot_variable FIELDS flowId, name, scope UNIQUE;
DEFINE INDEX bot_variable_flow_idx ON bot_variable FIELDS flowId;

-- ---- BotExecution (§4.2.7) -------------------------------------------
DEFINE TABLE bot_execution SCHEMAFULL;
DEFINE FIELD flowId       ON bot_execution TYPE int;
DEFINE FIELD triggeredBy  ON bot_execution TYPE string;
DEFINE FIELD triggerData  ON bot_execution TYPE option<string>;
DEFINE FIELD status       ON bot_execution TYPE string DEFAULT 'running';
DEFINE FIELD startedAt    ON bot_execution TYPE datetime VALUE $value OR time::now() READONLY;
DEFINE FIELD endedAt      ON bot_execution TYPE option<datetime>;
DEFINE FIELD error        ON bot_execution TYPE option<string>;
DEFINE INDEX bot_execution_flow_idx ON bot_execution FIELDS flowId;

-- ---- BotExecutionLog (§4.2.8) ----------------------------------------
-- executionId is nullable (engine-level entries) and `serverConfigId` has
-- a non-cascading FK per §4.2.8 — i.e. when the server is deleted, log
-- rows are intentionally orphaned rather than wiped.
DEFINE TABLE bot_execution_log SCHEMAFULL;
DEFINE FIELD executionId    ON bot_execution_log TYPE option<int>;
DEFINE FIELD serverConfigId ON bot_execution_log TYPE int;
DEFINE FIELD flowId         ON bot_execution_log TYPE option<int>;
DEFINE FIELD nodeId         ON bot_execution_log TYPE option<string>;
DEFINE FIELD nodeName       ON bot_execution_log TYPE option<string>;
DEFINE FIELD level          ON bot_execution_log TYPE string DEFAULT 'info';
DEFINE FIELD message        ON bot_execution_log TYPE string;
DEFINE FIELD data           ON bot_execution_log TYPE option<string>;
DEFINE FIELD timestamp      ON bot_execution_log TYPE datetime VALUE $value OR time::now() READONLY;
DEFINE INDEX bot_execution_log_execution_idx ON bot_execution_log FIELDS executionId;
DEFINE INDEX bot_execution_log_flow_idx      ON bot_execution_log FIELDS flowId;
DEFINE INDEX bot_execution_log_timestamp_idx ON bot_execution_log FIELDS timestamp;

-- ---- AppSetting (§4.2.9) ---------------------------------------------
-- String-keyed PK: the record id encodes the key (`app_setting:<key>`),
-- and `key` is also stored as a regular field so the document round-trips
-- through the JSON wire shape `{key, value, updatedAt}` without aliasing.
DEFINE TABLE app_setting SCHEMAFULL;
DEFINE FIELD key       ON app_setting TYPE string;
DEFINE FIELD value     ON app_setting TYPE string;
DEFINE FIELD updatedAt ON app_setting TYPE datetime VALUE time::now();
DEFINE INDEX app_setting_key_unique ON app_setting FIELDS key UNIQUE;

-- §4.2.9 seed: the reference's "max_music_bots = 5". Migrations only
-- run once per `_migration` row, so a plain CREATE is safe.
CREATE app_setting:max_music_bots CONTENT { key: 'max_music_bots', value: '5' };

-- ---- MusicBot (§4.2.10) ----------------------------------------------
DEFINE TABLE music_bot SCHEMAFULL;
DEFINE FIELD name                 ON music_bot TYPE string;
DEFINE FIELD serverConfigId       ON music_bot TYPE int;
DEFINE FIELD nickname             ON music_bot TYPE string DEFAULT 'MusicBot';
DEFINE FIELD serverPassword       ON music_bot TYPE option<string>;
DEFINE FIELD defaultChannel       ON music_bot TYPE option<string>;
DEFINE FIELD channelPassword      ON music_bot TYPE option<string>;
DEFINE FIELD nowPlayingChannelId  ON music_bot TYPE option<string>;
DEFINE FIELD voicePort            ON music_bot TYPE int  DEFAULT 9987;
DEFINE FIELD volume               ON music_bot TYPE int  DEFAULT 50;
-- identityData is an opaque TS3 identity blob; treated as sensitive per §7.5.
DEFINE FIELD identityData         ON music_bot TYPE option<string>;
DEFINE FIELD autoStart            ON music_bot TYPE bool DEFAULT false;
DEFINE FIELD streamPreset         ON music_bot TYPE string DEFAULT '720p';
DEFINE FIELD sidecarPort          ON music_bot TYPE int    DEFAULT 9800;
DEFINE FIELD createdAt            ON music_bot TYPE datetime VALUE $value OR time::now() READONLY;
DEFINE FIELD updatedAt            ON music_bot TYPE datetime VALUE time::now();
DEFINE INDEX music_bot_server_idx ON music_bot FIELDS serverConfigId;

-- ---- Song (§4.2.11) --------------------------------------------------
DEFINE TABLE song SCHEMAFULL;
DEFINE FIELD title          ON song TYPE string;
DEFINE FIELD artist         ON song TYPE option<string>;
DEFINE FIELD duration       ON song TYPE option<float>;
DEFINE FIELD filePath       ON song TYPE string;
DEFINE FIELD source         ON song TYPE string DEFAULT 'local';
DEFINE FIELD sourceUrl      ON song TYPE option<string>;
DEFINE FIELD fileSize       ON song TYPE option<int>;
DEFINE FIELD serverConfigId ON song TYPE int;
DEFINE FIELD createdAt      ON song TYPE datetime VALUE $value OR time::now() READONLY;
DEFINE INDEX song_server_idx ON song FIELDS serverConfigId;

-- ---- Playlist (§4.2.12) ----------------------------------------------
-- musicBotId is *set null* (not cascade) on MusicBot delete — see the
-- music_bot_set_null_playlist event below. The §4.5 no-cascade test
-- exercises this contract.
DEFINE TABLE playlist SCHEMAFULL;
DEFINE FIELD name        ON playlist TYPE string;
DEFINE FIELD musicBotId  ON playlist TYPE option<int>;
DEFINE FIELD createdAt   ON playlist TYPE datetime VALUE $value OR time::now() READONLY;
DEFINE INDEX playlist_music_bot_idx ON playlist FIELDS musicBotId;

-- ---- PlaylistSong (§4.2.13) ------------------------------------------
DEFINE TABLE playlist_song SCHEMAFULL;
DEFINE FIELD playlistId ON playlist_song TYPE int;
DEFINE FIELD songId     ON playlist_song TYPE int;
DEFINE FIELD position   ON playlist_song TYPE int DEFAULT 0;
DEFINE INDEX playlist_song_unique       ON playlist_song FIELDS playlistId, songId UNIQUE;
DEFINE INDEX playlist_song_playlist_idx ON playlist_song FIELDS playlistId;
DEFINE INDEX playlist_song_song_idx     ON playlist_song FIELDS songId;

-- ---- RadioStation (§4.2.14) ------------------------------------------
DEFINE TABLE radio_station SCHEMAFULL;
DEFINE FIELD name           ON radio_station TYPE string;
DEFINE FIELD url            ON radio_station TYPE string;
DEFINE FIELD genre          ON radio_station TYPE option<string>;
DEFINE FIELD imageUrl       ON radio_station TYPE option<string>;
DEFINE FIELD serverConfigId ON radio_station TYPE int;
DEFINE FIELD createdAt      ON radio_station TYPE datetime VALUE $value OR time::now() READONLY;
DEFINE INDEX radio_station_server_idx ON radio_station FIELDS serverConfigId;

-- ---- Widget (§4.2.15) ------------------------------------------------
DEFINE TABLE widget SCHEMAFULL;
DEFINE FIELD name              ON widget TYPE string;
DEFINE FIELD token             ON widget TYPE string;
DEFINE FIELD serverConfigId    ON widget TYPE int;
DEFINE FIELD virtualServerId   ON widget TYPE int  DEFAULT 1;
DEFINE FIELD theme             ON widget TYPE string DEFAULT 'dark';
DEFINE FIELD showChannelTree   ON widget TYPE bool   DEFAULT true;
DEFINE FIELD showClients       ON widget TYPE bool   DEFAULT true;
DEFINE FIELD hideEmptyChannels ON widget TYPE bool   DEFAULT false;
DEFINE FIELD maxChannelDepth   ON widget TYPE int    DEFAULT 5;
DEFINE FIELD createdAt         ON widget TYPE datetime VALUE $value OR time::now() READONLY;
DEFINE FIELD updatedAt         ON widget TYPE datetime VALUE time::now();
DEFINE INDEX widget_token_unique ON widget FIELDS token UNIQUE;
DEFINE INDEX widget_server_idx   ON widget FIELDS serverConfigId;

-- ---- MusicRequest (§4.2.16) ------------------------------------------
DEFINE TABLE music_request SCHEMAFULL;
DEFINE FIELD title          ON music_request TYPE string;
DEFINE FIELD url            ON music_request TYPE string;
DEFINE FIELD serverConfigId ON music_request TYPE int;
DEFINE FIELD requestedAt    ON music_request TYPE datetime VALUE $value OR time::now() READONLY;
DEFINE INDEX music_request_server_url_unique ON music_request FIELDS serverConfigId, url UNIQUE;
DEFINE INDEX music_request_server_idx        ON music_request FIELDS serverConfigId;

-- ---- StreamSession (§4.2.17) -----------------------------------------
-- Informational; `musicBotId` is intentionally *not* a formal FK per spec.
DEFINE TABLE stream_session SCHEMAFULL;
DEFINE FIELD musicBotId  ON stream_session TYPE int;
DEFINE FIELD source      ON stream_session TYPE string;
DEFINE FIELD preset      ON stream_session TYPE string DEFAULT '720p';
DEFINE FIELD startedAt   ON stream_session TYPE datetime VALUE $value OR time::now() READONLY;
DEFINE FIELD endedAt     ON stream_session TYPE option<datetime>;
DEFINE FIELD peakViewers ON stream_session TYPE int DEFAULT 0;

-- ---- Cascading delete events (§4.2 cascade rules) --------------------
-- Each event is name-scoped to its source table; SurrealDB fires every
-- DELETE event on the row, so chained cascades (bot_flow → bot_execution
-- → bot_execution_log) propagate correctly.

-- TsServerConfig fan-out: all per-server entities share the cascade. The
-- `_chapter4` suffix avoids colliding with the slice-1 event of the same
-- root name. bot_execution_log is *intentionally absent* — §4.2.8 says
-- its serverConfigId FK does not cascade.
DEFINE EVENT server_connection_cascade_chapter4 ON server_connection WHEN $event = "DELETE" THEN {
    LET $sid = record::id($before.id);
    DELETE bot_flow      WHERE serverConfigId = $sid;
    DELETE music_bot     WHERE serverConfigId = $sid;
    DELETE song          WHERE serverConfigId = $sid;
    DELETE radio_station WHERE serverConfigId = $sid;
    DELETE widget        WHERE serverConfigId = $sid;
    DELETE music_request WHERE serverConfigId = $sid;
};

DEFINE EVENT bot_flow_cascade ON bot_flow WHEN $event = "DELETE" THEN {
    LET $fid = record::id($before.id);
    DELETE bot_variable  WHERE flowId = $fid;
    DELETE bot_execution WHERE flowId = $fid;
};

DEFINE EVENT bot_execution_cascade ON bot_execution WHEN $event = "DELETE" THEN {
    LET $eid = record::id($before.id);
    DELETE bot_execution_log WHERE executionId = $eid;
};

-- §4.2.12 + §4.5: deleting a MusicBot must NOT delete its Playlists.
-- Instead, set their musicBotId to NONE (SurrealDB's "null" for option<int>).
DEFINE EVENT music_bot_set_null_playlist ON music_bot WHEN $event = "DELETE" THEN {
    LET $mid = record::id($before.id);
    UPDATE playlist SET musicBotId = NONE WHERE musicBotId = $mid;
};

DEFINE EVENT playlist_cascade ON playlist WHEN $event = "DELETE" THEN {
    LET $pid = record::id($before.id);
    DELETE playlist_song WHERE playlistId = $pid;
};

DEFINE EVENT song_cascade ON song WHEN $event = "DELETE" THEN {
    LET $sid = record::id($before.id);
    DELETE playlist_song WHERE songId = $sid;
};
