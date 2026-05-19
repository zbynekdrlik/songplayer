# Lyrics Eval — assemblyai-universal-3-pro rev 2 — 2026-05-19

**Run ID:** `2026-05-19T13:30:00Z`
**Backend:** `assemblyai-universal-3-pro` rev 2 (line-splitter `LINE_GAP_MS` tuned 800 → 400)
**Judge:** `claude-opus-4-7`, prompt revision 1
**Fixtures run:** 5 of 24
**Cost:** ~$0.07; cumulative AAI spend this session ~$0.14 across r1 + r2

## Headline

**5 of 5 fixtures pass wall-gate timing. Mean 7.6 / 10. Zero hallucinations.** Universal-3 Pro at r2 is the new front-runner for production champion.

| Metric | whisperx (prod) | gemini-fl | mimo-v2-omni (DROPPED) | aai-u3-pro r1 | **aai-u3-pro r2** |
|---|---:|---:|---:|---:|---:|
| Mean score | 4.4 | 5.0 | (dropped) | 6.6 | **7.6** |
| Wall-pass | 1/5 | 0/5 | 0/5 | 4/5 | **5/5** |
| Hallucination clusters | 1 | 2 | 2 | 0 | **0** |

## r1 → r2 delta (the LINE_GAP_MS tune)

| Category | r1 cov | r2 cov | Δ pp | r1 median_abs | r2 median_abs | r2 wall |
|---|---:|---:|---:|---:|---:|:---:|
| dense_vocal | 37% | **71%** | +34 | 145 ms | 200 ms | ✅ |
| reverb_heavy | 40% | **64%** | +24 | 721 ms | **386 ms** | ✅ |
| instrumental_breaks | 61% | **85%** | +24 | 188 ms | 188 ms | ✅ |
| multi_language | 70% | **92%** | +22 | 203 ms | 174 ms | ✅ |
| clean_pop | 63% | **74%** | +11 | 144 ms | 178 ms | ✅ |

Every category gained coverage. Timing held inside tolerance everywhere — reverb_heavy actually went from over-threshold to under-threshold because the tighter splitter produced more matched lines, pulling the median into the well-behaved zone.

## Per-category scores

| Category | whisperx | gemini-fl | **aai-u3-pro r2** | winner |
|---|---:|---:|---:|---|
| dense_vocal | 5 | 6 | **7** | AAI |
| reverb_heavy | 4 | 4 | **7** | AAI |
| instrumental_breaks | 3 | 5 | **8** | AAI |
| multi_language | 3 | 4 | **9** ★ | AAI |
| clean_pop | **7** | 6 | 7 | tied AAI / whisperx |

★ = single largest category swing in the project (whisperx 3 → AAI 9)

## Recommendation

**Propose AAI Universal-3 Pro for production champion promotion.**

Outstanding before promotion:

1. **Run the remaining 19 fixtures** to confirm category strength holds at N=4-5 per category. Pilot scores at N=1 each can mislead.
2. **Wall-verify on a real song** — pick one song from the manifest, run AAI lyrics through the production playback pipeline, watch it on the actual LED wall. The eval scores are diagnostic; the wall is the truth.
3. **Decide on `LINE_GAP_MS`** between 400 (current r2) and 500 ms (might collapse the YbGFYaA0SbY over-segmentation). Likely just leave at 400.
4. **Plan integration:** AAI replaces whisperx in `crates/sp-server/src/lyrics/whisperx_replicate.rs`, requires a new `crates/sp-server/src/lyrics/assemblyai_backend.rs`. Cost goes from $0.005/song (Replicate) to $0.01/song (AAI) — both trivial.

## Caveats

- N=1 per category. Need to run remaining 19 fixtures before declaring categorical dominance.
- Cost: $0.21/hr = $0.014 per 4-min song. 185 free hours per account ≈ 2700 4-min songs covered.
- The `LINE_GAP_MS` is a wrapper-side post-processing param, not an AAI-side setting. Future improvements to AAI's segmentation upstream may obsolete this knob.
- AssemblyAI is a paid third-party service. Production routing through them introduces a vendor dependency the project doesn't have today (Replicate is the existing vendor for whisperx).
