-- =====================================================================
-- TS6 Manager — PURA-144 (WS-6): video_source catalogue.
--
-- Phase 5 / Chapter 23 video streaming via MoQ sidecar (PURA-136). Each
-- row represents an operator-configured video source currently published
-- (or recently published) through `ts6-media-sidecar`. The sidecar owns
-- the live FFmpeg pipeline; this table is the persistent record of which
-- sources the operator wants to keep, so a manager restart can re-issue
-- `POST /source` against the sidecar to bring them back up. (Re-bring-up
-- itself is a follow-up; the row alone is sufficient for the FE to list
-- known sources after a restart.)
--
-- Cascade posture mirrors `widget` (§4.2.15): per-server entity, hard
-- cascades on `server_connection` delete via the existing
-- `server_connection_cascade_chapter4` event. The cascade list is
-- extended below rather than splitting into a fresh event so the
-- chapter-4 cascade convention stays a single source of truth.
--
-- `sourceId` is the sidecar's own identifier (the namespace under which
-- the broadcast is published on moq-lite) and the join key to the
-- sidecar's `GET /stats`. `status` mirrors the high-level pipeline state
-- the polling task observes (`starting` | `live` | `stopped` |
-- `failed`); the granular metrics (`frames_published`, `bytes_published`,
-- `ffmpeg_alive`) are not persisted — they live on the sidecar and are
-- fanned out via the WS push channel only.
-- =====================================================================

DEFINE SEQUENCE video_source_id BATCH 1 START 1;

DEFINE TABLE video_source SCHEMAFULL;
DEFINE FIELD sourceId        ON video_source TYPE string;
DEFINE FIELD label           ON video_source TYPE string;
DEFINE FIELD url             ON video_source TYPE string;
DEFINE FIELD preset          ON video_source TYPE string DEFAULT '720p';
DEFINE FIELD serverConfigId  ON video_source TYPE int;
DEFINE FIELD createdByUserId ON video_source TYPE option<int>;
DEFINE FIELD status          ON video_source TYPE string DEFAULT 'starting';
DEFINE FIELD createdAt       ON video_source TYPE datetime VALUE $value OR time::now() READONLY;
DEFINE FIELD updatedAt       ON video_source TYPE datetime VALUE time::now();

-- One sourceId per sidecar — `POST /source` rejects duplicates with 409
-- so the manager-side uniqueness lines up. Composite index on
-- (serverConfigId, sourceId) supports the FE-PAGES list query.
DEFINE INDEX video_source_source_id_unique ON video_source FIELDS sourceId UNIQUE;
DEFINE INDEX video_source_server_idx       ON video_source FIELDS serverConfigId;

-- ---- Cascade extensions ---------------------------------------------
-- Standalone DELETE event for `video_source`. The existing
-- `server_connection_cascade_chapter4` event in 0004 enumerates Chapter
-- 4 tables only; Phase 5 entities get their own cascade event so the
-- Chapter 4 cascade stays scoped to its original authors.
--
-- Note: when the server_connection row is deleted, this event purges
-- the rows from the manager-side catalogue. The sidecar pipelines are
-- NOT torn down by this cascade — the route handler must call
-- `POST /source/stop` BEFORE deleting the server. A follow-up
-- (post-WS-7) wires the server-delete handler to pre-stop all attached
-- sources; for now operators must stop video sources before deleting a
-- server connection.
DEFINE EVENT server_connection_cascade_video_source ON server_connection WHEN $event = "DELETE" THEN {
    LET $sid = record::id($before.id);
    DELETE video_source WHERE serverConfigId = $sid;
};
