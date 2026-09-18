//! Karaoke stem separation subsystem (#14).
//!
//! - [`control`] — the process-global live karaoke mode + vocal gain the
//!   dashboard drives and the playback pipeline reads.
//! - [`reader`] — the playback seam: open a plain mix reader or a live
//!   [`sp_decoder::StemMixReader`] over all existing stems, with an original-mix
//!   fallback when stems are missing (#186).
//! - [`separator`] — the Rust wrapper around `scripts/stem_worker.py`.
//! - [`worker`] — the background worker that separates the catalog under the
//!   #154 idle gate.
//!
//! The two stems live next to the mix sidecar, sharing its base name:
//! `{base}_audio.flac` → `{base}_audio_vocals.flac` + `{base}_audio_instrumental.flac`.

use std::path::{Path, PathBuf};

pub mod control;
pub mod progress;
pub mod reader;
pub mod separator;
pub mod worker;

pub use control::KaraokeControl;
pub use worker::StemWorker;

/// Derive the two stem sidecar paths from a mix audio path. Deterministic and
/// pure so BOTH the worker (which writes them) and the playback reader (which
/// reads them) agree without a DB round-trip:
///   `.../foo_audio.flac` → (`.../foo_audio_vocals.flac`, `.../foo_audio_instrumental.flac`)
pub fn stem_paths(audio: &Path) -> (PathBuf, PathBuf) {
    let parent = audio.parent().unwrap_or_else(|| Path::new("."));
    let stem = audio
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("audio");
    let ext = audio.extension().and_then(|e| e.to_str()).unwrap_or("flac");
    (
        parent.join(format!("{stem}_vocals.{ext}")),
        parent.join(format!("{stem}_instrumental.{ext}")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stem_paths_derive_from_mix_sidecar() {
        let (v, i) = stem_paths(Path::new("/cache/Song_Artist_abc123_normalized_audio.flac"));
        assert_eq!(
            v,
            PathBuf::from("/cache/Song_Artist_abc123_normalized_audio_vocals.flac")
        );
        assert_eq!(
            i,
            PathBuf::from("/cache/Song_Artist_abc123_normalized_audio_instrumental.flac")
        );
    }

    #[test]
    fn stem_paths_preserve_gf_suffix_and_dir() {
        let (v, i) = stem_paths(Path::new("/c/X_Y_id_normalized_gf_audio.flac"));
        assert_eq!(
            v,
            PathBuf::from("/c/X_Y_id_normalized_gf_audio_vocals.flac")
        );
        assert_eq!(
            i,
            PathBuf::from("/c/X_Y_id_normalized_gf_audio_instrumental.flac")
        );
    }
}
