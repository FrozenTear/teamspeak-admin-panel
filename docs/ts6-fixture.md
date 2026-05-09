# TS6 server fixture (dev / QA bring-up)

Operator-facing notes for running the upstream `teamspeak6-server` image
locally as a target for the manager. Tracks: [PURA-105](/PURA/issues/PURA-105).

## Required: `--network=host` (rootless podman)

Run the fixture with **host networking**, not the default rootless
`-p host:ctr` port-forward:

```bash
podman run -d --name ts6-fixture \
  --network=host \
  -e TSSERVER_LICENSE_ACCEPTED=accept \
  -e TSSERVER_QUERY_ADMIN_PASSWORD=qa-admin \
  -e TSSERVER_QUERY_SKIP_BRUTE_FORCE_CHECK=1 \
  -v ts6-fixture-data:/var/tsserver \
  docker.io/teamspeaksystems/teamspeak6-server:latest
```

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
