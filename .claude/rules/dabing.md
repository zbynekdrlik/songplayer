---
paths:
  - crates/sp-server/src/db/models_dabing.rs
  - crates/sp-server/src/api/dabing.rs
  - crates/sp-server/src/api/routes_import.rs
  - crates/sp-server/src/startup_dabing.rs
  - crates/sp-server/src/dabing/**
  - scripts/dub_worker.py
  - scripts/dub_live_session.py
  - scripts/dub_loudness.py
  - scripts/win_replace.py
  - scripts/tests/test_dub_worker*.py
  - scripts/tests/test_dub_live_session.py
  - scripts/tests/dub_live_fakes.py
  - sp-ui/src/pages/dabing.rs
  - sp-ui/src/components/dabing_list.rs
  - sp-ui/src/components/dub_toggle.rs
---

# Dabing (dubbing) feature — data model + section contract (#180 D1, epic #174)

The dubbing feature ("Dabing" — the word "sermon" appears NOWHERE in code/UI,
owner ruling #174) makes any video dubbable and adds an optimized **Dabing**
section for fast priority adds. Binding spec:
`docs/superpowers/specs/2026-09-17-dubbing-feature-design.md` (v2); plan:
`docs/superpowers/plans/2026-09-17-dubbing-feature.md` (lanes D1–D6).

## Data model (main decision: columns on `videos`, NOT a `dub_tracks` table)

Dub state lives as **columns on `videos`** (migration V26), mirroring the V24
stems pattern — one row per video, no joins. Spec §3's separate `dub_tracks`
table was **rejected** (two patterns for the same per-video processing state is
the worse maintenance deal). V26 columns:
`dub_requested`, `dub_status` (`none|queued|stems|transcript|translation|synth|
ready|failed`), `dub_file_path`, `dub_engine`, `dub_voice_ref_path`,
`dub_mix_ratio` (REAL, default 1.0), `dub_error`, `dub_attempts`,
`dub_next_attempt_at`, `dub_requested_at`, plus `stem_manual_priority` (the
stems worker's manual bucket — D3 uses it).

## Query surface — `db/models_dabing.rs` (declared `pub mod` in `db/mod.rs`)

- `set_dub_requested(pool, id, requested)` — requesting sets `dub_status='queued'`
  + stamps `dub_requested_at` + raises BOTH `stem_manual_priority` AND
  `lyrics_manual_priority` to 1 (so the chain's inputs jump their queues, spec
  §4). Un-requesting sets `dub_status='none'`, `dub_requested=0`, and LEAVES the
  priority flags (the video may still want stems/lyrics).
- `list_dub_videos(pool)` — `dub_requested=1`, newest first
  (`dub_requested_at DESC, id DESC`).
- `set_dub_mix_ratio(pool, id, ratio)` — clamps to `0.0..=1.0` (NaN → 1.0),
  persists, returns the clamped value.
- `dub_chain_state(dub_status, stem_status, lyrics_present) -> DubChainState`
  (PURE, unit-tested). `dub_status` is authoritative for the named stages; in the
  pre-transcript region (`queued`/`none`) it refines by artefacts: lyrics present
  ⇒ Transcript, else stems done ⇒ Stems, else Queued. Never overrides an explicit
  later status. `DubRow.chain_state` carries the resolved wire string so the UI
  does NOT duplicate the logic. The D4 write-side selectors
  (`get_next_dub_job`, `mark_dub_*`, `record_dub_deferral`) belong to D4, not D1.

## Import must pass the cookie gate (#180 addendum)

The Dabing/manual import metadata fetch is cookie-gated exactly like the download
path. `downloader/tools.rs::fetch_video_metadata(ytdlp_path, url, cookies)` takes
an optional cookie jar and threads `--cookies` (pure `metadata_args` helper,
unit-tested) before the URL. The shared import core `api/routes_import.rs::
import_video_core` resolves the jar as `cache_dir.parent()/cookies.txt` (the app
always sets `cache_dir = <data_dir>/cache`, so the parent IS the data dir where
the operator drops `cookies.txt`) and passes it when it exists. Both
`POST /api/v1/videos/import` and `POST /api/v1/dabing/import` reuse this core
(extracted OUT of `api/routes.rs` to respect its 1000-line cap). If the box's
`cookies.txt` has expired the import fails "Sign in to confirm you're not a bot"
— that is an ops step (`.claude/rules/youtube-cookies.md`), never hidden.

## API — `api/dabing.rs` (registered in `api/mod.rs`)

`GET /api/v1/dabing` → `{playlist_id, videos:[DubRow…]}`;
`POST /api/v1/dabing/import {url}` (import into the seeded Dabing playlist, then
`set_dub_requested(true)`); `PATCH /api/v1/videos/{id}/dub {requested}` (204/404);
`PATCH /api/v1/videos/{id}/dub-mix {ratio}` (200 + clamped `{ratio}` — D4 will
also push it to the live `DubControl`; D1 only persists).

## Seed — `startup_dabing.rs::ensure_dabing_playlist_exists`

Idempotent (`WHERE NOT EXISTS … kind='dabing'`), `name='Dabing'`,
`ndi_output_name='SP-dabing'`, `kind='dabing'`, `is_active=1`. Mirrors
`ensure_live_playlist_exists`; called from `lib.rs::start()`. Split into a sibling
`#[path]` module so `startup.rs` stays under the 1000-line cap. The OBS scene
`sp-dabing` is created by hand in D5 — legacy `yt*` scenes are NEVER touched.

## UI

Nav entry "Dabing" (`app.rs::Page::Dabing`, `/dabing`). `pages/dabing.rs` = a
paste field (`POST /api/v1/dabing/import`) + a 2 s poll filling
`store.dabing: RwSignal<Vec<DubRow>>` (page-owned loop, `try_get_untracked` on the
cancel flag per `sp-ui-frontend.md`). `components/dabing_list.rs` renders rows
newest-first with the glyph row `stiahnuté → stemy → prepis → preklad → dabing →
pripravené` (or `chyba: <krok>`) + a **Prehrať** button (reuses
`api::post_live_play_video` — it already accepts any playlist id).
`components/dub_toggle.rs` is the per-row Dabing toggle placed in `video_list.rs`
(any playlist). The `videos` list payload gains `dub_requested` + `dub_status`
(additive, `#[serde(default)]`, populated in `db::models::row_to_video`).

## Cap discipline hit here (do not repeat the mistake)

`db/models.rs` (998) and `lib.rs` (999) were AT the cap. Adding an AppState
`data_dir` field overflowed `lib.rs`, so the cookie jar is derived from
`cache_dir.parent()` instead (no new AppState field). Keep new server code in
sibling `#[path]`/`pub mod` modules; never grow `routes.rs`/`models.rs`/`lib.rs`/
`worker.rs`/`playback/mod.rs`.

# Dabing D4 (#183) — SK dub synthesis via Gemini Live Translate (audio→audio)

Owner ruling (#174): the ONLY dub engine is **Gemini Live Translate**
(`gemini-3.5-live-translate-preview`, audio→audio) — it keeps the preacher's
pacing, continuity and intensity. NO STT → translation → per-sentence TTS chain,
NO `DubEngine` trait, NO voice-reference selection (the older D4 ticket text is
obsolete). Default mix for a dub video = **dub only** (r=1.0); no same-colour
blend.

## Chain + state machine
`dub_status`: `queued → synth → ready | failed(+backoff)`. **Round 2 (#183): the
dub chain NO LONGER waits for stems** — there is no `stems` park state on the
happy path. Live folds the EN/SK transcripts INTO the one session, so there is NO
lyrics dependency (the `transcript`/`translation` `DubChainState` variants are
unused by D4). `models_dabing.rs` D4 selectors: `get_next_dub_job` (dub_requested=1,
downloaded, not none/ready, past backoff, **newest `dub_requested_at` first** =
the priority queue, now also selects `stem_status`), `mark_dub_synth`,
`mark_dub_ready`, `record_dub_deferral`; pure `dub_stems_ready` /
`dub_stems_state` / `synth_ready` (the proceed-without-stems decision) and
`raise_dub_stem_priority` (sets `stem_manual_priority=1` WITHOUT parking at
`stems`). The old `mark_dub_waiting_stems` (parked at `stems`) is RETIRED.

## Worker (`crates/sp-server/src/dabing/{mod,worker,child}.rs`)
Mirrors the stem worker: 10 s tick, `dub_worker_enabled` kill-switch, venv-python
gate, `HeavyStepPlan::for_activity` (BELOW_NORMAL, never gates playback — owner:
processing runs during playback at reduced priority), #167 startup floor, memory
guard, one heavy child at a time (shared slot). **Round 2 (#183): NO stems
precondition** — the pure `synth_ready(downloaded, dub_stems_state, duration_ms)`
decides `Proceed` / `ProceedRaisePriority` / `WaitForDownload`; the dub NEVER
waits for stems. When stems are merely pending (absent but the duration is within
the 15-min stem cap) the worker raises `stem_manual_priority` ONCE
(`raise_dub_stem_priority`) so a later separation ENRICHES the mix (2-stream →
4-stream on the next open) and proceeds to `synth` immediately; when
`stem_status='unsupported'` (over the cap) or beyond the cap it proceeds without
raising (separation would only be marked unsupported). `dub_engine=
"gemini-live-translate"`.

**Round H step 2 (the FROZEN state, see the round-H section below):**
`synthesize` resolves three things per job and runs the child once:
- **Input** — the pure `worker::dub_input_audio(original, vocals, stem_status,
  vocals_exists)` returns the VOCALS stem (`<base>_audio_vocals.flac`, the DB
  `vocals_file_path`) only when `stem_status == 'done'` (the RAW column vocab, see
  round A+B) AND the path is non-empty AND the file exists; else the normalized
  original `audio_file_path`. A cleaner input at no extra cost (the stem already
  exists).
- **Model** — `dub_model_from(setting dub_model)`: blank → `sp_core::config::
  DEFAULT_DUB_MODEL` (`gemini-3.5-live-translate-preview`); any non-blank id passes
  through (an upgrade = a setting change).
- **Voice** — `dub_voice_from(setting dub_voice)`: blank → `DEFAULT_DUB_VOICE` =
  `DUB_VOICE_SPEAKER` = `speaker` (the speaker's own voice, NO `speech_config`); any
  other non-blank name passes through as a pinned prebuilt voice. Persisted per
  video in the repurposed `dub_voice_ref_path` column (`set_dub_voice`, NO schema
  change) → `DubRow.dub_voice` → the Dabing row shows `sp_core::config::
  dub_voice_label` (`hlas: rečník` / `hlas: <name>`).
The child's stdout summary carries a `session` object (`child::DubSessionStats`:
connections, reconnects, output/input ratio, max voiced gap, latency, drain
reason) which the worker logs at info (`dub worker: live-translate session done`);
the child's last 8 stderr lines are logged at info on success
(`dub live-translate ok; stderr tail`). The eta passed to the stall wait is
`duration_ms + 120 s` (log only).

## Child (`scripts/dub_worker.py live-translate` + `scripts/dub_live_session.py`)
`--audio --out --transcripts --work-dir --model --voice` (no `--chunk-plan`, no
`--pace` — both DELETED). Key ONLY via `GEMINI_API_KEY` env (`bootstrap::
ensure_genai` pins `google-genai==2.24.0`, idempotent, never triggers the heavy
qwen/torch reinstall). The Rust worker ships FOUR scripts
(`embedded_tool_scripts`): `dub_worker.py`, `dub_live_session.py`,
`dub_loudness.py`, `win_replace.py` — all imported at module load, so a missing
one fails every dub (pinned by `embedded_tool_scripts_ship_worker_and_helpers`).
One run: removes the superseded `chunk_*` work files (round C–E2 resume cache) →
decodes `--audio` to 16 kHz mono s16le → ONE continuous Live session (round H
below) with the output PCM appended to `<work_dir>/live_output.raw` → places it on
the video timeline into `<work_dir>/dub_placed.wav` (24 kHz mono) → `_assemble_dub`
(round F loudness + round F2 POSIX replace, UNCHANGED, over that ONE WAV via
`stream_filter()` = `[0:a]aresample=48000[mix]`) → deletes the raw + placed
intermediates → writes the one-chunk transcripts JSON (D3) → prints the summary
JSON (the ONLY stdout line; logs go to stderr). Box evidence left in the work dir:
`session_summary.json`, `loudness.json`, `heartbeat`. **The session event log
(round H4) sits NEXT TO THE DUB** as `<base>_dub_events.jsonl`
(`dub_worker.py::events_path_for(--out)`: `…_normalized_dub.flac` →
`…_normalized_dub_events.jsonl`; every server message + decision incl. each
`input_transcription` / `output_transcription` fragment, flushed per line; a
re-dub overwrites it, a failed run keeps it; a pre-H4 `events.jsonl` in the work
dir is removed as a superseded leftover). It answers timing questions OFFLINE
(pull recipe under "Re-dub a video") instead of a ~40-min re-dub. No event
carries a secret (the key and the connect config are never logged, errors go
through `redact`, resumption handles only as `handle_present`). The `heartbeat`
is written on session PROGRESS only (frames sent or output arrived; at most
every 5 s) and before each assembly pass, and `live_output.raw` grows with every
output chunk — both in the work dir. So the #171 `wait_with_stall_timeout`
(newest mtime in the work dir) never kills a healthy real-time stream, but a
stuck one goes stale. A crashed job
restarts from the beginning (no partial resume — the dub is produced ahead of
playback; resumption handles are only used across connections of ONE run).
Cost ~$0.037/min.

## #184 round G — the dub mix is now the ONE global fader console (SUPERSEDES the per-video ratio below)

The per-video `videos.dub_mix_ratio` column, `set_dub_mix_ratio` / `clamp_dub_ratio`
/ `dub_ratio_if_ready`, `dub_gains` / `dub_over_original_gains` / `DUB_ORIGINAL_FLOOR`,
`PATCH /api/v1/videos/{id}/dub-mix`, `EngineCommand::SetDubMix`, and the
`components/dub_mixer.rs` adapter are **DELETED**. The mixer is now the ONE global
three-fader console (`vokaly` / `podklad` / `dabing`) — see
`.claude/rules/karaoke-stems.md` "#184 round G". Key deltas for dub playback:

- The reader gain sets are DERIVED from `sp_core::mixer_model::stream_gains_*`, not
  a ratio: `DubMix` uses `stream_gains_dub(f)` (`[1,0,0,dabing]` when vokaly &
  podklad full, else `[0,vokaly,podklad,dabing]`); `DubOverOriginal` uses
  `stream_gains_dub_no_stems(f)` = `[vokaly, dabing]` — the whole original at the
  `vokaly` fader, **NO −18 dB floor**. `MixControl` publishes both from
  `set_faders(f)`; `reader::dub_gains_for(kind, MixFaders)` is the pure per-kind
  gain (still unit-tested).
- The mix is set via `PATCH /api/v1/mix {kind, vokaly?, podklad?, dabing?}` →
  `EngineCommand::SetMix{kind, faders}` → `engine.set_mix` → `control.set_faders(kind, f)`;
  the persist is done by the API handler AFTER the live push
  (`api/mix_apply.rs::apply_mix`, the round-A order). The dub video's readiness
  (its `DubRow.dub_status` / `stem_status`) drives which faders are LIVE.
- **Round G2 (SUPERSEDES G1) — the mix VALUES are remembered PER ITEM KIND, and each
  reader FAMILY is fed from its OWN memory; there is NO global "active kind".** The
  console keeps a SONG memory and a DUB memory (`MixConsole { song, dub }`, default
  song `(1,1)` / dub `(0,1,1)`); the dub readers always ramp toward the DUB memory,
  the song reader toward the SONG memory, so a song starting on ANY other output no
  longer doubles a playing dub's voices (and a dub never instrumental-mutes the next
  song). `PATCH /mix` NAMES the memory it edits (`kind` required) + persists only that
  kind's keys (`mix_song_*` / `mix_dub_*`); `GET /mix` returns BOTH memories
  `{song, dub}`. The G1 global `active`/`select_kind`/item-open hook are DELETED. See
  `.claude/rules/karaoke-stems.md` "#184 round G2".
- The dabing list (`dabing_list.rs`) no longer shows a per-row ratio; the console
  lives in the shared Player above.

Everything below is the pre-round-G per-video-ratio history — treat `dub_mix_ratio`
/ `dub_gains(r)` / `DUB_ORIGINAL_FLOOR` / `/dub-mix` / `SetDubMix` as DELETED.

## Playback (`stems/reader.rs` + `stems/control.rs`)
`open_audio_stream`'s pure `audio_source_kind(vocals, instrumental, dub)` chooses:
- **`DubMix`** — dub + BOTH stems → a **4-stream** `StemMixReader` `[original,
  vocals, instrumental, dub]` fed `dub_gain_handles()`. `dub_gains(r) =
  (0, 1−r, 1, r)`: original full-mix silent, vocals = original voice,
  instrumental = ambient bed, dub = SK.
- **`DubOverOriginal` (#183 round 2)** — dub WITHOUT both stems → a **2-stream**
  `StemMixReader` `[original, dub]` fed `dub_over_original_gain_handles()`.
  `dub_over_original_gains(r) = (max(1−r, DUB_ORIGINAL_FLOOR), r)` with
  `DUB_ORIGINAL_FLOOR = 0.125` (−18 dB): the FULL original (English speaker) is
  the bed, floored so the room never goes dead under the dub; at r=0 the original
  is full and the dub silent. This is what lets a long, un-separable video be
  dubbed. Stems arriving later promote a fresh open to `DubMix`.

Default r=1.0. `PATCH /api/v1/videos/{id}/dub-mix` persists to DB AND sends
`EngineCommand::SetDubMix{video_id, ratio}` → `engine.set_dub_mix` →
`control.set_dub_ratio`, which publishes BOTH the 4-stream quad AND the 2-stream
pair from the same `r` (live, no pipeline reopen — the #186 seam; only one mix is
ever open). `dub_gains_for(kind, r)` is the pure, unit-tested per-kind gain
chooser. Any dub-reader open failure degrades to the stem/plain mix.

## Cap note
`lib.rs` was at exactly 1000 lines, so the `EngineCommand` match was extracted to a
sibling `engine_dispatch.rs` (free `dispatch(&mut engine, cmd)`) before adding the
`SetDubMix` arm + the dub-worker spawn.

## Known gotchas (D4, verified on win-resolume 18.9.2026)

- **RESOLVED in round 2 (#183): a long video no longer stalls at
  `dub_status=stems`.** The stems precondition was removed (`synth_ready` never
  waits); a video without both stems plays the 2-stream `DubOverOriginal` mix
  (`[original, dub]`, original bed floored at −18 dB). The 15-min
  `STEM_MAX_DURATION_MS` cap still marks a 40-min file `stem_status=unsupported`,
  but the dub now proceeds to `synth` regardless. The sample "Morning Prayer &
  Devotion" (40 min, video 344) is the acceptance case: `stems → synth → ready`
  on its own, playing the 2-stream mix. (Historical: round 1 required stems and
  parked long videos forever — the owner's actual 40-min use case.)
- **`dub_worker.py` is materialised only when a dub reaches the synth step.**
  `DubWorker::ensure_script` runs AFTER the stems precondition, so on a box whose
  only dub video is stuck at `stems`, `dub_worker.py` is NOT written to
  `cache/tools/` and `google-genai` is NOT yet installed. To test the child
  standalone, write the script + `pip install google-genai==2.24.0` yourself.
- **Live-Translate core proof (real key, on-box):** a 20 s EN slice → 44 s of
  24 kHz SK PCM (~real-time + drain); `google-genai==2.24.0` installs + imports in
  the lyrics venv. (The old `dub_pace` 2× input pacing is DELETED in round H —
  the continuous session is paced at exactly 1.0×.)
- **win-resolume console is cp1252 — a Python `print()` of Slovak (`ď` = ď)
  raises `UnicodeEncodeError`.** The real child is fine (it writes UTF-8 JSON with
  `ensure_ascii=False`); a debug probe must set `PYTHONIOENCODING=utf-8` or write to
  a file, not print SK text to the box console.

# Dabing D3 (#182) — EN/SK subtitles from the dub session (no second transcription)

Owner verdict (18.9.2026): a dubbed video does NOT run a second STT/translation
pass. The Gemini Live Translate session already returns the EN input
transcription + the SK output transcription; the dub child saves them as
`<base>_dub_transcripts.json`, and D3 turns that JSON into a normal
`sp_core::lyrics::LyricsTrack` so the wall/dashboard render bilingual subtitles
exactly like song lyrics.

## Transcript JSON schema (`scripts/dub_worker.py::build_transcripts`)
`{engine, target_lang, chunks:[{index, start_ms, end_ms, at_ms, tempo, en,
en_timed:[{t_ms, text}], sk, sk_timed:[{t_ms, text}]}]}`. **Round H step 2: exactly
ONE chunk** — `{index: 0, start_ms: 0, end_ms: <input length>, at_ms: 0, tempo:
1.0}` — whose `en` = the joined input transcription, `sk` = the joined output
transcription (both kept for readability only; the builder does not read them),
`sk_timed` = the SK fragments stamped on the VIDEO timeline (output-transcription
arrival − t0 − latency, non-decreasing) and **`en_timed` (round H3)** = the EN
input-transcription fragments stamped on the VIDEO timeline (input-transcription
arrival − t0 − **`en_latency_ms`** (round H4), non-decreasing). Live Translate
emits the input transcription per phrase, seconds after the audio was sent, so
the raw arrival (H3 stamped it with latency 0) sat ~4–5 s late — one SK line
below its own translation on video 344 (#184 5807462051); `en_latency_ms` is
measured per session with the SAME method as the SK (Placement, below). `dub_worker.py::_timed_from` holds the connection ordering +
overlap cap once; `sk_timed_from` and `en_timed_from` both call it. The session
records BOTH transcriptions as `(arrival_s, text, conn)`
(`dub_live_session.py::SessionState.input_parts` / `output_parts`), and
`joined_by_connection` takes that shape. With `at_ms` 0 / `tempo` 1.0 the builder
below maps `t_ms` straight to video time, so the round-H one-chunk form needed no
timing change in D3 (the H3 EN assignment is the only builder change since; pinned by
`subtitles_tests.rs::one_continuous_session_chunk_builds_a_monotonic_bilingual_track`
+ `test_dub_worker.py::test_transcripts_are_one_chunk_on_the_video_timeline`). The
multi-chunk form (per-chunk `at_ms`/`tempo`, a legacy JSON without them) is still
read correctly — dubs made before round H keep their subtitles.

## Subtitle builder (`crates/sp-server/src/dabing/subtitles.rs`, pure + tested)
`transcripts_to_track(&DubTranscripts) -> LyricsTrack`:
- Fragment `i` occupies the chunk-local output window `(t[i-1], t[i]]` (`t[-1]=0`).
- Fragments group into LINES: close at sentence punctuation (`. ! ? …`), at 14
  words (`MAX_WORDS_PER_LINE`), or on a `> 1500 ms` (`LINE_GAP_MS`) arrival gap.
- Line video time = `at_ms + local_ms / tempo` (legacy JSON without `at_ms`/
  `tempo` → `start_ms` and `1.0`); `tempo` is clamped to `0.25..=4.0`
  (NaN/∞/≤0 → 1.0) and `to_video_ms` uses `saturating_add` so a degenerate
  tempo can't overflow the offset (#182 release-review item 8).
- **Two-pass timing (#182 release-review item 7 — no MIN-extension drift).**
  Pass 1 gives each line its TRUE `start` (monotonic — clamped to the PREVIOUS
  line's START, never its extended end) and its TRUE end. Pass 2
  (`finalize_line_ends`) sets the displayed `end = max(true_end,
  start + MIN_LINE_MS)` TRIMMED back to the next line's start when that is
  earlier (the last line keeps the untrimmed value; a same-start pair gets
  `start + 1`). This fixes the old bug where clamping each `start` to the
  previous MIN-EXTENDED `end` made a run of short lines push every later line
  progressively later (accumulated drift). Result: starts monotonic, no
  overlap, and a normal line after a run of 100 ms lines keeps its true start.
- A fragment group whose joined SK is empty after trim produces NO line (#182
  item 9); an all-blank transcript yields an empty track, so
  `subtitles_store::build_and_store_subtitles` stores nothing and returns 0.
- **EN per line (round H3, #184 design 5806462894) — timed by the INPUT
  transcription, whole sentences to the nearest SK line.** The chunk's `en_timed`
  fragments are mapped through the SAME `to_video_ms(at_ms, t, tempo)` as the SK,
  then the pure `assign_en_sentences(en, line_starts)`:
  - groups them into SENTENCES with the SK's `ends_sentence` rule (a fragment
    ending in `. ! ? …`, or the last fragment). Fragments are concatenated AS-IS,
    exactly like the SK line text: Live Translate fragments carry their own
    spaces, so there is no separator; the sentence is then trimmed. A run of
    blank fragments yields no sentence;
  - times each sentence by its first NON-BLANK fragment;
  - gives each WHOLE sentence to the chunk's line whose start is nearest
    (`nearest_line_from`, the first index on a tie, so equal starts resolve to
    the earliest line). The search starts at the previous sentence's line, so the
    assignment is monotonic and never goes backwards;
  - joins several sentences on one line with a space. A line may carry 0, 1 or
    several EN sentences when the translation merges or splits sentences, which is
    better than a sentence cut mid-way.
  EN stays within its own chunk's lines. Intra-fragment punctuation does not
  split a sentence (the rule looks at fragment ENDS only).
  **`en_slice` and the SK character-fraction path are DELETED** (owner rule:
  superseded paths are deleted, not kept as a fallback). It cut the one untimed
  `en` string per SK line by the SK character fraction, and in the one-chunk
  regime the error accumulated over the whole video. On video 344, „Rene Garcia."
  showed „First one" and „Prvý prihlásený." showed „in. Good to see you.".
  **A transcript written before H3 (no `en_timed`) has NO EN** until the video is
  re-dubbed (`PATCH /api/v1/videos/{id}/dub {"requested":true}`, below). A
  subtitle track ALREADY STORED before H3 (e.g. video 344) keeps its old
  char-fraction EN: the startup backfill only builds dubs that have no track, so
  only a re-dub replaces it.
  No `sk_timed` → no lines.
- `words: None`, `source = "gemini-live-translate"` (`SOURCE_LIVE_TRANSLATE`).
  Every branch is covered in `subtitles_tests.rs`.

## Persist through the SHARED writer (no parallel writer)
The lyrics worker's JSON-sidecar + DB persist was extracted to
`lyrics/track_store.rs::persist_lyrics_track` (writes `{youtube_id}_lyrics.json`
+ `mark_video_lyrics_complete`). BOTH the lyrics worker AND
`dabing/subtitles_store.rs::build_and_store_subtitles` call it — never a second
writer. The dub worker (`dabing/worker.rs::synthesize`) builds + stores the
subtitle track after the dub file is finalized and BEFORE `dub_status = ready`;
a subtitle failure is a WARN log and NEVER fails the dub.

## Startup backfill for dubs finished before D3 (#182)
The builder runs only at the end of a synthesis, so a dub that was already
`ready` (the 40-min acceptance sample) would never get subtitles. Once per
process, `DubWorker::process_next` (after the `dub_worker_enabled` kill-switch)
calls `subtitles_store::backfill_missing_subtitles`: the pure-SQL selector
`models_dabing::list_ready_dubs_without_subtitles` (`dub_requested=1`,
`dub_status='ready'`, `lyrics_source` NULL or != `gemini-live-translate`) → build
from the saved `<base>_dub_transcripts.json` (a legacy JSON without `at_ms`/`tempo`
falls back to `start_ms` / `1.0`). Once per process, never per tick — an unusable
JSON must not loop; failures are WARN only. The call sits in `process_next`
(structurally mutation-excluded), NOT in `run`, so it adds no whole-fn mutant.

## Lyrics queue skips dub videos
Every selector bucket in `lyrics/reprocess.rs` (manual/null/stale/fullmix) ANDs
in ONE shared predicate const `EXCLUDE_DUB_REQUESTED`
(`AND (v.dub_requested IS NULL OR v.dub_requested = 0)`), and
`db/models_dabing.rs::set_dub_requested` NO LONGER raises `lyrics_manual_priority`
(keeps `stem_manual_priority`). A 40-min talk therefore never enters the ★/g35t
song-lyrics pipeline. `LYRICS_PIPELINE_VERSION` is untouched.

## UI (chain + mixer)
- `components/dabing_list.rs`: the chain is the real engine chain
  `stiahnuté → dabing → titulky → pripravené`; a `stemy` step is inserted when
  the video has or is getting stems (`shows_stems_step`: shown unless
  `stem_status = "unsupported"`). `prepis`/`preklad` are GONE.
- `sp_core::mixer_model::dub_channel_labels(has_stems)` +
  `components/dub_mixer.rs`: WITHOUT stems only 2 faders (`originál` / `dabing`,
  ambient hidden — the 2-stream over-original mix); WITH stems the full 3-fader
  strip (`originál hlas` / `dabing` / `ambient`). `has_stems = stem_status ==
  "done"` for the mixer (current reality), broader for the chain (intent).
  `e2e/mixer.spec.ts` asserts both fader shapes; `e2e/dabing.spec.ts` both chain
  shapes; zero console errors.

# Dabing round A+B (#184) — instant mix apply + one mixer rule on every page

## `DubRow.stem_status` is the RAW stems-worker column, NOT the `stems_state` wire vocab
`DubRow.stem_status` (server `db/models_dabing.rs::row_to_dub_row`) is the raw
`videos.stem_status` COLUMN: `NULL` (pending) / `'done'` (ready) / `'failed'` /
`'unsupported'`. This is a DIFFERENT vocabulary from the derived `stems_state`
wire strings (`ready`/`queued`/`processing`/`failed`/`unavailable`) that
`stems::models_stems::stems_state_of` produces for the karaoke/videos payloads.
So `dub_mixer.rs` uses `stem_status == "done"` for `has_stems`, and
`sp_core::mixer_model::mixer_controls`'s stems-capable set MUST include `"done"`
(the real value a dub row carries) — the wire strings alone would make a
stems-ready dub (`stem_status='done'`) fail the karaoke gate on the real box
while passing under a mock that feeds `"ready"`. Never assume the two columns
share a vocabulary.

## Dub-mix applies live-first: push the engine command BEFORE the DB persist
`api/dabing.rs::patch_dub_mix` awaits `EngineCommand::SetDubMix` FIRST, then
`set_dub_mix_ratio` (the persist), via the pure seam
`api/dabing_apply.rs::apply_dub_mix(push, persist)`. The old order (persist then
push) let the DB `acquire()` park up to sqlx's **30 s** default before the live
gains moved — the owner's "~30 s to apply" report. Clamp once via
`models_dabing::clamp_dub_ratio` so the push carries exactly what gets stored; a
persist failure is logged + 500 while the live change already happened. Pairs
with `db/mod.rs::pool_tuning()` (WAL + NORMAL sync + 5 s busy + **2 s** acquire),
applied only to the FILE pool (`create_memory_pool` stays plain — WAL needs a
file). No schema change.

## The `/api/v1/dabing` poll lives in `App`, not the Dabing page (#184 B1)
`store.dabing` (and `store.dabing_playlist_id`) are filled by an App-level
`store::poll_value` loop (`app.rs`, next to the playlists load), so the shared
Player picks the dub mixer for a playing dub video on EVERY page — not only after
visiting `/dabing`. `pages/dabing.rs` just READS the store now. `player.rs`
renders `<DubMixer>` and/or `<KaraokeMixer>` from `mixer_controls(dub_status,
stem_status)`, with `show_karaoke = controls.karaoke || !controls.dub` (a non-dub
song always keeps the karaoke default; a dub video shows karaoke only when
stems-capable). `e2e/dabing-mixer.spec.ts` proves it on Prehľad + Naživo without
a `/dabing` visit.

# Dabing rounds C / E / E2 (#184) — DELETED by round H step 2 (history only)

Round C pinned one prebuilt voice per video via `speech_config` (default `Charon`)
because the old per-chunk sessions re-rolled the voice. Round E found the pinned
voice DRIFTS inside a long session and capped each Live session at 2 min
(`chunk_plan.rs`, `dub_session_max_s`, ffmpeg `silencedetect` pause cuts) plus a
per-chunk voice-band guard; round E2 made that guard baseline-relative with up to 2
re-synths (`chunk_voice_drift`, `_chunk_is_drifted`, `VOICE_F0_BAND`,
`baseline_from_meta`), a voice-keyed per-chunk resume (`chunk_reusable`) and an
`atempo`/`adelay`/`amix` placement (`build_mix_filter`, ≤ 1.08×). The #184 audit
(comment 5794017528) traced the root cause to the MACHINERY itself: every one of
the ~18 session starts per talk re-rolls voice/prosody right after a pause — the
model's documented limitation ("voices might shift after long pauses") — so the
chunking, re-synth and pinning were fighting a symptom they created. The round-H
probe (5797563445) proved the designed usage instead, and the owner chose the
speaker's own voice + deleting the chunking (5797691198) and froze the result
(5797708129). ALL of the above is DELETED (owner rule: superseded paths are
deleted, never kept as a fallback): `chunk_plan.rs` + tests, `dub_pace`,
`dub_session_max_s`, the silencedetect pre-pass, the per-chunk drift check
(`DUB_MAX_DRIFT_MS`), the resume, the guard + re-synth, the atempo placement and
the pinned default voice. `scripts/dub_voice_check.py` (the f0 drift file check)
is NOT shipped with the dub any more, but the file stays: the eval harness uses it
(`eval/dubbing/voice_band_measure.py`, `dub_voice_check.py --source` on a probe
output — `.claude/rules/dubbing-eval.md`). A pinned prebuilt voice is still
possible (`dub_voice=<name>` adds `speech_config`), but no longer the default.

# Dabing round H step 2 (#184) — the FROZEN dub path: ONE continuous Live session in the speaker's own voice

Design record 5798586079 (main); probe evidence 5797563445 (arm A, 25 min of video
344: 3 connections via GoAway + resumption handle, 0 reconnect failures, context
compression past the 15-min limit, output/input 1.0017, first output ~3.1 s, stable
level); owner decisions 5797691198 (no TTS path, speaker voice, delete chunking)
and 5797708129 (freeze this state, the model is a setting).

## The session (`scripts/dub_live_session.py::ContinuousSession`)
- ONE logical session: `translation_config{target_language_code: sk,
  echo_target_language: False}`, input + output transcription,
  `context_window_compression` (trigger 25 000 / sliding target 8 000 tokens),
  `session_resumption`; `speech_config` ONLY for a pinned prebuilt voice
  (`live_config`, `voice_for_config`: `speaker`/blank → none). Model from `--model`.
- **Pacing:** 100 ms frames at 1.0× on ONE wall-clock anchor (frame k at
  `anchor + k·0.1`, computed, never a summed sleep; the pacer's clock/sleep are
  injectable so tests assert it exactly on a virtual clock). One anchor for the
  whole run — NOT re-anchored per connection like the probe — so a frame's video
  position is its index: after an unannounced close the owed frames go out at
  catch-up speed instead of shifting everything later.
- **GoAway = overlap, never a pause.** The GoAway handler immediately opens the
  next connection with the ACTIVE connection's latest resumption handle while the
  sender KEEPS feeding the old one; once the new one is open the sender switches at
  the next frame boundary, and the old one only drains its trailing translation
  until `time_left − 1 s` or 8 s without voiced output (`drained` event). The
  probe paused input ≤ 8 s per GoAway — its 13 s output gap. A handle arriving on
  a DRAINING connection is ignored (it would rewind to before the switch). A close
  without GoAway reconnects the same way; a failed send — or one stuck longer than
  `send_timeout_s` (30 s) — closes that connection and retries the frame on the
  next one. **No reconnect once every frame is sent** (a new connection would
  carry nothing; the old one drains what it got): a reconnect already in flight
  then is ignored if refused and closed if it opens (`reconnect_ignored`), so a
  GoAway on the last frames never fails a fully sent dub. **A refused (re)connect
  while input is unsent, a connect that does not complete in 30 s (`did not
  complete within 30 s`), or 50 connections raise `SessionFailed` → the child
  exits 1 with `Live reconnect (connection N) refused: …`** and the Rust backoff
  retries the job. A LOCAL error while recording (a full disk in the raw sink, a
  bug in `_record`) is fatal too — only the transport is guarded in `_receive`, so
  it can never pose as a closed websocket and reconnect into the same error (one
  recorded after the drain end is raised too, never returned as success).
  UNVERIFIED until the first box run: that the server accepts a resume while the
  old connection is still open (the probe only resumed after its grace).
- **Teardown never waits for a connect.** A connection still CONNECTING never
  looks at its `stop` event, and the SDK's setup wait has no timeout of its own
  (google-genai 2.24.0 `live.py` awaits the setup reply bare). ONE guard: a
  cancelled `_open` (teardown cancelling the in-flight reconnect, or the whole run
  cancelled during the first connect) cancels its connect task and marks it
  closed. Without it, a drain that ended while a reconnect was connecting hung
  the child until the Rust stall kill (review round 2, reproduced on 3.11 +
  3.12; pinned by `test_the_drain_ends_while_a_reconnect_is_still_connecting` +
  `test_cancelling_the_run_during_the_first_connect_cancels_that_connect`). A
  connection dropped by a failed/stuck send is marked draining, so a message
  recorded after the drop is not counted as the active stream nor resumed from.
- **Drain:** `audio_stream_end` once after the last frame (one best-effort send on
  the active connection), then until 8 s without VOICED output (a chunk above
  −50 dBFS — the session streams silence after speech), or 60 s, or every
  connection closed.
- **Heartbeat on progress only:** `on_progress` fires from a supervisor poll only
  when frames were sent or output arrived since the last call (the child writes
  `heartbeat` at most every 5 s from it), so a stuck session goes stale for the
  Rust stall timeout instead of looking alive because the loop still ticks.
- **Python 3.11 teardown trap:** `asyncio.wait_for` (the send timeout) can
  swallow a cancellation that lands as its inner send completes; a cancelled
  sender then waited forever on the cleared ready event (a failing session never
  returned — caught only by running the suite under 3.11, the eval-checks
  version). While stopping, the ready event stays set (`_not_ready`) and the
  sender returns on `_stopping`. Run the scripts suite under 3.11 locally
  (`uv venv --python 3.11`) when touching the session loop.
- Output chunks carry `arrival_s`, `conn`, `offset` (into the raw sink), `voiced`,
  `active` (arrived while its connection was the active one).

## Placement (`dub_worker.py`, pure + tested)
- `latency_ms = clamp_latency_ms(measure_latency_ms(first voiced output arrival,
  t0, input onset))`: first voiced output arrival − t0 (frame 0 sent) − the first
  voiced INPUT frame's position (`first_voiced_frame`, so a silent/instrumental
  intro is not counted as model latency), clamped to 1000–6000 ms. Raw + clamped
  values are logged and kept in `session_summary.json`.
- **EN latency (round H4, #184 design 5807466038):** `en_latency_ms(input_parts,
  t0, onset)` = the SAME `measure_latency_ms` + `clamp_latency_ms` over the FIRST
  (earliest non-empty) input-transcription arrival → `(clamped, measured)`; no
  input transcription (or no send) → `(0, None)`, EN then stamped at its raw
  arrival. The 1–6 s clamp fits the input side: the first fragment comes a phrase
  + the ASR lag after the onset (~4–5 s on 344). `en_timed_from(parts, t0,
  en_latency_ms)` subtracts it through the shared `_timed_from` (clamp at 0,
  monotonic, connection order + overlap cap unchanged). Summary fields
  `en_latency_ms` / `en_measured_latency_ms`. A single first-phrase measure can be
  noisy; if a 20-line read shows residual drift, derive a median offline from the
  saved `<base>_dub_events.jsonl` (the design's stated trade-off).
- `place_output`: every chunk lands at `max(cursor, arrival − t0 − latency)` —
  ONE continuous stream per connection in arrival order (a burst never overlaps
  itself, a stall re-syncs to arrival). Connections keep SEPARATE cursors and
  `render_placed_wav` SUMS them (clipped): the old connection's trailing
  translation overlaps the new one's start; one cursor across both would push
  every later second late by the overlap on each reconnect. When a connection's
  stream runs AHEAD of its arrival by more than `CATCH_UP_TOLERANCE_MS` (500 ms —
  a burst, output slightly faster than real time), its streamed SILENCE chunks are
  dropped (start `None`) until it is back; voiced audio is never dropped
  (`dropped_silence_s` in the summary). The WAV is at least the input's length;
  the body is a memmap and the raw file is read per chunk (a 36-min talk never
  sits in memory). A failed run removes `live_output.raw` / `dub_placed.wav`.
- Transcriptions carry their arrival time and connection (`(arrival_s, text,
  conn)`, input AND output since round H3); EN, SK, `en_timed` and `sk_timed`
  are ordered BY CONNECTION (the old connection's trailing text arrives after
  the new one's first text but translates earlier input — never interleaved).
  `sk_timed` =
  output-transcription arrival − t0 − latency, clamped ≥ 0 and made
  non-decreasing — by CAPPING a connection's late (overlap) fragments at the
  EARLIEST first fragment of any later connection, never by pushing a later
  connection's subtitles past its audio (review rounds 2–3). The placed-WAV memmap is released even on a
  failed render, and the intermediate cleanup logs a removal failure instead of
  masking the run's real error (Windows cannot delete a mapped file).

## Log lines the box acceptance reads (stderr → sp-server log)
`live: connect N (handle_present=…, frame K)`, `live: go_away time_left 50s on
connection N at frame K`, `live: reconnect (go_away|closed) handle_present=…
frame_index=K`, `live: switch to connection N at frame K`, `live: connection N
drained (quiet|deadline)`, `live: frame K/N output Xs` (every 600 frames),
`live: audio_stream_end after frame N/N`, `live: drain end (quiet|tail_cap|
closed)`, then the summary `dub session: connections C, reconnects R,
output/input X, max voiced gap Gs, latency L ms (measured M ms, input onset O ms),
EN latency E ms (measured N ms), dropped silence Ds, drain …`, then the round-F
`dub loudness: …` line (the last one). (The Rust `DubSessionStats` parses only
`latency_ms`; the EN latency reaches the sp-server log through this stderr line.)
`output_to_input_ratio` counts the active stream + VOICED draining output — the
silence a draining connection keeps streaming during the overlap would inflate it
by ~8 s per reconnect (`overlap_output_s` reports all draining output).

## Tests
`scripts/tests/test_dub_live_session.py` against `scripts/tests/dub_live_fakes.py`
(a copy of the probe's fake-Live pattern — eval and production stay decoupled; the
eval-checks job has no google-genai). The overlap proof: `FakeServer(...,
open_after_frames={2: 5})` holds connection 2's connect until the server received
5 MORE frames, which only the still-fed OLD connection can deliver — a pause would
hang the test into its 60 s `wait_for` failure. Other fake knobs:
`connect_delay_s` (a slow real reconnect), `connect_advance_s` (the reconnect
time on the VIRTUAL clock — proves owed frames catch up and the schedule is never
re-anchored), `hang_at` (a send that never returns). A GoAway meant to be seen
BEFORE the stream end must ride the second-to-last frame: on the last one it is
recorded only after `audio_stream_end`. `test_dub_worker.py`: placement,
latency (SK + the H4 EN latency), transcripts shape, render, argv defaults,
legacy cleanup, the event-log path and the whole `cmd_live_translate` with the
session + ffmpeg seams faked (the fake logs input/output transcription events and
the test reads them back from `<base>_dub_events.jsonl`). Rust: `worker.rs`
tests (`dub_model_from`, `dub_voice_from`, `dub_input_audio`), `child.rs` (argv,
session stats), `sp-core config` (keys, defaults, `dub_voice_label`),
`subtitles_tests.rs::one_continuous_session_chunk_builds_a_monotonic_bilingual_track`.
Trap: the staging secret-scan blocks a test literal `secret="…"` — the fake runner
takes `redact_word=`.

## Upgrade path (the ONLY sanctioned change to this frozen path)
When a newer Live Translate model ships: set `dub_model` (Nastavenia → Dabing →
`Model dabingu`, or `PATCH /api/v1/settings {"dub_model": "<id>"}`), run the round-H
probe with `--model <id>` on the box (`.claude/rules/dubbing-eval.md`, same slice
of video 344), re-dub ONE video (below) and compare the new `session_summary.json`
+ a listen against the current dub. No code change, no new tuning round. The
probe is only the capability pre-check (does the model accept compression /
resumption / GoAway, its latency and voice); it keeps its OWN loop, which pauses
input on GoAway (eval and production are deliberately decoupled — the dispatch
decision for step 2). The JUDGEMENT of a new model is the production re-dub's
`session_summary.json` + a listen, never the probe's gap numbers.

# Dabing round F (#184) — the dub is loudness-matched to the audio it translates

Owner verdict 2026-09-23 (#184 comment 5793796815): "ked su pomery rovnake tak
dabingovi hlas je tichsi ako orginal!". Measured on video 344 (5–15 min, ebur128):
original −14.5, vocals −14.5, **dub −16.1 LUFS**. The old assembly used a fixed
single-pass DYNAMIC `loudnorm=I=-16`, while the original is two-pass `I=-14`
(`downloader/normalize.rs`). The stems are never re-normalized (`karaoke-stems.md`),
and the mixer applies faders as plain linear gains, so the same fader value played
the dub 1.6 LU quieter than the voice next to it.

**Rule: dub loudness = the MEASURED integrated loudness of the child's `--audio`,
clamped to −24…−10 LUFS, applied with a two-pass LINEAR loudnorm (TP −1.5, LRA 11).**
`--audio` is what the session translates: since round H step 2 the VOCALS stem when
stems are done (`worker::dub_input_audio`), else the normalized ORIGINAL — so the
dub is matched to the voice it replaces in the 4-stream mix. For speech the two
measure the same (344: −14.5 / −14.5). The pure rules live in **`scripts/dub_loudness.py`**, which ships
next to the worker: it is the 3rd entry of `dabing::worker::embedded_tool_scripts`,
`dub_worker.py` imports it at module load, and it is in the CI ruff scope. If it is
missing on the box, every dub fails. `dub_worker.py::_assemble_dub` heartbeats
before each pass and makes three ffmpeg calls:
1. `loudness_measure_args(ff, audio)` does a loudnorm analysis of the input (null
   muxer), then `loudness_target(input_i)` applies the clamp (`-inf` clamps to −24,
   `NaN` raises).
2. `assembly_args(…, loudnorm_analysis_filter(target), None)` analyses the
   placed output (round H: the ONE `dub_placed.wav`, `stream_filter()`) to null.
3. `assembly_args(…, build_loudnorm_second_pass(mix, target), part, 48000)` writes
   `<base>_dub.part.flac` (`partial_out_path`) with `measured_I/LRA/TP/thresh` +
   `offset`, `linear=true`, `print_format=json`. It is promoted over the dub
   (`win_replace.replace_file`, see below) ONLY after ffmpeg exits 0 AND its
   loudness report parses. On any failure the partial is deleted and the PREVIOUS
   good dub stays. A partial left behind by a hard-killed child (stall timeout,
   server exit) is removed, with a log line, at the start of the next assembly.

**Promote the dub with a POSIX rename, NEVER `os.replace` (#184 round F2).** On Windows
`os.replace` is `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`. It fails with `[WinError 5]
Access is denied` whenever ANY other handle has the target open, even one opened with
`FILE_SHARE_DELETE`. SongPlayer holds `<base>_dub.flac` open whenever the video is
loaded in SP-dabing, even paused: `stems/reader.rs` uses Rust std, which shares
READ|WRITE|DELETE. The reader's path comes from `stems::dub_path(audio)`, so it is fixed.
Box 2026-09-23: the rebuild of 344 failed at exactly that call, and would fail on
every retry while the video stayed loaded.
- **The fix.** `scripts/win_replace.py::replace_file` opens the partial with
  `DELETE` access and calls `SetFileInformationByHandle(FileRenameInfoEx,
  REPLACE_IF_EXISTS | POSIX_SEMANTICS)` (Windows 10 1709+). This is the same call
  Rust std's `fs::rename` falls back to.
- **What the reader sees.** The directory entry switches at once. The open reader
  keeps reading the OLD data through its handle, and the NEXT open (the next video
  load) gets the new dub. Playback never has to stop.
- **Off Windows** it is plain `os.replace`.
- **Buffer layout.** `build_rename_info` is pure and pinned on Linux
  (`scripts/tests/test_win_replace.py`): the x64 layout Flags@0, RootDirectory@8,
  FileNameLength@16 (UTF-16 BYTES, no NUL), FileName@20, sizeof 24. The struct uses
  explicit-width ctypes types, because `c_ulong` and `c_wchar` differ in size
  between Linux and Windows.
- **Shipping.** `win_replace.py` is the 4th entry of `embedded_tool_scripts`, and
  `dub_worker.py` imports it at module load (`import win_replace as wr`). It is in
  the CI ruff scope.
- **When the rename itself fails** (a reader opened WITHOUT share-delete, a
  pre-1709 Windows, or a cache dir on a non-NTFS/network volume that lacks
  `FileRenameInfoEx`; there is deliberately no fallback to plain `FileRenameInfo`,
  because the box cache is on NTFS `C:`), the run fails loudly with the Win32 error
  and both paths.
  The old dub stays, and the dub row records the error and retries after the
  backoff.

`parse_loudnorm_json` reads the last `{…}` block carrying `input_i` (CRLF-safe) and
raises if the block is missing or incomplete. The stats (`source_i`, `target_i`,
`mix_i`, `output_i`, `normalization_type`) go to `<work_dir>/loudness.json` as
strict JSON (`json_safe_stats`: a non-finite value becomes `null`) and to the
`dub loudness: …` stderr line. That line is the last child log line, so it shows in
the sp-server "dub live-translate ok; stderr tail". ffmpeg silently falls back to
DYNAMIC mode when the linear gain would breach TP −1.5 or the mix LRA exceeds 11.
The child then logs a `dub loudness: WARNING … fell back to dynamic` line, and
`normalization_type` records which mode actually ran, so read it before trusting a
level. Local real-ffmpeg 6.1 smoke: source −14.75 → output −14.73 (linear), ebur128
orig −14.7 / dub −14.7. The mono stream → `-ac 2` upmix keeps the loudness (swr's
−3 dB centre gain). Tests: `scripts/tests/test_dub_worker_loudness.py` (pure helpers
+ the 3-call orchestration with `_run_stderr` faked; no ffmpeg in eval-checks).

## Re-dub a video (e.g. the video 344 acceptance after deploy)

`PATCH /api/v1/videos/344/dub` with body `{"requested": true}` (204):
1. `models_dabing::set_dub_requested(true)` sets `dub_status='queued'`, bumps
   `dub_requested_at` (the top of the newest-first queue) and sets
   `stem_manual_priority=1` (harmless).
2. The dub worker (10 s tick) picks it up (`get_next_dub_job` selects
   `dub_status NOT IN ('none','ready')`), runs `mark_dub_synth` and ONE full
   continuous session over the whole video (a 36-min talk = ~36 min + drain;
   there is no partial reuse). The old `chunk_*` files are removed first.
3. `_assemble_dub` (round F), the transcripts JSON + subtitles store, then
   `mark_dub_ready`.

Before it: deploy (the worker re-materialises the four embedded scripts via
`ensure_script`), and CHECK `GET /api/v1/settings` — the Nastavenia form saves
`dub_voice` on every save, so a box that saved settings during round C still holds
`dub_voice=Charon` (a pinned voice). Set `{"dub_voice": "speaker"}` (or pick
`Hlas rečníka` in Nastavenia) for the frozen speaker-voice state. Verify in
`<cache>/<youtube_id>_dub/`: `session_summary.json` (connections, reconnects 0
failures, output/input 0.98–1.02, max voiced gap, latency, EN latency),
`loudness.json`; `<cache>/<base>_dub_events.jsonl` next to the dub; then an
ebur128 of the new `<base>_dub.flac` against the vocals stem on the same slice
(±1 LU, round F) and the stored EN/SK subtitle line count.

**Pull the event log for offline timing analysis (never ssh, never
`FileDownload` — it base64s into the transcript).** Via the win-resolume MCP
`Shell` (PowerShell), serve the cache dir on the box's LAN address, detached:

```powershell
$py = 'C:\ProgramData\SongPlayer\cache\tools\lyrics_venv\Scripts\python.exe'
$p = Start-Process -FilePath $py -WindowStyle Hidden -PassThru `
  -WorkingDirectory 'C:\ProgramData\SongPlayer\cache' `
  -ArgumentList @('-m', 'http.server', '8931', '--bind', '10.77.9.201')
$p.Id   # note it
```

From dev1: `curl -fo 344_dub_events.jsonl "http://10.77.9.201:8931/<url-encoded
base>_dub_events.jsonl"` (the base has spaces — URL-encode it). Then STOP that
server on the box: `Stop-Process -Id <pid>` (only the pid you started — the cache
dir must not stay served). t0 in the log = the `send_start` event's `t`; each
`input_transcription` / `output_transcription` event's `t` is its arrival on the
same clock.
