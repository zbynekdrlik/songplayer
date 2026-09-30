//! #221 L1: the scene catalog — which scene is a playlist's, from the
//! playlists' `ndi_output_name` alone. Wired via `#[cfg(test)] #[path =
//! "scene_catalog_tests.rs"] mod tests;`.

use sp_core::models::Playlist;
use sqlx::SqlitePool;

use super::*;

/// The 10 live playlists (read on the box 28.9.2026: every one has a cg OBS
/// scene named its lowercased NDI output name).
const LIVE: [(i64, &str); 10] = [
    (2, "SP-warmup"),
    (3, "SP-presence"),
    (4, "SP-slow"),
    (5, "SP-90s"),
    (6, "SP-worship"),
    (7, "SP-fast"),
    (8, "SP-live"),
    (9, "SP-youth"),
    (10, "SP-alex"),
    (11, "SP-dabing"),
];

#[test]
fn the_ten_live_playlists_are_their_scenes() {
    let catalog = SceneCatalog::new(LIVE);
    for (pid, ndi_name) in LIVE {
        let scene = ndi_name.to_ascii_lowercase();
        assert_eq!(catalog.kind(&scene), SceneKind::Playlist(pid), "{scene}");
        assert_eq!(catalog.scene_of(pid), Some(scene.as_str()));
    }
    // The 4 manual cg OBS scenes on the box show no SongPlayer input.
    for manual in ["Blank", "Svedectvo", "Trailer", "CG Manual"] {
        assert_eq!(catalog.kind(manual), SceneKind::Manual, "{manual}");
    }
    assert!(catalog.conflicts().is_empty());
}

#[test]
fn a_scene_matches_its_playlist_ignoring_ascii_case_only() {
    let catalog = SceneCatalog::new([(7, "SP-fast"), (12, "SP-Ďábel")]);
    assert_eq!(catalog.kind("sp-fast"), SceneKind::Playlist(7));
    assert_eq!(catalog.kind("SP-FAST"), SceneKind::Playlist(7));
    assert_eq!(catalog.kind("Sp-Fast"), SceneKind::Playlist(7));
    assert_eq!(catalog.scene_of(7), Some("sp-fast"));
    // Only ASCII letters fold: the rest of the name must match exactly.
    assert_eq!(catalog.kind("sp-Ďábel"), SceneKind::Playlist(12));
    assert_eq!(catalog.scene_of(12), Some("sp-Ďábel"));
    assert_eq!(catalog.kind("sp-ďÁbel"), SceneKind::Manual);
    // A different name is a manual scene, and so is none at all.
    assert_eq!(catalog.kind("sp-fast "), SceneKind::Manual);
    assert_eq!(catalog.kind("fast"), SceneKind::Manual);
    assert_eq!(catalog.kind(""), SceneKind::Manual);
    assert_eq!(catalog.scene_of(99), None);
}

#[test]
fn an_empty_or_shared_ndi_name_names_no_scene() {
    let catalog = SceneCatalog::new([(1, ""), (2, "   "), (3, "SP-x"), (4, "sp-X"), (5, "SP-y")]);
    assert_eq!(catalog.kind("sp-x"), SceneKind::Manual);
    assert_eq!(catalog.kind(""), SceneKind::Manual);
    assert_eq!(catalog.kind("   "), SceneKind::Manual);
    for pid in 1..=4 {
        assert_eq!(catalog.scene_of(pid), None, "playlist {pid}");
    }
    assert_eq!(catalog.kind("sp-y"), SceneKind::Playlist(5));
    assert_eq!(catalog.scene_of(5), Some("sp-y"));
    assert_eq!(
        catalog.conflicts(),
        [
            "playlist 1 has no NDI output name",
            "playlist 2 has no NDI output name",
            "playlists [3, 4] share the NDI output name \"sp-x\"",
        ]
    );
}

#[test]
fn the_catalog_of_playlist_rows_reads_their_ndi_output_names() {
    let row = |id: i64, ndi: &str| Playlist {
        id,
        ndi_output_name: ndi.to_string(),
        ..Default::default()
    };
    let catalog = SceneCatalog::from_playlists(&[row(4, "SP-slow"), row(7, "SP-fast")]);
    assert_eq!(catalog, SceneCatalog::new([(4, "SP-slow"), (7, "SP-fast")]));
    assert_eq!(catalog.kind("sp-slow"), SceneKind::Playlist(4));
    assert_eq!(catalog.scene_of(7), Some("sp-fast"));
}

#[test]
fn each_conflict_is_warned_once() {
    // Names no other test uses: the "warned" set is process-wide.
    let first = [
        "test-221 conflict a".to_string(),
        "test-221 conflict b".to_string(),
    ];
    assert_eq!(warn_new_conflicts(&first), 2);
    assert_eq!(warn_new_conflicts(&first), 0, "each is logged once");
    let more = [
        "test-221 conflict b".to_string(),
        "test-221 conflict c".to_string(),
    ];
    assert_eq!(warn_new_conflicts(&more), 1, "only the new one");
    assert_eq!(warn_new_conflicts(&[]), 0);
}

async fn pool_with(playlists: &[(i64, &str, bool)]) -> SqlitePool {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    for &(id, ndi, active) in playlists {
        sqlx::query(
            "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(format!("p{id}"))
        .bind(format!("https://youtube.com/playlist?list=p{id}"))
        .bind(ndi)
        .bind(i32::from(active))
        .execute(&pool)
        .await
        .unwrap();
    }
    pool
}

#[tokio::test]
async fn the_stored_catalog_holds_only_the_active_playlists() {
    let pool = pool_with(&[
        (4, "SP-slow", true),
        (7, "SP-fast", true),
        (9, "SP-old", false),
    ])
    .await;
    let catalog = load_catalog(&pool).await.unwrap();
    assert_eq!(catalog, SceneCatalog::new([(4, "SP-slow"), (7, "SP-fast")]));
    assert_eq!(scene_of_source(&pool, 7).await.as_deref(), Some("sp-fast"));
    assert_eq!(scene_of_source(&pool, 4).await.as_deref(), Some("sp-slow"));
    assert_eq!(scene_of_source(&pool, 9).await, None, "inactive");
    assert_eq!(
        scene_of_source(&pool, sp_core::config::PROGRAM_INPUT_ID).await,
        None,
        "the NDI input is named by the resolver, not the catalog"
    );
    // An unreadable store: no scene name, never a panic.
    pool.close().await;
    assert!(load_catalog(&pool).await.is_err());
    assert_eq!(scene_of_source(&pool, 7).await, None);
}
