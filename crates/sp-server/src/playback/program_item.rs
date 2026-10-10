//! #228: the item on air on `SP-program`, and the 911014 burn switch.
//!
//! - [`ProgramItem`] (on `ProgramBus::item()`) is shared: the burn switch
//!   (`POST /api/v1/program/burn`, default OFF, never persisted), the item
//!   marks the engine writes, and the item the sender last put on the wire
//!   (`on_air_item` on `GET /api/v1/program`).
//! - [`ItemTrack`] is the `SP-program` sender's own (no lock): it follows the
//!   source of every boundary it serves.
//!
//! A play's media clock: the pacer stamps each decoded frame with its pts
//! from where the play started (`PacedFrame::pts_ns`, 0-based; a seek starts
//! it again), and the pipeline reports that start (`Started` / `Seeked`
//! `position_ms`). The engine marks it here ([`ProgramItem::mark`]: playlist,
//! video, start); the sender applies a playlist's newest mark at that
//! playlist's next boundary that carries a frame of the item (a live pair,
//! `SubmitJob::media_pts_100ns`). So the item's media time of a boundary is
//! `start + pts`, its frame index on the 30 fps grid [`grid_frame`] (for a
//! 30 fps item, the clip, its own frame number), and the moment its media
//! time 0 was on the wire, `started_at`, is `wire stamp − media time` of the
//! first frame after a mark, or after the pts went back (a loop, a restart
//! the mark has not reached yet).
//!
//! A boundary of another source (a cut, Blank, "OBS manuál") ends the item;
//! a boundary of the same source with no item frame (paused, a fill, the
//! pre-roll) keeps it as it was: the frame last on the wire.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use serde::Serialize;
use sp_core::genlock::GENLOCK_GRID_FPS;

/// 100 ns units in a millisecond.
const MS_100NS: i64 = 10_000;

/// 100 ns units in a second.
const SECOND_100NS: i64 = 10_000_000;

/// The frame index of the media time `media_100ns` (counted from the
/// item's first frame) on the 30 fps program grid, rounded to the nearest
/// frame (the decoder reports whole ms: frame 1 of a 30 fps item is 33 ms).
/// A time before the first frame is frame 0; past `u32::MAX` frames it
/// stays there.
pub fn grid_frame(media_100ns: i64) -> u32 {
    let frames = (media_100ns.max(0) * GENLOCK_GRID_FPS + SECOND_100NS / 2) / SECOND_100NS;
    u32::try_from(frames).unwrap_or(u32::MAX)
}

/// A playlist's newest media-clock start: the video it plays and where its
/// play (or its last seek) started. `seq` grows with every mark.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ItemMark {
    pub seq: u64,
    pub video_id: i64,
    pub start_ms: u64,
}

/// What one served boundary showed of the item: its frame index, its media
/// time from the item's frame 0, and the boundary's wire stamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ItemFrame {
    pub frame: u32,
    pub media_100ns: i64,
    pub wire_100ns: i64,
}

/// The item on `SP-program`, as `GET /api/v1/program` shows it
/// (`on_air_item`, with the video's YouTube id and title from the store).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct ItemStatus {
    pub playlist_id: i64,
    pub video_id: i64,
    /// When the item's media time 0 was (or would have been) on the wire,
    /// UTC ns.
    pub started_at_utc_ns: i64,
    /// The media time of the frame last on the wire, ms from frame 0.
    pub position_ms: i64,
    /// That frame's index from the item's frame 0 (the burn's `{frame}`).
    pub frame: u32,
    /// The wire stamp of the boundary that carried it, UTC ns.
    pub frame_utc_ns: i64,
}

/// The `SP-program` sender's view of the item on air (see the module doc).
#[derive(Debug, Default)]
pub struct ItemTrack {
    /// The source of the boundaries followed (`None` = nothing selected).
    source: Option<i64>,
    /// The mark applied (0 = none yet).
    mark_seq: u64,
    video_id: Option<i64>,
    /// The applied mark's start, 100 ns.
    start_100ns: i64,
    started_at_100ns: Option<i64>,
    last_pts_100ns: Option<i64>,
    last: Option<ItemFrame>,
}

impl ItemTrack {
    /// One served boundary: its `source`, the pts of the item frame it
    /// carries (`None` = no item frame: a standby, a fill, a paused
    /// picture), its wire stamp, and the source's newest `mark`. Returns
    /// what it showed of the item, `None` for a boundary with no item frame.
    pub fn observe(
        &mut self,
        source: Option<i64>,
        pts_100ns: Option<i64>,
        wire_100ns: i64,
        mark: Option<ItemMark>,
    ) -> Option<ItemFrame> {
        if source != self.source {
            *self = Self {
                source,
                ..Self::default()
            };
        }
        let pts = pts_100ns?;
        let fresh = mark.filter(|mark| mark.seq != self.mark_seq);
        let went_back = self.last_pts_100ns.is_some_and(|last| pts < last);
        if let Some(mark) = fresh {
            self.mark_seq = mark.seq;
            self.video_id = Some(mark.video_id);
            self.start_100ns = mark.start_ms as i64 * MS_100NS;
        }
        let media_100ns = self.start_100ns + pts;
        if fresh.is_some() || went_back || self.started_at_100ns.is_none() {
            self.started_at_100ns = Some(wire_100ns - media_100ns);
        }
        self.last_pts_100ns = Some(pts);
        let shown = ItemFrame {
            frame: grid_frame(media_100ns),
            media_100ns,
            wire_100ns,
        };
        self.last = Some(shown);
        Some(shown)
    }

    /// The item as it is on the wire: once a mark named its video and a
    /// frame of it went out on this source; `None` otherwise.
    pub fn status(&self) -> Option<ItemStatus> {
        let last = self.last?;
        Some(ItemStatus {
            playlist_id: self.source?,
            video_id: self.video_id?,
            started_at_utc_ns: self.started_at_100ns? * 100,
            position_ms: last.media_100ns / MS_100NS,
            frame: last.frame,
            frame_utc_ns: last.wire_100ns * 100,
        })
    }
}

/// The program's item record and burn switch, shared by the engine (marks),
/// the `SP-program` sender (reads marks and the switch, publishes the item)
/// and the API.
#[derive(Debug, Default)]
pub struct ProgramItem {
    burn_on: AtomicBool,
    burned: AtomicU64,
    seq: AtomicU64,
    marks: Mutex<HashMap<i64, ItemMark>>,
    on_air: Mutex<Option<ItemStatus>>,
}

impl ProgramItem {
    /// Turn the burn on or off (in memory only: a restart starts it off).
    pub fn set_burn(&self, on: bool) {
        self.burn_on.store(on, Ordering::Relaxed);
    }

    /// Whether the burn is on.
    pub fn burn_on(&self) -> bool {
        self.burn_on.load(Ordering::Relaxed)
    }

    /// Count one boundary that went out burned.
    pub fn count_burned(&self) {
        self.burned.fetch_add(1, Ordering::Relaxed);
    }

    /// The boundaries that went out burned since the start.
    pub fn burned(&self) -> u64 {
        self.burned.load(Ordering::Relaxed)
    }

    /// The engine: `playlist_id`'s play of `video_id` (re)started its media
    /// clock at `start_ms` (`Started`, `Seeked`).
    pub fn mark(&self, playlist_id: i64, video_id: i64, start_ms: u64) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let mark = ItemMark {
            seq,
            video_id,
            start_ms,
        };
        let mut marks = self.marks.lock().unwrap_or_else(|p| p.into_inner());
        marks.insert(playlist_id, mark);
    }

    /// The sender: `playlist_id`'s newest mark. Never waits: while the
    /// engine holds the marks, `None` (the mark is taken a boundary later).
    pub fn mark_of(&self, playlist_id: i64) -> Option<ItemMark> {
        self.marks.try_lock().ok()?.get(&playlist_id).copied()
    }

    /// The sender: the item now on the wire. Never waits: while the API
    /// reads it, this boundary's is skipped (the next one publishes).
    pub fn publish(&self, item: Option<ItemStatus>) {
        if let Ok(mut on_air) = self.on_air.try_lock() {
            *on_air = item;
        }
    }

    /// The item on the wire, as last published.
    pub fn on_air(&self) -> Option<ItemStatus> {
        *self.on_air.lock().unwrap_or_else(|p| p.into_inner())
    }
}

#[cfg(test)]
#[path = "program_item_tests.rs"]
mod tests;
