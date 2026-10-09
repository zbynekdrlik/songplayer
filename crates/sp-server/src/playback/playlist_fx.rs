//! A playlist's own sound, live (#242): the process-wide register of every
//! playlist's [`PlaylistFx`] and the audio-stream wrapper that applies it.
//!
//! - [`FxRegister`] holds one [`FxSlot`] per playlist id: the settings and a
//!   generation counter. Startup loads them from the rows ([`load_all`]);
//!   the API writes a playlist's row FIRST and then [`FxRegister::set`]s it.
//! - The decode thread wraps each song's audio stream right after the stem
//!   mix ([`wrap`], `pipeline_paced::run_decode_producer`). The wrapper reads
//!   its slot's generation on every chunk and, when it moved, hands the new
//!   settings to its [`FxProcessor`] (a ramp, a crossfade: no click). The
//!   preview tap and the pacer get the processed audio, so the dashboard
//!   preview sounds like the program, and a cut or fade between playlists
//!   mixes already-processed audio.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use sp_core::audio_fx::PlaylistFx;
use sp_decoder::{AudioStream, DecodedAudioFrame, DecoderError, MediaStream};
use sqlx::SqlitePool;
use tracing::{info, warn};

use crate::playback::playlist_fx_dsp::FxProcessor;

/// One playlist's live settings.
#[derive(Debug, Default)]
pub struct FxSlot {
    generation: AtomicU64,
    fx: Mutex<PlaylistFx>,
}

impl FxSlot {
    /// The settings and the generation they carry.
    pub fn read(&self) -> (u64, PlaylistFx) {
        let fx = self.fx.lock().unwrap_or_else(|p| p.into_inner());
        (self.generation.load(Ordering::Acquire), fx.clone())
    }

    /// The generation now (moves on every [`FxRegister::set`]).
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    fn store(&self, fx: PlaylistFx) {
        let mut held = self.fx.lock().unwrap_or_else(|p| p.into_inner());
        *held = fx;
        self.generation.fetch_add(1, Ordering::AcqRel);
    }
}

/// Every playlist's live settings.
#[derive(Debug, Default)]
pub struct FxRegister {
    slots: Mutex<HashMap<i64, Arc<FxSlot>>>,
}

impl FxRegister {
    /// The slot of `playlist_id` (made at the default, untouched sound, when
    /// none is held: the slot a pipeline gets is the one a later `set`
    /// changes).
    pub fn slot(&self, playlist_id: i64) -> Arc<FxSlot> {
        let mut slots = self.slots.lock().unwrap_or_else(|p| p.into_inner());
        slots.entry(playlist_id).or_default().clone()
    }

    /// Give `playlist_id` the settings `fx` (every stream of it follows).
    pub fn set(&self, playlist_id: i64, fx: PlaylistFx) {
        self.slot(playlist_id).store(fx);
    }
}

/// The process-wide register.
pub fn global() -> &'static FxRegister {
    static REGISTER: OnceLock<FxRegister> = OnceLock::new();
    REGISTER.get_or_init(FxRegister::default)
}

/// Startup: every playlist row's settings into `register`; returns how many
/// playlists have a sound of their own (not the untouched default). A
/// failed read leaves every playlist at the default sound, with a WARN.
pub async fn load_all(pool: &SqlitePool, register: &FxRegister) -> usize {
    match crate::db::models_playlist_fx::all_playlist_fx(pool).await {
        Ok(all) => {
            let shaped = all.iter().filter(|(_, fx)| !fx.is_identity()).count();
            for (id, fx) in all {
                register.set(id, fx);
            }
            info!(shaped, "playlist audio: own volume / EQ loaded");
            shaped
        }
        Err(e) => {
            warn!(%e, "playlist audio: reading the settings failed — every playlist plays untouched");
            0
        }
    }
}

/// A song's audio stream with its playlist's sound applied.
pub struct FxStream {
    inner: Box<dyn AudioStream>,
    slot: Arc<FxSlot>,
    generation: u64,
    processor: FxProcessor,
}

/// Wrap `inner` (a song of `playlist_id`) in its playlist's sound from
/// `register`.
pub fn wrap_with(
    register: &FxRegister,
    inner: Box<dyn AudioStream>,
    playlist_id: i64,
) -> Box<dyn AudioStream> {
    let slot = register.slot(playlist_id);
    let (generation, fx) = slot.read();
    let processor = FxProcessor::new(&fx, inner.sample_rate(), inner.channels());
    Box::new(FxStream {
        inner,
        slot,
        generation,
        processor,
    })
}

/// [`wrap_with`] the process-wide register (the decode thread's call).
pub fn wrap(inner: Box<dyn AudioStream>, playlist_id: i64) -> Box<dyn AudioStream> {
    wrap_with(global(), inner, playlist_id)
}

impl MediaStream for FxStream {
    fn duration_ms(&self) -> u64 {
        self.inner.duration_ms()
    }

    fn seek(&mut self, position_ms: u64) -> Result<(), DecoderError> {
        self.inner.seek(position_ms)
    }
}

impl AudioStream for FxStream {
    fn next_samples(&mut self) -> Result<Option<DecodedAudioFrame>, DecoderError> {
        let Some(mut frame) = self.inner.next_samples()? else {
            return Ok(None);
        };
        if self.slot.generation() != self.generation {
            let (generation, fx) = self.slot.read();
            self.generation = generation;
            self.processor.update(&fx);
        }
        self.processor.process(&mut frame.data);
        Ok(Some(frame))
    }

    fn sample_rate(&self) -> u32 {
        self.inner.sample_rate()
    }

    fn channels(&self) -> u16 {
        self.inner.channels()
    }
}

#[cfg(test)]
#[path = "playlist_fx_tests.rs"]
mod tests;
