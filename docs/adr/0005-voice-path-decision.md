# ADR-0005 — voice-path decision (lead bet for the broader voice product)

- **Status:** Proposed (board ack pending on [PURA-108](/PURA/issues/PURA-108)).
- **Date:** 2026-05-09.
- **Author:** CTO.
- **Reviewers:** CEO / board (gate before workstreams 2–6 spawn under PURA-108).

## Context

[PURA-108](/PURA/issues/PURA-108) is the company's Phase 3 voice-path epic — the
broader TeamSpeak-ecosystem voice product, distinct from the admin-panel
impl-plan's later "voice bots" slice. The board greenlit the epic on
2026-05-09 in [PURA-65](/PURA/issues/PURA-65) following Phase 1+2 close-out
at `main` HEAD `d03b04a`, with the explicit constraint that the strategic
fork has to land before any code so the protocol crate, codec choice,
server topology, and the next hire spec are all selected coherently.

The fork as the board sees it:

| Path | Wins | Loses |
|---|---|---|
| **TS6 native** (forked / pinned `tsproto`-family stack, [PURA-7](/PURA/issues/PURA-7) spike) | Drop-in compat with stock TS clients; immediate user base | Brittle reverse-engineered handshake; voice-protocol IC stays expensive forever |
| **WebRTC bridge** (Opus / SRTP / WebTransport) | Off-the-shelf codecs + signaling; browser-native; standard SFU stack | No interop with stock TS clients without a translator |

Two pieces of new evidence land on this fork that change the framing:

1. **PURA-7 produced a positive handshake result without a fork.** The
   spike connected `ReSpeak/tsclientlib` master `@04aa249` to the public
   TS6 beta voice server end-to-end on Day 1 — no `tsproto` fork needed,
   handshake / EAX crypto / licence chain / `clientinit` already upstream.
   The "TS6 native" column of the board's table is therefore *upstream
   pin*, not *forked tsproto*; the maintenance tail is third-party
   upstream rather than our own carry.
2. **PURA-7 also produced a negative self-host result.** No publicly
   distributed TS6 server exists today (`docker.io/library/teamspeak`
   ships TS3 only; the official TeamSpeak download page lists TS3 only;
   `beta.voice.teamspeak.com` is the single reachable TS6 endpoint).
   The TS6 native column **cannot satisfy the company's self-host-first
   doctrine for the server side**, because the artefact a community
   operator would deploy in <10min via podman-compose does not exist
   and is not under our control to ship.

Self-host first is the wedge. Cite from `AGENTS.md`:

> Self-host as a first-class target. A community operator must be able
> to stand up a server in under 10 minutes. This is part of the wedge
> against Discord.

A voice product that ships only as "connect to TeamSpeak's hosted beta
or buy a TeamSpeak server licence" loses the wedge. That is the
disqualifier against TS6 native as the lead path — not the protocol
risk, which PURA-7 retired.

## Decision

**Lead the broader voice product on the WebRTC bridge path.** Specifically:

1. **Voice product = self-hosted WebRTC SFU + browser/native clients.**
   Codec: Opus (RFC 7587 — Opus is mandatory-to-implement in WebRTC).
   Transport: SRTP over DTLS, with WebTransport as a forward-looking
   second binding. Signaling: standard WebRTC offer/answer over a
   minimal JSON+WebSocket layer (no SIP / XMPP / Jingle — overkill for
   the MVP).
2. **TS6 interop is deferred, not killed.** The PURA-7 / [PURA-101](/PURA/issues/PURA-101)
   /[PURA-106](/PURA/issues/PURA-106) work on the TS6 wire stays scoped to
   the **admin-panel control surface and the headless voice fixture**.
   When (and only when) TeamSpeak ships a self-hostable TS6 server, or
   a community-licence path opens, we re-evaluate adding a TS6↔WebRTC
   translator as a second-line interop product. The *broader voice
   product* does not depend on that translator existing.
3. **First deliverable under the epic is a 'two clients can talk'
   prototype** between two browsers via a self-hosted SFU, satisfying
   the CTO probation deliverable in `AGENTS.md`. Scope: bidirectional
   Opus over SRTP, two participants on one server, deployed via the
   existing podman-compose profile pattern from [PURA-105](/PURA/issues/PURA-105).
4. **SFU choice deferred to workstream 3** (federation / relay
   topology). The viable open-source candidates (mediasoup, livekit,
   ion-sfu) all run rootless in podman and meet the 10-minute self-host
   bar; the tradeoff between operator UX, federation story, and
   protocol footprint is a separate decision under the epic, not this
   ADR.

This ADR commits the *direction*. Codec / SFU / federation / hire-spec
decisions follow as workstreams 2–6 with their own ADR amendments
where the choice has compounding consequences.

## Acceptance criteria, scored

The issue body lists five evaluation axes. Reading both paths against
each:

| Axis | TS6 native (upstream pin) | WebRTC bridge | Verdict |
|---|---|---|---|
| **Handshake stability** | Upstream `tsclientlib` works today; pinned to `master @ 04aa249` per [PURA-101](/PURA/issues/PURA-101). TeamSpeak can rev the wire at any time; we depend on a third party to keep up. | RFC-defined ICE / DTLS-SRTP / SDP. Every browser implements it. Stable surface. | **WebRTC** wins on long-term stability; TS6 acceptable short-term for the admin-panel control surface. |
| **Codec / lipsync** | Opus framing settled in PURA-7 Day 2 (`voice_id u16 BE \| codec_id u8 \| opus_payload`). Lipsync handled inside the TS client. | Opus 20ms frames (RFC 7587). Lipsync via RTCP-RR + standard A/V sync. Browser handles it. | **Tie on capability.** WebRTC has the broader tooling around it. |
| **p99 mouth-to-ear latency** | Unknown without live measurement against beta.voice; Opus + 20ms framing puts the floor in the 60–100ms range. Esports (~80ms) target is borderline. | SFU one-hop typical 30–80ms over good links; can hit esports target with regional SFUs. Community target (~150ms) trivial. | **WebRTC** wins on headroom and on the existence of an op-ready measurement story. |
| **Self-host complexity** | **Server cannot be self-hosted at all** — no public TS6 server distribution. | mediasoup / livekit / ion-sfu all run rootless in podman; <10min via compose is reachable, in line with the [PURA-105](/PURA/issues/PURA-105) ts6-fixture pattern. | **WebRTC** wins decisively. This is the disqualifier for TS6 native as the lead. |
| **Ecosystem reach** | Drop-in compat with stock TS6 clients *if* the user has a TS6 server (paid licence or beta). Native client distribution is TeamSpeak-controlled. | Every browser. Native clients via libwebrtc. Mobile via Chrome / Safari. Existing translators to SIP / Matrix. | **WebRTC** wins on breadth; TS6 wins on the narrow "existing TS user with a licensed server" wedge — a wedge our admin-panel already addresses on the control surface. |

WebRTC wins on four of five axes and ties on the fifth. The remaining
ecosystem-reach point for TS6 ("immediate stock-client user base") is
already covered by the admin-panel itself, which sits on top of TS6
servers operators already run. The voice product's job is to win the
*next* user — the community organiser standing up a Discord
alternative on their own VPS.

## Alternatives considered

| Option | Verdict | Reason |
|---|---|---|
| **WebRTC bridge as lead** | ✅ **Chosen** | Aligns with self-host-first and open-standards-first. RFC-defined surface keeps the protocol IC's job sane. Browser-native = mobile + desktop on day one without shipping our own client. Latency story strictly better than TS6 native. Federation story available off-the-shelf (cascaded SFU). |
| **TS6 native as lead** | ❌ Rejected | Self-host is *infeasible on the server side today* — community operator cannot deploy a TS6 server in <10 minutes because no distribution exists. The wedge depends on self-host. Even if a distribution lands, TeamSpeak owns the wire, and the voice IC stays a perpetual dependency-tracking role. |
| **Both in parallel from day one** | ❌ Rejected | Doubles the protocol surface area on a one-engineer team. The PURA-7 spike already proved the TS6 wire is reachable; we do not need to maintain a parallel implementation while the strategic question is open. Re-evaluate after the WebRTC MVP demos. |
| **Pick later, build infra now** | ❌ Rejected | Codec, jitter buffer, server topology, and hire spec all derive from this call. Building "voice infra" without a chosen lead is how scope creeps and shipping slips. The board explicitly asked for the call before code. |
| **Custom protocol on top of QUIC / WebTransport** | ❌ Out of scope | Violates open-standards-first. Reserve as a research line if WebRTC's signaling friction or trickle-ICE cost shows up as a real operator problem in production. |

## Consequences

**Positive.**

- Self-host doctrine satisfied. A community operator can run the voice
  server alongside the admin-panel via the existing podman-compose
  profile pattern, with a single rootless container.
- Open standards throughout: Opus, SRTP, DTLS, WebRTC, JWT for
  signaling auth. Every layer is an RFC the IC can read instead of a
  reverse-engineered packet capture.
- The CTO probation deliverable ("two clients can talk") collapses to
  a well-trodden two-browser-via-SFU walkthrough; a working prototype
  is reachable inside the two-week window without inventing protocol.
- Mobile and browser clients land for free — we ship the room, the OS
  ships the audio stack.
- Hire spec for the next IC ([PURA-108](/PURA/issues/PURA-108) ws-6)
  resolves to a WebRTC / SFU generalist rather than a TS6 protocol
  reverse-engineering specialist. Wider candidate pool; shorter ramp;
  less single-point-of-failure on the company's bus number.
- The maintenance tail is shared with the rest of the open-source
  voice ecosystem (Jitsi, LiveKit, Janus, Galène). Bugs we hit are
  bugs many other shops have hit before us.

**Negative.**

- We forfeit the "drop-in compat with stock TS clients" axis as a
  *voice* selling point. Mitigation: the admin-panel already owns the
  "manage your existing TS server" use case, and the TS6↔WebRTC
  translator can ship as a Phase 4+ interop product when there is
  real user pull.
- Operators must learn one new concept (the SFU) on top of the
  admin-panel they already deploy. Mitigation: bundle the SFU into
  the same compose profile by default; the operator-facing message
  is "voice just works alongside your admin-panel".
- WebRTC's signaling layer is small but bespoke per shop. We commit
  to documenting our signaling protocol clearly so other open-source
  clients can interoperate with our SFU. JWT-bearer auth keeps it
  thin.
- TURN / NAT traversal becomes our problem in a way TS6's UDP-with-
  manual-port-forwarding never was. Mitigation: ship a co-hosted
  TURN by default in the compose profile; document the
  "open ports 3478/UDP + 5349/TCP" runbook line.
- We give up first-class observability into the TS6 mouth-to-ear
  internals. Acceptable — the WebRTC stack's `getStats()` surface is
  the most observable voice stack in the open-source world.

**Workstream impact.**

- WS-1 (this ADR): proposed with this document; lands once the board
  acks PURA-108's plan.
- WS-2 (latency budget memo): becomes a WebRTC SFU-tuning memo with
  Opus frame size, jitter buffer, and SFU regional placement as the
  knobs. Esports (~80ms) and community (~150ms) targets restated
  against the actual physics of an SFU one-hop.
- WS-3 (federation / relay topology): chooses the SFU implementation
  (mediasoup vs livekit vs ion-sfu vs hand-rolled-on-pion) and the
  federation story (cascade vs full mesh vs none-for-MVP). This is a
  separate ADR.
- WS-4 ("two clients can talk" prototype): becomes a two-browsers-
  via-self-hosted-SFU walkthrough. Probation deliverable.
- WS-5 (voice fixture extension on top of [PURA-106](/PURA/issues/PURA-106)):
  the headless fixture stays on TS6 *for the admin-panel*; a
  parallel WebRTC voice-fixture spawns under WS-5 to assert audio
  frames flow end-to-end through our SFU. The two fixtures coexist —
  TS6 for the control surface, WebRTC for the voice product.
- WS-6 (VoiceEngineer hire spec): drafted post-ADR-ack with the role
  shaped as **WebRTC / SFU generalist + Rust async**, NOT as a TS6
  protocol RE specialist.

## Hard-constraint reminders carried forward

- **No upstream PR / FR / bug filings without explicit board ack.**
  Applies to whatever SFU we end up depending on (mediasoup,
  livekit, ion-sfu), to `tsclientlib` upstream as it's already
  carried for the admin-panel control surface, and to any third
  party. Round-trip: document internally → draft external post text
  → wait for board ack → file.
- **Self-host first.** Any deployment-shape decision under WS-3 must
  be reachable inside one rootless podman-compose profile, with no
  hosted-only dependencies (no Cloudflare-only TURN, no managed-
  signal-server-only stories). Hosted convenience layers may be
  *added*; they may not be *required*.
- **Open standards before bespoke.** Opus, SRTP, DTLS, WebRTC, JWT,
  OAuth. If we find ourselves drafting a custom wire packet shape,
  stop and check whether an RFC already covers it.

## Cleanroom posture

This ADR is drafted from public sources only — RFC 7587 (Opus), RFC
8825 (WebRTC overview), the WebRTC implementer landscape, and the
PURA-7 / PURA-101 / PURA-106 / PURA-105 internal artefacts cited
above. No dependency on closed TeamSpeak documentation; no use of
the cleanroom-forbidden `Agent-Fennec/ts6-manager` reference repo.
