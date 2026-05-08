# TS6 Manager — Clean-Room Behavioral Specification

## Table of Contents (proposal)

> **Status:** Draft ToC for review. Chapters marked **[scope]** below show estimated page counts. Reply with which chapters to keep, drop, or shorten before I commit to the full draft.

> **Implementation deviation.** This document describes the upstream reference. The clean-room implementation in this repo uses **SurrealDB v3** instead of SQLite per impl-plan §1 D8 (board-ratified 2026-05-07). Sections that mention SQLite, `journal_mode`, `SQLITE_FULL`, sqlx, etc. describe the upstream contract; map them onto SurrealDB equivalents at implementation time.

---

## Reference

- **Canonical source under study:** `Agent-Fennec/ts6-manager`
- **Pinned commit:** `0a4b91e3d33b300e5c159f264d00f2408e8d5912`
- **Commit date:** 2026-04-13
- **License of source:** MIT (per README badge; no `LICENSE` file at repo root — worth noting but not blocking)
- **Author of this spec:** observer with read access to the source above. The spec is for an implementer who will not see the source directly.

## Clean-room ground rules used throughout

1. The spec describes **observable behavior, on-the-wire formats, and external contracts** of the system. It does not describe how the source is internally factored into files, classes, or functions.
2. Identifiers fall into two buckets:
   - **External-contract identifiers** (preserved verbatim): HTTP route paths, query parameters, env var names, on-disk file paths, default ports, JSON keys exchanged with browsers or with the TeamSpeak server, TeamSpeak ServerQuery command names and event names, music bot chat command tokens, the WebRTC sidecar HTTP endpoints, and database column names that appear in JSON wire types. These are facts about the world (or the database, which is part of the deployed product) and are *not* expression.
   - **Internal source-code identifiers** (NOT preserved): class names, function names, file names, internal helper names, internal type aliases. The spec uses neutral descriptive terms invented by the spec writer.
3. **No source code is reproduced.** Algorithms are described in prose or implementer-neutral pseudocode in the spec writer's own words.
4. Every chapter ends with a **"Test the implementation"** subsection — observable behavior the implementer can use to verify their build matches the spec without having to read the original source.
5. Where the spec writer is uncertain (e.g., undocumented edge cases that would need deeper source reading), the chapter flags an **Open Question** rather than guessing.

---

## Part I — System Overview

### Chapter 1. Purpose, Goals, and System Boundaries  **[2 pages]**
- What problem the system solves: a self-hosted web UI that controls one or more remote TeamSpeak 6 server instances over their HTTP "WebQuery" management API, plus side-cars that add capabilities the WebQuery API doesn't provide (live audio bots, live video streaming, automation flows, public widgets).
- What the system explicitly is *not*: a TeamSpeak server, a TeamSpeak voice client for end users, a Telnet-based ServerQuery tool.
- Trust model: the web UI is the only thing the operator interacts with; the backend is the only thing that holds API keys and SSH credentials; the browser never gets server credentials.
- External actors: human operators (admin/moderator/viewer roles), public widget viewers (no auth), TeamSpeak server (over WebQuery HTTP + SSH), YouTube/Twitch (via `yt-dlp` and direct URLs), optional Valkey/Redis cache.

### Chapter 2. Topology and Process Model  **[3 pages]**
- Three deployable processes plus storage: web SPA, application backend, media relay sidecar, SQLite database, optional Redis-compatible cache, downloaded music directory.
- Default port allocation, network reachability requirements (sidecar must be reachable from backend; sidecar's WebRTC must be reachable from end-user browsers).
- The five pre-defined deployment shapes shipped as compose files (default, dev, local-build, reverse-proxy/Coolify) and their differences in port exposure and image source.
- Container responsibilities and volume mounts.

### Chapter 3. Glossary of External Identifiers  **[2 pages]**
A reference list of the identifiers the implementer must preserve verbatim. Examples (non-exhaustive in ToC):
- Default ports: 3001, 3000, 5173, 9800, 9987, 10080, 10022.
- Env vars: `JWT_SECRET`, `ENCRYPTION_KEY`, `DATABASE_URL`, `JWT_ACCESS_EXPIRY`, `JWT_REFRESH_EXPIRY`, `FRONTEND_URL`, `MUSIC_DIR`, `SIDECAR_URL`, `SIDECAR_BINARY_PATH`, `YT_COOKIE_FILE`, `TS_ALLOW_SELF_SIGNED`, sidecar-specific `VIDEO_QUEUE_SIZE`, `AUDIO_QUEUE_SIZE`, `SYNC_PLAYOUT_BUFFER_MS`, `SYNC_VIDEO_BIAS_MS`, `AUDIO_DELAY_MS`, `SIDECAR_DEBUG_LOGS`, `VIDEO_READ_RTP_BUFFER`, `AUDIO_READ_RTP_BUFFER`, `VIDEO_BUFSIZE`, `STUN_SERVERS`, `FFMPEG_PATH`, `SIDECAR_PORT`.
- TeamSpeak event names (`notify…`).
- Music bot chat commands (`!radio`, `!play`, `!stop`, `!pause`, `!skip`/`!next`, `!prev`, `!vol`, `!np`).
- Trigger and action type strings used in flow JSON.
- Database column names that appear in wire types.

---

## Part II — Persistent Data and External Contracts

### Chapter 4. Data Model  **[6 pages]**
- All entities and their fields, types, defaults, uniqueness, indexes, foreign-key cascade rules.
- Entities (drawn from the database schema; canonical wire-level shape): user account, refresh-token record, per-user/per-server access grant, TeamSpeak server connection record, bot flow record, bot flow variable, bot flow execution, bot flow execution log line, key-value app setting, music bot record, song record, playlist record, playlist-song join, radio station, public widget configuration, music request history entry, video stream session.
- Relations diagram (text/ASCII).
- Per-entity invariants (which fields are encrypted-at-rest, which are foreign keys, which are unique combinations).
- Migration history: 2 migrations adding two nickname fields. Approach to schema versioning.

### Chapter 5. Configuration Surface (Environment, Files, Secrets)  **[4 pages]**
- Every env var: required vs optional, default, fallback rules (`ENCRYPTION_KEY` falls back to `JWT_SECRET`), production-mode validation (e.g., `JWT_SECRET` must exist).
- On-disk paths used by backend at runtime: SQLite file location, music download directory, optional yt-dlp cookies file path, optional sidecar binary path.
- Cookie-file management (operator can upload via UI; merges with file-system file).
- Production hardening checklist.

### Chapter 6. Security Model  **[5 pages]**
- Authentication: argon-style password hashing approach, rate-limited login endpoint, JWT access tokens (signed with `JWT_SECRET`, default 15-minute lifetime), refresh-token rotation with reuse detection by family, refresh-token revocation cascade on user delete.
- Authorization: three roles (admin/moderator/viewer); per-server access grants for non-admin users.
- Credential at-rest encryption: AES-256-GCM with the `ENCRYPTION_KEY`, applied to TeamSpeak API keys and SSH passwords.
- Network safety: SSRF blocklist for outbound HTTP and FFmpeg URLs (covered behaviorally in Chapter 9 and Chapter 11); WebQuery command whitelist for flow `webquery` actions; HTTP security headers; CORS allowlist driven by `FRONTEND_URL`.
- Password complexity rules.
- Authenticated WebSocket handshake.

---

## Part III — REST API and Realtime

### Chapter 7. REST API Surface  **[12 pages]**
For each of the 25 route groups listed below: the HTTP path, methods, request body shape, response shape, required role, error responses.

Route groups:
- Auth (login, refresh, logout, me)
- Setup (first-run admin creation; closes after first admin exists)
- Users
- Servers (TeamSpeak server connection records)
- Virtual servers (proxied to WebQuery)
- Channels
- Clients (online + database-only)
- Server groups
- Channel groups
- Permissions
- Bans
- Tokens (privilege keys)
- Complaints
- Messages (offline messages + chat send)
- Server logs
- Files (channel file browser)
- Instance settings
- Dashboard (aggregated stats)
- Bots (flow definitions)
- Music bots (lifecycle + playback)
- Music library (songs)
- Music requests
- Playlists
- Radio stations
- Settings (app-level KV)
- Widgets (CRUD; tokens)
- Widget public (no-auth widget rendering by token)

For each: full request/response JSON shape, derived from the shared TypeScript types in the source.

### Chapter 8. Realtime / WebSocket Channel  **[3 pages]**
- WebSocket path and authentication handshake.
- Event names, payload shapes, push frequency.
- Reconnection semantics expected of the browser.
- Backend → browser push categories: TeamSpeak events forwarded from the SSH bridge, music bot playback updates, video stream status, bot flow execution log streaming.

### Chapter 9. Outbound HTTP Safety (SSRF Protection)  **[2 pages]**
- Hostname/IP rules applied to every outbound URL the user can supply (flow webhook actions, HTTP request actions, music bot stream URLs, video stream URLs, FFmpeg-fed URLs).
- Block lists (private ranges, link-local, loopback) and allow-lists for known-safe sidecar URLs.

---

## Part IV — TeamSpeak Integration (Server Control Plane)

### Chapter 10. WebQuery HTTP Client  **[5 pages]**
- The server-side HTTP client that calls TeamSpeak's WebQuery endpoints.
- Authentication: API key header.
- Connection pooling and per-server reuse.
- Health-check probe behavior and `autoStart` policy.
- Error-translation table (TeamSpeak `{code, message}` → HTTP status returned to the browser).
- TLS/self-signed handling driven by `TS_ALLOW_SELF_SIGNED`.
- Parameter-encoding rules for ServerQuery values (escaping rules for spaces/pipes/special chars).

### Chapter 11. SSH Event Bridge  **[4 pages]**
- Why SSH is used at all (WebQuery does not push events; SSH is the only available subscription path on TS6).
- SSH session establishment, `servernotifyregister` registrations performed on connect (server, channel, textserver, textchannel, textprivate).
- Parsing line-based notify frames into structured events.
- The "query bot" identity: nickname, default channel, configurable per-server.
- Synthetic events derived from raw events (e.g., `client_mic_muted`, `client_sound_muted`, `client_went_away`/`client_came_back`, `client_group_added`/`removed`, `client_nickname_changed`, `client_recording_started`/`stopped`).
- Reconnection + backoff.

---

## Part V — Bot Flow Engine

### Chapter 12. Flow Document Model  **[3 pages]**
- JSON shape stored in `BotFlow.flowData`: nodes, edges, source/target handle conventions (notably `'true'` / `'false'` for condition outputs).
- Node kinds (`trigger`, `action`, `condition`, `delay`, `variable`, `log`).
- Variable scopes (`flow`, `temp`).

### Chapter 13. Triggers  **[5 pages]**
For each trigger kind:
- **Event** triggers: legal event names; per-trigger filter expressions; how filters are matched against event payload fields.
- **Cron** triggers: cron expression syntax accepted (5- or 6-field; library version implications), timezone handling.
- **Webhook** triggers: route shape (`/api/webhooks/<flowId>/<path>`), method, mandatory secret check, body and query forwarding into the run context.
- **Command** triggers: prefix (default `!`), command name match, optional channel-id scoping, who is allowed to invoke (talk-power / role gating, if any).

### Chapter 14. Actions  **[10 pages]**
For each of the 25+ action kinds: input fields, semantic effect, side effects, error modes, what gets written to the run-log, what gets stored back into variables (`storeAs`).

Action catalog:
- TS-control: kick, ban, move, message, poke, channelCreate, channelEdit, channelDelete (with `force`), groupAddClient, groupRemoveClient, groupRemoveAll (with keep-list), groupRestoreList (with exclude-list), generateToken (server-group / channel-group), setClientChannelGroup, pokeGroup.
- Composite TS-control: afkMover (with optional `checkMuteState` for double-muted users), idleKicker, tempChannelCleanup, animatedChannel (six animation styles).
- Network: webhook, httpRequest (with optional `cacheKey` + `cacheTtlSeconds`).
- WebQuery passthrough: webquery action with command-whitelist enforcement.
- Stateful: generateCode (length, numericOnly), valkeyGet / valkeySet / valkeyDelete (TTL).
- Domain: rankCheck.

### Chapter 15. Conditions, Variables, Delays, Logging  **[3 pages]**
- Condition expression evaluation: `{{placeholder}}` substitution rules, expression engine semantics, how `temp`-scope numeric strings are coerced for equality.
- Variable operations: set / increment / append; flow-scoped vs temp-scoped storage.
- Delay node semantics (millisecond resolution, accuracy guarantees).
- Log node behavior: levels, fan-out (run-log row + console).

### Chapter 16. Flow Runtime (Execution Loop)  **[4 pages]**
- Execution lifecycle: `BotExecution` row creation, status transitions (`running` → `completed`/`failed`/`cancelled`).
- Error handling boundaries (notably: SQLite-full conditions are caught at three named boundaries — execution loop, action dispatch, variable read/write).
- Concurrency: per-flow concurrency, parallel triggers.
- Persistence of run logs.
- The "command whitelist" for `webquery` actions: which TeamSpeak commands are permitted vs blocked, and the rationale (no destructive instance-level commands).

### Chapter 17. Pre-Built Flow Templates  **[3 pages]**
The 17 templates shipped to the operator (Clock Channel, Online Counter, Server Stats, Animated Channel Name, Welcome Message, Support System, Temp Channel Creator, Auto-Rank, Last-Seen Tracker, AFK Mover, Idle Kicker, Bad Name Checker, Group Protector, three Webhook templates, Anti-VPN). For each: trigger + action graph, parameters the operator can edit. Implementations of these are mostly compositions of nodes already specified in Chs 13–14, so this chapter is short.

---

## Part VI — Voice Bots (Audio)

### Chapter 18. Voice Bot Lifecycle  **[3 pages]**
- States: `stopped`, `starting`, `connected`, `playing`, `paused`, `error`.
- Auto-start on backend boot.
- Auto-reconnect with exponential backoff.
- Per-server multi-bot semantics.
- Interaction with the TeamSpeak server: bot connects as a *real client* (not a query bot), advertises a nickname, joins a channel.

### Chapter 19. TeamSpeak Voice Client Protocol  **[15 pages]**
The deepest chapter. The original TeamSpeak voice protocol is undocumented; the spec describes only the on-the-wire behavior necessary for a music bot to connect and stream audio:
- UDP framing: packet type tags, packet IDs, generation counters, fragmentation rules.
- Cryptographic handshake: identity proof-of-work (Ed25519 / EAX — to be confirmed against source), per-packet authenticated encryption mode, key derivation steps.
- Identity file format and how identities are generated (the worker-thread proof-of-work search).
- Compression: QuickLZ usage and which packet kinds are compressed.
- Login command sequence to the voice server (`clientinit`, channel join, etc.) and the corresponding state machine.
- License blob handling.
- Outbound voice frame composition: codec selection (Opus Voice / Opus Music), framing rate, FEC/redundancy (if any), silence packets.
- Inbound frame handling (we only need to *send*, but acks need to be parsed).
- Keep-alive / ping cadence.
- **This chapter has the highest "open question" density** — TeamSpeak voice protocol details are not officially documented and the source is the only reference. I will flag every place where I'm describing what the source does without an external-spec confirmation.

### Chapter 20. Audio Pipeline  **[4 pages]**
- Source ingestion modes: local file, YouTube (via `yt-dlp` subprocess), HTTP radio stream (with ICY metadata).
- Decoding/transcoding via FFmpeg subprocess with stdin/stdout piping.
- Opus encoding parameters (frame size, bitrate, stereo).
- Stable 20 ms pacing strategy (jitter-free outgoing cadence).
- Volume scaling.
- ICY metadata polling for "now playing" updates on radio streams.
- yt-dlp invocation: arguments, cookie-file passthrough, format selection.
- Now-playing channel description update side-effect.

### Chapter 21. Playlist, Queue, and Music Library  **[3 pages]**
- Per-bot queue model: ordering, current index, history for `!prev`.
- Repeat modes (`off`, `track`, `queue`) and shuffle interaction.
- Library entities (Song, Playlist, PlaylistSong) and their relationship to a bot's runtime queue.
- Music request history (de-duplicated by `(serverConfigId, url)`).

### Chapter 22. In-Channel Chat Commands  **[2 pages]**
- The 10 commands (`!radio`, `!radio <id>`, `!play [<url>]`, `!stop`, `!pause`, `!skip`/`!next`, `!prev`, `!vol [0-100]`, `!np`).
- Who is allowed to invoke (channel-membership requirement).
- Per-command output format.

---

## Part VII — Video Streaming

### Chapter 23. Video Streaming Architecture  **[3 pages]**
- Why a separate Go process exists (Node WebRTC stacks were judged unsuitable for low-latency relay).
- Roles: backend orchestrates, sidecar relays media, browser viewers consume via WebRTC.
- Two ports per sidecar instance for inbound RTP from FFmpeg (chosen at OS-level via UDP port 0); one HTTP port for control plane.
- Quality preset table (480p / 720p / 1080p) and the FFmpeg encoding parameters for each.

### Chapter 24. Sidecar Control-Plane Protocol  **[3 pages]**
HTTP API exposed by the sidecar (callable only by the backend):
- `POST /peer/create` ⇢ takes `{id}`, returns `{sdp}` (server-side offer SDP).
- `POST /peer/answer` ⇢ takes `{id, sdp}`, returns `{status}`.
- `POST /peer/ice` ⇢ takes `{id, candidate, sdpMid, sdpMLineIndex}`.
- `POST /peer/close` ⇢ takes `{id}`.
- `POST /source` ⇢ takes `{source, width, height, framerate, bitrate}` and starts FFmpeg streaming to internal RTP ports.
- `POST /source/stop` ⇢ stops FFmpeg, drains queues, resets sync state.
- `GET /stats` ⇢ peers + ports + source.
- `GET /health` ⇢ liveness probe.

Behavioral details: idempotency of `peer/create` (in-flight de-duplication), peer reuse rules based on ICE state, behavior when an "answer" arrives in the wrong signaling state (silently ignored), STUN server list (default 9 servers, overridable via `STUN_SERVERS`).

### Chapter 25. Media Plane (RTP, Codecs, Pacing, A/V Sync)  **[4 pages]**
- Codec choices: VP8 video (PT 96, 90 kHz clock), Opus stereo (PT 111, 48 kHz clock).
- FFmpeg → sidecar RTP loopback (bound to 127.0.0.1, OS-assigned UDP port).
- Hard-coded SSRCs in FFmpeg invocation (`11111111` video, `22222222` audio).
- VP8 keyframe gate: a peer's media stream is held closed until the first VP8 keyframe is observed in the input RTP, then opened.
- Adaptive playout pacing: per-track latency EWMA, target = max(audio, video) latency + sync buffer + optional video bias.
- A/V sync via RTCP Sender Reports: identical NTP timestamp + identical CNAME on both video and audio SR, sent every second.
- Queue back-pressure (drop on overflow, with periodic warning logs).
- VP8 payload-descriptor parsing (only enough to identify keyframe start of frame).

---

## Part VIII — Server Widgets (Public, Embeddable)

### Chapter 26. Widget Configuration and Token Model  **[2 pages]**
- Widget lifecycle: create with token (random URL-safe), regenerate token, delete.
- Per-widget visibility flags (channel tree, clients, hide-empty-channels, max channel depth).
- Six themes (dark/light/transparent/neon/military/minimal) with hex palettes.

### Chapter 27. Widget Rendering Formats  **[3 pages]**
- Public routes: `/widget/<token>` (HTML page), `/widget/<token>.svg`, `/widget/<token>.png`, `/widget/<token>.json`.
- Channel tree assembly: spacer detection (`line`/`dotline`/`dashline`/`center`/`left`/`right`/`none`), nesting via `pid`, depth clamp.
- SVG layout: header band, body, fonts, dimensions algorithm (height grows with content).
- PNG rasterization (resvg-style — preserve external behavior, not internal lib choice).
- Caching headers / TTL for public widgets.

---

## Part IX — Frontend (SPA)

### Chapter 28. Frontend App Shape  **[4 pages]**
- 25 page routes, organized by feature.
- Layout chrome: sidebar, header, server selector.
- State management seams: server selection (global), auth (global), UI prefs (global); per-feature query/cache layer for everything else.
- Theming and dark/light toggle.

### Chapter 29. Setup Wizard and Login  **[2 pages]**
- First-run flow: detect "no admin exists" → redirect to setup → create admin → setup endpoint becomes inert.
- Login form, validation, error messages, redirect-after-login behavior.
- Token storage strategy in browser (memory + httpOnly refresh cookie *or* explicit refresh-token storage — to be confirmed).

### Chapter 30. Server-Selection and Multi-Tenant UX  **[1 page]**
- Server picker behavior, per-user access filtering, default-server logic.

### Chapter 31. Bot Editor (Visual Flow Canvas)  **[3 pages]**
- Canvas node palette, drag-and-drop, edge creation.
- Node configuration drawer per node kind.
- Trigger event-filter UI (added by fork).
- Channel-picker component used in place of raw channel-id text inputs.
- Template gallery import.
- Validation before deploy.

### Chapter 32. Music Bots UI  **[2 pages]**
- Bot card layout, queue panel, music library panel, playlist panel.
- Now-playing display, progress bar, volume control, transport controls.

### Chapter 33. Video Stream UI  **[2 pages]**
- Tab placement.
- WebRTC peer establishment from the browser (offer-from-server pattern: browser asks backend to ask sidecar to create offer; browser sends answer back through backend).
- In-browser preview rendering.

### Chapter 34. Widget Manager UI  **[1 page]**
- CRUD UI, copy-to-clipboard for embed URLs and HTML snippets.

---

## Part X — Operations

### Chapter 35. Logging, Metrics, Observability  **[2 pages]**
- Structured logger format (JSON in production, prettified in dev), log levels.
- Per-request logging policy (redaction of secrets).
- Sidecar logging: dual-mode (info logs always on; debug logs gated behind `SIDECAR_DEBUG_LOGS=1`).
- What is *not* logged (passwords, API keys, refresh tokens).

### Chapter 36. Build and Packaging  **[2 pages]**
- Monorepo build steps: install, typecheck, build, run migrations, start.
- Sidecar build (Go).
- Three Docker images and their entry-points.
- Pre-built sidecar binaries shipped in `bin/` for environments that cannot build Go (and how they're selected via `SIDECAR_BINARY_PATH`).

### Chapter 37. Cache (Optional Valkey/Redis)  **[1 page]**
- Used by: HTTP-request action result cache, valkey* flow actions.
- Graceful degradation: missing cache = silent skip, non-fatal.

---

## Appendix A. Delta from Upstream `clusterzx/ts6-manager`  **[2 pages, optional]**
Reference table of what the fork changed (per the fork's own README): runtime migrated to Bun; Prisma adapter swap; SQLite stability fixes; Pino logger; new triggers (`client_recording_started`/`stopped`, `client_nickname_changed`); new actions (`generateToken`, `setClientChannelGroup`); 17 templates; UI improvements (channel picker, protected channels multi-select); now-playing channel descriptions; configurable bot nicknames; Valkey helpers. Only a delta listing — no source-level diffing.

## Appendix B. Open Questions for the Spec Writer  **[~1 page, grows during drafting]**
A live list of "this is what I see in the source but I'd want a TS protocol expert / a deeper read to confirm" items. Most of these will live in Chapter 19 (voice protocol) and Chapter 11 (SSH event bridge edge cases).

## Appendix C. Test Vectors and Smoke Tests  **[3 pages, optional]**
- A scripted operator flow that exercises every major subsystem.
- Recommended fixtures (a tiny TS server, a sample MP3, a sample radio URL, a sample YouTube URL).
- Expected observable outputs at each step.

---

## Estimated Total

| Part | Pages |
|------|-------|
| I. Overview | 7 |
| II. Data + Contracts | 15 |
| III. REST + Realtime + Safety | 17 |
| IV. TS Integration | 9 |
| V. Flow Engine | 28 |
| VI. Voice Bots | 27 |
| VII. Video Streaming | 10 |
| VIII. Widgets | 5 |
| IX. Frontend | 15 |
| X. Operations | 5 |
| Appendices | 6 |
| **Total** | **~144 pages** |

This is large but proportional to "exhaustive + strict two-team" on a system of this surface area. The two heaviest single chapters are **Ch 19 (TS voice protocol)** and **Ch 14 (action catalog)** — these are also the two chapters with the highest implementation risk, so the page count is earned.

---

## Suggested scope cuts (in order of pain saved per page lost)

If you want a shorter doc, the cheapest cuts are, roughly in order:

1. **Drop Appendix C (Test Vectors)** — saves 3 pages, costs you a smoke-test guide you can rebuild from scratch.
2. **Drop Chapter 17 (Templates)** — saves 3 pages; templates can be re-derived from the Trigger + Action chapters by anyone implementing.
3. **Compress Part IX (Frontend)** — frontend is the most easily reimplemented part; an implementer with the data + REST chapters can build any UI they want. Could shrink from 15 pages to ~3 by giving only a route list and screenshots-equivalent prose.
4. **Drop Chapter 11 (SSH Event Bridge) details and just say "subscribe to TS notifications via the documented `servernotifyregister` API over the official subscription channel"** — saves 2 pages but loses the synthetic-event derivations; an implementer can reinvent those.
5. **Cut Chapter 19 (TS Voice Protocol) entirely if you can use an existing third-party TS3 client library** — saves 15 pages and spares the riskiest reverse-engineered work. This is the single biggest scope decision for the project.

---

## What I need from you to proceed

Pick one of these directions:

- **A.** Approve as-is. I write all 37 chapters + appendices.
- **B.** Approve with cuts. Tell me which chapters to drop or compress, then I write the trimmed version.
- **C.** Phase the work. Tell me which chapters to write *first* (e.g., "Parts I–IV before anything else") and I deliver in passes.
- **D.** Adjust the depth target. (You picked "exhaustive"; if 144 pages is too much, I can re-estimate at "detailed" instead, which would land around 50–70 pages with the same chapter list but tighter writing.)
