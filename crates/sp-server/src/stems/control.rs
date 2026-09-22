//! Process-global live mixer control (#184 round G/G1/G2, was #14/#186 `KaraokeControl`).
//!
//! The mixer is ONE app-wide live console (one wall, one operator) — three
//! independent faders `[vokály, podklad, dabing]`. It keeps TWO remembered fader
//! triples (`sp_core::mixer_model::MixConsole`): a SONG memory and a DUB memory.
//! Songs and dub videos want opposite `vokály` positions almost always, so a
//! single global memory made a dub video mixed to `Len dabing` leave the next song
//! instrumental-only (and a song at `Plný mix` double a dub video's voices).
//!
//! Round G2 removes the GLOBAL "active kind": the wall runs SEVERAL pipelines at
//! once, so "the playing item's kind" is not a single value. Each reader FAMILY is
//! fed from its OWN memory, ALWAYS — the 3-stream song reader from the song memory
//! (`gains[3]`), the dub readers from the dub memory (`dub_gain_atomics[4]` +
//! `dub2_gain_atomics[2]`). `set_faders(kind, f)` writes ONE memory and republishes
//! ONLY that memory's gain set(s), so a song opening anywhere never re-publishes the
//! dub readers' atomics (and vice versa). A fader move is still a live GAIN write, not
//! a choice of which files to open (#186): every playing [`sp_decoder::StemMixReader`]
//! reads and ramps toward whichever set applies, with NO pipeline reopen.

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};

use sp_core::mixer_model::{
    MixConsole, MixFaders, MixKind, stream_gains_dub, stream_gains_dub_no_stems, stream_gains_song,
};

/// Live mixer console (two kind-scoped fader memories) + the derived per-stream
/// gain atomics, shared between the API/engine (writers) and every playback
/// pipeline (reader). There is NO global "active kind" (round G2): each memory
/// feeds its OWN reader family.
pub struct MixControl {
    /// The SONG memory's three fader positions (f32 bits), `0.0..=1.0`. Its `dabing`
    /// component is unused (a song has no dub stream).
    song_faders: [Arc<AtomicU32>; 3],
    /// The DUB memory's three fader positions (f32 bits), `0.0..=1.0`.
    dub_faders: [Arc<AtomicU32>; 3],
    /// The LIVE per-stream target gains `[original, vocals, instrumental]` a song's
    /// 3-stream [`sp_decoder::StemMixReader`] reads and ramps toward — ALWAYS derived
    /// from the SONG memory.
    gains: [Arc<AtomicU32>; 3],
    /// The LIVE per-stream target gains `[original, vocals, instrumental, dub]` a
    /// dub video's 4-stream reader reads and ramps toward — ALWAYS from the DUB memory.
    dub_gain_atomics: [Arc<AtomicU32>; 4],
    /// The LIVE per-stream target gains `[original, dub]` a NO-STEMS dub video's
    /// 2-stream reader reads and ramps toward (only one of the three ever open) —
    /// ALWAYS from the DUB memory.
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
            gains: triple_atomics(MixFaders::default()),
            dub_gain_atomics: [
                Arc::new(bits(0.0)),
                Arc::new(bits(0.0)),
                Arc::new(bits(0.0)),
                Arc::new(bits(0.0)),
            ],
            dub2_gain_atomics: [Arc::new(bits(0.0)), Arc::new(bits(0.0))],
        };
        ctrl.publish_song(c.song);
        ctrl.publish_dub(c.dub);
        ctrl
    }

    /// A snapshot of BOTH memories (round G2 — no active kind).
    pub fn console(&self) -> MixConsole {
        MixConsole {
            song: read_triple(&self.song_faders),
            dub: read_triple(&self.dub_faders),
        }
    }

    /// The fader positions of ONE memory — what `GET /api/v1/mix` returns per kind
    /// and the strip shows for its item.
    pub fn faders(&self, kind: MixKind) -> MixFaders {
        match kind {
            MixKind::Song => read_triple(&self.song_faders),
            MixKind::Dub => read_triple(&self.dub_faders),
        }
    }

    /// Publish the SONG reader's live gain set `[original, vocals, instrumental]` from
    /// the song faders `f`. The dub atomics are NOT touched (round G2).
    fn publish_song(&self, f: MixFaders) {
        let s = stream_gains_song(f);
        for (a, v) in self.gains.iter().zip(s) {
            a.store(v.to_bits(), Ordering::Relaxed);
        }
    }

    /// Publish BOTH dub reader gain sets (`[original, vocals, instrumental, dub]` and
    /// the no-stems `[original, dub]`) from the dub faders `f`. The song atomics are
    /// NOT touched (round G2).
    fn publish_dub(&self, f: MixFaders) {
        let d = stream_gains_dub(f);
        for (a, v) in self.dub_gain_atomics.iter().zip(d) {
            a.store(v.to_bits(), Ordering::Relaxed);
        }
        let e = stream_gains_dub_no_stems(f);
        for (a, v) in self.dub2_gain_atomics.iter().zip(e) {
            a.store(v.to_bits(), Ordering::Relaxed);
        }
    }

    /// Set ONE memory's three faders (clamped + NaN-guarded by [`MixFaders::new`]) and
    /// republish ONLY that memory's derived gain set(s). The OTHER memory — and the
    /// reader family it feeds — is untouched, so a song set never corrupts a dub
    /// output and vice versa (the round G2 invariant).
    pub fn set_faders(&self, kind: MixKind, f: MixFaders) {
        let f = MixFaders::new(f.vokaly, f.podklad, f.dabing);
        match kind {
            MixKind::Song => store_triple(&self.song_faders, f),
            MixKind::Dub => store_triple(&self.dub_faders, f),
        }
        // RED (#184 G2): publishes BOTH reader families from the just-written faders,
        // so a song set corrupts the dub readers' atomics and a dub set the song
        // reader's — the G1 cross-contamination bug. GREEN scopes the publish to the
        // written kind's OWN family.
        self.publish_song(f);
        self.publish_dub(f);
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

    /// Overwrite BOTH memories from a console and republish each reader family from
    /// its OWN memory (idempotent re-seed used by [`init`]).
    fn reset(&self, c: MixConsole) {
        store_triple(&self.song_faders, c.song);
        store_triple(&self.dub_faders, c.dub);
        self.publish_song(c.song);
        self.publish_dub(c.dub);
    }

    /// Construct a standalone control for tests (not the process global). Both
    /// memories seed from `f`.
    #[cfg(test)]
    pub(crate) fn new_for_test(f: MixFaders) -> Self {
        Self::new(MixConsole { song: f, dub: f })
    }

    /// Construct a standalone control for tests from a full console.
    #[cfg(test)]
    pub(crate) fn new_for_test_console(c: MixConsole) -> Self {
        Self::new(c)
    }
}

static GLOBAL: OnceLock<Arc<MixControl>> = OnceLock::new();

/// Initialise the process-global control from a stored console. Idempotent — the
/// first call wins; a later call re-seeds the existing control's two memories.
pub fn init(c: MixConsole) -> Arc<MixControl> {
    let ctrl = GLOBAL.get_or_init(|| Arc::new(MixControl::new(c)));
    ctrl.reset(c);
    Arc::clone(ctrl)
}

/// The process-global mixer control, lazily defaulting to the [`MixConsole`]
/// default (song `(1,1,·)`, dub `(0,1,1)`) if `init` was never called (unit tests,
/// degraded boot).
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
    init(MixConsole { song, dub })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(a: &Arc<AtomicU32>) -> f32 {
        f32::from_bits(a.load(Ordering::Relaxed))
    }

    #[test]
    fn reset_replaces_both_memories_and_republishes_each_family() {
        // `init` on an already-initialised global goes through `reset`; a reset
        // that does nothing would leave the previous console (and its gains).
        let c = MixControl::new_for_test_console(MixConsole {
            song: MixFaders::new(1.0, 1.0, 1.0),
            dub: MixFaders::new(0.0, 1.0, 1.0),
        });
        let next = MixConsole {
            song: MixFaders::new(0.4, 0.9, 1.0),
            dub: MixFaders::new(0.2, 0.8, 0.6),
        };
        c.reset(next);
        assert_eq!(c.console(), next);
        // The song reader is republished from the SONG memory: [0, vokaly, podklad].
        let [o, v, i] = c.gain_handles();
        assert_eq!((read(&o), read(&v), read(&i)), (0.0, 0.4, 0.9));
        // The dub reader from the DUB memory: [0, vokaly, podklad, dabing].
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
    fn new_control_holds_both_memories_faders() {
        let c = MixControl::new_for_test_console(MixConsole {
            song: MixFaders::new(0.3, 0.6, 1.0),
            dub: MixFaders::new(0.1, 0.9, 0.4),
        });
        assert_eq!(c.faders(MixKind::Song), MixFaders::new(0.3, 0.6, 1.0));
        assert_eq!(c.faders(MixKind::Dub), MixFaders::new(0.1, 0.9, 0.4));
    }

    #[test]
    fn new_control_publishes_each_family_from_its_own_memory() {
        // Both memories at the both-full corner via new_for_test → bit-exact original.
        let c = MixControl::new_for_test(MixFaders::default());
        let [o, v, i] = c.gain_handles();
        assert_eq!((read(&o), read(&v), read(&i)), (1.0, 0.0, 0.0));
        let [d0, d1, d2, d3] = c.dub_gain_handles();
        assert_eq!(
            (read(&d0), read(&d1), read(&d2), read(&d3)),
            (1.0, 0.0, 0.0, 1.0)
        );
        let [e0, e1] = c.dub_over_original_gain_handles();
        assert_eq!((read(&e0), read(&e1)), (1.0, 1.0));
    }

    #[test]
    fn set_faders_dub_updates_only_the_dub_reader_gains() {
        // song memory (1,1,1) → song reader stays bit-exact [1,0,0]; dub memory
        // starts (0,1,1) → dub reader [0,0,1,1], no-stems [0,1].
        let c = MixControl::new_for_test_console(MixConsole {
            song: MixFaders::new(1.0, 1.0, 1.0),
            dub: MixFaders::new(0.0, 1.0, 1.0),
        });

        c.set_faders(MixKind::Dub, MixFaders::new(0.3, 1.0, 1.0));

        // The dub reader gains follow the dub memory.
        let [d0, d1, d2, d3] = c.dub_gain_handles();
        assert_eq!(
            (read(&d0), read(&d1), read(&d2), read(&d3)),
            (0.0, 0.3, 1.0, 1.0)
        );
        let [e0, e1] = c.dub_over_original_gain_handles();
        assert_eq!((read(&e0), read(&e1)), (0.3, 1.0));
        // The SONG reader is UNTOUCHED by a dub set — the round G2 invariant.
        let [o, v, i] = c.gain_handles();
        assert_eq!(
            (read(&o), read(&v), read(&i)),
            (1.0, 0.0, 0.0),
            "a dub set must not republish the song reader's atomics"
        );
    }

    #[test]
    fn set_faders_song_updates_only_the_song_reader_gains() {
        let c = MixControl::new_for_test_console(MixConsole {
            song: MixFaders::new(1.0, 1.0, 1.0),
            dub: MixFaders::new(0.0, 1.0, 1.0),
        });

        c.set_faders(MixKind::Song, MixFaders::new(0.3, 1.0, 1.0));

        // The song reader gains follow the song memory: [0, vokaly, podklad].
        let [o, v, i] = c.gain_handles();
        assert_eq!((read(&o), read(&v), read(&i)), (0.0, 0.3, 1.0));
        // The DUB reader is UNTOUCHED by a song set (dub memory still (0,1,1)).
        let [d0, d1, d2, d3] = c.dub_gain_handles();
        assert_eq!(
            (read(&d0), read(&d1), read(&d2), read(&d3)),
            (0.0, 0.0, 1.0, 1.0),
            "a song set must not republish the dub reader's atomics"
        );
        let [e0, e1] = c.dub_over_original_gain_handles();
        assert_eq!((read(&e0), read(&e1)), (0.0, 1.0));
    }

    #[test]
    fn set_faders_writes_only_its_own_memory() {
        let c = MixControl::new_for_test_console(MixConsole {
            song: MixFaders::new(1.0, 1.0, 1.0),
            dub: MixFaders::new(0.0, 1.0, 1.0),
        });
        c.set_faders(MixKind::Dub, MixFaders::new(0.3, 1.0, 1.0));
        assert_eq!(c.console().dub, MixFaders::new(0.3, 1.0, 1.0));
        assert_eq!(
            c.console().song,
            MixFaders::new(1.0, 1.0, 1.0),
            "editing the dub memory must not touch the song memory"
        );
        c.set_faders(MixKind::Song, MixFaders::new(0.2, 0.5, 1.0));
        assert_eq!(c.console().song, MixFaders::new(0.2, 0.5, 1.0));
        assert_eq!(
            c.console().dub,
            MixFaders::new(0.3, 1.0, 1.0),
            "editing the song memory must not touch the dub memory"
        );
    }

    #[test]
    fn set_faders_guards_nan_to_the_default() {
        let c = MixControl::new_for_test(MixFaders::new(0.2, 0.2, 0.2));
        // A RAW struct literal carrying a NaN (bypasses `MixFaders::new`) exercises
        // set_faders' OWN re-guard.
        c.set_faders(
            MixKind::Song,
            MixFaders {
                vokaly: f32::NAN,
                podklad: 0.5,
                dabing: 0.5,
            },
        );
        // Any non-finite input → the (1,1,1) default memory.
        assert_eq!(c.faders(MixKind::Song), MixFaders::default());
        let [o, v, i] = c.gain_handles();
        assert_eq!((read(&o), read(&v), read(&i)), (1.0, 0.0, 0.0));
    }
}
