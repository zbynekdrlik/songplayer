//! Process-global live mixer control (#184 round G/G1, was #14/#186 `KaraokeControl`).
//!
//! The mixer is ONE app-wide live console (one wall, one operator) — three
//! independent faders `[vokály, podklad, dabing]`. Round G1 gives it TWO
//! remembered fader triples (`sp_core::mixer_model::MixConsole`), selected by the
//! KIND of the playing item: a SONG memory and a DUB memory. Songs and dub videos
//! want opposite `vokály` positions almost always, so a single global memory made
//! a dub video mixed to `Len dabing` leave the next song instrumental-only (and a
//! song at `Plný mix` double a dub video's voices). The strip and the API are
//! unchanged — only WHICH memory a fader write lands in changes.
//!
//! Since #186 a fader change is a live GAIN write, not a choice of which files to
//! open: the control holds the two remembered fader triples PLUS three DERIVED
//! per-stream gain sets — `[original, vocals, instrumental]` (a song), `[original,
//! vocals, instrumental, dub]` (a dub with stems), and `[original, dub]` (a dub
//! without stems). `set_faders` writes the ACTIVE memory and recomputes ALL THREE
//! gain sets in lock-step from the pure `sp_core::mixer_model::stream_gains_*`, and
//! every playing [`sp_decoder::StemMixReader`] reads and ramps toward whichever set
//! applies — so a fader move is heard immediately with NO pipeline reopen (the #186
//! seam). `select_kind` switches the active memory at each item open and republishes.

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};

use sp_core::mixer_model::{
    MixConsole, MixFaders, MixKind, stream_gains_dub, stream_gains_dub_no_stems, stream_gains_song,
};

/// The `active` atomic's encoding of [`MixKind`].
const KIND_SONG: u32 = 0;
const KIND_DUB: u32 = 1;

fn kind_code(k: MixKind) -> u32 {
    match k {
        MixKind::Song => KIND_SONG,
        MixKind::Dub => KIND_DUB,
    }
}

/// Live mixer console (two kind-scoped fader memories) + the derived per-stream
/// gain atomics, shared between the API/engine (writers) and every playback
/// pipeline (reader).
pub struct MixControl {
    /// The SONG memory's three fader positions (f32 bits), `0.0..=1.0`. Its `dabing`
    /// component is unused (a song has no dub stream).
    song_faders: [Arc<AtomicU32>; 3],
    /// The DUB memory's three fader positions (f32 bits), `0.0..=1.0`.
    dub_faders: [Arc<AtomicU32>; 3],
    /// Which memory is active — [`KIND_SONG`] / [`KIND_DUB`]. Selected by the
    /// playing item's kind at each open ([`MixControl::select_kind`]).
    active: Arc<AtomicU32>,
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

fn triple_atomics(f: MixFaders) -> [Arc<AtomicU32>; 3] {
    [
        Arc::new(bits(f.vokaly)),
        Arc::new(bits(f.podklad)),
        Arc::new(bits(f.dabing)),
    ]
}

fn read_triple(a: &[Arc<AtomicU32>; 3]) -> MixFaders {
    MixFaders::new(
        f32::from_bits(a[0].load(Ordering::Relaxed)),
        f32::from_bits(a[1].load(Ordering::Relaxed)),
        f32::from_bits(a[2].load(Ordering::Relaxed)),
    )
}

fn store_triple(a: &[Arc<AtomicU32>; 3], f: MixFaders) {
    a[0].store(f.vokaly.to_bits(), Ordering::Relaxed);
    a[1].store(f.podklad.to_bits(), Ordering::Relaxed);
    a[2].store(f.dabing.to_bits(), Ordering::Relaxed);
}

impl MixControl {
    fn new(c: MixConsole) -> Self {
        let ctrl = Self {
            song_faders: triple_atomics(c.song),
            dub_faders: triple_atomics(c.dub),
            active: Arc::new(bits_u32(kind_code(c.active))),
            gains: triple_atomics(MixFaders::default()),
            dub_gain_atomics: [
                Arc::new(bits(0.0)),
                Arc::new(bits(0.0)),
                Arc::new(bits(0.0)),
                Arc::new(bits(0.0)),
            ],
            dub2_gain_atomics: [Arc::new(bits(0.0)), Arc::new(bits(0.0))],
        };
        ctrl.publish_gains(c.active_faders());
        ctrl
    }

    /// The current active [`MixKind`].
    pub fn kind(&self) -> MixKind {
        if self.active.load(Ordering::Relaxed) == KIND_DUB {
            MixKind::Dub
        } else {
            MixKind::Song
        }
    }

    /// A snapshot of BOTH memories + the active kind.
    pub fn console(&self) -> MixConsole {
        MixConsole {
            song: read_triple(&self.song_faders),
            dub: read_triple(&self.dub_faders),
            active: self.kind(),
        }
    }

    /// The ACTIVE memory's fader positions — what the API returns and the strip
    /// shows.
    pub fn faders(&self) -> MixFaders {
        match self.kind() {
            MixKind::Song => read_triple(&self.song_faders),
            MixKind::Dub => read_triple(&self.dub_faders),
        }
    }

    /// Publish ALL THREE derived gain sets from `f` to the live atomics IN
    /// LOCK-STEP — the seam that makes a fader change audible with no pipeline
    /// reopen (#186). Every playing `StemMixReader` (song / dub-with-stems /
    /// dub-without-stems) ramps toward whichever set it holds.
    fn publish_gains(&self, f: MixFaders) {
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

    /// Set the ACTIVE memory's three faders (clamped + NaN-guarded by
    /// [`MixFaders::new`]) and republish the derived gain sets from them. The OTHER
    /// memory is untouched — an operator's song mix and dub mix each survive the
    /// other.
    pub fn set_faders(&self, f: MixFaders) {
        let f = MixFaders::new(f.vokaly, f.podklad, f.dabing);
        match self.kind() {
            MixKind::Song => store_triple(&self.song_faders, f),
            MixKind::Dub => store_triple(&self.dub_faders, f),
        }
        self.publish_gains(f);
    }

    /// Switch the active memory to `kind` and republish the derived gain sets from
    /// the newly-active memory — called by the engine when an item opens (Dub when
    /// the item has a ready dub, else Song). The remembered faders are untouched.
    pub fn select_kind(&self, kind: MixKind) {
        self.active.store(kind_code(kind), Ordering::Relaxed);
        self.publish_gains(self.faders());
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

    /// Overwrite BOTH memories + the active kind from a console and republish
    /// (idempotent re-seed used by [`init`]).
    fn reset(&self, c: MixConsole) {
        store_triple(&self.song_faders, c.song);
        store_triple(&self.dub_faders, c.dub);
        self.active.store(kind_code(c.active), Ordering::Relaxed);
        self.publish_gains(c.active_faders());
    }

    /// Construct a standalone control for tests (not the process global). Both
    /// memories seed from `f`, active Song.
    #[cfg(test)]
    pub(crate) fn new_for_test(f: MixFaders) -> Self {
        Self::new(MixConsole {
            song: f,
            dub: f,
            active: MixKind::Song,
        })
    }

    /// Construct a standalone control for tests from a full console.
    #[cfg(test)]
    pub(crate) fn new_for_test_console(c: MixConsole) -> Self {
        Self::new(c)
    }
}

fn bits_u32(v: u32) -> AtomicU32 {
    AtomicU32::new(v)
}

static GLOBAL: OnceLock<Arc<MixControl>> = OnceLock::new();

/// Initialise the process-global control from a stored console. Idempotent — the
/// first call wins; a later call re-seeds the existing control's two memories +
/// active kind.
pub fn init(c: MixConsole) -> Arc<MixControl> {
    let ctrl = GLOBAL.get_or_init(|| Arc::new(MixControl::new(c)));
    ctrl.reset(c);
    Arc::clone(ctrl)
}

/// The process-global mixer control, lazily defaulting to the [`MixConsole`]
/// default (song `(1,1,·)`, dub `(0,1,1)`, active Song) if `init` was never called
/// (unit tests, degraded boot).
pub fn global() -> Arc<MixControl> {
    Arc::clone(GLOBAL.get_or_init(|| Arc::new(MixControl::new(MixConsole::default()))))
}

/// Read one f32 mixer-fader setting, defaulting to `default` when absent or
/// unparseable.
async fn read_fader(pool: &sqlx::SqlitePool, key: &str, default: f32) -> f32 {
    crate::db::models::get_setting(pool, key)
        .await
        .ok()
        .flatten()
        .and_then(|s| s.trim().parse::<f32>().ok())
        .filter(|v| v.is_finite())
        .unwrap_or(default)
}

/// Seed the process-global control from the five per-kind mixer settings
/// (`mix_song_vokaly` / `mix_song_podklad` / `mix_dub_vokaly` / `mix_dub_podklad` /
/// `mix_dub_dabing`, derived + seeded by migration V28), so a restart restores the
/// operator's last SONG and DUB consoles.
pub async fn init_from_settings(pool: &sqlx::SqlitePool) -> Arc<MixControl> {
    let d = MixConsole::default();
    let song = MixFaders::new(
        read_fader(
            pool,
            sp_core::config::SETTING_MIX_SONG_VOKALY,
            d.song.vokaly,
        )
        .await,
        read_fader(
            pool,
            sp_core::config::SETTING_MIX_SONG_PODKLAD,
            d.song.podklad,
        )
        .await,
        1.0, // song memory has no dub stream
    );
    let dub = MixFaders::new(
        read_fader(pool, sp_core::config::SETTING_MIX_DUB_VOKALY, d.dub.vokaly).await,
        read_fader(
            pool,
            sp_core::config::SETTING_MIX_DUB_PODKLAD,
            d.dub.podklad,
        )
        .await,
        read_fader(pool, sp_core::config::SETTING_MIX_DUB_DABING, d.dub.dabing).await,
    );
    init(MixConsole {
        song,
        dub,
        active: MixKind::Song,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(a: &Arc<AtomicU32>) -> f32 {
        f32::from_bits(a.load(Ordering::Relaxed))
    }

    #[test]
    fn reset_replaces_both_memories_the_active_kind_and_republishes() {
        // `init` on an already-initialised global goes through `reset`; a reset
        // that does nothing would leave the previous console (and its gains).
        let c = MixControl::new_for_test_console(MixConsole {
            song: MixFaders::new(1.0, 1.0, 1.0),
            dub: MixFaders::new(0.0, 1.0, 1.0),
            active: MixKind::Song,
        });
        let next = MixConsole {
            song: MixFaders::new(0.4, 0.9, 1.0),
            dub: MixFaders::new(0.2, 0.8, 0.6),
            active: MixKind::Dub,
        };
        c.reset(next);
        assert_eq!(c.console(), next);
        assert_eq!(c.kind(), MixKind::Dub);
        // Republished from the ACTIVE (dub) memory: [0, vokaly, podklad, dabing].
        let [d0, d1, d2, d3] = c.dub_gain_handles();
        assert_eq!(
            (read(&d0), read(&d1), read(&d2), read(&d3)),
            (0.0, 0.2, 0.8, 0.6)
        );
    }

    #[tokio::test]
    async fn read_fader_parses_trims_and_falls_back_to_the_default() {
        let pool = crate::db::create_memory_pool().await.unwrap();
        crate::db::run_migrations(&pool).await.unwrap();
        // Absent key → the caller's default (a value no mutant constant equals).
        assert_eq!(read_fader(&pool, "mix_test_absent", 0.6).await, 0.6);
        // Stored values are parsed (with surrounding whitespace trimmed).
        crate::db::models::set_setting(&pool, "mix_test_a", "0.4")
            .await
            .unwrap();
        crate::db::models::set_setting(&pool, "mix_test_b", " 0.7 ")
            .await
            .unwrap();
        assert_eq!(read_fader(&pool, "mix_test_a", 0.6).await, 0.4);
        assert_eq!(read_fader(&pool, "mix_test_b", 0.6).await, 0.7);
        // Non-numeric / non-finite → the default, never a poisoned console.
        crate::db::models::set_setting(&pool, "mix_test_c", "abc")
            .await
            .unwrap();
        crate::db::models::set_setting(&pool, "mix_test_d", "NaN")
            .await
            .unwrap();
        assert_eq!(read_fader(&pool, "mix_test_c", 0.6).await, 0.6);
        assert_eq!(read_fader(&pool, "mix_test_d", 0.6).await, 0.6);
    }

    #[test]
    fn new_control_holds_the_active_faders() {
        let c = MixControl::new_for_test(MixFaders::new(0.3, 0.6, 0.5));
        assert_eq!(c.faders(), MixFaders::new(0.3, 0.6, 0.5));
        assert_eq!(c.kind(), MixKind::Song);
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

    #[test]
    fn set_faders_writes_only_the_active_memory() {
        // Active DUB: a set writes the DUB memory and leaves the SONG memory as
        // seeded.
        let c = MixControl::new_for_test_console(MixConsole {
            song: MixFaders::new(1.0, 1.0, 1.0),
            dub: MixFaders::new(0.0, 1.0, 1.0),
            active: MixKind::Dub,
        });
        c.set_faders(MixFaders::new(0.3, 1.0, 1.0));
        let console = c.console();
        assert_eq!(
            console.dub,
            MixFaders::new(0.3, 1.0, 1.0),
            "a set on the active DUB memory writes the dub memory"
        );
        assert_eq!(
            console.song,
            MixFaders::new(1.0, 1.0, 1.0),
            "editing the dub memory must not touch the song memory"
        );
        assert_eq!(c.faders(), MixFaders::new(0.3, 1.0, 1.0));
    }

    #[test]
    fn select_kind_switches_active_and_republishes_from_that_memory() {
        let c = MixControl::new_for_test_console(MixConsole {
            song: MixFaders::new(1.0, 1.0, 1.0),
            dub: MixFaders::new(0.0, 1.0, 1.0),
            active: MixKind::Song,
        });
        // Song active → the song gain set (bit-exact original).
        let [o, v, i] = c.gain_handles();
        assert_eq!((read(&o), read(&v), read(&i)), (1.0, 0.0, 0.0));

        c.select_kind(MixKind::Dub);
        assert_eq!(c.kind(), MixKind::Dub);
        assert_eq!(c.faders(), MixFaders::new(0.0, 1.0, 1.0));
        // Dub memory (0,1,1) republished: song gains now [0, vokaly=0, podklad=1],
        // dub (with stems) [0, 0, 1, dabing=1], no-stems [vokaly=0, dabing=1].
        assert_eq!((read(&o), read(&v), read(&i)), (0.0, 0.0, 1.0));
        let [d0, d1, d2, d3] = c.dub_gain_handles();
        assert_eq!(
            (read(&d0), read(&d1), read(&d2), read(&d3)),
            (0.0, 0.0, 1.0, 1.0)
        );
        let [e0, e1] = c.dub_over_original_gain_handles();
        assert_eq!((read(&e0), read(&e1)), (0.0, 1.0));

        // Back to Song → the song memory (1,1) is intact.
        c.select_kind(MixKind::Song);
        assert_eq!(c.faders(), MixFaders::new(1.0, 1.0, 1.0));
        assert_eq!((read(&o), read(&v), read(&i)), (1.0, 0.0, 0.0));
    }
}
