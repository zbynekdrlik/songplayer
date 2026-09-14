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
            let vocals = SymphoniaAudioReader::open(&vpath)?;
            let instrumental = SymphoniaAudioReader::open(&ipath)?;
            let (vg, ig) = gain_atomics_for(mode, control);
            info!(
                ?mode,
                vocals = %vpath.display(),
                "karaoke: mixing stems"
            );
            Ok(Box::new(KaraokeAudioReader::new(
                Box::new(vocals),
                Box::new(instrumental),
                vg,
                ig,
            )?))
        }
    }
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
}
