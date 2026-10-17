---
paths:
  - "crates/sp-server/src/cache_sweep*.rs"
  - "crates/sp-server/src/data_size*.rs"
  - "crates/sp-server/src/log_rotate*.rs"
  - "crates/sp-server/src/panic_hook.rs"
  - "crates/sp-server/src/downloader/cache.rs"
  - "crates/sp-server/src/api/routes_import*.rs"
  - "crates/sp-server/src/api/routes_status_tests.rs"
  - "crates/sp-core/src/data_size*.rs"
  - "sp-ui/src/components/data_size_line.rs"
  - "e2e/settings-data-size.spec.ts"
---

# No infinite archive: the files (#261 lane 2)

The owner (13.10.2026, on #260): "nema sa budovat nekonecny archiv!".
Design: #261 comment 6228880917 items 6-8 (lane 1, the database, is in
`playlist-membership.md`); how lane 2 reads it: the `Anchors-confirmed`
comment 6230045110.

## The daily cache sweep (`cache_sweep.rs`, `cache_sweep_plan.rs`)

- **When:** `crate::daily_pass` (5 min after a start, then daily; a held
  pass an hour later), `Job::CacheSweep` ("cache_sweep" / "čistenie
  cache") behind the #230 background hold. Spawned in `lib.rs` with the
  config's cache dir and data dir.
- **Under `cache::SONG_FILES` from reading the rows, through the listing,
  to the last delete** (`pass`): a unit move, a re-link, the retention, a
  sync's records and a download's never land between what the pass reads
  and what it deletes. Review round 2 put the listing back under the lock:
  listed outside it, a unit move caught half-way (its `<name>` and
  `<name>.replaced` both there, then a failed give-back) let the pass
  delete the only copy as `replaced`. The listing and the deletes run on
  the blocking pool; the pass's log line names how long it held the lock
  (`held_ms`).
- **A database with no row at all deletes no file of a video**
  (`RowFacts::is_empty`, one WARN `cache sweep: the database has no row`):
  a fresh or replaced database would make every song a video no row has.
  The rules that need no row (stale names, parts, …) still run.
- **The rows:** every row's `youtube_id` and its five recording columns
  (`cache::RECORDING_COLUMNS`: `file_path`, `audio_file_path`,
  `vocals_file_path`, `instrumental_file_path`, `dub_file_path`; the same
  list `cache::recorded_by_a_row` reads), read once (`row_facts` →
  `RowFacts`: ids, recorded file NAMES and the song bases they name, all
  lowercased: NTFS ignores case, and a match only KEEPS more).
- **The listing** (`list`): the cache dir's FILES, its jobs' work DIRS
  (`{id}_isolation`, `{id}_stemsep`, `{id}_dub`: walked for their tree's
  bytes and newest write, `data_size::walk_exact` (each entry's own
  record: a file a job still holds open reads as it is now, where NTFS's
  listing may lag, `an_exact_walk_reads_a_file_still_open_for_writing`
  logs both readings on the Windows job), only when the plan could take
  them (`RowFacts::may_take`: a video no row has); a row's video's work dir
  is listed with no time, never walked; an unreadable part of a tree = no
  time = never deleted), and `<cache>/peer/`'s files (only when `peer` is
  a real dir, never a symlink or junction; its name is the exchange's
  `peer::fetch::PARTS_DIR`). No other dir is walked
  (`tools/` with the lyrics venv, `peer/` itself, `.cli-proxy-api/` with
  CLIProxyAPI's OAuth tokens). Each entry's time is its own record
  (`std::fs::symlink_metadata`: never a symlink's target, and never the
  directory index, which NTFS updates lazily for an open file such as a
  part a fetch is writing). A symlink (a junction on Windows) is never
  listed, so never followed or deleted.
- **The decision** (`plan`, pure): an entry goes only when it is KNOWN and
  old enough, by its last write (mtime; a rename keeps it):
  - `no_row` — named after a video no row has, recorded by no row, 7 d:
    a song file by its exact shape `{song}_{artist}_{id}_normalized[_gf]…`
    (`SONG_RE`: sanitized song / artist carry no `_`, so the id is the 11
    characters before `_normalized`), an id-first file of a known kind
    (`ID_FILE_RE`: `_lyrics.json`, `_vocals16k.wav`,
    `_g35t_words[_used].json`, `_g35t_audit.json`,
    `_alignment_audit.json`, `_title_search_audit.json`, the cleanup
    caches, `_description.txt`, `_description_lyrics.json`,
    `_mtl_{text,out}.json`, the download / upgrade / repair temps, the
    retired outputs below), or a work dir (`WORK_DIR_RE`). NEVER a generic
    `{11 chars}_…` match: a song named `HolySpirit1_…` would read as a
    video id and lose its files. A song's whole set goes this way once its
    rows are gone (a playlist DELETE, #260's retention): video, audio,
    stems, dub, transcripts, `_dub_events.jsonl`, its `.prev` /
    `.replaced` / a job's `.tmp`.
  - `stale` — a name no code writes any more, whatever its video, 7 d: the
    retired outputs (`RETIRED_KINDS`: `_whisperx_track.json`,
    `_descmerge_audit.json`, `_asr_path_audit.json`,
    `_gemini_chunks.json`, `_chunks.json`, `_asr_output.json`,
    `_align_output.json`, `_align_out.json`, `_vocals_dereverbed.wav`,
    from git history) and the cleanup caches older than `_cleaned_v3`
    (`CLEANED_VERSION`; unversioned = older). A newer version is kept (a
    rollback must not eat a newer build's caches).
  - `replaced` — a `<name>.replaced` / `<name>.N.replaced` of a SONG file
    (`<name>` matches `SONG_RE`: a set-aside only ever comes from a song's
    unit move) 30 d, only while
    `<name>` holds a file: it is then the stale copy the unit move meant to
    delete (a stuck move-back, a failed delete, a crash after the rename).
    With `<name>` empty it may be the ONLY copy of a song file: it stays,
    and the self-heal WARNs it at every start (`song-files.md`).
  - `dub_prev` — a dub's `<base>_dub.flac.prev` (`dub_worker.py
    trim-tail`) 14 d after the LATER of its own and its dub's write
    (`shutil.copy2` keeps the original dub's time; the cut dub is written
    at the trim), only while the dub is there.
  - `part` — a `<cache>/peer/*.part` 7 d (a fetch resumes from a younger
    one; a part being written is fresh).
  - A file a row records is never deleted, whatever its name says.
  - At most 500 entries a pass (`MAX_PER_PASS`), the oldest first; `left`
    counts the rest, taken the next day.
- **The deletes** (`execute`): only a name the listing read, one plain
  path component (`is_plain_name`), in the cache dir or `peer/`; a dir
  through `remove_dir_all` (it removes a symlink inside, never its
  target); a file already gone is skipped; a failure is a WARN and
  counted, the pass goes on.
- **Logs:** a DEBUG `cache sweep: deleted` per entry (`path`, `why`,
  `bytes`; `sp_server=debug` is on in the box's log); one INFO `cache
  sweep: deleted what no row needs any more (#261)` with `files`, `dirs`,
  `bytes`, `no_row`, `stale`, `replaced`, `dub_prev`, `parts`, `failed`,
  `left`, `held_ms` (DEBUG `nothing to delete`, with `held_ms`); a WARN
  when the pass failed.
- **Untouched rules:** the startup self-heal (#136), #260's retention and
  the video upgrade's `.prev` retention keep their own rules; the sweep
  only adds what nothing deleted. An old stems pair or a superseded dub of
  a video that HAS a row stays (`song-files.md` "Leftovers are never
  deleted by inference").
- **Bumping a lyrics cleanup cache** (`_cleaned_vN` in `lyrics/gather.rs` /
  `lyrics/title_search.rs`): bump `CLEANED_VERSION` with it, or
  `the_cleaned_version_is_the_one_the_lyrics_code_writes` fails. A new
  per-video file kind written into the cache goes into `ID_FILE_RE` (or
  carries the song base), or the sweep never removes it.

## The data dir's size (`data_size.rs`, `sp_core::data_size`)

- After each sweep (`cache_sweep::run`, held with it): `measure(data_dir,
  cache_dir)` walks both (symlinks never followed; the cache walked once
  and counted into the data dir when it lies in it), kept in the
  process-wide store (`current` / `set`), one INFO `data dir: measured`.
- `GET /api/v1/status`: `data_dir_bytes`, `cache_bytes`,
  `data_dir_measured_ms` at its top level (`StatusResponse.data_size`,
  `serde(flatten)`), null until the first measure.
- Nastavenia (`data_size_line.rs`, `data-size-line`), under #256's usage
  line, read once: `Dáta na disku: 123,4 GB (cache 120,0 GB), merané
  17.10. 14:05` (`size_sk`: whole MiB under a GiB, else tenths of a GiB,
  Windows' units; the time in the browser's zone) or `… ešte nezmerané
  (meria sa pri dennom čistení cache)` (5 min after a start, later while
  the #230 hold holds the pass). Not on the HealthBar (read rarely).

## Logs SongPlayer writes on its own (`log_rotate.rs`)

- `ROTATE_AT_BYTES` = 10 MiB: `rotate_if_over` renames the log to
  `<name>.1` (an older `.1` replaced). Each log keeps at most ~20 MiB.
- `songplayer-panic.log` / `sp-audio-host-panic.log`: at the hook's install
  (its INFO line says `rotated`) and before each record
  (`panic_hook::write_crash_record_at`), one record at a time (`WRITING`:
  two threads panicking at the limit would both rotate, the second moving
  the first's fresh record over the full `.1`).
- `sp-audio-host.log`: a `RotatingFile` (`audio_host/host_win.rs`), at the
  open and before every write that finds it full, so during a run too.
- A rotation that fails never loses a line (the same file goes on); a
  `RotatingFile` tries again once the file grew by another sixteenth of
  the limit (`retry_at`), never on every line; every (re)open starts that
  spacing afresh, so a fresh file after a failed reopen rotates at its own
  limit. `log_rotate.rs` is on the host's no-spawn scan.
- The `WRITING` test (`a_crash_record_waits_for_the_one_being_written`)
  runs nothing that can panic while it holds the lock (the writer thread is
  spawned first and waits for a go): a panic there would wait for the lock
  in an installed hook, a hung test binary instead of a failed test.
- The daily `songplayer.<date>.log` keep their own 14-file bound.

## The import into an inactive playlist (`api/routes_import.rs`)

`upsert_imported` (the import core's DB half, its test seam): a NEW row
takes `removed_at` = its playlist's `deactivated_at` (NULL while active and
for the test item's playlist), lane 1's own mark, so the reactivation
clears it and #260's clock counts it; an existing row keeps its mark and
takes the title. One statement. `routes_import_tests.rs` turns the
playlist off and on through the real `PUT /api/v1/playlists/{id}`. The
RED commit's helper (ea936b21) held the one-connection test pool while it
queried it again (a 30 s pool timeout, red for the wrong reason); review
round 1 moved it onto the real route. With that helper the old SQL fails
exactly the two mark tests, the new one none (modelled on the real
schema with Python sqlite3).

## Box checks (MAIN SESSION OPS)

- Before the first deploy that carries the sweep, dry-run count on each
  box (SNV, PP) what it would delete: the listing's names against the
  shapes above and `GET /api/v1/playlists` + the videos' ids (no row = a
  candidate), with their mtime; anything unexpected = stop the deploy.
- 5 min after the start: the INFO `cache sweep: deleted …` (or the DEBUG
  `nothing to delete`) with its `held_ms` (how long the song-file lock was
  held: expect seconds at most), then `data dir: measured`;
  `GET /api/v1/status` `data_dir_bytes` not null; Nastavenia's line.

## Tests

`cache_sweep_plan_tests.rs` (every shape, rule and edge at exactly N days
and a millisecond under, the bound and the order; pins derived from a
scratch Python model of the plan), `cache_sweep_tests.rs` (a whole pass on
real files and an in-memory DB, the lock, the listing, symlinks (unix),
failed and foreign deletes, every recording column), `data_size_tests.rs`,
`log_rotate_tests.rs`, `panic_hook.rs`'s install test, sp-core
`data_size_tests.rs`, `api/routes_status_tests.rs`, `routes_import_tests.rs`,
`e2e/settings-data-size.spec.ts` (mock: `/__mock/data-size {…}`,
`/__mock/data-size-reset`).
