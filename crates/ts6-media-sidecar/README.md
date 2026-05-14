# `ts6-media-sidecar` — Phase-5 MoQ + WebTransport video sidecar

WS-1 scaffold + WS-2 FFmpeg pipeline + WS-3 control plane under
[PURA-136](/PURA/issues/PURA-136). Boots a QUIC/WebTransport listener
(ALPN-pinned to `moq-lite-04`), an axum control-plane HTTP surface, and
— per [`POST /source`](#control-plane) call — a per-source FFmpeg
pipeline that publishes VP8 + Opus into MoQ tracks.

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

## Control plane

The sidecar serves an axum HTTP surface on `--http-listen` (default
`127.0.0.1:7080`) — operator-only, distinct from the QUIC listener.

| Method | Path                       | Purpose                                                                                              |
| ------ | -------------------------- | ---------------------------------------------------------------------------------------------------- |
| GET    | `/health`                  | Cheap liveness probe. JSON: `{ status, uptime_s, sessions, broadcasts }`.                            |
| GET    | `/stats`                   | Process + per-source counters. JSON: `{ uptime_s, active_sessions, lifetime_sessions, registered_broadcasts, sources[] }`. |
| GET    | `/certificate.sha256`      | `text/plain` SHA-256 hex digest of the cert (matches `serverCertificateHashes` in WS-0).             |
| POST   | `/source`                  | **WS-3** — start a pipeline for the given source URL. SSRF-checked. Returns `201` + `{ source_id, track }`. |
| POST   | `/source/stop`             | **WS-3** — stop a pipeline by id. Returns `204` on success, `404` if the id is unknown.              |
| GET    | `/track/{source_id}`       | **WS-3** — look up the MoQ track descriptor for a registered source. `404` if unknown.               |

All other routes return `404`.

### Read-only probes

```sh
curl -sf http://127.0.0.1:7080/health   | jq .
curl -sf http://127.0.0.1:7080/stats    | jq .
curl -sf http://127.0.0.1:7080/certificate.sha256
```

### `POST /source`

```sh
curl -sSf -XPOST http://127.0.0.1:7080/source \
    -H 'content-type: application/json' \
    -d '{
        "url": "https://example.com/stream.mp4",
        "source_id": "camera-1",
        "preset": "720p"
    }' | jq .
```

Request:

| Field       | Type   | Required | Notes                                                                                                                                                                       |
| ----------- | ------ | -------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `url`       | string | yes      | HTTP or HTTPS source URL. SSRF-blocked for loopback / private / link-local / metadata.                                                                                      |
| `source_id` | string | no       | Operator-supplied id. Server generates a v4 UUID if omitted.                                                                                                                |
| `preset`    | string | no       | Quality preset (WS-4). One of `"480p"` / `"720p"` / `"1080p"` — case-insensitive. Omit or send `null` for the default `"720p"`. Unknown values return `400 invalid_request`. |

Preset → FFmpeg encoder triple (spec §23.4):

| `preset` | Resolution | Framerate | Video bitrate |
| -------- | ---------- | --------- | ------------- |
| `480p`   | 854 × 480  | 24 fps    | 1000 kbps     |
| `720p`   | 1280 × 720 | 30 fps    | 2500 kbps     |
| `1080p`  | 1920 × 1080| 30 fps    | 4500 kbps     |

The preset is immutable for the life of a source — switching presets
requires `POST /source/stop` followed by a fresh `POST /source`. Audio
encoding (Opus) is not preset-dependent in v1.

Response (`201`):

```json
{
  "source_id": "camera-1",
  "track": {
    "namespace": "camera-1",
    "video": "video",
    "audio": "audio"
  }
}
```

`track.namespace` is the moq-lite broadcast path; `video` / `audio` are
the track names inside that broadcast (`pipeline.rs` hardcodes them so
the WS-0 reference player subscribes without configuration).

### `POST /source/stop`

```sh
curl -sSf -XPOST http://127.0.0.1:7080/source/stop \
    -H 'content-type: application/json' \
    -d '{"source_id": "camera-1"}'
```

Returns `204 No Content` on success.

### `GET /track/{source_id}`

```sh
curl -sf http://127.0.0.1:7080/track/camera-1 | jq .
```

Same shape as `POST /source`'s response.

### Error model

All `4xx` / `5xx` responses are JSON `{ error, detail? }` with these
codes:

| HTTP | `error`                       | When                                                              |
| ---- | ----------------------------- | ----------------------------------------------------------------- |
| 400  | `ssrf_blocked`                | URL fails the shared `ts6-ssrf` validator (loopback, private, …). |
| 400  | `invalid_request`             | Missing/empty `url`, bad characters in `source_id`, …             |
| 404  | `unknown_source_id`           | `/source/stop` or `/track/{id}` for a source not in the registry. |
| 409  | `source_id_already_running`   | `POST /source` with a `source_id` that's already live.            |
| 500  | `internal`                    | Pipeline boot, broadcast registration, or track creation failed.  |

### SSRF posture

The sidecar reuses the shared `ts6-ssrf` validator (extracted under
PURA-141 from `ts6-manager-server`) for every `POST /source` URL. The
sidecar's resolver uses `tokio::net::lookup_host` (system getaddrinfo);
the manager-server uses `hickory-resolver`. Both implement the same
`Resolver` trait so the validator is identical bytes either side.

DNS-rebinding defence in v1 splits by scheme:

* **HTTPS**: TLS hostname validation binds the connection to the
  certificate, which is a stronger guarantee than IP-pinning — a DNS
  rebinder substituting a private-range answer fails on the SAN check.
* **HTTP**: a small TOCTTOU window remains between the SSRF resolve
  and FFmpeg's own DNS lookup. `ts6-ssrf` has already rejected the
  metadata-host list and any answer in a private range, so a successful
  rebinder needs a public→private answer flip inside that window. A
  follow-up will route plaintext HTTP through a Rust-side proxy that
  pins the IP while preserving the `Host:` header.

The earlier "rewrite the FFmpeg-input URL to the resolved IP literal"
approach (PURA-149) was reverted because it broke TLS SNI / `Host:`
on every virtual-hosted CDN.

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

Boot the binary with `--source-name` + (`--source <url>` *or* both
`--source-lavfi-video <spec>` and `--source-lavfi-audio <spec>`) to
start a pipeline at boot. The pipeline registers a broadcast under
`--source-name` and publishes `video` + `audio` tracks the
WS-0 reference player can subscribe to.

Examples — pipe a local file (FFmpeg transcodes to VP8/Opus):

```sh
cargo run --release -- \
    --listen '[::]:4443' \
    --http-listen '127.0.0.1:7080' \
    --tls-generate localhost \
    --source-name camera-1 \
    --source tests/fixtures/sample.mp4
```

Synthetic lavfi source (no fixture file needed):

```sh
cargo run --release -- \
    --listen '[::]:4443' \
    --http-listen '127.0.0.1:7080' \
    --tls-generate localhost \
    --source-name camera-1 \
    --source-lavfi-video 'testsrc2=size=320x240:rate=15' \
    --source-lavfi-audio 'sine=frequency=440:sample_rate=48000'
```

Boot-time CLI flags and the WS-3 [`POST /source`](#control-plane) REST
plane both create the same kind of `Pipeline`; pick whichever fits the
operator's workflow. WS-3 sources can be stopped on demand without a
restart; boot-time CLI sources only stop on process exit.

## Self-signed cert + Helium / Chromium

Same pattern as the WS-0 spike: launch the browser with
`--ignore-certificate-errors-spki-list=<base64-spki-hash>`. See
[`moq-spike/README.md`](../../moq-spike/README.md) for the full recipe.
Production cert management is a WS-7 / operator-experience concern.

## Smoke tests

`tests/smoke.rs` (WS-1) boots the sidecar lib on ephemeral ports,
asserts the JSON shape of every control-plane endpoint, registers a
broadcast through `SidecarOrigin::register_broadcast`, and re-checks
`/stats`. Always-on (no ffmpeg required):

```sh
cd crates/ts6-media-sidecar
cargo test --test smoke
```

`tests/control_plane.rs` (WS-3) boots the sidecar with a deterministic
`MockResolver`, walks
`POST /source → GET /track/{id} → GET /stats → POST /source/stop` end-to-end,
and asserts SSRF rejection for both private-range DNS rebinders and
loopback IP literals. ffmpeg-free (`ffmpeg_path = /bin/true`):

```sh
cd crates/ts6-media-sidecar
cargo test --test control_plane
```

`tests/pipeline_two_tab_smoke.rs` (WS-2) boots the sidecar, starts a
`Pipeline` against a synthetic `lavfi` source (FFmpeg subprocess
transcodes to VP8 + Opus), then connects two `moq-native` client
sessions and asserts each one receives at least one frame from both
the `video` and `audio` tracks. Gated behind the `ffmpeg-smoke` Cargo
feature so `cargo test` stays ffmpeg-free for environments that don't
have it:

```sh
cd crates/ts6-media-sidecar
cargo test --features ffmpeg-smoke --test pipeline_two_tab_smoke
```

`tests/fixtures/build.sh` regenerates the operator-side static
fixtures (`sample.mp4`, `video.ivf`, `audio.ogg`) — promoted from
`moq-spike/fixture/`. The cargo smoke does *not* depend on these files
(lavfi keeps CI self-contained); they're for manual operator smoke
against the WS-0 reference player.

## What this crate does NOT do yet

- **No on-the-fly preset switching.** WS-4 (PURA-142) wires `preset` into
  the FFmpeg encoder triple, but switching presets on a live stream is
  deferred to v1.1 — operators must `POST /source/stop` + `POST /source`.
- **No Dioxus widget** — the player is the no-build reference subscriber
  in [`moq-spike/player/`](../../moq-spike/player/) until WS-5.
- **No browser-side audio decode** — the WS-0 reference player only
  subscribes to the `video` track. The sidecar already publishes
  `audio`; wiring the player to subscribe + run WebCodecs
  `AudioDecoder` is part of WS-5.
