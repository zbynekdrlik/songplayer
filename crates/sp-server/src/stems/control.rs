//! Process-global live karaoke control (#14).
//!
//! The karaoke mode + vocal gain are a single app-wide live setting (one wall,
//! one operator flipping it during a service), so — unlike the per-output
//! `burn_on` registry — they live in ONE process-global [`KaraokeControl`]. The
//! playback pipeline reads it directly at song-open (mode) and per emitted chunk
//! (the vocal-gain slider, via a shared `Arc<AtomicU32>`), so the dashboard
//! slider takes effect live without threading params through `spawn`.

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};

use sp_core::playback::KaraokeMode;

/// Live karaoke mode + vocal gain, shared between the API/engine (writers) and
/// every playback pipeline (reader).
pub struct KaraokeControl {
    /// `KaraokeMode::as_u8` — read when a song opens.
    mode: AtomicU8,
    /// f32 bits, `0.0..=1.0` — the KaraokeLow vocal attenuation. Behind `Arc` so
    /// the same cell can be handed to a `KaraokeAudioReader`, which reads it live
    /// per emitted chunk.
    vocal_gain: Arc<AtomicU32>,
}

/// Default vocal gain for KaraokeLow when no setting is stored (30 %).
pub const DEFAULT_VOCAL_GAIN: f32 = 0.3;

impl KaraokeControl {
    fn new(mode: KaraokeMode, vocal_gain: f32) -> Self {
        Self {
            mode: AtomicU8::new(mode.as_u8()),
            vocal_gain: Arc::new(AtomicU32::new(clamp_gain(vocal_gain).to_bits())),
        }
    }

    /// Current karaoke mode.
    pub fn mode(&self) -> KaraokeMode {
        KaraokeMode::from_u8(self.mode.load(Ordering::Relaxed))
    }

    /// Set the karaoke mode.
    pub fn set_mode(&self, mode: KaraokeMode) {
        self.mode.store(mode.as_u8(), Ordering::Relaxed);
    }

    /// Current vocal gain (`0.0..=1.0`).
    pub fn vocal_gain(&self) -> f32 {
        f32::from_bits(self.vocal_gain.load(Ordering::Relaxed))
    }

    /// Set the vocal gain (clamped to `0.0..=1.0`). Takes effect live — the
    /// playing `KaraokeAudioReader` reads the same atomic each chunk.
    pub fn set_vocal_gain(&self, gain: f32) {
        self.vocal_gain
            .store(clamp_gain(gain).to_bits(), Ordering::Relaxed);
    }

    /// The shared vocal-gain atomic to hand a `KaraokeAudioReader` (KaraokeLow),
    /// so slider moves are heard immediately mid-song.
    pub fn vocal_gain_handle(&self) -> Arc<AtomicU32> {
        Arc::clone(&self.vocal_gain)
    }

    /// Construct a standalone control for tests (not the process global).
    #[cfg(test)]
    pub(crate) fn new_for_test(mode: KaraokeMode, vocal_gain: f32) -> Self {
        Self::new(mode, vocal_gain)
    }
}

fn clamp_gain(g: f32) -> f32 {
    if g.is_finite() {
        g.clamp(0.0, 1.0)
    } else {
        DEFAULT_VOCAL_GAIN
    }
}

static GLOBAL: OnceLock<Arc<KaraokeControl>> = OnceLock::new();

/// Initialise the process-global control from the stored settings. Idempotent —
/// the first call wins; later calls just set the values on the existing control.
pub fn init(mode: KaraokeMode, vocal_gain: f32) -> Arc<KaraokeControl> {
    let ctrl = GLOBAL.get_or_init(|| Arc::new(KaraokeControl::new(mode, vocal_gain)));
    // If the global already existed (e.g. a test set it first), reconcile.
    ctrl.set_mode(mode);
    ctrl.set_vocal_gain(vocal_gain);
    Arc::clone(ctrl)
}

/// The process-global karaoke control, lazily defaulting to `FullMix` + the
/// default vocal gain if `init` was never called (unit tests, degraded boot).
pub fn global() -> Arc<KaraokeControl> {
    Arc::clone(GLOBAL.get_or_init(|| {
        Arc::new(KaraokeControl::new(
            KaraokeMode::FullMix,
            DEFAULT_VOCAL_GAIN,
        ))
    }))
}

/// Seed the process-global control from the `karaoke_mode` + `karaoke_vocal_gain`
/// DB settings, so a restart restores the operator's last choice and pipelines
/// pick it up at song open.
pub async fn init_from_settings(pool: &sqlx::SqlitePool) -> Arc<KaraokeControl> {
    let mode = crate::db::models::get_setting(pool, "karaoke_mode")
        .await
        .ok()
        .flatten()
        .map(|s| KaraokeMode::from_str_lossy(&s))
        .unwrap_or(KaraokeMode::FullMix);
    let gain = crate::db::models::get_setting(pool, "karaoke_vocal_gain")
        .await
        .ok()
        .flatten()
        .and_then(|s| s.trim().parse::<f32>().ok())
        .unwrap_or(DEFAULT_VOCAL_GAIN);
    init(mode, gain)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_control_holds_mode_and_gain() {
        let c = KaraokeControl::new(KaraokeMode::KaraokeLow, 0.4);
        assert_eq!(c.mode(), KaraokeMode::KaraokeLow);
        assert!((c.vocal_gain() - 0.4).abs() < 1e-6);
    }

    #[test]
    fn set_mode_and_gain_update() {
        let c = KaraokeControl::new(KaraokeMode::FullMix, 0.3);
        c.set_mode(KaraokeMode::InstrumentalOnly);
        c.set_vocal_gain(0.75);
        assert_eq!(c.mode(), KaraokeMode::InstrumentalOnly);
        assert!((c.vocal_gain() - 0.75).abs() < 1e-6);
    }

    #[test]
    fn vocal_gain_is_clamped_and_nan_guarded() {
        let c = KaraokeControl::new(KaraokeMode::FullMix, 0.3);
        c.set_vocal_gain(2.0);
        assert!((c.vocal_gain() - 1.0).abs() < 1e-6);
        c.set_vocal_gain(-1.0);
        assert!((c.vocal_gain() - 0.0).abs() < 1e-6);
        c.set_vocal_gain(f32::NAN);
        assert!((c.vocal_gain() - DEFAULT_VOCAL_GAIN).abs() < 1e-6);
    }

    #[test]
    fn vocal_gain_handle_is_shared() {
        let c = KaraokeControl::new(KaraokeMode::KaraokeLow, 0.3);
        let h = c.vocal_gain_handle();
        c.set_vocal_gain(0.9);
        // The handle observes the live change (same atomic cell).
        assert!((f32::from_bits(h.load(Ordering::Relaxed)) - 0.9).abs() < 1e-6);
    }
}
