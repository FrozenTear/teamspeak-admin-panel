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
ticket. Slice b (the **scaffold** — `crates/ts6-voice-translator`) joins
the TS6 voice room as a synthetic `tsclientlib` client, mints a LiveKit
access token (HS256 JWT, video grant for the room), and brings up a
stub LiveKit bridge so an operator can validate both legs of the bridge
before audio forwarding lands. Slices c-e fill in the actual Opus
forwarding — see "What remains in WS-7" below.

## Bring-up

```bash
# Default (development) bring-up — uses the dev keys baked into the compose file
make voice-translator-up

# Smoke check — LiveKit health on :7880 and a real STUN Binding Request to coturn
make voice-translator-smoke

# Slice-b daemon dry-run — TS6 handshake + LiveKit token mint + stub bridge
make voice-translator-run VOICE_TRANSLATOR_DURATION=10

# Slice-b unit tests — JWT roundtrip + signature/TTL bounds
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
- **WS-7c** — TS6 → LiveKit half-duplex. Replace `StubLiveKitBridge`
  with a real `livekit` Rust SDK-backed implementation; forward
  inbound TS6 Opus frames into a LiveKit room as a publisher track;
  browser tab can hear native TS6 clients.
- **WS-7d** — LiveKit → TS6 reverse path. Subscribe to LiveKit Opus,
  forward into the TS6 voice room as a synthetic-client send so native
  clients hear the browser.
- **WS-7e** — Browser demo + acceptance recipe. LiveKit Web SDK demo
  page (or `meet.livekit.io` against the local SFU), end-to-end ≥30 s
  bidirectional audible voice between TS6 client and browser tab,
  acceptance per [PURA-114](/PURA/issues/PURA-114) description.

## Cleanroom

This deliverable is drafted from public sources only — LiveKit's GitHub
README and `config-sample.yaml`, coturn's GitHub README and
`turnserver.conf` man page, RFC 5389 (STUN), RFC 5766 (TURN), and the
in-repo ADR-0005 / ADR-0006 / WS-4 prototype artefacts. The forbidden
`Agent-Fennec/ts6-manager` reference is not read.
