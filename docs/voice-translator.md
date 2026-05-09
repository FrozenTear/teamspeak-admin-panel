# Voice translator — Phase 3.5 WebRTC bridge

[PURA-114](/PURA/issues/PURA-114) ([WS-7 of PURA-108](/PURA/issues/PURA-108)).
Operator-facing notes for the `voice-translator` podman-compose profile that
ships the SFU + TURN side of the WebRTC bridge per [ADR-0006 Decision B](./adr/0006-voice-topology-and-translator-deployment.md).

## What this profile does

`podman-compose --profile voice-translator up -d` brings up:

- `ts6-fixture` — the upstream `teamspeaksystems/teamspeak6-server` image
  (host networking; same QA-grade defaults as the standalone `ts6-fixture`
  profile, see [`ts6-fixture.md`](./ts6-fixture.md)).
- `livekit` — `livekit/livekit-server` v1.7 in a rootless container.
  WebSocket signaling on `7880/tcp`, JWT auth, ICE/UDP single-port mux on
  `7882/udp`, ICE/TCP fallback on `7881/tcp`.
- `coturn` — `coturn/coturn` 4.6 in a rootless container with host
  networking (`3478/udp+tcp` plus `49152-49252/udp` relay range).

The translator daemon ships incrementally on the same [PURA-114](/PURA/issues/PURA-114)
ticket. Slice b shipped the **scaffold** (`crates/ts6-voice-translator`):
TS6 handshake via `tsclientlib`, LiveKit access-token minter, and a stub
publish bridge. Slice c (this revision) replaces the stub with the
**real publisher**: the daemon now joins a LiveKit room with the
official `livekit` Rust SDK, opens a `LocalAudioTrack` over a
`NativeAudioSource`, and forwards inbound TS6 Opus voice frames into
the LiveKit track 1:1. Browser participants subscribed to the room
hear the synthetic `ts6-bridge` Microphone track (slice e wires the
end-to-end browser demo). Slices d-e fill in the reverse path — see
"What remains in WS-7" below.

## Bring-up

```bash
# Default (development) bring-up — uses the dev keys baked into the compose file
make voice-translator-up

# Smoke check — LiveKit health on :7880 and a real STUN Binding Request to coturn
make voice-translator-smoke

# Slice-c daemon dry-run — TS6 handshake + LiveKit Room::connect + real
# publisher track (no talker, no audio forwarded yet — exits cleanly).
make voice-translator-run VOICE_TRANSLATOR_DURATION=10

# Slice-c bridge smoke — translator + a TS6 prototype "alice" talking
# 440Hz for 8 s, asserts audio_frames_seen == audio_frames_published > 0.
# Pass = the publisher track is forwarding TS6 Opus 1:1 into LiveKit.
make voice-translator-bridge-smoke

# Slice-d reverse smoke — two translator instances + alice on TS6.
# Each translator subscribes to the other's published track and forwards
# remote Opus into TS6. Pass = at least one translator reports
# reverse_frames_received > 0.
make voice-translator-reverse-smoke

# Unit tests — JWT roundtrip + signature/TTL bounds
make voice-translator-test

# Tear down
make voice-translator-down
```

`make voice-translator-run` is a deeper bring-up smoke than
`voice-translator-smoke`: it actually completes the TS6 handshake
against the fixture and validates that the LiveKit dev key/secret in
`deploy/voice/livekit.yaml` produce a token the LiveKit server will
accept. Override `VOICE_TRANSLATOR_TS6` / `VOICE_TRANSLATOR_LIVEKIT` /
`VOICE_TRANSLATOR_ROOM` / `VOICE_TRANSLATOR_DURATION` for non-default
deployments. Set `LIVEKIT_API_KEY` / `LIVEKIT_API_SECRET` in the
environment to override the dev defaults.

The dev keys in the compose file are explicitly weak so an operator can run
`up` without setup. **Production deployments MUST override them** — see the
"production overrides" section below.

## Port surface

| Service  | Port               | Purpose                                            |
|----------|--------------------|----------------------------------------------------|
| livekit  | `7880/tcp`         | WebSocket signaling + JWT auth + HTTP health       |
| livekit  | `7881/tcp`         | ICE/TCP fallback for restrictive networks          |
| livekit  | `7882/udp`         | ICE/UDP single-port mux                             |
| coturn   | `3478/udp + 3478/tcp` | STUN/TURN listening                              |
| coturn   | `49152–49252/udp`  | TURN relay range (intentionally narrowed from default) |
| ts6-fixture | `9987/udp`      | TS6 voice (existing)                                |
| ts6-fixture | `10080/tcp`     | TS6 WebQuery (existing)                             |

The `49152–49252/udp` range is deliberately narrower than the LiveKit
out-of-the-box `50000-60000/udp` recommendation. The narrow range keeps the
host firewall surface auditable; it carries ~50 simultaneous browser
participants per relay leg, which exceeds anything we expect on a Phase 3.5
deployment. Operators bumping past that range edit `--min-port`/`--max-port`
in the coturn command-line and (optionally) raise LiveKit's media port mux
to a range via `rtc.port_range_start`/`rtc.port_range_end` in
`deploy/voice/livekit.yaml`.

## Environment variables

| Variable                    | Default (dev)                                | Purpose                                       |
|-----------------------------|----------------------------------------------|-----------------------------------------------|
| `LIVEKIT_KEYS`              | `devkey: devsecretdevsecret…`                | YAML map of `apiKey: apiSecret`. Generate the secret via `openssl rand -hex 32`. |
| `COTURN_REALM`              | `voice.local`                                | TURN realm. Set to your operator hostname.    |
| `COTURN_AUTH_SECRET`        | `devturnsecret…`                             | Shared secret for time-limited TURN credentials. Generate via `openssl rand -hex 32`. |
| `TS6_FIXTURE_ADMIN_PASSWORD`| `qa-admin`                                   | Inherited from the existing ts6-fixture profile. |

Drop a `.env` file at the repo root with operator-supplied values; podman-compose
picks it up automatically. Do **not** commit `.env`.

## Production overrides

ADR-0006 commits to "self-host first" — the default profile works rootless
on a dev box. For a real deployment:

1. Generate strong secrets:
   ```bash
   echo "LIVEKIT_KEYS=opkey: $(openssl rand -hex 32)" >> .env
   echo "COTURN_AUTH_SECRET=$(openssl rand -hex 32)" >> .env
   echo "COTURN_REALM=voice.example.com" >> .env
   ```

2. Edit `deploy/voice/livekit.yaml` to point at coturn under
   `turn_servers` once the realm/secret are decided:
   ```yaml
   turn_servers:
     - host: turn.example.com
       port: 3478
       protocol: udp
       credential: <static-auth-secret-here>
   ```
   See the LiveKit upstream `config-sample.yaml` for the full schema.

   Then flip `rtc.use_external_ip` back to `true` and add `node_ip:
   <public-ip>` so LiveKit advertises the operator's reachable address
   in ICE candidates. The dev profile sets `use_external_ip: false`
   because LiveKit's STUN-based auto-detection picks up a WAN IP that
   the local translator daemon (talking to `ws://127.0.0.1:7880`)
   can't actually reach, so the peer-connection handshake times out
   (`wait_pc_connection timed out`).

3. Confirm the host firewall opens 3478, 7880-7882, and the coturn relay
   range. The compose mappings only land the ports inside the rootless
   network namespace — host firewall (`firewalld` / `nftables`) is the
   operator's responsibility.

4. Pin the upstream tags. The compose file pins `livekit-server:v1.7` and
   `coturn:4.6`; bump deliberately and verify with `make voice-translator-smoke`
   after each upgrade.

## Smoke test details

`make voice-translator-smoke` performs two independent checks:

- HTTP `GET http://127.0.0.1:7880/` — LiveKit's signaling server responds
  to a plain HTTP probe with a small status body. If the daemon binds and
  is healthy, this returns 200; if the container is still starting we
  retry up to 10 seconds.
- A real STUN Binding Request on `127.0.0.1:3478/udp` — sends the 20-byte
  RFC 5389 request and asserts the response is a Binding Success
  (`0x0101` message type). This proves coturn bound the port and is
  speaking STUN at all; it does *not* validate TURN allocation, which
  needs a real shared-secret credential and is exercised by the
  full-translator demo (lands in a later WS-7 slice).

Failures point at:
- `livekit OK` missing → check `podman logs ts6-livekit`. Most common
  cause is a malformed `LIVEKIT_KEYS` env value.
- coturn STUN timeout → check `podman logs ts6-coturn`. coturn refuses
  to start if the auth secret is empty; verify `COTURN_AUTH_SECRET` is
  set.

## What remains in WS-7

Slice **a** (the compose profile + dev config + smoke target + this
runbook) landed the deployment shape. Slice **b** (the daemon scaffold:
`crates/ts6-voice-translator`, TS6 handshake + LiveKit token mint + stub
bridge + `make voice-translator-{run,test}`) is the first
runnable cut of the daemon. The audio forwarding and the end-to-end
browser demo land in the remaining slices on the same WS-7 epic
([PURA-114](/PURA/issues/PURA-114)):

- ✅ **WS-7a** — `voice-translator` compose profile (LiveKit + coturn,
  rootless). [Shipped.]
- ✅ **WS-7b** — `ts6-voice-translator` daemon scaffold. TS6
  handshake via `tsclientlib` (lifted from `ts6-voice-prototype`);
  LiveKit access-token minter (HS256 JWT, video grant); stub
  publish/subscribe bridge gating slice c. [Shipped.]
- ✅ **WS-7c** — TS6 → LiveKit half-duplex. Real `livekit` Rust SDK
  publisher (`Room::connect` + `LocalAudioTrack` over a
  `NativeAudioSource`). TS6 Opus → PCM → LiveKit RTP/Opus over
  SRTP/DTLS. `make voice-translator-bridge-smoke` validates 1:1 frame
  forwarding (401 frames over 8 s). [Shipped.]
- ✅ **WS-7d** — LiveKit → TS6 reverse path. `RoomEvent::TrackSubscribed`
  spawns a per-track subscriber that drains a `NativeAudioStream`,
  reframes 10 ms PCM blocks into 20 ms windows (TS6's §19.10 framing),
  encodes to Opus with `audiopus`, and forwards via mpsc to the main
  loop, which calls `ts6.send_audio` as a synthetic-client send.
  `make voice-translator-reverse-smoke` validates the path end-to-end
  with two translator instances + a TS6 talker. [Shipped.]
- ✅ **WS-7e** — Browser demo + acceptance recipe. `deploy/voice/demo.html`
  using `livekit-client` Web SDK; `make voice-translator-mint-token`
  prints a JWT; `make voice-translator-demo-up` serves the demo via
  rootless `python3 -m http.server`. Operator-driven ≥30 s
  bidirectional acceptance check below. [Shipped.]

## Browser demo recipe (slice e)

Closes the description's acceptance bar. Runtime: ~5 minutes including
the 30 s acceptance window.

```bash
# 0. One-time / per-fresh-fixture: bring up the bridge profile.
make voice-translator-up
make voice-translator-smoke              # confirm livekit + coturn healthy

# 1. Start the translator daemon. Lives for the duration of the demo.
mkdir -p target/voice-translator/demo
./target/release/ts6-voice-translator \
    --identity-dir target/voice-translator/demo \
    --duration-secs 600 \
    >target/voice-translator/demo/translator.log 2>&1 &

# 2. Mint a JWT for the browser tab. Identity must be unique within the
#    room; use anything other than the translator's own
#    `ts6-bridge-translator`.
make voice-translator-mint-token IDENTITY=browser-demo

# 3. Serve the static demo (rootless, no npm). Leave running in another
#    terminal.
make voice-translator-demo-up
#    ==> open http://localhost:8080/demo.html

# 4. In the demo tab: paste the JWT into the Token textarea, leave URL +
#    room at defaults, click "Connect + publish microphone". Browser
#    grants mic permission; "subscribed: ts6-bridge-translator …" line
#    appears in the status pane.

# 5. In a second terminal, spawn a TS6 talker. The prototype recording
#    captures everything the browser publishes for post-run inspection.
mkdir -p target/voice-translator/demo/alice
./target/release/ts6-voice-prototype \
    --name alice --send-tone-hz 440 --duration-secs 35 \
    --identity-dir target/voice-translator/demo/alice \
    --out-wav target/voice-translator/demo/alice.wav

# 6. Speak into the browser microphone for the full window. Listen for
#    the 440 Hz tone in the browser tab's <audio> element. Watch the
#    translator's heartbeat log:
grep -E "audio_frames|reverse_frames" target/voice-translator/demo/translator.log \
    | sed 's/\x1b\[[0-9;]*[mGKH]//g' | tail
```

Acceptance:

- The 440 Hz tone is audible in the browser tab for ≥30 seconds.
- The browser microphone audio is audible in the TS6 channel; the
  prototype WAV (`target/voice-translator/demo/alice.from-*.wav`)
  contains it. Spectrogram-check is fastest:
  ```bash
  sox target/voice-translator/demo/alice.from-*.wav -n spectrogram \
      -o /tmp/browser-into-ts6.png
  ```
- Translator's final `exited cleanly` heartbeat shows
  `audio_frames_published > 0` AND `reverse_frames_sent > 0`.
- No `wait_pc_connection timed out`, no `ConnectFailedBanned`,
  no `opus encode failed` lines.

### Locking the demo SDK

The dev profile loads `livekit-client@2.5.7` from `cdn.jsdelivr.net`
without an `integrity=` attribute so a fresh-clone operator can demo
without further setup. Production-style deployments lock the SDK by:

```bash
SDK_URL="https://cdn.jsdelivr.net/npm/livekit-client@2.5.7/dist/livekit-client.umd.min.js"

# Option A — drop the file next to demo.html, fully offline.
curl -fL -o deploy/voice/livekit-client.umd.min.js "$SDK_URL"
sed -i 's#https://[^"]*livekit-client[^"]*#./livekit-client.umd.min.js#' \
    deploy/voice/demo.html

# Option B — keep the CDN URL, add Subresource Integrity.
HASH=$(curl -fLs "$SDK_URL" | openssl dgst -sha384 -binary | base64)
sed -i "s#crossorigin=\"anonymous\"#integrity=\"sha384-${HASH}\" crossorigin=\"anonymous\"#" \
    deploy/voice/demo.html
```

### Limitations of the demo page

- **No stats UI.** Track sids and connection state are dumped to the
  status log; LiveKit's full RTC stats are reachable via
  `room.localParticipant.getTrackStats()` in DevTools but the demo
  doesn't render them.
- **No reconnect handling.** The page treats `Disconnected` as terminal.
  Operator clicks Connect again to recover.
- **Mic device picking.** The demo uses the browser's default
  microphone — pick a specific device via the browser's site
  permissions UI before clicking Connect if needed.

A polished browser-side product is a separate Phase 4+ epic per
ADR-0006 §"Out of scope". The demo page exists to close the
WS-7 acceptance bar and to serve as the bring-up template for
operator-supplied UIs.
  page (or `meet.livekit.io` against the local SFU), end-to-end ≥30 s
  bidirectional audible voice between TS6 client and browser tab,
  acceptance per [PURA-114](/PURA/issues/PURA-114) description.

## Cleanroom

This deliverable is drafted from public sources only — LiveKit's GitHub
README and `config-sample.yaml`, coturn's GitHub README and
`turnserver.conf` man page, RFC 5389 (STUN), RFC 5766 (TURN), and the
in-repo ADR-0005 / ADR-0006 / WS-4 prototype artefacts. The forbidden
`Agent-Fennec/ts6-manager` reference is not read.
