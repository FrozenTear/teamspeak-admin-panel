# Music-bot REST API (PURA-117 / PURA-123 WS-5)

This is the wire contract the FE-PAGES Dioxus UI (WS-6) and any external automation drives the music-bot product through. Every endpoint speaks JSON in and JSON out, lives under `/api`, and authenticates via the same `Authorization: Bearer <jwt>` header as the rest of `ts6-manager-server` — see `crates/ts6-manager-server/src/auth/extractors.rs`.

The wire types are defined once in [`ts6_manager_shared::music_bots`](../../crates/shared/src/music_bots.rs); the FE imports them directly so a renamed field shows up as a compile error on both sides.

## Conventions

- **Auth**: `RequireAuth` extractor on every endpoint (one fresh DB lookup per call, per spec §6.4.1). Multi-tenant / per-bot ACLs are flagged for follow-up on the parent epic — every authenticated user can drive every bot today.
- **JSON keys**: camelCase on the wire, snake_case in Rust.
- **Error envelope** (every non-2xx): `{ "error": "<message>", "code": "<class>", "details": "<optional>" }`. `code` is one of `validation`, `not_found`, `conflict`, `queue_full`. Unknown codes render as a generic error in the FE.
- **Timestamps**: ISO 8601 / RFC 3339, UTC (`chrono::DateTime<Utc>`).
- **IDs**: numeric (`u64`), serialised as bare JSON numbers via `#[serde(transparent)]`.
- **Pagination**: not implemented in WS-5; `/music-requests` accepts `limit`. The library/playlist endpoints return the full list — fine for the in-memory store, will gain pagination alongside the SurrealDB swap.

### `AudioSource` shape

Externally-tagged enum; wire-format:

```json
{ "kind": "url",         "url":  "https://example.com/song.mp3" }
{ "kind": "libraryPath", "path": "lo-fi/track.mp3" }
```

### `BotState` shape

Snake-case strings: `"disconnected" | "connecting" | "connected" | "in_channel" | "disconnecting" | "playing"`. The `playing` value is a route-layer synthesis when the bot is online AND has a `nowPlaying` track; the underlying lifecycle FSM does not include `Playing` (audio dispatch lives in WS-2).

## Endpoints

### `/music-bots` — bot CRUD + lifecycle

| Method | Path | Body | Response | Notes |
| --- | --- | --- | --- | --- |
| `GET` | `/api/music-bots` | — | `200 [MusicBotSummary]` | All tracked bots, sorted by id. |
| `POST` | `/api/music-bots` | `CreateBotRequest` | `201 MusicBotSummary` | Stamps a `BotId`, spawns the actor, optionally auto-connects. |
| `GET` | `/api/music-bots/{id}` | — | `200 MusicBotDetail` | Includes the queue snapshot. `404` if the bot is not tracked. |
| `DELETE` | `/api/music-bots/{id}` | — | `204` | `Shutdown` — actor task is awaited before the response. `404` for unknown id. |
| `POST` | `/api/music-bots/{id}/connect` | — | `202` | Dispatches `BotCommand::Connect`. Idempotent in-flight (no-op when already connected). |
| `POST` | `/api/music-bots/{id}/disconnect` | — | `202` | Dispatches `BotCommand::Disconnect`. |
| `POST` | `/api/music-bots/{id}/join` | `JoinChannelRequest` | `202` | Dispatches `BotCommand::JoinChannel`. |
| `POST` | `/api/music-bots/{id}/leave` | — | `202` | Dispatches `BotCommand::LeaveChannel`. |
| `GET` | `/api/music-bots/{id}/events` | — | `text/event-stream` | SSE stream of `BotEventWire` events. Tagged `type` discriminator. 15 s keep-alive. |

Sample — create + join:

```bash
TOKEN=eyJ...
curl -s -X POST http://localhost:3000/api/music-bots \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"name":"DJ-Bot","serverAddr":"127.0.0.1:9987","autoConnect":true}'
# → 201 { "id": 1, "name": "DJ-Bot", "serverAddr": "127.0.0.1:9987",
#          "state": "connecting", "nowPlaying": null }

curl -s -X POST http://localhost:3000/api/music-bots/1/join \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"channelId":42}'
# → 202
```

### `/music-library` — per-bot saved sources

| Method | Path | Body / Query | Response |
| --- | --- | --- | --- |
| `GET` | `/api/music-library?bot={id}&tag={tag}` | `tag` optional, exact match | `200 [LibraryEntry]` |
| `POST` | `/api/music-library` | `{ "bot": <id>, ...AddLibraryEntryRequest }` | `201 LibraryEntry` |
| `PATCH` | `/api/music-library/{trackId}` | `{ "bot": <id>, ...PatchLibraryEntryRequest }` | `200 LibraryEntry` |
| `DELETE` | `/api/music-library/{trackId}?bot={id}` | — | `204` |

`PATCH` is read-modify-write — the route deletes the existing entry and inserts a fresh one with the patched fields, so the returned `LibraryEntry` carries a new `id`.

### `/playlists` — per-bot playlist CRUD

| Method | Path | Body / Query | Response |
| --- | --- | --- | --- |
| `GET` | `/api/playlists?bot={id}` | — | `200 [PlaylistSummary]` |
| `POST` | `/api/playlists` | `CreatePlaylistRequest` | `201 PlaylistSummary` |
| `GET` | `/api/playlists/{name}?bot={id}` | — | `200 PlaylistDetail` |
| `PATCH` | `/api/playlists/{name}?bot={id}` | `{"newName":"..."}` | `200 PlaylistSummary` |
| `DELETE` | `/api/playlists/{name}?bot={id}` | — | `204` |
| `POST` | `/api/playlists/{name}/tracks` | `{ "bot": <id>, ...AddTrackRequest }` | `201 Track` |
| `DELETE` | `/api/playlists/{name}/tracks/{trackId}?bot={id}` | — | `204` |
| `POST` | `/api/playlists/{name}/enqueue?bot={id}` | — | `200 PlaylistDetail` |

`POST /enqueue` dispatches `BotCommand::Queue(EnqueuePlaylist)` at the bot actor and stamps a `MusicRequest` row per track in the request log.

### `/radio-stations` — radio-source presets

Backed by the per-bot library — each station is a `LibraryEntry` carrying the `radio` marker tag (constant `RADIO_TAG` in shared). DELETE / play refuse to operate on entries that are not tagged `radio` so the same id space stays unambiguous.

| Method | Path | Body / Query | Response |
| --- | --- | --- | --- |
| `GET` | `/api/radio-stations?bot={id}` | — | `200 [RadioStation]` |
| `POST` | `/api/radio-stations` | `CreateRadioStationRequest` | `201 RadioStation` |
| `DELETE` | `/api/radio-stations/{id}?bot={id}` | — | `204` |
| `POST` | `/api/radio-stations/{id}/play?bot={id}` | — | `202` |

`POST /play` dispatches `BotCommand::Audio(Play{source})` directly, bypassing the queue. The request log gets a row with `trackId: null`.

### `/music-requests` — request log (read-only)

| Method | Path | Query | Response |
| --- | --- | --- | --- |
| `GET` | `/api/music-requests` | `bot`, `requestedBy`, `since`, `until`, `limit` | `200 [MusicRequest]` |

Returned newest-first. Rows land here as a side-effect of:

- `POST /playlists/{name}/enqueue` (one row per track)
- `POST /radio-stations/{id}/play` (one row, `trackId: null`)
- WS-4 chat commands (queue / now-playing / playlist enqueue ops)

WS-5 stores the log in-process (cap: 1 000 rows, oldest dropped first); the SurrealDB-backed swap is a follow-up under the parent epic.

## Error model

All error bodies use the shared envelope:

```json
{ "error": "name must not be empty", "code": "validation" }
```

Mapping to status codes:

| Code | HTTP | Trigger |
| --- | --- | --- |
| `validation` | 400 | Empty / malformed required field, illegal id, etc. |
| `not_found` | 404 | Bot / playlist / library entry / station does not exist. |
| `conflict` | 409 | Duplicate playlist name on `POST /playlists`. |
| `queue_full` | 503 | Bot's command queue is at capacity (transient). |

Untagged 5xx responses still match the same envelope shape so the FE can render a generic toast without branching on `code`.

## SSE event stream

`GET /api/music-bots/{id}/events` opens an `event-stream` response. Each event is a JSON object; the discriminator is `"type"`:

```json
{ "type": "stateChanged", "from": "connecting", "to": "connected" }
{ "type": "joinedChannel", "channelId": 42 }
{ "type": "queueChanged", "len": 3, "current": { "id": 7, "title": "Song", … } }
{ "type": "nowPlaying",   "track":  { "id": 7, "title": "Song", … } }
{ "type": "queueEmpty" }
{ "type": "playlistChanged", "name": "lo-fi-radio" }
{ "type": "libraryChanged" }
{ "type": "error", "message": "audio command Stop not implemented in WS-1" }
```

Lagged subscribers (slow client + bursty events) are silently dropped; the FE refetches `/music-bots/{id}` on lag.

## Out of scope (flagged for follow-up)

- Multi-tenant / org scoping. RBAC granularity is "any authenticated user" today.
- WebSocket push beyond SSE — every other live-state surface in the manager is `axum::extract::ws`, but WS-5 stays SSE-only because the FE consumes it via the browser's native `EventSource`.
- Persistence. The supervisor + request log both run in-process; restarting the binary loses every bot, queue, library entry, playlist, and request row. The SurrealDB swap is queued under the parent epic alongside the existing repo-pattern.
