#!/usr/bin/env bash
set -euo pipefail
# run_review.sh — the #349 November-2026 review, every GPU stage on dev1, in order.
#
# Stages (each writes <eval-dir>/<stage>.rc with its exit code, so a resumed
# session knows exactly what finished):
#   mtl        the production aligner (LyricsAlignment-MTL, MTL+BDR) on the
#              dev1-rebuilt WAVs with the 2026-08-05 reference text = the
#              input-parity anchor (backend id lyrics-alignment-mtl-dev1)
#   w2v        w2v-bert-2.0-music-lyrics-ctc on the dereverbed vocal
#   w2v-mix    the same model on the full mix (the input its card trains on)
#   sylla      sylla-600m-v1 on the dereverbed vocal
#   heart      HeartTranscriptor-oss (Whisper word timestamps) on the vocal
#
# Usage: run_review.sh <eval-dir> [stage ...]
#   <eval-dir> holds venv/, wav/, LyricsAlignment-MTL/ (prep_fixtures.py output).

E="$1"
shift
W="$(cd "$(dirname "$0")/../../.." && pwd)"
RAW="$W/eval/lyrics/reports/2026-11-21-raw"
REF="$W/eval/lyrics/reports/2026-08-05-aligner-raw"
PY="$E/venv/bin/python"
LOGS="$E/logs"
mkdir -p "$RAW" "$LOGS"
cd "$W"

stage_mtl() {
  local rc=0 ref vid wav out
  for ref in "$REF"/lyrics-alignment-mtl_*.json; do
    vid="${ref##*lyrics-alignment-mtl_}"
    vid="${vid%.json}"
    [ "$vid" = "Xvm4_fWkXe8" ] && continue
    wav="$E/wav/${vid}_vocal16k.wav"
    out="$RAW/lyrics-alignment-mtl-dev1_${vid}.json"
    [ -f "$out" ] && continue
    if [ ! -f "$wav" ]; then
      echo "$vid: no wav"
      continue
    fi
    if "$PY" eval/lyrics/aligners/lyrics_alignment_mtl/run.py --wav "$wav" \
      --text-json "$ref" --out "$out.tmp" --repo-dir "$E/LyricsAlignment-MTL" \
      >"$LOGS/mtl_${vid}.log" 2>&1; then
      # the scorer keys on the file name; the id inside names the dev1 run too
      "$PY" -c 'import json,sys; d=json.load(open(sys.argv[1])); d["backend_id"]="lyrics-alignment-mtl-dev1"; open(sys.argv[2],"w").write(json.dumps(d, ensure_ascii=False))' "$out.tmp" "$out"
      rm -f "$out.tmp"
      echo "$vid: ok"
    else
      rc=1
      echo "$vid: FAILED (see $LOGS/mtl_${vid}.log)"
    fi
  done
  return "$rc"
}

run_stage() {
  local name="$1" rc=0
  echo "=== $name $(date -Is)"
  case "$name" in
    mtl) stage_mtl || rc=$? ;;
    w2v) "$PY" eval/lyrics/review_2026_11/ctc_lyrics.py --model w2v-bert-lyrics \
      --wav-dir "$E/wav" --out-dir "$RAW" || rc=$? ;;
    w2v-mix) "$PY" eval/lyrics/review_2026_11/ctc_lyrics.py --model w2v-bert-lyrics \
      --input mix --wav-dir "$E/wav" --out-dir "$RAW" || rc=$? ;;
    sylla) "$PY" eval/lyrics/review_2026_11/ctc_lyrics.py --model sylla \
      --wav-dir "$E/wav" --out-dir "$RAW" || rc=$? ;;
    heart) "$PY" -m eval.lyrics.review_2026_11.heart_asr --batch-size 1 \
      --wav-dir "$E/wav" --out-dir "$RAW" || rc=$? ;;
    *)
      echo "unknown stage $name"
      rc=2
      ;;
  esac
  echo "$rc" >"$E/$name.rc"
  echo "=== $name rc=$rc $(date -Is)"
}

stages=("$@")
[ "${#stages[@]}" -eq 0 ] && stages=(mtl w2v w2v-mix sylla heart)
for s in "${stages[@]}"; do
  run_stage "$s"
done
