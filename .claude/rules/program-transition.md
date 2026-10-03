---
paths:
  - "crates/sp-server/src/playback/program_transition*.rs"
  - "crates/sp-server/src/playback/program_follow*.rs"
  - "crates/sp-server/tests/obs_snapshot_follow.rs"
  - "crates/sp-server/src/playback/scene_off*.rs"
  - "crates/sp-server/src/playback/tests_hold.rs"
  - "crates/sp-server/src/playback/tests_scene_off_wall.rs"
  - "crates/sp-server/src/playback/tests_wall_owner.rs"
  - "crates/sp-server/src/playback/handle_pipeline_event.rs"
  - "crates/sp-server/src/playback/clear_lyrics.rs"
  - "crates/sp-server/src/playback/engine_play.rs"
  - "crates/sp-server/src/playback/program_bus*.rs"
  - "crates/sp-server/src/playback/program_output*.rs"
  - "crates/sp-server/src/playback/program_canvas*.rs"
  - "crates/sp-server/src/playback/pacer_tests_live.rs"
  - "crates/sp-server/src/playback/nv12_fit.rs"
  - "crates/sp-server/src/playback/nv12_mix*.rs"
  - "crates/sp-server/src/api/program*.rs"
  - "sp-ui/src/components/program_control.rs"
  - "sp-ui/src/components/settings_form.rs"
  - "e2e/program-control.spec.ts"
  - "e2e/settings-program-transition.spec.ts"
---

# Scene transitions on `SP-program` (#215, B5 of EPIC #174)

The owner's rule: "žiadne trhané zvuky a obrazy sú neprípustné v produkcii".
A program cut is no longer a hard switch on one boundary. It is a crossfade of
audio AND picture that follows cg OBS's scene transition, and the outgoing
playlist keeps playing until the fade is over. Design record: #215 comment
5853036223 (Approach 1); the implementation notes are in comment 5853106216.
The addendum (design record 5855841198, after box run 1 in 5855833871): the
picture dissolves across different sizes, and a fade waits for the incoming
song's first real pair.

## A cut is a transition WINDOW (`program_bus.rs`)

- `ProgramCore::cut` still records the `(first_stamp, pid)` segment of #209:
  `to` owns every boundary from the cut boundary on. It ALSO pushes a
  `program_transition::Window::cued` of the current spec: `from` (the source
  ON AIR — see the cue gate below; `None` for a fade up from nothing), `to`,
  `cut_100ns` = the cut boundary, `start_100ns` = the first MIXED boundary,
  `n_slots`, an exclusive `end`, and its `cue`. The window takes `from`'s
  pairs over `[cut, end)` (`Window::covers`); only `[start, end)` is mixed
  (`Window::slot`).
- The window half of the core is the child module `program_bus_window.rs`
  (`window_at`, `on_air`, `window_step`, `open_cue`, `commit_held`,
  `commit_mix`; 1000-line cap).
- On each window boundary the outgoing source's pair goes to `from_pending`,
  the incoming one to `pending`. The boundary is released as ONE
  `ProgramJob::Mix` once each side is here or MISSED. "Missed" uses the #209
  per-source rules (`source_missed`): the source passed the boundary, it is
  absent for 1 s, or its 3-slot grace ran out (on the sender's wall only).
  A missed side is left `None` and mixed against the standby (#223: the
  `SP-program` canvas black, the program's standby picture — 1920×1080 in
  production, `program-bus.md` — + silence, `side_fills`). With NEITHER side
  here, the
  boundary is filled like any other (`ProgramJob::Standby`). Either reorder
  buffer over 16 forces the boundary.
- **A Cut is a zero-length window**: no boundary is mixed, the output is the
  #209 cut byte for byte. `ProgramCore::new()` starts with a Cut
  (`SpecSource::Fallback`), so every #209 test still runs a Cut.
- A second cut: a window that has not started at the new boundary is replaced
  (a cut back to the source that still owns it cancels it); a running one is
  TRUNCATED at the new cut boundary, and the new window starts there. The new
  cut is also placed after the newest stamp of every window's `from`, so the
  outgoing source never has a waiting pair for a boundary that left its window
  (a stale `from_pending` entry would count toward the reorder bound forever).
  A window whose cue still WAITS and whose span reaches the new cut boundary
  is truncated too and FROZEN (`Cue::Frozen`: it never opens, its boundaries
  stay held), and the new window fades out of the source really on air, the
  frozen window's `from` (`ProgramCore::on_air`). A cut back to that source
  needs no window: it just takes the boundaries over (segment only). A
  waiting window whose LATEST end lies before the new cut boundary is left
  alone: it opens by its deadline and its fade is over before the new cut
  (review round 5 — frozen, it hard-cut its two sources at its end). Both
  decisions use ONE predicate, `Window::holds_on_air` (the cue waits or was
  frozen, cut ≤ boundary ≤ end), so `on_air` and the freeze never disagree.
- `prune` drops a window once its last boundary is served
  (`transitions_done`, a Cut included; a frozen window too, a same-slot
  replaced one never). `Window::covered()` is the number of MIXED boundaries:
  the open window's (possibly truncated) span, `n_slots` while its cue waits,
  0 once frozen; `served` and the dashboard `progress` count against it, so a
  superseded fade still ends at 100 %.
- Grid math is exact: `sp_core::genlock::{grid_index_100ns,
  grid_boundary_100ns}` (tested in sp-core `genlock_tests_grid_index.rs`).
  Slots are 333 333 or 333 334 × 100 ns wide, so NEVER `start + k · interval`.

## The cue gate (#215 addendum B2, `program_bus_window.rs`)

The box run 1 finding: a playlist whose scene comes on program starts a NEW
song. Its first decoded pair arrives ~10 boundaries after the cut (the paused
song's teardown = ~4 paced-output fills, then the new song's pre-roll = ~6
standby pairs while the decoder opens, see "B1" below), so a fade laid on the
cut boundary mixed the outgoing song against silence.

- `SubmitJob::live` marks a pair of the source's OWN content: the pacer's
  `PacedSink::emit` (a decoded frame with its song audio, a stall repeat
  included) and an NDI input capture. Never live: `PacedSink::emit_standby`
  (the paused frozen picture, and via `default_submit_shared` the idle and
  pre-roll black, a starve fill, a held seek frame), the paced consumer's
  fill (`PacedConsumer::fill_job`) and the NDI input's standby pair. The
  default `emit_standby` is `emit`, so `FrameSubmitter` and emit-only sinks
  see what they always saw; `HandoffSink` overrides it (live = false).
- A Fade window starts `Cue::Waiting { deadline }` (`Window::cued`), with
  `deadline` = the cut + `CUE_WAIT_MAX_SLOTS` (15 = the design's
  `CUE_WAIT_MAX` 500 ms) and its `end` the latest it can reach (the cut +
  15 + `n_slots`). A Cut is `Cue::Open` at once and zero-length: it never
  waits.
- While it waits, a boundary is decided once the INCOMING pair is here or
  missed (its first live pair may be the one):
  - `to`'s pair is live → the cue OPENS there (`Window::open`: `start` = that
    boundary, `end` = start + `n_slots`), and the boundary is mixed as slot 0;
  - `to` decided and the boundary is the deadline → the cue opens anyway
    (`cue_timeouts` +1, WARN `the incoming source sent no live pair in time`);
  - otherwise the boundary is HELD (`commit_held`): `from`'s own pair at full
    level as a `ProgramJob::Source` (counted `forwarded`), or the program's
    standby pair when `from` missed (`filled`); `to`'s pair is dropped;
  - both missed and neither here: filled like any boundary (the core's fill +
    resync path).
- `open_cue` drops `from`'s pairs past the window's new end (they belonged to
  the wait's worst case and would count toward the reorder bound forever).
- `transition.cue_wait_boundaries` = the LAST opened window's wait (0 = live
  on the cut boundary, 15 = timed out; more only if a > 8-slot resync jumped
  past the deadline); `cue_timeouts` counts the timeouts. A frozen window
  never opens and never touches them. NOT every timeout is a fault: a
  dashboard cut to the NDI input with no source offers only standby pairs,
  so it waits the full 15 boundaries, WARNs and counts one. A dashboard cut
  to a playlist that was off program is NOT that case any more (release
  0.69.0 review 🔵 11): since #221 L4b the cut puts it on air, the playback
  authority's ON (`SceneOn` → `SelectAndPlay`) starts a new song, and the
  fade opens on that song's first live pair — a timeout there means the song
  took longer than 15 boundaries to start. Read the box's `cue_timeouts` +0
  check on the cg OBS scene-change path only.
- A WAITING cue opens only on its own window's live pair: once a later cut
  froze it, a live pair of its incoming source inside it is held like any
  other (review round 1). `cut` reads `on_air(boundary)` BEFORE
  `windows.retain`, so a same-slot re-cut that drops the waiting window still
  fades out of that window's outgoing source. `on_air` takes the first
  window that `holds_on_air` the new boundary — WAITING or FROZEN, cut on or
  before it, end on or after it — else the selected source (windows stay
  disjoint and in push order, so at most two match, where one ends exactly
  where the next starts, and the older one holds the slot before):
  - a frozen window's end may lie on (a cut back, then a same-slot re-cut
    that drops the cut back's segment, review round 3) or AFTER the new
    boundary: cut boundaries are NOT monotone. A cut is placed after the
    newest stamp of every source still involved, and a source that drops out
    (its window served) no longer pushes it later, so a later cut can land a
    slot or more BEFORE an earlier one (review round 4, X running two slots
    ahead: cut back on b(18), the next cut on b(17));
  - a window cut AFTER the new boundary is one this cut replaces: its
    outgoing source held nothing on program yet, so it never decides (X four
    slots ahead: a Cut to B on b(17), a waiting fade B → C on b(18), then a
    cut to D on b(17) fades out of A, not B — review round 4).
- Logs: INFO `the incoming source is live — the fade starts` (from, to,
  waited) / WARN `… sent no live pair in time …` per opened cue, INFO `a later
  cut froze the fade still waiting for its cue`, and from the sender one INFO
  `the fade's mixed boundaries went out` per run of mixed boundaries
  (`boundaries`, `fitted`, `max_picture_us` = the worst fit + blend time on
  the `SP-program` thread).
  The dashboard "Prechod" line shows no counters, so it is unchanged; a
  waiting window shows as `active` with 0 %.
- `hold_for` reports a waiting window's LATEST end, so `scene_off.rs` keeps
  the outgoing playlist playing through the wait with no new timer; its
  re-check finds the window over, or asks again.

### B1: the silence before a new song is its PRE-ROLL, not a defect

Traced on #215 (comment 5856377673): after `paced output: a pacer feeds
again`, `Pacer::preroll` (`pacer_preroll.rs`) services every boundary with the
standby pair (the 1920×1080 black + silence) until `PrerollGate::poll` sees the
decoder opened (MF reader, the stems — `karaoke: mixing stems` ~93 ms after
`starting playback` — and `open_paced_decoder`) AND its first frame buffered.
The song's first decoded pair then carries FLAC block 0 (`split_sync` pairs
the first frame with every chunk up to pts + 250 ms; the A/V anchor starts the
block at the frame's media time). `pacer_tests_live.rs` pins both halves. What
would shorten the wait (main-session decisions, not built): overlap the paused
song's producer teardown (`stop` + `join`, ~4 fills) with the new song's open,
or resume the paused song on scene-on instead of `SelectAndPlay`.

## The mix (`program_transition.rs` pure, `program_output.rs` on the sender thread)

- Audio: equal power, `a = a_from·cos θ + a_to·sin θ`, θ = π/2 · (j + ½)/N over
  the WHOLE window (N = n · 1600 samples, j runs across boundaries), so the gain
  never steps at a boundary edge. A mono side feeds both channels; a missing
  side is silence. The audio stamp is `to`'s, else `from`'s.
- Picture: Q8 weight `w = round(256 · (k + ½)/n)`, blended
  `(f·(256 − w) + t·w + 128) >> 8` on Y and UV alike into a `frame_pool`
  buffer, always in the `SP-program` canvas (#223: 1920×1080, `program-bus.md`
  "SP-program is ALWAYS 1920×1080"; before, the INCOMING side's layout). The
  incoming picture is made a canvas picture first (as it is, or fitted onto
  the canvas black at weight 0); the outgoing one is fitted into the canvas
  as it is blended when it is not a canvas picture (#215 addendum A; one
  pass since addendum 3) — there is no midpoint cut any more:
  - `FitPlan` (test-only `fit_nv12_into` is its one-shot form) places the picture with
    `nv12_fit::aspect_fit` — the SAME placement the #178 preview letterbox
    (`preview_stream::placement_for`) uses: each axis the destination capped by
    the aspect-scaled other axis, floored to even, centred on even offsets (a
    chroma sample covers its 2×2 luma block; 2560×1080 into 1920×1080 → rows
    134 to 943), studio-black bars Y 16 / UV 128. Only the pixel paths differ:
    the preview copies nearest-neighbour on the decode thread (cheap by rule),
    the program fit is bilinear and, since #223, runs on every boundary whose
    picture is not a canvas picture;
  - bilinear in Q8 at the pixel CENTRES (`HALF_PIXEL_Q8`), luma and the
    half-resolution chroma each on their own grid, clamped at the edges;
  - the column taps are built once per source layout: the canvas
    (`program_canvas.rs`) keeps the two plans used last, so both sides of a
    fade fit every boundary with no rebuild (`Canvas::built` counts builds,
    the tests read it as `fit_plans()`), and logs one INFO line per new plan.
    The plans are ALL it keeps: there is no fitted scratch buffer (addendum
    3);
  - a source or destination that is not whole NV12 for its layout gives the
    black canvas alone, never a panic (a zero-size one simply draws nothing,
    and a zero-stride destination is the canvas too).
- **The fit + blend is ONE fused pass in K row bands (#215 addendum 3, design
  record 5858472395, `nv12_mix.rs` — a child module of `program_transition`,
  so it reads `FitPlan`'s private rectangle and taps).** Box run 2 measured
  ~56 ms per 2560×1440 fitted boundary (canvas + fit into a scratch + blend,
  three passes on one thread) against the 33.3 ms slot.
  - `mix_nv12_into(Outgoing::Same(layout, from) | Outgoing::Fitted(plan,
    src), to, w, K, out)` computes every byte ONCE as `blend(fit(from)[p],
    to[p], w)`; the fitted byte is never stored. It is BIT-IDENTICAL to
    `FitPlan::apply` then `blend_nv12_into` (the same `tap` / `bilinear`, the
    same Q8 rounding). Those two and `fit_nv12_into` are `#[cfg(test)]` now:
    the reference the kernel is pinned against. Change the fit or the blend
    in BOTH places, or the equality tests fail.
  - The rows are shared out per plane in K contiguous bands (`band_bounds`):
    luma rows `dh·i/K .. dh·(i+1)/K` and chroma rows `ch·i/K .. ch·(i+1)/K`.
    The last band also gets the bytes past the chroma plane, and every offset
    is capped at the mixed length. Band 0 runs on the `SP-program` thread;
    bands 1..K run on named `program-mix` scoped threads
    (`std::thread::scope`, spawned per painted pass — a mixed boundary,
    or since #223 a plain fit into the canvas — no pool, no new
    dependency). A helper that cannot start has its band painted inline,
    with a WARN. The return value is the number of threads that painted; the
    tests use it as the proof of parallelism.
  - K = `mix_bands(heavy_slot::logical_cores())` = clamp(cpus / 4, 1, 6):
    6 on the 24-thread box, 1 on a 4-vCPU CI runner (tests set
    `out.mix_bands` themselves). The `program output thread started` INFO
    line prints `mix_bands`.
  - The run painter takes ANY byte run (a run may start mid-row): it cuts
    the run at the plane edge, then `plane_run` paints the rest of the first
    row and then whole rows as `chunks_mut(stride)`. So the picture never
    depends on K or on the band edges, and every step paints at least one
    byte: a mutated row length panics or moves the picture, it never stalls
    (review round 1: a hand-advanced cursor hung on a `%`→`+` mutant, which
    is a cargo-mutants TIMEOUT and a red gate).
  - The only large buffer is the pooled output, `resize`d once (a memset:
    the bands need disjoint `&mut` slices in safe code). Per painted pass
    there are also K−1 scoped thread spawns (a name `String` and a stack
    each) and a few small `Vec`s (the band offsets, runs and slots). Since
    #223 a fade boundary whose incoming picture is not a canvas picture
    paints TWO passes (the incoming side's bilinear fit, then the blend,
    bilinear too when the outgoing side is not a canvas picture either):
    2·(K−1) spawns, where a fade between two 1440p songs used to be one
    byte-for-byte blend. Every forwarded boundary of a
    source that is not 1920×1080 paints one pass too (`program-bus.md`,
    "SP-program is ALWAYS 1920×1080").
  - A helper that fails to start WARNs once per failed band. That is at most
    K−1 = 5 per boundary and 300 boundaries per window, and it only happens
    when the OS cannot create a thread at all, so each failed band gets its
    own line and there is no rate limiter.
  - Box run 3: read the `max_picture_us` TAIL, not a mean. On Windows each
    of the K−1 spawns per boundary runs every loaded DLL's thread attach
    under the loader lock (NDI, Media Foundation, WebView2). If the target is
    missed, time the spawns separately before touching the kernel (review
    round 2).
  - `max_picture_us` is the wall time of `paint_mix` (all bands) on the
    `SP-program` thread (#210: `mix_picture` is only the tests' `Option` view
    of it now); since #223 it includes the incoming side's canvas fit, and
    `health.timing.submit_us` includes the whole picture too. Box run 3's
    target (historical: ONE pass into a 2560×1440 output) was ≤ 15 000 at
    2560×1440 with `fitted=9` and 0 late drops, for a 300 ms fade and a 1 s
    fade (30 boundaries). Since #223 the target is one slot for the whole
    video side (`program-bus.md` "SP-program is ALWAYS 1920×1080", its box
    check).
- `mix_audio_block` collects its samples instead of pre-sizing the `Vec`: a
  capacity formula is an equivalent mutant.

## The outgoing playlist keeps playing (`scene_off.rs`)

`handle_scene_change(pid, false)` (#221 L4b: the playback authority's OFF,
`program-bus.md`) no longer pauses at once. It asks
`ProgramBus::hold_for(pid)` (only for a Playing pipeline):

- `Hold::Until(t)`: `pid` is the `from` of a window (or a Cut) not served yet.
  `t` = one slot after the window's end. It is re-checked at `t`.
- `None`: pause now, exactly as before.

#221 L4b deleted `Hold::OnProgram` + `CUT_SETTLE` (a 500 ms wait for the
program's own source, whose cut away could follow cg OBS's scene event):
the authority drops a stale OFF, and the program's selected source is always
on air, so it is not taken off program; a hold's re-check leaves a playlist
in the authority's diffed set alone (its ON is queued). (`hold_for` of a
selected source is `None` unless an earlier window still has it as `from`,
e.g. after A→B→A in a later slot.)

The re-check is `PipelineEvent::SceneOffDue` on the engine's own channel (a
spawned sleep). If the scene is back on program by then, it does nothing.
`scene_off_step` / `scene_off_recheck` take `now_100ns`, so the tests drive
them at chosen stamps; only the wrappers read `utc_now_100ns()`.

### The wall after an OFF (`wall_after_scene_off`, #221 L4b review rounds 3-4)

The authority sends the incoming ON at the press and the outgoing OFF only
at the mirror's OK, so on almost every playlist press another playlist is
already on program when the OFF is handled (before L4b the bridge sent the
OFF first). The wall step runs before the pause/hold:

- "Another playlist on program" = `scene_active` AND in the authority's
  diffed set (`on_air_contains`). A flagged playlist out of the set has its
  OFF queued too (two members leaving in one coalesced value): counting it
  flashed its title.
- None on program: `HideTitle` (a fade, through `title::push_hide`, which
  also clears cg OBS's `#sp-title` text: round 6, the OFF cancelled the
  song's hide timer) + `HideSubtitles`, as before L4b.
- One on program: the title from `decide_wall_title` — a due title is a
  `Resync`; none due (the incoming playlist just started its song, the
  usual press) is a `HideTitle` through `title::push_hide`, so the outgoing
  title FADES (a `Resync(None)` hides at once, round 4's 🟡) and cg OBS's
  `#sp-title` text is cleared (round 5: B's ON came first, so B's Play
  re-sync still named A's title there); a failed read of the due title
  sends nothing. Its line is re-sent at once (`on_program_lines`,
  shared with the Resolume recovery, filtered to the diffed set), and ONE
  `HideSubtitles` goes only when none has a line: one playlist never clears
  another's line (the recovery's rule, `resolume-driver.md`).
- The title candidates and the re-sent lines are those of the wall owner
  only while there is one (release 0.69.0 review 🟡 2, `program-bus.md`
  "One wall owner"). Residual, with no owner only: the candidates are
  `scene_active` ones (`title_candidates`), so a due title of a playlist
  whose OFF is queued can be re-synced until its own OFF re-syncs the wall.
- The stage display (#221 review round 3): with another playlist on
  program, the OFF ends with `resync_presenter(owner)` — the wall owner's
  line at its last position, or a cleared display — because the owner can
  change by this OFF alone (a cut to "OBS manuál" while cg OBS still
  shows another playlist). After the OFF of a member that did not own the
  wall it repeats the owner's line, or clears the display while the owner
  is in a blank stretch (review round 4: blank like the wall until the
  owner's next line).
- The other half, an ON: the wall owner's ON re-syncs the title, the line
  and the stage display to it (`scene_off::wall_after_owner_on`, review
  rounds 1-2), since the old owner writes nothing any more; details in
  `program-bus.md` "One wall owner".
- Pinned in `tests_scene_off_wall.rs` (`going_off_program_*`, a child
  module of `tests_scene_change.rs` that reuses its rig) and, for the
  wall owner, `tests_wall_owner.rs`
  (`an_owner_change_by_an_off_re_syncs_the_stage_display`,
  `the_new_owner_s_on_re_syncs_the_wall_s_line`,
  `a_new_owner_that_plays_nothing_takes_the_old_owner_s_title_down`).

### A held playlist has no side effects (release 0.68.0 blockers)

Design record 5863318980 (the cross-lane review of PR #220), plus its two
review rounds. The hold keeps the outgoing playlist DECODING, nothing more:
for every engine side effect it is off program. Before, its song's end
inside the hold started the next song off program, whose hide timer later
faded out the on-program title.

- **The hold is a marker.** `PlaylistPipeline::scene_off_due` keeps the
  pending `SceneOffDue`'s id and the abort handle of its sleeping task;
  `Some` = held. `end_hold` cancels it, and these call it:
  - every `PlayAction::Pause`;
  - a scene back on program (`handle_scene_change(pid, true)`);
  - an operator's pick (`handle_play_video`, which a ▶ resume also runs,
    and `handle_previous`): the song then plays like any song played off
    program by hand, and its end starts the next. The fade still mixes out
    of that playlist, so for the rest of the window its side is the picked
    song's pre-roll standby, then its start, at a falling weight;
  - a newer hold: a re-check that finds the window not over replaces its
    predecessor.
- **A re-check names its hold.** `SceneOffDue` carries the hold's re-check
  id, a process-wide `u64` (`NEXT_RE_CHECK`). Not a tokio task id: tokio may
  reuse one once its task has ended, which is a stale re-check's state. A
  hold registers its re-check before the engine can see the event, so
  `scene_off_due` acts only on the PENDING one and ignores any other as
  stale:
  - a newer hold replaced it: queued during an A→B→A→B, it was taken as the
    newer hold's re-check and re-checked that hold early;
  - the hold ended (a pause, a scene-on, an operator's pick): queued when
    the pick came (the engine's `select!` is unbiased), it held the picked
    song again or paused it. A pick ends the hold BEFORE its PlayVideo clear,
    so the clear goes out as for any song played off program by hand.
- **Its end, a failure or a skip never start a song off program.**
  `pause_if_held` runs first in the `Ended` and `Error` arms and for a
  `Skip`. A held playlist gets `SceneOff`, the hold's own end, instead of
  `SelectAndPlay` / `ReplayCurrent` / Single's `SendBlack`. So none of
  these happen:
  - an off-program `Started` (its title timers took down the on-program
    title);
  - a `record_play` of an unaired song;
  - the song-end `clear_lyrics_display` (see the next bullet).

  The accepted trade-offs:
  - A song that ends inside the hold is not followed by the next one, and
    the rest of the window mixes the paused side's standby. The hold is the
    fade plus up to 15 slots of cue wait: about 0.8 s for a 300 ms fade, up
    to about 10.5 s for the longest one (300 slots).
  - The pause keeps its resume point while the playlist stays paused. For
    a song that ENDED that is its end (a later ▶ plays its last moment,
    then the next song); for a skipped one the skipped song where it was
    (the skip is not carried over). Every Play drops the resume point
    (`begin_play`), so it never outlives the pause.
- **`clear_lyrics_display` follows the dispatch gates.** A held playlist
  clears nothing. One merely off program leaves the shared subtitle clips
  alone, as its lines do; its own karaoke WS clear and the Presenter clear
  still go out (the off-program contract of `dispatch_lyrics_if_changed`).
  Before, a held song's `Started` with no lyrics, and any off-program song
  end or PlayVideo, blanked the on-program playlist's `#sp-subs` line (its
  dedup key kept it blank until its next line).
- **A `Started` a pause overtook shows nothing.** The Play went out, then a
  pause came (the hold's end, the dashboard's Pause) before the song's
  `Started`. After the NowPlaying broadcast the arm returns, with no lyrics,
  clock, timers or clear; the resume's `Started` does them. The test is
  "paused since the last Play": `paused_at` is set (every Pause sets it,
  every Play clears it in `begin_play`). Not the `WaitingForScene` state: a
  Skip whose selection starts nothing leaves that state with no Pause sent,
  and the pipeline keeps playing the song (review round 5).
- **The bus needs nothing new.** A paused paced pipeline emits its frozen
  last frame + a silent block per boundary (`Standby::FrozenLast`), an ended
  one its idle standby. They are `from`'s pairs like any other (only `to`'s
  liveness gates a window). A side that sends nothing is MISSED and mixed
  against the program standby, so the window never stalls.
- **Both title timers fire only on program** (`title_timers.rs`): the hide
  timer reads `scene_active` when it fires, like the show timer — and, while
  another playlist owns the wall, neither fires (`WallGate`, release 0.69.0
  review 🟡 2: one on-air member's hide timer took the other's title down). A pause
  cancels them, and on program it clears the line
  (`clear_lyrics_display`) and re-syncs the title (`resync_after_play`): a
  paused song's title and line are not due, so they go down at the pause,
  as any later re-sync would take them down. Off program (the hold's end)
  nothing of the song is up, so nothing is sent. With several SongPlayer
  playlists on one program scene the clear also blanks the other's line,
  like the song-end and PlayVideo clears: the shared-clip corner, where the
  lines already overwrite each other at every line change (older, as is).
- **Residual: the stage display at a scene change** (older than the
  hold). A scene-off clears the wall, never the Presenter, and a scene-on
  of a song without lyrics sends nothing. So a song without lyrics whose
  `Started` came inside the hold (its clear skipped) and whose scene then
  comes back on leaves the stage display and its karaoke view on their
  last line for that song, as a scene-on of any song without lyrics does.
- **The lyrics survive the hold.** The scene-off used to drop
  `lyrics_state`, so a scene back on inside the hold played on with no
  subtitles. Now:
  - While held, `dispatch_lyrics_if_changed` sends nothing to the wall, the
    Presenter or the karaoke WS, which is what `None` did. The gate is the
    MARKER, not `scene_active`: a song played off program by hand still
    feeds the karaoke WS + Presenter
    (`dispatch_lyrics_resolume_gated_on_scene_active`).
  - The scene-off resets the wall and Presenter dedup keys (the wall was
    cleared), so a scene back on re-sends the line at the next Position.
  - The PAUSE drops the lyrics, where the scene-off used to: a Position
    queued before the pause took effect sends nothing. Every Play drops
    them too (`begin_play`, with the position set to the Play's start): a
    recovery before the new `Started` must not re-push the old song's line.
- Pinned in `tests_hold.rs`. The hold there is real: the bus cuts a minute
  ahead on the live clock. "No Play was sent" is read from the title clock,
  which every Play clears (`begin_play`). That works on every platform:
  the Windows test pipeline never answers a Play, so counting its replies
  is not portable. A "nothing went out while held" check must use a NEW
  line: the same line is held back by the dedup keys anyway, so it proves
  nothing.

## Following cg OBS (`program_follow.rs`)

#219 (design record: #219 comment 5868318993, Approach 1) made the follow a
PURE CONSUMER of the OBS client's state: it asks cg OBS nothing itself. The
old parallel view (its own `GetCurrentProgramScene` / scene-lookup /
transition reads through `remote::Upstream`, `drain`, `missed_scene`, the
fallback to a dropped scene change, `obs_up`, `read_pending`,
`MAX_CATCH_UP_REREADS`) is DELETED — do not bring any of it back; the
unanswered-catch-up gap it had is gone by construction.

- Settings (`sp_core::config`, re-read every 5 s):
  - `program_follow_obs`: only `"true"` follows (default off);
  - `program_transition`: `obs` (default) / `fade` / `cut`;
  - `program_transition_ms`: a positive integer, default 300 (= 9 slots),
    rounded to whole slots, at least 1 slot, at most 300 slots (10 s). ONE
    parse for the server and the Nastavenia form:
    `sp_core::config::program_transition_ms` (+ `MAX_PROGRAM_TRANSITION_MS` =
    10 000, pinned equal to 300 slots by a test). The form shows the value the
    server uses, so its `min` / `max` never refuse a save of a stored
    out-of-range value.
- The spec every cut uses (`effective_spec`): the `fade` / `cut` override,
  else cg OBS's transition (`cut_transition` → Cut, any other kind → a Fade of
  its duration, a fixed-duration one → `program_transition_ms`), else a Fade
  of `program_transition_ms` (`fallback`). `ProgramBus::set_transition` returns
  whether it changed, and only a change is logged.
- **The input is the OBS client's `ObsSnapshot`** (`obs::snapshot`, a
  `tokio::sync::watch`; contract in `obs-ndi-health.md`): `connected`,
  `current_scene`, `active_playlist_ids`, `lookup_failed`, `transition`.
  `lib.rs` step 7 is `obs_bridge::start_obs` (the client spawns; #221 L4b
  deleted the engine bridge that subscribed first); its `snapshots` go to
  `PlaybackEngine::start_program(bus, shutdown, snapshots)` →
  `start_follow`. Without OBS configured the follow gets a CLOSED channel
  (the default, disconnected snapshot): `run_follow_task` stops selecting
  on it (`obs_open`) and only the settings polls run.
- cg OBS's transition is the snapshot's: the client reads
  `GetCurrentSceneTransition` itself (`obs::transition`, at connect, on
  `CurrentSceneTransitionChanged` / `…DurationChanged`, retried every 2 s
  until answered). `FollowLoop::take_transition` stores a snapshot's
  transition in `FollowShared` (the `follow.obs_transition` telemetry) and
  keeps the LAST KNOWN one while a snapshot has none (unknown, or cg OBS
  away), then applies the spec — BEFORE any cut of the same snapshot, so a
  cut always uses the spec of the snapshot that caused it.
- **What the follow acts on** (`FollowLoop::on_snapshot`, `program_scene`):
  - a snapshot's program scene is `Known(SceneView {scene, playlists})` only
    when connected, named and NOT `lookup_failed`; `Unknown` when
    disconnected or not named yet; `LookupFailed` when the client's playlist
    lookup failed (#218 — its `active_playlist_ids` then belong to an EARLIER
    scene). The order of the checks: not connected → `Unknown` first, then
    `lookup_failed` → `LookupFailed`, then the scene;
  - a `Known` scene is followed (when `program_follow_obs` is on) only when
    it differs from `seen`, the last known scene (name AND playlists). A
    snapshot that changes only the transition cuts nothing, so a manual
    dashboard cut is never undone by it;
  - `LookupFailed` is ignored completely: no cut, no `last_follow_cut`,
    `seen` kept. The client's repaired lookup is the next change. A failure
    that ends back on the followed scene changes nothing;
  - `Unknown` (not connected wins over a flagged failure) resets `seen`, so
    the reconnect's scene is followed again (as the old `SceneChanged` of a
    reconnect was) — when the follow SAW the disconnected snapshot: a
    disconnect shorter than the follow's own step (it is inside
    `follow_scene`) can be coalesced away, and a reconnect to the same scene
    then changes nothing;
  - a CATCH-UP — at start (`run_follow_task`'s first snapshot) and when
    `program_follow_obs` flips false → true (`on_tick`, the snapshot read
    with `borrow()`) — clears `seen` first, so the current scene is followed
    whether it changed or not; on an unknown or failed snapshot the NEXT
    known one is followed;
  - the watch coalesces: only the newest snapshot is seen, so a scene cg OBS
    showed only briefly between two wakes is never cut to (correct: the
    program ends on cg OBS's final scene).
  - **Behaviour change vs #215 (for the main session to confirm):** the old
    follow acted on EVERY `SceneChanged`, so cg OBS re-taking the SAME scene
    (a same-scene transition that fires `CurrentProgramSceneChanged`) cut
    `SP-program` back to that scene's source after a manual dashboard cut.
    Now only a CHANGED scene view acts (the design record's "reacts to every
    change"; `send_if_modified` also drops an identical snapshot), so such a
    re-take leaves the manual cut alone. The Companion path is unaffected:
    the #213 facade cuts on every press itself.
- Follow = `scene_action` (the #213 rule) → cut via `persist_and_cut`, unless
  the program already shows that source (`Follow::follow_scene`, which also
  records `last_follow_cut`). This replaces the event-night watcher
  `%TEMP%\sp_follow.ps1`, which polled the scene every 200 ms.
- **A followed scene is a switch like any other** (release 0.69.0 review
  🟡 1): `follow_scene` holds the bus's `switch_order`, takes a `legacy_cg`
  ticket before its cut, and records what cg OBS shows —
  `legacy_cg.confirmed(ticket, observed)`, the scene catalog's playlist or
  `None` for a manual scene, on every known scene (`remote-control.md`,
  "SongPlayer's record of what it told cg OBS"). With the follow on, cg OBS
  is the authority, so a playlist mirrored earlier leaves the air.
- Order vs the engine (#221 L4b): the engine no longer follows cg OBS's
  scene at all; a follow cut is a program cut like any other, and the
  playback authority plays what it put on air.

## API + UI

- `GET /api/v1/program` (and the cut answer) → `transition {kind, duration_ms,
  n_slots, source (obs|setting|fallback), active {from, to,
  start_boundary_100ns, n_slots, served_slots, progress} | null,
  transitions_done, mixed_boundaries, side_fills, cue_wait_boundaries,
  cue_timeouts}` and `follow {enabled, mode,
  ms, obs_transition {name, kind, duration_ms} | null, last_follow_cut}`
  (`last_follow_cut` has the #213 `RemoteCut` shape).
- Dashboard `ProgramControl`: the `program-transition` line, e.g.
  `Prechod: prelínanie 300 ms (podľa OBS)` / `(nastavenie)` / `(predvolené)`,
  `Prechod: strih (…)`, plus `— prebieha N %` while a fade runs.
- Nastavenia fieldset `settings-program-transition`: `…-follow-obs`
  (checkbox), `…-kind` (select obs/fade/cut), `…-ms` (number).
- The mock derives `transition` / `follow` from the stored settings. It runs no
  cg OBS and no sender: `/__mock/program-obs-transition` injects cg OBS's
  transition and `/__mock/program-transition-active` a running window; both are
  cleared by `/__mock/program-reset`.

## Tests

- `program_transition_tests.rs` (pure, exact pins; the fit's from a scratch
  Python model of `FitPlan`), `program_bus_tests_transition.rs`
  (the #209 rig: A 4×2, B 8×2, C 6×2, standby 2×2; `take_all` renders each job as
  `src W` / `fill` / `mix k/n F>T`; its helpers are `pub(super)`),
  `program_bus_tests_cue.rs` (the cue gate: `standby()` is a non-live pair;
  it reads `core.pending` / `core.from_pending` directly),
  `pacer_tests_live.rs` (which pacer pairs are `emit` vs `emit_standby`, and
  the B1 pin), `program_output_tests.rs` (the mixed
  boundary on the mock sender, the fitted blend, the plan reuse at every
  band count, on small canvases), `program_output_tests_fhd.rs` (#223: a
  fade between two sizes drawn in the 1920×1080 canvas),
  `nv12_mix_tests.rs` (addendum 3: the fused kernel against
  the two-pass reference on fixed-seed SplitMix64 frames at every weight
  and K = 1..=6, odd and short row counts, a sweep of small and broken
  layouts, arbitrary run cuts, the exact `band_bounds`, K threads for K
  bands; the `blend` / `fitted` helpers of `program_transition_tests.rs`
  also run the kernel, so every blend and fit pin pins it),
  `scene_off_tests.rs`, `program_follow_tests.rs` (pure + `Follow` +
  `program_scene`; the snapshot helpers `on_program` / `lookup_failed` /
  `with` / `fade` / `cut` are `pub(super)`), `program_follow_tests_loop.rs`
  (`FollowLoop::on_snapshot` / `on_tick` awaited one by one — no task, so
  every effect is complete when the step returns; a manual `persist_and_cut`
  makes a wrong re-follow observable as an extra cut) and
  `program_follow_tests_task.rs` (`run_follow_task` on a `watch` the test
  publishes on, incl. a closed channel = no OBS client) — the follow is
  driven ONLY by snapshots, there is no fake cg OBS any more.
  `tests/obs_snapshot_follow.rs` runs the real `ObsClient` against the
  `FakeObsServer` (its `scene_transition` knob): the transition read at
  connect / on each transition event / retried, and the real follow task on
  the client's snapshots through a failed lookup (#218) and its repair.
  Then `api/program_tests.rs`, and the mock E2Es `program-control.spec.ts` +
  `settings-program-transition.spec.ts`.
- Pins were derived with scratch Python models of `ProgramCore` and the
  weight / blend math (rust-workspace.md, no-compile box). Re-derive them with
  your own model when the Rust changes.
- A follow-task effect is proven by a LATER observable one (a spec, a cut),
  never by a sleep: publish a scene change, then a snapshot with a new
  transition, and `spec_becomes` the new one — the scene change before it
  has been handled (the watch may coalesce the two, with the same result).
  Wait for the start's catch-up to be DONE (a `last_follow_cut`, or the
  spec it applied) before publishing a change that must take the change
  path.
- To prove a poll does NOT act (no re-follow while following), cut by hand,
  then sync on a LATER poll's observable effect (store a setting,
  `spec_becomes`) and assert the manual source stayed.
- The engine tests never assert a "not yet" against the wall clock: the bus
  runs on fixed stamps, the steps are driven at chosen instants, and the real
  re-check timer is only bounded from below.

## Box acceptance (the supervisor's job, after the event)

Two playing playlists, a scene change via Companion/remote with a 300 ms OBS
fade: `mixed_boundaries` +9, audio RMS never more than 3 dB below the quieter
source, a dev1 VBAN capture with no zero-run ≥ 5 ms across the change, and the
owner confirms on the PA and the wall. Addendum (run 2): no `differ in size`
WARN (since #223 the fit logs only the canvas's INFO line `a picture of a new
size — fitted into the SP-program canvas`, once per new layout), the
incoming song's FLAC block 0 inside the
window, no true-zero run between the outgoing fade and the incoming song, and
`cue_wait_boundaries` ≤ 15 with `cue_timeouts` +0 — about 10–11 for a paused
playlist that starts a new song (the fills + the pre-roll, B1 above), 0 for an
already playing one. `program_follow_obs` is the setting
switched on for events (design record); the watcher script
`%TEMP%\sp_follow.ps1` is retired once it is on.

**Also check the common production path: a PAUSED incoming playlist.** Every
off-program playlist is paused when its scene leaves program; on its
scene-go-on the engine starts a NEW song for it, at the same moment as the cut.
Until that song's first decoded pair, its paced output offers fills, then
pre-roll standby pairs (black + silence) — none of them live, so the cue gate
HOLDS those boundaries (the outgoing song on program at full level) and the
fade starts on the first live pair. The wait shows up as
`cue_wait_boundaries` (~10–11, see B1) and must stay under the 15-boundary
bound (`cue_timeouts` +0). During a 2560×1440 ↔ 1920×1080 fade also read the
sender's `max_picture_us` line (≤ 15 000 since addendum 3, one pass into a
1440p output; run 2 read ~56 000 before it; since #223 a fade is drawn in the
1920×1080 canvas and the bound is one slot for the whole video side, see
`program-bus.md`) and `health.coalesced` +0, for a 300 ms AND a 1 s fade,
with 0 late drops.
