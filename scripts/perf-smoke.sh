#!/usr/bin/env bash
#
# PURA-162 WS-Perf — driver for the music-bot pipeline perf smoke.
#
# Usage:
#   scripts/perf-smoke.sh quick       # 60 s synthetic-tone smoke (CI gate)
#   scripts/perf-smoke.sh sustained   # 1800 s synthetic-tone sustained-load
#   scripts/perf-smoke.sh ffmpeg <input>   # 60 s with real ffmpeg source
#   scripts/perf-smoke.sh icy <url>   # THE-972: 60 s real ICY radio path +
#                                     # 5 first-frame probes (needs network)
#
# Builds the release binary the first time, then runs it. Writes a JSON
# report to qa-evidence/perf-smoke/<utc>.json and stamps PERF_SMOKE_GIT_SHA
# into the report so regressions can be diffed against a known revision.
#
# Exit code mirrors the binary: 0 = all budgets pass, 1 = at least one
# budget failed. WS-Gate wires this directly into the release-gate check.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

MODE="${1:-quick}"
shift || true

mkdir -p qa-evidence/perf-smoke
TS="$(date -u +%Y%m%dT%H%M%SZ)"
OUT="qa-evidence/perf-smoke/${MODE}-${TS}.json"

# Resolve the real release dir. Paperclip / CI may redirect cargo's target
# directory via `.cargo/config.toml` or `CARGO_TARGET_DIR`; trust cargo's
# own answer instead of assuming `target/`.
TARGET_DIR="${CARGO_TARGET_DIR:-$(cargo metadata --no-deps --format-version 1 2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])' 2>/dev/null || echo target)}"
BIN="$TARGET_DIR/release/perf-smoke"

if ! [ -x "$BIN" ]; then
    echo "perf-smoke binary missing at $BIN — building release …" >&2
    cargo build --release -p music-bot-audio --bin perf-smoke >&2
fi

GIT_SHA="$(git rev-parse --short=12 HEAD 2>/dev/null || echo unknown)"
export PERF_SMOKE_GIT_SHA="$GIT_SHA"

case "$MODE" in
    quick)
        ARGS=(
            --duration-seconds 60
            --source synthetic
            --output "$OUT"
        )
        ;;
    sustained)
        ARGS=(
            --duration-seconds 1800
            --source synthetic
            --output "$OUT"
        )
        ;;
    ffmpeg)
        INPUT="${1:?need ffmpeg input path/URL as second arg}"
        ARGS=(
            --duration-seconds 60
            --source ffmpeg
            --ffmpeg-input "$INPUT"
            --output "$OUT"
        )
        ;;
    icy)
        # THE-972 — the real `!radio` path against a live station. First-frame
        # latency folds in network connect + ffmpeg probe, so it is measured
        # over 5 fresh probe connections (p50 in the report) before the
        # steady-state pacing run.
        URL="${1:?need icy stream URL as second arg}"
        ARGS=(
            --duration-seconds 60
            --source icy
            --icy-url "$URL"
            --first-frame-probes 5
            # The bot's runtime buffering (crates/voice/src/audio.rs) — the
            # steady-state drift then measures what the wire would see, not
            # the station's chunk cadence. Probes ignore the prebuffer.
            --frame-buffer 250
            --prebuffer-frames 150
            --output "$OUT"
        )
        ;;
    *)
        echo "unknown mode: $MODE" >&2
        echo "supported: quick | sustained | ffmpeg <input>" >&2
        exit 2
        ;;
esac

echo "perf-smoke: mode=$MODE git_sha=$GIT_SHA bin=$BIN → $OUT" >&2
set +e
"$BIN" "${ARGS[@]}"
status=$?
set -e

echo >&2
echo "perf-smoke: report → $OUT (status=$status)" >&2
exit $status
