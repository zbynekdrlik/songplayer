//! Scene identity from SongPlayer's own data (#221 L1, design record
//! 5873773896 §1b): which scene name is a playlist's, with no cg OBS lookup.
//!
//! - A scene is a PLAYLIST scene when exactly one ACTIVE playlist's
//!   `ndi_output_name` equals it, ignoring ASCII case. Its name is that
//!   `ndi_output_name` lowercased (`SP-fast` → `sp-fast`, the scene cg OBS
//!   shows the playlist in; all 10 live playlists follow this, read
//!   28.9.2026).
//! - #245: `Blank` (ASCII case ignored) is SongPlayer's own black, the
//!   program source `PROGRAM_BLANK_ID`; a playlist whose NDI output name is
//!   "Blank" names no scene (a logged conflict), so the name stays Blank's.
//! - Every other name is a MANUAL scene (cg OBS's media, browser, Slido…).
//! - A playlist with an empty `ndi_output_name`, or one whose name another
//!   active playlist shares, names no scene. Each such conflict is logged
//!   once as a WARN.
//! - The catalog knows which playlists are active (`is_active`), so a
//!   dashboard cut refused for a playlist that names no scene can say why:
//!   inactive, or active with no scene (`program_switch::cut_scene`, #221
//!   ROZHODNUTÉ 6022247729).
//!
//! The #221 switch path decides with this catalog, never with cg OBS's scene
//! items. A `playlists.scene_name` column was rejected in the design: nothing
//! else would use it.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, PoisonError};

use sp_core::config::{PROGRAM_BLANK_ID, PROGRAM_BLANK_LABEL, is_blank_scene};
use sp_core::models::Playlist;
use sqlx::SqlitePool;
use tracing::warn;

/// What a scene name is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SceneKind {
    /// The scene of this playlist.
    Playlist(i64),
    /// #245: SongPlayer's own black (`PROGRAM_BLANK_ID`).
    Blank,
    /// Not a playlist's scene (a manual cg OBS scene).
    Manual,
}

/// The playlist scenes of the active playlists.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SceneCatalog {
    /// Scene name (the lowercased `ndi_output_name`) → playlist id.
    scenes: BTreeMap<String, i64>,
    /// The playlists that name no scene, one log line each.
    conflicts: Vec<String>,
    /// Every playlist the catalog was built from: the active ones.
    active: BTreeSet<i64>,
}

impl SceneCatalog {
    /// The catalog of `(playlist id, ndi_output_name)` pairs, one per ACTIVE
    /// playlist.
    pub fn new<'a>(playlists: impl IntoIterator<Item = (i64, &'a str)>) -> Self {
        let mut named: BTreeMap<String, Vec<i64>> = BTreeMap::new();
        let mut conflicts = Vec::new();
        let mut active = BTreeSet::new();
        for (pid, ndi_name) in playlists {
            active.insert(pid);
            if ndi_name.trim().is_empty() {
                conflicts.push(format!("playlist {pid} has no NDI output name"));
            } else if is_blank_scene(ndi_name) {
                conflicts.push(format!(
                    "playlist {pid}'s NDI output name {ndi_name:?} is SongPlayer's Blank scene"
                ));
            } else {
                let scene = ndi_name.to_ascii_lowercase();
                named.entry(scene).or_default().push(pid);
            }
        }
        let mut scenes = BTreeMap::new();
        for (scene, pids) in named {
            match pids.as_slice() {
                [pid] => {
                    scenes.insert(scene, *pid);
                }
                _ => conflicts.push(format!(
                    "playlists {pids:?} share the NDI output name {scene:?}"
                )),
            }
        }
        Self {
            scenes,
            conflicts,
            active,
        }
    }

    /// The catalog of these (active) playlists.
    pub fn from_playlists(playlists: &[Playlist]) -> Self {
        Self::new(playlists.iter().map(|p| (p.id, p.ndi_output_name.as_str())))
    }

    /// What `scene` is, ignoring ASCII case.
    pub fn kind(&self, scene: &str) -> SceneKind {
        if is_blank_scene(scene) {
            return SceneKind::Blank;
        }
        match self.scenes.get(&scene.to_ascii_lowercase()) {
            Some(&pid) => SceneKind::Playlist(pid),
            None => SceneKind::Manual,
        }
    }

    /// Playlist `pid`'s scene name, `None` when it names none (inactive, no
    /// NDI output name, or a name another playlist shares).
    pub fn scene_of(&self, pid: i64) -> Option<&str> {
        self.scenes
            .iter()
            .find(|&(_, &p)| p == pid)
            .map(|(scene, _)| scene.as_str())
    }

    /// The playlists that name no scene, one log line each.
    pub fn conflicts(&self) -> &[String] {
        &self.conflicts
    }

    /// Whether playlist `pid` is one the catalog was built from (an ACTIVE
    /// playlist), whether it names a scene or not.
    pub fn is_active(&self, pid: i64) -> bool {
        self.active.contains(&pid)
    }
}

/// The conflicts already logged: each distinct one is WARNed once per
/// process, not on every press.
static WARNED: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// WARN each conflict not logged before; returns how many were new.
pub(crate) fn warn_new_conflicts(conflicts: &[String]) -> usize {
    let mut warned = WARNED.lock().unwrap_or_else(PoisonError::into_inner);
    let mut new = 0;
    for conflict in conflicts {
        if warned.insert(conflict.clone()) {
            warn!(
                conflict = %conflict,
                "scene catalog: this playlist names no scene — a press of its name is a manual scene"
            );
            new += 1;
        }
    }
    new
}

/// The catalog of the active playlists (one `get_active_playlists` read);
/// its conflicts are WARNed once.
pub async fn load_catalog(pool: &SqlitePool) -> Result<SceneCatalog, sqlx::Error> {
    let playlists = crate::db::models::get_active_playlists(pool).await?;
    let catalog = SceneCatalog::from_playlists(&playlists);
    warn_new_conflicts(catalog.conflicts());
    Ok(catalog)
}

/// The scene a program source is published with when nobody pressed one
/// (the startup restore): the playlist's catalog scene, `Blank` for Blank
/// (#245). `None` for the NDI input (the resolver names it "OBS manuál"),
/// for a playlist that names no scene, and when the playlists cannot be
/// read (WARN).
pub async fn scene_of_source(pool: &SqlitePool, source: i64) -> Option<String> {
    if source == PROGRAM_BLANK_ID {
        return Some(PROGRAM_BLANK_LABEL.to_string());
    }
    match load_catalog(pool).await {
        Ok(catalog) => catalog.scene_of(source).map(str::to_string),
        Err(e) => {
            warn!(%e, source, "scene catalog: reading the playlists failed — the source is published without a scene name");
            None
        }
    }
}

#[cfg(test)]
#[path = "scene_catalog_tests.rs"]
mod tests;
