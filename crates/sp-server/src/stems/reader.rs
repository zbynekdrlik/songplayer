//! Playback audio-source seam (#14, #186).
//!
//! [`open_audio_stream`] is what the decode loop calls in place of a bare
//! `SymphoniaAudioReader::open`. Since #186 the karaoke MODE no longer decides
//! WHICH files open — it is a live gain preset. This seam opens EVERYTHING that
//! exists, once:
//!   - both stems present → `[original, vocals, instrumental]` in a live
//!     [`sp_decoder::StemMixReader`] whose three gain atomics come from the
//!     process-global [`KaraokeControl`]; a preset change is then a gain write,
//!     never a reopen (the seconds-of-silence dropout #186 fixes).
//!   - stems missing (or only one of the pair) → the plain original mix reader,
//!     and presets are a no-op for that song (#177 "nedostupné").

use std::path::Path;

use sp_decoder::{AudioStream, DecoderError, StemMixReader, SymphoniaAudioReader};
use tracing::{info, warn};

use crate::stems::control::KaraokeControl;

/// One of the streams the decode loop can open — INDEPENDENT of the karaoke
/// mode (#186). A song opens everything that exists once; the mode is a live
/// gain preset over those streams, applied without reopening.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StemRole {
    Original,
    Vocals,
    Instrumental,
}

/// Pure chooser: the stream set is `[Original, Vocals, Instrumental]` when BOTH
/// stems exist, else `[Original]` alone. Mode-INDEPENDENT — extracted so the
/// branch is unit-tested without touching the filesystem.
pub fn stream_roles(vocals_exist: bool, instrumental_exist: bool) -> &'static [StemRole] {
    if vocals_exist && instrumental_exist {
        &[StemRole::Original, StemRole::Vocals, StemRole::Instrumental]
    } else {
        &[StemRole::Original]
    }
}

/// Which reader `open_audio_stream` will build for a given stem availability.
/// Returned as an ENUM so the choice is observable in a unit test — both reader
/// kinds share the 48 kHz stereo format, so the opened reader itself cannot be
/// told apart through the `AudioStream` interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioSourceKind {
    /// A dub track AND both stems exist → a live 4-stream
    /// `[original, vocals, instrumental, dub]` mix (#183 D4).
    DubMix,
    /// Both stems exist → a live `[original, vocals, instrumental]` mix.
    StemMix,
    /// Stems missing / incomplete → the plain original mix (presets no-op, #177).
    PlainMix,
}

/// Pure reader-choice (#183 D4): `DubMix` when a dub track AND both stems exist
/// (a dub video plays the 4-stream mix); else `StemMix` for the full stem triple;
/// else `PlainMix`. A dub track without both stems degrades to the stem/plain
/// choice — the ambient bed needs the instrumental stem.
pub fn audio_source_kind(
    vocals_exist: bool,
    instrumental_exist: bool,
    dub_exist: bool,
) -> AudioSourceKind {
    if dub_exist && vocals_exist && instrumental_exist {
        return AudioSourceKind::DubMix;
    }
    match stream_roles(vocals_exist, instrumental_exist) {
        [StemRole::Original, StemRole::Vocals, StemRole::Instrumental] => AudioSourceKind::StemMix,
        _ => AudioSourceKind::PlainMix,
    }
}

/// Open the audio stream for `audio_path` honouring the live karaoke control.
/// Returns a boxed [`AudioStream`] ready to hand to `SplitSyncedDecoder::new`.
pub fn open_audio_stream(
    audio_path: &Path,
    control: &KaraokeControl,
) -> Result<Box<dyn AudioStream>, DecoderError> {
    let (vpath, ipath) = crate::stems::stem_paths(audio_path);
    let dpath = crate::stems::dub_path(audio_path);
    match audio_source_kind(vpath.exists(), ipath.exists(), dpath.exists()) {
        // A dub track + both stems → open all four and mix live: original voice
        // (vocals stem) ↔ Slovak dub over the ambient (instrumental) bed, blended
        // by the live dub ratio (#183 D4). Any open failure degrades to the stem
        // or plain mix so a dub video still plays.
        AudioSourceKind::DubMix => {
            match build_dub_reader(audio_path, &vpath, &ipath, &dpath, control) {
                Ok(reader) => {
                    info!(
                        original = %audio_path.display(),
                        dub = %dpath.display(),
                        "dub: mixing 4 streams (live ratio, no reopen)"
                    );
                    Ok(reader)
                }
                Err(e) => {
                    warn!(
                        audio = %audio_path.display(),
                        %e,
                        "dub: track present but 4-stream open failed — falling back to the stem/plain mix"
                    );
                    // Fall back to the stem mixer (or plain) below.
                    match build_stem_reader(audio_path, &vpath, &ipath, control) {
                        Ok(r) => Ok(r),
                        Err(_) => Ok(Box::new(SymphoniaAudioReader::open(audio_path)?)),
                    }
                }
            }
        }
        // Both stems exist → open all three and mix live (a preset change writes
        // gains, never a reopen).
        AudioSourceKind::StemMix => match build_stem_reader(audio_path, &vpath, &ipath, control) {
            Ok(reader) => {
                info!(
                    original = %audio_path.display(),
                    vocals = %vpath.display(),
                    "karaoke: mixing stems (live preset, no reopen)"
                );
                Ok(reader)
            }
            Err(e) => {
                // Defence in depth: a stem may EXIST yet fail to open — a torn
                // file from a killed separator, a stem deleted between the
                // exists() check and here, or a rate/channel disagreement in
                // StemMixReader::new. Any such failure degrades to the original
                // mix (which always plays) rather than taking the wall dark.
                warn!(
                    audio = %audio_path.display(),
                    %e,
                    "karaoke: stems present but unreadable — falling back to the original mix"
                );
                Ok(Box::new(SymphoniaAudioReader::open(audio_path)?))
            }
        },
        // No stems (or an incomplete pair): play the original mix. Presets are a
        // no-op for this song (#177) — the plain reader never sees the gains.
        AudioSourceKind::PlainMix => Ok(Box::new(SymphoniaAudioReader::open(audio_path)?)),
    }
}

/// Open `[original, vocals, instrumental]` and wrap them in a live
/// [`StemMixReader`] fed the control's three gain atomics. Any error (open
/// failure, rate/channel mismatch) is returned so [`open_audio_stream`] can fall
/// back to the plain mix instead of failing the pipeline.
fn build_stem_reader(
    original_path: &Path,
    vpath: &Path,
    ipath: &Path,
    control: &KaraokeControl,
) -> Result<Box<dyn AudioStream>, DecoderError> {
    let original = SymphoniaAudioReader::open(original_path)?;
    let vocals = SymphoniaAudioReader::open(vpath)?;
    let instrumental = SymphoniaAudioReader::open(ipath)?;
    // gain_handles() is [original, vocals, instrumental] — the SAME order the
    // streams are pushed below, so gain_k applies to stream_k.
    let gains = control.gain_handles();
    Ok(Box::new(StemMixReader::new(
        vec![Box::new(original), Box::new(vocals), Box::new(instrumental)],
        gains.to_vec(),
    )?))
}

/// Open `[original, vocals, instrumental, dub]` and wrap them in a live 4-stream
/// [`StemMixReader`] fed the control's four dub gain atomics (#183 D4). Any error
/// (open failure, rate/channel mismatch — e.g. a dub not written at the stem 48
/// kHz stereo format) is returned so [`open_audio_stream`] can fall back.
fn build_dub_reader(
    original_path: &Path,
    vpath: &Path,
    ipath: &Path,
    dpath: &Path,
    control: &KaraokeControl,
) -> Result<Box<dyn AudioStream>, DecoderError> {
    let original = SymphoniaAudioReader::open(original_path)?;
    let vocals = SymphoniaAudioReader::open(vpath)?;
    let instrumental = SymphoniaAudioReader::open(ipath)?;
    let dub = SymphoniaAudioReader::open(dpath)?;
    // dub_gain_handles() is [original, vocals, instrumental, dub] — the SAME order
    // the streams are pushed below, so gain_k applies to stream_k.
    let gains = control.dub_gain_handles();
    Ok(Box::new(StemMixReader::new(
        vec![
            Box::new(original),
            Box::new(vocals),
            Box::new(instrumental),
            Box::new(dub),
        ],
        gains.to_vec(),
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stems::control::KaraokeControl;
    use sp_core::playback::KaraokeMode;

    #[test]
    fn both_stems_open_all_three_regardless_of_mode() {
        assert_eq!(
            stream_roles(true, true),
            &[StemRole::Original, StemRole::Vocals, StemRole::Instrumental]
        );
    }

    #[test]
    fn missing_either_stem_opens_original_only() {
        assert_eq!(stream_roles(false, true), &[StemRole::Original]);
        assert_eq!(stream_roles(true, false), &[StemRole::Original]);
        assert_eq!(stream_roles(false, false), &[StemRole::Original]);
    }

    #[test]
    fn audio_source_kind_is_stem_mix_only_with_both_stems() {
        // The observable reader choice (both reader kinds share the 48 kHz stereo
        // format, so the opened reader cannot be told apart directly). No dub.
        assert_eq!(audio_source_kind(true, true, false), AudioSourceKind::StemMix);
        assert_eq!(audio_source_kind(false, true, false), AudioSourceKind::PlainMix);
        assert_eq!(audio_source_kind(true, false, false), AudioSourceKind::PlainMix);
        assert_eq!(
            audio_source_kind(false, false, false),
            AudioSourceKind::PlainMix
        );
    }

    #[test]
    fn audio_source_kind_is_dub_mix_only_with_dub_and_both_stems() {
        // #183 D4: a dub track needs BOTH stems (the ambient bed is the
        // instrumental stem) → 4-stream DubMix; otherwise it degrades.
        assert_eq!(audio_source_kind(true, true, true), AudioSourceKind::DubMix);
        // Dub present but a stem missing → not a dub mix (falls back).
        assert_eq!(audio_source_kind(false, true, true), AudioSourceKind::PlainMix);
        assert_eq!(audio_source_kind(true, false, true), AudioSourceKind::PlainMix);
    }

    /// A dub track + both stems → the 4-stream dub reader opens and reports the
    /// shared 48 kHz stereo format.
    #[test]
    fn dub_present_with_stems_builds_four_stream_reader() {
        let dir = tempfile::tempdir().unwrap();
        let mix = dir.path().join("Song_Artist_id_normalized_audio.flac");
        std::fs::copy(FIXTURE_FLAC, &mix).unwrap();
        let (vpath, ipath) = crate::stems::stem_paths(&mix);
        std::fs::copy(FIXTURE_FLAC, &vpath).unwrap();
        std::fs::copy(FIXTURE_FLAC, &ipath).unwrap();
        let dpath = crate::stems::dub_path(&mix);
        std::fs::copy(FIXTURE_FLAC, &dpath).unwrap();

        assert_eq!(
            audio_source_kind(vpath.exists(), ipath.exists(), dpath.exists()),
            AudioSourceKind::DubMix
        );
        let ctrl = KaraokeControl::new_for_test(KaraokeMode::FullMix, 0.3);
        let stream = open_audio_stream(&mix, &ctrl).expect("dub 4-stream should open");
        assert_eq!(stream.sample_rate(), 48_000);
        assert_eq!(stream.channels(), 2);
    }

    // ── open_audio_stream I/O behaviour (real decodable fixture) ──────────────
    // A real 48 kHz stereo FLAC lives in sp-decoder's test fixtures; reuse it as
    // the mix and (copied) the stems so these tests exercise the actual
    // SymphoniaAudioReader open + StemMixReader path on Linux CI.
    const FIXTURE_FLAC: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../sp-decoder/tests/fixtures/silent_3s.flac"
    );

    /// Both stems are real, decodable FLACs → the stem-mixing reader is built and
    /// reports the shared 48 kHz stereo format. Mode is irrelevant to WHICH files
    /// open now (#186), so pick a non-FullMix mode.
    #[test]
    fn stems_present_and_readable_builds_stem_reader() {
        let dir = tempfile::tempdir().unwrap();
        let mix = dir.path().join("Song_Artist_id_normalized_audio.flac");
        std::fs::copy(FIXTURE_FLAC, &mix).unwrap();
        let (vpath, ipath) = crate::stems::stem_paths(&mix);
        std::fs::copy(FIXTURE_FLAC, &vpath).unwrap();
        std::fs::copy(FIXTURE_FLAC, &ipath).unwrap();

        let ctrl = KaraokeControl::new_for_test(KaraokeMode::VocalsOnly, 0.3);
        let stream = open_audio_stream(&mix, &ctrl).expect("stems should open");
        assert_eq!(stream.sample_rate(), 48_000);
        assert_eq!(stream.channels(), 2);
    }

    /// #186: even FullMix opens the mixer when stems exist (the FullMix preset
    /// plays the original), so a later switch FROM FullMix costs no reopen.
    #[test]
    fn full_mix_mode_still_opens_all_three_when_stems_exist() {
        let dir = tempfile::tempdir().unwrap();
        let mix = dir.path().join("Song_Artist_id_normalized_audio.flac");
        std::fs::copy(FIXTURE_FLAC, &mix).unwrap();
        let (vpath, ipath) = crate::stems::stem_paths(&mix);
        std::fs::copy(FIXTURE_FLAC, &vpath).unwrap();
        std::fs::copy(FIXTURE_FLAC, &ipath).unwrap();

        let ctrl = KaraokeControl::new_for_test(KaraokeMode::FullMix, 0.3);
        let stream = open_audio_stream(&mix, &ctrl).expect("mixer should open");
        assert_eq!(stream.sample_rate(), 48_000);
        assert_eq!(stream.channels(), 2);
    }

    /// Stems EXIST but are corrupt (garbage bytes, not valid FLAC). The reader
    /// must NOT error the pipeline — it falls back to the real mix so the wall
    /// keeps playing (the #14 review 🟡 defence-in-depth guard, kept for #186).
    #[test]
    fn corrupt_stems_fall_back_to_original_mix() {
        let dir = tempfile::tempdir().unwrap();
        let mix = dir.path().join("Song_Artist_id_normalized_audio.flac");
        std::fs::copy(FIXTURE_FLAC, &mix).unwrap();
        let (vpath, ipath) = crate::stems::stem_paths(&mix);
        std::fs::write(&vpath, b"not a flac, torn write").unwrap();
        std::fs::write(&ipath, b"not a flac, torn write").unwrap();

        let ctrl = KaraokeControl::new_for_test(KaraokeMode::InstrumentalOnly, 0.3);
        let stream = open_audio_stream(&mix, &ctrl).expect("must fall back to the mix, not error");
        assert_eq!(stream.sample_rate(), 48_000);
        assert_eq!(stream.channels(), 2);
    }

    /// A missing stem (only one of the pair present) degrades to the original
    /// mix via `stream_roles`, and `open_audio_stream` opens the real mix.
    #[test]
    fn one_missing_stem_opens_original_only() {
        let dir = tempfile::tempdir().unwrap();
        let mix = dir.path().join("Song_Artist_id_normalized_audio.flac");
        std::fs::copy(FIXTURE_FLAC, &mix).unwrap();
        let (vpath, _ipath) = crate::stems::stem_paths(&mix);
        std::fs::copy(FIXTURE_FLAC, &vpath).unwrap(); // instrumental absent

        let ctrl = KaraokeControl::new_for_test(KaraokeMode::KaraokeLow, 0.3);
        let stream = open_audio_stream(&mix, &ctrl).expect("original mix should open");
        assert_eq!(stream.sample_rate(), 48_000);
    }
}
