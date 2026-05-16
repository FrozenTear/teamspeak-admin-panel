# Phase 6 readiness audit (WS-0)

- **Status:** Drafted by CTO under [PURA-156](/PURA/issues/PURA-156).
- **Date:** 2026-05-14.
- **Scope of gate:** v1.0 release. Phase 6 epic — [PURA-155](/PURA/issues/PURA-155).
- **Base commit surveyed:** `885f3f3` (PURA-154 music-bot-audio wire), `main`. The wake payload for [PURA-156](/PURA/issues/PURA-156) named `46e7e3c` as the "last green commit on `main`"; that commit was actually the WS-3 IP-pin revert ([PURA-149](/PURA/issues/PURA-149)). Phase 5 was merged in `c56f84c` ([PURA-152](/PURA/issues/PURA-152)) and the Phase 4 carry-over [PURA-154](/PURA/issues/PURA-154) landed on top in `885f3f3`. Audit is taken against `885f3f3` because that is where Chapter 1 verification 5 (music bot streams audible audio) first becomes potentially live.

This document is the gate output for WS-0. It is documentation + inventory only — no new code is in scope under this audit. Child workstreams (WS-OPS-\*, WS-Security, WS-Perf, WS-Runbook, WS-Gate) are filed on [PURA-155](/PURA/issues/PURA-155) after this audit lands so they reflect *actual* state, not the wake-payload sketch.

---

## 1. Chapter 1 verification matrix

The seven verifications are quoted from `study-documents/ts6-manager-spec.md` §1.5. **State** values:

- **pass** — wired end-to-end and verified against a real TS6 server at a phase gate.
- **partial** — wired end-to-end against the dev fixture, but not yet exercised against a fresh rootless Podman deploy of the v1.0 release artefact (which is what Phase 6 §5 calls for).
- **fail** — known broken or unimplemented.
- **not yet exercised** — code path exists but no verification artefact records it passing.
- **cut for v1.0** — verification deferred to a post-v1.0 release; deviation recorded in the gate scope row below.

> **Scope update (2026-05-14):** V6 (flow trigger) is **cut from the v1.0 WS-Gate matrix** per board decision on [PURA-195](/PURA/issues/PURA-195). [PURA-181](/PURA/issues/PURA-181)/[PURA-182](/PURA/issues/PURA-182) probes against `v0.1.0-rc1` confirmed the release artefact merges 11 routers — `auth`, `setup`, `servers`, `ws`, `dashboard`, `control`, `video_sources`, `metrics`, `music_bots`, `widget`, `widget_admin` — with no `flows_router` and no `crates/ts6-manager-server/src/flow/`. The gap was flagged as `partial` here pre-tag and never landed (not a regression). v1.0 ships with a **6-row gate (V1, V2, V3, V4, V5, V7)**; flow automation defers to v1.1 with a clean design brief tracked under [PURA-155](/PURA/issues/PURA-155). The V6 row below is retained for traceability and marked `cut for v1.0`.
>
> **Scope update (2026-05-16):** V6 is **resolved for v1.1**. The flow engine + routes + UI shipped per [PURA-198](/PURA/issues/PURA-198) and the four-step gate probe `scripts/ws-gate/v6-probe.sh` ([PURA-244](/PURA/issues/PURA-244)) ran green against a rootless-Podman v1.1-gate image. The V6 row below is re-marked `pass (v1.1)` and the gate now runs **seven rows**. The deviation note [`docs/deviations/v6-flow-engine-cut.md`](../deviations/v6-flow-engine-cut.md) is reclassified `status: resolved` (retained for traceability per [`v1.1-gate.md`](../flows/v1.1-gate.md) §4). v1.1 release tagging is tracked under [PURA-255](/PURA/issues/PURA-255).

| # | Verification (spec §1.5) | State | Artefact location | Blocking gaps for v1.0 |
|---|---|---|---|---|
| 1 | Log in to the web UI | partial | `crates/ts6-manager-server/src/auth/` (login + JWT); FE at `crates/ts6-manager-server/src/ui/pages/login.rs`; Phase 1 gate [PURA-74](/PURA/issues/PURA-74). | Not yet exercised against a fresh rootless Podman deploy of the release OCI image. WS-Gate dependency. |
| 2 | Add a TeamSpeak server connection (host, WebQuery port, API key, optional SSH credentials) | partial | `crates/ts6-manager-server/src/routes/servers.rs`; setup wizard at `crates/ts6-manager-server/src/ui/pages/setup.rs`; Phase 1 gate [PURA-74](/PURA/issues/PURA-74). | Same as V1 — needs fresh-deploy gate run. |
| 3 | Read the live dashboard for that server | partial | `crates/ts6-manager-server/src/routes/dashboard.rs` + WS hub tick republisher [PURA-81](/PURA/issues/PURA-81) (still `in_review`). | [PURA-81](/PURA/issues/PURA-81) is in review — fold close-out into WS-Security or WS-Runbook before the gate. Otherwise needs fresh-deploy gate run. |
| 4 | Issue at least one mutating action against the TS server (e.g., kick a client) | pass (against fixture) → partial (against gate deploy) | Phase 2 epic [PURA-66](/PURA/issues/PURA-66); QA evidence [PURA-74](/PURA/issues/PURA-74) ran V4 against the real TS6 fixture. | Needs WS-Gate rerun against the v1.0 image; otherwise wired. Finding A fixup tracked under [PURA-193](/PURA/issues/PURA-193) / [PURA-196](/PURA/issues/PURA-196). |
| 5 | Connect a music bot to a channel and stream a known-good radio URL | not yet exercised | Voice path: `crates/voice/` (forked tsproto); pipeline: `crates/music-bot-audio/`; wire: [PURA-154](/PURA/issues/PURA-154) commit `885f3f3` (`crates/music-bot-audio/src/pipeline.rs` → `bot.send_audio`). | **Highest-risk gate.** [PURA-154](/PURA/issues/PURA-154) only wired the call site; the end-to-end "audible in a real TS voice client" round-trip is unproven post-wire. Needs a `!radio` smoke against the TS6 fixture + a real TS voice client, then the same against the gate deploy. |
| 6 | Define a trivial flow, enable it, observe it triggers | pass (v1.1) | Engine + routes + trigger: `crates/ts6-manager-server/src/flow/{engine,routes,trigger}.rs`; UI: `crates/ts6-manager-server/src/ui/pages/flows/`; shared types: `crates/shared/src/flows.rs`. Gate probe: `scripts/ws-gate/v6-probe.sh` ([PURA-244](/PURA/issues/PURA-244), green). | Engine + UI shipped per [PURA-198](/PURA/issues/PURA-198). Probe: `scripts/ws-gate/v6-probe.sh` against the v1.1 image. |
| 6g | Graph flow — fire a multi-node graph (`transform → branch → parallel(sub-flow) → join`) and assert the per-node outcome: taken branch `ok`, pruned branches `skipped`, parallel paths `ok`, run `ok` | not yet exercised | Graph/node flow engine shipped per [PURA-259](/PURA/issues/PURA-259) — engine: `crates/ts6-manager-server/src/flow/engine/graph.rs` ([PURA-266](/PURA/issues/PURA-266)); wire types: `crates/shared/src/flows/v2.rs` ([PURA-265](/PURA/issues/PURA-265)). Probe: `scripts/ws-gate/v6-graph-probe.sh` against the v2 image — branch + parallel + per-node assertions ([PURA-268](/PURA/issues/PURA-268)). | Probe is committed and `shellcheck`-clean but not yet run green: the v2 HTTP REST surface it drives (`POST /api/flows/validate`, the `FlowSpec` graph body on `POST /api/flows`, `GET /api/flows/{id}/runs/{runId}` with `nodeResults`) is **not yet merged** — `crates/ts6-manager-server/src/flow/routes.rs` is still the v1.1 router (PURA-266 landed the engine, not its routes). V6g goes green once the v2 REST surface lands and the probe runs against a rootless-Podman v2 deploy. |
| 7 | Generate a public widget URL; load as SVG/PNG/JSON unauthenticated | partial | `crates/ts6-manager-server/src/widgets/` (SVG `themes.rs`, PNG via resvg per [PURA-88](/PURA/issues/PURA-88), CRUD per [PURA-89](/PURA/issues/PURA-89)); Phase 2 gate [PURA-74](/PURA/issues/PURA-74) covered V7 against fixture. | Needs WS-Gate rerun against the v1.0 image. Public widget video player ([PURA-146](/PURA/issues/PURA-146)) is *additional* but not on the V7 gate path. |

**Summary read:** the v1.0 gate ran **six rows** (V1, V2, V3, V4, V5, V7); with V6 resolved for v1.1 and the v2 graph-flow row V6g added under [PURA-259](/PURA/issues/PURA-259) the matrix now stands at **eight rows**. V6g is `not yet exercised` — the `v6-graph-probe.sh` probe is committed but the v2 HTTP REST surface it drives is not yet merged (see the V6g blocking-gaps cell). Of the v1.0 six, V1/V2/V3/V4/V7 are functionally wired against the local fixture and ran green at the Phase 1/2 gates ([PURA-74](/PURA/issues/PURA-74)); V5 (music bot) is the highest-risk gate because the wire landed in `885f3f3` and has not had an audible end-to-end smoke. V6 (flow trigger) was **cut from v1.0** per [PURA-195](/PURA/issues/PURA-195) — the engine never landed in the v1.0 release artefact — and is now **resolved for v1.1**: engine + UI shipped per [PURA-198](/PURA/issues/PURA-198) and the gate probe `scripts/ws-gate/v6-probe.sh` ([PURA-244](/PURA/issues/PURA-244)) ran green. None of the remaining six has been run against a *fresh rootless Podman deploy of a release-tagged OCI image* — that is what Phase 6 §5 calls the v1.0 gate. The audit's main finding is that the Chapter 1 verifications, taken individually, are mostly ready; the gate that *combines* them under release packaging conditions is the unbuilt artefact.

---

## 2. Current-state inventory

| Component | What works | What doesn't | Source of truth |
|---|---|---|---|
| **Signaling / TS6 manager** | REST surface (auth, servers, channels, clients, server-groups, channel-groups, permissions, bans, tokens, complaints, messages, server-logs, files, instance-settings, virtual-servers), WS hub with metrics ([PURA-82](/PURA/issues/PURA-82)), SSH event bridge ([PURA-80](/PURA/issues/PURA-80)), WebQuery client. | None functionally; release packaging only. | `crates/ts6-manager-server/src/{auth,routes,sshbridge,webquery,ws}/`. |
| **Voice path (Opus, SRT, sidecar)** | TS6 voice handshake against `teamspeak6-server:6.0.0-beta9` fixture (Phase 4 epic); Opus encode pipeline (`crates/music-bot-audio/src/encoder.rs`, `pacer.rs`); music-bot → `bot.send_audio` wire ([PURA-154](/PURA/issues/PURA-154)); LiveKit↔TS6 translator binary at `crates/ts6-voice-translator/`. | No audible-in-real-client smoke since the [PURA-154](/PURA/issues/PURA-154) wire; voice latency budget (`docs/voice/0001-latency-budget.md`) is targets, not measurements. Note: "SRT" in the wake checklist is a mis-spec — the voice path uses Opus over RTP into the TS6 handshake, not SRT. Audited as Opus + RTP. |
| **WS-3 outbound HTTP pin status** | `crates/ts6-ssrf` (IP normalization, blocklist, async DNS resolver); sidecar `GaiResolver` (`crates/ts6-media-sidecar/src/ssrf_resolver.rs`); URL→host validation runs *before* FFmpeg spawn ([PURA-141](/PURA/issues/PURA-141)); URL→IP-literal rewrite was correctly reverted in [PURA-149](/PURA/issues/PURA-149) (TLS SNI / virtual-hosted CDNs would break). | **R6 still open.** FFmpeg itself performs the outbound DNS at fetch time, so the resolved IP we validated and the IP FFmpeg actually connects to can diverge (DNS rebinding window). [PURA-150](/PURA/issues/PURA-150) is the planned Rust-side reqwest proxy that preserves `Host:` while pinning the socket address. Currently `backlog`. |
| **Rootless podman story** | `Containerfile.fullstack` builds and runs; `podman-compose.yml` boots end-to-end with **named volumes** workaround ([PURA-67](/PURA/issues/PURA-67) deviation), confirmed in `study-documents/ts6-manager-impl-plan.md` §9 callout; `docs/ts6-fixture.md` documents the required `--network=host` for the upstream TS6 fixture. | **OPS deliverables almost entirely missing.** No `deploy/quadlet/*.container`, no `deploy/quadlet/ts6-manager.pod`, no `deploy/kube/ts6-manager.yaml`, no `Containerfile.sidecar` (sidecar still built from source via cargo), no `bin/` for pre-built sidecar binaries, no CI image push to GHCR, no `LICENSE` file (OD7 unenforced), no `.github/workflows/` (OD2 unenforced). The "self-host in <10 minutes from public images" wedge is unsupported today. |
| **Auth — refresh-token reuse detection (R5)** | Full spec §6.5 + §6.6 behaviour in `crates/ts6-manager-server/src/auth/refresh.rs`: family ids, predecessor-preserved reuse check, family-wide revocation on reuse, cross-user isolation. Unit tests cover the canonical reuse, cross-family non-revocation, and revoked-family rejection paths. | No fuzz / property-test harness on token format, family id collisions, or concurrent rotation. Treat as **R5 = covered for the spec contract**; recommend a short fuzz pass in WS-Security as defence-in-depth, not a blocker. |
| **SurrealDB error boundaries (R8 / D8)** | D8 deviation captured in `crates/ts6-manager-server/src/db/mod.rs` and `config.rs:10`; the spec's SQLite-full / `SQLITE_FULL` / sqlx boundaries are remapped onto SurrealKV embedded backend. Migrations runner is in place ([PURA-133](/PURA/issues/PURA-133)). | **No explicit three-state mapping in code.** Impl-plan calls for **write-failure / transaction-conflict / capacity-pressure** boundaries with error-injection tests against SurrealDB error variants. Today the DB layer returns `anyhow::Result` and bubbles SurrealDB errors raw; no boundary detection in repos. This is the dominant R8 carry-over for v1.0. |
| **ServerQuery escaper (R7)** | Spec §10.4 escape table implemented in `crates/ts6-manager-server/src/webquery/escape.rs` with `escape` + `unescape` + unit tests for the documented escape characters. Used by the SSH bridge ([PURA-80](/PURA/issues/PURA-80)) per `crates/ts6-manager-server/src/repos/ssh_audit_log.rs:30`. | No fuzzer / roundtrip property test against the spec's escape rules (`cargo-fuzz` harness not present). R7 says "Fuzz the escaper against the spec rules; add a roundtrip test" — roundtrip is implicit in the unit tests but not asserted on random input. Small-cost fix; flag for WS-Security. |
| **Audio / music bot (post-[PURA-154](/PURA/issues/PURA-154))** | Pipeline → bot wire shipped in `885f3f3`; `!radio`, `!play`, `!stop`, `!pause`, `!skip`, `!prev`, `!vol`, `!np` chat commands per `crates/voice/src/chat.rs` + spec Chapter 22; Icecast/HLS source path in `crates/music-bot-audio/src/icy.rs` + `source/`. | Verification 5 (audible in real TS voice client) not yet smoke-tested under the post-wire codebase. Highest-priority pre-gate smoke: spin up the TS6 fixture, point a music bot at a known-good radio URL, confirm audible Opus playout into a connected TS voice client. |

**Stragglers / parked items (from [PURA-155](/PURA/issues/PURA-155)):**

- [PURA-81](/PURA/issues/PURA-81) Phase 2 dashboard tick republisher — `in_review`. Close it under WS-Security or fold into WS-Runbook before the gate run.
- [PURA-131](/PURA/issues/PURA-131) headless-probe deadlock — low priority, deferred. No Phase 6 dependency.
- [PURA-8](/PURA/issues/PURA-8) OD1/OD2/OD7 — see §3 below.

---

## 3. Carry-over closure — [PURA-8](/PURA/issues/PURA-8) OD1/OD2/OD7

[PURA-8](/PURA/issues/PURA-8) has been `in_review` since 2026-05-07. Board ratified the three recommendations on the issue thread; the question is whether the codebase actually reflects them.

| Open decision | Board decision (2026-05-07) | Codebase reality (2026-05-14) | Disposition for [PURA-8](/PURA/issues/PURA-8) |
|---|---|---|---|
| **OD1** — workspace layout | Single Cargo workspace: `crates/server`, `crates/web`, `crates/shared`, `crates/voice-bot`, `crates/tsproto-fork`. | Single workspace confirmed. Layout has *diverged* in naming: `crates/ts6-manager-server`, `crates/shared`, `crates/voice`, `crates/music-bot-audio`, `crates/ts6-media-sidecar`, `crates/ts6-ssrf`, `crates/ts6-voice-fixture`, `crates/ts6-voice-prototype`, `crates/ts6-voice-translator`. The decision (single workspace) holds; the per-crate naming evolved during Phases 1–5 and is fine. | **Close OD1.** |
| **OD2** — CI host | GitHub Actions; images to GHCR. | No `.github/workflows/` directory exists. No images are pushed anywhere. | **Carry into WS-OPS-Images (and WS-OPS-CI).** Needs a real workflow before the gate. |
| **OD7** — license | MIT; `LICENSE` file at repo root. | No `LICENSE` file at repo root. | **Carry into WS-OPS-Images.** Required before any public OCI image push (re-distributing the tsproto fork without a `LICENSE` is contract-violating). |

**Recommended action on [PURA-8](/PURA/issues/PURA-8):** close as `done` with a comment that links this audit, then file two scoped follow-ups under [PURA-155](/PURA/issues/PURA-155) (one for OD2 = CI bring-up, one for OD7 = LICENSE add) so the remaining work is tracked against Phase 6, not against the now-stale open-decisions ticket.

(The two still-open items from [PURA-8](/PURA/issues/PURA-8)'s last CTO comment — OD3 verification target = `scuffedcrew.no`, and OD6 cleanroom audit process — are *not* v1.0 gate blockers and stay parked on [PURA-8](/PURA/issues/PURA-8) for separate board response.)

---

## 4. Risk-register diff (impl-plan §6 vs. end of Phase 5)

| ID | Risk | Status @ Phase 5 ship | Status today (2026-05-14) | Δ |
|---|---|---|---|---|
| R1 | TS6 handshake delta | Bridged by `tsclientlib` fork ([PURA-101](/PURA/issues/PURA-101)) | Music-bot Opus path wired ([PURA-154](/PURA/issues/PURA-154)); V5 unverified end-to-end | **No regression; verification needed.** |
| R2 | MoQ draft churn | Pinned to `moq-dev/moq` + `moq-lite` ([ADR-0007](docs/adr/0007-moq-flavor-and-draft-pin.md)) | Same pin; no draft bump scheduled | Unchanged. |
| R3 | iOS / Safari lacks WebTransport | Documented exclusion; Helium 148 (Chromium) is the supported viewer | Same | Unchanged. |
| R4 | Dioxus flow-canvas | FE-EDITOR shipped through Phase 3 | Same; no regression observed | Unchanged. |
| R5 | Refresh-token reuse-detection bug | Implementation + unit tests landed Phase 1 | Same; no fuzz harness | **R5 = covered, with a fuzz-pass nice-to-have.** Recommend dropping to R5-low. |
| R6 | SSRF blocklist gaps (DNS rebinding, IPv6 link-local, octal private) | `ts6-ssrf` crate + sidecar resolver shipped Phase 5 | **Still open at the FFmpeg fetch boundary** — [PURA-150](/PURA/issues/PURA-150) Rust-side reqwest pin is `backlog` | **R6 = still high.** Recommend bumping to **escalate to CEO** if WS-Security can't land [PURA-150](/PURA/issues/PURA-150) before the gate; this is the most operator-visible attack surface in v1.0. |
| R7 | ServerQuery escaper bug | Escape table + unit tests landed Phase 1 | Same; no fuzz / roundtrip property test | R7 partially covered. Recommend a one-day fuzz pass under WS-Security. |
| R8 | SurrealDB storage-full / boundary surface | D8 deviation ratified; SurrealKV embedded backend | **Three boundaries (write-failure / transaction-conflict / capacity-pressure) not mapped in code; no error-injection tests** | **R8 = unchanged from end of Phase 1.** This is the second-most-load-bearing security gap behind R6. Recommend dedicated workstream slice. |
| R9 | Synthetic SSH events drift | Phase 2 epic close-out | Same; no new spec deltas | Unchanged. |
| R10 | Dioxus WS scalability ceiling | Raw axum WS hub used per impl-plan; not hit | Same | Unchanged. |

**New risks surfaced by this audit (not in impl-plan §6):**

- **R11 — OPS deliverable cliff.** The Phase 6 `deploy/quadlet/`, `deploy/kube/`, `Containerfile.sidecar`, `bin/`, CI workflow, and `LICENSE` are all unstarted. Likelihood: certain. Impact: high (no path to "self-host in <10 minutes from public images"). Owner: WS-OPS-\* under IC.
- **R12 — Gate-run-against-release-image not exercised.** All Chapter 1 verifications have been run against the dev fixture, never against a release-tagged OCI image on a fresh host. Likelihood: certain. Impact: medium (likely surfaces hidden assumptions on `MUSIC_DIR`, `YT_COOKIE_FILE`, `ENCRYPTION_KEY` fallback, asset-bundle paths). Owner: WS-Gate.

**Escalate to CEO:** R6 (SSRF DNS-rebinding window via FFmpeg fetch) is the highest residual security risk for v1.0. Recommend the CEO/board confirm we **do not ship v1.0** without [PURA-150](/PURA/issues/PURA-150) closed.

---

## 5. Recommended sequencing

The impl-plan's [PURA-155](/PURA/issues/PURA-155) sequencing diagram routes everything through "IC onboards → WS-OPS-\* (parallel) | WS-Security (parallel) | WS-Perf | WS-Runbook | WS-Gate". The audit surfaces a different critical path: **R6 + R8 are the security blockers that must close before the gate, not in parallel with it**, and **OPS-Images / CI-LICENSE are themselves blockers for the gate** because there is no release artefact to gate against until then. The revised order is:

```
[WS-0 audit] ────> [WS-Hire] ──> IC onboards
                                     │
                                     ▼
        ┌────────────────────────────┴────────────────────────────┐
        │ Critical-path serial (gate-blocking)                    │
        │ ─────────────────────────────────                       │
        │  WS-OPS-CI-LICENSE (OD2 + OD7 + .github/workflows)      │
        │            │                                            │
        │            ▼                                            │
        │  WS-OPS-Quadlet  ┬─  WS-OPS-Sidecar-Containerfile-Image │
        │  WS-OPS-Kube     ┘                                      │
        │            │                                            │
        │            ▼                                            │
        │  WS-Security-R6 (PURA-150 Rust-side reqwest pin)        │
        │            │                                            │
        │            ▼                                            │
        │  WS-Security-R8 (SurrealDB three-boundary mapping       │
        │                  + error-injection tests)               │
        │            │                                            │
        │            ▼                                            │
        │  WS-Gate (7 verifications against the release image     │
        │           on a fresh rootless Podman host) ──► v1.0     │
        └─────────────────────────────────────────────────────────┘
                                     │
                                     ▼
        ┌─────────────────────────────────────────────────────────┐
        │ Parallel tracks (gate-supporting, not gate-blocking)    │
        │ ─────────────────────────────────                       │
        │  WS-Security-R7 (ServerQuery escaper fuzz)              │
        │  WS-Security-R5 (refresh-token fuzz; nice-to-have)      │
        │  WS-Perf (sidecar latency + sustained-load smoke)       │
        │  WS-Runbook (operator runbook + troubleshooting docs)   │
        │  Carry-overs from [PURA-8](/PURA/issues/PURA-8):        │
        │     close OD1 in the same WS-OPS-CI-LICENSE PR          │
        │  Close-out [PURA-81](/PURA/issues/PURA-81) under        │
        │     WS-Security or WS-Runbook                           │
        └─────────────────────────────────────────────────────────┘
```

**Why this differs from impl-plan §5 / [PURA-155](/PURA/issues/PURA-155) sequencing:**

1. The original sequencing parallelises WS-OPS-\* with WS-Security with WS-Perf. The audit shows R6 has no scaffold yet ([PURA-150](/PURA/issues/PURA-150) backlog) and R8 has *no* boundary mapping in code — these are not "polish" items. They must close before the gate.
2. WS-Gate cannot start without a release-tagged OCI image to deploy. WS-OPS-CI-LICENSE → WS-OPS-Quadlet/Kube → WS-OPS-Sidecar-Image is therefore a hard prerequisite of WS-Gate.
3. WS-Perf and WS-Runbook do not block the gate; they support it. Run in parallel.
4. WS-Security-R7 (ServerQuery escaper fuzz) and WS-Security-R5 (refresh-token fuzz) are defence-in-depth on already-covered risks. Run in parallel; do not block the gate.

**Estimate vs. impl-plan §5 "3–4 weeks":**

- WS-Hire: 3–5 days to draft role spec, route hire approval, onboard.
- WS-OPS-CI-LICENSE: 1–2 days (mechanical, the IC's onboarding PR).
- WS-OPS-Quadlet/Kube/Sidecar-Image: 3–5 days (mechanical but needs a real rootless host).
- WS-Security-R6 ([PURA-150](/PURA/issues/PURA-150)): 3–5 days (Rust-side reqwest proxy that preserves `Host:` while pinning the socket address — non-trivial because `reqwest` does not expose a connect-to-IP-but-send-this-`Host` knob directly; either patch `hyper` resolver or build a custom `Connector`).
- WS-Security-R8: 2–3 days (boundary detection on `surrealdb::Error` variants + error-injection tests).
- WS-Gate: 1–2 days, contingent on a real rootless Podman host (CEO's `scuffedcrew.no` candidate — see [PURA-8](/PURA/issues/PURA-8) OD3) or a fresh CI runner.

Critical path total: **2.5–4 weeks from IC start.** With IC ramp-up (1 week) and Phase 6's own carry-overs, expect **4–6 weeks** elapsed before v1.0 tag — consistent with [PURA-155](/PURA/issues/PURA-155)'s upper estimate.

---

## 6. Action items emerging from this audit

These are the child issues WS-0 expects to file on [PURA-155](/PURA/issues/PURA-155) after this audit lands. Filing is intentionally held until the audit doc is on `main` so the children reflect *actual* state, not the wake-payload sketch.

1. **WS-OPS-CI-LICENSE** — add `LICENSE` (MIT, per OD7) + `.github/workflows/ci.yml` (cargo build/test + dx build + OCI image push to GHCR per OD2). Folds OD2/OD7 closure of [PURA-8](/PURA/issues/PURA-8).
2. **WS-OPS-Sidecar-Containerfile-Image** — `Containerfile.sidecar` + image push, replace cargo-build-from-source assumption.
3. **WS-OPS-Quadlet** — `deploy/quadlet/*.container` + `deploy/quadlet/ts6-manager.pod`, rootless smoke test.
4. **WS-OPS-Kube** — `deploy/kube/ts6-manager.yaml`, `podman kube play` smoke.
5. **WS-OPS-Sidecar-Bin-Release** — pre-built sidecar binaries as release artefacts, `SIDECAR_BINARY_PATH` documented.
6. **WS-Security-R6** — close [PURA-150](/PURA/issues/PURA-150) (Rust-side reqwest proxy with Host-preserving IP pin). **Block WS-Gate on this.**
7. **WS-Security-R8** — SurrealDB three-boundary mapping (write-failure / transaction-conflict / capacity-pressure) + error-injection tests.
8. **WS-Security-R7** — ServerQuery escaper fuzz harness + roundtrip property test (parallel, non-blocking).
9. **WS-Security-R5-defense** — refresh-token reuse-detection fuzz pass (parallel, non-blocking, nice-to-have).
10. **WS-Perf** — sidecar latency + sustained-load smoke against fixture, parameter sweep for `SYNC_PLAYOUT_BUFFER_MS` / `AUDIO_DELAY_MS`.
11. **WS-Runbook** — `docs/runbook/` (operator install in <10 min, troubleshooting, common failures from [PURA-67](/PURA/issues/PURA-67) / [PURA-93](/PURA/issues/PURA-93) / [PURA-75](/PURA/issues/PURA-75) post-mortems).
12. **WS-Gate** — run the **six remaining Chapter 1 verifications** (V1, V2, V3, V4, V5, V7) against a fresh rootless Podman deploy of the v1.0 OCI image, on a real host. Tag v1.0 on green. Blocked by WS-OPS-\* and WS-Security-R6 + R8. V6 (flow trigger) is **cut from the v1.0 gate** per [PURA-195](/PURA/issues/PURA-195); flow engine ships in v1.1.
13. **Close-out** of [PURA-81](/PURA/issues/PURA-81) and [PURA-8](/PURA/issues/PURA-8) under WS-Runbook + WS-OPS-CI-LICENSE respectively.

---

## 7. Definition of done for this audit

- [x] Chapter 1 verification matrix (§1).
- [x] Current-state inventory (§2).
- [x] [PURA-8](/PURA/issues/PURA-8) OD1/OD2/OD7 carry-over closure (§3).
- [x] Risk-register diff vs. end of Phase 5 (§4).
- [x] Recommended sequencing (§5), explicitly superseding [PURA-155](/PURA/issues/PURA-155)'s parallel-WS layout for the gate-blocking subset.
- [x] Doc committed to `main`.
- [ ] Comment on [PURA-156](/PURA/issues/PURA-156) summarising findings + linking impl-plan sections (posted by CTO at audit close).
- [ ] Child issues on [PURA-155](/PURA/issues/PURA-155) filed *after* this audit lands.
