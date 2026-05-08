# TS6 Manager — Implementation Deviations Register

This is the single living register of every conscious deviation between the clean-room implementation in this repo and the upstream behavioural spec (`ts6-manager-spec.md`, `Agent-Fennec/ts6-manager` @ `0a4b91e3…`, pinned 2026-04-13).

Source of truth: `ts6-manager-impl-plan.md` §1 names the decisions; this file expands each one and tracks external-contract impact + what stays preserved verbatim.

**Rules of the register**

- Every deviation from the spec's external contract gets one entry here. New deviations are appended (not scattered across banners and footnotes elsewhere).
- The `ts6-manager-impl-plan.md` and `ts6-manager-spec.md` banners point at this file rather than duplicate its content.
- Locked decisions (e.g. D8 SurrealDB) are not re-opened by editing this file. Re-opening requires a fresh board decision linked from the entry.

---

## D6 — Video transport: MoQ over WebTransport + WebCodecs

- **Spec reference:** Chapter 24 (Sidecar Control Plane) and the §3.10 "Sidecar HTTP control routes" glossary; Chapter 23 §23.5 viewer-session topology; §2.1 sidecar process role.
- **Decision row:** impl-plan §1 D6.

**What the deviation is.** The spec's video path uses WebRTC for browser-side media delivery and an HTTP control plane on the sidecar (`POST /source`, `/peer/*`, `/source/stop`) plus a TS3 `streamsignaling` sub-protocol carrying SDP offer/answer/ICE through the upstream TeamSpeak server. The clean-room implementation replaces this with **MoQ over WebTransport** for media transport plus **WebCodecs** for browser-side decode. The spec's HTTP control plane is redesigned around MoQ track subscriptions; the public widget embeds a MoQ player rather than a WebRTC peer.

**Why / ratification.** Better fit for one-to-many public viewers (single publisher, many subscribers, CDN-friendly fan-out) than per-viewer WebRTC peer connections. Captured at impl-plan drafting time as a conscious external-contract deviation (D6); flagged in the source repo's README so future maintainers see it as a choice, not a bug.

**External-contract impact.**
- **Sidecar HTTP routes (§3.10):** redesigned around MoQ. The spec's `/source`, `/peer/*`, `/source/stop` shapes are NOT preserved verbatim.
- **TS3 `streamsignaling` sub-protocol (§24.2):** does not carry WebRTC offer/answer/ICE. Whatever signaling the MoQ path needs rides on its own channel.
- **Hard-coded RTP SSRCs (§24.1, video=`11111111`, audio=`22222222`):** not applicable; no RTP on the viewer path.
- **Default sidecar port `9800` / `SIDECAR_PORT`:** preserved as the sidecar control port (the protocol on it changes; the operator-visible env var name and default port don't).
- **STUN env vars (`STUN_SERVERS`, §3.4):** not used by the MoQ path. Kept as accepted-but-unused for forward compatibility, or dropped — TBD by VIDEO workstream's first PR.
- **Public widget HTML:** widget tokens and `/api/widget/...` route paths (§3.9, §7.28) are preserved verbatim. Only the embedded player implementation changes.
- **Older-iOS gap risk:** WebTransport + WebCodecs has narrower mobile-Safari support than WebRTC. Tracked as VIDEO-stream risk; documented for operators.

**Cleanroom note.** Preserved verbatim despite this deviation: public widget routes (§3.9, §7.28), widget tokens, FFmpeg ingest contract for the publisher side (the sidecar still ingests via FFmpeg subprocess), `SIDECAR_PORT` / `SIDECAR_URL` / `SIDECAR_BINARY_PATH` env-var names, sidecar `/health` endpoint shape. Only the viewer-facing transport (WebRTC → MoQ) and the back-end ↔ sidecar control payloads change.

---

## D8 — Database: SurrealDB v3

- **Spec reference:** §2.1 (Topology — SQLite database file as the persistent storage volume), §2.4 (Volumes), §2.5 (back-end bring-up step "Set SQLite `journal_mode=MEMORY`"), Chapter 4 (Data Model — entity tables and column names), Chapter 5 §5.1 `DATABASE_URL`, §5.3 on-disk layout, Chapter 16 §16.2 (failure-tolerance boundaries — `SQLITE_FULL`, `SQLITE_BUSY`, `SQLITE_CORRUPT`).
- **Decision row:** impl-plan §1 D8.

**What the deviation is.** The spec describes persistence as a single SQLite file driven by `sqlx`. The clean-room implementation uses **SurrealDB v3** instead — embedded SurrealKV backend by default (single-binary self-host), with the same `surrealdb` crate able to speak `ws://` to an external `surreal start` server. `DATABASE_URL` is the sole operator-visible toggle between the two topologies; that decision is RustPlatform's call in the first DATA PR.

**Why / ratification.** Original D8 was SQLite + `sqlx` because the spec's column names appear in JSON wire types and operator backups, so the schema is part of the external contract. The board **ratified the SurrealDB swap on 2026-05-07** ([PURA-43](/PURA/issues/PURA-43)). Driver: better fit for the structured-document plus realtime-query workload the flow engine and dashboard route impose; same crate covers embedded and server topology, keeping the self-host wedge intact.

**External-contract impact.**
- **JSON wire-format keys (§3.11 "Database column names exposed in wire types", §4.2 entity tables):** **preserved verbatim** by mapping spec column names to SurrealDB record field names 1:1. No client-visible rename.
- **Operator backups:** **deviation accepted** — backups become SurrealDB exports rather than `.sqlite` files. The spec implies a single `.sqlite` file at the `DATABASE_URL` path that operators back up; SurrealDB equivalents are exports of the running database.
- **`DATABASE_URL` env var (§5.1):** kept as the single connection-string knob. Default value changes from `file:./data/ts6webui.db` to a SurrealDB connection string per the DATA PR.
- **`journal_mode=MEMORY` bring-up step (§2.5):** not applicable; SurrealDB has no equivalent and the implementation does not set it.
- **Database volume mount (§2.4):** still required, still rooted at the path resolved from `DATABASE_URL` — but the on-disk layout under that path is SurrealKV files, not a `.sqlite` file plus WAL.
- **Three SQLite-full boundaries (§16.2):** mapped to **write-failure / transaction-conflict / capacity-pressure** against SurrealDB error variants. Tracked in impl-plan §6 R8.
- **`sqlx` migration mechanism:** replaced by a hand-rolled `.surql` migration runner driven from `migrations/*.surql`.

**Cleanroom note.** Preserved verbatim despite this deviation: every JSON key the front-end and the back-end exchange (including those mirroring DB columns), the `DATABASE_URL` env-var name and role, the `MUSIC_DIR` env var and `/data/music` reference path, the music-directory volume contract, all REST/WebSocket route shapes that read or write entities. The change is internal to the storage engine; the external wire formats and operator-mounted directory contract for the music volume are unchanged.

**Status.** Locked. Re-opening requires a fresh board decision; do NOT relitigate inside this register.

---

## D-PROC — Process model: collapse the spec's three-process topology

- **Spec reference:** Chapter 2 (Topology and Process Model) — §2.1 "three long-running processes" (front-end / back-end / sidecar), §2.3 deployment shapes, §2.5 container responsibilities (front-end serves the SPA bundle as static files; back-end reads `VITE_API_URL` / `VITE_WS_URL` at build time).
- **Decision row:** not a numbered D-row in the impl-plan decisions table; ratified as part of the impl-plan §1 "Cleanroom rules in force" and the §2 architecture sketch.

**What the deviation is.** The spec deploys as **three processes** — a static-frontend container (nginx-style), a back-end application server, and a WebRTC media sidecar. The clean-room implementation collapses the static-frontend container **into the Dioxus fullstack server** (`axum`-based, serving SSR + WASM bundle + REST + WebSocket from one process). The deployed topology becomes **two processes** — Dioxus fullstack server + media sidecar — plus the database (SurrealDB, embedded or external) and the optional Valkey/Redis cache.

**Why / ratification.** The Dioxus fullstack model already serves the SPA bundle from `axum`, so a separate static container would be pure overhead — extra process, extra Dockerfile, extra reverse-proxy hop, extra surface for VITE-style build-time URL injection that Dioxus does not need. Captured at impl-plan drafting time as the second of two intentional external-contract deviations (alongside D6); flagged in the source repo's README.

**External-contract impact.**
- **Default port `3000` (front-end host-exposed, §3.1):** dropped. The fullstack server takes over the externally-exposed HTTP port directly. Operators who hard-coded `:3000` in reverse-proxy configs see a difference.
- **Default port `80` (front-end nginx container-internal, §3.1):** dropped.
- **`VITE_API_URL` / `VITE_WS_URL` build-time env vars (§2.5):** not applicable. Dioxus fullstack uses same-origin routing by default; no build-time URL injection.
- **Deployment shapes (§2.3):** the four reference `docker-compose` files collapse. Single-binary self-host stays, the spec's reverse-proxy shape still works (proxy targets the fullstack server + sidecar).
- **Volume contracts (§2.4):** the `MUSIC_DIR` mount and the database volume still apply, both on the fullstack server's container. The front-end container's "no volume" property is moot.

**Cleanroom note.** Preserved verbatim despite this deviation: every REST route path (Chapter 7), every WebSocket envelope shape (Chapter 8), every back-end env var (§5.1) — including `JWT_SECRET`, `ENCRYPTION_KEY`, `PORT`, `DATABASE_URL`, `FRONTEND_URL`, `MUSIC_DIR`, `SIDECAR_URL`, `SIDECAR_BINARY_PATH`, `YT_COOKIE_FILE`, `TS_ALLOW_SELF_SIGNED`. The two-process collapse is purely a packaging choice; nothing the operator or browser sees on the wire changes except the disappearance of the `:3000` static-front-end port.

---

## D-WS — WebSocket: topic subscriptions + last-event-id replay

- **Spec reference:** Chapter 8 — §8.3 ("There is no client-to-server message protocol; the WebSocket is a push channel"), §8.5 ("The server MUST NOT keep client-specific state across reconnections; each reconnection re-authenticates and re-subscribes — subscription is implicit").
- **Decision row:** ratified by the [PURA-66](/PURA/issues/PURA-66) Phase 2 epic and made concrete in [PURA-70](/PURA/issues/PURA-70) — board-authored task scope.

**What the deviation is.** The spec's WebSocket is push-only and stateless: the server pushes everything the recipient is authorised to see, and reconnections re-authenticate from scratch with no replay. Phase 2's hub adds two client-driven controls on top of the spec's envelope:

1. **Topic subscriptions** — clients send `subscribe`/`unsubscribe` JSON frames for topics `server:{id}:clients`, `server:{id}:channels`, `server:{id}:logs`, and `server:{id}:widget`. Per-topic authorisation runs on every subscribe. This contradicts §8.3's "no client-to-server message protocol".
2. **`last-event-id` reconnect replay** — clients reconnect with `last-event-id` in their first `subscribe` frame; the hub replays missed events from a small bounded ring buffer (per-server, ≤256 events). This contradicts §8.5's "MUST NOT keep client-specific state across reconnections" — the *server* state (ring buffer) is shared and bounded, not per-client, but it does enable resume semantics across the boundary §8.5 forbids.

**Why / ratification.** Two pressures: (a) the dashboard, control pages, and public widgets all need different events at different cadences, so an implicit "push everything authorised" channel is wasteful on bandwidth and on per-recipient filtering work; (b) Phase 2 widgets render in third-party embeds where transient network blips are common, and a tiny replay window keeps "live counts" widgets from showing brief gaps every reconnect. PURA-66 / PURA-70 ratified both as Phase 2 design moves.

**External-contract impact.**
- **Server→client envelope (§8.3):** preserved verbatim. Every server-pushed message is still `{ "type": "<event-name>", "data": <payload> }` plus the new `id` field for the resume contract — the spec envelope keys are unchanged.
- **`/ws?token=…` URL + JWT-rejection-with-401 (§8.1, §8.2):** preserved verbatim.
- **Client→server frames:** new — `{ "kind": "subscribe", "topic": "...", "lastEventId"?: <u64> }`, `{ "kind": "unsubscribe", "topic": "..." }`, `{ "kind": "ping" }`. Any other frame closes the connection (the §8.3 hardening clause stays in force for non-recognised frames).
- **Implicit-subscription rule (§8.5):** dropped. Clients now opt in per topic; the server pushes nothing until a subscribe lands.
- **Widget topic:** authorises via the existing widget token (§4.2.15 / §7.28) instead of a JWT. The handshake URL stays `/ws?token=<widget-token>` for widget connections — the token type is detected by lookup order (try JWT first, fall back to widget token).
- **`bot:execution:*` / `voice:*` / `video:*` / `ts:event` types (§8.4):** preserved verbatim for content. The hub will route them as topic-prefixed events on top of the same envelope — the `type` field stays exactly what §8.4 specifies.

**Cleanroom note.** Preserved verbatim despite this deviation: the §8.3 envelope `type`/`data` keys, every event name in §8.4, the `/ws?token=…` URL shape, the §8.2 401-on-invalid behaviour. The deviation is purely additive — `subscribe`-style frames extend the protocol; the implicit-fan-out behaviour is replaced by an explicit one. A Phase 1 client that connected and listened (without subscribing) would receive nothing under D-WS — this is intentional and the FE Phase 2 dashboard wiring (PURA-73) ships the subscribe call alongside.

---

## D-SSH-AUTH — TsServerConfig: per-server control-path + SSH auth-method selector

- **Spec reference:** §4.2.4 TsServerConfig (the canonical column list — `sshUsername` + `sshPassword` only); §4.4 migrations history (the spec ships two append-only nickname migrations on top of `server_connection`); §6.3.2 credential envelope (`enc:<iv>:<tag>:<ct>`); §7.5 `/api/servers` response shape.
- **Decision row:** ratified by [PURA-69](/PURA/issues/PURA-69) (SSHBridge foundation) and made concrete by this follow-up [PURA-77](/PURA/issues/PURA-77).

**What the deviation is.** Spec §4.2.4 only allows password-based SSH for `server_connection` rows (`sshUsername` + `sshPassword`). The clean-room implementation keeps password as one option and adds **two more authentication shapes** plus a **per-server backend selector**, all surfaced as new columns on the same table:

- `controlPath` — string enum, default `'webquery'`, also accepts `'ssh'`. Drives the per-server backend selector consumed by the SSHBridge follow-up that wires russh into the existing WebQueryPool fan-out.
- `sshAuthMethod` — string enum, default `'password'`, also accepts `'agent'` and `'key'`. Selects which credential variant the russh transport pulls at connect time. `'password'` keeps the existing `sshPassword` ciphertext path verbatim.
- `sshPrivateKey` — `option<string>`. AES-256-GCM ciphertext (`enc:` envelope per §6.3.2) of the operator-supplied private key. NULL unless `sshAuthMethod = 'key'`.
- `sshKeyAgentSocket` — `option<string>`. Filesystem path to the operator-supplied `SSH_AUTH_SOCK`. NULL unless `sshAuthMethod = 'agent'`. Stored plaintext — it is a path, not a credential, and the agent socket itself is the trust anchor.
- `sshHostKeyFingerprint` — `option<string>`. Used by SSHBridge's strict-fingerprint host-key verifier (parent-issue follow-up A); NULL means accept-on-first-use is allowed.

**Why / ratification.** Parent PURA-69 mandated "default to ssh-agent or an encrypted-at-rest private key using the existing AES-256-GCM credential envelope" — a hardening pass on top of the spec's password-only model, since plaintext SSH passwords (even sealed at rest) are a weaker posture than agent or key auth for production deployments. PURA-77 is the schema half of that hardening; the russh consumer side and the REST surface are split into sibling follow-ups.

**External-contract impact.**
- **Spec §4.2.4 entity definition:** **deviation** — the `server_connection` table gains five fields the spec does not list. This is an append-only schema change; spec columns retain their names and types verbatim (`sshUsername`, `sshPassword` continue to mean what §4.2.4 says).
- **Spec §4.4 migrations history:** **deviation** — the reference ships migrations 0002 and 0003 on `server_connection`. The clean-room implementation adds a fourth (`0005_ssh_bridge_auth`) on the same table. Operator-visible: a fresh `cargo run -- migrate` against an existing reference DB applies the new columns with their defaults.
- **Spec §7.5 `/api/servers` response shape:** **preserved verbatim for now.** The new columns are NOT added to `ServerSummary`. `controlPath`, `sshAuthMethod`, `sshKeyAgentSocket`, `sshHostKeyFingerprint`, and `sshPrivateKey` (sealed or otherwise) MUST stay off the wire until SecurityEngineer signs off on which subset (likely the four non-secret ones plus a `hasSshPrivateKey: bool` boolean) is safe to expose. `hasSshCredentials` keeps its existing `!!sshUsername` semantics.
- **Spec §6.3.2 envelope:** **preserved verbatim.** `sshPrivateKey` is sealed with the same `enc:<iv>:<tag>:<ct>` shape and the same process-wide AEAD key as `apiKey` and `sshPassword` — no new key, no new envelope variant, no new KDF.
- **Defaults:** `controlPath='webquery'` and `sshAuthMethod='password'` keep every existing row functionally identical to its pre-migration shape; an operator who never opts into SSH or never opts into key/agent auth sees no behaviour change.

**Cleanroom note.** Preserved verbatim despite this deviation: every spec §4.2.4 field name and type, the `enc:<iv>:<tag>:<ct>` ciphertext shape (§6.3.2), the `/api/servers` response shape (§7.5) for as long as SecurityEngineer's audit gates the new fields, the `hasSshCredentials` boolean's spec wording. The deviation is additive on the persistence layer and gated on the wire layer — clients that only know the spec's `ServerSummary` keep working, and operators who only ever supply `sshPassword` keep working with no migration toil beyond the schema bump itself.

**Status.** Schema-side: shipped with PURA-77 (`0005_ssh_bridge_auth.surql`). Wire-surface change: gated on SecurityEngineer review before any of the new fields are added to `/api/servers`. russh consumer side: scoped to PURA-69 follow-up A. REST surface for managing key/agent auth: scoped to PURA-69 follow-up C.
