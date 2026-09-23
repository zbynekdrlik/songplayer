//! Dubbing D4 (#183) — the Slovak dub synthesis lane.
//!
//! A background worker mirrors the stems/lyrics pattern (10 s tick, heavy slot,
//! BELOW_NORMAL Python child, #171 stall timeout): for each dub-requested,
//! downloaded video it streams the video's audio (the vocals stem when it is
//! ready, else the original) through ONE continuous Gemini Live Translate
//! session (audio→audio, owner ruling #174; #184 round H step 2 — the speaker's
//! own voice, the model a setting) and writes the Slovak dub on the video
//! timeline as `<base>_dub.flac`, plus the EN/SK transcripts JSON for D3.
//! Playback opens the dub mix (`stems/reader.rs`) when the dub file exists.
//!
//! - [`child`] — the Rust wrapper spawning `scripts/dub_worker.py live-translate`.
//! - [`worker`] — the background [`worker::DubWorker`].
//! - [`subtitles`] / [`subtitles_store`] — the D3 EN/SK subtitle track.

pub mod child;
pub mod subtitles;
pub mod subtitles_store;
pub mod worker;

pub use worker::DubWorker;

/// The `dub_engine` tag stored on a finished dub row (owner ruling #174: the only
/// path is Gemini Live Translate audio-to-audio).
pub const DUB_ENGINE: &str = "gemini-live-translate";
