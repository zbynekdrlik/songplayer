---
paths:
  - "crates/sp-core/src/song_block*.rs"
  - "crates/sp-server/src/db/models_blocked*.rs"
  - "crates/sp-server/src/db/mod_tests_v44.rs"
  - "crates/sp-server/src/api/blocked*.rs"
  - "crates/sp-server/src/playlist/loop_memory.rs"
  - "crates/sp-server/src/playlist/selector.rs"
  - "sp-ui/src/pages/block.rs"
  - "e2e/block-songs.spec.ts"
  - "e2e/post-deploy-block-songs.spec.ts"
  - "e2e/block-gate*.ts"
---

# A song blocked on one node (#265)

The owner (13.10.2026): at PP they did not like a song and wanted it never
played there again; the easiest way in, from the room, is a short address
whose page blocks the song shown now or one found by the wall's title.
Design: #265 comment 6814953166; the lane's anchors: comment 6814998873.

## The block (V44 `blocked_songs`)

- `blocked_songs(youtube_id TEXT PRIMARY KEY, blocked_at_ms, title)`: by
  YouTube id, so every playlist's row of the song at once. Per NODE: the
  node exchange carries no blocks (PP's taste is PP's).
- **ONE "never pick" predicate**, `db::pickable_row!()` (`db/mod.rs`,
  qualified `v.`) = #260's `v.removed_at IS NULL` AND no `blocked_songs`
  row of `v.youtube_id`. It is spliced into exactly the automatic picks:
  - the youtube selector's `get_unplayed_normalized_video_ids` and
    `get_normalized_video_ids` (the Continuous / Single pick and its
    rotation restart: a blocked song is never "the one unplayed song");
  - Loop's remembered song after a restart (`loop_memory::remembered`, its
    youtube / dabing branch; a custom set list's branch is untouched);
  - the open-failure retry's next song: the retry's attempt IS the
    selector (`apply_event(PlayEvent::Start)`), no code of its own.

  A new automatic pick splices `pickable_row!()`, never `removed_at`
  alone. The worker queues (`queue_row!()`) do NOT ask the block: a blocked
  song keeps its stems, lyrics and files (an operator may still play it).
- **What a block never touches (decided):** an operator's play-video by id
  (`video_playlist_membership`, still only #260's mark), a custom set
  list's items (by position) and its Loop memory, Loop's song held in
  memory (`ReplayCurrent`, the selector's Loop branch with a current song:
  not a pick), and the song playing when it is blocked (it plays to its
  end: the block acts at the next pick).

## The API (`api/blocked.rs`, `db/models_blocked.rs`)

- `GET /api/v1/blocked` → 200, `[BlockedSong {youtube_id, title,
  blocked_at_ms}]`, the newest block first.
- `POST /api/v1/blocked {youtube_id}` (typed `sp_core::song_block::
  BlockRequest`; the id trimmed) → 201 with the stored row; a song this
  node has no row of (never the #228 test item) → 404 "no such song on this
  node"; a second block keeps the FIRST time and title (201 again, the same
  row). The title = the wall's text from the node's own rows
  (`models_blocked::song_title`: a row with a song name first, then the
  oldest; `sp_core::song_block::shown_title` = `wall_title(song, artist)`,
  else the YouTube title).
- `DELETE /api/v1/blocked/{youtube_id}` → 204; not blocked → 404.
- One INFO per block (`song block: a song is blocked on this node - never
  picked again`, `youtube_id`, `title`) and per unblock; a repeated block
  is DEBUG.
- **One title formatter:** `sp_core::song_block::wall_title` is the
  Resolume title's (`resolume::handlers::format_title_text` re-exports it,
  so `playback::title` and the driver use it too): the page searches by
  exactly the text the wall shows.

## The page `/blok` (`sp-ui/src/pages/block.rs`)

- A route (`Page::Block`, `/blok`), no tab: reached by its short address
  (`http://<node>:8920/blok`, on SNV `http://sp.local:8920/blok`), LAN only,
  no login (like the dashboard). Phone first (the `.block-*` styles, 44 px
  targets, no horizontal scroll at 390 px).
- "Teraz na programe" (`block-current`, `data-state` loading | nothing |
  song): `song_block::program_view` — the song of the playlist on program
  (`player_view::program_badge(..).is_on_program()`, the one on-program
  rule; the lowest playlist id when a cut shows two) shows at once;
  "Načítavam…" until every playlist's state was told
  (`selection::states_known` over the page's OWN read of
  `/api/v1/playlists`, `None` until it landed: an empty list before the
  load is no "every state known"; #225: never "nothing plays" before), then
  "Na programe nehrá žiadna pieseň.". Its title = THAT playlist's WS
  now-playing `wall_title`, else the row's; its YouTube id from the node's
  rows (the `GET /api/v1/playlists/{id}/videos` of every listed playlist,
  read again when the list changes). Button `block-current-btn` "Zablokovať v <uzol>"
  (`block_label`: `node_name` from `/api/v1/status`, else "tomto uzle"),
  disabled with "Už je zablokovaná" once blocked or while its id is
  unknown.
- The search `block-search`: `song_block::search` over the node's songs
  (`catalog`: each YouTube id once, its title the server's pick — a row
  with a song name first, then the oldest —, by title), every word, case and
  diacritics ignored (`fold`), at most `SEARCH_LIMIT` 20, nothing for an
  empty query; each `block-result` (`data-youtube-id`) with "Zablokovať".
- The list `block-list` (`data-loaded` once read; "Načítavam…" before,
  and a failed read keeps it so with the error on `block-message`), each
  row "Odblokovať" (`block-unblock`).
- Every rule is sp-core's (sp-ui has no unit tests).

## Tests

- sp-core `song_block_tests.rs` (every rule, every fold arm, the limit).
- sp-server `db/mod_tests_v44.rs`; `db/models_blocked_tests.rs` (the
  queries, and the selection: 30 picks with two songs blocked — one by a
  row of another playlist —, the rotation restart, Loop after a restart
  and Loop's song in memory, a custom set list keeps a blocked song);
  `api/blocked_tests.rs` (the routes through the real router: statuses,
  idempotence, 404s, a typed body, a play by id of a blocked song still
  plays).
- Mock E2E `e2e/block-songs.spec.ts` at 390×844 (the mock mirrors the
  routes; `/__mock/blocked-reset`). Post-deploy `e2e/post-deploy-block-songs.spec.ts`
  (SNV and PP's subset) is READ-ONLY: `GET /api/v1/blocked`, the page's
  list loaded with no error line, and the area past "Načítavam…" with,
  for a youtube playlist's song on program, an enabled "Zablokovať v …"
  or "Už je zablokovaná" (the pure `e2e/block-gate.ts::currentAreaProblem`,
  mock unit spec `block-gate.spec.ts`; a set list's song is not judged: it
  may be a row no song list names). It never clicks, never blocks a song
  on a box.

## Not built (follow-up only if the owner asks after using the page)

The QR code on the wall / stage display: it would need a picture on the
operator's Resolume composition (SongPlayer never drives Arena's clips) or
on the Presenter.
