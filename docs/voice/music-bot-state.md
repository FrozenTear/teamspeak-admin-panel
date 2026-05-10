# Music-bot per-bot state (PURA-121 WS-3)

This document is the **persistence contract** between WS-3 (queue /
playlists / library data model) and the WS-5 SurrealDB-backed
`MusicBotStore` impl that lands in `ts6-manager-server`.

WS-3 ships an in-memory impl
(`music_bot::InMemoryMusicBotStore`) and the trait surface
(`music_bot::MusicBotStore`). WS-5 ships:

- `crates/ts6-manager-server/migrations/0007_music_bot_state.surql` —
  matches the field names below verbatim.
- `crates/ts6-manager-server/src/repos/music_bot.rs` — implements
  `MusicBotStore` against the existing `Surreal<Db>` connection.
- The supervisor-into-axum wiring that finally consumes both.

---

## Schema version

Pinned in `music_bot::SNAPSHOT_VERSION`. Bump on **any** field
rename / type change / cardinality change. Snapshots / Surreal rows
that don't match the running build's version MUST be rejected at load
time; the in-memory impl already does this in `load_from_json`.

| Version | Date | Change |
|---------|------|--------|
| 1 | 2026-05-10 | Baseline — queue + playlists + library, per-bot, schemafull. |

## Data model

All state is **per-bot**. Tables key on `(bot_id, …)` so a single
SurrealDB instance can host every bot the supervisor spawns.

### `Track`

The unit of playback. Stored both inline in the queue and inline in
playlist entries. `id` is store-assigned (a monotonically increasing
`u64`) and stable across snapshot/load.

| Field | Type | Notes |
|-------|------|-------|
| `id` | `u64` (`TrackId`) | Store-assigned. Stable across restart. |
| `source` | `AudioSource` | `Url(String)` or `LibraryPath(PathBuf)`. Wire form: `{"Url": "..."}` / `{"LibraryPath": "..."}` (default serde-tagged). |
| `title` | `String` | Display title. |
| `duration_secs` | `Option<u64>` | Set when known (yt-dlp probe / library entry). |
| `requested_by` | `Option<String>` | TS6 client name, when the track was queued via chat command. |

### `LibraryEntry`

Per-bot saved source. Source for the future `radio-stations` surface.

| Field | Type | Notes |
|-------|------|-------|
| `id` | `u64` (`LibraryEntryId`) | Store-assigned. |
| `source` | `AudioSource` | Same shape as `Track::source`. |
| `title` | `String` | |
| `tags` | `Vec<String>` | Exact-match filter at query time. No tag normalisation today (case-sensitive). |

### Per-bot envelope (`PerBotState`)

The in-memory impl serialises this struct verbatim. The Surreal impl
in WS-5 will project each field onto its own table — but the
field names below are the migration contract.

```json
{
  "queue":    [Track, ...],
  "playlists": { "<name>": [Track, ...], ... },
  "library":  [LibraryEntry, ...]
}
```

### Snapshot envelope (`StoreSnapshot`)

```json
{
  "version":         1,
  "next_track_id":   <u64>,
  "next_library_id": <u64>,
  "bots": {
    "<bot_id>": <PerBotState>
  }
}
```

`next_track_id` / `next_library_id` are the in-memory monotonic
counters. Persisting them keeps freshly-issued ids from colliding with
already-stored ones after a restart. The Surreal impl will use a
`DEFINE SEQUENCE` per id family instead — but on import from the
in-memory snapshot it must reseed each sequence to at least the
persisted value.

## Concurrency

The in-memory impl guards `PerStoreState` with a single
`tokio::sync::RwLock`. Reads (`peek`, `list`, `current`, `lookup`)
take the read lock; writes (`enqueue`, `dequeue_head`, `clear`,
`reorder`, all playlist + library mutations) take the write lock.
Per-op critical sections are short — a few `Vec` / `HashMap`
operations on small data — so a more granular lock layout would buy
nothing today.

The Surreal impl in WS-5 will drop the lock and rely on the
per-statement transactions Surreal already provides for each
`CREATE` / `UPDATE` / `DELETE`. Read-after-write ordering on a single
bot is guaranteed by the per-bot mpsc dispatch in `bot::run_bot`: every
queue mutation flows through one actor task in FIFO order.

## Durability

In-memory impl: **non-durable**. `snapshot_to_json` /
`load_from_json` are the WS-3 mechanism for proving the data model is
restart-safe. The supervisor doesn't auto-snapshot — that's WS-5's
problem (the SurrealDB impl is implicitly durable per write).

WS-5 obligation: the Surreal impl MUST write through on every mutation
so that an unclean process restart never loses queue state. Coarse
batching (snapshot every N seconds) is **not** acceptable for the
queue — losing the head on a crash means a song restarts from
position 0 on resume.

## Migration path (in-memory snapshot → Surreal v1)

For operators who run an early (WS-3 / WS-4 era) build that only ever
held in-memory state, WS-5 ships a one-shot import:

1. Operator triggers an admin-only `POST /v1/admin/music-bot/import` with
   the JSON snapshot in the body.
2. `ts6-manager-server` validates `version == SNAPSHOT_VERSION`,
   rejecting future versions.
3. The handler runs the `0007_music_bot_state.surql` migration if it
   hasn't already, then bulk-inserts each `PerBotState` field into the
   appropriate table, preserving `id`.
4. `next_track_id` / `next_library_id` are projected into the matching
   `DEFINE SEQUENCE` definitions via `BUMP SEQUENCE` (Surreal v3
   semantics) so subsequently-issued ids never collide with imported
   ones.

If the operator skips the import, WS-5's `MusicBotStore::queue_peek`
returns an empty queue on first read — same as a brand-new bot. No
data loss because there was nothing durable to lose.

## Future bumps

- **v2 (likely WS-4 chat commands)**: add `requested_at: datetime` to
  `Track` so chat-bridge can show a "queued 5 min ago" pill.
- **v3 (post-MVP)**: per-server library scope (the issue spec keeps
  per-bot for WS-3 ease of scope).

Each bump:

1. Bumps `SNAPSHOT_VERSION` and adds a row to the table at the top.
2. Lands a new `0008_*.surql` migration on the SurrealDB side.
3. Adds an `UpgradeFromVN` arm to `load_from_json` in
   `crates/voice/src/store.rs` — explicit migration code, not silent
   "best-effort" merging.
