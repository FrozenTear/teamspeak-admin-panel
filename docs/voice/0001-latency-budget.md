# Voice latency budget — TS6 native lead (Phase 3)

- **Status:** Proposed.
- **Date:** 2026-05-09.
- **Author:** VoiceProtocol.
- **Reviewers:** CTO / board.
- **Source ticket:** [PURA-109](/PURA/issues/PURA-109) (WS-2 under [PURA-108](/PURA/issues/PURA-108)).
- **Companion:** [ADR-0005 rev 2](docs/adr/0005-voice-path-decision.md) (lead-path bet).

## Targets

- **Esports:** ~80 ms one-way mouth-to-ear. Restated against TS6 client → self-hosted `teamspeak6-server` → TS6 client. The server is a single forwarder, so the network portion is **two** one-way UDP segments (A→server + server→B), not one.
- **Community:** ~150 ms one-way mouth-to-ear. Same topology.

The "single UDP hop" framing in [PURA-108](/PURA/issues/PURA-108)'s WS-2 brief refers to the server doing one forward (vs. an SFU cascade); the audio path still crosses two UDP segments.

## Topology under measurement

- **Server:** `docker.io/teamspeaksystems/teamspeak6-server:6.0.0-beta9`, brought up via `podman-compose --profile ts6-fixture up -d ts6-fixture` (`network_mode: host`, per `docs/ts6-fixture.md` — passt wedge workaround).
- **Client:** `tsclientlib` master `@04aa249` (the pin already carried by the admin-panel control surface, [PURA-101](/PURA/issues/PURA-101)). Audio handler in `src/audio.rs` decodes Opus at 48 kHz stereo with an adaptive jitter queue (sliding-window minimum over the last 255 packets, `MAX_BUFFER_PACKETS = 50`, `MAX_BUFFER_TIME = 0.5 s`).
- **Voice frame layout:** `voice_id u16 BE | codec_id u8 | opus_payload`, settled in PURA-7 Day 2.
- **Codec:** Opus 20 ms frames, mono or stereo, CELT or hybrid mode.

## Per-stage breakdown (one direction, mouth-to-ear)

| Stage | Source | Esports profile (knobs tuned) | Community profile (defaults) |
|---|---|---|---|
| Mouth → capture chunk fill (avg = period/2) | PipeWire quantum | 5 ms (10 ms quantum) | 10 ms (20 ms quantum) |
| Opus encode (frame + lookahead + wall) | RFC 7587 §2.1.4: 20 ms frame + 6.5 ms lookahead; +~1 ms wall | 27.5 ms | 27.5 ms |
| Egress (tokio pacing + UDP send) | Userspace; <1 ms on Linux high-res timer | <1 ms | <1 ms |
| A → server (one-way) | Measured | <0.1 ms LAN / ~13 ms WAN | <0.1 ms LAN / ~13 ms WAN |
| Server forward | Measured (loopback proxy) | <1 ms | <1 ms |
| Server → B (one-way) | Measured | <0.1 ms LAN / ~13 ms WAN | <0.1 ms LAN / ~13 ms WAN |
| Recv jitter buffer hold | tsclientlib `audio.rs` adaptive; floor = 1 frame | 20 ms (1 frame, floored) | 60 ms (~3 frames, default) |
| Opus decode | Causal | 1 ms | 1 ms |
| Output → ear (avg = period/2) | PipeWire quantum | 5 ms (10 ms quantum) | 10 ms (20 ms quantum) |
| **Total LAN-fixture (median)** | | **~60 ms** | **~110 ms** |
| **Total WAN (median)** | | **~86 ms** | **~135 ms** |
| **Total WAN (p99, +~8 ms jitter)** | | **~94 ms** | **~143 ms** |

The 26.5 ms Opus algorithmic delay is the **dominant** term and is structural (from the codec's frame size + lookahead). It is not negotiable below 25 ms without dropping to SILK-only mode (which costs music quality) or cutting frame size to 10 ms (which doubles per-second packet rate and worsens jitter-buffer behavior). The recommendation is to leave it alone.

## Measurements

Instrumentation source: this host (Norway / Telia transit). Two anchor points: localhost loopback to the running `ts6-fixture` container, and last-reachable-hop near `141.95.98.185` (= `beta.voice.teamspeak.com`, OVH Frankfurt) as a stand-in because beta.voice filters ICMP and replies neither RST nor ICMP-port-unreachable on UDP/TCP probes.

### Local fixture (LAN-quality server hop)

ICMP loopback to `127.0.0.1`, n=200 at 20 ms inter-probe:

```
rtt min/avg/max/mdev = 0.008/0.015/0.032/0.004 ms
```

One-way "server forward" cost ≈ 0.01 ms median, 0.02 ms p99. **Negligible against the 80 ms esports budget.** The TS6 server's worker thread adds a small bounded delay on top, but the forward path on a single-host fixture is not a budget item — there is no realistic configuration where this stage reaches 1 ms.

### Representative WAN (beta.voice path)

Direct probes to `141.95.98.185` are filtered (no ICMP reply, no TCP RST on 9987, no ICMP-port-unreachable on UDP/9987). Used `tracepath` against the same destination over 8 runs and pulled RTT samples from the last reachable hops sitting on the OVH ingress (`fra-fr5-pb1-ptx.de.eu`, `ffm-b11-link.ip.twelve99.net`, `ffm-bb1-link.ip.twelve99.net`):

```
fra-fr5-pb1-ptx.de.eu RTT (n=5):  25.6 / 25.8 / 25.8 / 25.8 / 25.9 ms
ffm-b11-link RTT       (n=8):  25.7 / 26.7 / 26.7 / 26.9 / 27.9 / 28.9 / 41.7* / 26.7 ms
ffm-bb1-link RTT       (n=8):  25.8 / 26.8 / 26.9 / 26.9 / 27.0 / 27.9 / 29.8 / 45.4*
```

`*` = transient queue spike. Steady-state RTT is ~26 ms (one-way ~13 ms); rare outliers ~42 ms RTT (~21 ms one-way). p99 of one-way under steady transit is ~14 ms; including transients it rises to ~21 ms.

These numbers are tied to my regional placement (Telia/Frankfurt, ~15 ms physical distance). For an operator hosting their own TS6 server on a DC closer to their player base, the per-leg one-way drops accordingly. **The budget table assumes ~13 ms one-way per leg as a representative regional case; a community where every player is within 10 ms of the operator's server gets ~10 ms back into the budget.**

### Confidence

The latency budget table is a mix of **measured** (server hop, network) and **literature-cited** (Opus algorithmic delay from RFC 7587 §2.1.4, audio-stack period from PipeWire docs) values. The send/receive endpoints have not been instrumented end-to-end yet — that's the WS-4 prototype's job. WS-4 should validate the table by capturing wall-clock timestamps at PCM-in (talker) and PCM-out (listener), which closes the only loop where this memo extrapolates instead of measures.

## Phase-3.5 comparison: TS6 ↔ WebRTC SFU translator

[PURA-108](/PURA/issues/PURA-108) WS-7 will sit a translator alongside the TS6 server: stock TS6 clients keep talking to `teamspeak6-server`, browser users join the same room via a self-hosted SFU, and a translator process bridges the two sides over a TS6 voice membership.

Latency delta vs. TS6-native one-way budget:

| Term | TS6 native (this memo) | TS6 ↔ WebRTC (browser side) | Delta |
|---|---|---|---|
| Codec | Opus 20 ms, no transcode | Opus 20 ms, **passthrough** if SFU forwards untouched; transcode only if the SFU re-encodes for SVC/simulcast | +0 ms passthrough / +~26 ms transcoded |
| Translator hop (TS6 ↔ SFU bus) | n/a | LAN to SFU (co-located) | +1–3 ms |
| SFU forward | n/a | Single-AZ SFU forward | +1–3 ms |
| TURN relay (worst case) | n/a | Browser blocked behind symmetric NAT → TURN UDP relay | +5–30 ms (measured worst-case for public TURN clusters) |
| Browser RTP/JitterBuffer | n/a | WebRTC NetEq / Chrome neteq target = ~50 ms baseline; min ~10 ms; can balloon under loss | +40–100 ms median over tsclientlib's 20 ms floor |

Net Phase-3.5 budget for the **browser** participant on a default WebRTC client, against a TS6 server in Frankfurt:

| Profile | Median | p99 |
|---|---|---|
| Phase-3 TS6-only (esports) | 86 ms | 94 ms |
| Phase-3.5 TS6→browser (good NAT, passthrough) | ~130 ms | ~150 ms |
| Phase-3.5 TS6→browser (TURN, transcode) | ~190 ms | ~220 ms |

Translation: the **community** target (~150 ms) survives a default-config WebRTC translator with codec passthrough. The **esports** target does not survive the bridge under any realistic configuration — the WebRTC NetEq baseline alone consumes more than the 14 ms slack the esports profile has. Phase-3.5 is therefore a **community-target product**; esports stays on the TS6 native side. WS-7 should be designed around codec passthrough (no transcode) and a co-located SFU + TURN, but should not promise esports latency to the browser audience.

This matches the framing in ADR-0005: WebRTC bridge is broader-reach, not lower-latency.

## Recommendation — knobs for WS-4

WS-4 ("two clients can talk" prototype on the local TS6 fixture) should adopt the following defaults so the prototype lands inside the esports target on a LAN fixture and inside the community target on representative WAN:

1. **Opus framing:** 20 ms frames, CELT or hybrid mode (don't shrink to 10 ms). The 26.5 ms algorithmic delay is structural; trying to fight it costs more than it saves. Default bitrate 32–64 kbps for voice; the bitrate doesn't affect latency.
2. **Send pacing:** drive frame TX from a tokio interval with **absolute-deadline scheduling** (`tokio::time::Interval::tick` followed by `set_missed_tick_behavior(Burst)` or equivalent, NOT `sleep(20ms)` loops). Userspace pacing precision on Linux high-res timers is ~100 µs; relative-sleep loops drift.
3. **Receive jitter buffer:** keep `tsclientlib`'s adaptive sliding-window-minimum scheme (`audio.rs:LAST_BUFFER_SIZE_COUNT = 255`); expose two profile knobs:
   - **Esports floor:** start `buffering_samples = 1 frame (20 ms)`. Trust the adaptive expansion for jitter spikes. The floor alone saves ~40 ms vs. the implicit ~3-frame default.
   - **Community default:** `buffering_samples = 3 frames (60 ms)`. The classic TS3-client setting; safer under residential WAN jitter.
   - The jitter buffer is the **single most impactful knob in the budget** — every 20 ms of buffer is 20 ms of latency.
4. **Audio I/O on Linux:**
   - **Esports profile:** PipeWire `default.clock.quantum = 480` samples (= 10 ms at 48 kHz). Document this in the operator runbook; it is not the distro default everywhere (Fedora ships 1024 / ~21 ms by default).
   - **Community profile:** PipeWire defaults are fine.
5. **WS-4 timing instrumentation:** capture wall-clock timestamps at PCM-in on talker and PCM-out on listener; report median + p99 per session. Without this, WS-4 cannot validate this memo's table — and the table is the gating contract for whether the esports target is a real claim or a hope.
6. **Server placement guidance for operators:** the WAN budget assumes each client is ≤13 ms one-way from the server. Document a target of **≤10 ms one-way** (≤20 ms RTT) for operators who care about esports-class latency; community-target operators have no real placement constraint.

## Open questions for WS-4 / WS-7

- **Real-world tsclientlib audio-pipeline jitter under packet loss.** The adaptive buffer expands; how much, how fast, what's the audible artifact rate? Worth a 5%-loss test in WS-4.
- **TS6 server's internal audio-frame queueing.** Loopback ICMP measures the IP path, not the server-thread-to-thread forward. Is `teamspeak6-server` adding ≥1 ms of internal queueing under low-load conditions? Worth one direct A→server→B end-to-end measurement once WS-4 has two clients on the fixture.
- **Browser NetEq under our exact translator pattern.** The 50 ms NetEq baseline is for typical WebRTC traffic; the translator might present unusual jitter characteristics. Validate when WS-7 prototype lands.

## Out of scope for this memo

- SFU choice / federation topology (covered by WS-3).
- The actual two-clients-can-talk prototype (WS-4 build).
- Buying or running a beta-grade SLO against `teamspeak6-server` until it leaves beta.

---

**Cleanroom posture:** drafted from RFC 7587 (Opus), the upstream `ReSpeak/tsclientlib` source tree (`audio.rs`), `docs/ts6-fixture.md`, and direct measurements on this host. No reads of `Agent-Fennec/ts6-manager`.
