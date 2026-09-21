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
pub mod queue_tiers; // #195 in-use-first stems queue tier inputs
pub mod reader;
pub mod separator;
pub mod worker;

pub use control::MixControl;
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

/// Strip a trailing `_audio` from a mix sidecar's file stem to recover the shared
/// base name (`{song}_{artist}_{id}_normalized[_gf]`). `.../foo_audio.flac` →
/// `foo`. Pure so the dub worker (which writes) and the playback reader (which
/// reads) agree without a DB round-trip.
fn dub_base(audio: &Path) -> (PathBuf, String, String) {
    let parent = audio
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let stem = audio
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("audio");
    let base = stem.strip_suffix("_audio").unwrap_or(stem).to_string();
    let ext = audio
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("flac")
        .to_string();
    (parent, base, ext)
}

/// Derive the dub track path from a mix audio path (#183 D4):
///   `.../foo_normalized_audio.flac` → `.../foo_normalized_dub.flac`
/// Deterministic + pure so both the dub worker and `open_audio_stream` agree.
pub fn dub_path(audio: &Path) -> PathBuf {
    let (parent, base, ext) = dub_base(audio);
    parent.join(format!("{base}_dub.{ext}"))
}

/// Derive the EN/SK dub transcripts JSON path from a mix audio path (#183 D4,
/// consumed by D3):
///   `.../foo_normalized_audio.flac` → `.../foo_normalized_dub_transcripts.json`
pub fn dub_transcripts_path(audio: &Path) -> PathBuf {
    let (parent, base, _ext) = dub_base(audio);
    parent.join(format!("{base}_dub_transcripts.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dub_path_strips_audio_suffix() {
        assert_eq!(
            dub_path(Path::new("/cache/Song_Artist_abc123_normalized_audio.flac")),
            PathBuf::from("/cache/Song_Artist_abc123_normalized_dub.flac")
        );
        assert_eq!(
            dub_path(Path::new("/c/X_Y_id_normalized_gf_audio.flac")),
            PathBuf::from("/c/X_Y_id_normalized_gf_dub.flac")
        );
    }

    #[test]
    fn dub_transcripts_path_strips_audio_suffix() {
        assert_eq!(
            dub_transcripts_path(Path::new("/cache/Song_Artist_abc123_normalized_audio.flac")),
            PathBuf::from("/cache/Song_Artist_abc123_normalized_dub_transcripts.json")
        );
    }

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
