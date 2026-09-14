//! Playback audio-source seam (#14).
//!
//! [`open_audio_stream`] is what the decode loop calls in place of a bare
//! `SymphoniaAudioReader::open`: it consults the live [`KaraokeControl`] and
//! returns either a plain mix reader (FullMix, or a non-FullMix mode whose stems
//! are not generated yet — the safe fallback) or a stem-mixing
//! [`sp_decoder::KaraokeAudioReader`].

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;

use sp_core::playback::KaraokeMode;
use sp_decoder::{
    AudioStream, DecoderError, KaraokeAudioReader, SymphoniaAudioReader, shared_gain,
};
use tracing::{info, warn};

use crate::stems::control::KaraokeControl;

/// Which audio source the decode loop should open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioSource {
    /// Play the original mix `{id}_audio.flac`.
    FullMix,
    /// Mix the vocals + instrumental stems.
    Stems,
}

/// Pure decision: open stems only for a non-FullMix mode whose BOTH stems exist;
/// otherwise fall back to the full mix. Extracted so the branch logic is
/// unit-tested without touching the filesystem.
pub fn choose_source(
    mode: KaraokeMode,
    vocals_exist: bool,
    instrumental_exist: bool,
) -> AudioSource {
    if mode.needs_stems() && vocals_exist && instrumental_exist {
        AudioSource::Stems
    } else {
        AudioSource::FullMix
    }
}

/// The `(vocal_gain, instrumental_gain)` atomics to hand a
/// [`KaraokeAudioReader`] for `mode`. KaraokeLow shares the LIVE vocal-gain
/// atomic from `control` so the dashboard slider is heard mid-song; the other
/// modes use fixed 0/1 gains.
pub fn gain_atomics_for(
    mode: KaraokeMode,
    control: &KaraokeControl,
) -> (Arc<AtomicU32>, Arc<AtomicU32>) {
    match mode {
        KaraokeMode::KaraokeLow => (control.vocal_gain_handle(), shared_gain(1.0)),
        KaraokeMode::VocalsOnly => (shared_gain(1.0), shared_gain(0.0)),
        KaraokeMode::InstrumentalOnly => (shared_gain(0.0), shared_gain(1.0)),
        // FullMix never mixes stems (handled by choose_source); mix-equivalent.
        KaraokeMode::FullMix => (shared_gain(1.0), shared_gain(0.0)),
    }
}

/// Open the audio stream for `audio_path` honouring the live karaoke control.
/// Returns a boxed [`AudioStream`] ready to hand to `SplitSyncedDecoder::new`.
pub fn open_audio_stream(
    audio_path: &Path,
    control: &KaraokeControl,
) -> Result<Box<dyn AudioStream>, DecoderError> {
    let mode = control.mode();
    if !mode.needs_stems() {
        return Ok(Box::new(SymphoniaAudioReader::open(audio_path)?));
    }

    let (vpath, ipath) = crate::stems::stem_paths(audio_path);
    let source = choose_source(mode, vpath.exists(), ipath.exists());
    match source {
        AudioSource::FullMix => {
            warn!(
                audio = %audio_path.display(),
                ?mode,
                "karaoke: stems missing — falling back to FullMix"
            );
            Ok(Box::new(SymphoniaAudioReader::open(audio_path)?))
        }
        AudioSource::Stems => {
            // Defence in depth: a stem may EXIST yet fail to open — a torn file
            // from a killed separator (the worker now writes atomically, so this
            // is the residual case), a stem deleted between the exists() check
            // above and here, or a rate/channel disagreement in
            // KaraokeAudioReader::new. Any such failure degrades to FullMix (the
            // real mix always plays) rather than erroring the whole pipeline and
            // taking the wall dark for that song.
            match build_stem_reader(&vpath, &ipath, mode, control) {
                Ok(reader) => {
                    info!(
                        ?mode,
                        vocals = %vpath.display(),
                        "karaoke: mixing stems"
                    );
                    Ok(reader)
                }
                Err(e) => {
                    warn!(
                        audio = %audio_path.display(),
                        ?mode,
                        %e,
                        "karaoke: stems present but unreadable — falling back to FullMix"
                    );
                    Ok(Box::new(SymphoniaAudioReader::open(audio_path)?))
                }
            }
        }
    }
}

/// Open both stem readers and wrap them in a [`KaraokeAudioReader`]. Any error
/// (open failure, rate/channel mismatch) is returned so [`open_audio_stream`]
/// can fall back to the full mix instead of failing the pipeline.
fn build_stem_reader(
    vpath: &Path,
    ipath: &Path,
    mode: KaraokeMode,
    control: &KaraokeControl,
) -> Result<Box<dyn AudioStream>, DecoderError> {
    let vocals = SymphoniaAudioReader::open(vpath)?;
    let instrumental = SymphoniaAudioReader::open(ipath)?;
    let (vg, ig) = gain_atomics_for(mode, control);
    Ok(Box::new(KaraokeAudioReader::new(
        Box::new(vocals),
        Box::new(instrumental),
        vg,
        ig,
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stems::control::KaraokeControl;
    use std::sync::atomic::Ordering;

    #[test]
    fn full_mix_never_opens_stems() {
        assert_eq!(
            choose_source(KaraokeMode::FullMix, true, true),
            AudioSource::FullMix
        );
    }

    #[test]
    fn non_full_mix_with_both_stems_opens_stems() {
        assert_eq!(
            choose_source(KaraokeMode::KaraokeLow, true, true),
            AudioSource::Stems
        );
        assert_eq!(
            choose_source(KaraokeMode::VocalsOnly, true, true),
            AudioSource::Stems
        );
        assert_eq!(
            choose_source(KaraokeMode::InstrumentalOnly, true, true),
            AudioSource::Stems
        );
    }

    #[test]
    fn missing_either_stem_falls_back_to_full_mix() {
        assert_eq!(
            choose_source(KaraokeMode::KaraokeLow, false, true),
            AudioSource::FullMix
        );
        assert_eq!(
            choose_source(KaraokeMode::KaraokeLow, true, false),
            AudioSource::FullMix
        );
        assert_eq!(
            choose_source(KaraokeMode::KaraokeLow, false, false),
            AudioSource::FullMix
        );
    }

    fn read(a: &Arc<AtomicU32>) -> f32 {
        f32::from_bits(a.load(Ordering::Relaxed))
    }

    #[test]
    fn karaoke_low_shares_live_vocal_gain_and_full_instrumental() {
        let ctrl = KaraokeControl::new_for_test(KaraokeMode::KaraokeLow, 0.25);
        let (vg, ig) = gain_atomics_for(KaraokeMode::KaraokeLow, &ctrl);
        assert!((read(&vg) - 0.25).abs() < 1e-6);
        assert!((read(&ig) - 1.0).abs() < 1e-6);
        // The vocal-gain atomic is the LIVE one — a slider move is observed.
        ctrl.set_vocal_gain(0.6);
        assert!((read(&vg) - 0.6).abs() < 1e-6);
    }

    #[test]
    fn vocals_only_and_instrumental_only_use_fixed_gains() {
        let ctrl = KaraokeControl::new_for_test(KaraokeMode::VocalsOnly, 0.3);
        let (vg, ig) = gain_atomics_for(KaraokeMode::VocalsOnly, &ctrl);
        assert_eq!((read(&vg), read(&ig)), (1.0, 0.0));
        let (vg2, ig2) = gain_atomics_for(KaraokeMode::InstrumentalOnly, &ctrl);
        assert_eq!((read(&vg2), read(&ig2)), (0.0, 1.0));
    }

    // ── open_audio_stream I/O behaviour (real decodable fixture) ──────────────
    // A real 48 kHz stereo FLAC lives in sp-decoder's test fixtures; reuse it as
    // both the mix and (copied) the stems so these tests exercise the actual
    // SymphoniaAudioReader open + KaraokeAudioReader path on Linux CI.
    const FIXTURE_FLAC: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../sp-decoder/tests/fixtures/silent_3s.flac"
    );

    /// Both stems are real, decodable FLACs → the stem-mixing reader is built and
    /// reports the shared 48 kHz stereo format.
    #[test]
    fn stems_present_and_readable_builds_karaoke_reader() {
        let dir = tempfile::tempdir().unwrap();
        let mix = dir.path().join("Song_Artist_id_normalized_audio.flac");
        std::fs::copy(FIXTURE_FLAC, &mix).unwrap();
        let (vpath, ipath) = crate::stems::stem_paths(&mix);
        std::fs::copy(FIXTURE_FLAC, &vpath).unwrap();
        std::fs::copy(FIXTURE_FLAC, &ipath).unwrap();

        let ctrl = KaraokeControl::new_for_test(KaraokeMode::KaraokeLow, 0.3);
        let stream = open_audio_stream(&mix, &ctrl).expect("stems should open");
        assert_eq!(stream.sample_rate(), 48_000);
        assert_eq!(stream.channels(), 2);
    }

    /// Stems EXIST but are corrupt (garbage bytes, not valid FLAC). The reader
    /// must NOT error the pipeline — it falls back to the real mix so the wall
    /// keeps playing. This is the #14 review 🟡 defence-in-depth guard.
    #[test]
    fn corrupt_stems_fall_back_to_full_mix() {
        let dir = tempfile::tempdir().unwrap();
        let mix = dir.path().join("Song_Artist_id_normalized_audio.flac");
        std::fs::copy(FIXTURE_FLAC, &mix).unwrap();
        let (vpath, ipath) = crate::stems::stem_paths(&mix);
        // A torn/half-written file: exists (so choose_source picks Stems) but is
        // not a decodable FLAC.
        std::fs::write(&vpath, b"not a flac, torn write").unwrap();
        std::fs::write(&ipath, b"not a flac, torn write").unwrap();

        let ctrl = KaraokeControl::new_for_test(KaraokeMode::InstrumentalOnly, 0.3);
        // Must succeed (fell back to the mix), NOT return Err.
        let stream = open_audio_stream(&mix, &ctrl).expect("must fall back to FullMix, not error");
        assert_eq!(stream.sample_rate(), 48_000);
        assert_eq!(stream.channels(), 2);
    }

    /// A missing stem (only one of the pair present) also degrades to FullMix via
    /// `choose_source`, and `open_audio_stream` opens the real mix.
    #[test]
    fn one_missing_stem_opens_full_mix() {
        let dir = tempfile::tempdir().unwrap();
        let mix = dir.path().join("Song_Artist_id_normalized_audio.flac");
        std::fs::copy(FIXTURE_FLAC, &mix).unwrap();
        let (vpath, _ipath) = crate::stems::stem_paths(&mix);
        std::fs::copy(FIXTURE_FLAC, &vpath).unwrap(); // instrumental absent

        let ctrl = KaraokeControl::new_for_test(KaraokeMode::KaraokeLow, 0.3);
        let stream = open_audio_stream(&mix, &ctrl).expect("full mix should open");
        assert_eq!(stream.sample_rate(), 48_000);
    }
}
