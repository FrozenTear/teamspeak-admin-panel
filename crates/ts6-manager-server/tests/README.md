# Test fixtures playbook

Operational notes for the human-driven QA loop and CI smoke tests that
exercise the TS6-manager against a real TS6 server.

This file lives next to the manager crate because the playbook is the
glue between two siblings:

1. **TS6 server fixture** — `docker.io/teamspeaksystems/teamspeak6-server`
   container.
2. **Voice-client fixture** — [`ts6-voice-fixture`](../../ts6-voice-fixture)
   binary in the workspace; connects-only, no audio.

The fixtures are referenced together (not packaged together) so QA can
choose to run only the server or run both.

## When to use this

- **PURA-74** Phase-2 verification 4 (kick → observed disconnect).
- **PURA-74** Phase-2 verification 7 (public widget online-count tick).
- Any future visual / observational e2e that needs ≥1 connected voice
  client without involving the desktop CEF app.

## Prerequisites

- Rootless `podman` 5.x and a recent `docker.io/teamspeaksystems/teamspeak6-server` image.
- Rust toolchain pinned by `rust-toolchain.toml`.
- Free UDP/9987, TCP/10080, TCP/10022, TCP/30033 on host.

## 1 — TS6 server fixture

Use `--network=host`. The default rootless-podman `passt` port-forward
wedges the upstream HTTP query interface after exactly 5 sequential
requests; `--network=host` avoids it. Root cause is tracked on
[PURA-105](/PURA/issues/PURA-105) and is not blocking.

```bash
podman run -d --name ts6-qa --network=host \
  -e TSSERVER_LICENSE_ACCEPTED=accept \
  -e TSSERVER_QUERY_ADMIN_PASSWORD=qa-pura-admin \
  -e TSSERVER_QUERY_POOL_SIZE=32 \
  -e TSSERVER_QUERY_SKIP_BRUTE_FORCE_CHECK=1 \
  -v ts6qadata:/var/tsserver \
  docker.io/teamspeaksystems/teamspeak6-server:latest

# Confirm the HTTP query interface answers (root key prints once on first boot —
# grab it from the container logs, then use it as the api-key):
podman logs ts6-qa 2>&1 | grep -A1 "ServerAdmin\|api-key"
curl "http://127.0.0.1:10080/version?api-key=<paste-here>"
```

Tear down:

```bash
podman rm -f ts6-qa
podman volume rm ts6qadata   # only if you want a clean identity slate
```

## 2 — Voice-client fixture (`ts6-voice-fixture`)

A connect-only headless TS3-protocol client. Generates (or loads) a
TeamSpeak identity, completes the TS6 handshake against `--server`, and
stays connected until SIGINT/SIGTERM.

### Build

```bash
cargo build -p ts6-voice-fixture --release
# Binary path is workspace target dir + release/ts6-voice-fixture, e.g.:
ls "$(cargo metadata --no-deps --format-version 1 \
  | python3 -c 'import json,sys;print(json.load(sys.stdin)["target_directory"])')"/release/ts6-voice-fixture
```

The binary pins `tsclientlib` to upstream master @ `04aa249` (see
`crates/ts6-voice-fixture/Cargo.toml` for why — the crates.io 0.2.0 build
predates the TS6 license-chain support and fails handshake).

### Run a single client

```bash
./target/release/ts6-voice-fixture \
  --server 127.0.0.1:9987 \
  --name 'qa-fixture-A' \
  --identity-dir /tmp/ts6-voice-fix-A
```

`identity.json` is created inside `--identity-dir` on first run and
re-used on subsequent runs (faster boot, stable UID). Delete the dir to
re-generate.

Stop with `Ctrl-C` (SIGINT) or `kill -TERM <pid>`.

### Run two clients in parallel (kick / online-count tests)

Each instance MUST point at its own identity dir.

```bash
./target/release/ts6-voice-fixture --server 127.0.0.1:9987 \
  --name qa-fixture-A --identity-dir /tmp/ts6-voice-fix-A &
PID_A=$!
./target/release/ts6-voice-fixture --server 127.0.0.1:9987 \
  --name qa-fixture-B --identity-dir /tmp/ts6-voice-fix-B &
PID_B=$!

# … run V4/V7 verifications against the manager …

kill -TERM $PID_A $PID_B
```

### Useful logging

```bash
RUST_LOG=info,ts6_voice_fixture=debug,tsclientlib=debug,tsproto=info \
  ./target/release/ts6-voice-fixture …
```

Set to `trace` if you need the full per-packet stream (very chatty).

## 3 — V4 smoke recipe (kick → observed disconnect)

Acceptance from [PURA-74](/PURA/issues/PURA-74):

> Connect two distinct TS clients to the host. Authenticate to the
> manager FE as an operator with kick permissions. Use the Clients
> page to kick one of the connected clients. Confirm: the target
> client disconnects on the TS host, live Clients page reflects the
> disconnect via WS, activity feed shows the operator action.

```bash
# 1. Boot the TS6 server fixture (see §1).
# 2. Boot the manager (see top-level README / podman-compose.yml).
# 3. Spawn two voice fixtures (see §2 "two clients in parallel").
# 4. Wait 2 s for the manager to refresh its dashboard tick:
sleep 2
# 5. Sanity-check both clients show up via WebQuery:
curl -s -H "Authorization: Bearer $JWT" \
  "$MANAGER/api/servers/1/vs/1/clients" \
  | python3 -m json.tool | grep -E '"client_nickname"|"client_type"'
# Expect: qa-fixture-A and qa-fixture-B with client_type=0 (voice clients).

# 6. Kick fixture A via the FE (or REST, e.g.):
curl -s -X POST -H "Authorization: Bearer $JWT" \
  "$MANAGER/api/servers/1/vs/1/clients/<clid-A>/kick" -d '{"reason":"PURA-74 V4"}'

# 7. Observe:
#    - $PID_A logs "DisconnectedTemporarily(…)" and exits within ~2 s.
#    - GET /api/servers/1/vs/1/clients no longer lists qa-fixture-A.
#    - WS frame for topic "server:1:clients" shows the new client count.

kill -TERM $PID_B
```

## 4 — V7 smoke recipe (widget online-count tick)

```bash
# 1. Boot TS6 + manager + a public widget (the wizard's "create widget"
#    UI or POST /api/widgets) — note the widget token.
# 2. Open the widget page (or curl /api/widget/{token}/data) and read the
#    initial onlineUsers count (should be 0 with no clients connected).
# 3. Spawn one voice fixture:
./target/release/ts6-voice-fixture --server 127.0.0.1:9987 \
  --name qa-fixture-A --identity-dir /tmp/ts6-voice-fix-A &
PID_A=$!
# 4. Within ≤5 s, the WS frame for topic "server:1:widget" should publish
#    an updated count. Capture with wscat or the widget HTML page.
# 5. Disconnect:
kill -TERM $PID_A
# 6. Within ≤5 s the WS frame should drop the count back to 0.
```

V7 also requires the [PURA-103](/PURA/issues/PURA-103) WS topic-isolation
fix to land before the redaction-guarantee half of V7 can be closed —
that's a separate blocker, not a fixture problem.

## 5 — Known nuisances (not blockers)

- `tsclientlib` master logs `Unknown argument` warnings for
  `virtualserver_address`, `virtualserver_version_sign`, and
  `client_is_streaming`. These are upstream `tsdeclarations` schema
  gaps tracked on [PURA-18](/PURA/issues/PURA-18). They do not affect
  connect, list-clients, or kick semantics.
- `Packet N not in receive window` warnings appear during the post-
  init burst as the server replays the channel/clientgroup tree out
  of order. tsclientlib treats them as non-fatal; the connection
  remains `Connected`.
- The fixture identity is generated at PoW level 8 by default (cheap,
  ~ms). If a TS6 server enforces higher minimums, tsclientlib will
  emit `IdentityLevelIncreasing(<level>)` events and upgrade in-place
  before retrying connect. No manual intervention required.

## 6 — When NOT to use this fixture

- **Audio playback / capture.** Out of scope. Use a real desktop client
  for any test that depends on audible voice.
- **Driving the FE visually.** This fixture replaces the desktop voice
  client only. Playwright / browser-driving still uses the same headed
  or headless Chromium path.
- **Production voice bots.** The Wave-4 voice-bot crate (see PURA-7
  final memo) will use the same upstream `tsclientlib` master pin but
  add audio + the per-bot lifecycle on top. This fixture is QA-only.
