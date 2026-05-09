# ADR-0005 — voice-path decision (lead bet for the broader voice product)

- **Status:** Proposed (rev 2 after board correction; first draft was rejected on the [PURA-108](/PURA/issues/PURA-108) thread because it built on a stale PURA-7 Day-3 conclusion).
- **Date:** 2026-05-09 (rev 1 superseded by rev 2 same day).
- **Author:** CTO.
- **Reviewers:** CEO / board (gate before workstreams 2–6 spawn under PURA-108).

## Context

[PURA-108](/PURA/issues/PURA-108) is the company's Phase 3 voice-path
epic — the **broader TeamSpeak-ecosystem voice product**, distinct
from the admin-panel impl-plan's later "voice bots" slice. The board
greenlit the epic on 2026-05-09 in [PURA-65](/PURA/issues/PURA-65)
following Phase 1+2 close-out at `main` HEAD `d03b04a`, with the
explicit constraint that the strategic fork has to land before any
code so the protocol crate, codec choice, server topology, and the
next hire spec are all selected coherently.

The epic body's framing of the fork:

| Path | Wins | Loses |
|---|---|---|
| **TS6 native** ([PURA-7](/PURA/issues/PURA-7) spike) | Drop-in compat with stock TS clients; immediate user base | Brittle reverse-engineered handshake; voice-protocol IC stays expensive forever |
| **WebRTC bridge** (Opus / SRTP / WebTransport) | Off-the-shelf codecs + signaling; browser-native; standard SFU stack | No interop with stock TS clients without a translator |

Three pieces of evidence from inside the repo land on this fork and
move both downside columns substantially:

1. **PURA-7 retired the protocol risk for TS6 native without a fork.**
   Day 1 of the spike connected `ReSpeak/tsclientlib` master `@04aa249`
   to the public TS6 beta voice server end-to-end — handshake, EAX
   crypto, licence chain, `clientinit` all upstream. The "TS6 native
   = forked tsproto carry" framing in the epic body is therefore
   *upstream pin*, not *fork*. Day 2 settled the voice-frame body
   layout (`voice_id u16 BE | codec_id u8 | opus_payload`) against a
   live wire send. The "brittle reverse-engineered handshake" downside
   is mitigated as long as upstream `tsclientlib` stays alive.

2. **TS6 server self-host is solved.**
   `docker.io/teamspeaksystems/teamspeak6-server:latest` (currently
   `6.0.0-beta9`, published by TeamSpeak Systems, ~484k pulls,
   updated 2026-04-22) is the public, self-hostable TS6 server
   distribution. `docs/ts6-fixture.md` already documents it and the
   repo's `podman-compose.yml --profile ts6-fixture` brings it up
   rootless under the `--network=host` workaround from
   [PURA-105](/PURA/issues/PURA-105). A community operator can stand
   up a TS6 server alongside the admin-panel today via the same
   compose pattern they already use.

3. **Voice frames already flow against beta.voice in the spike.** Day
   2 of PURA-7 sent 50× 20ms Opus frames + voice-stop and the server
   stayed connected. The "two clients can talk" probation deliverable
   is a small extension of that existing wire path against a
   self-hosted fixture instead of beta.voice.

The first draft of this ADR (now superseded) cited a contradicting
PURA-7 Day-3 conclusion that "no public TS6 server distribution
exists" as the central disqualifier against TS6 native. That Day-3
finding was a search miss — the agent checked
`docker.io/library/teamspeak` (TS3) and the official TeamSpeak
download page, but missed the `teamspeaksystems` Docker Hub
namespace. The board flagged the error on the thread and
[PURA-108](/PURA/issues/PURA-108)'s rev-1 confirmation was rejected
accordingly. This rewrite carries the correction.

## Decision

**Lead the broader voice product on the TS6 native path.** Specifically:

1. **Voice product = self-hosted TS6 server + TS6-protocol clients.**
   Server: `docker.io/teamspeaksystems/teamspeak6-server` deployed
   rootless via the existing podman-compose profile. Client
   protocol: `tsclientlib` master pin (`@04aa249`), already carried
   for the admin-panel control surface per
   [PURA-101](/PURA/issues/PURA-101). Voice codec: Opus, framed per
   the §19.10 layout settled in PURA-7 Day 2.
2. **WebRTC bridge is deferred, not killed.** It becomes a Phase
   3.5 follow-up workstream for browser reach — a TS6↔WebRTC
   translator that exposes the TS6 voice room through a self-hosted
   SFU so users without a native TS client can join from a browser.
   This sequencing keeps the *voice product* aligned with the
   "TeamSpeak-ecosystem voice product" framing while opening the
   door to broader reach as a second-line capability.
3. **First deliverable under the epic is a 'two clients can talk'
   prototype** between two `tsclientlib`-based clients connected to
   the self-hosted TS6 fixture, exchanging Opus frames bidirectionally
   for ≥30s under the existing `--network=host` profile. This
   satisfies the CTO probation deliverable in `AGENTS.md` and reuses
   the PURA-7 Day-2 wire path against an in-house server.
4. **Voice-protocol IC dependency on upstream `tsclientlib` is
   accepted as a planned cost,** not absorbed as a fork. We track
   upstream master, contribute schema patches as warm-up tasks
   (carried in [PURA-18](/PURA/issues/PURA-18) per the PURA-7
   close-out), and treat tsclientlib upstream as an
   ecosystem-shared maintenance line rather than our private carry.

This ADR commits the *direction*. Codec / SFU / federation /
hire-spec details follow as workstreams 2–6 with their own ADR
amendments where the choice has compounding consequences.

## Acceptance criteria, scored

The issue body lists five evaluation axes. Reading both paths
against each, with the corrected facts:

| Axis | TS6 native | WebRTC bridge | Verdict |
|---|---|---|---|
| **Handshake stability** | `tsclientlib` master pin works today against beta.voice and against the self-hosted `teamspeak6-server:6.0.0-beta9` image. TeamSpeak controls the wire and can rev it; we ride upstream's RE work. | RFC-defined ICE / DTLS-SRTP / SDP. Stable surface, every browser implements it. | **WebRTC** has the cleaner long-term stability story. **TS6** is *good enough today* and improves as TeamSpeak progresses past beta. |
| **Codec / lipsync** | Opus framing settled (`voice_id u16 BE \| codec_id u8 \| opus_payload`, PURA-7 Day 2). Lipsync inside the TS client. | Opus 20ms frames (RFC 7587) + RTCP-RR sync inside the browser. | **Tie.** Both ship Opus; both have a working lipsync story. |
| **p99 mouth-to-ear latency** | TS6 client → self-hosted TS6 server → TS6 client is a single UDP hop. Opus 20ms framing puts the floor in the 50–80ms range over a LAN-quality link to the server. Esports (~80ms) target reachable; community (~150ms) easy. | SFU one-hop typical 30–80ms; cascaded SFUs add ~30–60ms per region hop. Esports reachable with regional placement; community easy. | **Tie at the floor; TS6 wins on simpler topology** for a single-server deployment because there's no SFU between the two clients. |
| **Self-host complexity** | `docker.io/teamspeaksystems/teamspeak6-server` rootless via `podman-compose --profile ts6-fixture`. Documented in `docs/ts6-fixture.md`. License click-through via `TSSERVER_LICENSE_ACCEPTED=accept`. <10min from a fresh box. | mediasoup / livekit / ion-sfu rootless via compose, plus co-hosted TURN, plus signaling layer. <10min reachable but with more moving parts. | **TS6** wins on operator simplicity (one container, no TURN, no SFU choice question). The board's self-host-first doctrine is satisfied by both, but TS6 is the lower-friction path. |
| **Ecosystem reach** | Drop-in compat with stock TS6 clients on every desktop (Win/macOS/Linux); Android/iOS clients exist. Server config language is the one TS admins already know. | Every browser; mobile via Chrome/Safari; native via libwebrtc. **No** stock-TS-client interop without a translator. | **TS6** wins on the *immediate* "TeamSpeak-ecosystem" user base named in the epic framing. **WebRTC** wins on the *adjacent* "anyone with a browser" user base. The translator-as-Phase-3.5 plan captures the WebRTC reach without forfeiting the TS6 base. |

Net: TS6 native wins or ties on four of five axes once the
self-host correction is applied, and is the closer fit to the
epic's "broader TeamSpeak-ecosystem voice product" framing. The
remaining stability gap (TeamSpeak controls the wire) is a planned
maintenance cost, not a kill criterion.

## Alternatives considered

| Option | Verdict | Reason |
|---|---|---|
| **TS6 native as lead** | ✅ **Chosen** | Aligned with the "TeamSpeak-ecosystem voice product" epic framing. Self-host now solved (`teamspeaksystems/teamspeak6-server`). Protocol risk retired by PURA-7 (upstream `tsclientlib` works). Reuses existing admin-panel TS6 wire competence ([PURA-101](/PURA/issues/PURA-101) tsclientlib pin, [PURA-105](/PURA/issues/PURA-105) fixture, [PURA-106](/PURA/issues/PURA-106) headless voice-fixture). Drop-in compat with stock clients = immediate user base. Hire spec narrows to a tractable Rust + tsclientlib + Opus role. |
| **WebRTC bridge as lead** | ❌ Rejected as the *lead* (kept as Phase 3.5 follow-up) | Diverges from the epic framing — a WebRTC-led product is a Discord alternative, not a TeamSpeak-ecosystem voice product. Forfeits the existing TS user base on the *voice* side too (admin-panel still serves them on the control side). More moving parts (SFU + TURN + signaling) for the operator. Longer time-to-demo for "two clients can talk" because we'd need to pick the SFU, stand up signaling, and ship a browser client before any frames flow — vs. the TS6 path where frames already flow in the PURA-7 spike. |
| **Both in parallel from day one** | ❌ Rejected | Doubles the protocol surface area on a one-engineer team. PURA-7 already proved the TS6 wire is reachable; we don't need to build the second path before the first one lands. Re-evaluate once the TS6 voice-product MVP demos. |
| **Pick later, build infra now** | ❌ Rejected | Codec / topology / hire spec all derive from this call. Building "voice infra" without a chosen lead is how scope creeps and shipping slips — the board explicitly asked for the call before code. |
| **Forked `tsproto` instead of upstream `tsclientlib`** | ❌ Rejected | PURA-7 Day 1 demonstrated upstream master works without a fork. Carrying our own fork is strictly more cost than tracking master and contributing patches upstream. The hard-constraint reminder "no upstream PR/FR/bug without explicit board ack on this thread" still binds us, but warm-up patches like [PURA-18](/PURA/issues/PURA-18) (tsdeclarations schema) follow the round-trip and land cleanly. |
| **Custom protocol on top of QUIC / WebTransport** | ❌ Out of scope | Violates open-standards-first. Reserve as a research line if a real operator problem ever justifies it. |

## Consequences

**Positive.**

- Self-host doctrine satisfied. A community operator runs the voice
  server alongside the admin-panel via the same `podman-compose`
  pattern they already deploy, in a single rootless container, in
  under 10 minutes. The license-acceptance env var is documented;
  no SaaS dependency.
- Open standards on the wire that matters for the audio path
  (Opus + UDP). The licensing / handshake layer is TeamSpeak-
  controlled; that's a known TeamSpeak-ecosystem cost, not a
  surprise.
- The CTO probation deliverable ("two clients can talk") collapses
  to a small extension of the PURA-7 Day-2 voice-tx spike running
  against the self-hosted fixture. Reachable inside the two-week
  window without inventing protocol.
- Voice-IC hire spec ([PURA-108](/PURA/issues/PURA-108) ws-6)
  resolves to **Rust async + `tsclientlib` familiarity + Opus**,
  not "WebRTC / SFU generalist". Smaller, more focused candidate
  pool overlapping with the TeamSpeak community.
- The admin-panel and the voice product share infra (the same
  podman-compose, the same TS6 fixture, the same `tsclientlib`
  pin, the same audit / control layer). Bus number stays low; the
  next hire ramps up on a stack the company already runs.
- Voice fixture extension ([PURA-108](/PURA/issues/PURA-108) ws-5)
  inherits from [PURA-106](/PURA/issues/PURA-106) directly — the
  headless TS6 voice-client fixture moves from connect-only to
  full audio-frame round-trip without a new fixture stack.

**Negative.**

- **TeamSpeak controls the wire and the server image.** Upstream
  ships beta releases at their cadence and can rev the wire any
  time. Mitigation: pin `tsclientlib` per
  [PURA-101](/PURA/issues/PURA-101); pin the
  `teamspeak6-server:6.0.0-betaN` tag (not `latest`) in our
  default compose profile so an upstream cut doesn't break
  community deployments overnight. Track upstream's wire-change
  cadence as part of the operator-runbook line.
- **Voice IC depends on `tsclientlib` upstream staying alive.** It
  is a small project. Mitigation: the warm-up tsdeclarations
  patches in [PURA-18](/PURA/issues/PURA-18) seed an
  upstream-maintainer relationship; we carry capacity to fork if
  upstream ever stops moving (with board ack per the round-trip
  rule).
- **License acceptance pulls operators into TeamSpeak's EULA.**
  Acceptable per "self-host first" (no SaaS dependency), but it
  is a contractual surface that didn't exist with a hypothetical
  WebRTC-first product. Document it clearly in the ops runbook
  alongside the `TSSERVER_LICENSE_ACCEPTED=accept` line.
- **No browser reach until WebRTC bridge ships in Phase 3.5.**
  Browser users have to install a TS6 client. Mitigation: WebRTC
  translator stays on the roadmap as a high-leverage follow-up,
  not a parallel build.
- **Beta server status.** `teamspeak6-server` is still
  `6.0.0-betaN`. Operators running it are running TeamSpeak's
  beta. Document this; revisit the recommendation once a
  general-availability tag ships (likely Phase 4).

**Workstream impact.**

- WS-1 (this ADR): rev 2 after board correction; lands once the
  board acks PURA-108's plan revision.
- WS-2 (latency budget memo): reframes around the **TS6 client →
  TS6 server → TS6 client** topology (single-UDP-hop). Esports
  (~80ms) and community (~150ms) targets restated. Includes the
  Phase-3.5 WebRTC bridge translator's latency budget as a
  separate chart so it's clear when we cross to a two-hop path.
- WS-3 (federation / relay topology): becomes **TS6 multi-server
  topology + the eventual WebRTC translator deployment shape**.
  Federation in the TS6 ecosystem is per-server-membership rather
  than SFU-cascade; the memo articulates how a community of
  operators surfaces a multi-server experience. Own ADR amendment.
- WS-4 ("two clients can talk" prototype): two `tsclientlib`-based
  clients (Rust binaries or extensions of the PURA-7 voice-tx
  spike) connected to the local `teamspeak6-server` fixture,
  exchanging Opus frames bidirectionally. Probation deliverable.
- WS-5 (voice fixture extension on top of [PURA-106](/PURA/issues/PURA-106)):
  the headless fixture extends from connect-only to assert audio
  frames flow end-to-end through the self-hosted TS6 server.
  Single fixture stack, no parallel WebRTC fixture in this phase.
- WS-6 (VoiceEngineer hire spec): drafted post-ADR-ack with the
  role shaped as **Rust async + `tsclientlib` + Opus + audio
  pipeline**, NOT as a WebRTC / SFU generalist.
- **New: WS-7 (Phase 3.5 — WebRTC bridge translator).** Spawns
  *after* WS-4 demos. Scope: bridge a self-hosted TS6 voice room
  to a browser client via a self-hosted SFU, so non-TS-client
  users join the same room. Own ADR.

## Hard-constraint reminders carried forward

- **No upstream PR / FR / bug filings without explicit board ack.**
  Applies to `tsclientlib` upstream (already carried for the
  admin-panel control surface), to `teamspeak6-server` /
  TeamSpeak issue trackers, to whatever SFU we eventually adopt
  for WS-7, and to any third-party project. Round-trip:
  document internally → draft external post text on the
  PURA-108 thread → wait for ack → file.
- **Self-host first.** The default `podman-compose` profile must
  bring up the voice product (TS6 server + admin-panel +
  fixtures) on a fresh box in <10 minutes, rootless, with no
  hosted-only dependencies. Hosted convenience layers may be
  *added*; they may not be *required*.
- **Open standards before bespoke.** Opus on the audio path; JWT
  / OAuth where they show up in the admin-panel control surface.
  We are *not* re-implementing the TS6 licence handshake — we
  consume it via `tsclientlib`. If we find ourselves drafting a
  custom wire packet shape, stop.
- **Cleanroom rule.** No read of `Agent-Fennec/ts6-manager`. Spec
  questions get answered via PURA-7-style spikes against the
  self-hosted fixture or the public beta.voice endpoint.

## Cleanroom posture

This ADR is drafted from public sources only — the
`teamspeaksystems/teamspeak6-server` Docker Hub page, RFC 7587
(Opus), the upstream `ReSpeak/tsclientlib` source tree, and the
PURA-7 / PURA-101 / PURA-105 / PURA-106 internal artefacts cited
above. No dependency on closed TeamSpeak documentation; no use of
the cleanroom-forbidden `Agent-Fennec/ts6-manager` reference repo.

## Correction history

- **rev 1 (2026-05-09T17:10Z)** — proposed WebRTC bridge as lead
  with the disqualifier "no public TS6 server distribution exists
  → self-host doctrine fails for TS6 native". Rejected by board on
  the [PURA-108](/PURA/issues/PURA-108) thread; correction:
  `docker.io/teamspeaksystems/teamspeak6-server` is the public,
  self-hostable distribution and `docs/ts6-fixture.md` already
  documents it. The Day-3 PURA-7 conclusion the disqualifier was
  built on was a search miss (checked `library/teamspeak`, missed
  `teamspeaksystems`).
- **rev 2 (this revision, 2026-05-09T17:35Z)** — flips the lead
  recommendation to TS6 native; relegates WebRTC bridge to Phase
  3.5 follow-up; updates alternatives, consequences, and
  workstream impact to match.
