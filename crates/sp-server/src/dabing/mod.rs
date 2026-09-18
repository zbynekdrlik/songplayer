//! Dubbing D4 (#183) — the Slovak dub synthesis lane.
//!
//! A background worker mirrors the stems/lyrics pattern (10 s tick, heavy slot,
//! BELOW_NORMAL Python child, #171 stall timeout): for each dub-requested,
//! downloaded video whose stems are ready, it streams the ORIGINAL audio through
//! the Gemini Live Translate API (audio→audio, owner ruling #174) and writes the
//! Slovak dub on the video timeline as `<base>_dub.flac`, plus the EN/SK
//! transcripts JSON for D3. Playback opens the 4-stream dub mix
//! (`stems/reader.rs`) when the dub file exists.
//!
//! - [`chunk_plan`] — the pure split (at pauses, `<= 8 min`) + placement/drift
//!   decisions + the ffmpeg `silencedetect` parser.
//! - [`child`] — the Rust wrapper spawning `scripts/dub_worker.py live-translate`.
//! - [`worker`] — the background [`worker::DubWorker`].

pub mod child;
pub mod chunk_plan;
pub mod worker;

pub use worker::DubWorker;

/// The `dub_engine` tag stored on a finished dub row (owner ruling #174: the only
/// path is Gemini Live Translate audio-to-audio).
pub const DUB_ENGINE: &str = "gemini-live-translate";
