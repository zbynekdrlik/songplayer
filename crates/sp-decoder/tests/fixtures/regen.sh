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

# 160x120 testsrc2 H.264 video, exactly 3.000 seconds, no audio track.
#
# The original intent was a 32x32 all-black fixture (smaller file) but
# Media Foundation's H.264 decoder returned end-of-stream immediately on
# such a tiny all-zero source — either a minimum-size constraint inside
# the hardware transform or an optimisation that folds constant-colour
# frames away. testsrc2 at 160x120 produces a real coloured test pattern
# that MF decodes normally. The filename is kept as `black_3s.mp4` for
# history; the test fixture is no longer black, only silent and tiny.
ffmpeg -y \
  -f lavfi -i "testsrc2=s=160x120:d=3:r=30" \
  -c:v libx264 -pix_fmt yuv420p \
  -an \
  black_3s.mp4

echo "Regenerated silent_3s.flac ($(stat -c%s silent_3s.flac) bytes) and black_3s.mp4 ($(stat -c%s black_3s.mp4) bytes)"
