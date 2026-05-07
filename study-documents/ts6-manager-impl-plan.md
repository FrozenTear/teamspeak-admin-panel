# TS6 Manager — Clean-Room Implementation Plan

A delivery plan for an independent Rust + Dioxus reimplementation of the TS6 Manager system, built from `ts6-manager-spec.md` (`Agent-Fennec/ts6-manager` @ `0a4b91e3…`, pinned 2026-04-13). The plan presumes the implementer has not read and will not read the reference source.

---

## 1. Decisions locked

These were resolved before drafting; the rest of the plan flows from them.

| # | Decision | Value | Rationale |
|---|---|---|---|
| D1 | Team shape | Larger team or unknown — plan parallelises around clear integration contracts | User input |
| D2 | MVP scope | **Full parity** with the spec | User input |
| D3 | Backend language | **Rust** | User input |
| D4 | Frontend | **Dioxus + fullstack server functions** | User input — shared types between client and server |
| D5 | Voice protocol path | **Fork `tsproto` / `tsclientlib`, port the TS6 handshake delta from the spec** | TS6 servers are wire-compatible with TS3 clients on most of the stack (UDP framing, Opus voice, command channel). The handshake / shared-secret derivation changed (see `Splamy/TS3AudioBot#1078`). Forking buys ~70% of the Chapter 19 work for free; the delta is implemented from the spec only. Cleanroom-safe: tsproto is independent of the reference system. |
| D6 | Video transport | **MoQ over WebTransport + WebCodecs** instead of WebRTC | Conscious deviation from spec Chapter 24's external contract. Better fit for one-to-many public viewers; Chapter 24's HTTP control plane is redesigned around MoQ. Cost: custom WebCodecs playback in Dioxus, possible older-iOS gap. |
| D7 | Optional cache | Same as spec — optional Valkey/Redis, graceful degradation | No reason to deviate |
| D8 | Database | **SQLite** (matches spec's external contract: column names appear in JSON wire types and operator backups) | Schema is part of external contract |

### Cleanroom rules in force

- **External contracts are preserved verbatim**: HTTP route paths, query parameters, env var names, default ports, on-disk paths, JSON keys (including those that mirror DB columns), TS ServerQuery command/event names, music-bot chat tokens, flow JSON tag strings, widget tokens.
- **Internal source-code identifiers are NOT preserved**: file/module/function/class/type names from the reference system are unknown to us and stay unknown. We invent our own.
- **Two intentional deviations from the spec's external contract** (allowed; documented):
  - **Video sidecar HTTP API (Chapter 24)** is replaced with a MoQ control plane. Public widget HTML embeds a MoQ player rather than a WebRTC peer.
  - **Backend process model (Chapter 2)** collapses the spec's separate static-front-end container into the Dioxus fullstack server. The spec's three-process topology becomes two processes (fullstack server + media sidecar) plus DB + optional cache.

Both deviations are flagged in the source repo's README so future maintainers understand they are choices, not bugs.

---

## 2. Architecture sketch

```
                     ┌─────────────────────────────────────┐
                     │         Dioxus client (WASM)        │
                     │  - Operator SPA (25 routes)         │
                     │  - Bot flow editor canvas           │
                     │  - MoQ video player (WebCodecs)     │
                     └────────────────┬────────────────────┘
                                      │ Server functions / REST / WS / MoQ
                                      ▼
   ┌───────────────────────────────────────────────────────────────────┐
   │                  Dioxus fullstack server (axum)                    │
   │                                                                    │
   │  ┌───────────────┐  ┌────────────┐  ┌──────────────────────────┐  │
   │  │ Auth + RBAC   │  │ REST API   │  │ WebSocket realtime hub   │  │
   │  └───────┬───────┘  └─────┬──────┘  └──────────┬───────────────┘  │
   │          │                │                    │                  │
   │  ┌───────▼────────────────▼────────────────────▼─────────────┐    │
   │  │              Domain services (per server connection)       │    │
   │  │                                                            │    │
   │  │  WebQuery client │ SSH event bridge │ Flow runtime │ ...  │    │
   │  └────────┬─────────────┬──────────────────┬────────────────┘    │
   │           │             │                  │                     │
   │  ┌────────▼──────┐ ┌────▼──────┐  ┌────────▼─────────────────┐  │
   │  │ Voice bot pool│ │ Widget    │  │ Music library / playlist  │  │
   │  │ (forked       │ │ renderer  │  │ store                     │  │
   │  │  tsproto)     │ │ (SVG/PNG) │  │                           │  │
   │  └───┬───────────┘ └───────────┘  └──────────────────────────┘  │
   │      │ UDP voice                                                  │
   └──────┼──────────────────────────────────────────────────────────┘
          │                              │ HTTP control      │ DB
          │                              ▼                   ▼
          │               ┌────────────────────────┐    ┌─────────┐
          │               │  MoQ media sidecar     │    │ SQLite  │
          │               │  - FFmpeg ingest       │    │  (file) │
          │               │  - MoQ relay (Rust)    │    └─────────┘
          │               │  - WebTransport server │
          │               └─────────┬──────────────┘
          ▼                         │ WebTransport
   TeamSpeak 6 server               ▼
   (WebQuery + SSH)         Browsers (public widget viewers)
```

### Crate selection (recommended)

| Concern | Crate | Notes |
|---|---|---|
| Web framework | `axum` (via Dioxus fullstack) | Server-functions ride on top |
| Frontend | `dioxus` + `dioxus-fullstack` + `dioxus-web` | Shared types FE↔BE |
| ORM / SQL | `sqlx` | Compile-time query checking; preserves SQL surface for the column-name external contract. Avoid `sea-orm` / `diesel` if you want SQL strings to stay close to spec |
| Migrations | `sqlx::migrate!` | Driven from `migrations/*.sql` |
| Password hashing | `argon2` | Default `Argon2id` |
| JWT | `jsonwebtoken` | HS256 with `JWT_SECRET` |
| Symmetric encryption | `aes-gcm` | AES-256-GCM with `ENCRYPTION_KEY` |
| TS WebQuery (HTTP) | `reqwest` + `rustls` | `TS_ALLOW_SELF_SIGNED` flag toggles cert verification |
| TS SSH bridge | `russh` | Mature async SSH client |
| TS voice (Chapter 19) | **fork of `tsproto`** + new TS6 handshake module | See §3.9 |
| Opus | `audiopus` (libopus binding) or pure-Rust `opus` | Pacing in user-space |
| Audio mux/demux | `ffmpeg` subprocess | Same as spec |
| YouTube ingest | `yt-dlp` subprocess | Same as spec |
| MoQ relay | `moq-dev/moq` (Rust) or `moq-rs` | See §3.10 |
| WebTransport server | `wtransport` | QUIC + WebTransport |
| QuickLZ | `quicklz` crate (FFI) | For tsproto-handled compression |
| WebSocket | `axum::extract::ws` | Built-in |
| Cron | `cron` or `tokio-cron-scheduler` | For flow cron triggers |
| Logging | `tracing` + `tracing-subscriber` | JSON in prod, pretty in dev |
| SVG | `svg` crate or hand-built XML | For widget rendering |
| PNG rasterization | `resvg` | Drop-in for the spec's `resvg-style` rasterizer |
| Cache | `redis` or `valkey` crate | Optional; degrade gracefully |
| Config | `figment` or `envy` | Env-var driven |

---

## 3. Workstreams

Fourteen workstreams, each with: **scope** (what chapters of the spec it covers), **dependencies** (which other streams must land first), **integration contract** (what the stream produces that other streams consume), **risk** (low/medium/high + why).

### 3.1 Foundation / platform (FOUNDATION)
- **Scope:** Cargo workspace layout, Dioxus fullstack scaffolding, `axum` server bootstrapping, env-var config loader (Ch. 5), `tracing` logging (Ch. 35), error-translation middleware, dev/prod build scripts, Docker images (Ch. 36).
- **Dependencies:** None — first stream.
- **Integration contract:** A running fullstack server that responds to a health endpoint + serves a "hello" Dioxus page. Loads all env vars from Chapter 5 at boot with the fallback rules (`ENCRYPTION_KEY` falls back to `JWT_SECRET`, etc.).
- **Risk:** Low. Standard Rust infra.

### 3.2 Database + data model (DATA)
- **Scope:** Schema for the ~17 entities in Chapter 4, migrations, seed/fixture loader. Column names follow the spec verbatim where they appear in JSON wire types. Two starter migrations (the spec's "two nickname-field migrations").
- **Dependencies:** FOUNDATION.
- **Integration contract:** `sqlx` types + a `repos::*` module per entity exposing typed CRUD. Migrations runnable via `cargo run -- migrate`.
- **Risk:** Low. Spec is precise on shape and FK cascades.

### 3.3 Auth + security (SECURITY)
- **Scope:** Chapter 6 — Argon2id password hashing, JWT access (15 min default) + refresh-token rotation with reuse detection by family, refresh-token revocation cascade on user delete, RBAC (admin/moderator/viewer), per-server access grants, AES-256-GCM credential encryption (TS API keys + SSH passwords), CORS allowlist driven by `FRONTEND_URL`, security headers, SSRF blocklist (Chapter 9), password complexity rules, authenticated WebSocket handshake, login rate limit.
- **Dependencies:** DATA (refresh-token table, user table).
- **Integration contract:**
  - `axum` extractors: `RequireRole(Role::Admin)`, `RequireServerAccess(server_id)`.
  - `crypto::seal(plaintext) -> Vec<u8>` / `unseal(...)` for credential at-rest encryption.
  - `ssrf::is_url_allowed(url) -> Result<()>` callable by any outbound-fetching code (flow webhooks, HTTP-request actions, music bot stream URLs, FFmpeg URLs, video URLs).
- **Risk:** Medium. Refresh-token reuse-detection-by-family is subtle; SSRF blocklist is security-critical.

### 3.4 TS WebQuery HTTP client (WEBQUERY)
- **Scope:** Chapter 10 — HTTP client to the TS6 WebQuery API, API-key header auth, per-server connection pooling, health probe + `autoStart` policy, error translation (TS `{code, message}` → HTTP status returned to the browser), TLS / `TS_ALLOW_SELF_SIGNED`, ServerQuery parameter escaping (spaces, pipes, special chars).
- **Dependencies:** SECURITY (decrypts API key on read), DATA (server connection records).
- **Integration contract:** A `WebQueryClient` per server connection, exposing typed methods for each TS command used in REST routes. Errors carry TS code + message for translation to HTTP at the route boundary.
- **Risk:** Low–medium. ServerQuery escaping has edge cases; the spec has the rules.

### 3.5 TS SSH event bridge (SSHBRIDGE)
- **Scope:** Chapter 11 — `russh`-based SSH client per server connection, `servernotifyregister` for server/channel/textserver/textchannel/textprivate, line-based notify-frame parser, query-bot identity (nickname, default channel), synthetic event derivation (`client_mic_muted`, `client_sound_muted`, `client_went_away`/`client_came_back`, `client_group_added`/`removed`, `client_nickname_changed`, `client_recording_started`/`stopped`), reconnect with backoff.
- **Dependencies:** SECURITY (decrypts SSH password), DATA, WEBQUERY (for state queries needed to synthesise some events).
- **Integration contract:** A `tokio::sync::broadcast<Event>` per server connection. Subscribers: WebSocket hub, flow runtime, voice bot pool.
- **Risk:** Medium. Synthetic event derivation has state-machine subtleties; reconnection needs to flush stale state.

### 3.6 REST API surface (REST)
- **Scope:** Chapter 7 — 25 route groups, all request/response shapes, error responses, role checks. Many are thin proxies through WEBQUERY; some are pure backend (auth, users, settings, widgets, flows, music, video).
- **Dependencies:** SECURITY, DATA, WEBQUERY (most route groups depend on it).
- **Integration contract:** Each route group is a separate axum `Router` mounted under its path. JSON shapes match the spec's TypeScript types verbatim where they appear in wire JSON.
- **Risk:** Medium. Sheer surface area (the largest single workstream by line count). Parallelises well across team members — assign one person per ~5 route groups.

### 3.7 WebSocket realtime hub (WS)
- **Scope:** Chapter 8 — auth handshake, event push categories (TS events, music bot updates, video stream status, flow execution log streaming), reconnection semantics, fan-out from per-server `broadcast` channels to authenticated browser sessions.
- **Dependencies:** SECURITY (auth handshake), SSHBRIDGE (event source), and later VOICE / VIDEO / FLOW for their respective events.
- **Integration contract:** A `WsHub` task per process, accepting subscriptions filtered by `(server_id, event_kind)`. Backpressure: slow clients are dropped, not queued unbounded.
- **Risk:** Medium. Backpressure + reconnection are easy to get wrong.

### 3.8 Bot flow engine (FLOW)
- **Scope:** Chapters 12–17 — flow document model (nodes/edges/handles, `'true'`/`'false'` condition outputs), 4 trigger kinds (event / cron / webhook / command), 25+ action kinds (TS-control, composite TS-control, network, WebQuery passthrough, stateful, domain), conditions, variables (flow + temp scopes), delays, logging, execution loop with three named SQLite-full boundaries, per-flow concurrency, run-log persistence, WebQuery command whitelist, 17 pre-built templates.
- **Dependencies:** SECURITY (SSRF for webhook actions; encryption for stored secrets in flow data), DATA, WEBQUERY (for TS-control actions and webquery passthrough), SSHBRIDGE (for event triggers), WS (for live execution-log streaming to the editor), REST (CRUD for flows + manual run + log fetch).
- **Integration contract:** `flow::execute(flow, trigger_payload) -> ExecutionId` spawns a `tokio::task`. Output: `BotExecution` rows + log lines, also broadcast over WS.
- **Risk:** High. Largest engine in the system. The 25+ actions × 4 triggers × condition/variable substitution explodes the test surface. Suggested approach: implement triggers + 5 most-used actions first as a vertical slice; fill in the rest after the slice passes.

### 3.9 Voice bot path — forked tsproto (VOICE)
- **Scope:** Chapters 18–22 — voice bot lifecycle (states, auto-start, auto-reconnect), forked `tsproto` + new TS6 handshake module (Chapter 19 delta only), audio pipeline (yt-dlp, FFmpeg, Opus, 20 ms pacing, ICY metadata), per-bot queue/playlist/library, in-channel chat commands (`!radio`, `!play`, `!stop`, `!pause`, `!skip`/`!next`, `!prev`, `!vol`, `!np`).
- **Dependencies:** FOUNDATION + DATA + SECURITY for the lifecycle layer; **runs largely independently** of REST/WS until integration milestones. The tsproto fork can begin in parallel with FOUNDATION.
- **Integration contract:**
  - `MusicBot` actor per bot record, controllable via `BotCommand` mpsc.
  - `BotEvent` broadcast (state, now-playing, queue-changed) consumed by REST polling endpoints, WS push, and chat-command output.
- **Risk:** **Highest single workstream.** The TS6 handshake delta is undocumented externally — only the spec's Chapter 19 describes it, and the spec author flags Chapter 19 as "highest open-question density." Mitigation:
  - **Spike first** (1 week budget): in a throwaway repo, get a forked tsproto to authenticate against a real TS6 server. If the spike fails, escalate (extra time, packet capture, possibly third-party voice expert).
  - **Kill criterion:** if after 4 weeks total the bot cannot authenticate + send Opus to a TS6 server, defer voice bots to v2. The rest of v1 must not block on this.
  - Isolate the work in a separate crate (`crates/voice/`) so that defer = remove a Cargo dependency, not unwind weeks of integration.

### 3.10 Video sidecar (MoQ) (VIDEO)
- **Scope:** Chapters 23–25, **redesigned around MoQ**:
  - Sidecar: separate Rust binary `ts6-media-sidecar`. Single QUIC/WebTransport listener. Per-source: FFmpeg subprocess produces VP8 + Opus into a local pipe; sidecar muxes into MoQ tracks; serves to subscribers over WebTransport.
  - Backend control plane (replaces spec Ch. 24's `/peer/*` endpoints):
    - `POST /source` (start) / `POST /source/stop` (unchanged in spirit)
    - `GET /track/<id>` returns the MoQ broadcast namespace + track names that browsers subscribe to
    - `GET /stats` / `GET /health` (unchanged)
  - Browser playback: Dioxus widget that uses `web-sys` + `wasm-bindgen` to drive WebTransport + WebCodecs decoders, painting frames to a `<canvas>`.
  - Quality presets (480p/720p/1080p) keep their FFmpeg parameters from the spec.
- **Dependencies:** FOUNDATION (config), SECURITY (SSRF for source URLs), REST (control plane endpoints).
- **Integration contract:** Sidecar exposes only HTTP control + WebTransport media. Backend addresses sidecar at `SIDECAR_URL`. Two ports: `SIDECAR_PORT` for HTTP control, second port for WebTransport (default `SIDECAR_PORT + 1`; configurable).
- **Risk:** Medium-high. MoQ spec is draft-17 (moving target); WebCodecs in WASM/Dioxus is uncommon territory; older iOS clients may lack WebTransport. Mitigations:
  - Pin to a specific MoQ draft + a specific `moq-rs` version; upgrade only on a planned rev.
  - Build a non-Dioxus reference HTML+JS player first (using `moq-dev/moq`'s TS bits) to validate end-to-end, then port to Dioxus.
  - Document the iOS gap in the operator-facing widget config so they can warn their viewers.

### 3.11 Public widget renderer (WIDGETS)
- **Scope:** Chapters 26–27 — widget CRUD with random URL-safe tokens, per-widget visibility flags + max channel depth + theme palettes (6 themes), public routes `/widget/<token>` (HTML), `.svg`, `.png`, `.json`, channel tree assembly with spacer detection (`line`/`dotline`/`dashline`/`center`/`left`/`right`/`none`), dynamic SVG layout (height grows with content), `resvg` rasterization, caching headers.
- **Dependencies:** SECURITY (no auth on the public path, but rate-limited and short-cache-controlled), DATA, WEBQUERY (for live state).
- **Integration contract:** Stateless rendering functions that take a snapshot + theme + flags and return bytes per format.
- **Risk:** Low. Pure rendering; deterministic.

### 3.12 Frontend SPA — Dioxus pages (FE-PAGES)
- **Scope:** Chapter 28–30, 32–34 — 25 page routes, layout chrome (sidebar/header/server selector), state seams (auth, server selection, UI prefs as global; per-feature query/cache for the rest), theming, setup wizard, login, server-selection/multi-tenant UX, music bots UI, video stream UI, widget manager UI.
- **Dependencies:** REST + WS for data; SECURITY for the JWT + refresh handling on the client side.
- **Integration contract:** Server functions for typed REST calls; thin WS subscription hooks. Page components keep their own per-route data.
- **Risk:** Medium. 25 routes is bulk work, parallelisable. Dioxus fullstack server functions reduce the typing-overhead of REST contracts since types are shared.

### 3.13 Bot flow editor canvas (FE-EDITOR)
- **Scope:** Chapter 31 — visual canvas, drag-and-drop nodes, edge creation with `'true'`/`'false'` handles, per-node configuration drawers, trigger event-filter UI, channel-picker component, template gallery import, pre-deploy validation.
- **Dependencies:** FE-PAGES (chrome), REST + WS (flow CRUD + live execution log streaming), FLOW (the actual runtime — the editor needs to know what node kinds exist and what fields each takes; this is pure config but must not drift).
- **Integration contract:** A `NodeKindCatalog` JSON document served by the backend describes every trigger and action kind, its configurable fields, and validation rules. The editor uses this catalog to render forms and validate before save. Single source of truth — both the runtime and the editor consume it.
- **Risk:** Medium-high. Canvas UX is the most demanding frontend work in the system; Dioxus does not have a mature flow-canvas ecosystem, so expect to write the canvas yourself or wrap a JS lib via interop.

### 3.14 Operations + packaging (OPS)
- **Scope:** Chapters 35–37, **adapted to a Podman-native deployment story** (see §9). Structured logging policy, per-request logging with secret redaction, sidecar dual-mode logging gated behind `SIDECAR_DEBUG_LOGS=1`, monorepo build (`cargo build` + `dx build` + sidecar build), two OCI container images, pre-built sidecar binaries in `bin/` selectable via `SIDECAR_BINARY_PATH`, deployment shapes (see §9 — replaces the spec's five Docker compose files).
- **Dependencies:** All other streams (consumes their artifacts).
- **Integration contract:**
  - `Containerfile.fullstack` and `Containerfile.sidecar` (OCI images, runtime-agnostic; Podman is the development and reference target).
  - Quadlet unit files (`fullstack.container`, `sidecar.container`, `ts6-manager.pod`) for systemd-managed production deploys.
  - Kubernetes YAML (`deploy/kube/*.yaml`) playable via `podman kube play` for portable / Kubernetes-bound deploys.
  - `podman-compose.yml` for development convenience only (compose semantics; not the recommended production path).
  - Rootless by default. CI builds OCI images and pushes to a registry.
- **Risk:** Low. Mechanical.

---

## 4. Dependency graph + critical path

```
                      FOUNDATION
                          │
              ┌───────────┼───────────┐
              ▼           ▼           ▼
            DATA      SECURITY     OPS (continuous)
              │           │
              └─────┬─────┘
                    ▼
                WEBQUERY ───────┐
                    │           │
                    ▼           ▼
              SSHBRIDGE       REST  ──────────────────────┐
                    │           │                          │
                    └─────┬─────┘                          │
                          ▼                                ▼
                         WS                            WIDGETS
                          │
            ┌─────────────┼────────────┐
            ▼             ▼            ▼
          FLOW         VOICE        VIDEO
                                       │
                                       ▼
                                   FE-PAGES
                                       │
                                       ▼
                                   FE-EDITOR
```

**Critical path (longest serial chain):** FOUNDATION → SECURITY → WEBQUERY → SSHBRIDGE → WS → FLOW → FE-EDITOR.

**Parallel tracks once foundation lands:**
- VOICE can run mostly alone (own crate) from Phase 1 onward.
- VIDEO can run mostly alone (own binary) from Phase 1 onward.
- WIDGETS can run after WEBQUERY without waiting for WS.
- FE-PAGES can begin scaffolding routes as soon as auth + a few REST endpoints land.

---

## 5. Phasing

Six phases, each ending in a verification gate from the spec's Chapter 1 verification list. **Phase end = the gate passes against a real TS6 server.**

### Phase 0 — Bootstrap (≈ 1–2 weeks, 1–2 people)
- **Streams:** FOUNDATION, OPS skeleton.
- **Deliverable:** A Dioxus fullstack app with `tracing` logging, env loading per Chapter 5, a `/health` endpoint, a `Hello` page, and a working `Containerfile.fullstack` + a minimal `podman-compose.yml` for dev.
- **Gate:** `cargo run` boots; `podman-compose up` boots rootless; the placeholder page renders.

### Phase 1 — Walking skeleton: auth + a single TS read (≈ 4–6 weeks, 3–4 people)
- **Streams:** SECURITY, DATA, WEBQUERY (read-only subset), REST (auth + servers + setup + dashboard), FE-PAGES (login + setup wizard + dashboard shell).
- **Spike in parallel:** VOICE handshake spike (1 week, 1 person, separate crate).
- **Deliverable:** Operator can complete Chapter 1 verifications **1–3** (log in, add a server, read live dashboard).
- **Gate:** Manual test against a real TS6 server: log in, add server connection, see channel/client counts.

### Phase 2 — TS control plane (≈ 6–8 weeks)
- **Streams:** WEBQUERY (full coverage), SSHBRIDGE, WS, REST (channels, clients, server-groups, channel-groups, permissions, bans, tokens, complaints, messages, server-logs, files, instance-settings, virtual-servers), FE-PAGES (per-feature pages stacked on the dashboard chrome), WIDGETS.
- **Continues:** OPS adds redaction, JSON-prod logging.
- **Deliverable:** Chapter 1 verifications **4 and 7** (kick a real client; render a public widget).
- **Gate:** Operator kicks a real client; widget URL renders SVG/PNG/JSON for an unauth viewer.

### Phase 3 — Flow engine (≈ 6–8 weeks)
- **Streams:** FLOW (core: triggers + 5 most-used actions first; then full action catalog), FE-EDITOR, REST (bots, music-requests, settings, webhooks). The 17 templates land at the end of the phase as data fixtures.
- **Deliverable:** Chapter 1 verification **6** (define a flow, observe it triggers).
- **Gate:** A `notifycliententerview` → `sendtextmessage` flow runs end-to-end against a real server; execution log streams live to the editor.

### Phase 4 — Voice bots (≈ 6–10 weeks; parallel to Phase 2/3 for the protocol crate)
- **Streams:** VOICE (full), REST (music-bots, music-library, playlists, radio-stations, music-requests), FE-PAGES (Music Bots UI), FLOW integration of music-bot actions if any.
- **Hard checkpoint at week 4:** if forked-tsproto cannot authenticate against TS6, escalate or drop Part VI from v1 (becomes v1.1 or v2). This is the planned-for failure mode.
- **Deliverable:** Chapter 1 verification **5** (bot streams audible audio into a channel).
- **Gate:** Music bot connects to a TS6 channel and a real TS voice client hears audio from a known-good radio URL.

### Phase 5 — Video streaming (MoQ) (≈ 5–7 weeks; can begin in parallel with Phase 3 on a separate track)
- **Streams:** VIDEO (full sidecar + control plane + Dioxus player), REST (video sessions), FE-PAGES (Video Stream UI).
- **Deliverable:** A non-trivial extension of Chapter 1's verification list — operator starts a stream from a YouTube URL, opens the public widget viewer in two browsers, both see the stream.
- **Gate:** End-to-end browser playback works; A/V sync is tight; sidecar `/health` and `/stats` return useful values.

### Phase 6 — Hardening + release (≈ 3–4 weeks)
- **Streams:** OPS (Quadlet units, `podman kube play` YAMLs, `podman-compose.yml` for dev, two OCI images, pre-built sidecar binaries), security review, performance smoke tests, production hardening checklist, rootless-deployment validation.
- **Deliverable:** Tagged v1.0 release.
- **Gate:** All seven Chapter 1 verifications pass against a fresh rootless Podman deploy of the release artifact.

### Total elapsed
Roughly **6–9 months** of calendar time depending on team size and the voice-protocol risk. With 4 engineers running parallel tracks, the lower end is realistic; with 2 engineers, plan for the upper end.

---

## 6. Risk register

| ID | Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|---|
| R1 | TS6 handshake delta is larger than the spec captures (Chapter 19 open questions multiply) | Medium | High | Phase 1 voice spike; week-4 kill criterion in Phase 4; voice isolated in its own crate so dropping is mechanical |
| R2 | MoQ draft churn breaks our pinned version | Medium | Medium | Pin `moq-rs` and the spec draft number; upgrade only on planned revs; keep the FFmpeg→sidecar path stable so only the WebTransport face moves |
| R3 | Older iOS / Safari lacks WebTransport for widget viewers | Medium | Medium | Document in operator-facing widget UI; defer fallback (e.g., HLS) to a v1.1 if needed |
| R4 | Dioxus flow-canvas (FE-EDITOR) lacks ecosystem support, slowing Phase 3 | High | Medium | Allocate the strongest frontend engineer to FE-EDITOR; budget extra time; consider wrapping a JS lib via WASM interop for the canvas only |
| R5 | Refresh-token reuse-detection-by-family logic ships a subtle bug enabling token replay | Low | High | Write reuse-detection unit tests first; security review at Phase 1 gate |
| R6 | SSRF blocklist gaps (e.g., DNS rebinding, IPv6 link-local, octal-encoded private ranges) | Medium | High | Use a battle-tested allow/deny library; add tests for known SSRF tricks; security review before Phase 2 gate |
| R7 | TS ServerQuery escaping bug corrupts strings with pipes/spaces | Medium | Medium | Fuzz the escaper against the spec rules; add a roundtrip test |
| R8 | SQLite-full conditions surface differently in Rust / sqlx than the three named boundaries the spec assumes | Low | Medium | Implement the three boundaries explicitly per spec; add error-injection tests |
| R9 | Synthetic SSH events (mute/away/recording) drift from the source's exact derivation rules | Medium | Low | Use spec's "How to verify" sections as test cases; flag remaining ambiguities to Appendix B for spec author follow-up |
| R10 | Dioxus fullstack server-functions hit a scalability ceiling for high-fanout WS workloads | Low | Medium | Use raw axum WS for the realtime hub instead of routing it through server functions; server functions are for request/response, not push |

---

## 7. Open decisions for the user

These do not block starting Phase 0 but should be resolved before the workstream that consumes them.

| Open | Needed by |
|---|---|
| **OD1.** Repo layout: single Cargo workspace vs. multi-repo (one for fullstack, one for sidecar, one for shared types). | Phase 0 |
| **OD2.** CI host (GitHub Actions / Forgejo / Woodpecker / self-hosted). Affects how OPS images are built. | Phase 0 |
| **OD3.** Where to host the TS6 reference server used for verification gates (operator-supplied? CI-hosted? local in dev only?). | Phase 1 |
| **OD4.** Which `moq-rs` flavor and draft number to pin (`moq-dev/moq` vs. alternatives). Spike in Phase 1. | Phase 5 |
| **OD5.** Whether to ship MoQ widget viewers behind a feature flag in v1 or wait for v1.1 if iOS gap is unacceptable. | Phase 5 gate |
| **OD6.** Cleanroom audit: do you want a third party to audit the spec for inadvertent source leakage *before* implementation begins? Cheap if done now, expensive if discovered late. | Before Phase 1 |
| **OD7.** Licensing of the new implementation (MIT, like the reference? AGPL? proprietary?). Affects whether tsproto fork can be redistributed. tsproto is dual-licensed MIT/Apache-2.0 so MIT works. | Phase 0 |

---

## 8. What the implementer should NOT do

Listed because they're the easy wrong turns:

- **Don't read the reference repo.** The whole cleanroom property collapses if any implementer reads `Agent-Fennec/ts6-manager`.
- **Don't preserve internal identifiers.** `BotFlow.flowData` is an external contract (it's a JSON column name in operator-visible APIs); a class named something like `FlowExecutor` in the reference is **not** — invent your own.
- **Don't try to wrap the unmodified `tsclientlib` crate against TS6.** It will fail at the handshake. The path is *fork* + handshake delta from spec, not consume-as-a-dependency.
- **Don't keep the spec's WebRTC sidecar API in parallel "for compatibility."** You picked MoQ; the spec's `/peer/create` endpoints don't need to exist. Half-implementing both is more code and more bugs.
- **Don't put long-running services (voice bot pool, SSH bridge, MoQ relay client) inside Dioxus server functions.** Server functions are RPC. Long-running services are tokio tasks owned by the axum server's app state.
- **Don't optimise before Phase 6.** The spec is the contract; meeting the spec is the goal. Performance is a hardening-phase concern unless a specific measurement shows otherwise.

---

## 9. Container runtime: Podman-native deployment

The reference system ships five Docker compose files (default, dev, local-build, reverse-proxy, Coolify). This implementation targets **Podman** as the only supported runtime; the deployment shapes are therefore reshaped around Podman idioms.

### Build artifacts

- **`Containerfile.fullstack`** — multi-stage build that compiles the Dioxus fullstack server + WASM bundle and produces an OCI image. Final stage runs as a non-root UID inside the container (rootless Podman aligns container UID to a host subuid).
- **`Containerfile.sidecar`** — multi-stage build for the MoQ media sidecar binary + FFmpeg. Final stage runs non-root.
- Build with `podman build -t ts6-manager:dev -f Containerfile.fullstack .` etc. Images are OCI-compliant; any OCI runtime can pull them, but Podman is the development and CI target.

### Deployment shapes (replaces the spec's five compose shapes)

| Shape | When to use | Files |
|---|---|---|
| **Dev (compose)** | Local development with hot iteration | `podman-compose.yml` |
| **Single-host pod** | Self-hosting on one box (homelab, small server) | `deploy/quadlet/*.container`, `deploy/quadlet/ts6-manager.pod` (systemd-managed via Quadlet) |
| **Kubernetes-style** | Multi-host or kube-bound deploys; portable to any orchestrator that speaks k8s YAML | `deploy/kube/ts6-manager.yaml` (playable via `podman kube play` or any k8s cluster) |
| **Reverse-proxy fronted** | Behind Caddy / nginx / Traefik on the host | Same as single-host pod, with the fullstack container's port bound to localhost only and the host proxy terminating TLS |

### What was dropped from the reference's deployment shapes

- **Coolify shape.** Coolify is Docker-only as of 2026. If Podman support lands later, this can be re-added; until then, operators wanting a Coolify-style PaaS UX should use a Podman-native alternative (e.g., Cockpit's container view, or simple Quadlet + systemd).
- The reference's "local-build" compose file collapses into the dev compose file (the difference was Docker-image-source vs. local Docker build, which is irrelevant once builds are `podman build`).

### Pod topology

```
ts6-manager.pod
├── fullstack.container   (Dioxus fullstack server, port 3001)
├── sidecar.container     (MoQ media sidecar, ports 9800 + WebTransport)
└── valkey.container      (optional; only if cache is configured)

Host volumes mounted into the pod:
  - ./data/sqlite       → /var/lib/ts6-manager/db        (DATABASE_URL points here)
  - ./data/music        → /var/lib/ts6-manager/music     (MUSIC_DIR)
  - ./data/yt-cookies   → /var/lib/ts6-manager/cookies   (YT_COOKIE_FILE)
```

Rootless Podman is the default. Volumes use host paths owned by the operator's user. SQLite + the music directory live on the host and survive `podman pod rm`.

### Notes for OPS workstream

- Generate Quadlet units via `podman generate systemd` once a known-good `podman pod create` recipe is locked.
- For the Kubernetes YAML, prefer hand-written manifests over `podman generate kube` — the generated YAML drifts and is hard to review.
- CI publishes images to a registry (`ghcr.io/<org>/ts6-manager-fullstack`, `…-sidecar`); the OD2 decision (CI host) determines exactly where.
