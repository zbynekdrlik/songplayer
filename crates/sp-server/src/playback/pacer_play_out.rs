//! A song's natural end plays out every sample it buffered (#257).
//!
//! The paced pipeline ends a song once its last frame has been shown (the
//! producer drained, nothing parked). Before #257 it then served ONE standby
//! boundary with the next buffered block and dropped the rest, so a song whose
//! audio outlasted that block lost its last milliseconds. And every block cut
//! short at the song's end counted as an underrun though no sample was
//! missing (the song's audio ends inside it), so every natural end of a song
//! on program read as a loss on the #257 ledger and failed the post-deploy
//! gate. Now:
//!
//! - the pipeline tells the pacer when the decoder handed out the song's last
//!   audio sample ([`Pacer::audio_source_ended`], from
//!   `SplitSyncedDecoder::audio_ended` on the frame that carried it): from
//!   then on a short block is the song's end, never an underrun;
//! - [`Pacer::play_out_audio`] serves frozen standby boundaries, each with the
//!   next block of what is still buffered, until nothing is left (the last
//!   block padded): audio is the master clock, the song ends once its audio
//!   has played;
//! - a song stopped before its source ended (a decode error, a producer gone,
//!   audio the decoder never read because the video ended first) counts one
//!   `end` underrun.
//!
//! A child of `pacer.rs` (1000-line cap), like `pacer_prepare.rs`.

use sp_ndi::AudioFrame;

use super::{PacedSink, Pacer, ServiceOutcome, Standby};
use crate::playback::ndi_health::AudioStats;

/// The most standby boundaries a play-out serves: the one a song end always
/// served, plus one per block the grid's 2 s cap can hold (60 of 1600
/// samples). It never cuts a play-out short.
const PLAY_OUT_MAX_BOUNDARIES: usize = 1;

impl Pacer {
    /// The decoder handed out the song's last audio sample (#257): a block cut
    /// short from now on is the song's own end, not an underrun. The paced
    /// pipeline calls it once the frame that carried that sample was pulled;
    /// a new map (`anchor`, a Resume's flush) clears it.
    pub fn audio_source_ended(&mut self) {
        self.audio_buf.end_source();
    }

    /// A song's natural end (#257): serve frozen standby boundaries (the last
    /// frame held), each with the next block of the audio still buffered,
    /// until nothing is left; the last block is padded. At least one boundary
    /// is served, as before #257. `wait(pacer, until)` sleeps to a boundary
    /// (production: `pipeline_paced::sleep_to_boundary`; tests: set the fake
    /// clock). A song whose source had not ended counts one `end` underrun
    /// first: the audio after what is buffered never plays.
    pub fn play_out_audio<S, W>(&mut self, sink: &mut S, mut wait: W)
    where
        S: PacedSink,
        W: FnMut(&Pacer, i64),
    {
        self.audio_buf.end_song();
        for _ in 0..PLAY_OUT_MAX_BOUNDARIES {
            self.hold_eos_tail_for_standby();
            self.serve_frozen_boundary(sink, &mut wait);
            if self.audio_buf.level_samples() == 0 {
                break;
            }
        }
    }

    /// Serve ONE frozen standby boundary (the held last frame, the held tail
    /// as its block): wait until it is due, then emit it.
    fn serve_frozen_boundary<S, W>(&mut self, sink: &mut S, wait: &mut W)
    where
        S: PacedSink,
        W: FnMut(&Pacer, i64),
    {
        loop {
            match self.service_standby(Standby::FrozenLast, &mut *sink) {
                ServiceOutcome::Wait { until_100ns } => wait(&*self, until_100ns),
                _ => {
                    self.tick_wall();
                    return;
                }
            }
        }
    }

    /// The next block of the buffered audio, zero-filled to
    /// `samples_per_boundary` (#148 rework, item 4);
    /// [`play_out_audio`](Self::play_out_audio) takes one per boundary until
    /// nothing is left (#257). Empty `Vec` when nothing is buffered;
    /// `hold_eos_tail_for_standby` keeps it.
    pub fn take_eos_tail(&mut self) -> Vec<AudioFrame> {
        if self.audio_buf.level_samples() == 0 {
            return Vec::new();
        }
        self.take_boundary_audio()
    }

    /// Snapshot the paced-audio telemetry for the health document (the A/V
    /// alignment itself is reported on `PacingStats`). `underruns` is the sum
    /// of the three phases (#257).
    pub fn audio_stats(&self) -> AudioStats {
        AudioStats {
            enabled: self.enabled,
            samples_per_boundary: self.samples_per_boundary as u64,
            underruns: self.audio_buf.underruns(),
            underruns_start: self.audio_buf.underruns_start(),
            underruns_mid: self.audio_buf.underruns_mid(),
            underruns_end: self.audio_buf.underruns_end(),
            overflows: self.audio_buf.overflows(),
            buffer_ms: self.audio_buf.buffer_ms(),
        }
    }
}
