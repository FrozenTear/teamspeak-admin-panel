# ADR-0007 — MoQ flavor + draft pin for Phase 5 video sidecar

- **Status:** Accepted (two-tab VP8 smoke passed on Helium 148, PURA-138 heartbeat 3).
- **Date:** 2026-05-14.
- **Author:** CTO.
- **Reviewers:** CEO / board (gate before WS-1..WS-8 spawn under [PURA-136](/PURA/issues/PURA-136)).
- **Source issue:** [PURA-138](/PURA/issues/PURA-138).

## Context

[PURA-136](/PURA/issues/PURA-136) commits Phase 5 to a **MoQ + WebTransport
+ WebCodecs** video path (deviation D6 from the original spec, which
called for a WebRTC sidecar). The wedge against Discord-class
"watch-with-friends" is sub-second glass-to-glass latency at
small-group scale without forcing every operator to stand up a TURN +
SFU. MoQ over QUIC trades the WebRTC mesh for an explicit
publish/relay/subscribe pipeline that scales by adding relay nodes
rather than by full-meshing peers.

Before any sidecar code or frontend integration starts, three
risk-bearing questions had to be answered (WS-0, this ADR):

1. **Which Rust MoQ implementation do we pin against?** The MoQ
   ecosystem has three candidate Rust stacks at the time of writing.
   They are at different draft compliance levels, different licenses,
   and different stages of liveness. Picking wrong locks Phase 5 to a
   dead branch.
2. **Which MoQ transport / catalog draft do we pin to?** IETF
   moq-transport is at draft-17 (published 2026-05-01). Browser
   support lags. Picking a draft that no shipping browser speaks
   blocks the player; picking the bleeding edge blocks the Rust side.
3. **Does an HTML+JS reference player actually subscribe and render
   over current Chromium WebTransport + WebCodecs?** If WebCodecs
   gates the wedge (decoder availability, codec restrictions) we
   must know before we commit any FE work.

This ADR answers (1) and (2). The two-tab Chromium smoke is the
companion evidence required for [PURA-138](/PURA/issues/PURA-138)'s
GO/NO-GO on Phase 5; the spike artefacts live in
[`moq-spike/`](../../moq-spike/) and the result is posted as a
comment on [PURA-136](/PURA/issues/PURA-136).

## Survey

### Candidate Rust MoQ stacks

| Stack | Origin | Draft target | Latest release | License | Stance |
|---|---|---|---|---|---|
| [`moq-dev/moq`](https://github.com/moq-dev/moq) | Luke Curley (`@kixelated`, original MoQ author) — canonical home, formerly `kixelated/moq` | `moq-lite` (forwards-compatible subset of IETF moq-transport draft-14+) | `moq-ffi v0.2.9` on 2026-05-09 (5 days before this ADR); `moq-lite v0.16`, `moq-relay v0.11`, `moq-native v0.14`, `hang v0.16` on crates.io | MIT OR Apache-2.0 | **Active.** Lead-author-owned. Rust + TypeScript in one repo, so the relay/publisher and the reference player are draft-aligned by construction. Production deployments named (gameboy demo, `moq.dev`). |
| [`cloudflare/moq-rs`](https://github.com/cloudflare/moq-rs) | Cloudflare | Full IETF moq-transport draft-14 on `main`; draft-07 maintained on a side branch for Cloudflare's production deployment | `moq-relay-ietf v0.7.17` on 2026-04-13 | MIT OR Apache-2.0 | Active but **3 drafts behind upstream IETF** and no `moq-lite` subset. No paired browser stack — would force us to integrate against the kixelated TS player anyway. |
| Other `moq-*` Rust forks | n/a | mixed | varies | mixed | Surveyed via crates.io: nothing else has both relay + publisher + browser-aligned client beyond the two above. |

### Browser-side stack

- **WebTransport** is stable in Chromium ≥ 97 and shipping in Firefox
  ≥ 114. Our local QA browser is Helium 0.12.1.1 (Chromium
  148.0.7778.96), which carries the full WebTransport API surface
  including `unidirectionalStreams` and `incomingUnidirectionalStreams`
  required by moq-lite subscribers.
- **WebCodecs** ships in Chromium ≥ 94. The MDN-listed `VideoDecoder`
  codec strings exposed in Chromium 148 include `vp8`, `vp09.*`,
  `av01.*`, and `avc1.*`. Firefox WebCodecs is still flagged behind
  `dom.media.webcodecs.enabled` and gates AV1 / H.264 hardware
  decoders behind platform availability — we therefore exclude Firefox
  from the Phase-5 MVP browser matrix and revisit when Firefox
  WebCodecs lands by default. Zen (Firefox fork) inherits the same
  limitation.
- **Codec choice for the spike:** VP8 in IVF + Opus in Ogg. Rationale:
  (a) royalty-free, no decoder-availability risk on any Chromium
  build; (b) lowest WebCodecs barrier (no SPS/PPS extraction, no
  `description` parameter parsing); (c) `ffmpeg` produces an IVF
  stream of constant-frame-rate VP8 with one keyframe-per-second in a
  single command. VP9 / AV1 are upgrade paths once the pipeline
  proves out. H.264 is deferred until we have a clear operator story
  for the patent-licensing question.

## Decision

We pin Phase 5 to **`moq-dev/moq` + `moq-lite` + IETF moq-transport
draft-14+ forwards-compatible**.

Concretely:

- **Rust crates** (workspace-pinned via `cargo add`):
  - `moq-lite = "0.16"` — pub/sub transport, the protocol surface our
    sidecar talks to a relay through.
  - `moq-native = { version = "0.14", default-features = false, features = ["aws-lc-rs"] }`
    — Quinn QUIC endpoint helpers, certificate plumbing.
  - `moq-relay = "0.11"` *(spike-time dev-dependency only; we run it
    out-of-process for the two-tab smoke and do not link it into the
    sidecar at this stage)*.
  - `hang = "0.16"` — media-specific encoding layer (`hang::cmaf`
    helpers for catalog + group writer). Reserved for WS-2 once we
    replace the IVF fixture with FFmpeg subprocess output.
  - Rust toolchain: **stable, MSRV 1.85**; we are on 1.95.0 on the
    spike host.
- **Wire draft:** `draft-lcurley-moq-lite` (forwards-compatible with
  IETF `draft-ietf-moq-transport-14`). This is *not* the latest IETF
  draft (which is `-17` as of 2026-05-01) — moq-lite's contract is
  that it is a strict subset of `-14+`, and the upstream maintainers
  bump the subset only when they have re-validated the browser stack.
  We accept the lag explicitly: the wedge here is *deployability*, not
  draft completeness.
- **Browser-side library:** `@moq/lite` from npm (matched to the same
  release as the Rust `moq-lite = "0.16"`) for any future Dioxus
  integration. The WS-0 reference player vendors the protocol logic
  inline as plain JS so the spike has no build step (see
  `moq-spike/player/`).
- **Codec pin for WS-0 fixture:** VP8 video in `.ivf` + Opus audio in
  `.ogg`. Reset after WS-2 if FFmpeg subprocess produces a more
  practical codec.
- **TLS / QUIC dev posture:** self-signed certificate generated by
  `moq-native`'s `--tls-self-sign` flag; Chromium tabs launched with
  `--ignore-certificate-errors-spki-list=<sha256>` per the
  `moq-spike/README.md` recipe. No automated cert management at the
  spike stage — that is a WS-7 / operator-experience concern.

## Risks and mitigations

- **R2 (impl-plan): MoQ draft churn.** Mitigated by pinning *both*
  ends to the moq-lite subset rather than to the bleeding IETF draft,
  and by upgrading only on planned revs aligned with `moq-dev/moq`
  releases. We track moq-dev releases via the `moq-lite` crates.io
  page; each rev bump becomes its own ticket on the Phase-5 epic, not
  an opportunistic patch.
- **moq-lite is a subset, not full moq-transport.** If a future
  use-case (e.g. broadcast-grade restream into IETF-compliant CDNs)
  needs full draft-17, we either upstream a moq-lite extension or
  fork into a parallel sidecar; we do *not* swap implementations
  late in Phase 5.
- **Chromium-only Phase-5 MVP browser matrix.** Firefox / Zen users
  will see a graceful "codec unavailable" message at the player
  boundary. We revisit when Firefox WebCodecs lands by default.
- **No viable Rust impl** (the explicit escalation case in
  [PURA-138](/PURA/issues/PURA-138)): not triggered. `moq-dev/moq` is
  actively maintained, lead-author-owned, dual-licensed, and shipping
  releases within the last week.
- **Self-signed cert UX in Chromium.** Documented in
  `moq-spike/README.md` as a manual SPKI flag at the spike stage; not
  a blocker for go/no-go.

## Planned-upgrade cadence

- **Patch revs** (`moq-lite` `0.16.x`): apply on each release without
  ceremony, regression-tested against the player.
- **Minor revs** (`moq-lite` `0.17` etc.): one ticket per bump, with
  explicit re-validation of the two-tab smoke (`moq-spike` is the
  permanent regression artefact).
- **Major / draft-number bumps** (e.g. moq-lite moves from
  moq-transport-14+ to -17+): treat as a Phase-5 architecture review,
  not a routine bump. Re-run this ADR's survey before adopting.

## Alternatives considered

- **`cloudflare/moq-rs`**: rejected — three drafts behind upstream
  IETF, no paired browser stack, no `moq-lite` subset. We would still
  have ended up integrating against the kixelated TS player to get a
  browser side, which would have meant managing a draft-version skew
  between the relay and the player — exactly the risk this ADR is
  trying to retire.
- **Bespoke MoQ implementation**: rejected against engineering
  doctrine ("open standards first; reach for off-the-shelf before
  building bespoke voice or auth stacks"). MoQ is exactly the kind of
  protocol where being early-to-implement gives no wedge.
- **WebRTC sidecar (the original spec path)**: rejected at the Phase 5
  scoping stage; deviation D6 in impl-plan §6 documents why MoQ wins
  on small-group sub-second latency without TURN. Not re-litigated
  here.
- **Pin to IETF `draft-ietf-moq-transport-17` directly via
  `cloudflare/moq-rs` `main`**: rejected — Cloudflare main is on
  draft-14, not -17, so this would not actually deliver -17 anyway.

## Definition of done for this ADR

- [x] Survey of candidate stacks (above).
- [x] Crate + draft pins, codec pin, browser pin, TLS posture
  (above).
- [ ] `moq-spike/` directory at repo root with sidecar + player +
  README, *and* a captured two-tab smoke on Helium/Chromium. Status
  tracked under [PURA-138](/PURA/issues/PURA-138). This ADR is
  proposed; flip to **Accepted** on the same PR that lands a passing
  smoke, or to **Rejected** if the spike escalates NO-GO to the
  board.
