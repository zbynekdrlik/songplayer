//! #272: the test item's pinned clip LIST — camera-box's measurement clip
//! (#228, the default) and SongPlayer's own wall measurement clips
//! (`scripts/wall_clip.py`: each frame's index as a 16-cell Gray-code strip,
//! clip v2: in 9 self-contained copies across the width, each under a static
//! ruler, repeated in 8 bands, so any quarter of the width and height reads
//! under the operator's zoom; read back by `scripts/wall_frames.py`;
//! 3840×2160 H.264 at 24 / 25 / 30 fps, 60 s, a silent 48 kHz stereo AAC
//! track).
//!
//! - **One playlist, one row per clip.** Every clip is a video of the ONE
//!   hidden kind-`test` playlist (`SP-test`, scene `sp-test`, mode `loop`),
//!   never a playlist of its own: `not_test_item!` keeps every row of that
//!   playlist out of every worker queue, the operator's lists never show
//!   it, and the boxes run one test pipeline and one scene however many
//!   clips are listed. Rejected: a playlist per clip — each would add a
//!   scene to the catalog (Companion's list) and a pipeline with its idle
//!   fill on the production boxes.
//! - **Each clip is pinned by its sha256**, read from `<data dir>/bench`
//!   under its own file name and imported exactly as camera-box's clip is
//!   (the video stream copied, the audio into FLAC with no filter).
//! - **v1 → v2 keeps the rows** (`wallclip-24/25/30`): the v2 file and song
//!   name give new cache names, so the import records each row anew (the
//!   `already` check never takes the v1 files for v2), and the self-heal
//!   deletes the v1 pair as a superseded duplicate at the next start (the
//!   newest pair of an id is kept).
//! - **Which clip plays.** `/start {"clip"}` plays one from position 0 (no
//!   body: camera-box's). Loop repeats it, and Loop's memory (V37) keeps the
//!   clip started LAST: a press of `sp-test` with no start plays THAT clip,
//!   where before #272 it could only be camera-box's. So a wall run ends
//!   with `/start` and `/stop` with no body, which leaves camera-box's clip
//!   as the one a bare press plays.

use super::{
    TEST_ARTIST, TEST_CLIP_FILE, TEST_CLIP_SHA256, TEST_DURATION_MS, TEST_SONG, TEST_TITLE,
    TEST_YOUTUBE_ID,
};

/// One pinned measurement clip and the test playlist row it becomes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Clip {
    /// The name `POST /api/v1/test-item/start {"clip"}` takes.
    pub name: &'static str,
    /// Its file name in `<data dir>/bench`.
    pub file: &'static str,
    /// The file's sha256 (a byte array: a 64-hex string trips the staging
    /// hook).
    pub sha256: [u8; 32],
    /// Its video's frames per second.
    pub fps: u32,
    /// Its row's id: 11 characters, the cache's id rule.
    pub youtube_id: &'static str,
    /// The song and artist its files are named after, and its title.
    pub song: &'static str,
    pub artist: &'static str,
    pub title: &'static str,
    /// Its length, ms.
    pub duration_ms: i64,
}

impl Clip {
    /// The pinned sha256 as lowercase hex (`peer::hash::sha256_file`'s form).
    pub fn sha256_hex(&self) -> String {
        super::hex(&self.sha256)
    }
}

/// camera-box's measurement clip (#228): the default clip.
pub const CAMERA_BOX: Clip = Clip {
    name: "camera-box",
    file: TEST_CLIP_FILE,
    sha256: TEST_CLIP_SHA256,
    fps: 30,
    youtube_id: TEST_YOUTUBE_ID,
    song: TEST_SONG,
    artist: TEST_ARTIST,
    title: TEST_TITLE,
    duration_ms: TEST_DURATION_MS,
};

/// The wall clips' artist (their files are named after it).
pub const WALL_ARTIST: &str = "SongPlayer";

/// The wall clips' length, ms.
pub const WALL_DURATION_MS: i64 = 60_000;

/// The 24 fps wall clip (13 959 001 bytes; dev1
/// `~/.claude/work-products/issue-272/`, clip v2 generated 20.10.2026).
pub const WALL_24: Clip = Clip {
    name: "wall-24",
    file: "wall-clip-v2-24fps-60s.mp4",
    sha256: [
        0x7e, 0x7e, 0xaa, 0x4a, 0x0d, 0x60, 0xee, 0xd9, 0x39, 0xdf, 0xda, 0x00, 0x3a, 0x8d, 0x9b,
        0xa1, 0x2c, 0x60, 0x32, 0x35, 0x4b, 0x71, 0x5d, 0x7e, 0xa4, 0x45, 0x17, 0x77, 0xa5, 0xa1,
        0x2d, 0xf0,
    ],
    fps: 24,
    youtube_id: "wallclip-24",
    song: "Wall clip v2 24 fps",
    artist: WALL_ARTIST,
    title: "SongPlayer wall clip v2, 24 fps (60 s)",
    duration_ms: WALL_DURATION_MS,
};

/// The 25 fps wall clip (14 028 769 bytes), the one #251's blend runs on.
pub const WALL_25: Clip = Clip {
    name: "wall-25",
    file: "wall-clip-v2-25fps-60s.mp4",
    sha256: [
        0x7a, 0x9b, 0xdc, 0x6b, 0x28, 0x11, 0x15, 0x27, 0x09, 0x8b, 0xd8, 0xfe, 0x65, 0x20, 0x25,
        0x67, 0x96, 0xd0, 0x73, 0x5b, 0x22, 0x64, 0x54, 0xd2, 0x0a, 0xd8, 0xfe, 0xa5, 0x4d, 0xac,
        0x9d, 0x70,
    ],
    fps: 25,
    youtube_id: "wallclip-25",
    song: "Wall clip v2 25 fps",
    artist: WALL_ARTIST,
    title: "SongPlayer wall clip v2, 25 fps (60 s)",
    duration_ms: WALL_DURATION_MS,
};

/// The 30 fps wall clip (15 748 924 bytes): the grid's own rate.
pub const WALL_30: Clip = Clip {
    name: "wall-30",
    file: "wall-clip-v2-30fps-60s.mp4",
    sha256: [
        0xf2, 0x13, 0x49, 0x02, 0xbd, 0xfe, 0x74, 0x21, 0x6b, 0x8a, 0x41, 0x05, 0x84, 0x36, 0x3e,
        0x84, 0x66, 0xea, 0xd3, 0xea, 0x1e, 0x86, 0x7a, 0x4f, 0xfe, 0xab, 0xc6, 0x8b, 0x49, 0xd3,
        0x12, 0x8d,
    ],
    fps: 30,
    youtube_id: "wallclip-30",
    song: "Wall clip v2 30 fps",
    artist: WALL_ARTIST,
    title: "SongPlayer wall clip v2, 30 fps (60 s)",
    duration_ms: WALL_DURATION_MS,
};

/// Every pinned clip, the default (camera-box's) first. A `static`, so
/// [`clip_named`] hands out `&'static` references into it.
pub static CLIPS: [Clip; 4] = [CAMERA_BOX, WALL_24, WALL_25, WALL_30];

/// The listed clip called `name`.
pub fn clip_named(name: &str) -> Option<&'static Clip> {
    CLIPS.iter().find(|clip| clip.name == name)
}
