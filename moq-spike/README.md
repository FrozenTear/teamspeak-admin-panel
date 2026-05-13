# moq-spike — Phase 5 WS-0 two-tab MoQ smoke

Self-contained sub-tree that proves out the MoQ + WebTransport +
WebCodecs wedge for [PURA-136](/PURA/issues/PURA-136) before any
sidecar production code lands. Scope and pin decisions live in
[ADR-0007](../docs/adr/0007-moq-flavor-and-draft-pin.md).

This sub-tree is *not* a Cargo workspace member of the main crate. It
has its own `Cargo.toml` so that the spike can be torn down, replaced,
or shipped as an example without touching the main build.

## Goal

Two Helium / Chromium tabs subscribe to the same MoQ namespace and
render the same video + audio. Sub-second glass-to-glass latency.
That's the GO signal for [PURA-138](/PURA/issues/PURA-138).

## Layout

```
moq-spike/
├── README.md            # this file
├── Cargo.toml           # independent workspace for the sidecar
├── sidecar/             # Rust publisher (this spike's deliverable)
│   ├── Cargo.toml
│   └── src/main.rs
├── player/              # no-build HTML+JS reference subscriber
│   ├── index.html
│   └── player.js
└── fixture/             # synthetic VP8 + Opus generator
    ├── build.sh
    └── .gitignore       # ignore generated .ivf / .ogg
```

## Method

We work in two passes — **upstream-first**, then **strip to fixture**
— per the methodology in [PURA-138](/PURA/issues/PURA-138).

### Pass 1: confirm upstream demo works on this host

Before forking anything, prove the wedge by running the
[`moq-dev/moq`](https://github.com/moq-dev/moq) sample end-to-end on
this workstation. If `pass 1` fails, the right move is to escalate
NO-GO to the board, not to grind on a custom sidecar.

```sh
# In a scratch directory outside this repo:
git clone https://github.com/moq-dev/moq.git moq-upstream
cd moq-upstream
git checkout <pinned-tag>     # see ADR-0007 for crate versions

# Three terminals:
just relay                    # localhost:4443 (QUIC) + 8080 (HTTP origin)
just pub tos                  # FFmpeg → moq-pub Big Buck Bunny clip
just web                      # vite dev server on :5173
```

Then in two Helium tabs:

```
https://localhost:5173/?broadcast=tos
```

Both tabs should render the same frame within ~250 ms of each other.
Capture a screenshot to attach to [PURA-138](/PURA/issues/PURA-138).

### Pass 2: replace upstream relay+pub with our custom sidecar

Once pass 1 is green, swap upstream's `moq-pub` (FFmpeg subprocess
publisher, **out of scope for WS-0**) for our custom `sidecar/` binary
that reads a static `fixture/video.ivf` + `fixture/audio.ogg` and
publishes a single namespace with `video` + `audio` tracks.

```sh
# Generate the fixture (uses ffmpeg, ~5s):
./fixture/build.sh                # → fixture/video.ivf + fixture/audio.ogg

# Run the upstream relay (still needed at the spike stage; production
# sidecar can embed its own relay later under WS-2):
(cd ../moq-upstream && just relay)

# Run our custom sidecar against the relay:
cargo run --release -p moq-spike-sidecar -- \
    --relay https://localhost:4443 \
    --namespace pura-spike/0 \
    --video fixture/video.ivf \
    --audio fixture/audio.ogg

# Serve the no-build player on a separate http origin so the
# WebTransport "trusted origin" check passes:
python3 -m http.server -d player 8000
```

Open two Helium tabs against the no-build player and subscribe to the
same namespace:

```
http://localhost:8000/index.html?ns=pura-spike/0
```

## Self-signed certs and Helium

WebTransport requires QUIC + a trusted (or trust-pinned) certificate.
moq-native ships a self-signed cert generator. Two options at the
spike stage:

1. **Match the SPKI fingerprint** — moq-native prints the SPKI hash on
   startup; pass it to Helium:
   ```
   /opt/helium-browser-bin/helium \
       --ignore-certificate-errors-spki-list=<sha256-base64>
   ```
2. **Disable cert checks entirely** *(developer-only, never for
   external operators)*:
   ```
   /opt/helium-browser-bin/helium --ignore-certificate-errors
   ```

Production cert management is a WS-7 / operator-experience concern,
explicitly out of scope here.

## Definition of done for the smoke

- [ ] Pass 1 (upstream demo) confirmed on this host with screenshot.
- [ ] Pass 2 (custom sidecar + no-build player + fixture file) confirmed
      on this host with screenshot.
- [ ] [PURA-136](/PURA/issues/PURA-136) gets the GO/NO-GO comment.
- [ ] [ADR-0007](../docs/adr/0007-moq-flavor-and-draft-pin.md) flipped
      from **Proposed** to **Accepted** (or **Rejected** on NO-GO).

## Risks specific to the spike

- **Helium ↔ Chromium parity**: Helium is Chromium 148.0.7778.96 with
  the Brave-style privacy stripping. WebTransport / WebCodecs are core
  Chromium features; we expect parity, but if Helium has disabled or
  flagged either API, fall back to vanilla `chromium` from the system
  package manager.
- **moq-lite client config schema drift**: `moq-lite 0.16` is the pin.
  If `cargo` resolves to a newer minor that changes the public API,
  the sidecar build fails noisily — that's by design, see
  [ADR-0007](../docs/adr/0007-moq-flavor-and-draft-pin.md) "Planned-upgrade
  cadence".
- **Loopback QUIC weirdness on Linux**: passt-style sandboxing has
  bitten us before on TS6 (see `docs/ts6-fixture.md`). We are on host
  network for the spike to side-step it. Document if it bites again.
