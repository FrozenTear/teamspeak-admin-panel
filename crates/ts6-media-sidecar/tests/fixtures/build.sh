#!/usr/bin/env bash
# Generate the WS-2 sidecar test fixtures. Promoted from the WS-0 spike
# (moq-spike/fixture/build.sh) — the same synthetic source so the player
# regression smoke and the cargo integration smoke share input semantics.
#
# Outputs (gitignored — see ./.gitignore):
#   sample.mp4    — 10s synthetic 320x240 @ 15 fps + 48 kHz mono sine,
#                   muxed into mp4 (H.264 + AAC). The WS-2 pipeline runs
#                   FFmpeg over this to transcode to VP8 + Opus.
#   video.ivf     — legacy VP8/IVF artefact for the WS-0 reference flow.
#   audio.ogg     — legacy Opus/Ogg artefact for the WS-0 reference flow.
#
# The cargo integration test (`pipeline_two_tab_smoke`) does NOT depend
# on this script — it drives FFmpeg from a `lavfi` source directly so CI
# has no fixture-prebuild step. This script is for operator-side manual
# smoke testing.

set -euo pipefail
cd "$(dirname "$0")"

DURATION=10
WIDTH=320
HEIGHT=240
FPS=15

if ! command -v ffmpeg >/dev/null 2>&1; then
  echo "ffmpeg is required on PATH" >&2
  exit 1
fi

echo "generating sample.mp4 ..."
ffmpeg -y -hide_banner -loglevel warning \
  -f lavfi -i "testsrc2=size=${WIDTH}x${HEIGHT}:rate=${FPS}:duration=${DURATION}" \
  -f lavfi -i "sine=frequency=440:duration=${DURATION}:sample_rate=48000" \
  -c:v libx264 -preset ultrafast -pix_fmt yuv420p \
  -c:a aac -b:a 64k -ac 1 \
  -shortest \
  sample.mp4

echo "generating video.ivf ..."
ffmpeg -y -hide_banner -loglevel warning \
  -i sample.mp4 \
  -an \
  -c:v libvpx -b:v 800k -deadline realtime -cpu-used 5 \
  -g $((FPS * 1)) -keyint_min $((FPS * 1)) -pix_fmt yuv420p \
  -f ivf video.ivf

echo "generating audio.ogg ..."
ffmpeg -y -hide_banner -loglevel warning \
  -i sample.mp4 \
  -vn \
  -c:a libopus -b:a 64k -ac 1 -ar 48000 -application voip \
  -f ogg audio.ogg

echo "done:"
ls -lh sample.mp4 video.ivf audio.ogg
