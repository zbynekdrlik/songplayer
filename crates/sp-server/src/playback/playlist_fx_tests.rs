//! #242: the register of every playlist's live sound and the stream wrapper
//! — a change set while a song plays reaches its next chunk, every stream of
//! the playlist follows, and startup loads the rows.
//! Wired via `#[cfg(test)] #[path = "playlist_fx_tests.rs"] mod tests;`.

use std::sync::{Arc, Mutex};

use sp_core::audio_fx::{BandKind, EqBand, PlaylistFx};
use sp_decoder::{AudioStream, DecodedAudioFrame, DecoderError, MediaStream};

use super::{FxRegister, load_all, wrap_with};

/// A stereo stream of `chunks` chunks of 480 frames at 1.0, recording seeks.
struct Dc {
    chunks: usize,
    seeks: Arc<Mutex<Vec<u64>>>,
}

impl MediaStream for Dc {
    fn duration_ms(&self) -> u64 {
        4321
    }

    fn seek(&mut self, position_ms: u64) -> Result<(), DecoderError> {
        self.seeks.lock().unwrap().push(position_ms);
        Ok(())
    }
}

impl AudioStream for Dc {
    fn next_samples(&mut self) -> Result<Option<DecodedAudioFrame>, DecoderError> {
        if self.chunks == 0 {
            return Ok(None);
        }
        self.chunks -= 1;
        Ok(Some(DecodedAudioFrame {
            data: vec![1.0; 960],
            channels: 2,
            sample_rate: 48_000,
            timestamp_ms: 0,
        }))
    }

    fn sample_rate(&self) -> u32 {
        48_000
    }

    fn channels(&self) -> u16 {
        2
    }
}

fn dc(chunks: usize) -> (Box<dyn AudioStream>, Arc<Mutex<Vec<u64>>>) {
    let seeks = Arc::new(Mutex::new(Vec::new()));
    (
        Box::new(Dc {
            chunks,
            seeks: seeks.clone(),
        }),
        seeks,
    )
}

fn gain(db: f64) -> PlaylistFx {
    PlaylistFx {
        gain_db: db,
        eq: vec![],
    }
}

#[test]
fn a_slot_starts_untouched_and_every_set_moves_its_generation() {
    let register = FxRegister::default();
    let slot = register.slot(7);
    assert_eq!(slot.read(), (0, PlaylistFx::default()));
    register.set(7, gain(-5.0));
    assert_eq!(slot.read(), (1, gain(-5.0)), "the same slot sees the set");
    register.set(7, gain(-6.0));
    assert_eq!(slot.generation(), 2);
    assert!(Arc::ptr_eq(&slot, &register.slot(7)));
    assert_eq!(
        register.slot(8).read(),
        (0, PlaylistFx::default()),
        "others untouched"
    );
}

/// A song opened at its playlist's sound plays it; a change set while it
/// plays reaches the next chunk (a 50 ms ramp), and the stream passes the
/// rest through.
#[test]
fn a_change_set_while_a_song_plays_reaches_its_next_chunk() {
    let register = FxRegister::default();
    register.set(3, gain(-6.020_599_913_279_624));
    let (inner, seeks) = dc(12);
    let mut stream = wrap_with(&register, inner, 3);
    assert_eq!(
        (
            stream.duration_ms(),
            stream.sample_rate(),
            stream.channels()
        ),
        (4321, 48_000, 2)
    );
    stream.seek(1500).unwrap();
    assert_eq!(*seeks.lock().unwrap(), vec![1500]);
    let first = stream.next_samples().unwrap().unwrap();
    assert!(
        first.data.iter().all(|s| (s - 0.5).abs() < 1e-6),
        "{}",
        first.data[0]
    );
    register.set(3, PlaylistFx::default());
    let mut chunks = Vec::new();
    while let Some(frame) = stream.next_samples().unwrap() {
        chunks.push(frame.data);
    }
    assert_eq!(chunks.len(), 11);
    assert!(
        (chunks[0][0] - 0.5).abs() < 1e-6,
        "the ramp starts where it was"
    );
    assert!(chunks[0][958] > 0.5, "and rises within the chunk");
    let last = chunks.last().unwrap();
    assert!(last.iter().all(|s| *s == 1.0), "{}", last[0]);
}

/// An EQ set on the playlist reaches its stream too.
#[test]
fn an_eq_set_on_the_playlist_reaches_its_stream() {
    let register = FxRegister::default();
    let (inner, _) = dc(30);
    let mut stream = wrap_with(&register, inner, 9);
    let untouched = stream.next_samples().unwrap().unwrap();
    assert!(untouched.data.iter().all(|s| *s == 1.0));
    register.set(
        9,
        PlaylistFx {
            gain_db: 0.0,
            eq: vec![EqBand {
                kind: BandKind::HighPass,
                freq_hz: 100.0,
                gain_db: 0.0,
                q: 0.707,
                enabled: true,
            }],
        },
    );
    let mut last = Vec::new();
    while let Some(frame) = stream.next_samples().unwrap() {
        last = frame.data;
    }
    assert!(
        last.iter().all(|s| s.abs() < 0.01),
        "a high-pass blocks DC: {}",
        last[0]
    );
}

#[tokio::test]
async fn startup_loads_every_rows_sound() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url) \
         VALUES (51001, 'a', 'ua'), (51002, 'b', 'ub'), (51003, 'c', 'uc')",
    )
    .execute(&pool)
    .await
    .unwrap();
    crate::db::models_playlist_fx::set_playlist_fx(&pool, 51002, &gain(-10.0))
        .await
        .unwrap();
    let eq_only = PlaylistFx {
        gain_db: 0.0,
        eq: vec![EqBand {
            kind: BandKind::LowShelf,
            freq_hz: 120.0,
            gain_db: -12.0,
            q: 0.707,
            enabled: true,
        }],
    };
    crate::db::models_playlist_fx::set_playlist_fx(&pool, 51003, &eq_only)
        .await
        .unwrap();
    let register = FxRegister::default();
    assert_eq!(
        load_all(&pool, &register).await,
        2,
        "two playlists have a sound of their own"
    );
    assert_eq!(register.slot(51002).read().1, gain(-10.0));
    assert_eq!(register.slot(51003).read().1, eq_only);
    assert_eq!(register.slot(51001).read().1, PlaylistFx::default());
    assert_eq!(
        register.slot(51001).generation(),
        1,
        "loaded, even untouched"
    );
}
