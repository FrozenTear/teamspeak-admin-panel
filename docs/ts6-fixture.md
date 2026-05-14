# TS6 server fixture (dev / QA bring-up)

Operator-facing notes for running the upstream
`docker.io/teamspeaksystems/teamspeak6-server` image locally as a target
for the manager. Same image is used as a GitHub Actions service container
under the `ts6-fixture-smoke` CI job (`.github/workflows/ci.yml`); see
§"CI service container" below.

Tracks: [PURA-105](/PURA/issues/PURA-105) (passt wedge),
[PURA-170](/PURA/issues/PURA-170) (CI wiring, OD3 ratification).

## Canonical local bring-up: `make ts6-up`

For new engineers / verification runs, the shortest path is:

```bash
make ts6-up        # starts the fixture, prints API key + env block
make ts6-apikey    # re-prints the key
make ts6-logs      # tail container logs
make ts6-down      # stop + remove
```

`make ts6-up` is a thin wrapper over the `ts6-fixture` compose profile
described below — it bakes in `--network=host` and prints the per-run
API key the verification tests need.

## Required: `--network=host` (rootless podman)

Run the fixture with **host networking**, not the default rootless
`-p host:ctr` port-forward:

```bash
podman run -d --name ts6-fixture \
  --network=host \
  -e TSSERVER_LICENSE_ACCEPTED=accept \
  -e TSSERVER_QUERY_ADMIN_PASSWORD=qa-admin \
  -e TSSERVER_QUERY_SKIP_BRUTE_FORCE_CHECK=1 \
  -e TSSERVER_QUERY_HTTP_ENABLED=1 \
  -e TSSERVER_QUERY_SSH_ENABLED=1 \
  -v ts6-fixture-data:/var/tsserver \
  docker.io/teamspeaksystems/teamspeak6-server:latest
```

> `TSSERVER_QUERY_HTTP_ENABLED=1` and `TSSERVER_QUERY_SSH_ENABLED=1` are
> mandatory (PURA-177). Both flags default to `0` in `tsserver --help` —
> without them the image only starts the legacy telnet ServerQuery, no
> 10080/tcp WebQuery and no 10022/tcp SSH ServerQuery. The upstream image's
> `EXPOSE` list reflects this (only `10022/tcp`, `30033/tcp`, `9987/udp`
> are exposed by default).

The fixture now exposes:

| Port | Purpose |
|---|---|
| `10080/tcp` | WebQuery HTTP (manager → fixture) |
| `10022/tcp` | SSH ServerQuery (event bridge) |
| `9987/udp` | Voice |
| `30033/tcp` | File transfer |

Add a managed-server row in the manager pointing at `127.0.0.1:10080`
with the API key the fixture prints to `podman logs ts6-fixture` on
first boot.

## Why `--network=host` is mandatory

`teamspeak6-server:6.0.0-beta9` wedges its WebQuery HTTP interface after
exactly **5 successful requests** when the fixture is reached through
rootless podman's default user-mode networking (passt port-forward).
Subsequent calls return TCP-accept-then-immediate-close
(`curl` reports `000` / "Empty reply from server") until the container
is restarted. Inside the container's own netns the same call pattern
succeeds — the wedge is on the passt-translated path.

For the manager this matters because the dashboard tick worker
(`ws::dashboard_tick`) fans out four WebQuery calls every 5 s. Combined
with operator activity and widget polling, the 5-request budget
evaporates within the first 30 s of a fresh fixture boot. `--network=host`
removes passt from the path; the fixture then handles 50+ keep-alive
requests without trouble.

## What we know about the root cause

Two plausible causes; neither confirmed:

1. **Upstream antiflood miscount under passt translation.** The TS6
   query subsystem may be counting passt's translated source IP as a
   single repeat client and tripping
   `virtualserver_antiflood_points_needed_command_block: 150`.
   `TSSERVER_QUERY_SKIP_BRUTE_FORCE_CHECK=1` is set in the fixture above
   and **does not** mitigate; `--query-ip-allow-list` for the passt
   egress IP has not been tested.
2. **passt bug forwarding rapid sequential connections** to a backend
   that holds keep-alive sockets open. cachyos passt 5.8.2 + podman
   5.8.2 was the observed combination. A minimal repro outside of TS6
   would be needed before any upstream report — see the parent's
   no-upstream-PR-without-board-approval rule.

Neither investigation is on the critical path; the workaround unblocks
QA today.

## Reproduction (for anyone chasing root cause)

```bash
# fixture brought up with -p (the failing case)
podman run -d --name ts6-qa \
  -p 127.0.0.1:10080:10080/tcp \
  -p 127.0.0.1:10022:10022/tcp \
  -p 127.0.0.1:9987:9987/udp -p 127.0.0.1:30033:30033/tcp \
  -e TSSERVER_LICENSE_ACCEPTED=accept \
  -e TSSERVER_QUERY_ADMIN_PASSWORD=qa-admin \
  -v ts6qadata:/var/tsserver \
  docker.io/teamspeaksystems/teamspeak6-server:latest

API_KEY=$(podman logs ts6-qa 2>&1 | grep -oE 'apikey=[^ ]+' | head -1 | cut -d= -f2)

# 30 sequential probes — wedges at #6, persists until restart
for i in $(seq 1 30); do
  curl -s -o /dev/null -w '%{http_code} ' \
    -H "x-api-key: $API_KEY" "http://127.0.0.1:10080/1/serverinfo"
done
echo
# expected: 200 200 200 200 200 000 000 000 000 …
```

Inside the container's own netns, the same 30 probes all return `200`.

## Containerised fixture in `podman-compose.yml`

The repo's `podman-compose.yml` defines a profile-gated `ts6-fixture`
service that bakes the `--network=host` requirement in. Bring it up
with:

```bash
podman-compose --profile ts6-fixture up -d ts6-fixture
```

The compose-managed fixture uses a named volume (`ts6-fixture-data`) so
the API key persists across `podman-compose down`.

## CI service container (PURA-170)

Per OD3 (board-ratified on [PURA-169](/PURA/issues/PURA-169) on 2026-05-14)
the same upstream image is booted as a CI service container on every
push / PR in the `ts6-fixture-smoke` job
(`.github/workflows/ci.yml`). The job:

1. Pulls `docker.io/teamspeaksystems/teamspeak6-server:latest` (tag tracks
   the `ts6-fixture` profile in `podman-compose.yml`; pin tighter here if
   chasing a regression — no need to touch the local profile).
2. Boots the container with `--network=host` and the standard env block
   (`TSSERVER_LICENSE_ACCEPTED=accept`,
   `TSSERVER_QUERY_ADMIN_PASSWORD=qa-admin`,
   `TSSERVER_QUERY_SKIP_BRUTE_FORCE_CHECK=1`).
3. Tails the container logs for the freshly minted `apikey=…` line, then
   polls `GET /1/version` until it returns `200` (proves the WebQuery HTTP
   surface is up).
4. Exports `TS6_INTEGRATION_{HOST,PORT,API_KEY}` into the workflow env
   (the API key is masked in workflow logs via `::add-mask::`).
5. Runs `cargo test -p ts6-manager-server --locked --
   integration_against_local_ts6_host_when_env_configured --nocapture`
   — the env-gated WebQuery integration test in
   `crates/ts6-manager-server/src/webquery/tests.rs`. It exercises
   `version`, `hostinfo`, `serverlist`, `serverinfo`, `clientlist`, and
   `banlist` against the live host — Chapter 1 verifications V2 (server
   credentials accepted) and V3 (live dashboard reads) on the read path.
6. Dumps `docker logs ts6-fixture` on failure and unconditionally removes
   the container in a final cleanup step.

The job is intentionally **not** a service-container declaration
(`services:` block) — TS6's first-boot API key is only available via
container logs, and we want to fail loudly + with full logs if the boot
or HTTP handshake regresses. Service-container declarations don't expose
that ergonomically.

**Out of scope.** Mutating verifications (V4 kick, V5 music-bot
audio-out) and SSH-bridge integration are not in this smoke. The SSH
ServerQuery integration test still uses the `teamspeak3-server` image
under the separate `ssh-integration` compose profile because upstream TS6
SSH ServerQuery is not yet stable. The TS6 voice-fixture audio E2E
(`crates/ts6-voice-fixture/tests/audio_e2e.rs`) remains a local-only
target gated on `TS6_VOICE_FIXTURE=1`.

**No permanent staging instance.** OD3 ratification explicitly defers
the staging-instance question until Phase 2 OPS and the first
end-to-end smoke is passing.

## Audio-E2E assertion (PURA-110)

The `ts6-voice-fixture` crate ships a feature-gated integration test that
asserts Opus frames flow end-to-end through the live fixture, not just
that the connection succeeds. It builds on PURA-106 (connect-only) and
the PURA-7 Day-2 voice-tx spike findings (body layout
`voice_id u16 BE | codec_id u8 | opus_payload`).

**What it does:**

1. Spawns two `tsclientlib` participants against `127.0.0.1:9987`.
2. The sender encodes a 20 ms Opus frame (440 Hz mono sine) and re-uses
   the same encoded payload for ≥1500 frames (≥30 s @ 20 ms cadence),
   then a final empty Opus frame as the voice-stop signal.
3. The receiver collects every `S2C` audio frame off the wire.
4. Asserts: frame count ≥(1 − drop_tol)×sent (default 5 % UDP-loss
   budget); every frame uses the codec the sender used (default
   `OpusVoice` = byte 4); ≥1 voice-stop received.

**Run it:**

```bash
podman-compose --profile ts6-fixture up -d ts6-fixture
TS6_VOICE_FIXTURE=1 cargo test -p ts6-voice-fixture \
    --features ts6-voice-fixture -- ts6_voice_fixture::audio_e2e \
    --ignored --nocapture
```

The test is gated *three* ways so it cannot wedge default CI:

| Gate | Purpose |
|---|---|
| `--features ts6-voice-fixture` | Pulls in `audiopus` + `tsproto-packets`; without it the test file is `cfg`-stripped to nothing. |
| `#[ignore]` | `cargo test --workspace` skips it even when the feature is on; only `--ignored` includes it. |
| `TS6_VOICE_FIXTURE=1` env | Runtime guard. The test prints a skip line if missing, so the operator knows why nothing ran. |

**Tunables (env vars):**

| Var | Default | Purpose |
|---|---|---|
| `TS6_VOICE_FIXTURE_ADDR` | `127.0.0.1:9987` | Voice port to connect to. |
| `TS6_VOICE_FIXTURE_FRAMES` | `1500` | Frames to send (×20 ms). |
| `TS6_VOICE_FIXTURE_DROP_TOL` | `0.05` | Fractional drop budget (0.0 = strict). |
| `RUST_LOG` | see below | Standard tracing filter. |

A useful filter:

```
RUST_LOG=info,ts6_voice_fixture=debug,tsclientlib=warn,tsproto=warn
```

**Failure modes the test surfaces with a useful diagnostic:**

- *Handshake never completes.* Likely the fixture is wedged on the
  passt port-forward issue (§ "Why `--network=host` is mandatory").
  The error names the symptom and points back to the compose recipe.
- *Frames sent but ≪drop_tol received.* Diagnostic includes the
  receiver's last-seen `CanSendAudio` / `CanReceiveAudio` state and a
  hint to check that the default Guest server-group has
  `b_channel_voice_speak` in the Default Channel — the
  `beta.voice.teamspeak.com` policy that bit the Day-2 spike could
  apply to a fresh self-hosted fixture too if the operator has tightened
  Guest perms.
- *Codec mismatch.* The receiver got an unexpected `codec_id` byte.
  Should be impossible against the sender we control; if it fires,
  the on-wire `AudioData` parse contract has drifted.
- *No voice-stop.* The sender dispatched one but the receiver never
  saw an empty-payload S2C frame; usually downstream of one of the
  failures above (receiver disconnected before tail).

The test is **not** part of any default CI lane. It exists as a
single-command local-rig regression net for PURA-108 / Phase 3 work
and is consumed by the WS-4 prototype (parent ticket
[PURA-108](/PURA/issues/PURA-108)). Owner: VoiceProtocol.
