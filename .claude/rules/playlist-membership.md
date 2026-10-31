---
paths:
  - "crates/sp-server/src/playlist/mod.rs"
  - "crates/sp-server/src/playlist/membership*.rs"
  - "crates/sp-server/src/playlist/retention*.rs"
  - "crates/sp-server/src/playlist/selector.rs"
  - "crates/sp-server/src/playlist/loop_memory*.rs"
  - "crates/sp-server/src/db/models_membership*.rs"
  - "crates/sp-server/src/db/mod_tests_v40.rs"
  - "crates/sp-server/src/db/mod_tests_v43.rs"
  - "crates/sp-server/src/db/models_set_list*.rs"
  - "crates/sp-server/src/playlist/history_bound*.rs"
  - "crates/sp-server/src/api/routes_tests_retention.rs"
---

# A song that left its YouTube playlist (#260)

Owner, 13.10.2026: "sem tam davam z playlistov odobrat alebo pridat
pesnicky … mam pocit ze vidim aj po tyzdni hrat tu istu piesen". The sync
only ever UPSERTED what yt-dlp listed, so a song taken out of the YouTube
playlist kept its row and kept being picked: ytfast had 114 rows on SNV and
PP, its YouTube playlist 108 (the 6 included `TwzfEwsTfag` "Never Lost",
on the wall that morning). ROZHODNUTÉ (issue comment, same day): marked at
once, kept a year, then deleted with its files.

## The mark (V40 `videos.removed_at`, UTC ms; NULL = in the playlist)

- `playlist::membership::record_fetch` (called by `sync_playlist` after a
  successful yt-dlp run) under `cache::SONG_FILES`:
  - reads the playlist's rows (`models_membership::members`), diffs them
    against the fetched ids (`membership::diff`, pure): a row not marked
    and not listed is MARKED at the sync's time; a marked row listed again
    is BACK;
  - upserts every entry; `upsert_entry`'s UPDATE clears `removed_at` (the
    bring-back is that clear);
  - marks with `models_membership::mark_removed`, which never moves an
    existing mark (`AND removed_at IS NULL`): the year counts from the day
    the song LEFT, however many syncs miss it after.
- **An empty fetch marks nothing** (one WARN `playlist sync: yt-dlp listed
  no video - nothing is marked as removed`): yt-dlp can answer success
  with no entry on a glitch, and marking everything would empty the
  playlist. A failed yt-dlp run never reaches `record_fetch` at all.
- The mark is per ROW (`playlist_id`, `youtube_id`): the same video in
  another playlist is another row and stays in it.
- One INFO per sync, the existing `playlist sync complete` (lib.rs) with
  `new_count`, `marked`, `back` and the songs by name (`removed=` /
  `returned=`, `membership::names`: `<youtube id> <title>; …`, `-` for
  none).

## What a marked row is NOT any more

- **Picked:** the youtube selector's queries (`get_unplayed_normalized_video_ids`,
  `get_normalized_video_ids`, so the Continuous / Single pick AND its
  rotation restart), `get_unplayed_video_ids`, Loop's remembered song
  after a restart (`loop_memory::remembered`), a play-video by id
  (`models_membership::video_playlist_membership` → 404 "video not in
  playlist", the engine told nothing).
- **Listed:** `GET /api/v1/playlists/{id}/videos` (`get_videos_for_playlist`;
  its only production caller). The UI needs no field: the row is simply
  not there.
- **Processed:** every worker queue and its dashboard count splices ONE
  fragment, `crate::db::queue_row!()` (db/mod.rs) = `test_item::not_test_item!`
  + `removed_at IS NULL`: download (`DOWNLOAD_DUE`), stems
  (`get_next_video_for_stems`, `STEM_ELIGIBLE_PRED` for every tier,
  `count_stems_progress`), lyrics (`queue_sql::lyrics_eligible!`, every
  bucket; `api::lyrics::fetch_queue_counts`, all three counts), dub
  (`get_next_dub_job`), metadata repair (`REPAIR_QUEUE_WHERE`, so also
  `status.metadata.failed_videos`), video upgrade (`next_song`, `counts`).
  The exchange's queued announcements read the same predicates
  (`peer::queued`), so they follow. A NEW worker queue splices
  `crate::db::queue_row!()`, never `not_test_item!` alone. The translation,
  missing-translation and register passes (served tracks, paid AI) carry
  their own `v.removed_at IS NULL`. Nothing finished is undone: files,
  stems, lyrics, dub stay.

## What the mark does NOT touch (decided, keep it so)

- **A song playing when its row is marked plays to its end.** The mark acts
  only at the next pick; nothing stops a pipeline.
- **Loop keeps the song it holds in memory.** `PlayAction::ReplayCurrent`
  (Loop at a song's end) and the selector's Loop branch with a current song
  are not picks: an operator's loop goes on until the mode or the song
  changes. Only after a restart is the remembered song checked.
- **A custom playlist's set list** (`playlist_items`, ytlive) is the
  operator's explicit choice: its items play whatever their YouTube
  playlist did, Loop's remembered item included
  (`a_custom_set_list_keeps_a_song_that_left_its_youtube_playlist`), and
  the retention never deletes a row a set list holds.
- **The exchange's catalog** still serves a marked row's files to a peer
  (it has them); only its QUEUED jobs follow the queues.

## The retention (`playlist::retention`, a year later)

- `REMOVED_RETENTION_DAYS` = 365 (the owner may ask to change it).
  `cutoff_ms(now) = now − 365 d`; a row is DUE when `removed_at <=` it and
  no `playlist_items` row holds it (`models_membership::due_ids`, oldest
  mark first, at most `RETENTION_BATCH` = 50 per pass). Edge pinned: 365 d
  exactly goes, 365 d − 1 ms and 364 d 23 h stay.
- `run` (spawned in lib.rs next to the wall): the first pass 5 min after
  the start, then every 24 h; held by the #230 background hold as
  `Job::Retention` ("retention" / "mazanie odobratých skladieb"), a held
  pass retried 1 h later: the shared daily timer `crate::daily_pass`
  (`daily_pass::next_wait`), the #256 usage-log prune's too. A failed pass
  is one WARN.
- Per row, under `cache::SONG_FILES` (the sync writes under it too, so a
  song brought back at that moment is never deleted):
  1. `due_row` reads it again (gone or brought back → nothing);
  2. its files: `candidates` = video, audio, then `cache::derived_files`
     (stems, dub, dub transcripts); `plan_files` (pure) keeps every file
     another row records in ANY path column (`recorded_elsewhere`: the
     same video in another playlist shares the files by name, #136) and
     every file named after a kept audio;
  3. the files are deleted in that order, the video first (a player may
     hold it): a file already gone is skipped, any other failure is one
     WARN and the row STAYS for the next pass (nothing after it deleted);
  4. `delete_row`, one transaction: the row (its `play_history` goes by the
     foreign key's cascade), the `peer_waits` / `peer_fetches` /
     `peer_standins` of its video once NO row of the video is left, the
     `peer_hashes` of the files it deleted;
  5. one INFO `playlist retention: deleted a song a year after it left its
     YouTube playlist` with `youtube_id`, `title`, `removed` and `kept`.
- Not deleted HERE: the video's `{yt}_lyrics.json`, its g35t transcript
  caches and a dub's `<base>_dub_events.jsonl` (per YouTube id or not in
  the song's set); #261 lane 2's daily cache sweep deletes them 7 days
  after their last write once no row of the video is left
  (`cache-sweep.md`). A video-upgrade `.prev` is the upgrade retention's
  (an orphan `.prev` goes at its next sweep).
- Known limit: a NEW row of the same video downloading into the shared
  names at the very moment its marked sibling is deleted (a new playlist
  added the video that day) can lose the fresh file before the download
  records it — the same class as `song-files.md`'s "normalize writes
  straight into the shared name".

## Tests

- `db/mod_tests_v40.rs`: the column (NULL for old rows) and the partial
  index.
- `db/models_membership_tests.rs`: members, the per-row mark, the first
  mark kept, a play by id refused.
- `playlist/membership_tests.rs`: `diff` (all four cases in one table),
  `names`, `record_fetch` (marked / back / new, first mark kept, an empty
  fetch, the `SONG_FILES` lock held across the writes).
- `playlist/membership_tests_play.rs`: the picks (30 picks out of three,
  the rotation restart, Loop after a restart), the custom set list, the
  dashboard list and a play by id through the real router.
- `playlist/membership_tests_queues.rs`: every queue and count, modelled on
  `test_item_tests_queues.rs` (the marked row first, then a listed one).
- `playlist/retention_tests.rs`: the pure plan (every keep / remove case),
  the edge, the shared file, the last row's own data, the brought-back
  song, the batch bound, a pass with nothing due, a set list, a file that
  cannot be deleted (a non-empty directory in its place: fails on Linux
  and Windows).
- `playlist/mod.rs` is out of the mutation gate (it shells out to yt-dlp),
  so the logic lives in the gated `membership.rs` / `retention.rs`.

## No infinite archive: deactivation, the play history, a DELETE (#261 lane 1)

The owner's rule (13.10.2026, on #260): "nema sa budovat nekonecny
archiv!". Design: #261 comment 6228880917; the lane's notes: the
`Anchors-confirmed` comment 6228996578.

- **A deactivated playlist starts the clock.** `PUT /api/v1/playlists/{id}`
  with `is_active` writes the row and its marks in ONE transaction under
  `cache::SONG_FILES` (`routes::update_playlist`, `follow_activation`):
  - `false` → `models_membership::deactivate`: the playlist keeps its FIRST
    V43 `playlists.deactivated_at` (UTC ms; NULL = active), and every row
    not marked yet gets exactly that value as `removed_at` (a sync's mark
    stays); never the test item's playlist (kind `test`, pinned clips);
  - `true` → `reactivate`: clears exactly the rows with `removed_at =
    deactivated_at` (every deactivation mark carries that very value, so a
    sync's mark never equals it, even after a clock step), then the column.
    A song the sync marked before the deactivation stays marked (cleared,
    it would be picked again until the next sync, and a sync during a #230
    hold is dropped).
  - The song-file lock is released with the transaction, before the engine
    is told (`EnsurePipeline` / `RemovePipeline`).
  - One INFO `playlist activation: its songs' marks follow it (#261)`
    (`active`, `songs` = rows marked / cleared).
  - V43 back-filled every playlist inactive at the migration (its
    `deactivated_at` and the marks = the migration's time).
  - So an inactive playlist's songs are deleted a year later by the
    retention below, with their files (a set list's item never), and
    `GET /api/v1/playlists/{id}/videos` of an inactive playlist lists
    nothing (its rows are marked). A video IMPORTED into a playlist while
    it is inactive (`POST /api/v1/videos/import` with its id, the Dabing
    import) is marked at its insert like the playlist's other songs (#261
    lane 2, RED/GREEN: `routes_import::upsert_imported`, the import core's
    DB half, gives a NEW row `removed_at` = the playlist's
    `deactivated_at`, so the reactivation clears it; an existing row keeps
    its mark; `routes_import_tests.rs`).
  - A transaction that reads before it writes (`delete_playlist`,
    `append_playlist_item`) begins `IMMEDIATE`: in WAL a deferred one whose
    snapshot went stale fails its first write at once, with no busy wait.
- **An inactive playlist's sync records nothing** (`record_fetch`, one WARN
  `playlist sync: the playlist is inactive - nothing is recorded`): a
  manual `POST /playlists/{id}/sync` would clear the deactivation's marks
  (`upsert_entry`) and add rows no clock counts. The periodic and the
  startup syncs take active playlists only.
- **The stems queue and the video upgrade take only rows of active
  playlists** (`db::active_playlist!()`: `STEM_ELIGIBLE_PRED` (every tier,
  the queue position, the exchange's queued stems),
  `get_next_video_for_stems`, `count_stems_progress`, the upgrade's
  `counts`; `next_song` asks `p.is_active = 1`), as the download and lyrics
  queues already did. A NEW worker queue splices both fragments.
- **The rotation counts only the playlist's OWN plays** (RED/GREEN):
  `get_unplayed_normalized_video_ids` / `get_unplayed_video_ids` join
  `ph.playlist_id = v.playlist_id`. A song played on ytlive's set list used
  to read as played at home for good (the home restart clears only its own
  plays). V43 `idx_play_history_video (video_id, playlist_id)`.
- **`play_history` is bounded** (`playlist::history_bound`, the shared daily
  timer, `Job::PlayHistory` "mazanie starej histórie prehrávania"):
  - one row per (playlist, video): the newer `played_at` stays, a tie by
    the later `id`;
  - no row older than `KEEP_DAYS` (30) except a youtube / dabing
    playlist's play of its OWN song (its current rotation; bounded by its
    size). `cutoff_s` is in whole seconds (`played_at` is
    `datetime('now')`, a row at the cutoff second stays);
  - the readers keep their answers: the rotation reads whether a song was
    played, the stems tier 2 (`stems_recent_days`, default 7; a setting
    over 30 sees at most 30 days of a custom playlist's plays) and the
    upgrade's `.prev` retention (14 days) the newest play;
  - one INFO (`duplicates`, `old`) when it deleted something.
- **A playlist's DELETE numbers the set lists again** (RED/GREEN):
  `models_set_list::delete_playlist`, one transaction: the row (its songs,
  plays and items by the cascades), then `compact` of every OTHER custom
  playlist that held one of its songs. `compact` is the set-list removal's
  too (`models::remove_playlist_item`): positions `0..N-1` in order, the
  current position follows its item (`shifted_current`: the same item; a
  removed current one → the item before the one that followed it, so the
  next pick is that one; none before it → the last item, the next pick
  wraps). A hole used to make `select_next_custom` wrap to 0 and skip
  every item after it. Accepted: a restart right after the CURRENT item
  was taken out resumes at the current position and plays the item before
  it once more (the live pick is exact).
- **The exchange's records of a video no row has go daily**
  (`peer::leftovers`, `peer-exchange.md`); V43's `idx_videos_youtube`
  keeps its lookups (and the exchange's per-video reads) off a scan.
- **V43 dropped `resolume_clip_mappings`** (no code ever wrote it).
- Tests: `history_bound_tests.rs`, `models_set_list_tests.rs`,
  `mod_tests_v43.rs`, `api/routes_tests_retention.rs` (the DELETE through
  the router and the selector after it, a 404, the activation marks, the
  test item, the lock), `membership_tests_inactive.rs` (every stems entry
  point and the upgrade with the inactive row UNMARKED, so the playlist
  decides), `membership_tests.rs::a_fetch_of_an_inactive_playlist_records_nothing`,
  `selector.rs::a_play_on_another_playlist_keeps_the_song_in_its_home_rotation`.
- A test that holds `cache::SONG_FILES` must not take `MODE_ORDER` while
  it holds it: a parallel test's activation PUT holds `MODE_ORDER` while it
  waits for the song-file lock (a deadlock until a timeout).

## Box acceptance (MAIN SESSION OPS, after the deploy)

#261 lane 1 (V43 runs at SongPlayer's first start on a build that has
it: on SNV that is the DEPLOY OF THE DEV PUSH that integrates this lane
(`ci.yml` deploys every dev push there), on PP the release that reaches
it):

- BEFORE that push (SNV) / that release (PP), list the box's inactive
  playlists (`GET /api/v1/playlists` → `is_active: false`, each with its
  kind and its row count): V43 marks their rows at that start. Tell the
  owner, by name, that their songs then leave every worker queue and the
  dashboard list and are deleted with their files a year later unless the
  playlist is turned on again before then (a set list's item never). A
  `dabing` playlist among them would take its imported videos with it: ask
  the owner before the build reaches that box.
- A set list holed by a playlist DELETE BEFORE this lane stays holed (the
  numbering runs at a removal or a DELETE): read ytlive's
  `GET /api/v1/playlists/{id}/items`; positions must be `0..N-1`. On a gap,
  remove its last item and add it back (`DELETE` then `POST
  /api/v1/playlists/{id}/items`): the removal numbers the list again, the
  add appends the item where it was.
- 5 min after the start (then daily): the INFO lines `play history:
  deleted …` and `exchange leftovers: deleted …`, or their DEBUG
  "nothing to delete".

#260:

On SNV and PP, after the next sync of ytfast (playlist 7): its
`playlist sync complete` line reads `marked=6` and names the six ids;
`GET /api/v1/playlists/7/videos` lists 108 songs; none of the six is
picked again. A song put back in the YouTube playlist logs `back=1` at the
next sync and plays again.
