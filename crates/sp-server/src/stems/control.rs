//! Process-global live mixer control (#184 round G, was #14/#186 `KaraokeControl`).
//!
//! The mixer is ONE app-wide live console (one wall, one operator) — three
//! independent faders `[vokály, podklad, dabing]`. Unlike the per-output `burn_on`
//! registry it lives in ONE process-global [`MixControl`].
//!
//! Since #186 a fader change is a live GAIN write, not a choice of which files to
//! open: the control holds the three fader positions PLUS three DERIVED per-stream
//! gain sets — `[original, vocals, instrumental]` (a song), `[original, vocals,
//! instrumental, dub]` (a dub with stems), and `[original, dub]` (a dub without
//! stems). `set_faders` recomputes ALL THREE in lock-step from the pure
//! `sp_core::mixer_model::stream_gains_*`, and every playing
//! [`sp_decoder::StemMixReader`] reads and ramps toward whichever set applies — so
//! a fader move is heard immediately with NO pipeline reopen (the #186 seam).

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};

use sp_core::mixer_model::{
    MixFaders, stream_gains_dub, stream_gains_dub_no_stems, stream_gains_song,
};

/// Live mixer faders + the derived per-stream gain atomics, shared between the
/// API/engine (writers) and every playback pipeline (reader).
pub struct MixControl {
    /// The three live fader positions (f32 bits), `0.0..=1.0`.
    faders: [Arc<AtomicU32>; 3],
    /// The LIVE per-stream target gains `[original, vocals, instrumental]` a song's
    /// 3-stream [`sp_decoder::StemMixReader`] reads and ramps toward.
    gains: [Arc<AtomicU32>; 3],
    /// The LIVE per-stream target gains `[original, vocals, instrumental, dub]` a
    /// dub video's 4-stream reader reads and ramps toward.
    dub_gain_atomics: [Arc<AtomicU32>; 4],
    /// The LIVE per-stream target gains `[original, dub]` a NO-STEMS dub video's
    /// 2-stream reader reads and ramps toward (only one of the three ever open).
    dub2_gain_atomics: [Arc<AtomicU32>; 2],
}

fn bits(v: f32) -> AtomicU32 {
    AtomicU32::new(v.to_bits())
}

impl MixControl {
    fn new(f: MixFaders) -> Self {
        let s = stream_gains_song(f);
        let d = stream_gains_dub(f);
        let e = stream_gains_dub_no_stems(f);
        Self {
            faders: [
                Arc::new(bits(f.vokaly)),
                Arc::new(bits(f.podklad)),
                Arc::new(bits(f.dabing)),
            ],
            gains: [
                Arc::new(bits(s[0])),
                Arc::new(bits(s[1])),
                Arc::new(bits(s[2])),
            ],
            dub_gain_atomics: [
                Arc::new(bits(d[0])),
                Arc::new(bits(d[1])),
                Arc::new(bits(d[2])),
                Arc::new(bits(d[3])),
            ],
            dub2_gain_atomics: [Arc::new(bits(e[0])), Arc::new(bits(e[1]))],
        }
    }

    /// Current fader positions.
    pub fn faders(&self) -> MixFaders {
        MixFaders::new(
            f32::from_bits(self.faders[0].load(Ordering::Relaxed)),
            f32::from_bits(self.faders[1].load(Ordering::Relaxed)),
            f32::from_bits(self.faders[2].load(Ordering::Relaxed)),
        )
    }

    /// Set the three faders (clamped + NaN-guarded by [`MixFaders::new`]) and
    /// publish ALL THREE derived gain sets to the live atomics IN LOCK-STEP — the
    /// seam that makes a fader change audible with no pipeline reopen (#186). Every
    /// playing `StemMixReader` (song / dub-with-stems / dub-without-stems) ramps
    /// toward whichever set it holds.
    pub fn set_faders(&self, f: MixFaders) {
        let f = MixFaders::new(f.vokaly, f.podklad, f.dabing);
        self.faders[0].store(f.vokaly.to_bits(), Ordering::Relaxed);
        self.faders[1].store(f.podklad.to_bits(), Ordering::Relaxed);
        self.faders[2].store(f.dabing.to_bits(), Ordering::Relaxed);

        let s = stream_gains_song(f);
        for (a, v) in self.gains.iter().zip(s) {
            a.store(v.to_bits(), Ordering::Relaxed);
        }
        let d = stream_gains_dub(f);
        for (a, v) in self.dub_gain_atomics.iter().zip(d) {
            a.store(v.to_bits(), Ordering::Relaxed);
        }
        let e = stream_gains_dub_no_stems(f);
        for (a, v) in self.dub2_gain_atomics.iter().zip(e) {
            a.store(v.to_bits(), Ordering::Relaxed);
        }
    }

    /// Clone the three LIVE song gain atomics `[original, vocals, instrumental]`
    /// to hand a `StemMixReader`, so a fader change is heard immediately mid-song
    /// without reopening the pipeline.
    pub fn gain_handles(&self) -> [Arc<AtomicU32>; 3] {
        [
            Arc::clone(&self.gains[0]),
            Arc::clone(&self.gains[1]),
            Arc::clone(&self.gains[2]),
        ]
    }

    /// Clone the four LIVE dub gain atomics `[original, vocals, instrumental, dub]`
    /// to hand a 4-stream `StemMixReader`.
    pub fn dub_gain_handles(&self) -> [Arc<AtomicU32>; 4] {
        [
            Arc::clone(&self.dub_gain_atomics[0]),
            Arc::clone(&self.dub_gain_atomics[1]),
            Arc::clone(&self.dub_gain_atomics[2]),
            Arc::clone(&self.dub_gain_atomics[3]),
        ]
    }

    /// Clone the two LIVE gain atomics `[original, dub]` to hand a 2-stream
    /// no-stems `StemMixReader` (#183 round 2).
    pub fn dub_over_original_gain_handles(&self) -> [Arc<AtomicU32>; 2] {
        [
            Arc::clone(&self.dub2_gain_atomics[0]),
            Arc::clone(&self.dub2_gain_atomics[1]),
        ]
    }

    /// Construct a standalone control for tests (not the process global).
    #[cfg(test)]
    pub(crate) fn new_for_test(f: MixFaders) -> Self {
        Self::new(f)
    }
}

static GLOBAL: OnceLock<Arc<MixControl>> = OnceLock::new();

/// Initialise the process-global control from the stored faders. Idempotent — the
/// first call wins; a later call just sets the faders on the existing control.
pub fn init(f: MixFaders) -> Arc<MixControl> {
    let ctrl = GLOBAL.get_or_init(|| Arc::new(MixControl::new(f)));
    ctrl.set_faders(f);
    Arc::clone(ctrl)
}

/// The process-global mixer control, lazily defaulting to all-full faders if
/// `init` was never called (unit tests, degraded boot).
pub fn global() -> Arc<MixControl> {
    Arc::clone(GLOBAL.get_or_init(|| Arc::new(MixControl::new(MixFaders::default()))))
}

/// Read one f32 mixer-fader setting, defaulting to `1.0` (full) when absent or
/// unparseable.
async fn read_fader(pool: &sqlx::SqlitePool, key: &str) -> f32 {
    crate::db::models::get_setting(pool, key)
        .await
        .ok()
        .flatten()
        .and_then(|s| s.trim().parse::<f32>().ok())
        .filter(|v| v.is_finite())
        .unwrap_or(1.0)
}

/// Seed the process-global control from the `mix_vokaly` / `mix_podklad` /
/// `mix_dabing` settings (migration V27 derives these from the old karaoke /
/// dub-ratio settings), so a restart restores the operator's last console.
pub async fn init_from_settings(pool: &sqlx::SqlitePool) -> Arc<MixControl> {
    let vokaly = read_fader(pool, sp_core::config::SETTING_MIX_VOKALY).await;
    let podklad = read_fader(pool, sp_core::config::SETTING_MIX_PODKLAD).await;
    let dabing = read_fader(pool, sp_core::config::SETTING_MIX_DABING).await;
    init(MixFaders::new(vokaly, podklad, dabing))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(a: &Arc<AtomicU32>) -> f32 {
        f32::from_bits(a.load(Ordering::Relaxed))
    }

    #[test]
    fn new_control_holds_the_faders() {
        let c = MixControl::new_for_test(MixFaders::new(0.3, 0.6, 0.5));
        assert_eq!(c.faders(), MixFaders::new(0.3, 0.6, 0.5));
    }

    #[test]
    fn set_faders_publishes_all_three_gain_sets_in_lock_step() {
        let c = MixControl::new_for_test(MixFaders::default());
        // Both full → the bit-exact original everywhere (song/dub); dub carries 1.
        let [o, v, i] = c.gain_handles();
        assert_eq!((read(&o), read(&v), read(&i)), (1.0, 0.0, 0.0));
        let [d0, d1, d2, d3] = c.dub_gain_handles();
        assert_eq!(
            (read(&d0), read(&d1), read(&d2), read(&d3)),
            (1.0, 0.0, 0.0, 1.0)
        );
        let [e0, e1] = c.dub_over_original_gain_handles();
        assert_eq!((read(&e0), read(&e1)), (1.0, 1.0));

        // A move off the both-full corner publishes all three sets from the SAME
        // faders to the SAME atomics (live, no reopen).
        c.set_faders(MixFaders::new(0.3, 1.0, 0.5));
        assert_eq!(c.faders(), MixFaders::new(0.3, 1.0, 0.5));
        // Song: original silent, stems at the fader positions.
        assert_eq!((read(&o), read(&v), read(&i)), (0.0, 0.3, 1.0));
        // Dub (with stems): the same pair + the dub gain.
        assert_eq!(
            (read(&d0), read(&d1), read(&d2), read(&d3)),
            (0.0, 0.3, 1.0, 0.5)
        );
        // Dub (no stems): vokály is the whole original bed, dabing the dub.
        assert_eq!((read(&e0), read(&e1)), (0.3, 0.5));
    }

    #[test]
    fn set_faders_guards_nan_to_the_default_console() {
        let c = MixControl::new_for_test(MixFaders::new(0.2, 0.2, 0.2));
        // A RAW struct literal carrying a NaN (bypasses `MixFaders::new`) exercises
        // set_faders' OWN re-guard.
        c.set_faders(MixFaders {
            vokaly: f32::NAN,
            podklad: 0.5,
            dabing: 0.5,
        });
        // Any non-finite input → the (1,1,1) default console.
        assert_eq!(c.faders(), MixFaders::default());
        let [o, v, i] = c.gain_handles();
        assert_eq!((read(&o), read(&v), read(&i)), (1.0, 0.0, 0.0));
    }
}
