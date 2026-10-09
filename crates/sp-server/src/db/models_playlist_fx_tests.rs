//! #242: a playlist's sound round-trips through its row; a stored value from
//! outside the API that does not read or validate plays untouched.
//! Wired via `#[cfg(test)] #[path = "models_playlist_fx_tests.rs"] mod tests;`.

use sp_core::audio_fx::{BandKind, EqBand, PlaylistFx};

use super::{all_playlist_fx, get_playlist_fx, row_fx, set_playlist_fx};

async fn pool_with(ids: &[i64]) -> sqlx::SqlitePool {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    for id in ids {
        sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (?, ?, ?)")
            .bind(id)
            .bind(format!("p{id}"))
            .bind(format!("u{id}"))
            .execute(&pool)
            .await
            .unwrap();
    }
    pool
}

fn shaped() -> PlaylistFx {
    PlaylistFx {
        gain_db: -5.5,
        eq: vec![
            EqBand {
                kind: BandKind::LowShelf,
                freq_hz: 180.0,
                gain_db: -20.0,
                q: 0.7,
                enabled: true,
            },
            EqBand {
                kind: BandKind::Peak,
                freq_hz: 3000.0,
                gain_db: 2.5,
                q: 1.4,
                enabled: false,
            },
        ],
    }
}

#[tokio::test]
async fn a_playlists_sound_round_trips_through_its_row() {
    let pool = pool_with(&[42001, 42002]).await;
    assert_eq!(
        get_playlist_fx(&pool, 42001).await.unwrap(),
        Some(PlaylistFx::default())
    );
    assert!(set_playlist_fx(&pool, 42001, &shaped()).await.unwrap());
    assert_eq!(get_playlist_fx(&pool, 42001).await.unwrap(), Some(shaped()));
    assert_eq!(
        get_playlist_fx(&pool, 42002).await.unwrap(),
        Some(PlaylistFx::default()),
        "the other playlist is untouched"
    );
    assert_eq!(get_playlist_fx(&pool, 49999).await.unwrap(), None);
    assert!(!set_playlist_fx(&pool, 49999, &shaped()).await.unwrap());
    let all = all_playlist_fx(&pool).await.unwrap();
    let ours: Vec<_> = all.iter().filter(|(id, _)| *id >= 42001).cloned().collect();
    assert_eq!(
        ours,
        vec![(42001, shaped()), (42002, PlaylistFx::default())]
    );
}

#[test]
fn a_stored_value_that_does_not_read_or_validate_plays_untouched() {
    assert_eq!(row_fx(1, 0.0, "not json"), PlaylistFx::default());
    assert_eq!(row_fx(1, 50.0, "[]"), PlaylistFx::default());
    assert_eq!(
        row_fx(1, 0.0, r#"[{"kind":"peak","freq_hz":5}]"#),
        PlaylistFx::default()
    );
    let eq = serde_json::to_string(&shaped().eq).unwrap();
    assert_eq!(row_fx(1, -5.5, &eq), shaped());
}
