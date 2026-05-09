# ADR-0006 — TS6 multi-server topology + Phase 3.5 WebRTC translator deployment shape

- **Status:** Proposed (WS-3 deliverable under [PURA-108](/PURA/issues/PURA-108)).
- **Date:** 2026-05-09.
- **Author:** CTO.
- **Reviewers:** CEO / board (gate before WS-7 implementation spawns).
- **Source issue:** [PURA-111](/PURA/issues/PURA-111).

## Context

[ADR-0005 rev 2](./0005-voice-path-decision.md) commits the lead voice
path to **TS6 native** with `tsclientlib` master and the
`teamspeaksystems/teamspeak6-server` self-host image. Two follow-on
deployment-shape questions were carved out as WS-3 because their
answers compound on the operator surface, the hire spec, and what we
later sign up to upstream:

1. **TS6 multi-server topology.** TS6 is per-server-membership rather
   than SFU-cascade or ActivityPub-style federation. The voice product
   needs a memo on what we surface to a *community of operators*
   running independent TS6 servers, and what we explicitly defer.
2. **Phase 3.5 WebRTC translator deployment shape (WS-7 anchor).**
   ADR-0005 defers WebRTC reach to a translator that bridges a
   self-hosted TS6 voice room to browser clients via a self-hosted
   SFU. WS-7 implements the translator; this ADR fixes the SFU choice,
   the TURN/signaling layer, and how the translator slots into the
   existing `podman-compose --profile ts6-fixture` profile so an
   operator brings voice up rootless in <10 minutes whether or not
   the translator is enabled.

Both decisions need to land before WS-7 implementation can spawn —
the SFU choice changes the hire spec, the TURN exposure changes the
operator runbook, and the federation posture decides whether we need
a registry component at all.

## Decision A — Multi-server topology

**Adopt the per-server-membership model as the federation model.** No
central registry, no cross-server presence sync, no multi-region SFU
cascade.

A community of operators running independent TS6 servers surfaces a
multi-server experience to end-users via three layers:

1. **Per-operator server list** (ship in Phase 3). The admin-panel
   already enumerates the operator's `server_connection` rows. We
   extend that view with a public read-only `servers.json` manifest
   the operator can opt into publishing — listing each server's
   `host:9987`, display name, region tag, and a community description.
   Manifest format is documented in the operator runbook; consumption
   is by URL (operator pastes another operator's manifest URL into
   their own panel to render a "neighboring servers" panel). No
   central directory; the manifest URL is the only coupling.
2. **TS6-native identity is already portable** across servers. Each
   client owns its `clientidentity` keypair and travels with it; we
   do not invent a cross-server identity layer because TS6 already
   provides one. Server-side per-client trust state (UID, groups)
   stays scoped to each server, as it does today.
3. **DNS-based server discovery (deferred to Phase 4 if at all).** A
   `_ts6._udp.example.com` SRV record is the standards-compliant way
   to publish a TS6 server endpoint. Honoring this on the client
   side belongs in `tsclientlib` upstream, not in our codebase.
   We document the SRV recipe in the operator runbook and otherwise
   defer.

### What we explicitly defer

| Capability | Status | Rationale |
|---|---|---|
| Central federated directory (operator registry) | Deferred indefinitely | Reintroduces a hosted dependency. Conflicts with self-host doctrine. Operators trade manifest URLs out-of-band. |
| Cross-server presence (who's online on a remote server) | Deferred to Phase 5+ | Requires a presence protocol we'd own and version. Not in the MVP cut. |
| Cross-server channel routing (one channel spanning two servers) | Deferred indefinitely | Equivalent to building a TS6-over-TS6 mesh. Out of scope. |
| ActivityPub / Matrix-style federation | Rejected | Not a TS6 ecosystem fit. Different identity, different routing. |
| Multi-region SFU cascade | Not applicable | TS6 native is one UDP hop. SFU cascade is a WebRTC-translator concern; see Decision B. |
| Single-sign-on across operators | Deferred indefinitely | TS6's keypair identity already covers the user-facing case. Cross-operator SSO is a Phase 5+ admin-panel feature, not a voice-product feature. |

### Alternatives considered for Decision A

| Option | Verdict | Reason |
|---|---|---|
| **Per-operator manifest (chosen)** | ✅ | Self-host first; no central registry; operator opts in by publishing one URL; consumption is voluntary and auditable. Lowest-friction path that still answers "how does my community run a multi-server experience." |
| Hosted central registry (we run it) | ❌ Rejected | Adds a SaaS dependency to the default profile. Operator who refuses to register is invisible. Conflicts with self-host doctrine. |
| ActivityPub-style federation | ❌ Rejected | Identity-model mismatch with TS6. We'd be inventing a parallel federation layer rather than using the one TS6 already provides client-side. |
| Server-discovery via DNS SRV only | ⚠️ Documented as Phase 4+ | Cleanest standards path for endpoint discovery, but client honoring is a `tsclientlib` upstream item, not a Phase 3 deliverable. We document the recipe; we do not ship the resolver. |
| Build a presence/chat federation now | ❌ Out of scope | Doubles MVP surface. The lead voice product is one server; federation features compound on a working single-server product. |

## Decision B — Phase 3.5 WebRTC translator deployment shape

**Adopt LiveKit (Apache-2.0) as the SFU; coturn (Apache-2.0) as the
TURN relay; LiveKit's built-in WebSocket signaling as the signaling
layer; ship them behind a single rootless `voice-translator`
podman-compose profile.**

The translator process — implemented in WS-7, not here — is a
small Rust daemon that:

- Joins the local TS6 voice room as a synthetic `tsclientlib` client
  (no protocol invention; reuses ADR-0005's wire path).
- Forwards inbound TS6 Opus frames into a LiveKit room as a publisher.
- Forwards inbound LiveKit Opus frames back into the TS6 voice room
  as a synthetic-client send.

LiveKit was chosen against three alternatives. Decision criteria:
self-host posture, Rust integration, license, operator runtime
footprint, and the size of the surface we sign up to maintain.

### Alternatives considered for Decision B (SFU)

| SFU | License | Verdict | Reason |
|---|---|---|---|
| **LiveKit Server** (Go) | Apache-2.0 | ✅ **Chosen** | Single static Go binary, official rootless container `livekit/livekit-server`, Apache-2.0 with no dual-license carve-out on the server. Rooms / participants / tracks model maps cleanly onto TS6 channels and clients. First-class Rust SDK (`livekit-api`, `livekit`, Apache-2.0) lets the translator publish + subscribe in our existing tokio runtime without an FFI bridge. Built-in WebSocket signaling and JWT-based auth — no separate signaling project to choose, deploy, or pin. Battle-tested at scale by multiple production deployments. Operator-runbook surface is small: one container, one config file, one port. |
| **mediasoup** (C++ workers + Node orchestrator) | ISC | ❌ Rejected | mediasoup ships as a Node.js library, not a server. Adopting it forces a Node orchestrator into the operator's compose profile and into our maintenance surface. We are a Rust-first shop; adding Node to the voice product violates the "bus number stays low" consequence in ADR-0005. The C++ worker performance edge is real but does not justify the second runtime. |
| **ion-sfu** (Pion / Go) | MIT | ⚠️ Documented backup | Lighter than LiveKit; pure SFU with no opinionated room/auth model. Smaller community and noticeably less production traffic. Worth keeping as the documented fallback if LiveKit's roadmap diverges from self-host posture, but not the default. The lighter footprint is appealing for very small operators; we will revisit during WS-7 implementation if LiveKit's resource floor turns out to be a problem on a 1-vCPU host. |
| **Custom SFU on `webrtc-rs` / Pion-on-Rust** | n/a | ❌ Rejected | No production-ready Rust SFU framework exists; `webrtc-rs` is a transport library, not a server. Building one for a one-engineer team multiplies the protocol surface. The `tsclientlib` ecosystem cost we accepted in ADR-0005 already covers our voice-protocol carry — adding a second carry on the WebRTC side blows the budget. |
| **Run no SFU; mesh WebRTC peers directly** | n/a | ❌ Rejected | Mesh topology breaks past ~6 participants. The translator's whole point is to bridge a single TS6 room (which can hold dozens of speakers) to a browser audience. Mesh is a non-starter for that fan-out. |

### Alternatives considered for Decision B (TURN)

| TURN | License | Verdict | Reason |
|---|---|---|---|
| **coturn** | BSD-3 | ✅ **Chosen** | Reference TURN implementation; battle-tested; runs rootless in a podman container; widely understood by ops folks; documented `podman-compose` recipe. |
| **pion/turn** | MIT | ⚠️ Considered | Go library, not a server. We would have to wrap it. LiveKit's own embedded TURN can serve simple cases without a separate relay; coturn remains the recommendation for any production deployment that needs symmetric-NAT traversal. |
| **LiveKit embedded TURN only** | Apache-2.0 | ⚠️ Acceptable for the simplest deployments | LiveKit can act as its own STUN/TURN endpoint when colocated with the SFU. Acceptable for operators on public IPs without restrictive firewalls. The runbook recommends coturn as the production-grade default; LiveKit-embedded as the simplest-bring-up path. |

### Alternatives considered for Decision B (signaling)

| Signaling | Verdict | Reason |
|---|---|---|
| **LiveKit's built-in WebSocket signaling** (chosen) | ✅ | Ships with the SFU. JWT-authenticated. No separate process. Browser SDK speaks it natively. |
| Matrix / XMPP / SIP / standalone Janus signaling | ❌ Rejected | Adds a separate protocol surface, separate auth model, separate operator-runbook line. No advantage over LiveKit's bundled signaling for our use case. |

### Compose profile shape

A single rootless `podman-compose.yml` exposes the voice product
under two profiles, both extensions of the existing `ts6-fixture`
profile:

```yaml
# (Conceptual shape; concrete YAML lands in WS-7.)
profiles:
  voice:                  # TS6 native only — the default voice profile
    services: [fullstack, ts6-server]
  voice-translator:       # TS6 native + WebRTC bridge (Phase 3.5)
    services: [fullstack, ts6-server, livekit, coturn, translator]
```

Operator commands:

```bash
# Default — TS6 native voice up in <10 minutes:
podman-compose --profile voice up -d

# WebRTC bridge enabled — adds SFU + TURN + translator:
podman-compose --profile voice-translator up -d
```

Both profiles share the same `ts6-server` service (driven by the
same TS6 license-acceptance env var documented in
`docs/ts6-fixture.md`). The `voice-translator` profile adds three
services on top: a `livekit` SFU container, a `coturn` TURN container,
and a `translator` container running the WS-7 daemon. Disabling the
translator is the absence of the profile flag — no runtime toggle,
no shared state to clean up.

The `ts6-fixture` profile in the current `podman-compose.yml`
remains as a QA-only profile (license = `accept`, admin password =
`qa-admin`, brute-force check skipped). The `voice` profile uses
production defaults: license-acceptance is still required, admin
password is operator-supplied, brute-force protection stays on.

## Consequences

**Positive.**

- **Self-host bar still met.** Default `--profile voice` is one
  service beyond the existing fullstack manager — TS6 server. <10
  minutes from a fresh box, no SaaS, no central registry, no SFU.
- **WebRTC reach is opt-in.** Operators who don't need browser users
  pay no LiveKit/coturn footprint. Operators who do flip one profile
  flag.
- **Translator hire spec narrows.** WS-7's IC role becomes "Rust async
  + LiveKit Rust SDK + Opus + tsclientlib" — overlaps with the WS-6
  Rust-async/tsclientlib hire spec already drafted in ADR-0005.
- **Federation posture is honest.** We are not pretending TS6 is a
  Mastodon-style federation. Operators publish a manifest URL if they
  want to be discoverable; that is the entire mechanism.
- **No new wire protocols.** Audio path on the WebRTC side is Opus
  over SRTP/DTLS (RFC-defined). Identity portability on the TS6 side
  reuses the existing `clientidentity` keypair model.

**Negative.**

- **LiveKit footprint.** The SFU container plus coturn plus the
  translator daemon adds three processes to operators who enable the
  bridge. RAM floor for the bridge profile is ~1 GB on a 1-vCPU host.
  Documented in the operator runbook; ion-sfu is the documented
  fallback if this turns out to be a community pain point.
- **Cross-operator presence is missing.** A user on operator-A's
  server cannot see who's online on operator-B's server. This is the
  intended federation posture, but worth being honest about — the
  multi-server experience is "browse the manifest, click to connect,"
  not "see all your friends across servers in one panel." Phase 5+
  may revisit if community demand clarifies the shape.
- **JWT signing key is a new operator secret.** LiveKit's auth model
  needs an API-key/secret pair. Operators must generate and rotate it.
  Documented in the operator runbook. The translator daemon mints
  short-lived per-room tokens; long-lived tokens are not stored.
- **No built-in browser client in Phase 3.5.** WS-7 ships the
  translator; a polished browser UI is a separate workstream. The
  LiveKit Web SDK demo page is acceptable as the bring-up client.
- **Beta status of `teamspeak6-server` carries forward.** Same caveat
  as ADR-0005 — pin a `6.0.0-betaN` tag rather than `latest`.

## Operator runbook outline

(Full runbook lands as a docs PR alongside WS-7. This is the outline;
each line below becomes a section.)

1. **License acceptance.** `TSSERVER_LICENSE_ACCEPTED=accept` is required
   for the `ts6-server` service. The runbook calls out the implicit
   TeamSpeak EULA agreement and links to the upstream license text.
2. **Port surface.**
   - **TS6 native (`--profile voice`):**
     `9987/udp` (voice), `10080/tcp` (WebQuery, manager → server),
     `10022/tcp` (SSH ServerQuery), `30033/tcp` (file transfer).
   - **Translator-enabled (`--profile voice-translator`):** all of the
     above, plus `7880/tcp` (LiveKit signaling/WebSocket),
     `7881/tcp` (LiveKit RTC TCP fallback), `3478/udp + 3478/tcp`
     (coturn STUN/TURN), `49152-65535/udp` (LiveKit RTC media; range
     configurable down to `50000-50100/udp` for restrictive firewalls).
3. **TURN exposure.** Public IP deployments without restrictive NAT
   can run with LiveKit's embedded ICE only. Symmetric-NAT or
   firewalled deployments need coturn on a public address; the
   runbook gives the rootless coturn compose snippet plus the
   `realm`/`use-auth-secret` config.
4. **Observability hooks.** LiveKit emits Prometheus on `:6789`;
   coturn emits Prometheus on `:9641`; the translator daemon (WS-7)
   exposes a `/metrics` Prometheus endpoint and emits structured
   `tracing` logs under target `voice::translator`. The runbook
   names the dashboards we ship and the alert lines we recommend.
5. **First-connect smoke test.** The runbook ends with the
   `livekit-cli load-test`–driven five-minute smoke test that
   exercises TS6 → translator → LiveKit → browser → translator
   → TS6 round-trip. Pass criterion: <300 ms end-to-end p95.
6. **Backup and rotation.** TS6 server data volume rotation, LiveKit
   API-secret rotation, coturn shared-secret rotation, translator
   restart playbook.

## Cleanroom posture

This ADR is drafted from public sources only — LiveKit's GitHub
README and rootless-deploy docs, mediasoup's site, ion-sfu's GitHub
README, coturn's GitHub README, RFC 5766 (TURN), RFC 5389 (STUN), RFC
7587 (Opus), RFC 8829 (SDP / WebRTC), the
`teamspeaksystems/teamspeak6-server` Docker Hub page, the upstream
`ReSpeak/tsclientlib` source tree, and the in-repo ADR-0005,
PURA-7 / PURA-101 / PURA-105 / PURA-106 artefacts.

No use of the cleanroom-forbidden `Agent-Fennec/ts6-manager` reference
repo. No closed-source TeamSpeak documentation. Any spec ambiguity
during WS-7 implementation gets resolved via PURA-7-style spikes
against the self-hosted fixture or the public beta.voice endpoint —
**not** by reading the forbidden reference.

## Hard-constraint reminders carried forward

- **No upstream PR / FR / bug filings without explicit board ack on the
  PURA-108 thread.** Applies to LiveKit, coturn, mediasoup, ion-sfu,
  `tsclientlib`, `teamspeak6-server`, and any other third-party
  project we depend on. Round-trip: document internally → draft
  external post text on the [PURA-108](/PURA/issues/PURA-108) thread →
  wait for ack → file under the board's identity.
- **Self-host first.** The default `voice` profile must bring up TS6
  native rootless in <10 minutes on a fresh box with no hosted-only
  dependencies. The `voice-translator` profile must do the same with
  the bridge enabled. Hosted convenience layers (e.g. a paid TURN
  service) may be *added* by an operator; they may not be *required*.
- **Open standards on the WebRTC side.** Opus on the audio path,
  DTLS-SRTP on the media path, ICE/STUN/TURN on the connectivity
  path. The translator does not re-implement any of these; it
  consumes them via LiveKit's Rust SDK. If we find ourselves
  drafting a custom SDP munger or wire packet, stop.
- **Cleanroom rule.** No read of `Agent-Fennec/ts6-manager`. SFU and
  TURN integration questions get answered against the public LiveKit
  / coturn projects.

## Workstream impact

- **WS-3 (this ADR):** lands once the board acks. Closes
  [PURA-111](/PURA/issues/PURA-111).
- **WS-7 (translator implementation):** unblocked by board ack on
  this ADR. Scope: ship the Rust translator daemon, the
  `voice-translator` compose profile, and the operator runbook.
  Owner: VoiceEngineer once hired (WS-6); CTO if the hire slips
  past the WS-7 spawn window.
- **WS-2 (latency budget memo):** consumes this ADR's two-hop
  topology when sketching the bridge-path budget chart. No change
  to the TS6-native budget chart.
- **ADR-0005:** unchanged in substance; cross-link added to point
  WS-3 readers at this ADR.
