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
