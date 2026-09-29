---
paths:
  - "crates/sp-server/src/downloader/cache*.rs"
  - "crates/sp-server/src/reprocess/**"
  - "crates/sp-server/src/startup.rs"
  - "crates/sp-server/src/song_relink*.rs"
  - "crates/sp-server/src/stems/mod.rs"
  - "crates/sp-server/src/stems/worker.rs"
  - "crates/sp-server/src/dabing/worker.rs"
  - "crates/sp-server/src/lyrics/idle_gate_abort.rs"
  - "crates/sp-server/src/db/models_stems*.rs"
  - "crates/sp-server/tests/startup_migration.rs"
---

# A song's files are ONE set, named after its audio (#136)

On 29.9.2026 the metadata repair renamed ~99 songs' video + audio with two
independent renames. It left their stems under the old `_gf` name. Every
consumer derives the stem names from the CURRENT audio, so:

- the lyrics queue waited for stems forever (`WaitForStems`: `stem_status` was
  `done`, so the stem worker never re-ran them);
- the stem mixer played the plain mix;
- `stems_state` still read `ready` for all 344 songs.

Design record: #136 comment 5894034820.

## The set

- A song = the video + the audio (the DB's `file_path` / `audio_file_path`)
  plus every file NAMED AFTER the audio: `downloader::cache::derived_files(audio)`
  = `[vocals, instrumental, dub, dub transcripts]` (`stems::stem_paths`,
  `stems::dub_path`, `stems::dub_transcripts_path`).
- These consumers derive those names from the CURRENT audio path:
  - the stem / dub mixer (`stems/reader.rs::open_audio_stream`);
  - the lyrics isolation (`idle_gate_abort::resolve_isolation_input`);
  - `StemsState` (`models_stems::stems_on_disk`).
- The recorded `vocals_file_path` / `instrumental_file_path` / `dub_file_path`
  columns are a record kept in sync with every move. The dub worker's input
  choice (`dabing/worker.rs::dub_input_audio`) and the dashboard read them.
- A new file named after the audio goes into `derived_files`. The rename and
  the re-link then follow it.

## Rules

- **Rename a song only through `cache::rename_song_files(id, &old, &new)`**,
  never a hand-written `fs::rename` of one sidecar.
  - It moves derived → audio → video as ONE unit (`move_as_unit`).
  - A failed move (Windows refuses to rename a file another process holds open
    without share-delete) moves every file already moved back and returns the
    OLD set. The exception is a file whose move back ALSO failed
    (`MoveFailed::stuck`). A stuck audio is returned at its NEW name
    (`in_effect_after_failure`), so the row records where the file IS. The video
    moves last, so it is never stuck. A stuck stems pair or dub is re-linked at
    the next start; a lone stuck stem resets the row. A lone stuck
    `dub_transcripts.json`, where the dub moved back and its transcripts did
    not, is NOT re-linked, because the dub already sits under the audio's name.
    That dub plays, but it has no subtitles until it is dubbed again. It takes
    a double failure.
  - Record `SongFiles::columns()` of the set it RETURNS, on EVERY row that
    recorded the old set: the same video in another playlist is a second row
    pointing at the same files.
  - Read the old set from the DB right before the move, never from a batch
    snapshot (an earlier row may have moved it).
  - Hold `cache::SONG_FILES` (a process-wide async lock) from that read to the
    DB record. The re-link holds it from reading its rows to its last record,
    so a rename and a re-link never interleave on the same song. Tests prove it
    with two tasks: hold the lock, spawn the other side, show it waits.
- **A job that writes derived files re-links its song when it finishes.** The
  stem worker runs `song_relink::relink_song` after `mark_stems_done`
  (`record_stem_result`), and the dub worker after `mark_dub_ready`
  (`record_dub_ready`). A job writes under the name its song had
  when it STARTED, so a rename while it ran would strand the output.
  `relink_song(…, written_for)` takes that start-time audio. When the song was
  renamed, the rename already carried the song's OLDER files to the new name,
  so the job's own output is the fresh copy: it moves over them first, as one
  unit, and then the normal pass runs. Without that, a re-dub left its new dub
  stranded while the old one played (review round 4).
- **Delete a superseded download through `remove_duplicates`.**
  - It removes the video, the audio and the stems (re-separated on their own).
  - It KEEPS the dub + transcripts. The re-link adopts them under the kept
    song's name (same YouTube id, same audio). A dub is operator-requested and
    nothing re-runs it.
- **`startup::self_heal_cache` never deletes an orphan half-sidecar a row
  records.** That half belongs to a song split across two names (a move-back
  that failed); it is kept and WARNed.
- **Re-link (`song_relink`: `relink_derived_files` at startup, `relink_song`
  after a job)** runs after the pair re-link and the duplicate removal. It
  skips a row whose audio is missing.
  - `done` stems missing under the audio's name come from the old name with the
    NEWEST stems pair holding BOTH stems, never a pair mixed from two names;
    else the row is reset to pending (`models_stems::requeue_lost_stems` = the
    `enqueue_stems` reset + the recorded paths cleared).
  - A `ready` dub comes from the old name with the newest dub, with its
    transcripts. A dub no name holds is WARNed + counted, never reset.
  - Each unit is ranked by its OWN files' age; `derived_file_owners` only lists
    names, in path order.
  - A failed move leaves the row for the next pass. The columns are written
    only when they differ. The cache is scanned (`derived_file_owners`) at
    most once per pass, and only when a unit is missing.
  - Its WARNs start with `re-link:`, because it runs after jobs too. The
    startup pass's INFO count line starts with `self-heal:`.
  - Leftovers are never deleted by inference: an old stems pair not chosen,
    or a superseded dub the kept row did not adopt, stays on disk. It costs
    disk space, never correctness, and deleting by name could destroy the only
    copy of an operator-requested dub.

## Box verification (after a deploy that runs the self-heal)

- Startup log, one INFO line: `self-heal: re-linked the stems / dub left under
  an old name stems_relinked=N stems_reset=N dubs_relinked=N dubs_missing=N`.
- Each file moved is logged as `cache: moved a song file from=… to=…`.
- Tripwire WARN: `lyrics: stems are recorded done but the vocals file is
  missing under the audio's name`. After the fix it should never fire; if it
  does, the song's files drifted in a way neither the rename nor the post-job
  re-link covered. Grep that song's id in the re-link lines.

## Tests

- A stems-`ready` fixture must write the REAL stem files where
  `stem_paths(audio_file_path)` derives them
  (`db::models_stems::fixtures::give_real_stems`). A `done` row with made-up
  recorded paths IS this bug.
- A rename that fails on both Linux and Windows: a NON-EMPTY directory at the
  target path.
- A regex-matched sidecar name needs TWO name components before the id
  (`{song}_{artist}_{id}_normalized…`). A test file named `Debris_{id}_…` is
  not a sidecar at all, so the scan ignores it.
- `downloader/` is excluded from the mutation gate (substring
  `sp-server/src/downloader/`). `cache.rs` logic is still unit-tested
  (`cache_tests_files.rs`), while the DB-driving pass lives in the gated
  `song_relink.rs`.
