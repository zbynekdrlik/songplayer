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
  - "crates/sp-server/src/lyrics/g35t_client*.rs"
  - "crates/sp-server/src/lyrics/g35t_probe*.rs"
  - "e2e/post-deploy-g35t.spec.ts"
  - "e2e/g35t-gate*.ts"
  - "crates/sp-server/src/lyrics/orchestrator.rs"
  - "crates/sp-server/src/lyrics/audit_ctx.rs"
  - "crates/sp-server/src/lyrics/gather.rs"
  - "crates/sp-server/src/lyrics/youtube_subs.rs"
  - "crates/sp-server/src/lyrics/genius*.rs"
  - "crates/sp-server/src/lyrics/description_provider*.rs"
  - "crates/sp-server/src/lyrics/reprocess*.rs"
  - "crates/sp-server/src/api/lyrics*.rs"
  - "crates/sp-server/src/lyrics/worker.rs"
  - "crates/sp-server/src/lyrics/worker_outcome*.rs"
  - "crates/sp-server/src/playback/lyrics_loader.rs"
---

# Lyrics reference text: the two-way gate, the title search, one transcript (#144)

## The gate is two-way

- `reference_gate::evaluate` checks BOTH directions on ONE alignment (`sung_coverage::align`: the order-preserving LCS of ALL reference words against ALL sung words, computed once):
  - reference → transcript: `match_lines` — a line is matched when at least half of its words are on the alignment (`line_matched`: `aligned * 2 >= words`), at the sung start of its first aligned word; `matched_frac` ≥ 0.60, median offset ≤ 400 ms, agreement ≥ 0.70;
  - transcript → reference: the same alignment's coverage of the sung words.
- The first-week review (8.10.2026) replaced the line-anchor walk (first 3 → 2 → 1 words after a forward-only cursor): a misheard first word sent the 1-word fallback to a later occurrence and every line in between was lost — the catalog's +102 / +313 / +444 s medians were that jump. On the 21 in-repo eval fixtures (mtl 2026-08-05 vs g35t 2026-09-12) 4 → 18 pass; the 3 that fail are the poisoned and two partial texts (`reference_gate_tests_lcs.rs` pins three of them exactly). A matcher change is re-checked the same way: a scratch Python port of `evaluate` over those fixtures, both matchers side by side.
- A text must cover ≥ 0.55 of the sung words (`MIN_SUNG_COVERED_FRAC`) and leave no uncovered sung run over 25 s (`MAX_UNCOVERED_SUNG_MS`). Otherwise the result is `Fail{Coverage}`, and the audit's `sung_coverage_ok` tells the two coverage failures apart.
- The thresholds are MEASURED (#144 comment 5899043518):
  - complete texts: 0.64–0.977 coverage (0.64–0.97 against g35t transcripts), runs ≤ 20.6 s;
  - ★ rows whose wall held one line over other singing: runs ≥ 34.9 s.
- To re-measure, use the gate's own transcripts: `{yt}_g35t_words.json` (kept) and `{yt}_g35t_words_used.json` (retired) in the box cache. The v20 WhisperX `{yt}_whisperx_track.json` files are a fallback. `eval/lyrics/reports/2026-09-12-raw/` has real g35t transcripts plus the eval gold texts.
- On the box, run a read-only Python script: write it to `%TEMP%` with the win-resolume MCP `FileWrite`, open the DB `file:...songplayer.db?mode=ro`, print only a compact table, then delete the script and its `__pycache__`.

## One transcript per song, one pass

- `worker_text_tiers::run_text_tiers` transcribes the isolated vocal ONCE, right after isolation (`transcribe_vocal`). The title search, the gate and the base tier all use that one transcript. The reference stage never transcribes.
- `transcript_cache.rs` keeps the transcript for the pass's no-penalty deferral re-picks (wall gate, memory floor, startup grace, mtl wall-abort):
  - it is reused only for the same vocal (same length and mtime), within 6 h, and never when empty;
  - when the pass ends, it is retired to `_used.json`, so a manual reprocess transcribes afresh. `run_text_tiers` does it in ONE place, after the tiers, for every outcome `ends_the_pass` accepts: a ★ or base-tier track, or a quarantine (tested). `a_pass_ending_in_a_track_retires_its_transcript` drives `run_text_tiers` offline to a base-tier track to pin the call itself.
- `run_mtl_reference_stage` removes an earlier pass's `{yt}_alignment_audit.json` first; every PASS / FAIL / ERROR writes a new one, carrying `sung_*` and `sung_coverage_ok`.
- The transcript is requested with `language_codes ["en-US", "es-419"]` (`g35t_client::LANGUAGE_CODES`, 5.10.2026; design record #144 comment 5995867005). Until then every song had an English-only hint, so a Spanish song's transcript, and with it its gate verdict and base-tier text, came from an English reading. A Spanish song processed before that change should be re-run with the targeted reprocess before its gate numbers are trusted.

## The live g35t gate (5.10.2026)

Before it, no post-deploy check sent a Gemini 3.5 Transcribe request: a dead or refused key, a renamed model or a request field the API refuses (the `language_codes` hint above) stayed invisible with CI green while every new transcript failed. Design: #144 comments 5996797762 (main) and 5996959706 (the clip).

- `POST /api/v1/lyrics/g35t/probe` (`api/lyrics_g35t.rs` → `lyrics/g35t_probe.rs::run_probe`) sends ONE short real request from the box through `g35t_client::transcribe_at`: the worker's own upload, poll, `interactions_body` (`MODEL_SLUG`, `LANGUAGE_CODES`) and key rotation. `transcribe_words` is `transcribe_at` on `GEMINI_API_ROOT`. Never give a probe its own request body or its own copy of the hint.
- The clip: the lowest `videos.id` with `normalized = 1`, `has_lyrics = 1`, its audio on disk and a `{yt}_lyrics.json` with a line. The input is the isolated vocal the worker itself uploads (`aligner::isolated_vocal_path`, `{yt}_vocals16k.wav`: the one place the path is built; the startup cache scan's `VOCALS_RE` matches the same name) when on disk, else its vocal stem (`stems::stem_paths`), else the audio (`clip.source` = `isolated_vocal` / `vocal_stem` / `mix`). The window is 20 s (`CLIP_MS`) from the EARLIEST served line: an intro would answer 0 words, a false red. The app's ffmpeg (`tool_paths.ffmpeg`) cuts it to a 16 kHz mono float WAV (`pcm_f32le`, the format `scripts/lyrics_worker.py` writes the isolated vocal in) in a temp dir. The pick and the cut hold `cache::SONG_FILES` (a rename holds it from its read to its record), and the cut is bounded by `CUT_TIMEOUT` (15 s; ffmpeg is killed on drop).
- The answer is `{ok, model, key_index, language_codes, word_count, latency_ms, error, refused_keys, clip, sample}`, always with 200. `ok` needs at least one word.
  - `key_index` is 0-based (the worker's log field); the error text names a key as `key i of n`, 1-based.
  - Never a key: `transcribe_at` redacts every failure text with every key (`gemini_api::redact_keys`), and `send_with_retry` keeps a refused body as its `gemini_api::body_excerpt` (one line, redacted with its request's key, THEN cut to 400 characters; the metadata provider cuts with the same helper at 200).
  - `refused_keys` lists every key refused before the one that decided the outcome, each with `rate_limited` (a 429, out of quota now) or not (a 403 / a 400 naming the key: dead, invalid, or not allowed this model or API; the reason says which, so read it before pruning a key), so a refused key shows even while a later key answers; the worker logs the same at WARN and the spec logs each one 1-based. The gate FAILS on any `refused_keys` entry with `rate_limited: false` (a dead or invalid key), even when a later key answered, and passes a 429 (a quota state, not a dead key): ROZHODNUTÉ #144 comment 5999711400 (the live probe of 5.10.2026 answered key 0 with `refused_keys []`, the invalid entry the list carried since 12.9.2026 is gone). Keys after the answering one are never tried (a paid call each), so a dead key AFTER the answering one stays unseen until an earlier key stops answering. Pinned by `e2e/g35t-gate.spec.ts`.
  - A refused body is redacted before the cut with its REQUEST's key only (Google echoes only that one); the whole text is redacted again with every key after the cut.
  - A call cut by `PROBE_TIMEOUT` answers `key_index: null` and no `refused_keys`.
  - The key list is read from the setting on every call, as the worker reads it per song.
  - `PROBE_TIMEOUT` (180 s) bounds it below the spec's 220 s, so a hung call fails the gate with its own error.
- `e2e/post-deploy-g35t.spec.ts` gates the deploy through the pure `e2e/g35t-gate.ts` (`g35tGateFailures`: not ok, 0 words, another model, another hint, a key refused for any reason but a 429), unit-tested by the mock-suite `e2e/g35t-gate.spec.ts`. A change of the model or the hint updates `G35T_MODEL` / `G35T_LANGUAGE_CODES` in the same PR.
- `g35t_probe_tests.rs` drives `transcribe_at` against wiremock (`api_root` = the mock):
  - a 429 key and a dead key move on (`key_index` 2), each in `refused_keys` with its `rate_limited`;
  - every key refused → `no key answered (2 tried); key 2 of 2: …` with the API's message, every refusal in `refused_keys`, an echoed key redacted;
  - a key echoed across the 400-character cut leaves no prefix (`a_key_echoed_at_the_cut_never_leaks_a_prefix`);
  - a 400 on the request body stops at once: the spare key is never tried, the uploaded file is still deleted;
  - an answer with no words fails, and so does the bound.
- The HTTP helpers of `g35t_client.rs` stay `mutants::skip`, and so do the probe's ffmpeg shell-out `g35t_probe::cut_clip` (its arguments are the tested `clip_args`) and its route glue `api/lyrics_g35t.rs::probe` (Google's root; the route is tested through the real router). `transcribe_at`, `on_key`, `failure` and every other probe function are gated.
- `e2e/post-deploy-g35t.spec.ts` first polls `GET /api/v1/status` until `tools.ffmpeg_available` (a read that throws counts as "not yet"), so it does not rely on earlier specs for ffmpeg readiness. The status answers while the startup follow-ups (the yt-dlp self-update, the sample-rate sweep) run: they start only after `tools_ready::publish_then` released the `tools_status` lock (`server-startup.md`).

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
- EVERY path that sets `lyrics_manual_priority = 1` also sets `lyrics_attempts = 0, lyrics_next_attempt_at = NULL` (#144 review round 2): the routes above and the "Nesedí" feedback (`db/models_reference.rs::record_reference_feedback`). The served-failure cap below counts from that 0, and a queued song never waits out an older backoff. A new queue path does the same. Pinned by `lyrics_catalog.rs::endpoint_router_queues_each_row_with_a_fresh_attempt_budget` and `models_reference_tests_mutants.rs::reference_feedback_queues_the_song_with_a_fresh_attempt_budget`.
- **No reprocess route may blank served lyrics.** `playback/lyrics_loader.rs` and `GET /api/v1/videos/{id}/lyrics` serve nothing for `has_lyrics = 0`, even with the file on disk.
- The deleted per-video `POST /api/v1/videos/{id}/lyrics/reprocess` (`reset_video_lyrics`: `has_lyrics = 0, lyrics_source = NULL`) did exactly that. Queueing the #144 rollout through it blanked 211 songs on the wall for ~6 h on 30.9.2026 (comment 5905405307).
- Never add a second per-song reprocess route, and never re-queue a row by resetting it.
- Pinned by `api/lyrics_tests.rs`:
  - `a_per_video_reprocess_request_never_blanks_the_served_lyrics`;
  - `the_one_reprocess_path_keeps_the_served_lyrics_and_sets_manual_priority`.
- A bulk re-queue on the box, such as the post-deploy set of a lane, goes through `POST /api/v1/lyrics/reprocess` with `video_ids`.
- **This targeted reprocess is how an output change reaches the catalog without a pipeline bump.** A `LYRICS_PIPELINE_VERSION` bump needs the owner's explicit approval (the `lyrics-pipeline` skill). The #144 rollout was exactly that: 211 + 42 songs queued through `{video_ids}` on 30.9.2026, with no bump.

## A failed or empty re-run never darkens a served song (release 0.69.0 blockers, ROZHODNUTÉ 5905945274, refined by 5908227646)

- A row **serves lyrics** when `has_lyrics = 1` AND its `<yt>_lyrics.json` exists (`worker_outcome.rs::serves_lyrics`, what `playback/lyrics_loader.rs` loads).
- The worker's two failure exits ask it first (`keep_served_lyrics`):
  - **an error** — `fail_song`, the `Err` arm of `process_next` (e.g. a failed Claude cleanup in `gather.rs`);
  - **an empty transcript** — `quarantine_empty_transcript`, the g35t base tier.
- A served row records ONLY the attempt (`db::models::record_served_lyrics_failure`):
  - `lyrics_attempts` + the `lyrics_next_attempt_at` backoff, through the existing `record_lyrics_deferral` and `downloader::retry_backoff` (shared with `defer_song` via `next_backoff`). The manual bucket honours that backoff, so a queued song does not loop;
  - **its `lyrics_manual_priority` stays** (ROZHODNUTÉ 5908227646, lane A): a transient Claude / Gemini error must not drop the song out of the manual rollout. Only the `SERVED_RERUN_MAX_ATTEMPTS`th (3rd) failed attempt since the song was queued clears it (`CASE WHEN lyrics_attempts >= 3`; queueing resets `lyrics_attempts` to 0, and a deferral counts as an attempt too).
- It keeps `has_lyrics`, `lyrics_source` and the file until a successful run replaces them. `record_served_lyrics_failure` returns a `ServedFailure {attempts, was_manual, left_manual_queue}` and the WARN keys on it (`worker_outcome.rs::log_served_failure`, review round 1): `re-run failed on its last allowed attempt — … the song leaves the manual queue` exactly once, on the clear, with the video id, the YouTube id and the reason; `… the song stays queued for its next attempt after the backoff` before it; `… only the attempt is recorded` for a row that was not in the manual queue (the stale and full-mix buckets).
- An unserved row still takes today's terminal state: `no_source` for an error, and `asr_gap` plus the file removed for an empty transcript.
- The operator's `POST /api/v1/lyrics/quarantine` is unchanged: it parks a song on purpose.
- The failed pass ends like every other: `lyrics_processed_at = now` (review round 1). The #171 full-mix upgrade bucket re-attempts a row at most once a day by it, and a served full-mix row whose upgrade failed keeps that gate (`a_failed_upgrade_of_a_served_full_mix_row_waits_a_day`).
- What a served row whose re-run failed does next (the ROZHODNUTÉ's trade-off — it shows only in the DB and the WARN, never as a dark wall):
  - **Current version, re-queued by hand** (the #144 rollout): it stays in the manual queue and is picked again once its backoff runs out (5 min, then 10 min), so a transient error (e.g. CLIProxy down during the rollout) costs a retry, not the song. After the 3rd failed attempt the manual flag is cleared and it leaves the queue; re-queue it through `POST /api/v1/lyrics/reprocess {video_ids}` once the cause is fixed.
  - **Stale version** (after a pipeline bump): the stale bucket picks it again after each backoff (5 min · 2^(n-1), cap 24 h), so a song that always fails is retried about daily.
  - **Full-mix row** (`gemini-3-5-transcribe/fullmix`): once a day, as above.
  - The failures count in `lyrics_attempts`, the same counter `worker_g35t.rs` reads for the #171 full-mix fallback (`FULLMIX_MIN_ATTEMPTS` = 3): after 3 failed passes a song whose isolation yields no vocal is transcribed from the full mix, which replaces its served lyrics on success. Before this rule the error path reset the counter and darkened the song instead.
- **Not a failed re-run: the 30-min cap.** `mark_over_cap` stamps a > 30-min video `unsupported_source` with `has_lyrics = 0` even if it served lyrics: that is the #144 policy (a live set / mix is not a song), not a failure. On the box (read 30.9.2026) the only served row over 30 min is 344, a dub subtitle track (`gemini-live-translate`), which the lyrics worker never picks (`dub_requested`).
- Pinned by `lyrics/worker_outcome_tests.rs` (both served paths — still manual, not re-picked before the backoff, picked once due —, `a_served_song_stays_queued_until_its_third_failed_attempt`, the unserved `no_source` / `asr_gap` pins, and `has_lyrics = 1` with the file gone = not served).
- Never "fix" a failing re-run by resetting the row: that is the darkening this rule removed.

## Mutation-safe loops (a hang fails the gate)

The LCS walk advances with `(i..n).find(..)` inside `for j in ..`, never with a hand-incremented `while` cursor. An `i += 1` → `*=` mutant then gives a wrong result instead of a TIMEOUT. The walk's drop test `suffix[at(r, j)] > suffix[at(r + 1, j)]` has no equivalent mutant; the `< suffix[at(r, j + 1)]` form had one.
