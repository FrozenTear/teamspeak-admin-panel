-- =====================================================================
-- TS6 Manager — PURA-357: `music_bot_runtime` table.
--
-- WS-5 (PURA-123) shipped the music-bot supervisor as an in-memory,
-- in-process structure: a created bot lived only in `BotSupervisor`'s
-- `HashMap`. Every configured bot therefore vanished on a process
-- restart — and a live deploy upgrades by `podman kube down` /
-- `kube play`, which recreates the container. Operators lost every bot
-- on each image upgrade (board report, PURA-356).
--
-- This table persists exactly the `music_bot::BotConfig` runtime shape
-- so the boot-time rehydration path in `main.rs` can re-spawn each bot.
-- The record id IS the supervisor's `BotId` (a `u64`): the supervisor's
-- atomic counter stays the id authority, and boot seeds it past the
-- highest persisted id via `BotSupervisor::spawn_with_id`.
--
-- This is deliberately distinct from the spec-era `music_bot` table
-- (0004): that table models spec §4.2.10's fuller configuration
-- (serverConfigId, voicePort, volume, streamPreset, sidecarPort, …) and
-- was never wired to the shipped WS-5 REST surface — its repo is
-- exercised only by tests. Converging the two models is future scope;
-- this table tracks only what the running supervisor actually needs.
-- =====================================================================

DEFINE TABLE music_bot_runtime SCHEMAFULL;
-- Display name in-channel (`BotConfig.name`).
DEFINE FIELD name         ON music_bot_runtime TYPE string;
-- TS6 server `host:port` (`BotConfig.server_addr`).
DEFINE FIELD serverAddr   ON music_bot_runtime TYPE string;
-- On-disk path of the cached TS identity (`BotConfig.identity_path`).
-- The file itself must also live on a persistent volume — see
-- `AppState::from_config`, which roots the default identity directory
-- under `DATA_DIR` so a rehydrated bot keeps its TS identity.
DEFINE FIELD identityPath ON music_bot_runtime TYPE string;
-- Auto-connect on (re)spawn (`BotConfig.auto_connect`).
DEFINE FIELD autoConnect  ON music_bot_runtime TYPE bool DEFAULT true;
