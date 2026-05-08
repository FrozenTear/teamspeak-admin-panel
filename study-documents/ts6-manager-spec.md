# TS6 Manager — Clean-Room Behavioral Specification

A behavioural specification for an independent reimplementation of the TS6 Manager system. This document is written for an implementer who is not permitted to read the source of the reference system.

> **Implementation deviation.** This document describes the upstream reference. The clean-room implementation in this repo uses **SurrealDB v3** instead of SQLite per impl-plan §1 D8 (board-ratified 2026-05-07). Sections that mention SQLite, `journal_mode`, `SQLITE_FULL`, sqlx, etc. describe the upstream contract; map them onto SurrealDB equivalents at implementation time (record field names for column names, SurrealDB exports for `.sqlite` backups, `write-failure` / `transaction-conflict` / `capacity-pressure` for the three SQLite-full boundaries — see impl-plan §3.2, §3.8, §6 R8).

---

## Reference and Provenance

| Attribute | Value |
|---|---|
| **System under specification** | TS6 Manager (a web UI and supporting services for managing TeamSpeak 6 servers) |
| **Reference repository (read by spec writer)** | `Agent-Fennec/ts6-manager` |
| **Pinned commit** | `0a4b91e3d33b300e5c159f264d00f2408e8d5912` |
| **Commit date** | 2026-04-13 |
| **Reference license (per repository README)** | MIT |
| **Spec writer's role** | Read the reference; produce this behavioural spec. The spec writer does not implement. |
| **Implementer's role** | Implement from this spec without reading the reference. May freely use third-party libraries that are themselves clean of the reference system's source. |
| **Target implementation language** | Rust (informational — the spec itself is language-neutral) |

## Clean-Room Ground Rules Used Throughout

1. The spec describes **observable behaviour and on-the-wire formats**. It does not describe how the reference system is internally factored into files, classes, or functions. Internal source-code identifiers (class names, function names, file names) are **not** preserved in this document; the spec uses neutral descriptive terms.
2. **External-contract identifiers are preserved verbatim.** These are facts about the world that any conforming implementation must use. They include, in particular:
   - HTTP route paths and query parameters as observed by clients;
   - Environment variable names;
   - Default ports;
   - On-disk file paths and directory layouts that operators are expected to mount or back up;
   - JSON keys exchanged between the front-end and the back-end, between the back-end and the WebRTC sidecar, and between the back-end and the TeamSpeak server;
   - TeamSpeak ServerQuery command names and event names (these are part of the TeamSpeak external API, not of the reference system);
   - Music-bot in-channel chat command tokens (these are part of the user-facing UI);
   - String literals that flow documents persist into the database (trigger-type and action-type tags), since they appear in stored data and would break user data if renamed;
   - Database column names, when they appear directly in JSON wire types.
3. **No source code from the reference system is reproduced.** Algorithms are described in prose or in implementer-neutral pseudocode written by the spec writer.
4. Each chapter ends with a **"How to verify"** section listing observable behaviours an independent implementation can be tested against without reference to the original source.
5. Where the spec writer is uncertain about a detail (typically because it is implementation-internal and not externally observable, or because the underlying upstream protocol is itself not formally documented), the chapter raises an **Open Question** rather than guessing. Open questions are also collected in Appendix B.

## Notation

- **MUST / SHOULD / MAY** are used in the conventional RFC sense.
- **Reference value** prefixes a default that the reference system uses when no operator override is supplied; an implementation MAY choose a different default if no external contract pins the value, but doing so will diverge operator-visible defaults.
- **Wire shape** introduces the JSON or string format of a message exchanged between processes.
- **Persistence shape** introduces the column-level layout of a database row that participates in an external contract (e.g., is exposed in a wire type).
- **External identifier** introduces a name that the implementer MUST preserve verbatim.

## Reading Order

The chapters are arranged so that earlier chapters establish concepts (the data model, the configuration surface, security primitives) that later chapters refer to. An implementer working top-to-bottom can build the system in order. Chapter 19 (TeamSpeak Voice Protocol) is the riskiest chapter and is written last; an implementer may elect to substitute an existing third-party TeamSpeak voice client library and skip Chapter 19 entirely.

---

# Part I — System Overview

## Chapter 1. Purpose, Goals, and System Boundaries

### 1.1 What the system is

The system is a self-hosted web application that allows one or more human operators to manage one or more TeamSpeak 6 server instances through a browser. The browser communicates only with the system's own back-end; the system's back-end communicates with each TeamSpeak server over its **WebQuery HTTP** management API (the modern TS6 replacement for the older Telnet-based ServerQuery). The system also operates an SSH client connection to each TeamSpeak server in order to subscribe to event notifications, since the WebQuery HTTP API does not provide a push subscription channel.

In addition to plain server management, the system runs three classes of in-process side-services that operate against each TeamSpeak server:

- **Voice bots (audio):** processes inside the back-end that connect to the TeamSpeak voice server as ordinary clients and stream audio (radio, YouTube, on-disk music files) into a channel. These bots are controllable from the web UI and via in-channel text commands.
- **Bot Flow Engine (automation):** a programmable runtime that executes operator-defined flow documents (graphs of triggers, conditions, actions, and variable mutations) in response to TeamSpeak events, cron schedules, inbound webhooks, or in-channel chat commands.
- **Public widgets:** read-only HTML / SVG / PNG / JSON renderings of a TeamSpeak server's live state, served over a public URL guarded by an unguessable token, intended for embedding on websites and forums.

A separate, single-purpose **media relay sidecar** (a process distinct from the back-end) provides WebRTC-based live video streaming from arbitrary HTTP sources (YouTube, Twitch, direct URLs) into TeamSpeak channels. Browsers receive the WebRTC stream from the sidecar; the back-end orchestrates the sidecar but does not itself touch RTP traffic.

### 1.2 What the system is not

The system is **not**:

- a TeamSpeak server. It controls and bots remote TeamSpeak server instances; it does not host them.
- a TeamSpeak voice client for end users. The voice connections it makes are exclusively for music/automation bots.
- a Telnet ServerQuery client. The reference system explicitly does not support the legacy Telnet-based ServerQuery protocol; only WebQuery HTTP is used.
- a generic remote-administration tool. Its scope is bounded to TeamSpeak management and adjacent automation.

### 1.3 Trust model

- **Operators** authenticate to the system's web UI with username + password. Operator credentials are stored only in the system's own database, hashed; the TeamSpeak server's own user database is independent and is not consulted for web-UI authentication.
- **TeamSpeak server credentials** (WebQuery API keys, SSH credentials) are held only by the back-end. They are stored encrypted at rest in the system's database and are never delivered to the browser.
- **Browsers** see only the back-end's REST and WebSocket APIs. They never see API keys, SSH passwords, or raw TeamSpeak responses verbatim — the back-end re-shapes responses into application-level JSON.
- **Public widget viewers** (anyone who knows a widget's token) receive a small read-only projection of server state. They do not authenticate. Tokens are not bearer-equivalent to operator credentials; they grant only the narrow widget-viewing capability.
- **The WebRTC sidecar** is reached only by the back-end (over HTTP) and by viewers' browsers (over RTP/UDP after WebRTC negotiation). The sidecar holds no operator credentials and no TeamSpeak credentials.
- **The optional Valkey/Redis cache** is reached only by the back-end and is treated as untrusted-but-cooperative storage: missing or unreachable cache MUST degrade gracefully (cache miss, action proceeds), never crash a request.

### 1.4 External actors

| Actor | Reaches | Authentication |
|---|---|---|
| Operator (human) | Front-end SPA, which calls back-end REST + WebSocket | Username + password → JWT access + refresh tokens |
| Public widget viewer (human) | Back-end public widget routes only | None; widget token in URL path |
| TeamSpeak server | Reached **by** back-end over WebQuery HTTPS/HTTP and SSH | Per-server: WebQuery API key; SSH user/password |
| YouTube / Twitch / arbitrary HTTP | Reached **by** back-end via `yt-dlp` and `ffmpeg` subprocesses | None or stored cookies file |
| Valkey / Redis (optional) | Reached **by** back-end | Connection string |
| Browsers (WebRTC viewers) | Reached **by** sidecar via WebRTC (UDP) after offer/answer through the back-end | None at the WebRTC layer; access is gated by the prior REST authentication that triggered the stream |

### 1.5 How to verify

A correct implementation, brought up against a TeamSpeak 6 server with WebQuery enabled, MUST permit an operator to:

1. Log in to the web UI;
2. Add a TeamSpeak server connection (host, WebQuery port, API key, optional SSH credentials);
3. Read the live dashboard for that server;
4. Issue at least one mutating action against the TeamSpeak server (e.g., kick a client) and observe the effect on a real TeamSpeak voice client connected to the same server;
5. Connect a music bot to a channel and stream a known-good radio URL into it, audible in a real TeamSpeak voice client;
6. Define a trivial flow (e.g., on `notifycliententerview` send a `!welcome` message), enable it, and observe that joining the server triggers the action;
7. Generate a public widget URL and load it in an unauthenticated browser as SVG, PNG, and JSON.

If all seven hold against a real TeamSpeak server, the system is functionally correct at the system-boundary level. Per-feature verification is given in subsequent chapters.

---

## Chapter 2. Topology and Process Model

### 2.1 Processes

A deployed instance comprises three long-running processes plus persistent storage and an optional cache:

| Process | Role | Default port |
|---|---|---|
| **Front-end** | Static SPA bundle served by an HTTP static server (e.g., a stripped-down nginx). Pure static files; no per-request server logic. | `80` (container-internal); `3000` (host-exposed) |
| **Back-end** | The application server. Owns the database, owns all TeamSpeak credentials, runs the bot engine, runs voice bots, exposes REST + WebSocket. | `3001` (`PORT`) |
| **Sidecar** | The WebRTC media relay. Owns the FFmpeg subprocess. Stateless across restarts; holds only live peer connections and the current FFmpeg session. | `9800` (`SIDECAR_PORT`) |
| Persistent storage | A SQLite database file plus a music download directory, both volumes mounted into the back-end container. | n/a |
| Cache (optional) | A Valkey- or Redis-compatible server. The system speaks a small subset of the protocol (GET, SET with optional EX, DEL). | `6379` (typical) |

The reference system's development mode runs the front-end as a Vite dev server on `5173` and the back-end on `3001` directly on the host; production deployments containerize all three.

### 2.2 Network reachability requirements

```
                    ┌──────────────────────────┐
                    │  Operator Browser        │
                    │  (HTTPS to front-end,    │
                    │   WebSocket to back-end, │
                    │   WebRTC to sidecar)     │
                    └──┬────────┬─────────┬────┘
                       │        │         │
                       ▼        ▼         ▼
            ┌────────────┐ ┌──────────┐ ┌──────────────────────┐
            │ Front-end  │ │ Back-end │ │ Sidecar              │
            │ (static)   │ │ (REST,   │ │ (HTTP control plane, │
            │            │ │  WS,     │ │  inbound RTP from    │
            │            │ │  bots)   │ │  ffmpeg, outbound    │
            │            │ │          │ │  WebRTC to viewers)  │
            └────────────┘ └────┬─────┘ └─────▲────────────────┘
                                │             │
              ┌─────────────────┼─────────────┘
              │                 │ HTTP control
              ▼                 ▼
    ┌────────────────┐  ┌────────────────────────┐
    │  TeamSpeak     │  │  Optional Valkey/Redis │
    │  WebQuery HTTP │  │                        │
    │  + SSH events  │  │                        │
    └────────────────┘  └────────────────────────┘
                                │
                                ▼
                    ┌────────────────────────┐
                    │  SQLite DB + music dir │
                    │  (mounted volumes)     │
                    └────────────────────────┘
```

The hard constraints are:

- **Browser → back-end** must be reachable for REST and WebSocket;
- **Browser → sidecar** must be reachable on WebRTC's negotiated UDP ports (after STUN/ICE);
- **Back-end → sidecar** must be reachable on the sidecar's HTTP control port;
- **Back-end → TeamSpeak** must be reachable on both the WebQuery HTTP port and the SSH port for each managed server;
- **Sidecar → STUN servers** must be reachable on UDP `3478` (or whatever port the configured STUN entries advertise).

A reverse-proxy deployment (e.g., behind a single domain) is supported by exposing only the front-end and back-end through the proxy and giving the sidecar a routable hostname for WebRTC traffic.

### 2.3 Deployment shapes shipped by the reference system

Four `docker-compose` files are provided, differing only in surface details:

| File | Purpose | Key differences |
|---|---|---|
| `docker-compose.yml` | Default production. Pulls pre-built images. | Exposes 3000, 3001, 9800. |
| `docker-compose.local.yml` | Build images locally from source. | Same exposure as default; uses `Dockerfile.*` directly. |
| `docker-compose.dev.yml` | Developer workflow. | Mounts source; uses dev runners. |
| `docker-compose.coolify.yml` | Reverse-proxy (e.g., Coolify) deployment. | Drops the `ports` section so the proxy can route. |

An implementation MUST be able to run under at least the default and reverse-proxy shapes; the local-build and dev shapes are operator conveniences and MAY be omitted.

### 2.4 Volumes and persistent state

The back-end requires two writable mounts:

- **Database volume**, mounted at the path resolved from `DATABASE_URL` (reference: `/app/packages/backend/data` for the SQLite file). The implementation MUST place all schema migrations and write-ahead logging under this single directory.
- **Music directory**, mounted at `MUSIC_DIR` (reference: `/data/music`). All `yt-dlp`-downloaded files and operator-uploaded music files live here. The implementation MUST NOT write music data anywhere else.

The sidecar is stateless and requires no volume.

### 2.5 Container responsibilities

Each container is responsible for:

| Container | Bring-up steps |
|---|---|
| Back-end | (a) Validate `JWT_SECRET` exists in production mode (refuse to start otherwise); (b) Apply pending database migrations; (c) Set SQLite `journal_mode=MEMORY`; (d) Start REST + WebSocket listeners; (e) Auto-start any `MusicBot` rows with `autoStart=true`; (f) Initialise SSH event subscriptions for any enabled `TsServerConfig`. |
| Front-end | Serve the built SPA bundle. The bundle reads `VITE_API_URL` and `VITE_WS_URL` at build time, not runtime. |
| Sidecar | (a) Open OS-assigned UDP ports for video and audio inbound RTP on `127.0.0.1`; (b) Start the HTTP control listener; (c) Wait for `POST /source` to spawn FFmpeg. |

### 2.6 How to verify

- `docker compose up -d` against a fresh checkout MUST bring all three processes to a healthy state: front-end serves `200 OK` on its root, back-end responds on its health-check route, sidecar responds `200` on `GET /health` with a JSON body containing `videoPort` and `audioPort` integers.
- Stopping and restarting all three containers MUST preserve operator accounts, TeamSpeak server connections, music bots, flows, widgets, and music-library files; the sidecar holds no state and is allowed to lose live peer connections on restart.

---

## Chapter 3. Glossary of External Identifiers

This chapter is a reference list of identifiers the implementer MUST preserve verbatim because they are part of an externally observable contract. They are grouped by category. Each subsequent chapter that introduces such an identifier cross-references this glossary.

### 3.1 Default ports

| Port | Role |
|---|---|
| `80` | Front-end nginx (container-internal) |
| `3000` | Front-end (host-exposed) |
| `3001` | Back-end REST + WebSocket (`PORT`) |
| `5173` | Vite dev server (development only) |
| `9800` | Sidecar HTTP control plane (`SIDECAR_PORT`) |
| `9987` | TeamSpeak voice (default; per-bot configurable as `voicePort`) |
| `10080` | TeamSpeak WebQuery (default; per-server configurable as `webqueryPort`) |
| `10022` | TeamSpeak SSH event subscription (default; per-server configurable as `sshPort`) |
| `6379` | Valkey/Redis (optional) |

### 3.2 Environment variables (back-end)

| Name | Required | Reference default |
|---|---|---|
| `JWT_SECRET` | Yes (production) | — |
| `ENCRYPTION_KEY` | No | falls back to `JWT_SECRET` |
| `NODE_ENV` | No | `development` |
| `PORT` | No | `3001` |
| `DATABASE_URL` | No | `file:./data/ts6webui.db` |
| `JWT_ACCESS_EXPIRY` | No | `15m` |
| `JWT_REFRESH_EXPIRY` | No | `7d` |
| `FRONTEND_URL` | No (CORS) | `http://localhost:3000` (prod) / `http://localhost:5173` (dev) |
| `MUSIC_DIR` | No | `/data/music` |
| `SIDECAR_URL` | No | unset; required if sidecar runs as a separate container |
| `SIDECAR_BINARY_PATH` | No | unset; if set, back-end MAY spawn the sidecar binary directly instead of calling a remote sidecar |
| `YT_COOKIE_FILE` | No | unset |
| `TS_ALLOW_SELF_SIGNED` | No | `false` |

### 3.3 Environment variables (sidecar)

| Name | Reference default | Effect |
|---|---|---|
| `SIDECAR_PORT` | `9800` | HTTP control listener port |
| `STUN_SERVERS` | (built-in list of 9; see §3.4) | Comma-separated STUN URI list |
| `FFMPEG_PATH` | `ffmpeg` | Path to the FFmpeg binary |
| `VIDEO_QUEUE_SIZE` | `2048` | RTP forward queue depth (video) |
| `AUDIO_QUEUE_SIZE` | `4096` | RTP forward queue depth (audio) |
| `SYNC_PLAYOUT_BUFFER_MS` | `4` | Adaptive playout-pacing buffer |
| `SYNC_VIDEO_BIAS_MS` | `4` | Optional extra video holdback for fine-tuning |
| `AUDIO_DELAY_MS` | `0` | Legacy/manual audio delay (typically `0`) |
| `SIDECAR_DEBUG_LOGS` | `1` (in source default; operators typically set `0` in production) | Verbose debug logging |
| `VIDEO_READ_RTP_BUFFER` | `4194304` | OS UDP socket read buffer (video) |
| `AUDIO_READ_RTP_BUFFER` | `1048576` | OS UDP socket read buffer (audio) |
| `VIDEO_BUFSIZE` | `1M` | FFmpeg video buffer size |
| `VIDEO_WIDTH`, `VIDEO_HEIGHT`, `VIDEO_FRAMERATE`, `VIDEO_BITRATE`, `AUDIO_BITRATE` | `1280`, `720`, `30`, `1500k`, `128k` | FFmpeg fallbacks if `POST /source` did not specify |

### 3.4 Default STUN servers (sidecar)

The reference system ships with 9 default STUN entries (8 anonymous IPs and Google's public STUN). An implementation MAY ship a different default list provided that:

- The default list contains at least one publicly reachable STUN server;
- The list is overridable by `STUN_SERVERS` (comma-separated `stun:host:port` entries).

The reference list is informational only and SHOULD NOT be replicated verbatim by an implementation that wishes to remain operationally independent.

### 3.5 TeamSpeak ServerQuery event names

The reference system listens for the following event names exactly as TeamSpeak delivers them, and forwards them into the flow engine. These names are part of the TeamSpeak API and the implementation MUST use them verbatim:

| Event name | Origin |
|---|---|
| `notifycliententerview` | TS direct |
| `notifyclientleftview` | TS direct |
| `notifyclientmoved` | TS direct |
| `notifyclientupdated` | TS direct |
| `notifyserveredited` | TS direct |
| `notifychanneledited` | TS direct |
| `notifychanneldescriptionchanged` | TS direct |
| `notifychannelcreated` | TS direct |
| `notifychanneldeleted` | TS direct |
| `notifychannelmoved` | TS direct |
| `notifychannelpasswordchanged` | TS direct |
| `notifytextmessage` | TS direct |
| `notifytokenused` | TS direct |

The implementation MUST also synthesise the following higher-level event names from `notifyclientupdated` payloads and from join/leave correlations. These are part of the **flow engine's** external API and MUST be preserved verbatim because operator-authored flows reference them by name:

| Synthetic event name | Derivation |
|---|---|
| `client_went_away` | `client_away` toggled to `1` |
| `client_came_back` | `client_away` toggled to `0` |
| `client_mic_muted` / `client_mic_unmuted` | `client_input_muted` toggled |
| `client_sound_muted` / `client_sound_unmuted` | `client_output_muted` toggled |
| `client_mic_disabled` / `client_mic_enabled` | `client_input_hardware` toggled |
| `client_sound_disabled` / `client_sound_enabled` | `client_output_hardware` toggled |
| `client_group_added` / `client_group_removed` | `client_servergroups` membership delta between successive snapshots |
| `client_recording_started` / `client_recording_stopped` | `client_is_recording` toggled |
| `client_nickname_changed` | `client_nickname` differs between successive snapshots for the same `clid` |

The five TeamSpeak event-registration channel names — `server`, `channel`, `textserver`, `textchannel`, `textprivate` — MUST be subscribed to via the TeamSpeak `servernotifyregister` command to receive the events above. The implementation chooses when to register; the reference registers all five immediately after SSH login.

### 3.6 Music-bot in-channel chat commands

The reference exposes the following operator-visible commands. They MUST be preserved verbatim (case-sensitive, leading `!`):

| Command | Effect |
|---|---|
| `!radio` | List available radio stations for this server |
| `!radio <id>` | Begin playing the radio station with the given numeric id |
| `!play <url>` | Enqueue the YouTube/HTTP URL and begin playback if idle |
| `!play` (no arg) | Resume paused playback |
| `!stop` | Stop playback and clear pacing |
| `!pause` | Toggle paused / resumed |
| `!skip` / `!next` | Advance to the next queue entry |
| `!prev` | Step back to the previous queue entry |
| `!vol` | Report current volume |
| `!vol <0-100>` | Set volume (clamped) |
| `!np` | Report the currently playing track |

### 3.7 Flow trigger and action type tags

These string literals are persisted in flow JSON in the database and MUST be preserved verbatim. Renaming any of them would silently break stored flows.

**Trigger types:** `event`, `cron`, `webhook`, `command`.

**Action types:** `kick`, `ban`, `move`, `message`, `poke`, `channelCreate`, `channelEdit`, `channelDelete`, `groupAddClient`, `groupRemoveClient`, `webquery`, `webhook`, `httpRequest`, `afkMover`, `idleKicker`, `pokeGroup`, `rankCheck`, `tempChannelCleanup`, `animatedChannel`, `generateCode`, `valkeyGet`, `valkeySet`, `valkeyDelete`, `groupRemoveAll`, `groupRestoreList`, `generateToken`, `setClientChannelGroup`.

**Other node `nodeType` tags:** `condition`, `delay`, `variable`, `log`.

**Animated-channel style tags:** `scroll`, `typewriter`, `bounce`, `blink`, `wave`, `alternateCase`.

**Variable operations:** `set`, `increment`, `append`.

**Variable scopes:** `flow`, `temp`.

**Repeat modes (music bot):** `off`, `track`, `queue`.

**Voice-bot status states:** `stopped`, `starting`, `connected`, `playing`, `paused`, `error`.

**Widget themes:** `dark`, `light`, `transparent`, `neon`, `military`, `minimal`.

**Widget channel-spacer types:** `line`, `dotline`, `dashline`, `center`, `left`, `right`, `none`.

**Video stream presets:** `480p`, `720p`, `1080p`.

### 3.8 TeamSpeak fixed-meaning enum values

These integer constants appear directly in JSON wire types between front-end and back-end and are also defined by TeamSpeak itself. The implementation MUST use the same values:

| Field | Value | Meaning |
|---|---|---|
| Kick `reasonid` | `4` | Kick from channel |
| Kick `reasonid` | `5` | Kick from server |
| Message `targetmode` | `1` | Private message to a client |
| Message `targetmode` | `2` | Channel message |
| Message `targetmode` | `3` | Server message |
| Token / privilege key `tokentype` | `0` | Server-group token |
| Token / privilege key `tokentype` | `1` | Channel-group token |
| Codec `channel_codec` | `0`–`5` | `Speex Narrowband`, `Speex Wideband`, `Speex Ultra-Wideband`, `CELT Mono`, `Opus Voice`, `Opus Music` |
| File entry `type` | `0` | File |
| File entry `type` | `1` | Directory |

### 3.9 Public widget routes

These routes are bookmarked by widget viewers and embedded in third-party HTML, and therefore MUST be preserved verbatim:

| Route | Returns |
|---|---|
| `GET /widget/<token>` | HTML page rendering the widget |
| `GET /widget/<token>.svg` | `image/svg+xml` |
| `GET /widget/<token>.png` | `image/png` |
| `GET /widget/<token>.json` | `application/json` (machine-readable widget data) |

### 3.10 Sidecar HTTP control routes

These routes are called by the back-end and MUST match exactly:

| Route | Body in | Body out |
|---|---|---|
| `POST /peer/create` | `{"id": string}` | `{"sdp": string}` |
| `POST /peer/answer` | `{"id": string, "sdp": string}` | `{"status": "ok"}` |
| `POST /peer/ice` | `{"id": string, "candidate": string, "sdpMid": string, "sdpMLineIndex": uint16}` | `{"status": "ok"}` |
| `POST /peer/close` | `{"id": string}` | `{"status": "ok"}` |
| `POST /source` | `{"source": string, "width": int, "height": int, "framerate": int, "bitrate": string}` | `{"status": "ok"}` |
| `POST /source/stop` | `{}` | `{"status": "ok"}` |
| `GET /stats` | — | `{"videoPort": int, "audioPort": int, "peerCount": int, "peers": object, "source": string}` |
| `GET /health` | — | `{"status": "ok", "videoPort": int, "audioPort": int}` |

### 3.11 Database column names exposed in wire types

The following snake- or camelCase field names appear in JSON between front-end and back-end and MUST match the database column names so that wire types and persistence stay aligned. They are listed in full in Chapter 4 (Data Model). The implementer MUST keep them stable across schema migrations.

### 3.12 How to verify

- A ServerQuery event-name conformance test: subscribe to `server`, `channel`, `textserver`, `textchannel`, `textprivate` channels on a real TeamSpeak server, generate each of the 13 direct events, and confirm the implementation receives them with the names listed in §3.5.
- A flow-storage-stability test: create a flow with at least one node of every action type listed in §3.7, persist it, restart the back-end, reload the flow, and confirm the stored type tags round-trip unchanged.
- A widget URL stability test: create a widget, retrieve all four formats listed in §3.9, and confirm each returns the correct content type.

---

# Part II — Persistent Data and External Contracts

## Chapter 4. Data Model

This chapter is normative for the database schema. All field names listed in **Persistence shape** blocks are external-contract identifiers (they appear in JSON wire types) and MUST be preserved verbatim.

### 4.1 Identifier discipline

The reference uses SQLite for storage. The implementation MAY use any relational database that supports:

- 64-bit integer primary keys with autoincrement;
- `UNIQUE` constraints, including composite uniques;
- Cascading delete on foreign keys;
- A timestamp type with at least second resolution (the reference uses `DateTime`).

All primary keys are auto-assigned 32-bit (or wider) integers unless explicitly stated. All `createdAt` / `updatedAt` columns are server-managed timestamps (created on insert; `updatedAt` updated on every row update).

### 4.2 Entities

The schema contains 17 entities. Below, each entity is described as a **persistence shape** with field name, type, optionality, default, and notes.

#### 4.2.1 `User`

The operator account record. There is no concept of "anonymous user"; every authenticated request is bound to a `User`.

| Field | Type | Notes |
|---|---|---|
| `id` | int, PK | |
| `username` | string, unique | Operator-chosen handle, used for login |
| `passwordHash` | string | bcrypt hash, see §6.2 |
| `displayName` | string | Free-form label shown in UI; defaults to username if not set at creation |
| `role` | string | One of `admin`, `moderator`, `viewer`; default `viewer` at the database level |
| `enabled` | boolean | default `true`; if `false`, all login and authenticated-request attempts MUST fail |
| `createdAt`, `updatedAt` | timestamp | |
| `lastLoginAt` | timestamp, nullable | Updated on every successful login |

Relations: one-to-many `serverAccess` (`UserServerAccess`), one-to-many `refreshTokens` (`RefreshToken`); both cascade on delete.

#### 4.2.2 `RefreshToken`

A long-lived bearer token used to mint short-lived access tokens. Subject to rotation and reuse detection (see Chapter 6).

| Field | Type | Notes |
|---|---|---|
| `id` | int, PK | |
| `token` | string, unique | The bearer string. Reference: 64 random bytes, hex-encoded → 128 hex characters |
| `userId` | int, FK → `User.id`, cascade | |
| `expiresAt` | timestamp | Reference: `now + 7 days` |
| `createdAt` | timestamp | |
| `family` | string, nullable | Random short id (reference: nanoid, ~21 chars). All tokens issued from a single login share a family. Used by reuse-detection. |
| `replacedBy` | string, nullable | When this token is rotated, its successor's `token` value is recorded here. Used by reuse-detection. |

#### 4.2.3 `UserServerAccess`

A grant. Restricts non-admin users to specified TeamSpeak server connections.

| Field | Type | Notes |
|---|---|---|
| `id` | int, PK | |
| `userId` | int, FK → `User.id`, cascade | |
| `serverConfigId` | int, FK → `TsServerConfig.id`, cascade | |
| **Composite unique** on `(userId, serverConfigId)` | | |

Admins implicitly have access to all server configs and need no rows in this table.

#### 4.2.4 `TsServerConfig`

A connection record describing one managed TeamSpeak server. Holds credentials.

| Field | Type | Notes |
|---|---|---|
| `id` | int, PK | |
| `name` | string | Operator-supplied label |
| `host` | string | Hostname or IP |
| `webqueryPort` | int | default `10080` |
| `apiKey` | string | **Encrypted at rest** (§6.3). The encrypted ciphertext is stored in this field as `enc:<iv-hex>:<tag-hex>:<ct-hex>`. |
| `useHttps` | boolean | default `false` |
| `sshPort` | int | default `10022` |
| `sshUsername` | string, nullable | |
| `sshPassword` | string, nullable | **Encrypted at rest** when present |
| `queryBotChannel` | string, nullable | Channel id (as string) the SSH "query bot" should join on connect. If unset, the server's default channel is used. |
| `queryBotNickname` | string, nullable | Nickname the SSH query-bot session uses |
| `sshBotNickname` | string, nullable | Nickname for a separate SSH bot session, if used |
| `enabled` | boolean | default `true` |
| `createdAt`, `updatedAt` | timestamp | |

Relations: one-to-many `userAccess` (`UserServerAccess`), `botFlows` (`BotFlow`), `botLogs` (`BotExecutionLog`), `musicBots` (`MusicBot`), `songs` (`Song`), `radioStations` (`RadioStation`), `widgets` (`Widget`), `musicRequests` (`MusicRequest`); all cascade.

> **Wire-shape note:** the `apiKey` field is **never** returned to clients in cleartext. The server-side `ServerConfig` wire type omits `apiKey`, exposing only `hasSshCredentials: boolean` (a boolean indicating whether SSH credentials are present, not the credentials themselves).

#### 4.2.5 `BotFlow`

A persisted automation flow.

| Field | Type | Notes |
|---|---|---|
| `id` | int, PK | |
| `name` | string | |
| `description` | string, nullable | |
| `flowData` | string | JSON-serialised `FlowDefinition` (see §12.1). Default: `{"nodes":[],"edges":[]}`. |
| `serverConfigId` | int, FK → `TsServerConfig.id`, cascade | |
| `virtualServerId` | int | default `1`. Identifies the TeamSpeak virtual server within the configured TS instance. |
| `enabled` | boolean | default `false`. Disabled flows do not respond to triggers. |
| `createdAt`, `updatedAt` | timestamp | |

Relations: one-to-many `executions` (`BotExecution`), `variables` (`BotVariable`).

#### 4.2.6 `BotVariable`

Persisted per-flow state.

| Field | Type | Notes |
|---|---|---|
| `id` | int, PK | |
| `flowId` | int, FK → `BotFlow.id`, cascade | |
| `name` | string | |
| `value` | string | All variable values are stored as strings. Type coercion happens at evaluation time (§15.1). |
| `scope` | string | default `flow`; legal values `flow` and `temp` |
| **Composite unique** on `(flowId, name, scope)` | | |

`temp`-scoped variables MUST be deleted at the end of each flow execution; only `flow`-scoped variables persist across executions.

#### 4.2.7 `BotExecution`

A single run of a flow.

| Field | Type | Notes |
|---|---|---|
| `id` | int, PK | |
| `flowId` | int, FK → `BotFlow.id`, cascade | |
| `triggeredBy` | string | Identifier of the trigger node that started the run, plus optional context (e.g., event name) |
| `triggerData` | string, nullable | JSON-encoded trigger payload (event fields, webhook body, etc.) |
| `status` | string | `running`, `completed`, `failed`, `cancelled`. default `running` |
| `startedAt` | timestamp | |
| `endedAt` | timestamp, nullable | |
| `error` | string, nullable | Free-form error message if `status = failed` |

Relations: one-to-many `logs` (`BotExecutionLog`).

#### 4.2.8 `BotExecutionLog`

Append-only run log.

| Field | Type | Notes |
|---|---|---|
| `id` | int, PK | |
| `executionId` | int, FK → `BotExecution.id`, cascade, nullable | Null only for global / engine-level log entries |
| `serverConfigId` | int, FK → `TsServerConfig.id` (no cascade) | |
| `flowId` | int, nullable | denormalised for query speed |
| `nodeId` | string, nullable | The flow-node id (a UUID-ish string assigned at flow-design time) that emitted the entry |
| `nodeName` | string, nullable | denormalised label of that node |
| `level` | string | `debug`, `info`, `warn`, `error`. default `info` |
| `message` | string | |
| `data` | string, nullable | JSON-encoded structured payload |
| `timestamp` | timestamp | |
| **Indexed** on `executionId`, `flowId`, `timestamp` | | |

#### 4.2.9 `AppSetting`

A simple key-value store for application-level settings.

| Field | Type | Notes |
|---|---|---|
| `key` | string, PK | |
| `value` | string | |
| `updatedAt` | timestamp | |

The reference seeds at least one setting on first start: `max_music_bots = "5"`. Implementations SHOULD seed the same key with the same default to preserve operator UX.

#### 4.2.10 `MusicBot`

A configured music bot. The bot's *runtime* state (current track, queue, status) is held in memory and is not persisted; only the *configuration* lives here.

| Field | Type | Notes |
|---|---|---|
| `id` | int, PK | |
| `name` | string | |
| `serverConfigId` | int, FK → `TsServerConfig.id`, cascade | |
| `nickname` | string | default `MusicBot` |
| `serverPassword` | string, nullable | If the TS virtual server requires a password |
| `defaultChannel` | string, nullable | Channel id the bot joins on connect |
| `channelPassword` | string, nullable | If the default channel is password-protected |
| `nowPlayingChannelId` | string, nullable | Channel whose description the bot updates with the current track |
| `voicePort` | int | default `9987` |
| `volume` | int | default `50` (range 0–100) |
| `identityData` | string, nullable | TS3 identity blob (private key + metadata) generated for this bot. **Treated as sensitive**; not exposed in standard list queries (see §7.5). |
| `autoStart` | boolean | default `false`. If `true`, the back-end MUST attempt to connect this bot on boot. |
| `streamPreset` | string | default `720p`. Used when this bot is also the host of a video stream (Part VII). |
| `sidecarPort` | int | default `9800`. The sidecar control port the back-end will contact for this bot's video sessions. |
| `createdAt`, `updatedAt` | timestamp | |

Relations: one-to-many `playlists` (`Playlist`).

#### 4.2.11 `Song`

A library entry.

| Field | Type | Notes |
|---|---|---|
| `id` | int, PK | |
| `title` | string | |
| `artist` | string, nullable | |
| `duration` | float, nullable | seconds |
| `filePath` | string | absolute path on disk under `MUSIC_DIR` (or whichever sub-tree the operator uses) |
| `source` | string | `local`, `youtube`, or `url`. default `local` |
| `sourceUrl` | string, nullable | Original URL if `source ≠ local` |
| `fileSize` | int, nullable | bytes |
| `serverConfigId` | int, FK → `TsServerConfig.id`, cascade | A song belongs to a server's library, not to a bot |
| `createdAt` | timestamp | |

Relations: many-to-many with `Playlist` via `PlaylistSong`.

#### 4.2.12 `Playlist`

| Field | Type | Notes |
|---|---|---|
| `id` | int, PK | |
| `name` | string | |
| `musicBotId` | int, FK → `MusicBot.id`, **set null on delete** | A playlist may exist independently of a bot; deleting a bot does NOT delete its playlists |
| `createdAt` | timestamp | |

#### 4.2.13 `PlaylistSong`

The join table — explicit, with `position`.

| Field | Type | Notes |
|---|---|---|
| `id` | int, PK | |
| `playlistId` | int, FK → `Playlist.id`, cascade | |
| `songId` | int, FK → `Song.id`, cascade | |
| `position` | int | default `0`. The intended playback order; ties allowed; sort stable |
| **Composite unique** on `(playlistId, songId)` | | A song appears at most once in a given playlist |

#### 4.2.14 `RadioStation`

A pre-configured streaming radio URL.

| Field | Type | Notes |
|---|---|---|
| `id` | int, PK | |
| `name` | string | |
| `url` | string | The HTTP stream URL. Validated for SSRF safety on insert (§9). |
| `genre` | string, nullable | |
| `imageUrl` | string, nullable | |
| `serverConfigId` | int, FK → `TsServerConfig.id`, cascade | Radio stations are per-server |
| `createdAt` | timestamp | |

#### 4.2.15 `Widget`

A public widget definition.

| Field | Type | Notes |
|---|---|---|
| `id` | int, PK | |
| `name` | string | Operator label |
| `token` | string, unique | URL-safe random token; the only credential a viewer needs to render the widget |
| `serverConfigId` | int, FK → `TsServerConfig.id`, cascade | |
| `virtualServerId` | int | default `1` |
| `theme` | string | One of the six theme names (§3.7); default `dark` |
| `showChannelTree` | boolean | default `true` |
| `showClients` | boolean | default `true` |
| `hideEmptyChannels` | boolean | default `false` |
| `maxChannelDepth` | int | default `5` |
| `createdAt`, `updatedAt` | timestamp | |

#### 4.2.16 `MusicRequest`

A history of YouTube/HTTP URLs that have been queued through the music bot system. Used both for analytics and as a deduplicated suggestions cache.

| Field | Type | Notes |
|---|---|---|
| `id` | int, PK | |
| `title` | string | |
| `url` | string | |
| `serverConfigId` | int, FK → `TsServerConfig.id`, cascade | |
| `requestedAt` | timestamp | |
| **Composite unique** on `(serverConfigId, url)` | | Re-requesting the same URL on the same server does not create a duplicate row |

#### 4.2.17 `StreamSession`

A history record for video streaming sessions. Not a foreign-keyed entity; informational only.

| Field | Type | Notes |
|---|---|---|
| `id` | int, PK | |
| `musicBotId` | int | The bot that hosted the stream. **Not** declared as a formal FK in the reference. |
| `source` | string | The stream URL or YouTube video id |
| `preset` | string | default `720p` |
| `startedAt` | timestamp | |
| `endedAt` | timestamp, nullable | |
| `peakViewers` | int | default `0` |

### 4.3 Entity-relationship summary

```
User ──┬─< RefreshToken
       └─< UserServerAccess >── TsServerConfig ──┬─< BotFlow ──┬─< BotVariable
                                                 │              ├─< BotExecution ──< BotExecutionLog
                                                 │              └ (schedule via cron / event)
                                                 ├─< MusicBot ──< Playlist ──< PlaylistSong >── Song
                                                 ├─< Song
                                                 ├─< RadioStation
                                                 ├─< Widget
                                                 └─< MusicRequest

(StreamSession is unattached to TsServerConfig; it references MusicBot informally.)
```

### 4.4 Schema versioning

The reference applies migrations as ordered, append-only SQL files. Two migrations are present at the pinned commit; both add a single nullable string column to `TsServerConfig` (`queryBotNickname`, `sshBotNickname`). The implementation MUST:

- Apply migrations on container start, before opening the REST listener;
- Refuse to start if any migration fails;
- Set SQLite `journal_mode=MEMORY` after migrations apply (this avoids a class of SQLite-full failures observed by the reference operators; see §16.2).

### 4.5 How to verify

- A schema-roundtrip test: insert one row of every entity above, then read it back and confirm every column.
- A cascade test: delete a `User` row; verify `RefreshToken` and `UserServerAccess` rows for that user are removed.
- A no-cascade test: delete a `MusicBot` row; verify any `Playlist` rows that referenced it survive with `musicBotId` set to null.
- A composite-unique test: attempt to insert two `PlaylistSong` rows with the same `(playlistId, songId)`; the second insert MUST fail.
- A migration replay test: start from an empty database and apply migrations in order; the resulting schema MUST match the live system.

---

## Chapter 5. Configuration Surface

This chapter normalises every operator-tunable knob.

### 5.1 Environment variables — back-end

The full list and reference defaults appears in §3.2. Below, each variable is described in terms of its semantic effect, validation rules, and required behaviour on missing/invalid values.

#### `JWT_SECRET`

The HMAC key used to sign JWT access tokens (HS256). Required in production.

- If the variable is unset *or* equals the placeholder string the reference ships with for development (`dev-secret-change-me-in-production`), the back-end MUST refuse to start when `NODE_ENV=production`. In development mode, the back-end SHOULD log a warning and continue with the placeholder.
- Length recommendation: ≥32 characters of entropy. The implementation SHOULD NOT enforce a minimum length, but SHOULD warn the operator if the supplied value is shorter than 32 bytes.

#### `ENCRYPTION_KEY`

The key (or seed) used to derive the AES-256-GCM key for credential encryption (§6.3). Independent of `JWT_SECRET`.

- If unset, the back-end MUST fall back to using `JWT_SECRET` as the seed and SHOULD log a warning.
- In production, if unset, the implementation MAY treat this as fatal (the reference does); at minimum it MUST log loudly.
- If set to the same value as `JWT_SECRET`, the implementation SHOULD warn ("use separate values for better security") but MUST NOT refuse to start.
- Rotating `ENCRYPTION_KEY` invalidates all existing encrypted-at-rest values. Operators are responsible for re-encrypting; the implementation MAY provide a migration tool but is not required to.

#### `NODE_ENV`

Switches behaviour between development and production. Recognised values are `development` (default) and `production`. Other values MUST be treated as development.

#### `PORT`

The TCP port the back-end binds to for both REST and WebSocket. Default `3001`.

#### `DATABASE_URL`

A SQLite connection URL of the form `file:<path>` (or another scheme if the implementation uses a different RDBMS). Reference default: `file:./data/ts6webui.db`.

> **Open Question (Q-5.1):** The reference exhibits an inconsistency — its config module defaults to `ts6webui.db` while its boot path falls back to `ts6manager.db`, and its docker-compose injects `/app/packages/backend/data/ts6webui.db`. The implementation MUST pick a single name and use it consistently. Recommended: `ts6webui.db`, matching the docker-compose default that operators are most likely to have in production.

#### `JWT_ACCESS_EXPIRY`

Lifetime of access tokens. Reference default: `15m`. Accepts strings parseable by the conventional duration format (`<n>s`, `<n>m`, `<n>h`, `<n>d`).

#### `JWT_REFRESH_EXPIRY`

Lifetime of refresh tokens. Reference default: `7d`. Same format as access expiry. The implementation MUST treat the refresh token as expired the moment its `expiresAt` row column is in the past, regardless of whether the env-var-derived value still endorses it (i.e., the database is the source of truth for individual tokens).

#### `FRONTEND_URL`

The CORS allowlist origin. Exactly one origin SHOULD be supported. Reference defaults: `http://localhost:5173` (development) and `http://localhost:3000` (production via docker-compose).

The implementation MUST set CORS `credentials: true` so that the front-end can send the refresh token in the request body (the refresh flow uses bearer-in-body, not cookies; see §6.5).

#### `MUSIC_DIR`

Directory under which `Song.filePath` values are rooted. Reference default: `/data/music`. The implementation MUST:

- Create the directory on first need if it does not exist;
- Reject any `Song.filePath` insert that escapes this root (path-traversal guard);
- Refuse to serve files for download whose resolved path is not inside this directory.

#### `SIDECAR_URL`

Full URL of the WebRTC sidecar's HTTP control plane (e.g., `http://ts6-sidecar:9800`). Required when the sidecar runs in a separate container or host. If unset, the back-end MAY spawn a sidecar binary directly using `SIDECAR_BINARY_PATH`.

#### `SIDECAR_BINARY_PATH`

Filesystem path to a sidecar binary the back-end may exec to start the sidecar in-process-tree. Mutually informational with `SIDECAR_URL`: if both are set, `SIDECAR_URL` takes precedence and the binary path is ignored.

#### `YT_COOKIE_FILE`

Path to a Netscape-format `cookies.txt` file passed through to `yt-dlp`. The implementation MUST:

- Prefer this env-var path if set and the file exists.
- Otherwise, look for a saved file at `<data_dir>/yt-cookies.txt` (under the same data root as the database) and use it if present.
- If the env-var path is set but the file is missing, log a warning and proceed without cookies.

The path is also writable through the UI (see §7.x — Settings routes), which causes the implementation to update the saved file at the second location above.

#### `TS_ALLOW_SELF_SIGNED`

If `true` or `1`, the WebQuery HTTP client accepts self-signed TLS certificates from TeamSpeak servers. Default `false`. Production deployments using TLS to TeamSpeak SHOULD use a properly signed cert and leave this at the default.

### 5.2 Environment variables — sidecar

The sidecar's environment is described in §3.3. The variables fall into three categories:

- **Listener configuration** (`SIDECAR_PORT`): the HTTP control port.
- **STUN configuration** (`STUN_SERVERS`): comma-separated list overriding the built-in default. Required in deployments where the built-in defaults are unreachable.
- **Media-plane tuning** (`VIDEO_QUEUE_SIZE`, `AUDIO_QUEUE_SIZE`, `SYNC_PLAYOUT_BUFFER_MS`, `SYNC_VIDEO_BIAS_MS`, `AUDIO_DELAY_MS`, `VIDEO_READ_RTP_BUFFER`, `AUDIO_READ_RTP_BUFFER`, `VIDEO_BUFSIZE`, `VIDEO_WIDTH`/`HEIGHT`/`FRAMERATE`/`BITRATE`, `AUDIO_BITRATE`): tuning knobs for the FFmpeg invocation and the RTP forwarding pipeline. Default values are listed in §3.3 and the implementation SHOULD honour them as starting points.

Operators are not expected to tune these in normal operation; they exist for performance regression diagnosis.

### 5.3 On-disk layout

A reference deployment uses:

```
<container-mounted-data>/
  ts6webui.db                  # SQLite database
  ts6webui.db-shm, .wal        # SQLite auxiliary files (if WAL mode were enabled; see Q-5.2)
  yt-cookies.txt               # Optional, operator-uploaded yt-dlp cookies

<container-mounted-music>/
  <serverConfigId>/<source>/<filename>   # Song.filePath layout
```

> **Open Question (Q-5.2):** The reference sets `journal_mode=MEMORY` rather than WAL. This trades a class of stability properties for performance. The implementation MUST decide which mode to use and document it; switching modes requires SQLite `PRAGMA` execution after every fresh connection.

### 5.4 Settings persisted in the database

The `AppSetting` table is the back-end's KV store for runtime-tunable settings. The implementation MUST seed at least:

| Key | Reference default | Effect |
|---|---|---|
| `max_music_bots` | `"5"` | Cap on the number of music bots a single operator may have running at once across all servers |

Other keys MAY be added freely; the implementation MUST treat unknown keys as opaque strings and pass them through.

### 5.5 Operator-managed yt-dlp cookies

Two redundant paths exist: an operator may either (a) set `YT_COOKIE_FILE` to a path on the host they have mounted in, or (b) upload a cookies file through the UI's settings page, which the back-end writes to the in-data-directory location. At runtime, the back-end resolves the cookie file in this priority:

1. `YT_COOKIE_FILE` env var, if set and file exists;
2. `<data-directory>/yt-cookies.txt`, if file exists;
3. No cookies (yt-dlp invoked without `--cookies`).

### 5.6 Production hardening checklist

For an operator-facing readiness check, the implementation SHOULD on startup log a clearly formatted block listing:

- Whether `JWT_SECRET` is set (and not the dev placeholder);
- Whether `ENCRYPTION_KEY` is set (and distinct from `JWT_SECRET`);
- Whether `NODE_ENV=production`;
- Whether the database is reachable and migrations are applied;
- Whether the sidecar is reachable (HTTP `GET /health` returns `200`);
- Whether the cache is configured (and reachable, if so).

The implementation MUST NOT log secret *values*, only their presence/absence.

### 5.7 How to verify

- A boot-with-default-secret test: set `NODE_ENV=production`, leave `JWT_SECRET` unset; the back-end MUST exit with a non-zero status and a log message naming `JWT_SECRET`.
- A cookie-priority test: set `YT_COOKIE_FILE` to a non-existent path; place a valid file at the saved location; the implementation MUST log "cookie file not found" but not fall back to the saved file when the env var is set (the reference's priority ordering MUST be preserved). Re-test with env var unset and saved file present; the saved file MUST be used.
- A path-traversal test: attempt to insert a `Song.filePath` outside `MUSIC_DIR`; the implementation MUST reject the insert.

---

## Chapter 6. Security Model

### 6.1 Roles and capabilities

Three roles exist, with strict ordering:

| Role | Capabilities |
|---|---|
| `admin` | Create/delete users; manage all server connections; bypass per-server access checks; full read/write on all flows, music bots, widgets, settings |
| `moderator` | Read/write on flows, music bots, widgets, and TS-control actions (kick/ban/move/etc.) within servers granted by `UserServerAccess` |
| `viewer` | Read-only on the same scope as moderator |

The role enum lives at three layers and must remain consistent: the database `User.role` column, the JWT payload `role` claim, and the wire type `UserInfo.role`. The database default is `viewer`, but the reference does not formally constrain `User.role` to the three values above; the implementation MUST treat any value other than `admin`, `moderator`, `viewer` as `viewer`.

> **Note:** The reference has a residual inconsistency — its `UserRole` type alias in one shared module declares only `'admin' | 'viewer'`, while its API wire types declare `'admin' | 'moderator' | 'viewer'`. The implementation MUST adopt the three-role model consistently across all surfaces.

### 6.2 Password handling

#### 6.2.1 Hashing

- Algorithm: bcrypt.
- Cost factor: 12 rounds.
- Hashes are stored in `User.passwordHash`.
- Comparison uses constant-time bcrypt compare.

The implementation MAY substitute a stronger algorithm (Argon2id is recommended) provided that:

- Existing bcrypt hashes can still be verified during a transition period;
- New password sets use the chosen algorithm;
- Both algorithms produce a fixed-format hash string self-describing enough that the verifier can pick the right routine.

#### 6.2.2 Complexity rules

When a password is set (during initial setup, user creation, or password change), the implementation MUST validate against:

- Length ≥ 8 characters;
- Contains at least one uppercase letter (`[A-Z]`);
- Contains at least one lowercase letter (`[a-z]`);
- Contains at least one digit (`[0-9]`);
- Contains at least one character from the set: `!@#$%^&*()_+\-=\[\]{}|;':",./<>?`.

On failure, the implementation MUST return HTTP 400 with a human-readable error string identifying which rule failed (e.g., `"Password must contain at least one uppercase letter"`).

#### 6.2.3 Password change

The `PUT /api/auth/password` route requires the **current** password (re-verified by bcrypt-compare) and a **new** password (validated against the rules above). On success, the implementation MUST:

1. Hash the new password (bcrypt cost 12) and store it.
2. **Revoke every refresh token belonging to that user** (delete all `RefreshToken` rows where `userId` matches). This forces re-login on all the user's other sessions.
3. Return `204 No Content`.

### 6.3 Credential encryption at rest

Two persisted fields are sensitive: `TsServerConfig.apiKey` and `TsServerConfig.sshPassword`. These MUST be encrypted at rest using AES-256-GCM as follows.

#### 6.3.1 Key derivation

The encryption key is a 32-byte buffer derived once per process from a string seed:

```
seed   := ENCRYPTION_KEY (env) or JWT_SECRET if ENCRYPTION_KEY is unset
salt   := the literal ASCII string "ts6-webui-enc-v1"
key    := scrypt(seed, salt, 32 bytes)   // default scrypt parameters
```

The literal salt is an external-contract value: changing it invalidates all existing ciphertexts. The implementation MUST use the literal string `ts6-webui-enc-v1`.

The derived key is cached in process memory for the process lifetime. It MUST NOT be written to disk or to logs.

#### 6.3.2 Encryption format

For a plaintext string `P`:

```
iv   := 12 random bytes
ct,tag := AES-256-GCM(key, iv, P)
storage = "enc:" + hex(iv) + ":" + hex(tag) + ":" + hex(ct)
```

The literal prefix `enc:` is an external-contract marker. The implementation MUST emit it and MUST recognise it on decrypt.

#### 6.3.3 Decryption and plaintext migration

On decrypt, the implementation MUST:

- If the input string does NOT start with `enc:`, return it as-is (treat as legacy plaintext).
- Otherwise, split on `:` into exactly four parts (`enc`, `iv-hex`, `tag-hex`, `ct-hex`); reject if not exactly four; decrypt and return.

The plaintext-passthrough behaviour exists so that a deployment migrating from an older version (which stored these fields in the clear) keeps working until each row is re-saved (re-saving re-encrypts).

#### 6.3.4 Re-encryption on update

On every `UPDATE` of `TsServerConfig.apiKey` or `TsServerConfig.sshPassword`, the implementation MUST re-encrypt with a fresh IV. The implementation MUST NOT reuse an IV across encryptions of distinct plaintexts (this is a hard AES-GCM correctness property).

### 6.4 JWT access tokens

Access tokens are short-lived bearer credentials issued by `POST /api/auth/login` and `POST /api/auth/refresh`.

| Property | Value |
|---|---|
| Algorithm | HMAC-SHA-256 (HS256) |
| Secret | `JWT_SECRET` env var |
| Lifetime | `JWT_ACCESS_EXPIRY` (default `15m`) |
| Payload claims | `id` (int, user id), `username` (string), `role` (string), plus standard `iat` and `exp` |
| Transmission | `Authorization: Bearer <token>` header |

#### 6.4.1 Verification middleware

Every authenticated request MUST be processed by middleware that:

1. Reads `Authorization`, requires the `Bearer ` prefix; returns `401 {"error":"No token provided"}` on absence/wrong prefix.
2. Verifies the JWT signature with HS256. On verification failure, returns `401 {"error":"Invalid or expired token"}`.
3. Looks up the user row by `payload.id`. If the user does not exist or `enabled=false`, returns `401 {"error":"User account disabled or deleted"}`.
4. Builds an effective request-context user object combining the JWT payload with the *fresh* role from the database lookup — the JWT's role MUST NOT be trusted for authorization; the freshly read role MUST be used.

This per-request DB lookup is a deliberate security property of the reference: revoking a user's role takes effect immediately, without waiting for the access token to expire. The implementation MUST preserve this behaviour. (The implementation MAY add a short-lived in-memory cache of user enable/role to reduce DB load, provided cache TTL ≤ 5 seconds.)

#### 6.4.2 Role enforcement middleware

A second middleware MUST be provided that takes a list of allowed roles and rejects requests whose context user's role is not in the list. Returns `401` if no user is set, `403 {"error":"Insufficient permissions"}` if the role check fails.

### 6.5 Refresh tokens

Refresh tokens are long-lived (default 7 days), opaque, single-use bearer strings.

#### 6.5.1 Token format

64 random bytes encoded as hex (i.e., 128-character hex string). The implementation MUST use a CSPRNG.

#### 6.5.2 Family

Every refresh token belongs to a `family`, a short random id assigned at the moment of login. All refreshes derived from a single login share the same family. Family IDs MUST be unguessable (the reference uses `nanoid` with default 21 characters).

#### 6.5.3 Rotation

`POST /api/auth/refresh` MUST execute the following steps atomically (or with the side effects compatible with at-least-once execution):

1. Look up the supplied `refreshToken` in `RefreshToken`. If not found, jump to §6.5.4 (reuse detection).
2. If found but expired (`expiresAt < now`) or the owning user is disabled, delete the row and return `401`.
3. Generate a new refresh token (64-byte hex). Compute new `expiresAt = now + JWT_REFRESH_EXPIRY`.
4. Update the old row to set `replacedBy = <new token>`.
5. Insert the new row with the same `userId` and same `family` as the old row.
6. Delete the old row.
7. Issue a fresh access token (using the user's current role from DB).
8. Return both tokens to the client.

#### 6.5.4 Reuse detection

If `POST /api/auth/refresh` receives a token that does NOT exist in `RefreshToken`, the implementation MUST check whether the token appears in any row's `replacedBy` field. If yes, the token has been used before — this is the **reuse signal**. The implementation MUST:

1. Log the reuse with severity ≥ warning, identifying the affected `userId`.
2. **Delete every `RefreshToken` row belonging to that user** (revoke all sessions).
3. Return `401`.

If the token is neither present nor referenced by any `replacedBy`, simply return `401`.

#### 6.5.5 Logout

`POST /api/auth/logout` accepts a `refreshToken` in the body and deletes the matching `RefreshToken` row, if any. The route does NOT require authentication (the refresh token itself is the credential). The route returns `204` whether or not a row was deleted. The access token is not revoked server-side; clients MUST discard their local copy.

### 6.6 Per-server access control

For routes scoped under a `:configId` path segment (see §7), a middleware MUST enforce:

- If the request user has role `admin`, allow.
- Otherwise, parse the `:configId` URL parameter. On parse failure (non-integer), return `400 {"error":"Invalid server config ID"}`.
- Look up `UserServerAccess` for `(userId, serverConfigId)`. If absent, return `403 {"error":"No access to this server"}`.
- Otherwise allow.

### 6.7 Network safety: SSRF protection

A shared URL-validation routine guards every outbound HTTP request driven by user-supplied URLs (flow `webhook` and `httpRequest` actions, music-bot stream URLs, video-stream sources, radio station URLs, FFmpeg-fed URLs).

#### 6.7.1 Synchronous checks

The validator MUST:

1. Parse the URL with a strict URL parser; on parse failure, reject with `"Invalid URL format"`.
2. Reject if the protocol is not in the allowed list (default: `http:`, `https:`).
3. Lowercase the hostname and reject if it equals any of: `localhost`, `metadata.google.internal`, `metadata.internal`.
4. Reject if the hostname literally equals a known cloud-metadata IP: `169.254.169.254`, `fd00:ec2::254`.
5. If the hostname parses as an IP literal, reject if the IP is in any of:
   - IPv4: `10.0.0.0/8`, `172.16.0.0/12` (but only `172.16.x.x` through `172.31.x.x`, not the wider `172.0.0.0/8`), `192.168.0.0/16`, `127.0.0.0/8`, `169.254.0.0/16` (link-local), `0.0.0.0/8`.
   - IPv6: `::1`, anything starting `fe80:` (link-local), anything starting `fc` or `fd` (unique-local), and IPv4-mapped IPv6 (`::ffff:` prefix) where the embedded v4 fails the IPv4 check.

#### 6.7.2 DNS rebinding mitigation

After the synchronous checks pass and *before* the actual HTTP request is issued, the validator MUST:

1. Resolve the hostname to an IP via DNS lookup.
2. Apply the IP-range checks above to the resolved IP.
3. If the resolved IP fails the check, reject with an error that names both the hostname and the resolved IP.

If DNS resolution fails (NXDOMAIN, timeout, etc.), the validator MUST allow the request to proceed and let the downstream HTTP/FFmpeg call fail naturally. (Rationale: a DNS failure is not by itself an SSRF signal, and blocking on it would create operational fragility.)

#### 6.7.3 Where the validator is invoked

Every outbound HTTP from user-supplied URLs MUST pass through this validator before the request is dispatched. This includes:

- Flow action `webhook` and `httpRequest` URLs.
- Flow trigger `webhook` payload forwarding (the inbound webhook itself is unconstrained, but any *outbound* request triggered by the flow MUST be validated).
- Music bot radio stream URLs (when first added and on every play, in case DNS has rebound).
- Video stream `source` URLs handed to the sidecar.
- yt-dlp's input URL (yt-dlp itself accepts any URL, but the back-end MUST validate before invoking it).

The validator MUST be a single shared routine; per-call-site duplication is a footgun.

### 6.8 Rate limiting

| Path pattern | Window | Max requests |
|---|---|---|
| `POST /api/auth/login` | 15 minutes | 15 |
| `POST /api/auth/refresh` | 15 minutes | 15 |
| `POST /api/setup/*` | 15 minutes | 15 |
| `POST /api/bots/webhook/*` | 1 minute | 60 |

Rate-limit responses MUST be HTTP 429 with body `{"error":"Too many attempts, please try again later"}` (or `"Too many webhook requests"` for the webhook path). The implementation MUST emit `Retry-After` standard headers.

The rate limiter SHOULD be keyed by source IP. Behind a reverse proxy, the implementation MUST trust exactly one proxy hop (i.e., the leftmost `X-Forwarded-For` entry the proxy added) and MUST NOT trust client-supplied `X-Forwarded-For` values.

### 6.9 HTTP security headers

The implementation MUST emit a sensible set of security headers on all REST responses. The reference uses Helmet defaults, which include:

- `X-Content-Type-Options: nosniff`
- `X-Frame-Options: DENY` (for the back-end's responses; the public widget HTML is served by the same back-end and MUST be served with `X-Frame-Options` removed or set to `SAMEORIGIN` for embeddable rendering — see §27)
- `Strict-Transport-Security` with at least 6-month max-age (production with HTTPS only)
- `Referrer-Policy: no-referrer`
- A reasonable `Content-Security-Policy` for HTML responses

The implementation MAY tighten any of these but MUST NOT loosen `X-Content-Type-Options`.

### 6.10 CORS

`Access-Control-Allow-Origin` is set to the value of `FRONTEND_URL`. Credentials are allowed (`Access-Control-Allow-Credentials: true`) so that the front-end may include the refresh token in request bodies on cross-origin calls during dev.

### 6.11 WebSocket authentication

The WebSocket handshake at path `/ws` MUST accept an access token via the `?token=<jwt>` query string. The handshake handler MUST:

1. Parse the URL and extract `token`. Reject with HTTP 401 if missing.
2. Verify the JWT (HS256, `JWT_SECRET`).
3. Read a user-id field from the verified payload — the implementation MAY use either `id` or `sub`; the reference uses `sub` here but `id` for REST. The implementation MUST pick one and use it consistently. **Recommended: use `id` everywhere.**
4. Confirm the user exists and `enabled=true`. Reject with HTTP 401 if not.

After acceptance, every WebSocket message the back-end sends to that connection MUST carry an event name and data envelope (see Chapter 8).

> **Open Question (Q-6.1):** The reference's use of `sub` for WebSockets but `id` for REST is inconsistent and not documented. The implementation should standardise on one claim; this is a clean-up that does not affect external consumers since the WebSocket-side claim is invisible to clients.

### 6.12 Privileged routes summary

Below is a normative summary of which routes require which protection. Detailed per-route shapes are in Chapter 7.

| Path prefix | Auth required | Role required | Per-server access required |
|---|---|---|---|
| `/api/health` | No | — | — |
| `/api/setup/*` | No (gated by user-count check) | — | — |
| `/api/auth/login` | No | — | — |
| `/api/auth/refresh` | No | — | — |
| `/api/auth/logout` | No | — | — |
| `/api/auth/me` | Yes | any | — |
| `/api/auth/password` | Yes | any | — |
| `/api/widget/*` | No | — | — |
| `/api/bots/webhook/*` | No (rate-limited; secret-checked per flow) | — | — |
| `/api/servers` (list, create) | Yes | any (read) / `admin` (write) | — |
| `/api/servers/:configId/...` | Yes | any (mostly) | Yes |
| `/api/users/*` | Yes | `admin` | — |
| `/api/bots/*` (CRUD on flows) | Yes | any (read) / `admin`+`moderator` (write) | Implicit via flow's serverConfigId |
| `/api/music-bots/*` | Yes | any (read) / `admin`+`moderator` (write) | Implicit via bot's serverConfigId |
| `/api/playlists/*` | Yes | any (read) / `admin`+`moderator` (write) | — |
| `/api/widgets/*` | Yes | `admin`+`moderator` | — |
| `/api/settings/*` | Yes | `admin` | — |

> **Note:** the reference applies role checks somewhat unevenly across the route surface (some write routes are role-gated, some are not). The implementation SHOULD apply the table above as a normative tightening: every write should require at least `moderator`, and admin-scoped settings should require `admin`. Per-route specifics in Chapter 7 reflect this normalised model.

### 6.13 How to verify

- **Token reuse test:** log in, refresh once (obtain `R2` from `R1`), then submit `R1` again. The implementation MUST reject and revoke all sessions for that user.
- **Stale-role test:** log in as a user with role `viewer`, capture the JWT, then upgrade the user to `admin` server-side, then immediately re-issue any authenticated request with the same JWT. The user MUST be treated as `admin` (fresh DB lookup wins).
- **Disabled-user test:** log in, then set `enabled=false`. Subsequent requests MUST receive `401`.
- **SSRF probe set:** attempt to fetch `http://127.0.0.1`, `http://169.254.169.254`, `http://[::1]`, `http://10.1.2.3`, `http://localhost`, `http://my-rebinder.example/` (where `my-rebinder.example` resolves to `10.0.0.1`), `gopher://...`. All MUST be rejected.
- **Encryption roundtrip:** insert a `TsServerConfig` with API key `"hello world"`. Read the row directly from the database; the value MUST start with `enc:` and contain three colon-separated hex fields. Decrypt via the back-end and confirm `"hello world"` is returned.
- **Plaintext-passthrough:** insert a row with `apiKey = "rawkey"` (no `enc:` prefix) directly via SQL; the back-end MUST be able to use the value (treat as plaintext) until the row is next saved through the API, after which it MUST be stored re-encrypted.
- **Rate-limit test:** issue 16 login attempts in 5 seconds; the 16th MUST receive HTTP 429.

---

# Part III — REST API and Realtime

## Chapter 7. REST API Surface

This chapter is a normative catalogue of every HTTP route exposed by the back-end. Routes are presented as compact tables. Where a route is a **thin proxy** to a TeamSpeak ServerQuery command, the table lists the command and the implementation MUST forward the request body's keys (after the parameter-encoding rules in §10.4) to the named command. Where a route has non-trivial back-end logic, a **Notes** column flags the additional behaviour and a sub-section provides detail.

### 7.0 Routing conventions

- All paths are mounted under `/api`. The leading `/api` is omitted in tables for readability.
- `Auth` column abbreviations: `—` no auth; `Y` requires a valid access token; `Y+admin` requires `admin` role; `Y+access` requires per-server access (admin or matching `UserServerAccess`).
- `:configId`, `:sid`, `:cid`, `:clid`, `:cldbid`, `:sgid`, `:cgid`, `:banid`, `:msgid`, `:botId`, `:execId`, `:id`, `:userId`, `:token`, `:tcldbid`, `:fcldbid` are URL parameters; integer-typed ones MUST be parsed via the integer-param helper specified in §7.0.1.
- `Body fields` lists JSON keys the back-end reads from the request body. Anything else is ignored unless an explicit "passthrough" note says otherwise.
- Successful responses are `200 OK` with JSON body unless otherwise noted. `201 Created` is used for resource creation, `204 No Content` for successful operations with no body. Error shapes are governed by §7.0.2.

#### 7.0.1 Integer parameter parsing

URL parameters whose values must be integers MUST be parsed strictly. On parse failure (NaN), the back-end MUST return:

```json
HTTP/1.1 400 Bad Request
Content-Type: application/json

{ "error": "Invalid <name>: must be a number" }
```

Where `<name>` identifies which parameter failed (e.g., `id`, `configId`, `clid`).

#### 7.0.2 Error response shapes

| Class | Status | Body |
|---|---|---|
| Application error (validation, missing required field, rule violation) | the class's chosen status, default `400` | `{ "error": "<message>", "details": "<optional>" }` |
| TeamSpeak API error propagated from upstream | `502` | `{ "error": "TeamSpeak API Error", "code": <int>, "details": "<message>" }` |
| Authentication failure | `401` | `{ "error": "<message>" }` |
| Authorization failure | `403` | `{ "error": "<message>" }` |
| Rate-limit exceeded | `429` | `{ "error": "Too many attempts, please try again later" }` |
| Unmatched route | `404` | `{ "error": "Not found" }` |
| Unhandled exception | `500` | `{ "error": "Internal server error" }` |

The implementation MUST log unhandled exceptions at `error` level with stack trace, but MUST NOT include stack trace in client-visible response.

### 7.1 Health check

| Method | Path | Auth | Notes |
|---|---|---|---|
| GET | `/health` | — | Returns `{ "status": "ok", "timestamp": "<ISO8601>" }` |

### 7.2 Setup

| Method | Path | Auth | Body fields | Notes |
|---|---|---|---|---|
| GET | `/setup/status` | — | — | Returns `{ "needsSetup": <user_count == 0> }` |
| POST | `/setup/init` | — | `username`, `password`, `displayName?` | Creates the very first user as `admin`. Refuses with 403 if any user already exists. Validates password complexity. |

### 7.3 Authentication

| Method | Path | Auth | Body fields | Notes |
|---|---|---|---|---|
| POST | `/auth/login` | — | `username`, `password` | bcrypt-compares; on success issues access + refresh tokens (see §6); updates `lastLoginAt`. |
| POST | `/auth/refresh` | — | `refreshToken` | Rotates token; reuse-detection per §6.5.4. |
| POST | `/auth/logout` | — | `refreshToken?` | Deletes the matching `RefreshToken` row. Always returns 204. |
| GET | `/auth/me` | Y | — | Returns current user info. |
| PUT | `/auth/password` | Y | `currentPassword`, `newPassword` | Validates complexity; bcrypt-rehashes; revokes all refresh tokens for this user. |

### 7.4 Users (admin-only)

All routes here require `Y+admin`.

| Method | Path | Body fields | Notes |
|---|---|---|---|
| GET | `/users` | — | List users (without `passwordHash`) |
| POST | `/users` | `username`, `password`, `displayName`, `role?` | `role` defaults to `viewer`; rejects with 400 if outside the legal role set |
| PUT | `/users/:userId` | `displayName?`, `role?`, `enabled?`, `password?` | If `password` is provided, validate complexity and bcrypt-rehash |
| DELETE | `/users/:userId` | — | Refuses with 400 if `userId` matches the requesting user |

> **Note:** the reference's "valid roles" list in this route file declares only `admin` and `viewer`. The implementation MUST accept all three normalised roles (`admin`, `moderator`, `viewer`) per §6.1; reject anything else with a 400 listing the valid values.

### 7.5 Server connections

| Method | Path | Auth | Body fields | Notes |
|---|---|---|---|---|
| GET | `/servers` | Y | — | List server configs. **`apiKey` MUST NOT appear in any response.** Each row is augmented with `hasSshCredentials: !!sshUsername`. |
| POST | `/servers` | Y+admin | `name`, `host`, `webqueryPort?`, `apiKey`, `useHttps?`, `sshPort?`, `sshUsername?`, `sshPassword?` | Encrypts `apiKey` and `sshPassword` (if any) at rest; adds the new connection to the pool (§10). |
| GET | `/servers/:configId` | Y | — | Same projection as list, single row. |
| PUT | `/servers/:configId` | Y+admin | Any of: `name`, `host`, `webqueryPort`, `apiKey`, `useHttps`, `sshPort`, `sshUsername`, `sshPassword`, `enabled`, `queryBotChannel`, `queryBotNickname`, `sshBotNickname` | An empty string for `apiKey` or `sshPassword` MUST be treated as "no change" (do not overwrite). All other supplied fields are written; sensitive ones are re-encrypted. After update, refresh the connection-pool entry; if any of the three nickname/channel fields changed, asynchronously apply the change to the live SSH session. |
| DELETE | `/servers/:configId` | Y+admin | — | Cascades to all per-server entities; removes from connection pool. Returns 204. |
| POST | `/servers/:configId/test` | Y+admin | — | Creates a temporary WebQuery client with the decrypted credentials, calls a known-cheap WebQuery probe, returns `{ "success": <bool> }`, then disposes the client. |

### 7.6 Virtual servers

Mounted at `/servers/:configId/virtual-servers`. All routes require `Y+access`. ServerQuery proxy unless noted.

| Method | Path | TS command | Body fields | Notes |
|---|---|---|---|---|
| GET | `` | `serverlist` | — | (no virtual-server scope; uses sid=0) |
| GET | `:sid/info` | `serverinfo` | — | |
| PUT | `:sid` | `serveredit` | whitelisted set (see below) | Y+admin. Filters out any body keys not in the allowed-edit whitelist. |
| POST | `` | `servercreate` | passthrough | Y+admin. (sid=0 context.) Returns 201. |
| POST | `:sid/start` | `serverstart` | — (sid in body) | Y+admin. |
| POST | `:sid/stop` | `serverstop` | — (sid in body) | Y+admin. |
| DELETE | `:sid` | `serverdelete` | — (sid in body) | Y+admin. |
| POST | `:sid/snapshot` | `serversnapshotcreate` | — | Y+admin. |
| POST | `:sid/snapshot/deploy` | `serversnapshotdeploy` | passthrough (POST upload) | Y+admin. |
| GET | `:sid/connection-info` | `serverrequestconnectioninfo` | — | |

**Allowed `serveredit` keys (the implementation MUST drop everything else from the body):** `virtualserver_name`, `virtualserver_welcomemessage`, `virtualserver_maxclients`, `virtualserver_password`, `virtualserver_hostmessage`, `virtualserver_hostmessage_mode`, `virtualserver_default_server_group`, `virtualserver_default_channel_group`, `virtualserver_default_channel_admin_group`, `virtualserver_hostbanner_url`, `virtualserver_hostbanner_gfx_url`, `virtualserver_hostbanner_gfx_interval`, `virtualserver_hostbanner_mode`, `virtualserver_hostbutton_tooltip`, `virtualserver_hostbutton_url`, `virtualserver_hostbutton_gfx_url`, `virtualserver_icon_id`, `virtualserver_codec_encryption_mode`, `virtualserver_needed_identity_security_level`, `virtualserver_min_client_version`, `virtualserver_antiflood_points_tick_reduce`, `virtualserver_antiflood_points_needed_command_block`, `virtualserver_antiflood_points_needed_ip_block`, `virtualserver_log_client`, `virtualserver_log_query`, `virtualserver_log_channel`, `virtualserver_log_permissions`, `virtualserver_log_server`, `virtualserver_log_filetransfer`.

### 7.7 Channels

Mounted at `/servers/:configId/vs/:sid/channels`. Y+access throughout.

| Method | Path | TS command | Body fields | Notes |
|---|---|---|---|---|
| GET | `` | `channellist` | — | The implementation MUST request these flags: `-topic -flags -voice -limits -icon -secondsempty`. |
| GET | `:cid` | `channelinfo` | — | |
| POST | `` | `channelcreate` | passthrough | Y+admin |
| PUT | `:cid` | `channeledit` | passthrough | Y+admin |
| DELETE | `:cid` | `channeldelete` | `?force=0\|1` (default 1) | Y+admin |
| POST | `:cid/move` | `channelmove` | passthrough (`cpid`, `order?`) | Y+admin |
| GET | `:cid/permissions` | `channelpermlist` | `-permsid` | |
| PUT | `:cid/permissions` | `channeladdperm` | passthrough | Y+admin |
| DELETE | `:cid/permissions` | `channeldelperm` | passthrough | Y+admin |

### 7.8 Clients

Mounted at `/servers/:configId/vs/:sid/clients`. Y+access throughout.

| Method | Path | TS command | Notes |
|---|---|---|---|
| GET | `` | `clientlist` | Flags `-uid -away -voice -times -groups -info -country` always. **Only when role is `admin`** the request additionally adds `-ip`. |
| GET | `database` | `clientdblist` | `?start, ?duration` (defaults 0, 100) |
| GET | `database/:cldbid` | `clientdbinfo` | |
| GET | `:clid` | `clientinfo` | |
| POST | `:clid/kick` | `clientkick` | Y+admin. `reasonid` defaults to 5 (server kick); `reasonmsg` passthrough |
| POST | `:clid/ban` | `banclient` | Y+admin. `time` defaults to `0` (permanent); `banreason` passthrough |
| POST | `:clid/move` | `clientmove` | Y+admin. `cid`, `cpw?` |
| POST | `:clid/poke` | `clientpoke` | Y+admin. `msg` |
| POST | `:clid/message` | `sendtextmessage` | Y+admin. Targets `targetmode=1`, `target=:clid`, `msg=req.body.msg` |
| GET | `:cldbid/permissions` | `clientpermlist` | TS error 1281 (`database_empty_result`) MUST be translated to `[]` |
| PUT | `:cldbid/permissions` | `permidgetbyname` then `clientaddperm` | Y+admin. Resolve `permsid` → `permid`, then issue add. |
| DELETE | `:cldbid/permissions` | `permidgetbyname` then `clientdelperm` | Y+admin. |
| GET | `:clid/groups` | `servergroupsbyclientid` | Note the param key is `cldbid` upstream (the reference passes `:clid` here, which is a known minor inconsistency — implementations SHOULD resolve to actual `cldbid`). |

> **Note (M2 in reference):** The IP address (`-ip` flag) is included in `clientlist` only when the requesting user is `admin`. The implementation MUST preserve this guardrail.

### 7.9 Server groups

Mounted at `/servers/:configId/vs/:sid/server-groups`. Y+access throughout.

| Method | Path | TS command | Notes |
|---|---|---|---|
| GET | `` | `servergrouplist` | |
| POST | `` | `servergroupadd` | Y+admin. passthrough |
| PUT | `:sgid` | `servergrouprename` | Y+admin. passthrough |
| DELETE | `:sgid` | `servergroupdel` | Y+admin. passes `force=1` |
| POST | `:sgid/copy` | `servergroupcopy` | Y+admin. `:sgid` becomes `ssgid`; rest passthrough |
| GET | `:sgid/members` | `servergroupclientlist` | Adds flag `-names` |
| POST | `:sgid/members` | `servergroupaddclient` | Y+admin. `cldbid` |
| DELETE | `:sgid/members/:cldbid` | `servergroupdelclient` | Y+admin |
| GET | `:sgid/permissions` | `servergrouppermlist` | Adds `-permsid` |
| PUT | `:sgid/permissions` | `servergroupaddperm` | Y+admin. passthrough |
| DELETE | `:sgid/permissions` | `servergroupdelperm` | Y+admin. passthrough |

### 7.10 Channel groups

Mounted at `/servers/:configId/vs/:sid/channel-groups`. Y+access throughout.

| Method | Path | TS command | Notes |
|---|---|---|---|
| GET | `` | `channelgrouplist` | |
| POST | `` | `channelgroupadd` | Y+admin. passthrough |
| PUT | `:cgid` | `channelgrouprename` | Y+admin. passthrough |
| DELETE | `:cgid` | `channelgroupdel` | Y+admin. force=1 |
| GET | `:cgid/clients` | `channelgroupclientlist` | |
| POST | `:cgid/assign` | `setclientchannelgroup` | Y+admin. passthrough |
| GET | `:cgid/permissions` | `channelgrouppermlist` | -permsid |
| PUT | `:cgid/permissions` | `channelgroupaddperm` | Y+admin |
| DELETE | `:cgid/permissions` | `channelgroupdelperm` | Y+admin |

### 7.11 Permissions

Mounted at `/servers/:configId/vs/:sid/permissions`. Y+access throughout. Read-only.

| Method | Path | TS command | Notes |
|---|---|---|---|
| GET | `` | `permissionlist` | |
| GET | `find` | `permfind` | `?permid` or `?permsid` |
| GET | `overview/:cldbid` | `permoverview` | `?cid=0`, `?permid=0` |

### 7.12 Bans

Mounted at `/servers/:configId/vs/:sid/bans`. Y+access throughout.

| Method | Path | TS command | Notes |
|---|---|---|---|
| GET | `` | `banlist` | |
| POST | `` | `banadd` | Y+admin. Body keys forwarded: `ip`, `uid`, `mytsid`, `name`, `banreason`, `time`. Each is stringified except `time` which is numeric. |
| DELETE | `:banid` | `bandel` | Y+admin |
| DELETE | `` | `bandelall` | Y+admin (note: no `banid` in path) |

### 7.13 Tokens (privilege keys)

Mounted at `/servers/:configId/vs/:sid/tokens`. Y+access throughout.

| Method | Path | TS command | Notes |
|---|---|---|---|
| GET | `` | `privilegekeylist` | |
| POST | `` | `privilegekeyadd` | Y+admin. passthrough |
| DELETE | `:token` | `privilegekeydelete` | Y+admin |

### 7.14 Files (channel file browser)

Mounted at `/servers/:configId/vs/:sid/files`. Y+access throughout. **Special: routes here use SSH, not WebQuery, because TeamSpeak's `ft*` commands are not available over WebQuery HTTP.**

| Method | Path | SSH command | Notes |
|---|---|---|---|
| GET | `:cid` | `ftgetfilelist` | `?cpw`, `?path` (default `/`). TS error 1281 → `[]`. SSH-not-configured → 400 with explanatory message. |
| POST | `:cid/mkdir` | `ftcreatedir` | Y+admin. `dirname` |
| DELETE | `:cid/file` | `ftdeletefile` | Y+admin. `name` |

The implementation MUST execute these commands through the same SSH session used by the event bridge (§11), not a separate session.

### 7.15 Complaints

Mounted at `/servers/:configId/vs/:sid/complaints`. Y+access throughout.

| Method | Path | TS command | Notes |
|---|---|---|---|
| GET | `` | `complainlist` | `?tcldbid` |
| POST | `` | `complainadd` | Y+admin. passthrough |
| DELETE | `:tcldbid/:fcldbid` | `complaindel` | Y+admin |

### 7.16 Messages (offline)

Mounted at `/servers/:configId/vs/:sid/messages`. Y+access throughout.

| Method | Path | TS command | Notes |
|---|---|---|---|
| GET | `` | `messagelist` | |
| GET | `:msgid` | `messageget` | |
| POST | `` | `messageadd` | Y+admin. passthrough |
| DELETE | `:msgid` | `messagedel` | Y+admin |

### 7.17 Server logs

Mounted at `/servers/:configId/vs/:sid/logs`. Y+access throughout.

| Method | Path | TS command | Notes |
|---|---|---|---|
| GET | `` | `logview` | `?lines=100&reverse=1&instance=0&begin_pos?` |

### 7.18 Instance settings

Mounted at `/servers/:configId/instance`. Y+access throughout.

| Method | Path | TS command | Notes |
|---|---|---|---|
| GET | `` | `instanceinfo` | (sid=0) |
| PUT | `` | `instanceedit` | Y+admin. Filters to allowed keys (see below) |
| GET | `host` | `hostinfo` | |
| GET | `version` | `version` | |

**Allowed `instanceedit` keys:** `serverinstance_guest_serverquery_group`, `serverinstance_template_serveradmin_group`, `serverinstance_template_serverdefault_group`, `serverinstance_template_channeladmin_group`, `serverinstance_template_channeldefault_group`, `serverinstance_filetransfer_port`, `serverinstance_serverquery_flood_commands`, `serverinstance_serverquery_flood_time`, `serverinstance_serverquery_ban_time`.

### 7.19 Dashboard

Mounted at `/servers/:configId/vs/:sid/dashboard`. Y+access.

| Method | Path | Notes |
|---|---|---|
| GET | `` | Issues `serverinfo`, `clientlist`, `channellist`, `serverrequestconnectioninfo` in parallel. Aggregates into `DashboardData` per §7.19.1. |

#### 7.19.1 `DashboardData` shape

```json
{
  "serverName": "<virtualserver_name>",
  "platform": "<virtualserver_platform>",
  "version": "<virtualserver_version>",
  "onlineUsers": <count of clientlist entries with client_type == 0>,
  "maxClients": <virtualserver_maxclients>,
  "uptime": <virtualserver_uptime>,
  "channelCount": <channellist length>,
  "bandwidth": {
    "incoming": <connection_bandwidth_received_last_second_total>,
    "outgoing": <connection_bandwidth_sent_last_second_total>
  },
  "packetloss": <virtualserver_total_packetloss_total>,
  "ping": <virtualserver_total_ping>
}
```

ServerQuery clients (`client_type == 1`) MUST be excluded from `onlineUsers`.

### 7.20 Bot flows

Mounted at `/bots`. (Not under `:configId` — but write operations implicitly bind to a `serverConfigId` from the request body.)

| Method | Path | Auth | Body fields | Notes |
|---|---|---|---|---|
| GET | `` | Y | — | List flows; each augmented with `executionCount` (count of `BotExecution` rows). `flowData` is NOT included in list view. |
| GET | `:botId` | Y | — | Returns full flow including `flowData` parsed from JSON string. |
| POST | `` | Y+admin | `name`, `description?`, `serverConfigId`, `virtualServerId?`, `flowData?` | Verifies `serverConfigId` exists; serialises `flowData` to JSON string for storage |
| PUT | `:botId` | Y+admin | any of: `name`, `description`, `flowData`, `serverConfigId`, `virtualServerId` | Calls bot engine's `reloadFlow(botId)` after update |
| DELETE | `:botId` | Y+admin | — | Calls bot engine's `disableFlow(botId)` first; returns 204 |
| POST | `:botId/enable` | Y+admin | — | Sets `enabled=true`; calls engine `enableFlow` |
| POST | `:botId/disable` | Y+admin | — | Sets `enabled=false`; calls engine `disableFlow` |
| GET | `:botId/executions` | Y | — | Last 50 executions, ordered by `startedAt desc` |
| GET | `:botId/executions/:execId/logs` | Y | — | All log entries for an execution, ordered by `timestamp asc` |

#### 7.20.1 Webhook ingress (unauthenticated)

| Method | Path | Auth | Notes |
|---|---|---|---|
| ALL | `/bots/webhook/*<path>` | — (rate-limited at 60/min) | Routed into the bot engine's webhook dispatcher (§13.3). Flow's webhook trigger may require a secret header/query parameter; missing/wrong secret MUST result in 403. |

The path-suffix (after `/api/bots/webhook/`) MUST be passed to the engine intact (multi-segment paths supported).

### 7.21 Music bots

Mounted at `/music-bots`. **All routes require `Y+admin`.**

#### 7.21.1 Bot CRUD

| Method | Path | Body fields | Notes |
|---|---|---|---|
| GET | `` | — | List bots merged with runtime status. `identityData` MUST NOT be included. |
| GET | `:id` | — | Single bot with runtime status, `identityData` redacted |
| POST | `` | `name`, `serverConfigId`, plus optional `nickname`, `serverPassword`, `defaultChannel`, `channelPassword`, `voicePort`, `volume`, `autoStart`, `nowPlayingChannelId` | Delegates to voice-bot manager's `createBot` |
| PUT | `:id` | any of the above (subset) | Persists; if bot is loaded, applies live config-update via the runtime |
| DELETE | `:id` | — | Stops bot and removes config |

#### 7.21.2 Bot lifecycle

| Method | Path | Notes |
|---|---|---|
| POST | `:id/start` | |
| POST | `:id/stop` | |
| POST | `:id/restart` | Errors with 404 if the bot is not loaded |

#### 7.21.3 Playback

| Method | Path | Body | Notes |
|---|---|---|---|
| POST | `:id/play` | `songId` | Requires bot status in `{connected, playing, paused}`. Adds song to queue, plays at end. |
| POST | `:id/play-url` | `url` | Downloads via `yt-dlp`; saves a `MusicRequest` row (upsert by `(serverConfigId, url)`). |
| POST | `:id/play-radio` | `stationId` | Streams radio. Note: `playStream` path, not the queue-based `play`. |
| POST | `:id/pause` | — | |
| POST | `:id/resume` | — | |
| POST | `:id/stop-playback` | — | Stops audio output but does not disconnect the bot. |
| POST | `:id/skip` | — | |
| POST | `:id/previous` | — | |
| POST | `:id/seek` | `seconds` | |
| POST | `:id/volume` | `volume` | Clamped to `[0, 100]`; persisted to DB and applied live. |
| GET | `:id/state` | — | Returns full `PlaybackState` (§22). |

#### 7.21.4 Queue

| Method | Path | Body | Notes |
|---|---|---|---|
| GET | `:id/queue` | — | `{ items, shuffle, repeat }` |
| POST | `:id/queue` | `songId` | Append. Returns new queue length. |
| POST | `:id/queue/playlist` | `playlistId`, `clearFirst?` | Append all songs from the playlist (in order). |
| DELETE | `:id/queue/:index` | — | Remove by index. |
| DELETE | `:id/queue` | — | Clear queue. |
| POST | `:id/queue/shuffle` | `enabled?` | Default `true`. |
| POST | `:id/queue/repeat` | `mode` | One of `off`, `track`, `queue`. |
| POST | `:id/queue/:index/play` | — | Jump-to-index. Streams URL if the item is a stream, otherwise plays as file. |
| PUT | `:id/queue/move` | `from`, `to` | Reorder one item. |

#### 7.21.5 Video stream control

| Method | Path | Body | Notes |
|---|---|---|---|
| POST | `:id/stream/start` | `source`, `preset?`, `framerate?`, `bitrate?` | |
| POST | `:id/stream/stop` | — | |
| POST | `:id/stream/source` | `source` | |
| GET | `:id/stream/status` | — | `VideoStreamStatus` shape |
| DELETE | `:id/stream/viewer/:clid` | — | Kick a viewer |
| POST | `:id/stream/webrtc/offer` | — | Returns SDP offer |
| POST | `:id/stream/webrtc/answer` | `sdp` | |
| POST | `:id/stream/webrtc/ice` | `candidate`, `sdpMid?`, `sdpMLineIndex?` | |
| GET | `:id/player-widget-token` | — | Returns `{ token, jsonUrl, bbcodeUrl }` (deterministic per-bot HMAC token) |

### 7.22 Music library (per server)

Mounted at `/servers/:configId/music-library`. **Y+admin throughout.**

| Method | Path | Body | Notes |
|---|---|---|---|
| GET | `songs` | — | List songs for this server (newest first). |
| POST | `upload` | multipart `file` | Limits: 100 MiB, allowed extensions `.mp3 .wav .flac .ogg .opus .m4a .aac .wma .webm`. Filename randomized to `<UUIDv4><ext>`. Title/artist parsed from `<artist> - <title>.ext` if present. ffprobe extracts duration. |
| DELETE | `songs/:id` | — | Deletes the file from disk (best-effort) and the DB row. |
| POST | `youtube/search` | `query` | Length ≤200; rejects strings containing `--` or `https?://`. Returns up to 10 results. |
| POST | `youtube/download` | `url` | Validates URL is in the YouTube domain set + SSRF check; deduplicates by `(serverConfigId, sourceUrl)`. |
| POST | `youtube/info` | `url` | Returns `YouTubeUrlInfo` (video info or playlist with item list). |
| POST | `youtube/download-batch` | `urls[]` | Up to 50 URLs; processes sequentially; returns per-URL outcome. |

**YouTube domain allowlist:** `youtube.com`, `www.youtube.com`, `youtu.be`, `music.youtube.com`, `m.youtube.com`. Other hosts MUST be rejected with 400.

### 7.23 Music requests

Mounted at `/servers/:configId/music-requests`. Y+access.

| Method | Path | Notes |
|---|---|---|
| GET | `` | Last 100, newest first. |

### 7.24 Playlists

Mounted at `/playlists`. **Y+admin throughout.**

| Method | Path | Body | Notes |
|---|---|---|---|
| GET | `` | `?musicBotId` | List, optionally filtered. |
| GET | `:id` | — | Detail view including songs, ordered by `position asc`. |
| POST | `` | `name`, `musicBotId?` | |
| PUT | `:id` | `name?`, `musicBotId?` | |
| DELETE | `:id` | — | Cascades `PlaylistSong`. |
| POST | `:id/songs` | `songId` | Appends at next free `position` (max+1). |
| DELETE | `:id/songs/:songId` | — | |
| PUT | `:id/songs/reorder` | `songIds[]` | Reassigns `position` to match the array order. Implementation MUST run as a transaction. |

### 7.25 Radio stations

Mounted at `/servers/:configId/radio-stations`. **Y+admin throughout.**

| Method | Path | Body | Notes |
|---|---|---|---|
| GET | `presets` | — | Returns the 16 built-in presets (informational). |
| GET | `` | — | List custom stations for this server, name-sorted. |
| POST | `` | `name`, `url`, `genre?`, `imageUrl?` | URL passes SSRF validation. |
| DELETE | `:id` | — | |

### 7.26 Settings (admin)

Mounted at `/settings`. **Admin-only.**

| Method | Path | Body | Notes |
|---|---|---|---|
| GET | `yt-cookies` | — | Reports `{ active, exists, size, path }` |
| POST | `yt-cookies` | multipart `cookies` OR JSON `text` | 5 MiB limit; validates Netscape header pattern (case-insensitive: `# [Netscape ]HTTP Cookie File` allowing leading whitespace and BOM); writes to `<data-dir>/yt-cookies.txt` and activates it. |
| DELETE | `yt-cookies` | — | Removes the file and deactivates. |

### 7.27 Widgets (admin)

Mounted at `/widgets`. Y+admin for write; Y for list (any role).

| Method | Path | Body | Notes |
|---|---|---|---|
| GET | `` | — | List widgets with their server config |
| POST | `` | `name`, `serverConfigId`, plus optional `virtualServerId`, `theme`, `showChannelTree`, `showClients`, `hideEmptyChannels`, `maxChannelDepth` | Y+admin. Generates 21-char nanoid token. |
| PATCH | `:id` | any subset of `name`, `theme`, `showChannelTree`, `showClients`, `hideEmptyChannels`, `maxChannelDepth` | Y+admin. Invalidates the public-widget cache for this widget's token. |
| DELETE | `:id` | — | Y+admin. Cache-invalidates. |
| POST | `:id/regenerate-token` | — | Y+admin. New nanoid token; cache invalidate. |

### 7.28 Public widgets (no auth)

Mounted at `/widget`. **No authentication.** Cached for 45 s; CORS allow-origin `*`.

| Method | Path | Notes |
|---|---|---|
| GET | `:token/data` | JSON `WidgetData`. 404 if not found or upstream offline. |
| GET | `:token/image.svg` | SVG render of `WidgetData`. |
| GET | `:token/image.png` | PNG render via SVG-to-PNG; falls back to SVG if rasterizer unavailable. |
| GET | `player/:botId/data` | JSON now-playing + upcoming queue. Requires `?token=` matching deterministic per-bot HMAC (§7.28.1). |
| GET | `player/:botId/bbcode` | Plain-text BBCode for embedding in TeamSpeak channel descriptions. Same token check. |

> **Note on Glossary §3.9:** the original ToC documented the public widget routes under `/widget/<token>` and `/widget/<token>.svg` etc. The actual routes are under `/api/widget/<token>/...` as in this section. The implementation MUST match the actual paths. `Glossary §3.9 should be read as updated by §7.28.`

#### 7.28.1 Deterministic player-widget tokens

For a music bot of id `B`, the player-widget token is:

```
HMAC-SHA-256(JWT_SECRET, "player-widget:" + B)   → take first 16 hex characters
```

Returning operators an unguessable but reproducible token without storing one. The token SHOULD be regarded as a low-sensitivity capability (it grants only read of bot now-playing state). Rotating `JWT_SECRET` will invalidate all existing player-widget URLs; this is acceptable.

### 7.29 Public widget data caching

The public widget data routes (§7.28) MUST cache the resolved `WidgetData` for **45 seconds** keyed by widget token. Cache eviction:

- On widget mutation (`PATCH`, `DELETE`, `regenerate-token`) the entry MUST be invalidated;
- Once per minute the cache MUST be swept of expired entries;
- The cache MUST be size-bounded (recommended ≤1000 entries) with FIFO eviction on overflow.

`Cache-Control: public, max-age=45` SHOULD be emitted on the response. The `WidgetData` payload MUST redact the upstream `virtualserver_platform` and `virtualserver_version` fields (replace with the constant strings `"TeamSpeak"` and `""` respectively) to avoid handing version-targeting information to attackers.

### 7.30 Total route count and Glossary cross-check

The implementation MUST expose, at minimum, the routes catalogued above. Implementations MAY add additional routes for diagnostic purposes (e.g., `/api/__version`, prometheus endpoints) but MUST NOT use any of the path prefixes reserved by Chapter 3 for unrelated purposes.

### 7.31 How to verify

- A round-trip per route group: for every group above, exercise at least one read and one write (where applicable) and verify the upstream TS server reflects the change.
- An RBAC sweep: as a `viewer`, attempt every `Y+admin` route in the catalogue; each MUST return 403.
- A `:configId` access sweep: as a `moderator` without `UserServerAccess` for some `configId`, attempt every `:configId` route; each MUST return 403.
- A path-traversal attempt on music uploads: rename a file's `originalname` to include `..`; verify the implementation does not write outside `MUSIC_DIR`. (The reference relies on a UUID-based final filename, which makes traversal moot, but this should be verified explicitly.)
- A YouTube-search injection attempt: submit `query="--cookies /etc/passwd"`; verify the request is rejected with 400.
- A widget-cache-invalidation test: load `/api/widget/<token>/data`, then `PATCH /api/widgets/<id>` to change the theme, then immediately re-fetch the public route; the response MUST reflect the new theme (no stale cache).

---

## Chapter 8. WebSocket (Realtime)

### 8.1 Endpoint

The back-end MUST expose a WebSocket at path `/ws` on the same host:port as the REST API. The handshake URL accepts an access-token query parameter:

```
ws://<host>:<port>/ws?token=<access-token>
```

### 8.2 Authentication

Per §6.11, the handshake MUST validate the JWT, look up the user, and reject (HTTP 401 close-frame status) if missing/invalid/disabled. Connections that pass MUST be associated server-side with a user id for subsequent authorization decisions.

### 8.3 Message envelope

Every message sent **from server to client** MUST be a JSON object of shape:

```json
{
  "type": "<event-name>",
  "data": <payload>
}
```

There is no client-to-server message protocol; the WebSocket is a **push channel**. The implementation MUST disconnect a client that sends arbitrary frames (this is a hardening measure; the reference does not strictly enforce it but doing so is safe).

### 8.4 Event categories

The implementation MUST emit at least the following event types. Each subsystem chapter pinpoints the exact emit moments; this section is the cross-reference.

| `type` | Fan-out scope | When emitted | Payload outline |
|---|---|---|---|
| `bot:execution:start` | per-flow scope | A flow execution begins | `{ flowId, executionId, triggeredBy, startedAt }` |
| `bot:execution:end` | per-flow | A flow execution ends | `{ flowId, executionId, status, endedAt, error? }` |
| `bot:log` | per-flow | A flow node emits a log entry | `BotLogEntry` |
| `bot:flow:state` | global (admin) | Flow enable/disable | `{ flowId, enabled }` |
| `voice:status` | per-bot | A voice bot's status changes (connected/playing/paused/error/...) | `{ botId, status, error? }` |
| `voice:nowPlaying` | per-bot | Current track changes | `{ botId, item, position?, duration? }` |
| `voice:progress` | per-bot | Periodic progress tick (recommended every 1 s while playing) | `{ botId, position, duration }` |
| `voice:queue` | per-bot | Queue mutations | `{ botId, items, currentIndex, shuffle, repeat }` |
| `video:status` | per-bot | Video stream state changes | `VideoStreamStatus` |
| `ts:event` | per-server | A raw or synthetic TeamSpeak event arrives | `{ serverConfigId, eventName, fields }` |

Events MUST be filtered server-side by the recipient's role and per-server access. A `viewer` without access to `serverConfigId=N` MUST NOT receive any event tagged with `serverConfigId=N`.

### 8.5 Reconnection

The browser is expected to reconnect with exponential backoff. The server MUST NOT keep client-specific state across reconnections; each reconnection re-authenticates and re-subscribes (subscription is implicit — the server pushes everything the user is authorized to see).

### 8.6 How to verify

- Authenticate via `/api/auth/login`, open `/ws?token=<access>`, then trigger a flow with a chat-command trigger; the client MUST receive `bot:execution:start`, one or more `bot:log`, and `bot:execution:end`.
- Disable a user, force a reconnection; the new connection MUST be rejected.
- Connect as a `viewer` without access to `configId=2`, then have an admin trigger a flow on `configId=2`; the viewer MUST NOT receive any related events.

---

## Chapter 9. Outbound HTTP Safety (SSRF)

The detailed validator semantics are specified in §6.7. This chapter cross-references where the validator is invoked across the route surface, and adds two non-route invocation sites.

### 9.1 Invocation matrix

| Caller | URL source | Validator invoked? | Notes |
|---|---|---|---|
| `POST /servers/.../radio-stations` | Operator-supplied | Yes (`http`, `https`) | §7.25 |
| `POST /servers/.../music-library/youtube/{download,info,download-batch}` | Operator-supplied | Yes, after YouTube-domain allowlist check | §7.22 |
| Bot flow `webhook` action | Operator-stored URL | Yes | §14.2 |
| Bot flow `httpRequest` action | Operator-stored URL | Yes | §14.2 |
| Music bot `playStream` (radio) | URL stored on `RadioStation` | Yes (re-checked at play time) | §20 |
| Music bot YouTube playback (yt-dlp input) | URL on `MusicRequest` or runtime arg | Yes | §20 |
| Sidecar `POST /source` (back-end caller) | URL forwarded from operator | Yes (validated by back-end before forwarding) | §24 |
| Bot flow inbound webhook trigger payload | The *inbound* request itself | N/A — validation applies only outbound | §13.3 |

### 9.2 Two non-route invocation sites

- **Music download URL re-validation.** A song's `sourceUrl` was validated once on insert. Each play SHOULD re-validate, because DNS records can change after the original insert. (Cost: one DNS lookup per play. Acceptable.)
- **Periodic radio-station health probe (optional).** If the implementation adds a health-probe job, every probed URL MUST go through the validator.

### 9.3 Failure mode rationale

The validator's rule of "DNS failure → allow request" is deliberate: an SSRF attack model assumes the attacker controls DNS to resolve to a private IP. If DNS *fails* (NXDOMAIN), there is no resolution-to-private-IP risk. Letting the underlying HTTP/FFmpeg call fail naturally produces a more actionable error message than a synthetic "DNS failed" rejection. Implementations MUST keep this behaviour; do not "harden" it by failing closed on DNS errors.

### 9.4 How to verify

A compact SSRF test suite the implementation MUST pass (see also §6.13):

| Input URL | Expected result |
|---|---|
| `http://127.0.0.1` | 400 |
| `http://localhost/` | 400 |
| `http://[::1]` | 400 |
| `http://10.1.2.3/` | 400 |
| `http://192.168.1.1` | 400 |
| `http://169.254.169.254/latest/meta-data/` | 400 |
| `http://metadata.google.internal/` | 400 |
| `gopher://example.com/` | 400 |
| `https://example.com/` (resolves to a public IP) | Allowed (request issued) |
| `http://rebinder.example/` (resolves to `10.1.2.3`) | 400 |
| `http://nxdomain.example/` (DNS NXDOMAIN) | Allowed; downstream returns DNS error |

---

# Part IV — TeamSpeak Integration

## Chapter 10. WebQuery HTTP Client

The system reaches every managed TeamSpeak server through that server's WebQuery HTTP API. WebQuery is the modern TS6 management interface; commands are issued as HTTP requests and responses are JSON. This chapter is normative for the back-end's outbound side of that interaction.

### 10.1 Per-server client lifetime

For every enabled `TsServerConfig` row, the back-end MUST maintain exactly **one** persistent HTTP client (keep-alive enabled, maximum sockets = 1). This single-socket constraint is load-bearing for correctness: the TS WebQuery server registers each TCP connection as an independent ServerQuery login (`serveradmin`, `serveradmin1`, `serveradmin2`, …). Allowing multiple concurrent sockets per server pollutes the TS clientlist with phantom query users, exhausts the upstream's per-IP login limit, and disrupts the dashboard's client counts. The implementation MUST NOT use a multi-socket pool.

Acceptable techniques to satisfy the single-socket-per-server invariant:

- A keep-alive HTTP/1.1 connection pool with `max_idle_connections_per_host = 1` and serialised request dispatch.
- An async mutex around the request-issuing path, so concurrent callers queue.

Either approach yields the same observable behaviour upstream: one query session per server.

### 10.2 Client construction

A WebQuery client is parameterised by:

| Parameter | Source |
|---|---|
| Host | `TsServerConfig.host` |
| Port | `TsServerConfig.webqueryPort` (default `10080`) |
| Scheme | `TsServerConfig.useHttps ? "https" : "http"` |
| API key | decrypted `TsServerConfig.apiKey` |
| TLS verification | `!TS_ALLOW_SELF_SIGNED` (i.e., self-signed certs accepted iff env flag is `true`/`1`) |
| Request timeout | 15 seconds |

The API key MUST be sent on every request as the HTTP header `x-api-key: <key>`. It MUST NOT be placed in the URL or in a query parameter (avoid logging exposure).

### 10.3 URL shape and method choice

WebQuery commands are addressed by URL path:

```
/<sid>/<command>          — virtual-server-scoped (sid > 0)
/<command>                — instance-scoped (sid == 0)
```

Method:

- **`GET`** for the vast majority of commands. Command parameters are passed as URL query parameters (`?key=value&...`).
- **`POST`** only for commands that legitimately carry uploads (e.g., `serversnapshotdeploy`) and for the small set of commands the reference dispatches via POST (e.g., `clientaddperm`, `clientdelperm`). The implementation MUST issue POST for these specific commands; payload is empty body, parameters in the query string.

`undefined` and `null` parameters MUST be stripped before serialising; absent keys are different from empty-string keys (see §10.4).

### 10.4 Parameter encoding

WebQuery uses the same parameter encoding as the legacy ServerQuery protocol — a space-separated `key=value` string with the following character escapes (applied to *values* only, not keys):

| Raw | Escape |
|---|---|
| `\` | `\\` |
| `/` | `\/` |
| ` ` (space) | `\s` |
| `\|` | `\p` |
| LF (`\n`) | `\n` |
| CR (`\r`) | `\r` |
| TAB (`\t`) | `\t` |
| BS (`\b`) | `\b` |
| FF (`\f`) | `\f` |

The implementation MUST apply these escapes whenever a value is sent through the SSH bridge (raw ServerQuery wire format). When a value is sent through the WebQuery HTTP API, the underlying HTTP query-string encoder will URL-encode it; *additional* ServerQuery escaping is the WebQuery server's responsibility on its side. The implementation MUST NOT double-escape (do not ServerQuery-escape values that are about to be URL-encoded).

The reverse (unescaping) MUST be applied when parsing raw ServerQuery responses (over SSH).

### 10.5 Response shape

Every successful WebQuery response is JSON of the form:

```json
{
  "body": <command-specific-payload>,
  "status": { "code": 0, "message": "ok" }
}
```

The implementation MUST:

1. Read `status.code`. If it is non-zero, raise an upstream-error with that code and message (mapped to HTTP `502` per §7.0.2).
2. Otherwise, return `body` to the caller (the route handler), which serialises it as the JSON HTTP response body.

If the response is not JSON or has no `status`, treat as an upstream connection failure and raise upstream-error with code `-1` and the underlying error message.

### 10.6 Known TeamSpeak error codes the implementation must distinguish

| TS code | Meaning | Implementation behaviour |
|---|---|---|
| `0` | Success | Return `body` |
| `516` | "already registered" (event-registration only) | The implementation MUST treat as benign in `servernotifyregister` calls (§11.3) |
| `1281` | `database_empty_result` | Specific routes translate this to `[]` (e.g., `clientpermlist` → empty permissions, `ftgetfilelist` → empty directory) |
| any other non-zero | Upstream error | Propagate as `502 {error: "TeamSpeak API Error", code, details}` |

### 10.7 Connection pool

The back-end MUST maintain a **connection pool**: a map from `TsServerConfig.id` to its WebQuery client. The pool's responsibilities:

- **Initialisation on boot:** load every enabled `TsServerConfig`, decrypt each `apiKey`, instantiate a client. Boot completes only after pool initialisation (the listener on `:3001` is started afterwards, so route handlers always see a populated pool).
- **`getClient(configId)`** returns the client or raises a synchronous error `"No connection configured for server config ID <id>"`. Routes MUST treat this as the canonical 500-class error if reached.
- **`addClient` / `removeClient`** are called on `POST /servers` / `DELETE /servers/:configId`.
- **`refreshClient(configId)`** is called on `PUT /servers/:configId`. It re-reads the row, decrypts credentials, and replaces the entry. If the row's `enabled=false`, removes the entry instead.
- **Health check:** every 30 seconds, the pool MUST issue a cheap probe against each client (`version` against `sid=0`) and, on failure, call `refreshClient` for that id. Failure here is an information signal (the upstream is down) — the pool MUST NOT delete the entry on probe failure unless a `refreshClient` reveals the row is no longer enabled.
- **`destroy`** stops health checks and disposes every client (closes keep-alive sockets). Called on graceful shutdown.

### 10.8 Temporary clients

The "test connection" route (§7.5) MUST construct a *temporary* WebQuery client (one socket), call the cheap `version` probe, then immediately destroy it. This avoids polluting the pool with a connection to a server that may not yet be saved.

### 10.9 How to verify

- Bring up two managed TS servers; load the back-end; observe upstream's clientlist on each. Each server MUST register exactly one query client (one `serveradmin`-class slot).
- Take one upstream offline; within 30 seconds of the next health-check tick, the pool's reconnect logic MUST be triggered. Bring it back online; the next health check MUST succeed and the dashboard MUST resume populating.
- Issue 50 simultaneous requests through the back-end against one server; observe upstream — the connection count MUST remain at exactly 1.
- Update a server's API key; without restarting the back-end, the next request to that server MUST use the new key.

---

## Chapter 11. SSH Event Bridge

WebQuery HTTP has no push channel. Event subscription requires a stateful TCP session, which TeamSpeak exposes only over its SSH ServerQuery interface. The system therefore maintains an SSH-based **event bridge** on each server in addition to the WebQuery client.

### 11.1 What the bridge is for

Three uses, in order of importance:

1. **Event subscription.** Receive TeamSpeak `notify*` events (§3.5) so the bot flow engine can react to them.
2. **`ft*` commands** (file transfer / file browser). These commands are not implemented by the WebQuery HTTP API; the file-browser routes (§7.14) MUST execute them through the SSH bridge.
3. **Live mutation of the query session.** Routes that modify the SSH bot's nickname or current channel (`PUT /servers/:configId` with `queryBotNickname`, `sshBotNickname`, `queryBotChannel`) MUST issue `clientupdate` and `clientmove` commands through the existing SSH session — not by tearing it down and reconnecting.

### 11.2 Connection model

For each `(configId, sid)` pair on which an event-aware feature is needed, the bridge maintains exactly one **base** SSH session. Additionally, when a flow defines a `command` trigger scoped to a specific channel (§13.4), the bridge MUST open a separate **command-listener** SSH session per `(configId, sid, channelId)`. Command-listener sessions exist solely to receive `notifytextmessage` events from one channel.

The reason for separate command-listener sessions is that TeamSpeak's `textchannel` event subscription is stateful per session — the listener client must be a member of the channel whose messages it receives. A single SSH session cannot be a member of multiple channels simultaneously, so multiple command listeners require multiple sessions.

Connection identification key: `<configId>:<sid>` for base; `<configId>:<sid>:cmd:<channelId>` for command listeners.

### 11.3 Bring-up sequence

For each base connection:

1. Open SSH connection: host, SSH port (default `10022`), username, decrypted password. SSH-level keepalives: 30 s interval, disconnect after 3 missed (~90 s).
2. Open a shell channel.
3. Wait for the TeamSpeak SSH banner. The banner consists of (in order, as best-effort match):
   - A line equal to `TS3` or containing `TS3 Client`.
   - A line beginning with `Welcome` (the implementation MUST mark "banner received" at this point and start accepting commands).
   - Optional trailing lines: `virtualserver_status=...`, `error id=0 msg=ok`. These MUST be consumed silently if seen.
4. Issue the following commands in order (each waits for `error id=0 msg=ok` before the next begins):
   - `use sid=<sid>`.
   - `clientupdate client_nickname=<escaped-nickname>` where the nickname is `serverConfig.queryBotNickname` (or the default `TS6 Query` if unset). Use the escape rules in §10.4.
   - For each of the five event types `server`, `channel`, `textserver`, `textchannel`, `textprivate`:
     - Issue `servernotifyregister event=<type>` (for `channel`, append ` id=0`).
     - On TS error code `516` ("already registered"), continue silently.
     - On any other error, log a warning but continue.
   - If `serverConfig.queryBotChannel` is set:
     - Issue `whoami` and parse the response to extract this session's `clid` (try keys `clid`, `client_id`, `clientid`, `clientId`; fall back to a regex against the raw line).
     - Issue `clientmove clid=<clid> cid=<channelId>`.
     - On error, log a warning; do not fail bring-up.

For each command-listener connection (channel-scoped chat-command trigger):

1. Steps 1–3 as for base.
2. `use sid=<sid>`.
3. `clientupdate client_nickname=TS6-WebUI-Cmd-<channelId>-<r>` where `<r>` is a stable per-process random suffix (3 random bytes hex-encoded). If the upstream rejects the nickname (collision), retry once with `<r>-<extra>` where `<extra>` is two more random bytes.
4. `whoami` → `clientmove` into `channelId`.
5. `servernotifyregister event=textchannel id=<channelId>`. On error 516, ignore.

### 11.4 Wire-line protocol parsing

The SSH session yields a line-oriented stream of CR-LF-terminated frames. The implementation MUST:

- Buffer incoming bytes; split on `/\r?\n/`; retain the trailing partial line in the buffer for the next data event.
- Discard empty lines.
- Lines that begin with `notify` are **events**. Extract the event name (everything up to the first space) and parse the remainder as a pipe-separated list of records, each record being a space-separated list of `key=value` pairs (with values unescaped per §10.4). Emit one `(eventName, record)` upward per record.
- Lines that begin with `error ` (note the trailing space) **terminate the in-flight command**. Parse `id` and `msg` from the rest of the line. If `id == 0`, resolve the command's promise with the accumulated response lines joined by `\n`. Otherwise, reject with `Error("TS error " + id + ": " + msg)`.
- Any other non-empty line, while a command is in flight, is appended to that command's accumulated response.

Commands are issued one at a time; while one is in flight, additional commands queue. The queue MUST be drained in submission order. Per-command default timeout is 10 seconds.

### 11.5 Keepalive and reconnect

The bridge MUST maintain an application-level keepalive (above the SSH transport keepalive): every 30 seconds, issue `whoami` with a 5-second timeout. After 3 consecutive failures, force-disconnect the session. Force-disconnect MUST schedule a reconnect.

Reconnect uses exponential backoff: `delay = min(1000 * 2^attempts, 30000)` ms. After a successful connection, attempts is reset to 0.

If the underlying SSH error message contains the words `authentication` or `Auth`, the bridge MUST treat the failure as **fatal**: do not reconnect. Operator must fix credentials and the bridge will be re-created when the row is updated. This is to avoid lockout of an account whose password was rotated.

### 11.6 Outbound interface

The bridge exposes the following operations to the rest of the back-end:

| Operation | Behaviour |
|---|---|
| `connectServer(configId, sid)` | Idempotent; opens base session if not present and registers events. Does nothing if the row has no SSH credentials (`sshUsername`, `sshPassword`, `sshPort`). |
| `disconnectServer(configId, sid)` | Closes the session and removes from map. |
| `isConnected(configId, sid)` | Boolean |
| `executeCommand(configId, sid, rawCommand)` | Lazy-connect on first use; runs a raw ServerQuery command on the base session and returns the joined response string. Throws `Error("SSH not connected — check SSH credentials in server settings")` if the connection cannot be established. Used by the file-browser routes (§7.14). |
| `renameSshBot(configId, sid, nickname)` | Issues `clientupdate client_nickname=<escaped>` on the base session. No-op if not connected. |
| `moveSshBot(configId, sid, channelId)` | `whoami` + `clientmove`. No-op if not connected. |
| `connectCommandListener(configId, sid, channelId)` | Idempotent; opens a command-listener session. |
| `disconnectCommandListener(configId, sid, channelId)` | Closes a command-listener session. |
| `getCommandListenerChannelIds(configId, sid)` | Lists active command-listener channelIds for the (configId, sid). |

### 11.7 Event fan-out shape

The bridge emits **upward** one of the following events:

- `tsEvent(configId, sid, eventName, fields)` — an event from a base session OR a command-listener session.
- `sshConnected(configId, sid)`, `sshDisconnected(configId, sid)`, `sshError(configId, sid, err)` — connection-state signals.

Command-listener-originated events MUST be marked so consumers can distinguish them: the `fields` record receives an extra key `__cmd_listener_channel_id` whose string value equals the channel id the listener was scoped to. The flow engine uses this key to route the event only to flows whose chat-command trigger is scoped to that channel (§13.4).

### 11.8 Synthetic events

Many useful "events" are not delivered by TeamSpeak as discrete frames; they must be **derived** by comparing successive `notifyclientupdated` payloads (or, for some, comparing snapshots of `clientlist`). The bridge or the layer immediately above the bridge MUST produce the following synthetic events (§3.5 lists the legal names):

| Synthetic event | Derivation |
|---|---|
| `client_went_away` / `client_came_back` | toggle of `client_away` |
| `client_mic_muted` / `client_mic_unmuted` | toggle of `client_input_muted` |
| `client_sound_muted` / `client_sound_unmuted` | toggle of `client_output_muted` |
| `client_mic_disabled` / `client_mic_enabled` | toggle of `client_input_hardware` |
| `client_sound_disabled` / `client_sound_enabled` | toggle of `client_output_hardware` |
| `client_recording_started` / `client_recording_stopped` | toggle of `client_is_recording` |
| `client_nickname_changed` | change of `client_nickname` |
| `client_group_added` / `client_group_removed` | membership delta of `client_servergroups` (which is a comma-separated list) |

Each synthetic event MUST be emitted with a payload that includes the same client-identification fields the underlying `notifyclientupdated` event carried (`clid`, `client_unique_identifier` if present), plus the `from`/`to` value of whichever field flipped.

### 11.9 How to verify

- Open a real TeamSpeak voice client connected to a managed server; toggle mute; the bridge MUST emit `client_mic_muted` followed by `client_mic_unmuted` with matching `clid`.
- Disconnect the SSH transport (drop the network); within ~90 seconds, the bridge MUST detect the failure (keepalive) and start reconnect attempts. Restore the network; the bridge MUST reconnect and re-register events.
- Define a flow with a `command` trigger scoped to channel `42`; observe that a command-listener SSH session opens, registered ONLY for `textchannel id=42`. Send a chat message in channel `42`; the bridge MUST emit `tsEvent` with `__cmd_listener_channel_id="42"`. Send a chat message in another channel; nothing related MUST be emitted on this listener.
- Set `queryBotChannel` to a private channel via `PUT /servers/:configId`; the bridge's `moveSshBot` MUST move the live SSH client into that channel without dropping the SSH session.
- Provide deliberately wrong SSH credentials; the bridge MUST log a fatal-auth error and MUST NOT enter a reconnect loop.

---

# Part V — Bot Flow Engine

The Bot Flow Engine is a programmable runtime that lets operators define **automation flows** — graphs of triggers, conditions, and actions — and execute them in response to TeamSpeak events, schedules, webhooks, and chat commands.

## Chapter 12. Flow Document Model

### 12.1 Document shape

A flow is persisted as a single JSON document in `BotFlow.flowData`. The document has exactly two top-level fields:

```json
{
  "nodes": [ ... ],
  "edges": [ ... ]
}
```

#### Node shape

```json
{
  "id": "<string-uuid-or-nanoid>",
  "type": "trigger" | "action" | "condition" | "delay" | "variable" | "log",
  "position": { "x": <number>, "y": <number> },
  "data": <NodeData>
}
```

`position` is for the visual editor only and has no runtime semantics; the engine MUST NOT use position fields.

#### Edge shape

```json
{
  "id": "<string>",
  "source": "<source-node-id>",
  "target": "<target-node-id>",
  "sourceHandle": "true" | "false" | undefined,
  "label": "<optional>"
}
```

`sourceHandle` is meaningful only for `condition` nodes, which expose two named output handles `"true"` and `"false"`. Other node types have an unnamed (or `"out"`) default handle; the engine MUST traverse all outgoing edges from non-condition nodes regardless of handle.

### 12.2 Node-data discriminators

The runtime distinguishes node kinds by combining the top-level `type` with one of three discriminator fields inside `data`:

| `type` | Discriminator field in `data` | Notes |
|---|---|---|
| `trigger` | `triggerType: "event" \| "cron" \| "webhook" \| "command"` | |
| `action` | `actionType: <one of 25+ tags>` | See Chapter 14 |
| `condition` | `nodeType: "condition"` | |
| `delay` | `nodeType: "delay"` | |
| `variable` | `nodeType: "variable"` | |
| `log` | `nodeType: "log"` | |

All `data` objects also carry a free-text `label` for the editor; the runtime treats it as opaque metadata (used only for log-line annotation).

### 12.3 Backwards compatibility / editor-format normalisation

The reference's visual editor produces a slightly different on-disk shape (with `n.type === "trigger_event"` etc., and `n.config` instead of `n.data`). The engine MUST tolerate both shapes:

- If `n.data` contains a discriminator (`triggerType`, `actionType`, or `nodeType`), treat the node as already in **engine format**.
- Otherwise, normalise from **editor format** to engine format using the per-tag mapping below.

Editor `n.type` to engine `(type, data.discriminator)` mapping (non-exhaustive):

| Editor `n.type` | Engine `type` | Engine discriminator |
|---|---|---|
| `trigger_event` | `trigger` | `triggerType: "event"` |
| `trigger_cron` | `trigger` | `triggerType: "cron"` (cron expr from `config.cron` or `config.cronExpression`) |
| `trigger_webhook` | `trigger` | `triggerType: "webhook"` (path from `config.path` or `config.webhookPath`) |
| `trigger_command` | `trigger` | `triggerType: "command"` (auto-strips leading `!` from `config.command`; default prefix `!`) |
| `action_kick` | `action` | `actionType: "kick"` (default `reasonId=5`) |
| `action_ban`, `action_move`, `action_message`, … | `action` | matching `actionType` |
| `action_animatedChannel` | `action` | `actionType: "animatedChannel"` (default style `scroll`, default interval `3`s, default prefix `[cspacer]`) |
| `action_voicePlay`, … | `action` | voice family (Chapter 14.6) |
| `condition` | `condition` | `nodeType: "condition"` |
| `delay` | `delay` | `nodeType: "delay"` (delay from `config.delay` or `config.delayMs`; default 1000ms) |
| `variable` | `variable` | `nodeType: "variable"` |
| `log` | `log` | `nodeType: "log"` |

Editor edges with `sourcePort` MUST be normalised into engine edges with `sourceHandle = sourcePort`.

The editor's `targetMode` for `action_message` is given as a string (`"client"`, `"channel"`, `"server"`); the engine internalises as `1`, `2`, `3` respectively.

The implementation MUST preserve this "accept both formats" tolerance because operators upgrading from the editor's format will have flows persisted in the older shape.

### 12.4 Stored type-tag stability

The `triggerType`, `actionType`, and `nodeType` string literals are stored as-is in the database. They form an external contract between flow documents and the engine: renaming any tag silently breaks operator data. §3.7 lists the canonical set of tag values; the implementation MUST preserve them verbatim.

### 12.5 How to verify

- Save a flow with one trigger and one action via `PUT /bots/:id`; read it back; confirm a successful round-trip with no field renames.
- Migrate a flow that uses editor-format JSON into the database directly; load through the engine; confirm normalisation produces a working flow.
- Submit a flow with an unknown `actionType` tag; the engine MUST log a warning and continue (the unknown action is a no-op), not crash.

---

## Chapter 13. Triggers

### 13.1 Event triggers

```json
{
  "triggerType": "event",
  "label": "<...>",
  "eventName": "<one of §3.5>",
  "filters": { "<event-field>": "<expected-value>", ... }
}
```

**Behaviour:** when the event bridge emits an event whose **direct or synthetic** name (per the expansion in §13.1.1) matches `eventName`, and whose payload satisfies *all* `filters` (every listed key MUST equal the expected value as a plain string), the engine starts a flow execution rooted at the trigger node.

#### 13.1.1 Synthetic-event expansion

When the bridge emits a `notifyclientupdated` event (or, in the polled-synthesis path, a derived snapshot of one), the engine MUST expand it into a list of names: the literal `notifyclientupdated` plus every synthetic name (§3.5) whose triggering field equals the matching value in the payload.

When expansion is driven by a poll cycle that knows *which* fields actually changed in the cycle (not just which fields currently equal a target value), the expansion MUST further restrict synthetic emission to only those changed fields. This prevents spurious re-fires (e.g., emitting `client_mic_muted` every poll while a client is muted, instead of only when they transition into the muted state).

A trigger whose `eventName` is `notifyclientupdated` matches every expansion; one whose `eventName` is one of the synthetic names matches only that derivation.

#### 13.1.2 Filters

Filters are exact equality on the (string) event-payload field. The implementation MUST coerce both sides to strings before comparing. `filters` is optional; an absent or empty filter object means "match any payload".

### 13.2 Cron triggers

```json
{
  "triggerType": "cron",
  "label": "<...>",
  "cronExpression": "<crontab-style>",
  "timezone": "<IANA tz, default UTC>"
}
```

**Behaviour:** when the engine loads or enables the flow, it MUST register a cron schedule. On every tick, the engine MUST start a new execution. Cron expressions are validated at registration; an invalid expression MUST be logged at error level and skipped (the trigger does not fire).

The implementation MUST support the conventional 5-field crontab syntax (`min hour dom mon dow`); SHOULD also support an optional 6-field form with seconds. Timezones MUST be honoured (default `UTC`).

### 13.3 Webhook triggers

```json
{
  "triggerType": "webhook",
  "label": "<...>",
  "webhookPath": "<arbitrary path component(s)>",
  "method": "GET" | "POST" | "ANY",
  "secret": "<optional>"
}
```

**Routing:** the engine builds a registry of `(flowId, nodeId, path, method, secret)` tuples on flow enable, removes them on disable. Inbound requests at `/api/bots/webhook/<remainder>` (rate-limited per §6.8) match an entry when:

- `path` equals the URL path remainder (after `/api/bots/webhook/`); AND
- `method` equals the request method (or `ANY` matches everything); AND
- if `secret` is set, the request supplies a matching value, **constant-time-compared**, in either:
  - the `x-webhook-secret` header, or
  - the `?secret=` query string.

If no matching entry triggers, the route MUST respond with `404 {"error":"Not found"}`. If at least one fires, response is `{ "triggered": <count> }`.

#### 13.3.1 Timing oracle mitigation

The matching loop MUST run uniformly regardless of whether the path matched or only the secret was wrong. In particular: do NOT short-circuit the loop on path mismatch in a way that returns a different response shape for "wrong secret" vs "no such path". The reference returns the same `404 {"error":"Not found"}` for both, so an attacker cannot probe path-existence without knowing the secret. Implementations MUST preserve this property.

#### 13.3.2 Payload exposed to the flow

The trigger executes the flow with the following pseudo-event-data record:

| Key | Value |
|---|---|
| `webhook_path` | The matched path |
| `webhook_method` | The HTTP method |
| `webhook_body` | JSON-encoded request body (`{}` if empty) |
| `webhook_query` | JSON-encoded query string parameters |

Inside flow templates, nested access is supported via dot syntax: `{{event.webhook_body.fieldName}}` parses `webhook_body` as JSON and walks the path.

### 13.4 Command triggers

```json
{
  "triggerType": "command",
  "label": "<...>",
  "commandPrefix": "!",
  "commandName": "<word>",
  "channelId": "<optional channel id>"
}
```

**Behaviour:** the trigger fires on `notifytextmessage` events (i.e., when a real client sends a chat message). The full match string is `<commandPrefix><commandName>` (case-sensitive). The trigger fires when the message:

- Starts with the full match string;
- The next character (if any) is a space (i.e., `!ban` does not fire on `!banner`).

If `channelId` is set, the trigger MUST fire **only** for messages observed via the **command-listener SSH session** for that channel (see §11.2). If `channelId` is unset, the trigger MUST fire **only** for messages observed via the **base SSH session** (the global one). This invariant ensures a flow without `channelId` is not re-fired by per-channel listeners that happen to be active for unrelated reasons.

#### 13.4.1 Enriched event data

The flow's event-data record is the original event payload plus:

| Key | Value |
|---|---|
| `command_args` | The substring after the matched command, trimmed |
| `command_name` | The matched `commandName` |
| `command_channel_id` | The channel id the listener was scoped to (if any), else `target` from the original event, else empty |
| `clid` | The invoker's clid; if missing in the original payload, derived from `invokerid` / `invoker_id` / `client_id` |

### 13.5 Per-pair listener synchronisation

For each `(configId, sid)` pair on which any flow defines a `command` trigger with a non-null `channelId`, the engine MUST keep an open command-listener SSH session for every distinct `channelId` referenced by such triggers — and MUST close listeners for channels that are no longer referenced. The engine MUST NOT open a command-listener session for a channelId that no flow currently references.

### 13.6 How to verify

- Define an `event` trigger for `client_mic_muted` with no filters; have a real TS client toggle mic-muted; the flow MUST fire exactly once per transition.
- Define a `webhook` trigger at path `hello` with secret `s3cret`; request `POST /api/bots/webhook/hello` without the secret → 404; with the wrong secret → 404 (same shape); with the right secret → `200 {"triggered":1}`.
- Define a `command` trigger for `!ping` scoped to channel `42`; send `!ping` in channel `42` → fires once; send `!ping` in channel `99` → does NOT fire; send `!pinger` anywhere → does NOT fire.
- Define a `cron` trigger with expression `*/5 * * * *` and an invalid expression `garbage`; the second MUST log an error and not register.

---

## Chapter 14. Action Catalogue

This chapter is the normative catalogue of every action kind. Each action receives a fully-resolved data object (templates already substituted via §15.1) and an execution context. Common contracts:

- `client` is the WebQuery client for the flow's `serverConfigId`. `ctx.sid` is the flow's `virtualServerId`.
- `ctx.eventData` is the trigger's event payload; key fields it commonly carries: `clid`, `client_database_id`, `client_servergroups`, etc.
- `ctx.setTemp(name, value)` writes a temp variable; `ctx.resolveTemplate(string)` is the pre-resolver applied to templated fields.
- All actions log their entry at `info` level via the run-log.

### 14.1 Client-targeted TS3 actions

| Action | TS command issued | Required payload fields | Notes |
|---|---|---|---|
| `kick` | `clientkick` | `ctx.eventData.clid`; resolved `reasonMsg` | `reasonId` defaults to `5` (server kick) |
| `ban` | `banclient` | `ctx.eventData.clid`; resolved `reason` | `time` defaults to `0` (permanent) |
| `move` | `clientmove` | `ctx.eventData.clid`; resolved `channelId` | |
| `message` | `sendtextmessage` | resolved `message`, optional resolved `target` (else `ctx.eventData.clid`) | If `targetMode == 2` (channel), MUST move the query client into the target channel, send, then move back to the original (TS3 limits channel messages to "current channel of sender") |
| `poke` | `clientpoke` | `ctx.eventData.clid`; resolved `message` | |
| `groupAddClient` | `servergroupaddclient` | `ctx.eventData.client_database_id`; resolved `groupId` | |
| `groupRemoveClient` | `servergroupdelclient` | `ctx.eventData.client_database_id`; resolved `groupId` | |
| `setClientChannelGroup` | `setclientchannelgroup` | `ctx.eventData.client_database_id`; resolved `channelGroupId`, `channelId` | If `storeAs` is set, store full result |

**`groupRemoveAll`** is a composite that does NOT issue a single TS command. Behaviour:

- Read `client_database_id` from event-data; throw if missing.
- Parse `keepGroupIds` (comma-separated sgids).
- Read `client_servergroups` from event-data (comma-separated) — MUST be present (the polled snapshot puts it there for group-related synthetic events).
- For each current group not in `keepGroupIds`, issue `servergroupdelclient`. Errors per-group are swallowed.
- Log how many were removed and which were kept.

**`groupRestoreList`** is the symmetric add-many helper:

- Resolve `groupIds` template into a comma-separated list.
- Parse `excludeGroupIds` similarly.
- For each sgid not in exclude, issue `servergroupaddclient`. Errors per-group are swallowed.

### 14.2 Channel-targeted actions

| Action | TS command | Required payload | Notes |
|---|---|---|---|
| `channelCreate` | `channelcreate` | resolved `params: Record<string,string>` | The implementation MUST accept the editor-shape `config` keys (`channel_name`, `cpid`, `channel_topic`, `channel_password`, plus mutually exclusive flags `channel_flag_temporary` / `channel_flag_semi_permanent`) and merge them with any `params` map. Result's `cid` (when present) MUST be stored as `temp.lastCreatedChannelId`. |
| `channelEdit` | `channeledit` | resolved `channelId`, plus resolved `params: Record<string,string>` | |
| `channelDelete` | `channeldelete` | resolved `channelId`; `force: boolean` (mapped to `1` / `0`) | |
| `tempChannelCleanup` | `channellist` then `channeldelete` per match | resolved `parentChannelId`, optional resolved `protectedChannelIds` (comma list) | List all channels; for those whose `pid == parentChannelId`, not in protected set, with `total_clients == 0`, delete with `force=1`. Stores `temp.tempChannelsDeleted` count. |

### 14.3 Outbound HTTP actions (`webhook` and `httpRequest`)

Both actions:

- Resolve `url`. Run `validateUrl` (§6.7); on rejection, throw an error.
- Resolve `body` as a string. **If non-empty, the implementation MUST `JSON.parse` it before sending** (so the body is sent as JSON, not a JSON-quoted string). If the body is not valid JSON, the action fails.
- Resolve every header value template.
- Issue the HTTP request via a 10-second (`webhook`) or 15-second (`httpRequest`) timeout.

`webhook` defaults to `POST`; `httpRequest` defaults to `GET`.

`httpRequest` additionally supports an **optional response cache**:

- If `cacheKey` is set (resolved against templates):
  - On entry, look up `ts6:httpcache:<cacheKey>` in the configured Valkey/Redis cache. If hit, parse JSON and store in `temp.<storeAs>` and return *without* issuing the HTTP request.
  - If Valkey is unreachable, fall back to an in-process Map-based cache keyed by the same `cacheKey`.
  - On miss, after the request returns, parse the response body as JSON if possible, store under the same cache key with TTL `cacheTtlSeconds` (default `86400`).
- If `cacheKey` is not set, never cache.

Response storage: if `storeAs` is set, parse the response body as JSON if it looks like JSON; otherwise store the raw string. Stored under `temp.<storeAs>`.

### 14.4 `webquery` passthrough

```json
{
  "actionType": "webquery",
  "command": "<TS command name (string)>",
  "params": { "<key>": "<value-template>", ... },
  "storeAs": "<optional temp name>"
}
```

Behaviour:

1. Resolve `command` template. Trim. **If the command string contains a space, the substring after the first space MUST be parsed as inline `key=value` pairs** (separated by whitespace). Inline pairs are merged into `params` only when the same key is not already present in `params` (params take precedence).
2. **Whitelist enforcement (§16.4):** lower-case the resolved command and check membership in the allow-list. If not present, throw `WebQuery command "<x>" is not allowed in bot flows`.
3. Resolve each value in `params` against templates.
4. Issue `client.execute(sid, command, params)`.
5. Store the full result under `temp.lastResult` (JSON-stringified). If `storeAs` is set, store the first record (when `result` is an array) or the result object itself under `temp.<storeAs>`.

### 14.5 Idle / AFK helpers

**`afkMover`** sweeps clients into an AFK channel:

- Issue `clientlist -times -groups`.
- For each client (skipping query clients, those already in `afkChannelId`, and those whose `client_servergroups` intersects `exemptGroupIds`):
  - If `client_idle_time / 1000 < idleThresholdSeconds`, skip.
  - Else issue `clientmove clid=... cid=afkChannelId`. Errors are swallowed per-client.
- Default `idleThresholdSeconds` is 300.
- An optional `checkMuteState` flag (added by the fork) MAY also force-move "double-muted" clients (mic AND speakers off) regardless of idle time. Implementations MAY treat the flag as advisory in v1 and add the explicit double-mute check later.
- Stores `temp.afkMovedCount`.

**`idleKicker`** is the same sweep but kicks instead of moving:

- `clientkick clid=... reasonid=5 reasonmsg=<resolved-reason>` (default reason `Idle timeout`).
- Default `idleThresholdSeconds` is 1800.
- Stores `temp.idleKickedCount`.

**`pokeGroup`** broadcasts a poke to every online member of a server group:

- Issue `servergroupclientlist sgid=...` to get cldbids.
- Issue `clientlist` to map cldbid → clid.
- For each match (excluding query clients), issue `clientpoke`. Errors per-target are swallowed.
- Stores `temp.pokedCount`.

**`rankCheck`** auto-promotes long-time members:

- Resolve `ranks` template, JSON-parse to `[{hours, groupId}]`. Sort descending by `hours`.
- Issue `clientlist -times -groups`.
- For each non-query client:
  - Read accumulated `onlinetime_<cldbid>` flow-variable (seconds), add current connection time, convert to hours.
  - Walk the sorted ranks; find the first rank whose `hours` is met **and** the client is not already in that group; issue `servergroupaddclient` with that group; break (highest eligible only).
- Stores `temp.rankPromotedCount`.

### 14.6 Voice action family

These actions delegate to the music-bot manager (Part VI). Each takes a `botId` (resolved template). Behaviour:

| Action | Effect |
|---|---|
| `voicePlay` | If `playlistId` set, load all songs in playlist order into the bot's queue and play first if connected. Else if `songId` set, add and play that song. |
| `voiceStop` | `manager.stopBot(botId)` |
| `voiceJoinChannel` | Persist `defaultChannel` and optional `channelPassword`; restart the bot. |
| `voiceLeaveChannel` | Same as `voiceStop`. |
| `voiceVolume` | Clamp to `[0,100]`; apply via `bot.setVolume`. |
| `voicePauseResume` | `pause` / `resume` / `toggle` (default `toggle`). |
| `voiceSkip` | `direction === "previous"` calls `bot.previous()`; otherwise `bot.skip()`. |
| `voiceSeek` | `bot.seek(seconds)`. |
| `voiceTts` | Reserved; the reference logs a warning and does nothing. Implementations MAY implement TTS but MUST NOT crash the flow when this action is invoked. |

### 14.7 Stateful helpers

**`generateCode`** produces a random verification code:

- `length` clamped to `[1, 12]`, default `5`.
- If `numericOnly` is `true` (default): generate a uniform random integer in `[10^(length-1), 10^length)` using a CSPRNG. (For `length=1` the range is `[0, 10)`.) Cast to string.
- Otherwise: pick `length` characters from the unambiguous alphabet `ABCDEFGHJKLMNPQRSTUVWXYZ23456789` (no `I`, `O`, `0`, `1`).
- Store under `temp.<storeAs>` (default `code`).

**`generateToken`** issues a TeamSpeak privilege key from inside a flow:

- Resolve `groupId` (and `channelId` if `tokenType == "1"`).
- Issue `tokenadd` with `tokentype=<0|1>`, `tokenid1=<groupId>`, `tokenid2=<channelId or "0">`.
- Read `token` from result (object or array's first element).
- Store under `temp.<storeAs>` (if set) and `temp.lastToken` (always).

### 14.8 Cache helpers (`valkeyGet`, `valkeySet`, `valkeyDelete`)

Operate against the configured Valkey/Redis cache. **All three MUST tolerate cache unavailability gracefully**: log a warning, treat reads as cache misses (store `null` if `storeAs`), allow writes/deletes to silently fail. The flow MUST continue so subsequent action nodes still execute.

| Action | Behaviour |
|---|---|
| `valkeyGet` | `GET key`. If hit, JSON-parse the value if possible; store under `temp.<storeAs>`. If miss, store `null`. |
| `valkeySet` | `SET key value` with optional `EX <ttlSeconds>`. |
| `valkeyDelete` | `DEL key`. |

Keys are passed verbatim (no namespace prefix is applied automatically); the operator is responsible for picking unique keys.

### 14.9 `animatedChannel`

Animated channel names are managed by a dedicated **animation manager** that is independent of flow execution. The action node itself is essentially a *declaration*; on flow enable, the engine MUST start an animation timer for each `animatedChannel` node and MUST stop it on flow disable. The animation manager MUST tolerate per-tick TeamSpeak errors (e.g., name unchanged → upstream `304`); errors do NOT stop the animation.

#### 14.9.1 Animation parameters

| Field | Default | Notes |
|---|---|---|
| `channelId` | required | |
| `text` | required | May contain `{{time.*}}` placeholders, resolved per-tick (lightweight; no engine context required) |
| `style` | `scroll` | One of `scroll`, `typewriter`, `bounce`, `blink`, `wave`, `alternateCase` |
| `intervalSeconds` | `3` | Clamped to ≥ 0.25 (250 ms minimum) |
| `prefix` | `[cspacer]` | Common TeamSpeak channel-spacer prefix |

#### 14.9.2 Frame generation

`MAX_CHANNEL_NAME = 40`. Effective text width = `40 - prefix.length`.

- **`scroll`** rotates the text (with a 3-space tail) one character left per frame; the channel name is `prefix + (text|tail).slice(i, i + width)` for each `i` in `[0, len(text|tail))`.
- **`typewriter`** types out the text one character per frame, holds the full text for 3 frames, then blanks for 2 frames.
- **`bounce`** shifts the text rightward across the trailing whitespace, then back leftward (skipping endpoints to avoid double frames).
- **`blink`** alternates text → text → dashed (non-spaces replaced with `-`) → text → text → spaces. Six-frame cycle.
- **`wave`** decorates the text with a `[» ... «]`-style growing/shrinking border, six frames.
- **`alternateCase`** alternates case on alternating positions, two frames.

The implementation MUST honour these patterns exactly; operator UX depends on the visual feel.

#### 14.9.3 Tick

Each tick:

1. Resolve `{{time.hours}}`, `{{time.minutes}}`, `{{time.seconds}}`, `{{time.time}}`, `{{time.date}}`, `{{time.day}}`, `{{time.month}}`, `{{time.year}}`, `{{time.timestamp}}` against the current wall clock (and `timezone` if set). Any other `{{...}}` placeholders MUST be left as-is (the per-tick resolver is intentionally lightweight; richer placeholders are not supported in animation text).
2. Generate the frame array for `style` and resolved text.
3. `client.executePost(sid, "channeledit", { cid: channelId, channel_name: frames[frameIndex % frames.length] })`. Increment `frameIndex`.

The first tick fires immediately on start.

### 14.10 How to verify

- Build a flow: `event(notifycliententerview)` → `message(target=clid, msg="Welcome {{event.client_nickname}}")`. Connect a real client; the bot MUST send a private welcome.
- Build a flow: `cron(*/2 * * * *)` → `webquery(serverinfo, storeAs=server)` → `channelEdit(channelId=42, channel_name="Online: {{temp.server.virtualserver_clientsonline}}")`. Confirm the channel name updates every 2 minutes.
- Build a flow with an `httpRequest` to a public JSON endpoint with `cacheKey="weather"`, `cacheTtlSeconds=300`. Stop external connectivity; subsequent invocations MUST hit the cache (Valkey or in-memory).
- Build a flow with `webquery(command="serverstop")`. The action MUST throw `WebQuery command "serverstop" is not allowed in bot flows`.

---

## Chapter 15. Conditions, Variables, Delays, Logging

### 15.1 Template substitution

Flow string fields are subject to template substitution before being passed to actions. The template language uses `{{path|filter}}` placeholders:

- `path` is a dotted path beginning with one of the namespaces `event.`, `var.`, `temp.`, `time.`, `exec.`.
- `filter` is optional; supported filters are `uptime`, `round`, `floor` (descriptions below).

Resolution rules for each namespace:

#### `event.<key>`

- If the key exists in the trigger's event-data record, return its string value.
- Else, if the key contains a dot (`event.<top>.<rest>`), look up `<top>` in event-data, **JSON-parse the value**, and walk `<rest>` as a dotted path. Return the resolved leaf as string, or empty string if undefined or non-JSON.

#### `var.<name>`

- Look up the persistent flow variable with `(flowId, name, scope='flow')`. Return the stored string value, or empty string if missing.

#### `temp.<dotted>`

- If `<dotted>` has no dots: look up the top-level temp variable. If `null`/`undefined`, return the literal string `"null"` (so condition expressions like `{{temp.x}} != null` evaluate correctly). If an object/array, return JSON-stringified. Else stringify.
- If `<dotted>` has dots: look up the top-level value, JSON-parse if it is a string and parses, then walk the dotted remainder.

#### `time.<key>`

Returns one of these computed values from the wall clock (or the trigger's timezone if specified):

| Key | Format |
|---|---|
| `hours`, `minutes`, `seconds`, `day`, `month` | Two-digit zero-padded decimal |
| `time` | `HH:MM` |
| `date` | `DD.MM.YYYY` |
| `year` | Four-digit decimal |
| `timestamp` | Unix-epoch seconds (integer) |
| `dayOfWeek` | `0–6` (`0` = Sunday) |

#### `exec.<key>`

Returns metadata about the current execution: `flowId`, `executionId`, `configId`, `sid`, `triggerType`.

#### Filters

- `uptime`: takes a number-of-seconds string; returns a human-readable duration `<d>d <h>h <m>m` (`d` and `h` omitted when zero, but `h` is shown when `d > 0`).
- `round`: takes a numeric string; returns `Math.round` of it.
- `floor`: takes a numeric string; returns `Math.floor` of it.

Unknown filters MUST be a no-op (return the value unchanged).

### 15.2 Condition expression evaluation

Condition nodes evaluate the expression against a **scope** built from event/var/temp/time and a `null` literal. The expression language is the same library used at the visual-editor layer (`mathjs` in this fork; `expr-eval` in the upstream); semantics are an arithmetic-expression evaluator with the following extensions registered as functions:

| Function | Returns |
|---|---|
| `contains(haystack, needle)` | `1` if `String(haystack).includes(String(needle))` else `0` |
| `startsWith(s, prefix)` | `1` / `0` |
| `endsWith(s, suffix)` | `1` / `0` |
| `lower(s)` | Lowercased string |
| `upper(s)` | Uppercased string |
| `length(s)` | Length of string |
| `split(s, sep, index)` | The `index`-th element of `s.split(sep)`, or empty string |
| `hasGroup(groups, sgid)` | `1` if `sgid` (string-equal) is in `groups.split(",")` else `0` |

The implementation MUST NOT register or expose dangerous builtins (file IO, `eval`, etc.).

#### 15.2.1 Pre-processing

Before evaluation, the implementation MUST:

1. **Block prototype-pollution patterns:** if the expression matches the regex `/(__proto__|constructor|prototype)\s*[.[(]/`, do not evaluate; return `false`. The reference logs the blocked expression at warning level.
2. **Strip `{{...}}` template wrappers:** for every `{{inner}}` in the expression, replace with `inner` (with any pipe-filter suffix discarded). This is so operators can use familiar template syntax (e.g., `{{event.clid}} > 0`) while the underlying scope-evaluation does the resolution itself. Without this strip, the template would be substituted as a raw string and produce syntactically invalid expressions when the value contains commas.

#### 15.2.2 Scope namespace contents

Built per-evaluation:

- **`event`**: every field of `ctx.eventData`, with numeric strings coerced to numbers. (`client_clid: "5"` becomes `event.client_clid == 5`, evaluable as a number.)
- **`var`**: every persistent flow variable as `name → value` (with numeric coercion).
- **`temp`**: every temp variable; nested objects/strings have numeric strings recursively coerced (so `temp.server.virtualserver_clientsonline > 5` works against the JSON-decoded WebQuery response).
- **`time`**: as in §15.1, with numeric values as numbers (not strings).
- **`null`**: the JS `null` literal. Required because the underlying expression engine has no built-in null. Operators write `{{temp.x}} != null` to test for absence.

#### 15.2.3 Result branching

Evaluation result is converted to boolean by the standard "truthy" rules of the underlying language. The condition node has two outgoing handles (`"true"` and `"false"`); the engine MUST traverse only the edge with the matching `sourceHandle`.

On evaluation failure (parse error, type error), the implementation MUST log a warning and treat the result as `false`. The flow continues down the `"false"` branch.

### 15.3 Variables

Three operations on a `variable` node (default `set`):

| Operation | Behaviour |
|---|---|
| `set` | Resolve `value` template, write to `(flowId, name, scope='flow')` (upsert). |
| `increment` | Read current value, parse as float (default `0`), add resolved `value` parsed as float, store as string. |
| `append` | Read current value (default `""`), concatenate resolved `value` as a string, store. |

Variable writes MUST be tolerant of database failures (notably `SQLITE_FULL`): catch and silently swallow at the variable-write layer; the flow continues. Reads on the same failure path MUST return empty string (not raise).

`temp` variables (from inside the runner, not a `variable` node) are in-memory only and last for the lifetime of one execution.

### 15.4 Delays

```json
{
  "nodeType": "delay",
  "delayMs": <number>
}
```

The implementation MUST clamp `delayMs` to `[0, 300000]` (5-minute hard ceiling). Reasoning: long delays tie up flow-runner concurrency; the cap prevents runaway pile-up. Operators wanting longer delays MUST split into multiple cron-driven steps.

### 15.5 Logging

```json
{
  "nodeType": "log",
  "level": "debug" | "info" | "warn" | "error",
  "message": "<template>"
}
```

Resolves `message` and writes a `BotExecutionLog` row with the given level, plus mirrors to the in-process logger at the same level. The implementation MUST tolerate database write failures during logging (swallow); engine survival > log fidelity.

### 15.6 How to verify

- Define an `event` trigger with a condition node `{{event.client_servergroups}} == "8,15,75"`. Confirm the condition engine evaluates this as a string-equality check (the `{{...}}` strip MUST happen).
- Define a condition `event.clid > 100` against a payload where `clid="42"`. The evaluator MUST coerce to number and return `false`.
- Try a malicious expression `__proto__.foo = 1`. Evaluation MUST be blocked, logged at warning, and yield `false`.
- Set a flow-variable to `"5"`, then increment by `"3"`. The stored value MUST be `"8"`.
- A delay of 600000 ms MUST resolve in 300000 ms (clamped).

---

## Chapter 16. Flow Runtime (Execution Loop)

### 16.1 Lifecycle

The engine maintains an in-memory map of **loaded flows** (those that are enabled and parsed). Bring-up:

1. On engine start (or `enableFlow(flowId)`):
   - Read the row, parse `flowData`, normalise per §12.3.
   - Filter trigger nodes; require either at least one trigger node OR at least one `animatedChannel` action (the engine MAY load animation-only flows). Otherwise log a warning and skip.
   - Insert into the loaded map.
   - For `event` and `command` trigger nodes, ensure the SSH event bridge is connected to `(serverConfigId, virtualServerId)`.
   - For `command` triggers with `channelId`, sync the per-channel command-listener sessions for the pair (§13.5).
   - For event triggers whose `eventName` is one of the synthetic-event names (or `notifyclientupdated`), start the **client-status poller** for the pair (§16.3) — TeamSpeak does NOT push these via SSH `servernotifyregister`; polling is mandatory.
   - For `cron` triggers, register the schedule.
   - For `webhook` triggers, append entries to the webhook registry.
   - For `animatedChannel` action nodes, start the animation timers.
2. On `disableFlow(flowId)` or row update:
   - Stop any animations owned by this flow.
   - Tear down cron jobs for this flow.
   - Remove webhook entries for this flow.
   - Possibly stop the client-status poller for the pair (re-evaluated against remaining flows).
   - Remove from the loaded map.
   - Possibly close SSH event-bridge connections that no remaining flow needs (and per-channel command-listener sessions whose channels are no longer referenced).
3. On `reloadFlow(flowId)`:
   - If the flow was loaded, disable then re-enable it. If the row's `enabled` is now false, leave it disabled.

### 16.2 Failure-tolerance boundaries

The execution loop MUST be tolerant of `SQLITE_FULL` (database disk-full). Specifically, the implementation MUST catch `SQLITE_FULL` at three points:

1. **Around the per-execution top-level loop** (creating `BotExecution`, updating its status). Disk-full here MUST cause the run to proceed without persistence; flow logic still runs against TeamSpeak.
2. **Around `executeAction`** in the per-action error handler. Disk-full inside an action's persistence layer MUST not propagate.
3. **Around `getVariable` / `setVariable`** (the persistent flow-variable layer). Disk-full reads return empty string; writes are silently dropped.

Other database errors (connection lost, schema mismatch) MUST be logged and propagate as flow-execution failures, marked in the `BotExecution` row.

### 16.3 Client-status poller

For every `(configId, sid)` pair on which any loaded flow has an `event` trigger whose `eventName` is in the synthetic-event set (`notifyclientupdated`, `client_went_away`, `client_came_back`, `client_mic_muted`, `client_mic_unmuted`, `client_sound_muted`, `client_sound_unmuted`, `client_mic_disabled`, `client_mic_enabled`, `client_sound_disabled`, `client_sound_enabled`, `client_group_added`, `client_group_removed`, `client_recording_started`, `client_recording_stopped`, `client_nickname_changed`), the engine MUST run a poller every 5 seconds.

Each tick:

1. Issue `clientlist -away -voice -groups -uid` over the **base SSH session** for the pair (the bridge's `executeCommand`).
2. Parse the response. Skip query clients (`client_type == "1"`).
3. Build a snapshot per `clid` containing the trackable fields: `client_away`, `client_input_muted`, `client_output_muted`, `client_input_hardware`, `client_output_hardware`, `client_is_recording`, `client_servergroups`, `client_nickname`, plus identifying fields (`clid`, `cid`, `client_unique_identifier`, `client_database_id`, `client_type`).
4. Compare against the previous snapshot for the same `clid`. If this is the first time the `clid` is seen, store the snapshot and emit nothing.
5. Otherwise:
   - If any of the six boolean fields changed, dispatch a `notifyclientupdated` event with the full snapshot as event-data. Pass the *changed-fields set* to the synthetic-event expansion (per §13.1.1) so each transition fires its synthetic event exactly once.
   - If `client_servergroups` changed (set diff), emit one `client_group_added` per added sgid and one `client_group_removed` per removed sgid, with `sgid` injected into the event-data.
   - If `client_nickname` changed, emit `client_nickname_changed` with the snapshot.
6. Replace the cached snapshot with the new one.
7. Remove from cache any `clid` no longer present in the current `clientlist`.

If the SSH session is not yet connected, the tick MUST silently skip; the next tick will try again.

The poller MUST stop when no remaining flow needs it (i.e., when its pair set drops out of the "needs polling" predicate).

### 16.4 WebQuery command whitelist

The `webquery` action's allow-list of permitted TeamSpeak ServerQuery commands (case-insensitive) is part of the flow-engine's external contract:

```
serverinfo, serverlist, servergrouplist, servergroupsbyclientid,
channellist, channelinfo, channelfind, channelcreate, channeledit, channeldelete, channelmove,
clientlist, clientinfo, clientfind, clientgetids, clientgetdbidfromuid, clientgetnamefromuid, clientgetnamefromdbid,
clientmove, clientkick, clientpoke, clientdblist, clientdbinfo,
sendtextmessage, messageadd, messagelist, messagedel, messageget,
servergroupaddclient, servergroupdelclient, servergroupclientlist, channelgrouplist, channelgroupclientlist, setclientchannelgroup,
banclient, banlist, bandel, banadd,
tokenadd, tokenlist, tokendelete,
complainlist, complaindel, complainadd,
logview,
whoami, version, hostinfo, connectioninfo
```

Notable exclusions (deliberate): `serverstop`, `serverdelete`, `serverstart`, `servercreate`, `instanceedit`, `permissionreset`, `serversnapshotdeploy`, `clientaddperm`/`clientdelperm` (use the dedicated route surface for permissions, not flows), `permadd`/`permdel`. The implementation MUST NOT expand the list without explicit operator opt-in; the safety guarantee is that flows cannot destructively mutate server-instance state.

### 16.5 Concurrency

The engine MUST cap concurrent executions per flow at **20**. When a trigger fires while 20 executions are in flight for the same flow, the new firing MUST be rejected with a warning log; it does NOT queue. Different flows execute independently — there is no global concurrency cap.

Per-execution node visits MUST be capped at **100**. Any flow that visits more than 100 nodes in one execution MUST raise `Max node visits (100) exceeded — possible infinite loop` and the execution is marked failed. This protects against pathological cyclic flows.

### 16.6 Persistence

Per execution:

- A `BotExecution` row is created on start with `status="running"`, the trigger type, and JSON-encoded trigger data.
- A `BotExecutionLog` row is appended at every `info`/`warn`/`error`/`debug` log call inside the run.
- On completion, the `BotExecution` is updated to `status="completed"` and `endedAt=now`.
- On failure, `status="failed"`, `error=<message>`, `endedAt=now`.
- Every status change MAY also emit a `bot:execution:start` / `bot:execution:complete` / `bot:execution:failed` WebSocket message (Chapter 8).

If `BotExecution` could not be created (DB unavailable), the engine MUST still execute the flow; `executionId` becomes `0` and per-step log writes are best-effort (they will fail at the FK level, which the implementation swallows).

### 16.7 How to verify

- Trigger a flow whose action issues `serverstop` via `webquery`. The flow's `BotExecution` MUST end with `status=failed` and the error message MUST name the disallowed command.
- Force `SQLITE_FULL` (fill the SQLite volume to capacity). Trigger a flow with a `kick` action. The kick MUST still happen against the upstream TS server; the run-log MAY be incomplete.
- Define a `client_mic_muted` trigger. Mute and unmute repeatedly; the flow MUST fire once per transition only (no duplicates while held muted).
- Define a self-cycling flow (action → log → loop back to itself via condition). The execution MUST terminate at 100 visits with the cap message.

---

## Chapter 17. Pre-Built Flow Templates

The reference ships 17 templates (the front-end exposes a "Template Gallery" that lets operators import any of them and edit the resulting flow). Each template is a parameterised function that produces a flow document; the operator supplies the parameters at import time.

### 17.1 Template index

| Template | Category | Parameters | Behaviour |
|---|---|---|---|
| **Clock Channel** | info-channels | `channelId`, `timezone` | Cron `* * * * *` → `channelEdit` setting `channel_name` to `[cspacer]{{time.time}}` |
| **Online Counter** | info-channels | `channelId` | Cron `* * * * *` → `webquery serverinfo storeAs=server` → `channelEdit` to `[cspacer]Online: {{temp.server.virtualserver_clientsonline}}/{{temp.server.virtualserver_maxclients}}` |
| **Server Stats** | info-channels | three channelIds | Cron `*/5 * * * *` → `webquery serverinfo` → fan-out three `channelEdit`: uptime (with `\|uptime` filter), client count, channel count |
| **Animated Channel Name** | info-channels | `channelId`, `text`, `style`, `interval`, `prefix` | Single `animatedChannel` action; engine handles the timer |
| **Welcome Message** | automation | optional template | `event(notifycliententerview)` → `message(target=clid, msg=<welcome>)` |
| **Support System** | automation | `commandPrefix`/`commandName`, `supportChannelId`, optional staff group | `command` trigger → `move` → optional `pokeGroup` |
| **Temp Channel Creator** | automation | `parentChannelId`, optional cleanup interval | Two flows: (a) `event(channelmoved)` into trigger channel → `channelCreate` + `move`; (b) `cron` → `tempChannelCleanup` |
| **Auto-Rank** | automation | `ranks` JSON array | Cron `*/15 * * * *` → `rankCheck` |
| **Last-Seen Tracker** | automation | `descriptionChannelId` | `event(notifyclientleftview)` → variable update → `channelEdit` description |
| **AFK Mover** | moderation | `afkChannelId`, `idleThresholdSeconds`, optional exempt groups, optional `checkMuteState` | Cron → `afkMover` |
| **Idle Kicker** | moderation | `idleThresholdSeconds`, optional reason, optional exempt groups | Cron → `idleKicker` |
| **Bad Name Checker** | moderation | regex of disallowed names, action (kick/poke/warn) | `event(notifycliententerview)` → condition on nickname → `kick` or `poke` |
| **Group Protector** | moderation | `protectedChannelIds`, allowed group ids | `event(notifyclientmoved)` → condition on group membership → `move` back |
| **Webhook → Server Message** | integration | webhook path, secret, message template | `webhook` trigger → `message(targetMode=3)` |
| **Webhook → Assign Group** | integration | webhook path, secret, group id | `webhook` trigger → `groupAddClient` |
| **Webhook → Update Channel** | integration | webhook path, secret, channelId | `webhook` trigger → `channelEdit` |
| **Anti-VPN** | moderation | external API endpoint | `event(notifycliententerview)` → `httpRequest` to VPN-detection API → condition → `kick` |

The implementation MAY ship the same set, a subset, or a superset; templates are a UX convenience, not a contract. The implementation MUST ensure that each template, when imported with valid parameters, produces a *valid* flow document (per §12.1) that the engine accepts without modification.

### 17.2 Template structure

Each template is described by:

- A stable `id` string (operator-visible, used for "import again" UX).
- A human-readable `name` and `description`.
- A `category` (one of `info-channels`, `moderation`, `automation`, `integration`).
- A list of configuration fields (label, type, optional default, optional enum, required/optional).
- A factory: `(config: Record<string, string>) => FlowDefinition`.

The implementation MUST ensure that the produced `FlowDefinition` uses **engine-format** node shapes (so the engine does not need to normalise them on import). This is purely a quality-of-import property; importing through the same persistence layer used for hand-edited flows means normalisation will happen anyway.

### 17.3 How to verify

- For each template offered, import with a default-valid parameter set and `enable` it; inspect the resulting flow's `BotExecution` rows to confirm at least one execution succeeds for triggers within ~5 minutes (where applicable).

---

# Part VI — Voice (Music) Bots

The voice-bot subsystem connects to the TS6 voice server as a real client (not a query bot) and streams audio into a channel. The on-the-wire protocol used by these bots is the TS3-compatible UDP voice protocol, the deep details of which are deferred to **Chapter 19**. This part specifies the *application-level* lifecycle, audio pipeline, queue, and chat-command surface.

## Chapter 18. Voice Bot Lifecycle

### 18.1 States

A voice bot is in one of six states: `stopped`, `starting`, `connected`, `playing`, `paused`, `error`. Transitions:

```
                 ┌─────────────┐
       stop()    │             │   ensureDisconnected()
       ┌────────►│   stopped   │◄─────────────────────┐
       │         │             │                      │
       │         └──────┬──────┘                      │
       │                │ start()                     │
       │                ▼                             │
       │         ┌─────────────┐  fatal TS3 error    │
       │         │   starting  │─────────────────► error
       │         └──────┬──────┘ (2568, 3329, 1796)   │
       │                │ TS3 channel-join ack        │
       │                ▼                             │
       │         ┌─────────────┐                      │
       │         │  connected  │◄─────┐               │
       │         └─────┬───┬───┘      │               │
       │     play()    │   │ disconnect (transport)   │
       │               ▼   │          │               │
       │         ┌─────────┴─┐        │               │
       │         │  playing  │────────┘ (auto-reconnect)
       │         └─────┬─────┘                        │
       │     pause()   │  resume()                    │
       │               ▼                              │
       │         ┌─────────┐                          │
       └─────────┤ paused  │──────────────────────────┘
                 └─────────┘  (any error here surfaces as 'error')
```

The implementation MUST emit a `statusChange` event on every transition.

### 18.2 Boot-time auto-start

On voice-bot-manager initialisation, the implementation MUST:

1. Load every `MusicBot` row, including its server config.
2. For each row with `identityData` set: decrypt and restore the cached identity. Bots without a stored identity will generate a fresh one on first start (this consumes ~5 s of CPU; see Chapter 19).
3. Construct a runtime instance for every bot (regardless of `autoStart`).
4. For each bot with `autoStart=true`, call `start()` asynchronously. Failures MUST be captured into a per-bot "start error" map so the API can surface them; they MUST NOT block boot of other bots.

### 18.3 Auto-reconnect

When a connected bot's TS3 transport drops *unexpectedly* (the bot did not call `stop()`), the manager MUST schedule a reconnect:

| Parameter | Value |
|---|---|
| Max attempts | `10` |
| Backoff | `min(2^attempts × 1000 ms, 30000 ms)` |
| Grace period before each attempt | `5000 ms` |
| Manual stop OR fatal-error state | No reconnect |

After 10 failed attempts, the bot enters the manager's "given up" set; the implementation MUST emit `music:bot:reconnectFailed` over WebSocket and clear scheduling state. Operator action is then required (manual `start`).

A "fatal" TS3 error MUST suppress reconnect entirely:

| TS3 error code | Meaning |
|---|---|
| `2568` | Invalid password |
| `3329` | Banned |
| `1796` | Max clients reached (server-full) |

### 18.4 Per-server multi-bot semantics

There is no per-server bot count limit beyond the global `max_music_bots` AppSetting (default `5`). Multiple bots on the same TS server connect with distinct identities (independent UID hashes) so the server treats them as separate clients. Implementations MUST NOT serialise per-server bots; they connect and play independently, each with its own audio pipeline.

### 18.5 Identity generation and persistence

A music bot's TS3 identity is a public/private keypair plus a "security level" — see Chapter 19 for protocol-level details. The application-level rules are:

- **First-time bot creation** generates an identity at **security level 23** (a value comfortably above the typical TeamSpeak server requirement of 8 and acceptable on most strict servers requiring ≥ 21). Generation runs in a worker thread (~5 s of brute-force) so the main event loop is not blocked. The result is encrypted with the at-rest key and stored in `MusicBot.identityData`.
- **On bot start** the cached identity is decrypted and reused. If `identityData` is missing (legacy or freshly imported bot), the bot MUST generate a fresh identity at security level 8 (lower bar; faster) and proceed without persisting — operator can re-save the bot to upgrade.

The implementation MUST NOT log identity material. The encrypted blob MUST never appear in any wire response (the `GET /music-bots` and `GET /music-bots/:id` routes redact it).

### 18.6 Manager interface (for upper layers)

```
listBots()       — [{id, status, nowPlaying}]  (every bot, regardless of state)
getBot(id)       — runtime handle (or undefined)
createBot(...)   — generate identity, persist, instantiate runtime, return {id}
removeBot(id)    — stop and delete
startBot(id)     — clear reconnect state; start runtime
stopBot(id)      — clear reconnect state; stop runtime
stopAll()        — graceful shutdown of every bot
getStartError(id)— last auto-start error message, if any
```

### 18.7 How to verify

- Stop the upstream TS server with a bot connected. Within ~30 seconds the bot MUST attempt a reconnect; bring the upstream back, the bot MUST re-establish connection.
- Mark a bot as `autoStart=true`, restart the back-end, observe the bot reaches `connected` without operator action.
- Create a bot using a wrong server password; the bot's status MUST land at `error` and no reconnect attempts MUST be scheduled.

---

## Chapter 19. TeamSpeak Voice Client Protocol

This chapter specifies the on-the-wire UDP voice protocol used by music bots to connect to a TeamSpeak 6 voice server. The TeamSpeak voice protocol has never been officially documented; this chapter is the spec writer's behavioural reading of the reference's implementation. The chapter is the highest-risk in the spec.

> **Off-ramp reminder:** if the implementation uses an existing third-party TeamSpeak voice client library (`tsclientlib` for Rust, the C# `TS3AudioBot` codebase, the JavaScript `dreamspeak` lineage), the implementer can skip this chapter entirely. Such libraries are independent reverse-engineerings and using one is clean-room-clean for the purposes of reimplementing TS6 Manager. The remainder of this chapter is for an implementer who chooses to roll their own client.

> **Open Question (Q-19.0):** The reference's voice library is itself a port of `DreamSpeak`/`TSLib` (per its own header comments). The implementer SHOULD treat this chapter as a starting map and consult an open-source TS3 protocol reference (e.g., the public `tsclientlib` documentation or `TS3AudioBot` source) to fill in details the spec writer's read did not capture.

### 19.1 Transport

- **UDP** to `<host>:<voicePort>` (default voice port `9987`).
- Single client-side socket bound to ephemeral local port. Send and receive buffers are increased to 1 MiB each (best-effort; some platforms forbid the syscall and the implementation MUST tolerate failure).
- All packets are encrypted (with one exception, the unencrypted `Init1` exchange).

### 19.2 Packet types

The protocol defines 9 packet types, identified by a 1-byte tag in every packet header:

| Code | Name | Direction | Purpose |
|---|---|---|---|
| 0 | `Voice` | C2S, S2C | Voice frames (Opus, etc.) |
| 1 | `VoiceWhisper` | C2S, S2C | Voice frames addressed to a subset of clients |
| 2 | `Command` | C2S, S2C | Reliable command-channel (text "name key=value …" — see §19.7) |
| 3 | `CommandLow` | C2S, S2C | Low-priority reliable command-channel (server-side info, less time-sensitive) |
| 4 | `Ping` | C2S | Periodic keepalive |
| 5 | `Pong` | S2C | Reply to `Ping` |
| 6 | `Ack` | C2S, S2C | Acknowledge a `Command` |
| 7 | `AckLow` | C2S, S2C | Acknowledge a `CommandLow` |
| 8 | `Init1` | C2S, S2C | Unencrypted handshake exchange |

### 19.3 Packet header

Two header layouts. **Client-to-server**:

```
| MAC (8) | PacketID (2) | ClientID (2) | PacketType+Flags (1) | <body> |
```

**Server-to-client**:

```
| MAC (8) | PacketID (2) | PacketType+Flags (1) | <body> |
```

The 1-byte `PacketType+Flags` packs:

- low 4 bits: packet type (0..15, only 0..8 used).
- high 4 bits: flag bits.

| Flag | Meaning |
|---|---|
| `0x10` | `FRAGMENTED` — this packet is part of a multi-packet command/voice payload |
| `0x20` | `NEWPROTOCOL` — TS6 "new protocol" framing (vs. the TS3 v1 framing) |
| `0x40` | `COMPRESSED` — body is QuickLZ-compressed |
| `0x80` | `UNENCRYPTED` — body is not EAX-encrypted (used for `Init1` and during early handshake) |

**Maximum packet size:** 500 bytes total (including MAC and header). Maximum body content per packet: `500 − 8 − 5 = 487` bytes for C2S, `500 − 8 − 3 = 489` bytes for S2C. Larger payloads MUST be fragmented across multiple packets with `FRAGMENTED` set on each, and reassembled on the receiver before parsing.

### 19.4 Packet ID and generation counter

For each packet type, both sides maintain:

- **Packet ID** — a 16-bit counter that increments per outgoing packet of that type. On overflow it wraps to 0 *and* the **generation counter** is incremented.
- **Generation counter** — a 32-bit counter, separate per packet type, used in the per-packet key derivation (so successive 16-bit-wraps produce distinct keys).

Both sides track `(packetCounter, generationCounter)` for each of the 9 packet types both *outbound* and *inbound*. The generation counters MUST be initialised to 0 at connect time.

### 19.5 Cryptography

#### 19.5.1 EAX mode

All encrypted packets use **AES-128-EAX** (the implementation MUST hand-roll EAX since Node's `crypto` does not expose it). EAX combines:

- AES-128-CTR (counter mode) for confidentiality.
- AES-CMAC (OMAC1) over the (nonce, header, ciphertext) for authenticity.
- Final tag = `OMAC(0, nonce) XOR OMAC(1, header) XOR OMAC(2, ciphertext)`, truncated to **8 bytes**.

The implementation MUST follow this exact construction; deviating breaks interop with the upstream.

CMAC subkeys (`K1`, `K2`) are derived from `AES_E(key, 0^16)` via the standard GF(2^128) doubling, using polynomial constant `0x87`.

#### 19.5.2 MAC length

Every packet's MAC is **8 bytes** (the truncated EAX tag). The MAC is the first 8 bytes of the on-the-wire packet (per §19.3 layout).

#### 19.5.3 Per-packet key/nonce derivation

For each packet, the implementation MUST derive a fresh `(key, nonce)` from the long-lived `ivStruct` (negotiated during handshake) plus the packet's identifying tuple:

```
tmp = [
   1 byte: 0x30 if from-server else 0x31,
   1 byte: packet type,
   4 bytes: generation counter (big-endian),
   ivStruct  (20 bytes for old protocol, 64 bytes for new)
]
hash = SHA-256(tmp)                               // 32 bytes
key  = hash[0..16]  with key[0] ^= (packetId>>8), key[1] ^= packetId & 0xff
nonce = hash[16..32]
```

Note that the packet ID is NOT in the SHA-256 input itself; it is folded into `key[0]`/`key[1]` as XOR. This is the source's behaviour and the implementation MUST match it.

The key/nonce MAY be cached per `(packet-type, generationCounter)` and reused for every packet ID within that 16-bit window — cuts compute cost, but the XOR-of-packet-id update means each packet's effective key is still unique.

#### 19.5.4 Init MAC

During the handshake (when `cryptoInitComplete` is false), packets use a fixed MAC:

```
INIT_MAC = "TS3INIT1"   (8 ASCII bytes)
```

Once the handshake is complete, the implementation MUST switch to per-packet EAX MACs.

#### 19.5.5 Dummy key/nonce

Some early-handshake packets are encrypted with a **fixed** dummy key and nonce (so the server can decrypt them before the client has proven anything):

```
DUMMY_KEY   = "c:\\windows\\syste"   (16 ASCII bytes — the first half of "c:\\windows\\system\\firewall32.cpl")
DUMMY_NONCE = "m\\firewall32.cpl"     (16 ASCII bytes — the second half)
```

Yes, this is a Windows-style file path used as a constant. It is part of the protocol contract; the implementation MUST use these literal byte sequences.

#### 19.5.6 INIT_VERSION

The handshake includes a 4-byte version constant identifying the client implementation. The reference uses `1566914096` (which decodes as the wall-clock timestamp of TS3 client release `3.5.0 [Stable]`). The implementation MUST use a version constant the upstream accepts; the reference's value works against current TS6 servers.

### 19.6 Identity

#### 19.6.1 Cryptographic basis

A TeamSpeak identity is an **ECDSA P-256 (`prime256v1`)** keypair. Persistence and import-export use a libtomcrypt-flavoured ASN.1 format (not standard SEC1 or SPKI).

The **public key string** (called *omega* in the protocol) is a libtomcrypt SEQUENCE:

```
SEQUENCE {
  BIT STRING (0x00 unused bits, 1 byte 0x80 indicating public-with-private flag — 0x80 = pub-only, 0xc0 = priv-only),
  INTEGER (32),    -- always 32 (key size in bytes)
  INTEGER (x),     -- public key X
  INTEGER (y),     -- public key Y
  [INTEGER (d)]    -- optional: private key scalar
}
```

This SEQUENCE is base64-encoded to form the human-visible "omega" string.

#### 19.6.2 UID

```
uid = base64(SHA-1(public_key_string_as_ascii_bytes))
```

This is the operator-visible client UID.

#### 19.6.3 Identity blob format ("identity export")

A TeamSpeak client's exported identity is a string of the form:

```
<keyOffset>V<base64-blob>
```

Where:

- `keyOffset` is a non-negative integer, ASCII-encoded, the security-level brute-force result.
- `base64-blob` is base64 of a pre-obfuscated buffer.

To **decode**:

1. Parse `keyOffset`, decode `base64-blob` to bytes (`ident`).
2. Find a null byte starting at offset 20 in `ident` (or use the rest of the buffer if no null). `hash = SHA-1(ident[20..nullIdx])`.
3. XOR the first 20 bytes of `ident` with `hash` (in place).
4. XOR the first `min(100, ident.length)` bytes of `ident` with the **static obfuscation key**:
   ```
   OBFUSCATION_KEY = unhex(
     "b9dfaa7bee6ac57ac7b65f1094a1c155"
     "e747327bc2fe5d51c512023fe54a2802"
     "01004e90ad1daaae1075d53b7d571c30"
     "e063b5a62a4a017bb394833aa0983e6e")
   ```
5. The result is UTF-8 of a base64 string of an ASN.1 DER blob (libtomcrypt format from §19.6.1). Strip any trailing nulls. Base64-decode → ASN.1 DER → parse for `(x, y, d)`.

To **encode**, perform the inverse (apply the static-key XOR, then the SHA-1 XOR; prepend `<keyOffset>V`).

The implementation MUST treat the obfuscation key as a literal protocol constant.

#### 19.6.4 Security level (proof-of-work)

A security level for an identity is computed by brute-force search of an integer `keyOffset` such that:

```
SHA-1(publicKeyString_ascii || asciiInteger(keyOffset))
```

has *N* leading zero bits. *N* is the security level. Each level doubles the average compute cost. The reference's default `generateIdentity(securityLevel = 8)` is fast (~milliseconds); a music-bot identity is generated at level **23** (≈ a few seconds of CPU). Level 21+ is the minimum many strict TeamSpeak servers require.

The brute force is purely additive: `keyOffset` starts at 0 and is incremented; the implementation MUST keep the first `keyOffset` whose hash crosses *N* leading zero bits. The implementation SHOULD run it in a worker thread to avoid blocking the event loop.

#### 19.6.5 Persisting identities

Because Node's `crypto.KeyObject` does not survive a worker-thread postMessage, the implementation MUST serialise identity as scalars (`privateKeyBigInt`, `keyOffset`, `publicKeyString`, `uid`) and reconstruct on the consumer side via the JWK pathway. The `MusicBot.identityData` column stores the JSON of these scalars (encrypted at rest per §6.3).

### 19.7 Command framing

#### 19.7.1 Wire format

Commands are ASCII text inside the body of `Command` (or `CommandLow`) packets:

```
<commandName> <key>=<value> <key>=<value> ...
```

For commands that return multiple records (e.g., `channellist`), records are separated by `|`. Each record is the same `key=value …` shape.

Values use the escape map (§3.6 / common):

| Raw | Escape |
|---|---|
| `\` | `\\` |
| `/` | `\/` |
| space | `\s` |
| `\|` | `\p` |
| LF | `\n` |
| CR | `\r` |
| TAB | `\t` |
| VT | `\v` |
| FF | `\f` |

The implementation MUST escape every value before sending and MUST unescape on receive. Keys MUST NOT be escaped (they never contain whitespace by spec).

#### 19.7.2 Reliability

`Command` (type 2) and `CommandLow` (type 3) packets are **reliable**: the receiver MUST respond with an `Ack` (type 6) or `AckLow` (type 7) referencing the packet ID. The sender retains every unacknowledged outgoing command in a `resendMap`; a resend timer (recommended: 100 ms tick) MUST detect timeouts and re-send.

The implementation MUST NOT re-send `Voice`, `VoiceWhisper`, `Ping`, `Pong`, or `Init1` — they are best-effort. `Voice` loss manifests as audio glitches; the listener tolerates it.

### 19.8 Handshake (Init1 sequence)

The handshake establishes:

- The **`ivStruct`** — a 20- (old protocol) or 64-byte (new protocol) shared secret used for per-packet key derivation.
- The **client ID** — a server-assigned identifier for this connection.

The flow is multi-round-trip and proceeds with `Init1` packets. The reference implementation distinguishes a TS3 v1 path and a TS6 "new protocol" path (`FLAG_NEWPROTOCOL`) which uses libsodium Ed25519 scalar arithmetic over the **server's licence chain**.

> **Open Question (Q-19.1):** The full handshake state machine has not been deeply traced by the spec writer. The implementation should consult one of the open-source TS3 protocol references for the precise round-trip sequence. The chapter below describes the elements involved; the choreography is subject to verification.

#### 19.8.1 Identity proof

The client proves possession of its private key by providing the omega public key (in `clientinit` command parameters) and signing handshake-binding data with ECDSA-P-256-SHA256. The server's verification is implicit (it accepts the client's UID derived from omega).

#### 19.8.2 Server licence chain

The server provides its licence as a serialised chain of blocks. Each block is:

```
| 0x00 (key kind) | key (32 bytes) | type (1 byte) | <type-specific payload> |
```

Block types:

| Code | Meaning | Payload |
|---|---|---|
| 0 | Intermediate | Two bytes (timestamps) + null-terminated issuer string |
| 2 | Server | Three bytes (timestamps + flag) + null-terminated issuer string |
| 8 | TS5 server | Variable: 1 byte property count, then `length-prefixed` property entries |
| 32 | Ephemeral | (no extra payload) |

For each block, compute `block_hash = SHA-512(block_content)[0..32]` (block content excludes the leading kind byte).

Starting from the **License root key** (a fixed Ed25519 point):

```
ROOT = unhex("cd0de2aed46345509a7e3cfd8f68b3dc7555b29dccec73cd18750f993812408a")
```

For each block, derive:

```
scalar = block_hash with: scalar[0] &= 0xf8; scalar[31] &= 0x3f; scalar[31] |= 0x40;   // Ed25519 clamp
parentKey = ed25519_add( scalar_mult_noclamp(scalar, block.key), parentKey )
```

The final `parentKey` after walking all blocks is the server's derived public key. The client then derives a shared secret with a **temporary** Ed25519 keypair (generated for this session):

```
sharedSecret = SHA-512( scalar_mult_noclamp(tempPrivateKey & 0x7fmask_top_bit, serverDerivedKey) )
```

This shared secret seeds the new-protocol `ivStruct` (64 bytes derived from the shared secret in a way the implementation must trace from the source).

#### 19.8.3 Per-packet IV after handshake

After handshake, the per-packet `(key, nonce)` derivation (§19.5.3) uses the negotiated `ivStruct`. The implementation MUST NOT bypass this even for the first post-handshake packet.

### 19.9 Login command sequence

After crypto handshake, the client sends its first `Command`-type packet: `clientinit`. Reference parameters:

```
clientinit
  client_nickname=<bot-nickname>
  client_version=<INIT_VERSION-derived string, e.g., "3.?.? [Build: 5680278000]">
  client_platform=<"Linux" | "Windows" | "macOS">
  client_input_hardware=1
  client_output_hardware=1
  client_default_channel=<channelId or "">
  client_default_channel_password=<hashed-password or "">
  client_server_password=<hashed-password or "">
  client_meta_data=
  client_version_sign=<base64 signature blob — the reference uses TS3AudioBot's pre-signed value>
  client_key_offset=<keyOffset>
  client_nickname_phonetic=
  client_default_token=
  hwid=123,456                     -- arbitrary fingerprint
  ot=1
  ...
```

Passwords (`client_server_password`, `client_default_channel_password`) are hashed as `base64(SHA-1(plaintext))`. Empty passwords MUST be sent as empty strings, not `0` or absent.

The server's response includes the assigned `client_id` (the bot's `clid`). The implementation MUST capture this and store as `clientId`. Subsequent commands that need the bot's clid (e.g., `clientmove` to a different channel) use it.

### 19.10 Voice frames

#### 19.10.1 Outgoing voice

Each Opus frame produced by the audio pipeline (§20) is wrapped in a `Voice` (type 0) packet. The reference packs:

```
| (header, encrypted via §19.5.3) |
| codec_id (1 byte: 5 for OPUS_MUSIC) |
| voice_payload (the Opus bytes) |
```

> **Open Question (Q-19.2):** The reference's exact voice packet body layout (whether codec id is the only prefix, whether there are sequence numbers in the body, etc.) was not deeply traced. The implementation should consult either an open TS3 client lib or trace the source's `sendVoice` path.

#### 19.10.2 Voice stop

A `Voice` packet with **empty body** (after the codec-id byte) is the "end of voice transmission" signal. The bot MUST send this when playback ends or pauses, so listeners' clients release the visual "talking" indicator on the bot.

#### 19.10.3 Inbound voice

Music bots are senders only; they do not consume incoming voice frames from listeners. The implementation MAY drop them on receive. (The reference at least decodes them enough to keep the protocol's packet-ID counters healthy — necessary so the server doesn't think the bot is missing acks.)

### 19.11 Compression

Some packet bodies may arrive compressed (`FLAG_COMPRESSED` set). The compression scheme is **QuickLZ level 1**, decompression-only:

- The first byte's flags include the level (`(flags >> 2) & 0x03`); the implementation MUST reject anything other than level 1.
- The header is 3 bytes for the short form (`(flags & 0x02) == 0`) or 9 bytes for the long form. Decompressed size is at byte 2 (short) or bytes 5..9 (long, little-endian int32).
- Decompressed size is hard-capped at 1 MiB (defence against decompression-bomb attacks).

The implementation does NOT need to compress outbound packets; the reference always sends uncompressed.

### 19.12 Periodic timers

The implementation MUST run two periodic timers:

| Timer | Interval | Purpose |
|---|---|---|
| Resend | 100 ms | Walk the `resendMap`; for each unack'd command older than RTT-derived deadline, retransmit. |
| Ping | 1000 ms (recommended) | Send a `Ping` packet (type 4). Server replies with `Pong` (type 5). The implementation MAY use the round-trip to estimate RTT for the resend deadline. |

Inactivity timeout: if no packet (in either direction) has been seen for ~30 seconds, the connection SHOULD be considered dead and the implementation MUST emit a `disconnected` event so the manager can schedule reconnect (§18.3).

### 19.13 Disconnect

Graceful disconnect: send `clientdisconnect reasonid=8 reasonmsg=leaving`, wait ~500 ms for the ack, then close the socket. Force-close (used during `ensureDisconnected` in the reconnect grace period): close the socket immediately without sending the disconnect command.

### 19.14 Error events

The server emits `notifyclientleftview` when the bot disconnects — this is NOT delivered back to the bot itself. Errors during command exchange arrive as a record `error id=<n> msg=<text>` (in the same packet). The implementation MUST surface these as `ts3error` events; the manager's fatal-error list is in §18.3.

### 19.15 Implementation cost estimate

For an implementer building from scratch in Rust, this chapter represents **substantial work** — the spec writer estimates 4–8 weeks of focused effort to reach feature parity with the reference, plus inevitable test cycles against a real TS6 server. By contrast, integrating a third-party library (`tsclientlib`) is typically 1–3 days. The implementer should weigh this trade-off explicitly.

### 19.16 How to verify

- Connect a music bot to a real TS6 server with WebQuery enabled. Observe the bot in the upstream's `clientlist`; its `client_unique_identifier` MUST be a stable 28-character base64 (the UID derived per §19.6.2).
- Capture the UDP traffic (with permission, on a server you control). The first packets MUST be `Init1` (type 8) with `FLAG_UNENCRYPTED` set, and MUST contain `INIT_MAC = "TS3INIT1"` as the leading 8 bytes.
- Force the bot to send a track. Capture the `Voice` (type 0) packets. Their bodies after the 1-byte codec-id MUST decode as valid Opus frames at 48 kHz stereo.
- Disable network mid-track; the bot's pacing MUST not crash, the bot's resend map MUST start to grow, and after the inactivity timeout the bot MUST emit `disconnected`. Restoring the network MUST trigger reconnect (§18.3).

---

## Chapter 20. Audio Pipeline

### 20.1 Format and constants

All audio sent over the bot's voice connection is **Opus**-encoded **stereo** PCM at **48 kHz** in **20 ms** frames.

| Constant | Value |
|---|---|
| Sample rate | 48000 Hz |
| Channels | 2 (stereo) |
| Sample format | 16-bit signed little-endian PCM |
| Frame size (samples per channel) | 960 |
| Frame duration | 20 ms |
| Bytes per frame (PCM, both channels) | `960 × 2 × 2 = 3840` |
| Opus bitrate | 96 kbps |

These values are external-contract-equivalent: the TeamSpeak voice server expects 48 kHz Opus stereo (codec id 5, "Opus Music"); deviating breaks audio.

### 20.2 Decoding (FFmpeg subprocess)

Both file playback and stream playback decode through FFmpeg as a subprocess. The implementation MUST:

- Spawn `ffmpeg` with `shell: false` (no shell injection).
- Common args: `-f s16le -acodec pcm_s16le -ar 48000 -ac 2 -loglevel error pipe:1`.
- For files: pass `-i <local-path>` as the only input.
- For HTTP/HTTPS streams (radio): also pass `-reconnect 1 -reconnect_streamed 1 -reconnect_delay_max 5` *before* `-i`.

Buffered file decoding accumulates the full PCM into a `Buffer` and resolves a Promise when FFmpeg exits with code 0. Streaming decoding resolves immediately with the FFmpeg `stdout` Readable, the child-process handle, and a `kill()` function — the consumer pulls bytes as needed.

### 20.3 Frame splitting

The PCM buffer is split into `BYTES_PER_FRAME`-sized chunks. The trailing partial frame, if any, MUST be zero-padded to a full frame (silence) rather than dropped — to preserve track length.

For streams, frames are pulled from a chunk-buffer queue (see §20.5) which can supply a full frame's worth of bytes only when ≥ 3840 bytes have arrived. Until then, the playback tick MUST emit silence or skip (the reference skips and tries again next tick).

### 20.4 Opus encoding

The Opus encoder MUST be configured for **music**, not VOIP:

- Application: `AUDIO` (encoder constructor, not `VOIP`).
- Bitrate: 96 kbps.
- Mode: **hard CBR** (`OPUS_SET_VBR = 0`). VBR-constraint MAY be set but is ignored under CBR.
- Signal hint: `OPUS_SIGNAL_MUSIC` (CTL `OPUS_SET_SIGNAL = 4024`, value `3002`).

Each `encodeFrame(pcmFrame, volume)` call:

1. If `volume < 100`, scale every 16-bit signed sample by `volume/100`, clamping to `[-32768, 32767]`. The scaling produces a fresh buffer of the same length.
2. Encode through Opus, returning a variable-length frame buffer (typically 240–600 bytes for 96 kbps music).

### 20.5 Pacing — the "audio clock"

Audio is sent on a **strict 20 ms cadence**. The reference uses a `nextDue` timestamp pattern (the "audio clock"); the implementation MUST follow the same approach to avoid the two failure modes:

- **Under-pacing** (sleeping too long → audible gaps and pitch drift in the listener).
- **Burst-sending** (sleeping zero between many frames → upstream sees several frames at the same time, dropped or rate-limited).

Algorithm:

```
nextDue = now()
loop:
    if now() < nextDue:
        sleep(nextDue - now())
        continue
    lag = now() - nextDue
    if lag >= FRAME_MS:
        skip = floor(lag / FRAME_MS)
        // Skip data, not time — never burst-send to "catch up"
        frameIndex = min(frameIndex + skip, frames.length)
        nextDue = now() + FRAME_MS
    encode-and-send frame[frameIndex]
    frameIndex++
    nextDue += FRAME_MS
    if now() - nextDue > 5 * FRAME_MS:
        // Pathological lag (process pause, GC) — resync rather than catch up
        nextDue = now() + FRAME_MS
```

Each send updates a per-bot **loop epoch** counter; before any tick proceeds, it compares the current epoch against the captured one and exits early if they differ. This is the cancellation pattern that lets `pause`, `seek`, `skip`, and `stop` reliably halt a tick callback that is already scheduled.

For stream playback, the loop additionally:

- Opens with a 200 ms initial-buffer delay (the FFmpeg subprocess needs time to produce its first PCM bytes).
- Pulls one frame's worth of bytes from the chunk-buffer queue per tick. If insufficient bytes are available, the tick emits no voice frame and moves to the next slot.

### 20.6 Volume

Volume is an integer in `[0, 100]`. Setting the volume on a running bot MUST take effect immediately (the next encoded frame is scaled by the new factor). Persisting the volume to `MusicBot.volume` is a separate concern handled by the route (see §7.21.3).

### 20.7 Now-playing nickname

When a track starts playing, the bot MUST update its TS3 client nickname to `"<original> ♪ <title>"` (`U+266A` "EIGHTH NOTE", padded with single spaces). The composite nickname MUST NOT exceed **30 characters**; if it would, the title portion is truncated and ended with `U+2026` "…". When playback stops (skip/queue-empty/manual stop), the nickname MUST be reset to the original.

### 20.8 ICY metadata polling

For radio streams (any `playStream` invocation), the bot MUST poll the underlying URL for ICY metadata every **15 seconds** (with an immediate first poll). The polling logic:

- Parse the URL and refuse if the host resolves to a private IP (SSRF guard re-check; see §6.7).
- Open a raw TCP (or TLS for `https`) connection.
- Send an HTTP/1.0 GET with headers:
  - `Host: <host>`
  - `User-Agent: TS6-MusicBot/1.0` (any string is acceptable; avoiding default Node UA prevents some upstreams from refusing)
  - `Icy-MetaData: 1`
  - `Connection: close`
- Read the response. If the status line is `30[12378]`, follow `Location:` (max 5 redirects).
- Find the `icy-metaint: <N>` header. If absent, abandon (this stream lacks ICY metadata).
- After the header block, skip exactly `N` bytes of audio data. Read 1 byte; metadata length is `byte × 16`. If 0, no metadata at this position; abandon. Otherwise read `metaLen` bytes of metadata.
- Search the metadata for `StreamTitle='<...>'`. Extract; trim. If the title differs from the last observed title, emit a `metadataChange` event with parsed `Artist - Title` (split on first ` - `).
- Close the socket.

Polling MUST be tolerant of every error path (timeout, DNS failure, malformed headers, connection close without ICY): silently abandon this poll and try again on the next interval. The poll timeout is **10 seconds**.

### 20.9 yt-dlp integration

The bot subsystem invokes `yt-dlp` as a subprocess for three operations:

- **Search:** `yt-dlp ytsearch<N>:<query> --dump-json --flat-playlist --no-download` (one JSON object per stdout line).
- **URL info:** `yt-dlp <url> --dump-json --flat-playlist --no-download` — returns one JSON line per video (one for a single-video URL, multiple for a playlist URL).
- **Audio download:** two-step:
  1. `yt-dlp <url> --dump-json --no-playlist` to get `{id, title, uploader|channel, duration, thumbnail}`.
  2. `yt-dlp <url> -x --audio-format opus --audio-quality 0 --no-playlist -o "<music-dir>/%(id)s.%(ext)s"`. If `<id>.opus` already exists, skip the download.
- **Direct video URL resolution** (for video streaming sources): `yt-dlp <url> -f "best[height<=<H>][ext=mp4]/best[height<=<H>]/best[ext=mp4]/best" --no-playlist -g`. The first stdout line is the resolved direct URL.

If a yt-dlp cookies file is configured (env var or operator-uploaded), the implementation MUST pass `--cookies <path>` on every invocation.

The implementation MUST run yt-dlp with `shell: false` (no shell injection). Operator-supplied URLs MUST pass §6.7 validation before being handed to yt-dlp.

### 20.10 How to verify

- Decode a 5-minute MP3 to PCM; the resulting buffer MUST be approximately `300 × 48000 × 2 × 2 = 57.6 MB`.
- Encode 100 silence frames at volume `50`; verify each output is a non-empty, valid Opus frame.
- Connect a real TeamSpeak voice client; play a 30-second track; the listener MUST hear continuous audio with no audible burst-pacing artefacts (no fast-then-slow segments).
- Stream a known ICY-tagged radio (e.g., a SHOUTcast endpoint) and observe `metadataChange` events at roughly the rate the station rotates titles.

---

## Chapter 21. Playlist, Queue, and Music Library

### 21.1 Per-bot queue model

Each running voice-bot owns a single in-memory queue. The queue holds `QueueItem` records:

```
QueueItem {
  id:        <string>      // stable id ("song_<dbid>", "yt_<videoid>", "radio_<dbid>")
  title:     <string>
  artist:    <string?>
  duration:  <seconds?>    // omitted for radio (live stream)
  filePath:  <string>      // absolute, under MUSIC_DIR (empty for radio)
  source:    "local" | "youtube" | "url" | "radio"
  sourceUrl: <string?>     // origin URL, if any
  streamUrl: <string?>     // present iff this is a live stream
}
```

`currentIndex` starts at `-1` (queue empty / nothing playing). `index` after `playAt(N)` becomes `N` and `current` returns `items[N]`.

### 21.2 Operations

| Operation | Behaviour |
|---|---|
| `add(item)` | append. If shuffle is on, also splice into a random position of the shuffle order. |
| `addMany(items)` | each item via `add`. |
| `remove(id)` | delete by `id`. Adjust `currentIndex` to follow surviving items. |
| `clear()` | delete all; reset `currentIndex = -1`. |
| `next()` | advance `currentIndex`. If past end and `repeat="queue"`, wrap to 0 (and regenerate shuffle order). Else `currentIndex = -1` and return `null`. |
| `previous()` | step `currentIndex` back. If below 0 and `repeat="queue"`, wrap to last. Else clamp to 0. |
| `playAt(index)` | jump-to. |
| `move(from, to)` | reorder within the queue. Adjust `currentIndex` to follow the moved item. |
| `setRepeat(mode)` | `off` / `track` / `queue`. |
| `setShuffle(b)` | toggle. On enable, regenerate shuffle order via Fisher-Yates. |

### 21.3 Repeat-mode interaction

- **`off`** — `next()` returns `null` past the end; the bot stops playback and resets the nickname.
- **`track`** — the queue does NOT detect this mode; the *bot* does (its end-of-track handler re-plays the current item if `repeat=="track"`).
- **`queue`** — the queue wraps around, and shuffle order is regenerated on wrap (so a re-played queue shuffles differently each lap).

### 21.4 Shuffle implementation

The queue stores items in their **insertion order**; shuffle is layered on top via a `shuffleOrder` array of indices. Methods that consult "the current item" go through `shuffleOrder` when shuffle is on. New items added under shuffle are inserted at a random position in `shuffleOrder`. Removal updates `shuffleOrder` to drop the removed index and shift remaining indices appropriately. `move` regenerates the shuffle order entirely.

Fisher-Yates is the only randomisation algorithm specified; the implementation MUST NOT use `Math.random()` for cryptographic purposes (this is queue ordering, not secrets — `Math.random` is fine here).

### 21.5 Music library and playlist persistence

The `Song` and `Playlist` entities (§4.2.11–§4.2.13) define the library. The runtime queue is *not* persisted: stopping a bot loses its queue. Operators wanting a persistent queue use a `Playlist` and load it with `POST /music-bots/:id/queue/playlist`. The implementation MAY add per-bot queue persistence as a quality-of-life feature; the reference does not.

### 21.6 Music request history (`MusicRequest`)

Every URL queued through a music bot (via `!play`, `!queue <url>`, `POST /music-bots/:id/play-url`, or the chat handler's deduplicating path) MUST be upserted into `MusicRequest`:

- Key: `(serverConfigId, url)`.
- On hit: update `requestedAt = now`, `title = <new>`.
- On miss: insert.

This produces a deduplicated, recency-sorted history of what has been played on each server. The route `GET /servers/:configId/music-requests` returns the latest 100.

### 21.7 How to verify

- Add three songs in shuffle-off mode; play; the order MUST equal insertion order.
- Toggle shuffle; the order MUST change but every song MUST still appear exactly once.
- Set `repeat=queue`; reach the end of a 3-song queue; the queue MUST wrap to song 1.
- Move song 3 to position 1 while song 2 is playing; `currentIndex` MUST still point at song 2 after the move.
- Re-queue the same YouTube URL; the `MusicRequest` row MUST be upserted (one row, updated `requestedAt`).

---

## Chapter 22. In-Channel Chat Commands

### 22.1 Mechanism

Music-bot chat commands are delivered **directly through the bot's TS3 voice client connection**, not through the SSH event bridge. When the bot is connected and joined to a channel, the upstream TS server delivers `notifytextmessage` events on the voice connection itself (because the bot is now a real client, registered for chat in its current channel). The implementation MUST register a `textMessage` listener on the voice client and route messages whose `msg` starts with `!` and matches one of the recognised commands to the appropriate handler.

Commands sent by the bot itself (e.g., its own replies) MUST be ignored. The implementation MUST detect this by comparing the message's `invokerid` to the bot's own `clientId`.

### 22.2 Recognised command set

The implementation MUST recognise the following lower-case command tokens after the `!` prefix:

```
radio  play  stop  pause  skip  next  prev
vol    volume     np    nowplaying    queue   add
stream    stopstream    viewers
```

Unknown tokens after `!` MUST be silently ignored (the bot is not the only listener of `notifytextmessage`).

### 22.3 Per-command behaviour

| Token(s) | Behaviour |
|---|---|
| `radio` (no args) | Reply to invoker with a numbered list of `RadioStation` rows for this bot's server (alphabetical by name). |
| `radio <id>` | Look up `RadioStation` by id; if found, `playStream` it; reply `Now playing: <name>`. |
| `play` (no args) | If paused, resume. Else reply `Usage: !play <youtube-url>`. |
| `play <url>` | yt-dlp-download; if nothing is playing, play it; else queue it. Reply with status. Save to `MusicRequest`. |
| `stop` | Stop audio (clear pacing; bot stays connected). Reply `Playback stopped.`. |
| `pause` | If playing, pause; if paused, resume; else reply `Nothing is playing.`. |
| `skip` / `next` | Advance the queue; if next exists, play it (file or stream as appropriate). Else stop and reply. |
| `prev` | Step back; play if exists. |
| `vol` / `volume` (no args) | Reply with current volume. |
| `vol N` / `volume N` | Set volume to integer in `[0, 100]`; reject otherwise. |
| `np` / `nowplaying` | Reply with current track. |
| `queue` / `queue show` | Reply with up to 15 lines, current track marked `▶`. |
| `queue play N` | Jump-to-index (1-based). |
| `queue remove N` | Remove by index (1-based). |
| `queue clear` | Empty. |
| `queue <url>` | Same as `play <url>` but never interrupts current playback. |
| `stream <url> [preset]` | Start (or change source of) a video stream. |
| `stopstream` | Stop the active video stream. |
| `viewers` | Reply with current video-viewer list. |

### 22.4 Reply mechanism

All replies are sent as **private messages** (`targetmode=1`) to the invoker's `clid`, not as channel messages. This avoids spamming the channel for queue listings or error messages. Long replies (queue listings, station listings) MUST be a single multi-line message; the implementation MUST not split into multiple sends (TS3 chat tolerates newlines within a single sendtextmessage).

### 22.5 Authorisation

The reference does not enforce per-command operator-level authorisation; any client present in the bot's channel can invoke any command. The implementation MAY add an authorisation layer (e.g., based on TS3 server-group membership), but MUST not change the default no-auth behaviour without an explicit operator opt-in (it would break the "drop a bot in a public channel" UX). At minimum, the implementation SHOULD make destructive commands (`stop`, `stopstream`, `queue clear`) reject when the invoker is not in the bot's current channel; the reference does not currently make this check, so it is a reasonable hardening.

### 22.6 How to verify

- Connect a real client to the bot's channel; type `!np` while the bot is idle; receive `Nothing is playing.` as a private message.
- Type `!play https://youtu.be/...`; the bot MUST download (via yt-dlp), enqueue, and play; private reply lists the now-playing line.
- Type `!radio`; receive a numbered list. Type `!radio 3`; the bot MUST switch to that station.
- Send a chat message that starts with `!nonsense`; the bot MUST NOT respond.

---

# Part VII — Video Streaming

## Chapter 23. Video Streaming Architecture

### 23.1 Roles

A live video stream from the bot's perspective involves four cooperating processes:

| Process | Role |
|---|---|
| **Back-end (TS3 voice client)** | Talks the TS3 voice protocol on behalf of the bot. Issues `setupstream` to the upstream TS server, receives `notifyjoinstreamrequest` for each viewer, and forwards offer/answer/ICE between viewers and the sidecar via `streamsignaling`. |
| **Sidecar (Go process)** | The WebRTC media relay. Owns the FFmpeg subprocess that ingests the source URL, owns the inbound RTP UDP listeners, owns one outbound `RTCPeerConnection` per viewer, and computes the playout pacing/sync. |
| **FFmpeg subprocess** | Spawned by the sidecar. Reads the source (file/URL), encodes to VP8 video + Opus stereo audio, emits RTP packets to the sidecar's loopback UDP listeners. |
| **Viewer browser (a real TS6 client)** | Receives the WebRTC stream, plays it back. From the viewer's POV the stream is delivered exactly as it would be from any other TS6 streaming client. |

The back-end never touches RTP traffic; the sidecar never touches the TS3 voice protocol. The two coordinate through the sidecar's HTTP control plane (Ch 24.1) and through TS3 `streamsignaling` notifications relayed by the back-end (Ch 24.2).

### 23.2 Why a separate process

The sidecar is a separate Go process for two reasons:

1. **WebRTC stack maturity.** The Pion (Go) library is more battle-tested for SFU-style relay than the Node-WebRTC ecosystem at the time of writing. An implementation MAY substitute a Rust WebRTC stack (`webrtc-rs`) or any other implementation; the only contract is the HTTP control plane (Ch 24) and the on-the-wire WebRTC behaviour (codec, SSRC, RTCP).
2. **Decoupling restart cycles.** Restarting the back-end (e.g., for a deploy) does not interrupt active video streams. The sidecar can be left running, viewers stay connected, and the back-end re-attaches via the control HTTP API.

### 23.3 Deployment modes

The implementation MUST support two deployment modes:

- **External sidecar:** The operator runs the sidecar as a standalone container or process. The back-end discovers it via the `SIDECAR_URL` env var (e.g., `http://ts6-sidecar:9800`). The back-end MUST NOT spawn the sidecar in this mode.
- **Bundled sidecar:** When `SIDECAR_URL` is unset, the back-end MAY spawn the sidecar binary directly. The operator points to the binary via `SIDECAR_BINARY_PATH`. Reference deployments ship a Linux binary at `bin/sidecar-linux` and a Windows binary at `bin/sidecar.exe`.

### 23.4 Quality presets

The implementation MUST expose three quality presets driving FFmpeg encoding parameters:

| Preset | Resolution | Framerate | Bitrate |
|---|---|---|---|
| `480p` | 854×480 | 24 fps | 1000k |
| `720p` *(default)* | 1280×720 | 30 fps | 2500k |
| `1080p` | 1920×1080 | 30 fps | 4500k |

The default preset is `720p`. Per-bot the default is configurable via `MusicBot.streamPreset`. Presets are external-contract identifiers (their names appear in REST request bodies); the implementation MUST preserve the strings `480p`, `720p`, `1080p`.

### 23.5 Stream lifecycle

Per `POST /music-bots/:id/stream/start`:

1. Resolve preset/framerate/bitrate (defaults from the preset table).
2. Bring up sidecar:
   - **External mode:** construct the HTTP client; call `waitHealthy()` (poll `/health` every 200 ms up to 6 s).
   - **Bundled mode:** spawn the binary with the resolved env vars; wait for healthy.
3. Construct a `StreamSignaling` adaptor over the bot's TS3 voice client; subscribe to TS3 events (`channel`, `server`, `textchannel`).
4. Issue TS3 `setupstream` (Ch 24.2) with name `<bot-nickname> Stream`, type `3` (video), bitrate `4608`, accessibility `1`, mode `1`, viewer limit `0`, audio `1`. Wait up to 10 s for the upstream's `notifystreamstarted` whose `clid` matches the bot's; on timeout, fail.
5. Resolve the source URL: if it is a YouTube/Twitch/yt-dlp-supported URL, run `yt-dlp -g -f "best[height<=<H>][ext=mp4]/best[height<=<H>]/best[ext=mp4]/best" <url>` to get a direct media URL; otherwise pass through.
6. POST the resolved URL plus dimensions/framerate/bitrate to the sidecar's `/source` endpoint, which spawns FFmpeg internally.
7. Mark the bot as actively streaming; emit `videoStreamStarted` upward.

Per `POST /music-bots/:id/stream/stop`:

1. For each active viewer, emit TS3 `removeclientfromstream` and call sidecar `/peer/close` with the viewer's `clid` as id.
2. Call sidecar `/source/stop` to halt FFmpeg and drain RTP queues.
3. Issue TS3 `stopstream` (with reason code `1`) so the upstream tells viewers the stream is over.
4. Wait ~1 s (let the UDP-side stop ack reach the upstream).
5. **Bundled mode only:** SIGTERM the sidecar binary; SIGKILL after 3 s if still alive.
6. Emit `videoStreamStopped` upward.

### 23.6 How to verify

- Start a stream from a YouTube URL with a real TS6 client connected to the bot's channel; the client MUST see the stream offered and be able to join it.
- Restart the back-end while a viewer is connected (and the sidecar is in external-mode); the viewer's video MUST continue streaming during the back-end downtime.
- Switch source on a live stream via `POST /music-bots/:id/stream/source`; viewers MUST see the new content within ~3 seconds (the time for FFmpeg to re-establish the input).

---

## Chapter 24. Sidecar Control Plane

The sidecar is reachable on its HTTP control port (default `9800`) by the back-end only. The back-end uses two distinct control surfaces:

- **HTTP API on the sidecar** (this chapter, §24.1).
- **TS3 `streamsignaling` over the bot's voice connection** (this chapter, §24.2). This carries the WebRTC offer/answer/ICE between the sidecar and the viewer's browser, *via* the upstream TS server, since browsers' TS6 clients receive signaling through the TS3 protocol rather than over a direct WebSocket.

### 24.1 Sidecar HTTP API

Routes are documented normatively in §3.10. Behavioural notes:

| Route | Notes |
|---|---|
| `POST /peer/create` | Idempotent per `id`. If a create for this `id` is already in flight, MUST block on the in-flight result and return its outcome (so two callers don't race to create two peers). If a peer for this `id` already exists in non-terminal ICE state and has a `LocalDescription`, MUST return that existing offer's SDP. Otherwise MUST construct a new `RTCPeerConnection` with: VP8 video PT 96, Opus stereo PT 111, default Pion-style interceptors plus an `intervalpli` receiver interceptor, ICE servers from the configured STUN list. Adds local video and audio tracks; on `OnICEConnectionStateChange` resolves the SSRCs from the sender's encoding params. Returns the local-description SDP after gathering completes. |
| `POST /peer/answer` | Sets the remote description as type `answer`. MUST tolerate duplicate calls (already-stable signaling state) by returning OK without erroring. MUST tolerate calls in the wrong state (e.g., the peer is already past local-offer) by silently ignoring. |
| `POST /peer/ice` | Adds an ICE candidate to the named peer. |
| `POST /peer/close` | Tears down the peer. Active flag set false; SR-sender goroutine signaled; `RTCPeerConnection.Close()`. |
| `POST /source` | Stops any current FFmpeg, resets sync timing, drains RTP queues, resets per-peer "started" flags, then spawns a new FFmpeg process. Detailed FFmpeg invocation in §24.1.1. |
| `POST /source/stop` | Stops FFmpeg and resets the same state as `/source` does on entry. |
| `GET /stats` | Returns peer count, per-peer state, current source URL. |
| `GET /health` | Returns `{"status":"ok", "videoPort":N, "audioPort":N}`. |

#### 24.1.1 FFmpeg invocation

The sidecar spawns FFmpeg with the path resolved from `FFMPEG_PATH` (default `ffmpeg`). For an HTTP/HTTPS source URL the args include reconnect support; for a non-URL source, infinite-loop input. For an empty source the sidecar generates a black canvas. Encoding args (the order matters; quoting omitted in the table for readability):

```
-pix_fmt yuv420p
-c:v libvpx -cpu-used 6 -deadline realtime -lag-in-frames 0 -error-resilient 1
-b:v <bitrate> -maxrate <bitrate> -bufsize <VIDEO_BUFSIZE>
-keyint_min 15 -g 15 -auto-alt-ref 0
-payload_type 96 -ssrc 11111111 -f rtp rtp://127.0.0.1:<videoPort>
```

When the source has audio (`source != ""`), additional audio args:

```
-map 0:a:0?
[-af adelay=delays=<AUDIO_DELAY_MS>:all=1]   # only when AUDIO_DELAY_MS > 0
-c:a libopus -b:a <AUDIO_BITRATE> -ar 48000 -ac 2
-payload_type 111 -ssrc 22222222 -f rtp rtp://127.0.0.1:<audioPort>
```

Hard-coded SSRCs (`11111111` video, `22222222` audio) are the contract between FFmpeg and the sidecar's RTP demuxing layer; the implementation MUST emit these exact SSRC values.

`-keyint_min 15 -g 15` produces a keyframe every 15 frames (~0.5 s at 30 fps), which keeps the join-latency low: a viewer that connects mid-stream waits at most 15 frames for the next keyframe.

The video filter chain, applied when source is set:

```
fps=<framerate>,scale=<W>:<H>:force_original_aspect_ratio=decrease,
pad=<W>:<H>:(ow-iw)/2:(oh-ih)/2,format=yuv420p
```

(Letterbox/pillarbox padding to preserve aspect ratio.)

For URL sources the implementation MUST prepend `-reconnect 1 -reconnect_streamed 1 -reconnect_delay_max 5 -fflags +genpts+discardcorrupt -re -i <url>`; for non-URL sources, `-stream_loop -1 -fflags +genpts+discardcorrupt -re -i <path>`.

### 24.2 TS3 stream-signaling sub-protocol

Inside the TS3 voice protocol's command stream (§19.x), the upstream TS server delivers a small set of stream-related notifications and accepts a small set of stream-related commands. The back-end's `StreamSignaling` adaptor implements this, layered on the bot's voice client.

#### 24.2.1 Outgoing commands

| Command | Parameters | Effect |
|---|---|---|
| `setupstream` | `name=<...> type=3 bitrate=4608 accessibility=1 mode=1 viewer_limit=0 audio=1` | Register a new outbound stream owned by this client. The upstream returns `notifystreamstarted` with a server-assigned `id`. |
| `respondjoinstreamrequest` | `id=<streamId> clid=<viewerClid> msg= offer=<sdp> decision=1` | Accept a viewer's join request and hand them the WebRTC offer SDP. `decision=0` rejects. |
| `streamsignaling` | `id=<streamId> clid=<viewerClid> json=<encoded JSON>` | Send a free-form WebRTC signaling payload to one viewer. The `json` field is a JSON object `{cmd, args}` (see §24.2.3). |
| `stopstream` | `id=<streamId> reason=1` | Tear down the stream. Reason `1` is "normal stop". |
| `removeclientfromstream` | `id=<streamId> clid=<viewerClid>` | Force-disconnect a viewer. |

#### 24.2.2 Incoming notifications

| Notification | Meaning |
|---|---|
| `notifystreamstarted` | Upstream confirms a new stream is registered. Carries `id`, `clid` (the owner — should match this bot), `name`, `type`, `access`, `mode`, `bitrate`, `viewer_limit`, `audio`. |
| `notifystreamstopped` | The stream has ended. Carries `id` (and possibly `clid`). |
| `notifyjoinstreamrequest` | A viewer (`clid`) wants to join the stream `id`. The bot MUST respond with `respondjoinstreamrequest` after creating a peer at the sidecar. |
| `notifyrespondjoinstreamrequest` | Echo of the bot's own response (decision + offer SDP, if accepted). The signaling layer translates `decision=1` with offer into an "offer" upstream signaling event. |
| `notifystreamsignaling` | Carries a JSON-encoded `{cmd, args}` in the `json` parameter (see §24.2.3). |
| `notifystreamclientjoined` | Viewer's WebRTC connected. |
| `notifystreamclientleft` | Viewer's WebRTC disconnected. |
| `notifystreaminfo` | Periodic info about a stream (used to populate active-streams map). |

#### 24.2.3 Inner JSON `cmd` set

`streamsignaling` and `notifystreamsignaling` carry an inner JSON envelope:

```
{ "cmd": "<offer|reconnectOffer|answer|iceCandidate|reconnect>",
  "args": <object> }
```

| `cmd` | `args` shape | Meaning |
|---|---|---|
| `offer` | `{ offer | sdp: <SDP string> }` | The WebRTC offer SDP. Sent by the bot to viewer (via `notifyrespondjoinstreamrequest` with `decision=1, offer=<SDP>`) when a viewer is accepted. |
| `reconnectOffer` | same as `offer` | Same payload, but the viewer is reconnecting. |
| `answer` | `{ answer | sdp: <SDP string> }` | The viewer's WebRTC answer SDP. |
| `iceCandidate` | `{ sdp \| candidate, mid \| sdp_mid, mLine \| sdp_mline_index }` | One ICE candidate. |
| `reconnect` | `{}` | Viewer is requesting a fresh peer. Bot closes the existing sidecar peer and re-handles the join. |

The implementation MUST tolerate both naming styles (`offer`/`sdp`, `mid`/`sdp_mid`, `mLine`/`sdp_mline_index`) for compatibility with either side of the upstream.

### 24.3 End-to-end join sequence (one viewer)

```
viewer browser           upstream TS server                bot back-end                         sidecar
     │                          │                              │                                  │
     │  joinstreamrequest        │                              │                                  │
     ├──────────────────────────►│ notifyjoinstreamrequest      │                                  │
     │                          ├─────────────────────────────►│ POST /peer/create {id=<clid>}    │
     │                          │                              ├─────────────────────────────────►│
     │                          │                              │                {sdp: <offer>}    │
     │                          │                              │◄─────────────────────────────────┤
     │                          │ respondjoinstreamrequest      │                                  │
     │                          │   id=<sid> clid=<clid>        │                                  │
     │                          │   decision=1 offer=<sdp>      │                                  │
     │                          │◄─────────────────────────────┤                                  │
     │ notifyrespondjoinstreamrequest (offer)                  │                                  │
     │◄──────────────────────────┤                              │                                  │
     │  streamsignaling (answer)                                                                    │
     ├──────────────────────────►│  notifystreamsignaling (cmd=answer)                              │
     │                          ├─────────────────────────────►│ POST /peer/answer {id, sdp}      │
     │                          │                              ├─────────────────────────────────►│
     │  streamsignaling (iceCandidate) (multiple)                                                    │
     ├──────────────────────────►│  notifystreamsignaling (cmd=iceCandidate)                        │
     │                          ├─────────────────────────────►│ POST /peer/ice {...}             │
     │                          │                              ├─────────────────────────────────►│
     │                                                                                              │
     │                          ICE connectivity established (UDP, viewer ↔ sidecar)                │
     │ ◄═══════════════════════════════════════════════════════════════════════════════════════════│
     │                          │                              │                                  │
     │                          notifystreamclientjoined        │                                  │
     │                          ─────────────────────────────► │                                  │
     │                          (viewer is now in the viewers map)
```

### 24.4 How to verify

- Wireshark a viewer joining a live stream: confirm the offer SDP delivered to the viewer carries VP8 PT 96 + Opus PT 111 + at least one ICE candidate.
- Force a duplicate `peer/create` race (issue two simultaneous `notifyjoinstreamrequest` for the same `clid`). The sidecar MUST return the same SDP both times; only one peer MUST exist at the end.
- Send an `answer` for a peer that is already stable: the sidecar MUST silently accept (return OK), not error.

---

## Chapter 25. Media Plane (RTP, Codecs, Pacing, A/V Sync)

### 25.1 Codec negotiation

The sidecar advertises exactly two codecs in its WebRTC offer:

- **Video:** VP8, payload type **96**, clock rate **90000 Hz**.
- **Audio:** Opus, payload type **111**, clock rate **48000 Hz**, channels **2**.

Both numbers are external-contract values (they appear in the SDP). Browsers' TS6 clients negotiate VP8 successfully today; if the implementation chooses to add VP9/H.264 support, it MUST do so as additional codecs, not by replacing VP8.

### 25.2 RTP ingress (FFmpeg → sidecar)

On startup, the sidecar binds two UDP listeners on `127.0.0.1` with OS-assigned ports (port `0` → kernel-assigned). FFmpeg writes RTP to those ports per §24.1.1. The chosen ports are exposed via `GET /health` and `GET /stats`.

OS-level read buffers MUST be enlarged to avoid loss under bursty I/O:

| Listener | Reference value |
|---|---|
| Video (`VIDEO_RTP_READ_BUFFER` / `VIDEO_READ_RTP_BUFFER`) | 4 MiB |
| Audio (`AUDIO_RTP_READ_BUFFER` / `AUDIO_READ_RTP_BUFFER`) | 1 MiB |

The reads run in dedicated goroutines (or equivalent threads) per listener.

For each received packet:

- Unmarshal as RTP. Skip packets that fail to unmarshal.
- Track the latest-seen RTP timestamp atomically (used for RTCP Sender Reports, §25.5).
- Increment per-track packet- and octet-counters (atomically).
- **Clone** the packet (re-marshal/unmarshal) — the receive buffer is reused for the next read, so the queue MUST own its own copy.
- Push the cloned packet onto the per-track forward queue (default sizes: video 2048, audio 4096; configurable). On overflow, drop and log.

### 25.3 Per-peer forwarding

Two more goroutines (one per track) drain the queues and forward frames to active peers:

- For each packet:
  1. **Pacing** (§25.4): compute the per-track adaptive delay; sleep that long.
  2. For each peer (under read-lock):
     - For **video**: if the peer is `Active` but not yet `Started`, **and** this packet's payload begins a VP8 keyframe (per the inline VP8 payload-descriptor parser), set `Started = true`.
     - If `Active && Started`, write the RTP packet to the peer's local track. Errors are swallowed (packet drop is preferable to halting forwarding).

The keyframe gate ensures viewers never see a "torn" mid-keyframe state on join: the peer's stream is held closed (no packets forwarded) until the next keyframe.

Per-peer state machine:

```
new peer created  →  Active=false, Started=false
ICE connected     →  Active=true,  Started=false (SSRCs resolved here)
first VP8 keyframe (after Active) → Started=true (forward video and audio thereafter)
ICE disconnected/closed/failed   → Active=false, Started=false
```

The implementation MUST resolve the SSRCs from `RTCRtpSender.GetParameters().Encodings[0].SSRC` at the moment of `ICEConnectionStateConnected` — these values are required for the periodic Sender Reports (§25.5) and are not valid before negotiation completes.

### 25.4 Adaptive playout pacing

The sidecar implements a **per-track latency-EWMA** pacing scheme:

- Each track maintains: `initialized` flag, `baseRTP` (the first RTP timestamp seen), `latency` (smoothed observed latency).
- A shared "stream base wall clock" is set on the first packet of either track.
- For an arriving packet at wall-time `now` with timestamp `ts`:
  ```
  mediaElapsed   = (ts - baseRTP) * 1s / clockRate
  expectedWall   = streamBaseWall + mediaElapsed
  observedLatency = max(now - expectedWall, 0)
  latency        = (latency * 9 + observedLatency) / 10        // single-pole EWMA
  targetLatency  = max(this.latency, other-track.latency)      // align audio + video
  targetWall     = expectedWall + targetLatency + syncBuffer + (videoBias if track == video)
  delay          = max(targetWall - now, 0)
  sleep(delay)
  ```
- `syncBuffer` defaults to 50 ms in code (the env var `SYNC_PLAYOUT_BUFFER_MS` defaults to `4` in the README, but the in-source default is `50 ms` if the env var is unset; **the implementation should default to a value in the 4–50 ms range and document its choice**). `videoBias` defaults to 0 ms; can be increased via `SYNC_VIDEO_BIAS_MS` for fine-tuning.
- The pacing call is performed once per *new* timestamp (RTP timestamps in VP8 are constant for all packets that comprise a single video frame; the sidecar paces per-frame, not per-packet).

This scheme yields **lip-sync without strict packet-by-packet pacing**: once the latency-EWMA converges, audio and video frames are released to peers at the correct relative wall-clock, regardless of which arrived first or whether FFmpeg burst.

### 25.5 RTCP Sender Reports (A/V correlation)

For every connected peer, the sidecar MUST run a goroutine (or equivalent task) that emits **paired** RTCP Sender Reports (one for video, one for audio) once per second:

- **NTP timestamp:** identical 64-bit NTP-format value derived from the same `now`. The pair MUST share the NTP value byte-for-byte; the browser's WebRTC stack uses this to align the two RTP clocks.
- **RTP timestamp:** the latest-seen RTP timestamp for each track, atomically read at SR-emit time.
- **Packet/octet counters:** atomically read at SR-emit time.
- **CNAME (in SDES):** an identical string for both video and audio (reference: `"ts6-stream"`). The browser uses CNAME equivalence as the signal to align the two streams for lip-sync.
- **SSRCs:** the SSRCs resolved at ICE-connected time.

If a track's SSRC is still `0` (not yet resolved), the implementation MUST skip that track's SR for this tick (do not emit an SR with SSRC=0). The next tick will retry.

### 25.6 VP8 payload-descriptor parsing (keyframe gate)

The VP8 RTP payload begins with an optional payload descriptor that must be parsed to find the start of the frame. The implementation MUST:

1. Parse the first byte: `X` (extended), `S` (start of partition), `PID` (partition id).
2. Reject (not a keyframe start) unless `S == 1 && PID == 0`.
3. If `X == 1`, walk the optional extension fields (`I` → 1 or 2 bytes for PictureID; `L` → 1 byte; `T` or `K` → 1 byte).
4. After the descriptor, the first byte of the *frame tag* has bit 0 = 0 if the frame is a keyframe (or 1 if interframe). Return whether it is a keyframe.

Reference is the VP8 RFC; the parser need only handle as much as required for the keyframe-start detection (no need to extract VP8 picture id or other fields).

### 25.7 STUN

The default STUN configuration is a list of 9 servers shipped with the sidecar (8 anonymous IPs plus Google's public STUN). Operators MAY override via `STUN_SERVERS` (comma-separated list of `stun:host:port`). An implementation MAY ship a different default list provided that:

- The default contains at least one publicly reachable STUN entry.
- The override is honoured.
- The list is presented in `RTCConfiguration.iceServers` in offer-creation.

The implementation MAY also support TURN; the reference does not. Adding TURN means accepting the relay traffic cost and is left to operator deployment.

### 25.8 Configuration knobs (recap)

The full env-var inventory is in §3.3. The four most operationally relevant in production:

| Env var | Tune for |
|---|---|
| `SYNC_PLAYOUT_BUFFER_MS` | Increase if listeners report intermittent A/V desync; decrease for snappier first-frame. |
| `SYNC_VIDEO_BIAS_MS` | Add 4–10 ms if listeners report video runs slightly ahead of audio. |
| `VIDEO_QUEUE_SIZE` / `AUDIO_QUEUE_SIZE` | Increase if `[VIDEO] queue full, dropping packet` warnings appear under load. |
| `STUN_SERVERS` | Required in deployments where the default list is unreachable. |

### 25.9 How to verify

- Connect a VP8-capable WebRTC viewer to a live stream. The first `RTCStats` report MUST show non-zero `bytesReceived` for both audio and video tracks within ~3 seconds.
- Inspect the SDP offered by the sidecar: it MUST advertise VP8 (`a=rtpmap:96 VP8/90000`) and Opus (`a=rtpmap:111 opus/48000/2`).
- Capture the RTCP traffic on the sidecar→peer side: every second MUST contain one video SR and one audio SR with **matching NTP times** and **matching CNAME**.
- Force the FFmpeg input to glitch (drop the network mid-stream); upstream sidecar MUST log queue-full warnings if backlog accumulates, and MUST NOT crash. Restoring the input MUST resume forwarding.
- Verify the keyframe-gate: connect a viewer mid-stream; the viewer MUST NOT see torn frames; first frame is always a keyframe.

---

# Part VIII — Public Widgets

## Chapter 26. Widget Configuration and Token Model

A widget is a public, embeddable representation of a TeamSpeak server's live state. Each widget is a `Widget` row (§4.2.15) with an unguessable URL-safe token. Anyone who knows the token can render the widget; no further auth is required.

### 26.1 Token discipline

- Tokens MUST be **21-character URL-safe random strings** (the reference uses `nanoid(21)`). Implementations MAY use longer tokens; SHOULD NOT use shorter (collision and enumeration risk).
- Tokens MUST be unique (DB-enforced).
- Operators MAY rotate a widget's token at any time via `POST /widgets/:id/regenerate-token`. The old token immediately stops resolving (the public-data cache MUST be invalidated; §7.29).
- Tokens are not bearer-equivalent to operator credentials: leaking one grants only widget-rendering capability, not write access. Operators SHOULD still rotate any token they consider compromised.
- The implementation MUST NOT log full tokens at any level above `debug`.

### 26.2 Visibility flags

Per-widget settings:

| Field | Default | Effect |
|---|---|---|
| `theme` | `dark` | One of the six themes (§3.7). Affects rendered colour palette only. |
| `showChannelTree` | `true` | If false, the rendered widget shows only the header (server name + online count). |
| `showClients` | `true` | If false, the channel tree shows channel names but no per-channel client lists. |
| `hideEmptyChannels` | `false` | If true, channels with no clients (and no clients in any descendant) are pruned. Spacer channels are *always* shown. |
| `maxChannelDepth` | `5` | Channel-tree depth cap. Channels deeper than this MUST have their `children` array set to `[]` (the depth limit is hard, not just visual; nested children aren't sent in JSON either). |

### 26.3 Themes

Six built-in themes. Each is a palette of 8 colours: `background`, `backgroundSecondary`, `border`, `textPrimary`, `textSecondary`, `accent`, `clientColor`, `headerBg`. Theme names: `dark`, `light`, `transparent`, `neon`, `military`, `minimal`. Concrete palette values are spec'd in `WIDGET_THEMES` and MUST be preserved verbatim when reproducing the same visual identity. Implementations MAY add more themes (the type/list is extensible) but MUST NOT rename or remove any of the six.

### 26.4 How to verify

- Create a widget via `POST /widgets`, then `GET /api/widget/<token>/data`; the response JSON MUST round-trip the configured `theme`, `showChannelTree`, `showClients`, `hideEmptyChannels`, `maxChannelDepth`.
- `POST /widgets/:id/regenerate-token`; the old token MUST return 404 immediately on the next public-route call.

---

## Chapter 27. Widget Rendering

### 27.1 Channel-tree assembly

Building the tree from upstream `channellist` + `clientlist`:

1. **Group human clients by channel.** Filter `clientlist` to entries where `client_type == "0"` (skip query clients). For each, push `{clid, nickname, isAway, isMuted}` into a per-cid list.
2. **Build a flat node map.** For each `channellist` entry create a `WidgetChannelNode` with: `cid`, `name`, `hasPassword: channel_flag_password == 1`, `clients: clientsByChannel.get(cid) || []`, `children: []`, plus the spacer fields (§27.2).
3. **Link parents.** For each entry, push the node into its parent's `children` (where `pid == 0` makes it a root).
4. **Prune depth.** Walk the tree; once depth ≥ `maxChannelDepth`, set `children = []` for every node at that depth.
5. **Optionally filter empties.** If `hideEmptyChannels`, recursively drop nodes where `isspacer == false && hasClients(this) == false` (where `hasClients` is true if this node OR any descendant has a non-empty `clients` array).

### 27.2 Spacer detection

TeamSpeak channel names of the form `[<prefix>spacer<n>]<text>` (case-insensitive) are decorative dividers, not real channels. The implementation MUST detect them with the regex:

```
^\[([lcr]?\*?)spacer\d*\](.*)$/i
```

Mapping:

| Prefix capture | Text | `spacerType` |
|---|---|---|
| any | `---` | `dashline` |
| any | `...` | `dotline` |
| any | `___` or `===` or any string of `[=\-_.]` | `line` |
| `c` | other text | `center` |
| `r` | other text | `right` |
| `*`, `l*`, or empty (after the special-text checks above) | other text | `left` |
| (other) | (other) | `left` (default) |

A non-spacer channel sets `isspacer: false`, `spacerType: 'none'`, `spacerText: ''`.

### 27.3 SVG rendering

The implementation MUST render an SVG with:

- Width: **400 px** (fixed).
- Height: dynamic — sum of header (72), tree rows × 22, client rows × 18, footer (28), and padding × 2.
- Rounded outer clip path with `rx=10`.
- Header band at the top, drawn in `theme.headerBg`.
- The body (channel tree) drawn against `theme.background`.

Layout:

```
PADDING            = 14
HEADER_HEIGHT      = 72  (server name + ONLINE badge + stats line)
CHANNEL_ROW height = 22
CLIENT_ROW  height = 18
FOOTER_HEIGHT      = 28  (separator + "TS6 WebUI Widget" caption)
font-family = 'Segoe UI', 'Helvetica Neue', Arial, sans-serif
```

Row content:

- **Header.** Server name in `theme.accent`, weight 700, size 15, x=PADDING. ONLINE badge (filled rectangle in `theme.clientColor`, white text "ONLINE", 52×18) right-aligned. Below: `<users>/<max> users • <uptime> uptime` in `theme.textSecondary`, size 11. Separator line below header in `theme.border`.
- **Spacer row** with `line/dotline/dashline`: `<line>` element across the body width, with `stroke-dasharray` `"2,4"` for dotline, `"6,4"` for dashline, none for line.
- **Spacer row** with text: text element with x=PADDING (left), x=WIDTH/2 + `text-anchor="middle"` (center), or x=WIDTH-PADDING + `text-anchor="end"` (right). `theme.textSecondary`, size 11, weight 600, letter-spacing 0.5.
- **Channel row:** indent per depth = `PADDING + depth × 16`. `#` icon at indent (size 12, weight 700, `theme.accent`), then channel name (size 12, `theme.textPrimary`) at indent+14. Truncate channel name to `36 - depth × 2`. Lock emoji `🔒` right-aligned (in `theme.textSecondary`) when `hasPassword`. Client count at right (in `theme.textSecondary`, size 10, `text-anchor="end"`) when clients present.
- **Client row:** small filled circle (radius 3) at indent+10 in `theme.clientColor`, then nickname text (size 11, `theme.clientColor`) at indent+18, with `[away]`/`[muted]` suffixes if applicable. Truncate to `32 - depth × 2`.
- **Footer:** separator line in `theme.border`, then centered caption "TS6 WebUI Widget" (size 9, opacity 0.6).

All operator-supplied text (`name`, channel names, nicknames) MUST be XML-escaped before insertion (`&`, `<`, `>`, `"`, `'`).

### 27.4 PNG rendering

PNG is produced by rasterising the SVG (the reference uses `@resvg/resvg-js`, fitting to width 400). The implementation MUST:

- Render the SVG (per §27.3).
- Pass it to a SVG-to-PNG rasteriser configured for output width 400 px.
- Return the resulting PNG bytes with `Content-Type: image/png`.

If the rasteriser is unavailable (missing native dependency), the implementation MUST fall back gracefully by serving the SVG at the PNG endpoint — the operator gets a usable image, just not in the exact requested format. The reference does this; implementations MAY return 500 instead but the SVG fallback is the friendlier behaviour for operators who deploy without the rasteriser.

### 27.5 Player-widget rendering

The bot player widget (§7.28) returns:

- **JSON form** (`/api/widget/player/:botId/data?token=...`): now-playing track info plus the next 5 queued items. Cache `Cache-Control: public, max-age=10` (10 s; live data, low staleness tolerance).
- **BBCode form** (`/api/widget/player/:botId/bbcode?token=...`): a TeamSpeak BBCode block suitable for pasting into a channel description. The reference's format includes `[b]🎵 Now Playing[/b]` with `[color=#00aaff]<title>[/color]`, optional position/duration line `M:SS / M:SS`, and an "Up Next" section listing the next 5 items with overflow indicator.

Tokens for the player widget are deterministic per bot (HMAC-SHA-256 of `"player-widget:<botId>"` with `JWT_SECRET`, truncated to 16 hex chars; §7.28.1).

### 27.6 How to verify

- Render a widget against a server with at least 3 channels (one with a password, one with at least one client, one spacer like `[cspacer]Hello`); the SVG MUST contain the lock emoji on the password channel, the channel name on the named channels, and a centered text on the spacer.
- Render the same widget after applying `hideEmptyChannels=true`; channels with no clients (anywhere in their subtree) MUST be omitted, but spacers MUST remain.
- Switch theme from `dark` to `neon`; the SVG's fill colours MUST change to the neon palette without re-rendering the data.

---

# Part IX — Frontend (SPA)

This part is intentionally compact. The front-end is the most easily reimplemented surface — given the data model (Part II), the REST API (Chapter 7), and the WebSocket events (Chapter 8), an implementer can build any UI they choose. The reference implementation chose React 19 + Vite + TailwindCSS + shadcn/ui + TanStack Query + React Router 7 + Zustand + React Flow (for the bot editor) + Recharts (for the dashboard); a Rust-targeted re-implementation with Leptos, Dioxus, or Yew is equally valid, as is a Node-run alternative. This chapter specifies only the **UX contract** that downstream operators will perceive, not the internal frontend architecture.

## Chapter 28. Frontend App Shape

### 28.1 Routing

Routes (browser URLs) the implementation MUST honour:

| Route | Auth | Component contract |
|---|---|---|
| `/login` | none | Username/password form |
| `/setup` | none (gated) | First-run admin creation; redirects to login on success |
| `/widget/:token` | none | Public widget HTML page (renders `WidgetData` client-side) |
| `/` | auth | Redirect to `/dashboard` |
| `/dashboard` | auth | Live server stats |
| `/servers` | admin | Virtual-server list and controls |
| `/channels` | auth | Channel tree |
| `/clients` | auth | Online client list |
| `/server-groups` | admin | Server-group editor |
| `/channel-groups` | admin | Channel-group editor |
| `/permissions` | admin | Permission editor |
| `/bans` | admin | Ban list |
| `/tokens` | admin | Privilege key list |
| `/files` | admin | Channel file browser |
| `/complaints` | admin | Complaint viewer |
| `/messages` | admin | Offline message viewer |
| `/logs` | admin | Server logs |
| `/instance` | admin | Instance settings |
| `/music-requests` | admin | Music request history |
| `/bots` | admin | Flow list |
| `/bots/:botId` | admin | Visual flow editor |
| `/music-bots` | admin | Music bot management |
| `/settings` | auth | Admin settings (yt-dlp cookies etc.) |
| `*` | auth | 404 page |

The implementation MUST honour `/widget/:token` as a public route (no auth, no header chrome). The reference renders this client-side by fetching `/api/widget/<token>/data` on mount.

### 28.2 Layout chrome

Authenticated routes share a layout:

- **Sidebar** with navigation entries grouped by category (Server, Moderation, Automation, Admin); the implementation MAY collapse on small screens.
- **Header** with the current server's name, a server-switcher dropdown, the current user's display name, a theme toggle (light/dark), and a logout button.
- **Main content area**.

### 28.3 State management seams

The implementation MUST persist three categories of UI state across reloads:

- **Auth tokens** (`accessToken`, `refreshToken`, current user info). Reference uses `localStorage` under key `ts6-auth`; this is a deliberate trade-off (see §28.5). The implementation MAY choose httpOnly cookies for refresh, with access in memory; the reference's choice is acceptable provided the front-end has no XSS surface (no `dangerouslySetInnerHTML`, no `eval`, etc.).
- **Selected server** (`selectedConfigId`, `selectedSid`). Reference under `ts6-server`.
- **UI preferences** (theme, sidebar collapsed). Reference under a UI store (key not externally observable).

Per-feature data (server lists, flow lists, music bots) MUST NOT be persisted across reloads; it is fetched fresh through the query layer (TanStack Query equivalent or any cache abstraction).

### 28.4 Token handling

The implementation MUST:

- Send `Authorization: Bearer <accessToken>` on every authenticated REST call.
- On HTTP 401, attempt one silent refresh via `POST /api/auth/refresh` with the stored `refreshToken`. On success, replace tokens and retry the original request. On failure, clear all tokens and route to `/login`.
- Mark each in-flight request with a "retry done" flag so a single token-expiry does not trigger an unbounded refresh loop.
- Open a single WebSocket to `/ws?token=<accessToken>` for the duration of the session; reconnect on close with exponential backoff.

### 28.5 Token-storage trade-off note

Storing access and refresh tokens in `localStorage` exposes them to any JavaScript that runs in the page's origin. The reference's choice is justified by:

- The front-end having no `dangerouslySetInnerHTML`, no `eval`, no third-party scripts that could be supply-chain compromised in a routine way.
- The back-end's CSP and Helmet defaults preventing inline-script-injection by the upstream.

The implementation MUST NOT introduce any of the above without simultaneously moving tokens out of `localStorage`. If the implementation uses httpOnly cookies for refresh tokens, CORS `credentials: true` MUST be configured (§6.10).

### 28.6 How to verify

- Log in; observe `localStorage` (or cookies, depending on choice) populated. Refresh the page; the SPA MUST start authenticated without re-login.
- Wait for an access token to expire; subsequent API calls MUST silently refresh and continue working.
- Disable the user account from another session; the SPA MUST be logged out within ~1 access-token lifetime.

## Chapter 29. Setup Wizard and Login

### 29.1 First-run flow

On every page load, the SPA MAY (and SHOULD) call `GET /api/setup/status` to detect a fresh install:

- If `needsSetup === true`, redirect to `/setup` and present the admin-creation form (username, password with complexity hint, optional display name). On submit, `POST /api/setup/init`. On success, redirect to `/login`.
- After at least one user exists, the setup endpoints become inert (`POST /api/setup/init` returns 403). The SPA MUST handle this case gracefully (e.g., redirect to `/login` if a user navigates to `/setup` after the wizard is closed).

### 29.2 Login form

A simple username + password form. On submit, `POST /api/auth/login`. On success, store tokens and user info, redirect to the path the user was attempting (or `/dashboard`). On 401, display "Invalid credentials"; on 429, display "Too many attempts".

### 29.3 Password change

Operators may change their own password via the Settings page. Form: current password, new password (with the rules from §6.2.2 explained in real-time). On submit, `PUT /api/auth/password`. On success, all the user's other sessions are revoked server-side (see §6.2.3); the SPA MUST stay logged in (its own refresh token is the one that just got revoked, so the next refresh will fail and the SPA will re-route to `/login` — this is acceptable UX).

## Chapter 30. Server-Selection and Multi-Tenant UX

The header's server-switcher lists every `TsServerConfig` the user has access to (admins see all; non-admins see those with `UserServerAccess` rows). Selecting a server stores the choice in the server store and triggers a re-fetch of every per-server query.

If the user has access to exactly one server, the SPA SHOULD auto-select it on first load (no picker needed). If the user has access to zero servers (a new non-admin user), the dashboard MUST show an explanatory empty state directing them to ask an admin for access.

## Chapter 31. Bot Editor (Visual Flow Canvas)

The flow editor is a node-graph canvas. The implementation MUST:

- Allow drag-and-drop creation of nodes from a palette (categorised as Triggers, Actions, Conditions, Variables, Delays, Logs).
- Allow connection of nodes by dragging from a source handle to a target handle. Condition nodes MUST expose two handles labelled `true` and `false`.
- Allow per-node configuration via a side drawer or modal. The configuration UI MUST cover every field listed in Chapters 13–15 for the relevant node kind.
- Provide a **Channel Picker** component that replaces raw channel-id text inputs everywhere a `channelId` field is configured. The picker fetches `channellist` from the back-end and presents a searchable dropdown; the picker's value is the numeric channel id (as string).
- Provide a **Trigger Event Filter** UI for `event` triggers, allowing operators to add per-field filter pairs (each pair is one entry in the trigger's `filters` map).
- Provide a **Template Gallery** that lists the templates from Chapter 17. On import, the gallery MUST instantiate the template's flow document and present it for editing without immediately enabling it.
- Validate before save: e.g., reject flows with cycles unless the user opts in (via an explicit "allow cycles" toggle); reject `webquery` actions whose command is not in the whitelist; reject malformed cron expressions.

The reference uses React Flow for the canvas. An implementation may use any canvas library; the contract is strictly the persisted JSON document (Chapter 12).

## Chapter 32. Music Bot UI

The music-bot management page presents one card per bot showing:

- Status pill (one of the six states from §18.1).
- Now-playing section: track title, artist, duration progress bar (current position / total duration).
- Transport controls: play/pause, skip, previous, stop, volume slider, shuffle toggle, repeat-mode toggle.
- Queue panel (drag-to-reorder, remove-by-button, clear-all).
- Music library panel (search/upload songs).
- Playlist panel (load playlist into queue).
- Video stream panel (start/stop, source URL field, preset selector, viewer list, in-browser preview that establishes a separate WebRTC peer with the sidecar via the `/stream/webrtc/...` routes).

The implementation MUST subscribe to the WebSocket `music:bot:*` events for live updates. The progress bar MUST animate locally based on the last received `music:bot:progress` event interpolated forward, not just on event arrival (otherwise the bar pulses every second, which feels glitchy).

## Chapter 33. Video Stream UI (Browser-Side WebRTC)

The browser-side video preview is a separate WebRTC peer between the operator's browser and the sidecar — it does NOT consume the same WebRTC stream that TS6 viewers receive (which goes through the TS server). The SPA initiates by calling `POST /music-bots/:id/stream/webrtc/offer`; the back-end forwards to the sidecar's `POST /peer/create` with id `webui-preview`; the sidecar returns its offer SDP. The SPA creates a local `RTCPeerConnection`, sets the offer as remote, generates an answer, and POSTs it to `POST /music-bots/:id/stream/webrtc/answer`. ICE candidates flow through `POST /music-bots/:id/stream/webrtc/ice`.

This separate peer-id (`webui-preview`) is intentional: only one operator usually previews at a time, but multiple viewers (TS6 clients) may be connected with their own clid-based peer ids.

## Chapter 34. Widget Manager UI

The widget management page is a CRUD interface over `Widget` rows. For each widget the UI MUST show:

- The current token (with a copy-to-clipboard button).
- The four embed URLs (`/api/widget/<token>/data`, `/api/widget/<token>/image.svg`, `/api/widget/<token>/image.png`, plus the player widget's URLs if the widget is bot-bound).
- HTML snippet for a `<img>` embed with the SVG/PNG URL.
- Per-widget visibility flags as toggles, plus a theme picker (showing the six theme names).
- Regenerate-token and delete buttons (both with confirmation dialogs).

### 34.1 How to verify (Part IX as a whole)

- Bring up the SPA in a browser; verify each route (Chapter 28) loads and renders.
- Disconnect the back-end; the SPA MUST display a "connection lost" indicator (via WebSocket close handler).
- Resize the browser to mobile width; the layout MUST remain usable (sidebar collapses to a hamburger; cards stack).

---

# Part X — Operations

## Chapter 35. Logging, Metrics, Observability

### 35.1 Structured logging

The implementation MUST emit structured logs with at least the following fields per record:

- `time` (ISO-8601 or Unix-epoch).
- `level` (`debug`, `info`, `warn`, `error`, plus optional `trace`/`fatal`).
- `msg` (the log message).
- Optional structured context (`err` for error objects, plus arbitrary key/values).

Log format:

- **Production** (`NODE_ENV=production` and `LOG_PRETTY` not set): JSON-per-line.
- **Development** or when `LOG_PRETTY=true`: human-readable colourised form.

Log level controlled by `LOG_LEVEL` (default `info`).

The implementation MUST never log secret values (passwords, JWT secrets, encryption keys, API keys, refresh tokens, decrypted ciphertexts). It MAY log token *prefixes* (first 8 chars) for correlation if needed for debugging.

### 35.2 Sidecar logging

The sidecar emits to stderr/stdout (collected by Docker by default). The verbosity is two-mode:

- **Default** (always on): connection events, peer creation/answer/close, RTP-queue-overflow warnings, FFmpeg start/exit, RTCP Sender Report counters at intervals.
- **Debug** (`SIDECAR_DEBUG_LOGS=1`): per-packet timestamps and frequent stats. SHOULD be off in production due to volume.

### 35.3 Per-request logging

The back-end SHOULD log every request at `info` level with method, path, status, duration. Failed requests (status ≥ 400) SHOULD log at `warn` (4xx) or `error` (5xx). Implementations MUST NOT log request bodies that may contain credentials (login, password change, server-config create/update).

### 35.4 Metrics (optional)

The reference does not export metrics. Implementations MAY add Prometheus-compatible `/metrics` endpoints; the recommended surface:

- HTTP request counts by route × status.
- Bot-engine concurrent-execution gauge.
- Voice-bot connection count.
- Sidecar peer count.
- Valkey availability gauge.

These are operationally useful but not part of any external contract.

### 35.5 How to verify

- Set `NODE_ENV=production`; observe log output is JSON-per-line.
- Set `LOG_PRETTY=true`; observe human-readable colourised output even in production.
- Submit a login attempt with bad credentials; the request body MUST NOT appear in any log line.

---

## Chapter 36. Build and Packaging

### 36.1 Monorepo layout

The reference is a `pnpm` workspace with four packages:

| Package | Role |
|---|---|
| `@ts6/common` | Shared TypeScript types, constants, escape utilities, widget themes |
| `@ts6/backend` | Application back-end |
| `@ts6/frontend` | React SPA |
| `sidecar` (no namespace) | Go program |

Implementations MAY use any equivalent monorepo organisation (Cargo workspace, Nx, Turborepo, plain dirs). The shared-types package is helpful but not strictly required if the implementation language doesn't naturally separate types.

### 36.2 Build steps

1. `pnpm install` (or equivalent dependency install).
2. Build the shared types package first.
3. Generate the Prisma client (or equivalent ORM client) for the back-end.
4. Build the back-end (TypeScript compile).
5. Build the front-end (Vite production build).
6. Build the sidecar (Go build, statically linked).

The reference builds with `CGO_ENABLED=0` and `-ldflags="-s -w"` for the sidecar — produces a small statically-linked binary that runs on a `debian:bookworm-slim` base image without further dependencies (other than `ffmpeg` and CA certs).

### 36.3 Container images

Three images:

- **`ts6-manager:backend`.** Base: `node:20-slim`. System deps: `openssl`, `ffmpeg`, `python3` + `pip` (for `yt-dlp[default]`). Pre-creates `data/` and `/data/music`. Default `SIDECAR_URL=http://ts6-sidecar:9800` (bridge-network discovery). Startup script: `prisma db push --skip-generate && prisma db seed || true && node dist/index.js` (pure passthrough acceptable; the `|| true` on seed makes seeds non-fatal).
- **`ts6-manager:sidecar`.** Two-stage build: `golang:1.22-bookworm` for compile, `debian:bookworm-slim` for run. Adds `ffmpeg` + `ca-certificates`. Exposes 9800.
- **`ts6-manager:frontend`.** Two-stage: `node:20-slim` to build, `nginx:alpine` to serve. The nginx config:
  - Listens on `:80`.
  - `client_max_body_size 150m` (so multipart music uploads fit).
  - `location /api` proxies to `http://backend:3001` with HTTP/1.1 + WebSocket-upgrade headers + `X-Real-IP` / `X-Forwarded-For` / `X-Forwarded-Proto`.
  - `location /ws` proxies to the same with explicit WebSocket-upgrade.
  - `location /widget/` clears `X-Frame-Options` and sets `Content-Security-Policy: frame-ancestors *` so the SPA's widget page can be embedded as an iframe on third-party sites; falls back to `index.html` (SPA routing).
  - `location /` falls back to `index.html` (SPA routing).
  - `gzip on` for text/css/json/js/xml.

The implementation MUST preserve the `X-Frame-Options` clearing on `/widget/` (this is the operator-visible contract that lets widgets be iframe-embedded).

### 36.4 Pre-built sidecar binaries

The reference ships two pre-built sidecar binaries in `bin/` for environments that cannot build Go (e.g., Pterodactyl-managed game servers): `bin/sidecar-linux` (Linux x86_64) and `bin/sidecar.exe` (Windows). Operators in those environments set `SIDECAR_BINARY_PATH=/path/to/sidecar-linux` (or `.exe`) and the back-end spawns the binary directly instead of contacting a remote sidecar (§23.3). Implementations MAY ship pre-built sidecars for the same convenience or omit them entirely.

### 36.5 Database migrations

Migrations are append-only ordered SQL files. The reference applies them on container boot via `prisma db push --skip-generate` (this is the dev-style "apply schema state directly"; for production-quality deployments the `prisma migrate deploy` command is recommended instead). Implementations MAY use any migration tool (sqlx-migrate, refinery, etc.) provided that:

- Migrations apply automatically on startup before the listener opens.
- Failures abort startup (do not silently start with a stale schema).

### 36.6 How to verify

- `docker compose up --build` against a fresh checkout MUST produce three healthy services.
- `docker compose down && docker compose up` (no rebuild) MUST come back up against the same data volume with no operator data lost.
- `docker volume rm <data-volume>` then bring up cleanly: the back-end MUST run migrations, seed `max_music_bots`, and present a `needsSetup === true` status to the front-end.

---

## Chapter 37. Cache (Optional Valkey/Redis)

### 37.1 Use cases

The system supports an optional Valkey/Redis-compatible cache. Two consumers:

- **Flow `valkeyGet`/`valkeySet`/`valkeyDelete` actions** (§14.8). Operator-controlled key/value store usable inside flows.
- **Flow `httpRequest` action's response cache** (§14.3). Keyed by `cacheKey` (resolved template), stored under `ts6:httpcache:<cacheKey>`, default TTL 86400 s (24 h).

### 37.2 Connection model

The cache is a separate process (Valkey or Redis), reached via a connection string that the implementation MUST accept via env (the reference does not pin a specific env var name; recommend `REDIS_URL` or `VALKEY_URL`). The client connects on first use; on connection failure, every operation MUST fall through gracefully.

### 37.3 Required graceful-degradation behaviour

- A failed `GET` MUST be treated as a cache miss; the consumer continues.
- A failed `SET` MUST be silently logged (warn), not raised; the consumer continues.
- A failed `DEL` MUST be silently logged (warn), not raised; the consumer continues.
- The flow does not stop because the cache is unavailable.

For the `httpRequest` cache specifically, the implementation MUST also maintain an in-process fallback Map (with the same key) so that the cache continues to function (within one process's lifetime) when the external cache is unreachable.

### 37.4 Key namespace conventions

The implementation SHOULD prefix its system-internal keys (e.g., the HTTP request cache uses `ts6:httpcache:`). Operator-supplied keys (from `valkey*` actions) are passed verbatim, with no prefix — this is intentional so operators can share keys with other consumers of the same Valkey instance.

> **Open Question (Q-37.1):** The reference's flow-runner imports `valkey` from `../services/valkey.js`, but the corresponding source file is **not present** at the pinned commit. The Valkey-related actions and the HTTP-request cache thus would not function in a fresh checkout of the reference; the file is presumably present in the operator's local development tree but unstaged/uncommitted. Implementations MUST provide a working Valkey/Redis client and its graceful-degradation wrapper; this is part of the spec's contract and is described behaviourally above. The implementer should not look for a file to mirror — the surface is fully specified here.

### 37.5 How to verify

- Start without a configured cache; create a flow with a `valkeyGet` action; the flow MUST complete and the action's `storeAs` temp variable MUST be `null`.
- Start with a working cache; `valkeySet` then `valkeyGet`; the value MUST round-trip.
- Take the cache offline mid-flow; subsequent reads MUST return null (cache miss); subsequent writes MUST silently degrade.

---

# Appendix A — Delta from Upstream `clusterzx/ts6-manager`

The fork (`Agent-Fennec/ts6-manager`) under specification differs from its upstream in a number of ways. This appendix is informational; it identifies surface-level deltas the implementer can use when triangulating against either reference. It is not an audit of every line change.

### A.1 Runtime and dependency upgrades

| Surface | Upstream | This fork |
|---|---|---|
| TypeScript | 5.x | 6.x |
| Prisma | 6.x | 7.x |
| React | 18 | 19 |
| Express | 4 | 5 (required a webhook route syntax fix for `path-to-regexp` v8) |
| Vite | 6 | 7 |
| Tailwind CSS | 3 | 4 (required `tailwind-merge` integration update and `outline-solid` variant replacement) |
| React Router | 6 | 7 |
| Zod | 3 | 4 |
| Recharts | 2 | 3 |
| node-cron | 3 | 4 |

The fork's README states that the back-end runtime was migrated from Node.js to **Bun** and the start script changed from `node dist/index.js` to `bun dist/index.js`. The Dockerfile committed to the repo at the pinned commit, however, still uses Node 20. The implementer should treat the runtime as "Node.js-equivalent JavaScript runtime" and not depend on Bun-specific behaviour.

### A.2 Prisma adapter swap

Upstream uses `@prisma/adapter-better-sqlite3` (a native C++ binding, must be compiled per host). The fork's README documents a swap to `@prisma/adapter-libsql` (pure JS/WASM, no native deps) for Bun compatibility. The package.json at the pinned commit, however, still lists `@prisma/adapter-better-sqlite3`; the swap is undocumented in the source state at the pinned commit. **Implementer takeaway:** any pure-JS-or-Rust SQLite client is acceptable.

### A.3 SQLite stability

The fork catches `SQLITE_FULL` at three boundaries (engine loop, executeAction, getVariable/setVariable) so disk-full does not propagate. It also sets `journal_mode=MEMORY` at startup. The implementation MUST preserve both behaviours (§16.2 and §4.4).

### A.4 New trigger events

The fork adds three synthetic event names not present in upstream:

- `client_recording_started`
- `client_recording_stopped`
- `client_nickname_changed`

The poll-based synthesis derives these from `clientlist` field changes (§16.3).

### A.5 New flow actions

- `generateToken` — creates a privilege key from inside a flow.
- `setClientChannelGroup` — assigns a channel group from inside a flow.

Both are documented in Chapter 14 (sections 14.1 and 14.7).

### A.6 Flow-engine UI improvements

- **Trigger event filters:** UI section in the flow editor for adding per-event filter pairs.
- **AFK mover `checkMuteState`:** option to immediately move double-muted clients.
- **Channel picker** replaces raw channel-id text inputs across the flow editor.
- **Multi-select channel picker** for protected channels (in `tempChannelCleanup` and similar).

### A.7 17 flow templates

Listed in §17.1. Upstream had a smaller template gallery; the fork normalised the set.

### A.8 Music bot improvements

- **Now-playing channel descriptions:** the bot updates the `nowPlayingChannelId` channel's description with current track + upcoming queue (BBCode formatted).
- **Configurable bot nicknames:** the SSH "query bot" and the WebQuery session can be renamed without restart (`PUT /servers/:configId` with `queryBotNickname` / `sshBotNickname`).
- **Configurable query-bot channel:** which channel the SSH query bot joins on connect.
- **API key edit without re-entry:** sending an empty string for `apiKey` on `PUT /servers/:configId` is treated as "no change" (§7.5).

### A.9 Tokens UX

- Per-bot **player widget token** (`POST /music-bots/:id/player-widget-token`) — deterministic HMAC-derived public token for embedding now-playing on external sites.
- **Privilege key dialog** in the Tokens admin page — generate server-group / channel-group tokens with explicit group selection (in addition to the upstream's "raw" token form).

### A.10 Code quality / observability

- **Pino logger** replaces `console.log`, with `pino-pretty` enabled in container deployments.
- **Connection pool health checks** (every 30 s, §10.7).
- **Auth middleware** converted to async/await; `JWT_SECRET` causes a fatal startup error if missing in production.
- **`parseIntParam` helper** for safe URL-param parsing.
- **`identityData` excluded** from music-bot list queries (performance).
- **`MusicBots` page** split into focused subcomponents.

### A.11 Valkey/Redis cache

The fork adds `valkeyGet` / `valkeySet` / `valkeyDelete` flow actions and an `httpRequest` response cache, both with graceful degradation. As noted in Q-37.1, the underlying client wrapper file is missing from the pinned commit; the spec describes it behaviourally in §37.

### A.12 Comparison summary

If the implementer uses the upstream as a triangulation reference, the fork's deltas above are the principal differences. If only the fork is used, this appendix can be ignored. The behavioural specification in Chapters 1–37 reflects the **fork**'s state; the upstream is used only as a sanity-check oracle.

---

# Appendix B — Open Questions

This appendix collects every place in the spec where the spec writer is uncertain about a detail. The implementer should treat each as a decision point: pick the safer or simpler choice, or research further (the original source can be consulted by anyone other than the implementer, then summarised back).

| ID | Where | Question |
|---|---|---|
| Q-5.1 | §5.1 | The reference's database file name is inconsistent (`ts6webui.db` vs `ts6manager.db`). The implementation MUST pick one. Recommended: `ts6webui.db`. |
| Q-5.2 | §5.3 | SQLite `journal_mode` choice (`MEMORY` vs WAL). The reference uses MEMORY for crash-safety on disk-full; WAL is the standard production choice. The implementation MUST decide and document. |
| Q-6.1 | §6.11 | The reference uses `id` claim for REST authentication and `sub` claim for WebSocket. The implementation should standardise on one (recommended: `id`) since the inconsistency is invisible to clients. |
| Q-13.x | Ch 13 | The reference's `cron` library is `node-cron`. Implementations using a different cron library may differ on edge cases (5-field vs 6-field, end-of-month semantics). The implementation should document its chosen library's behaviour. |
| Q-14.x | §14.5 | The `checkMuteState` flag for `afkMover` is described in the README but the spec writer did not deeply trace its conditional logic in the source. The implementation should treat it as: when `true`, force-move clients whose `client_input_muted == "1"` AND `client_output_muted == "1"` regardless of idle time. |
| Q-19.x | Ch 19 | The deepest-density open-question chapter. Multiple sub-questions called out inline. |
| Q-25.x | §25.4 | `SYNC_PLAYOUT_BUFFER_MS` default is documented as `4` in the README but the in-source default is `50`. The implementer should default to one of the two and document the choice. The README's `4 ms` is unrealistically tight for real networks; `50 ms` is a more defensible default. |
| Q-37.1 | §37 | The reference's `services/valkey.ts` (or equivalent) is missing at the pinned commit. The Valkey integration is described behaviourally in the spec but not in the reference source. Implementers must build it themselves. |

---

# Appendix C — Test Vectors and Smoke Tests

A suggested operator-driven smoke test that exercises every major subsystem.

### C.1 Fixtures

| Fixture | Purpose |
|---|---|
| A real TS6 server with WebQuery enabled, reachable from the test host | Required for nearly every test |
| WebQuery API key (generated via `apikeyadd`) | Required |
| SSH credentials on the same TS server | Required for the SSH-event-bridge tests |
| A real TS6 voice client (a separate machine) | Required for the music-bot, video-stream, and chat-command tests |
| A small (≤10 MB) MP3 file | Local music test |
| A SHOUTcast/Icecast radio URL (e.g., `https://wdr-1live-live.icecast.wdr.de/...`) | Radio + ICY metadata test |
| A YouTube URL | yt-dlp test |
| A Twitch URL | Video stream test |

### C.2 Operator script

1. **Bring-up.** `docker compose up -d`. Confirm three healthy containers.
2. **Setup.** Open `http://localhost:3000`. SPA MUST redirect to `/setup`. Create admin (`admin` / `Admin1234!`). Land on `/login`.
3. **Login.** Submit credentials. Land on `/dashboard`. Sidebar visible. WebSocket green-dot.
4. **Add server.** Settings → Connections → Add Server. Provide host, WebQuery port, API key, SSH credentials. Save.
5. **Test connection.** Click "Test connection". Expect "success".
6. **Open dashboard.** Switch to the new server. Dashboard MUST populate within 5 s.
7. **Channel tree.** Open Channels page. Tree MUST show every channel on the upstream.
8. **Kick a client.** Connect the real client. Click their entry → Kick. Real client MUST be ejected.
9. **Create a music bot.** Music Bots → Create. Fill name + default channel + voice port. Wait for bot to enter `connected` state. Real client MUST see it in the channel list.
10. **Play a song.** Upload an MP3 to library; play it through the bot. Real client MUST hear audio.
11. **Define a flow.** Bots → New. Add `event(notifycliententerview)` → `message(target=clid, msg=Welcome {{event.client_nickname}})`. Save & enable.
12. **Trigger flow.** Disconnect and reconnect the real client. Welcome message MUST arrive.
13. **Public widget.** Widgets → Create → choose theme `dark`. Copy SVG URL. Open in incognito. Widget MUST render the channel tree.
14. **Video stream.** Music Bot → Video Stream tab → start with a YouTube URL. Real client MUST see the stream offered. Join it. Video MUST play.
15. **Restart back-end.** `docker compose restart backend`. After ~10 s the SPA reconnects WebSocket; the music bot reconnects the voice transport; the public widget keeps serving. Video stream **MAY** be interrupted in bundled-sidecar mode but **MUST** continue in external-sidecar mode.

If all 15 steps pass, the implementation is functionally correct end-to-end.

### C.3 Resilience tests

| Test | Expected behaviour |
|---|---|
| Take the upstream TS server offline mid-session | Connection pool health-check detects within 30 s; bot reconnect schedules; widget serves cached data for ≤45 s then 404. |
| Fill the SQLite volume to 100 % | Flow execution continues; per-execution log writes silently dropped; new login attempts may fail due to refresh-token write failures (acceptable). |
| Disconnect Valkey | Flow `valkey*` actions degrade; `httpRequest` cache falls back to in-memory; SPA unaffected. |
| Stop the sidecar mid-stream | Active video stream tears down; viewers' clients see "stream stopped"; subsequent `/stream/start` requires sidecar to come back. |
| Submit an SSRF probe URL (`http://169.254.169.254`) to a webhook action | Action throws; flow execution fails; the run-log records the rejection. |

