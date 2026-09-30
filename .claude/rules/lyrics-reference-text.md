---
paths:
  - "crates/sp-server/src/lyrics/reference_gate*.rs"
  - "crates/sp-server/src/lyrics/sung_coverage*.rs"
  - "crates/sp-server/src/lyrics/title_search*.rs"
  - "crates/sp-server/src/lyrics/lrclib_search*.rs"
  - "crates/sp-server/src/lyrics/text_candidate*.rs"
  - "crates/sp-server/src/lyrics/transcript_cache*.rs"
  - "crates/sp-server/src/lyrics/worker_text_tiers*.rs"
  - "crates/sp-server/src/lyrics/worker_reference*.rs"
  - "crates/sp-server/src/lyrics/worker_g35t.rs"
  - "crates/sp-server/src/lyrics/orchestrator.rs"
  - "crates/sp-server/src/lyrics/audit_ctx.rs"
  - "crates/sp-server/src/lyrics/gather.rs"
  - "crates/sp-server/src/lyrics/youtube_subs.rs"
  - "crates/sp-server/src/lyrics/genius*.rs"
  - "crates/sp-server/src/lyrics/description_provider*.rs"
  - "crates/sp-server/src/lyrics/reprocess*.rs"
  - "crates/sp-server/src/api/lyrics*.rs"
  - "crates/sp-server/src/lyrics/worker.rs"
  - "crates/sp-server/src/playback/lyrics_loader.rs"
---

# Lyrics reference text: the two-way gate, the title search, one transcript (#144)

## The gate is two-way

- `reference_gate::evaluate` checks BOTH directions:
  - reference → transcript: `match_lines`, `matched_frac` ≥ 0.60, median offset ≤ 400 ms, agreement ≥ 0.70;
  - transcript → reference: `sung_coverage.rs`, an order-preserving LCS of ALL words.
- A text must cover ≥ 0.55 of the sung words (`MIN_SUNG_COVERED_FRAC`) and leave no uncovered sung run over 25 s (`MAX_UNCOVERED_SUNG_MS`). Otherwise the result is `Fail{Coverage}`, and the audit's `sung_coverage_ok` tells the two coverage failures apart.
- The thresholds are MEASURED (#144 comment 5899043518):
  - complete texts: 0.64–0.977 coverage (0.64–0.97 against g35t transcripts), runs ≤ 20.6 s;
  - ★ rows whose wall held one line over other singing: runs ≥ 34.9 s.
- The LCS is used, not the line-anchor walk. A 1-word fallback anchor that jumps forward orphans every line in between, and gave a complete text a false 32 s run.
- To re-measure, use the gate's own transcripts: `{yt}_g35t_words.json` (kept) and `{yt}_g35t_words_used.json` (retired) in the box cache. The v20 WhisperX `{yt}_whisperx_track.json` files are a fallback. `eval/lyrics/reports/2026-09-12-raw/` has real g35t transcripts plus the eval gold texts.
- On the box, run a read-only Python script: write it to `%TEMP%` with the win-resolume MCP `FileWrite`, open the DB `file:...songplayer.db?mode=ro`, print only a compact table, then delete the script and its `__pycache__`.

## One transcript per song, one pass

- `worker_text_tiers::run_text_tiers` transcribes the isolated vocal ONCE, right after isolation (`transcribe_vocal`). The title search, the gate and the base tier all use that one transcript. The reference stage never transcribes.
- `transcript_cache.rs` keeps the transcript for the pass's no-penalty deferral re-picks (wall gate, memory floor, startup grace, mtl wall-abort):
  - it is reused only for the same vocal (same length and mtime), within 6 h, and never when empty;
  - when the pass ends, it is retired to `_used.json`, so a manual reprocess transcribes afresh. `run_text_tiers` does it in ONE place, after the tiers, for every outcome `ends_the_pass` accepts: a ★ or base-tier track, or a quarantine (tested). `a_pass_ending_in_a_track_retires_its_transcript` drives `run_text_tiers` offline to a base-tier track to pin the call itself.
- `run_mtl_reference_stage` removes an earlier pass's `{yt}_alignment_audit.json` first; every PASS / FAIL / ERROR writes a new one, carrying `sung_*` and `sung_coverage_ok`.

## The title search (covers)

- A cover's metadata names the COVER artist, so the artist+title lookups miss the original's text.
- `should_title_search` runs the search only when all hold:
  - the transcript is non-empty;
  - the mtl tooling is present;
  - no `lrclib` / `genius` / `override` / `tier1:spotify` candidate exists.
- It searches LRCLIB `/api/search?track_name=` (records within ±15 s of the song, synced lyrics preferred) and Genius by the title alone (the first 3 song pages).
- Candidates are scored by multiset Dice against the transcript. The floor is 0.50: a song's own lyric scores 0.664–0.951, another song ≤ 0.379.
- The best usable lyric becomes the reference text unless the video's own priority pick (`best_authoritative_candidate`) scores at least as high (`keeps_the_videos_text`).
- A cover more than ±15 s off the original's length is reached only through Genius. Example: 158 is 484 s, and Elevation's LRCLIB records are 539 s.
- The record is `{yt}_title_search_audit.json`: every candidate with its score, `chosen`, `videos_text`, and `reference.from`.

## Texts

- `parse_json3` gives ONE line per sung caption line. Each line keeps its event's own span, never divided by hand. No production code reads `CandidateText::line_timings`: mtl re-times the text.
- The scraped-lyrics Claude cleanup (`CleanupMode::ScrapedLyrics`) KEEPS every repeat, because mtl times exactly the lines it is given. When its prompt semantics change, bump the cleanup cache names (`_cleaned_v3.json` today), or the reprocess reuses stale decisions.
- Turn a fetched timed track or a scraped plain lyric into a candidate only through `text_candidate::{timed_candidate, cleaned_text_candidate}`. `gather.rs` and the title search share them, and `gather_uses_lyrics_ovh_primary_with_genius_fallback` reads both files. The override, description, lyrics.ovh and Spotify candidates are built directly.

## Reprocess: ONE per-song path, and no route blanks what the wall serves

- The one per-song reprocess path is `POST /api/v1/lyrics/reprocess` with `{"video_ids":[…]}` or `{"playlist_id":N}` (`api/lyrics.rs::post_reprocess`, the dashboard's Reprocess). It:
  - sets `lyrics_manual_priority = 1` (bucket 0);
  - resets `lyrics_attempts` / `lyrics_next_attempt_at`;
  - NULLs `lyrics_source` only for the no-lyrics sentinels (`failed`, `empty`, `no_source`, `unsupported_source`).
- It never touches `has_lyrics` or the `<yt>_lyrics.json`. The wall keeps the old lyrics while the song waits in the queue, and a successful run replaces them.
- The bulk sweeps use the same manual-priority queue and never reset `has_lyrics` either:
  - `POST /api/v1/lyrics/reprocess-all-stale` (the dashboard's "Spracovať všetky zastarané");
  - `POST /api/v1/lyrics/reprocess-catalog-with-new-gate` (`api/lyrics_catalog.rs`).
- **No reprocess route may blank served lyrics.** `playback/lyrics_loader.rs` and `GET /api/v1/videos/{id}/lyrics` serve nothing for `has_lyrics = 0`, even with the file on disk.
- The deleted per-video `POST /api/v1/videos/{id}/lyrics/reprocess` (`reset_video_lyrics`: `has_lyrics = 0, lyrics_source = NULL`) did exactly that. Queueing the #144 rollout through it blanked 211 songs on the wall for ~6 h on 30.9.2026 (comment 5905405307).
- Never add a second per-song reprocess route, and never re-queue a row by resetting it.
- Known gap, NOT fixed by that deletion: a re-run that fails still blanks a served song, on two paths.
  - **An error.** `lyrics/worker.rs`'s `Err` arm calls `mark_video_lyrics(false, Some("no_source"), …)` for a row that had lyrics too, and the manual and null buckets skip `no_source` at the current version. A failed Claude cleanup in `gather.rs` is one such error.
  - **An empty transcript.** The base tier quarantines the song (`worker_g35t.rs` → `quarantine_video_lyrics`: `has_lyrics = 0`, `asr_gap`) and DELETES `<yt>_lyrics.json`.
  - Either way the song stays dark until a manual reprocess or a version bump re-queues it AND a later run succeeds.
  - Both are recorded on #144 (comment 5905749132 and its follow-up) as a follow-up candidate: defer, or keep the served lyrics.
- Pinned by `api/lyrics_tests.rs`:
  - `a_per_video_reprocess_request_never_blanks_the_served_lyrics`;
  - `the_one_reprocess_path_keeps_the_served_lyrics_and_sets_manual_priority`.
- A bulk re-queue on the box, such as the post-deploy set of a lane, goes through `POST /api/v1/lyrics/reprocess` with `video_ids`.

## Mutation-safe loops (a hang fails the gate)

The LCS walk advances with `(i..n).find(..)` inside `for j in ..`, never with a hand-incremented `while` cursor. An `i += 1` → `*=` mutant then gives a wrong result instead of a TIMEOUT. The walk's drop test `suffix[at(r, j)] > suffix[at(r + 1, j)]` has no equivalent mutant; the `< suffix[at(r, j + 1)]` form had one.
