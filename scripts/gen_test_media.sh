#!/usr/bin/env bash
# Generate a local test clip (video + 440Hz tone) for the playback POC.
# Requires ffmpeg. Output: /tmp/anime-tui-poc-test.mp4
set -euo pipefail
OUT="${1:-/tmp/anime-tui-poc-test.mp4}"
ffmpeg -y \
  -f lavfi -i "testsrc2=size=640x360:rate=25" \
  -f lavfi -i "sine=frequency=440:sample_rate=48000" \
  -t 30 -pix_fmt yuv420p -c:v libx264 -preset veryfast -c:a aac \
  "$OUT"
echo "wrote $OUT"
