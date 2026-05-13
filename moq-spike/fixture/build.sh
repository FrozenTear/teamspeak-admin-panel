#!/usr/bin/env bash
# Generate the WS-0 spike fixture: 30s of synthetic VP8 video + Opus audio.
#
# Output (gitignored):
#   moq-spike/fixture/video.ivf   — VP8 in IVF container, 30 fps, 1280×720
#   moq-spike/fixture/audio.ogg   — Opus in Ogg container, 48kHz mono
#
# Why synthetic: no external download, no licensing question, fully
# reproducible. Swap for Big Buck Bunny etc. once WS-2 lands an FFmpeg
# subprocess source.

set -euo pipefail

cd "$(dirname "$0")"

VIDEO=video.ivf
AUDIO=audio.ogg
DURATION=30
WIDTH=1280
HEIGHT=720
FPS=30

if ! command -v ffmpeg >/dev/null 2>&1; then
  echo "ffmpeg is required on PATH" >&2
  exit 1
fi

echo "generating $VIDEO ..."
ffmpeg -y -hide_banner -loglevel warning \
  -f lavfi -i "testsrc2=size=${WIDTH}x${HEIGHT}:rate=${FPS}:duration=${DURATION}" \
  -c:v libvpx \
  -b:v 1500k \
  -deadline realtime \
  -cpu-used 5 \
  -g $((FPS * 1)) \
  -pix_fmt yuv420p \
  -f ivf \
  "$VIDEO"

echo "generating $AUDIO ..."
ffmpeg -y -hide_banner -loglevel warning \
  -f lavfi -i "sine=frequency=440:duration=${DURATION}:sample_rate=48000" \
  -c:a libopus \
  -b:a 64k \
  -ac 1 \
  -ar 48000 \
  -application voip \
  -f ogg \
  "$AUDIO"

echo "done:"
ls -lh "$VIDEO" "$AUDIO"
