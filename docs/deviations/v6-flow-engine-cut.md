---
status: resolved
---

# Chapter 1 deviation — V6 (flow trigger) cut from v1.0

## Resolution (2026-05-16)

**Status: `resolved`.** V6 is back on the Chapter 1 verification matrix and gated.

The flow engine deferred below was designed, implemented, and gated for v1.1:

- **Design:** [PURA-198](/PURA/issues/PURA-198) — flow-engine design brief
  (`docs/flows/architecture.md`, `http-api.md`, `ui-brief.md`,
  [`v1.1-gate.md`](../flows/v1.1-gate.md)).
- **Implementation:** engine + HTTP routes ([PURA-241](/PURA/issues/PURA-241),
  [PURA-242](/PURA/issues/PURA-242)), flows UI ([PURA-243](/PURA/issues/PURA-243)),
  trigger dispatcher ([PURA-249](/PURA/issues/PURA-249)). Surface:
  `crates/ts6-manager-server/src/flow/{engine,routes,trigger}.rs`,
  `crates/ts6-manager-server/src/ui/pages/flows/`, `crates/shared/src/flows.rs`.
- **Gate probe:** `scripts/ws-gate/v6-probe.sh` ([PURA-244](/PURA/issues/PURA-244))
  — the four-step create → enable → fire → observe probe from
  [`v1.1-gate.md`](../flows/v1.1-gate.md) §2. It ran **green** against a
  rootless-Podman deploy of the v1.1-gate image; evidence under
  `qa-evidence/ws-gate/v6/<ISO8601-UTC>/`.
- **Matrix:** the V6 row is restored to the Chapter 1 verification matrix in
  [`docs/phase6/readiness-audit.md`](../phase6/readiness-audit.md) §1, marked `pass`.

Per [`v1.1-gate.md`](../flows/v1.1-gate.md) §1, a green v6-probe is the only
condition for this reclassification, and it is met. The v1.1 release tag/notes
are tracked separately under [PURA-255](/PURA/issues/PURA-255) (merge the v1.1
flow stack to `main`); this note gets the release-tag link once v1.1 ships.

This note is **retained** for historical traceability — same policy class as the
D6 and D8 deviations — so future readers can see how the v1.0 wedge call was made
and unwound. It is **not deleted**, per [`v1.1-gate.md`](../flows/v1.1-gate.md) §4
and `docs/flows/architecture.md` §11.4.

The original v1.0 deviation record follows unchanged.

---

- **Date:** 2026-05-14.
- **Decision authority:** Board (CEO ratified on [PURA-195](/PURA/issues/PURA-195) [comment-2cd44510](/PURA/issues/PURA-195#comment-2cd44510-0146-4321-bfbb-2c0f4449dc98)).
- **Scope of deviation:** spec §1.5 verification 6 — *"Define a trivial flow, enable it, observe it triggers"*, plus Chapter 31 (flow engine surface).

## What is cut

V6 is removed from the v1.0 WS-Gate matrix. The flow engine — HTTP routes, persistence wire-through, trigger evaluation, and UI — does not ship in v1.0.

## Why

- The release artefact (`v0.1.0-rc1`, image digest `sha256:5399bc5c029fef29ac62fc44ec5ab2fa8b66538ebd27dd455642584a68ce2209` per [PURA-164](/PURA/issues/PURA-164) deploy-receipt §1) merges 11 routers — `auth`, `setup`, `servers`, `ws`, `dashboard`, `control`, `video_sources`, `metrics`, `music_bots`, `widget`, `widget_admin` — and contains no `flows_router`. The directory `crates/ts6-manager-server/src/flow/` does not exist in the release artefact. Only `crates/ts6-manager-server/src/repos/bot_flows.rs` exists, and it is referenced only by `repos/mod.rs` and `repos/tests_chapter4.rs` — no axum router merges it and no Dioxus UI renders it.
- QA probed [PURA-181](/PURA/issues/PURA-181)/[PURA-182](/PURA/issues/PURA-182): `GET /api/flows` → 404, `POST /api/flows {…}` → 404, `GET /api/flow` → 404, `GET /api/bot-flows` → 404, `GET /api/automations` → 404.
- The Phase 6 readiness audit ([`docs/phase6/readiness-audit.md`](../phase6/readiness-audit.md) §1) flagged V6 as `partial` pre-tag. V6 is therefore **not a regression** — it never landed.
- The TeamSpeak-vs-Discord wedge for v1.0 is voice + provisioning + dashboards + music-bots + embeddable widgets. All are working in `v0.1.0-rc1`. Flow automation is a value-add, not a wedge feature.
- A re-spin of `v0.1.0-rc2` to land an unaudited Phase-3 spike would cost 1–2 weeks of focused RustPlatform work against no design in repo. Better to ship the wedge now and design flows properly post-v1.0.

## Disposition

- v1.0 ships with a **6-row WS-Gate** (V1, V2, V3, V4, V5, V7).
- v1.0 release notes call out: *"Flow automation deferred to v1.x — Chapter 1 ecosystem ships without it."* The release-notes wording is staged internally only until the board lifts the [PURA-136](/PURA/issues/PURA-136) overnight moratorium.
- The flow-engine design brief is opened as a v1.1 child under [PURA-155](/PURA/issues/PURA-155). Design spec first, no rushed implementation.
- [PURA-189](/PURA/issues/PURA-189) (WS-Gate B3) is superseded by this decision and closes against [PURA-195](/PURA/issues/PURA-195).
- [PURA-189](/PURA/issues/PURA-189) is removed from [PURA-192](/PURA/issues/PURA-192)'s `blockedBy` set — it is no longer a gate blocker.

## Policy class

Same class as D6 (MoQ draft pin) and D8 (SurrealDB embedded backend): a deviation from the spec that is board-ratified and recorded in `docs/deviations/`.

## Self-host story

A community operator can stand up a server, provision a virtual server, manage clients, run a music-bot, and embed a widget without flows. The v1.0 self-host promise is intact.

## References

- Decision: [PURA-195](/PURA/issues/PURA-195) (CEO comment 2cd44510-0146-4321-bfbb-2c0f4449dc98, 2026-05-14T17:32Z).
- Parent triage: [PURA-192](/PURA/issues/PURA-192).
- QA evidence: [PURA-181](/PURA/issues/PURA-181), [PURA-182](/PURA/issues/PURA-182).
- Phase 6 audit: [`docs/phase6/readiness-audit.md`](../phase6/readiness-audit.md) §1.
- Phase 6 epic: [PURA-155](/PURA/issues/PURA-155).
- Moratorium: [PURA-136](/PURA/issues/PURA-136) (overnight outbound-posts freeze; release-notes wording stays internal until lifted).
