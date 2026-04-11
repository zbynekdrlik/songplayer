#!/usr/bin/env bash
# Regenerate committed test fixtures for sp-decoder.
#
# These fixtures are used by integration tests in crates/sp-decoder/tests/.
# They are committed as binary blobs (~3 KB each) so CI does not need FFmpeg
# to run the tests. Run this script after any intentional change to the
# fixture shape.
set -euo pipefail

cd "$(dirname "$0")"

# Silent stereo 48 kHz FLAC, exactly 3.000 seconds.
# FLAC compresses pure silence extremely well — ~3 KB.
ffmpeg -y \
  -f lavfi -i "anullsrc=r=48000:cl=stereo" \
  -t 3 \
  -c:a flac -compression_level 5 \
  silent_3s.flac

# 32x32 black H.264 video, exactly 3.000 seconds, no audio track.
# yuv420p and x264 baseline keep the file tiny (~3 KB) and ensure Media
# Foundation on Windows can open it without any codec pack.
ffmpeg -y \
  -f lavfi -i "color=c=black:s=32x32:d=3:r=30" \
  -c:v libx264 -profile:v baseline -pix_fmt yuv420p \
  -an \
  black_3s.mp4

echo "Regenerated silent_3s.flac ($(stat -c%s silent_3s.flac) bytes) and black_3s.mp4 ($(stat -c%s black_3s.mp4) bytes)"
