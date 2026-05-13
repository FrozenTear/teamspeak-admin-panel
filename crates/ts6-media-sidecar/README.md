# `ts6-media-sidecar` — Phase-5 MoQ + WebTransport video sidecar

WS-1 scaffold under [PURA-136](/PURA/issues/PURA-136). Boots a
QUIC/WebTransport listener (ALPN-pinned to `moq-lite-04`) and an axum
control-plane HTTP surface. **No real media yet** — the FFmpeg pipeline
and source-control REST plane land in WS-2 and WS-3.

Pinning rationale lives in
[ADR-0007](../../docs/adr/0007-moq-flavor-and-draft-pin.md). The WS-0
two-tab smoke that ratified the pins lives in [`moq-spike/`](../../moq-spike/).

## Why this is its own workspace

PURA-139 attempted to add this crate to the main `teamspeak-admin-panel`
workspace as the WS-1 ticket asked. That fails today: `moq-native@0.14`
enables `parking_lot/deadlock_detection`, `dioxus-server@0.7` (used by
`crates/ts6-manager-server`) enables `parking_lot/send_guard`, and
parking_lot itself rejects that combination at compile time. Same
containment trick that [`moq-spike/`](../../moq-spike/) uses.

Drift control is **by-policy**: keep `moq-lite` / `moq-native` / `hang`
versions identical in `moq-spike/Cargo.toml` and `Cargo.toml` here.
Bumps land in one PR touching both files. Future work to collapse the
two workspaces back together (upstream parking_lot fix or a deliberate
vendor of moq-native) is tracked under the Phase-5 epic.

## Build

The crate has its own `Cargo.lock` / `target/`. Run from the crate root:

```sh
cd crates/ts6-media-sidecar
cargo build
```

## Run

The transport listener needs a TLS keypair. For dev / smoke testing,
generate one in-memory with `--tls-generate <hostname>`:

```sh
cd crates/ts6-media-sidecar
cargo run -- \
    --listen '[::]:4443' \
    --http-listen '127.0.0.1:7080' \
    --tls-generate localhost
```

For production, supply an on-disk keypair. Repeat the flag if you need
to load multiple cert/key pairs (e.g. SAN per hostname):

```sh
cd crates/ts6-media-sidecar
cargo run --release -- \
    --listen '[::]:4443' \
    --http-listen '127.0.0.1:7080' \
    --cert /etc/ts6-media-sidecar/fullchain.pem \
    --key  /etc/ts6-media-sidecar/privkey.pem
```

## Control-plane endpoints (WS-1)

| Method | Path                  | Body                                                                                                |
| ------ | --------------------- | --------------------------------------------------------------------------------------------------- |
| GET    | `/health`             | `{ "status": "ok", "uptime_s": N, "sessions": N, "broadcasts": N }`                                 |
| GET    | `/stats`              | `{ "uptime_s": N, "active_sessions": N, "lifetime_sessions": N, "registered_broadcasts": [...] }`   |
| GET    | `/certificate.sha256` | `text/plain` SHA-256 hex digest of the configured cert (matches `serverCertificateHashes` in WS-0). |

All other routes return `404`.

```sh
curl -sf http://127.0.0.1:7080/health   | jq .
curl -sf http://127.0.0.1:7080/stats    | jq .
curl -sf http://127.0.0.1:7080/certificate.sha256
```

## Pointing a `moq-lite-04` browser at it

The browser-side flow is the one ratified in WS-0
([`moq-spike/player/`](../../moq-spike/player/)) — fetch the cert
fingerprint from `/certificate.sha256`, feed it into
`WebTransport(url, { serverCertificateHashes: [{ algorithm: "sha-256", value: bytes }], protocols: ["moq-lite-04"] })`,
then run a `moq-lite-04` subscriber against the WT URL.

The WT URL is `https://<host>:<listen-port>/anon`. With the dev defaults:

```
https://localhost:4443/anon
```

There are no broadcasts to subscribe to yet — WS-2 is the issue that
wires FFmpeg + IVF/Ogg into `SidecarOrigin::register_broadcast`.

## Self-signed cert + Helium / Chromium

Same pattern as the WS-0 spike: launch the browser with
`--ignore-certificate-errors-spki-list=<base64-spki-hash>`. See
[`moq-spike/README.md`](../../moq-spike/README.md) for the full recipe.
Production cert management is a WS-7 / operator-experience concern.

## Smoke test

The integration smoke under `tests/smoke.rs` boots the sidecar lib on
ephemeral ports, asserts the JSON shape of every endpoint, registers a
broadcast through `SidecarOrigin::register_broadcast`, and re-checks
`/stats`. Run it standalone:

```sh
cd crates/ts6-media-sidecar
cargo test --test smoke
```

## What this crate does NOT do yet

- **No FFmpeg pipeline** — `SidecarOrigin::register_broadcast` returns
  a `BroadcastProducer` but nothing writes to it. WS-2.
- **No `/source` REST plane** — control-plane HTTP is read-only at this
  stage. WS-3.
- **No SSRF allow-list** for source URLs. WS-3.
- **No quality presets** (resolution / bitrate / FPS knobs). WS-4.
- **No Dioxus widget** — the player is the no-build reference subscriber
  in [`moq-spike/player/`](../../moq-spike/player/) until WS-5.
