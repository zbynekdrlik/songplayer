//! #272: the test item's pinned clip LIST — camera-box's measurement clip
//! (#228, the default) and SongPlayer's own wall measurement clips
//! (`scripts/wall_clip.py`: each frame's index as a 16-cell Gray-code strip
//! repeated in 8 bands, read back by `scripts/wall_frames.py`; 3840×2160
//! H.264 at 24 / 25 / 30 fps, 60 s, a silent 48 kHz stereo AAC track).
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

/// The 24 fps wall clip (4 231 474 bytes; dev1
/// `~/.claude/work-products/issue-272/`, generated 14.10.2026).
pub const WALL_24: Clip = Clip {
    name: "wall-24",
    file: "wall-clip-v1-24fps-60s.mp4",
    sha256: [
        0x0b, 0x25, 0x40, 0xf1, 0x32, 0x87, 0x39, 0x69, 0x93, 0xa0, 0xd2, 0xc1, 0x45, 0xf5, 0xc4,
        0xc7, 0xd5, 0xef, 0x3e, 0xe3, 0x5f, 0xfa, 0xf6, 0xad, 0x5c, 0xba, 0xb0, 0xf8, 0xeb, 0x6a,
        0x54, 0x88,
    ],
    fps: 24,
    youtube_id: "wallclip-24",
    song: "Wall clip v1 24 fps",
    artist: WALL_ARTIST,
    title: "SongPlayer wall clip v1, 24 fps (60 s)",
    duration_ms: WALL_DURATION_MS,
};

/// The 25 fps wall clip (4 270 913 bytes), the one #251's blend runs on.
pub const WALL_25: Clip = Clip {
    name: "wall-25",
    file: "wall-clip-v1-25fps-60s.mp4",
    sha256: [
        0x7b, 0x00, 0xc1, 0x61, 0x1e, 0x9c, 0xed, 0xe9, 0xf3, 0x28, 0xfd, 0xc6, 0x0f, 0xd0, 0x14,
        0xce, 0xed, 0x7a, 0xa9, 0x85, 0x91, 0x5d, 0xf7, 0x8b, 0xbb, 0x5e, 0x87, 0x60, 0xc5, 0x38,
        0xc3, 0x55,
    ],
    fps: 25,
    youtube_id: "wallclip-25",
    song: "Wall clip v1 25 fps",
    artist: WALL_ARTIST,
    title: "SongPlayer wall clip v1, 25 fps (60 s)",
    duration_ms: WALL_DURATION_MS,
};

/// The 30 fps wall clip (4 893 700 bytes): the grid's own rate.
pub const WALL_30: Clip = Clip {
    name: "wall-30",
    file: "wall-clip-v1-30fps-60s.mp4",
    sha256: [
        0x78, 0x9d, 0xfd, 0xbe, 0x41, 0xdb, 0x15, 0x45, 0x1c, 0x3f, 0xf9, 0x46, 0x65, 0x14, 0x06,
        0x43, 0x1b, 0x6a, 0xd8, 0x04, 0x4c, 0xe8, 0x2c, 0x97, 0x1d, 0x2a, 0xd1, 0x48, 0x39, 0xb3,
        0x65, 0x2c,
    ],
    fps: 30,
    youtube_id: "wallclip-30",
    song: "Wall clip v1 30 fps",
    artist: WALL_ARTIST,
    title: "SongPlayer wall clip v1, 30 fps (60 s)",
    duration_ms: WALL_DURATION_MS,
};

/// Every pinned clip, the default (camera-box's) first. A `static`, so
/// [`clip_named`] hands out `&'static` references into it.
pub static CLIPS: [Clip; 4] = [CAMERA_BOX, WALL_24, WALL_25, WALL_30];

/// The listed clip called `name`.
pub fn clip_named(name: &str) -> Option<&'static Clip> {
    CLIPS.iter().find(|clip| clip.name == name)
}
