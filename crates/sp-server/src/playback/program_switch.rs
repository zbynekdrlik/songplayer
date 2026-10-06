//! The ONE switch path (#221 L2, design record 5873773896 §1c): a scene
//! press becomes a cut of `SP-program`, decided from SongPlayer's own
//! playlists, never from cg OBS's scene items. The #213 facade's
//! `SetCurrentProgramScene` and its studio-mode `TriggerStudioModeTransition`
//! both land here.
//!
//! One switch at a time, in arrival order, across every client: the bus's
//! `switch_order` lock is held for the whole switch.
//!
//! 1. The scene catalog (`scene_catalog`, one `get_active_playlists` read)
//!    says what the scene is.
//! 2. A PLAYLIST scene is cut (`persist_and_cut`, published as on air with
//!    the catalog name), and cg OBS is told NOTHING. #221 B4 step 6 deleted
//!    the legacy mirror (the same `SetCurrentProgramScene` to cg OBS, for the
//!    consumers that took cg OBS's program): every consumer takes
//!    `SP-program` now, and cg OBS keeps whatever manual scene it shows.
//! 3. A MANUAL scene goes to cg OBS FIRST, awaited under the lock: "OBS
//!    manuál" carries cg OBS's program, and a later press must not overtake
//!    it. cg OBS accepted → cut to "OBS manuál" (-1) while the NDI input is a
//!    source, else keep ([`INPUT_INACTIVE`]). Refused or not reachable → keep
//!    ([`NOT_SWITCHED`]); the caller passes cg OBS's answer through.
//! 4. "OBS manuál" itself (`PROGRAM_INPUT_LABEL`, the resolver's name for -1
//!    with no scene, e.g. a transition with no preview after the input was
//!    restored at startup) is the NDI input: cut to -1 while it is a source,
//!    with no scene to send to cg OBS (it keeps what it shows, like a
//!    dashboard cut to -1); else keep ([`INPUT_INACTIVE`]).
//! 5. The cut uses the bus's current transition spec unchanged. A manual →
//!    manual press keeps -1 (no mix) and publishes the new scene name.
//!
//! Every switch is recorded as `remote.last_remote_cut`, with `via` (what
//! triggered it) and `cg_forward` (cg OBS's answer to a manual scene's
//! forward; `null` when nothing went to cg OBS).
//!
//! #221 L4a: `POST /api/v1/program/cut` switches a SOURCE through the same
//! path ([`switch_source`], `via=dashboard`): a playlist is cut with its
//! catalog scene, -1 ("OBS manuál") with none; neither tells cg OBS
//! anything. ROZHODNUTÉ 6022247729: a playlist whose catalog names no scene
//! (inactive, or no / a shared NDI output name) is REFUSED ([`cut_scene`],
//! recorded as a keep, 409): every consumer takes `SP-program`, so such a
//! cut would black them all at once. [`refused_sources`] lists those
//! playlists by the same rule for the dashboard, which disables their
//! buttons.

use serde::Serialize;
use serde_json::{Value, json};
use sp_core::config::{PROGRAM_INPUT_ID, PROGRAM_INPUT_LABEL};
use sqlx::SqlitePool;
use tracing::{info, warn};

use crate::playback::ndi_input::load_input_settings;
use crate::playback::program_bus::{ProgramBus, ProgramStatus, persist_and_cut};
use crate::playback::scene_catalog::{SceneCatalog, SceneKind, load_catalog};
use crate::remote::protocol::Reply;
use crate::remote::{RemoteCut, Upstream, clip, now_ms};

/// `cg_forward` when cg OBS accepted.
pub const CG_OK: &str = "ok";
/// `cg_forward` when cg OBS is not reachable or did not answer in time.
pub const CG_NOT_READY: &str = "not_ready";
/// The keep reason of a manual scene cg OBS did not accept (unknown scene,
/// not reachable).
pub const NOT_SWITCHED: &str = "not_switched";
/// The keep reason of "OBS manuál" (a manual scene, or the input itself)
/// while the NDI input is not a source.
pub const INPUT_INACTIVE: &str = "input_inactive";
/// The keep reason of a switch whose playlists could not be read.
pub const CATALOG_FAILED: &str = "catalog_failed";
/// The keep reason of a switch whose source could not be persisted.
pub const PERSIST_FAILED: &str = "persist_failed";
/// The keep reasons of a refused dashboard cut (#221 ROZHODNUTÉ 6022247729):
/// an INACTIVE playlist (it has no output, so SP-program — and every
/// consumer of it — would carry black), and an active one whose catalog
/// names no scene. ONE vocabulary with the dashboard's texts, in sp-core.
pub use sp_core::program_refusal::{NO_SCENE, PLAYLIST_INACTIVE};

/// Why a dashboard cut to a playlist is refused (409, #221 ROZHODNUTÉ
/// 6022247729).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The playlist is inactive.
    Inactive,
    /// The playlist is active, but its catalog names no scene.
    NoScene,
}

impl Refusal {
    /// The keep reason ([`PLAYLIST_INACTIVE`] / [`NO_SCENE`]).
    pub fn reason(self) -> &'static str {
        match self {
            Self::Inactive => PLAYLIST_INACTIVE,
            Self::NoScene => NO_SCENE,
        }
    }

    /// The answer's text.
    pub fn message(self) -> &'static str {
        match self {
            Self::Inactive => {
                "the playlist is inactive: it has no output, so a cut would put black on SP-program and every consumer of it"
            }
            Self::NoScene => {
                "the playlist names no scene (no NDI output name, or one another active playlist shares), so it cannot go on SP-program"
            }
        }
    }
}

/// The scene a dashboard cut to playlist `pid` is published with, or why it
/// is refused: inactive (not in the catalog of the active playlists), or
/// active with no scene. The ONE rule of [`switch_source`] and
/// [`refused_sources`].
pub fn cut_scene(catalog: &SceneCatalog, pid: i64) -> Result<&str, Refusal> {
    match catalog.scene_of(pid) {
        Some(scene) => Ok(scene),
        None if catalog.is_active(pid) => Err(Refusal::NoScene),
        None => Err(Refusal::Inactive),
    }
}

/// What a dashboard cut ([`switch_source`]) could not do.
#[derive(Debug)]
pub enum SourceError {
    /// The playlist cannot go on program; recorded as a keep with its
    /// reason, nothing cut.
    Refused(Refusal),
    /// Reading the playlists or persisting the source failed; recorded as a
    /// keep, nothing cut.
    Store(sqlx::Error),
}

/// A playlist a dashboard cut refuses now (`cut_refused` on both program
/// answers).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RefusedSource {
    pub source: i64,
    /// [`PLAYLIST_INACTIVE`] or [`NO_SCENE`].
    pub reason: &'static str,
}

/// Every playlist a dashboard cut refuses now, in id order: the switch's own
/// catalog read plus the playlists' ids, decided by [`cut_scene`]. Two reads,
/// not one snapshot: a playlist (de)activated between them is listed wrongly
/// for one dashboard poll at most; the cut itself always decides from its
/// own read under the `switch_order`.
pub async fn refused_sources(pool: &SqlitePool) -> Result<Vec<RefusedSource>, sqlx::Error> {
    let catalog = load_catalog(pool).await?;
    let ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM playlists ORDER BY id")
        .fetch_all(pool)
        .await?;
    Ok(ids
        .into_iter()
        .filter_map(|source| {
            let refusal = cut_scene(&catalog, source).err()?;
            Some(RefusedSource {
                source,
                reason: refusal.reason(),
            })
        })
        .collect())
}

/// What triggered a switch (`last_remote_cut.via`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Via {
    /// `SetCurrentProgramScene`.
    Program,
    /// `TriggerStudioModeTransition` (the client's preview scene).
    Transition,
    /// #221 L4a: `POST /api/v1/program/cut` (the dashboard's Program control).
    Dashboard,
}

impl Via {
    /// The telemetry label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Program => "program",
            Self::Transition => "transition",
            Self::Dashboard => "dashboard",
        }
    }
}

/// What a switch did, for the caller's answer (it is already recorded).
#[derive(Debug)]
pub enum Switched {
    /// `SP-program` was cut (to the playlist, or to "OBS manuál").
    Cut,
    /// A manual scene cg OBS switched to (or "OBS manuál" itself) while the
    /// NDI input is not a source: the program is kept.
    Kept,
    /// cg OBS refused the manual scene (its op=7 `d`, to pass through) or
    /// did not answer (`None`): the program is kept.
    NotSwitched(Option<Value>),
    /// Reading the playlists or persisting the source failed: nothing was
    /// cut.
    StoreFailed(sqlx::Error),
}

/// What a scene press works on: the store, the program bus and cg OBS (for
/// a manual scene's forward).
pub struct SwitchCtx<'a> {
    pub pool: &'a SqlitePool,
    pub bus: &'a ProgramBus,
    pub upstream: &'a Upstream,
}

/// cg OBS's answer as `cg_forward`: `ok`, `error <code>` (its own
/// `requestStatus.code`), or `not_ready` when there was none.
pub fn cg_forward_label(answer: Option<&Value>) -> String {
    match answer.map(Reply::from_upstream) {
        None => CG_NOT_READY.to_string(),
        Some(reply) if reply.succeeded() => CG_OK.to_string(),
        Some(reply) => format!("error {}", reply.status["code"]),
    }
}

/// The record of a switch that kept the program for `reason`.
fn kept(pressed: &str, via: Via, reason: &'static str, cg_forward: Option<String>) -> RemoteCut {
    RemoteCut {
        scene: clip(pressed),
        action: "keep",
        source: None,
        reason: Some(reason),
        cut_boundary_100ns: None,
        at_ms: now_ms(),
        via: via.label(),
        cg_forward,
    }
}

/// The record of a switch that cut the program to `source`.
fn cut_done(
    pressed: &str,
    via: Via,
    source: i64,
    status: &ProgramStatus,
    cg_forward: Option<String>,
) -> RemoteCut {
    RemoteCut {
        scene: clip(pressed),
        action: if source == PROGRAM_INPUT_ID {
            "input"
        } else {
            "playlist"
        },
        source: Some(source),
        reason: None,
        cut_boundary_100ns: status.cut_boundary_100ns,
        at_ms: now_ms(),
        via: via.label(),
        cg_forward,
    }
}

/// Switch `SP-program` to `scene` (the module doc). The outcome is recorded
/// as `remote.last_remote_cut`.
pub async fn switch_scene(ctx: &SwitchCtx<'_>, scene: &str, via: Via) -> Switched {
    let _order = ctx.bus.switch_order().lock().await;
    let catalog = match load_catalog(ctx.pool).await {
        Ok(catalog) => catalog,
        Err(e) => {
            warn!(scene = %clip(scene), %e, "program switch: reading the playlists failed — SP-program unchanged");
            ctx.bus
                .remote()
                .record_cut(kept(scene, via, CATALOG_FAILED, None));
            return Switched::StoreFailed(e);
        }
    };
    match catalog.kind(scene) {
        SceneKind::Playlist(pid) => {
            // The catalog's name: the lowercased NDI output name.
            let name = catalog.scene_of(pid).unwrap_or(scene).to_string();
            let cut = cut_and_record(ctx.pool, ctx.bus, scene, via, pid, Some(&name), None);
            match cut.await {
                Ok(_) => Switched::Cut,
                Err(e) => Switched::StoreFailed(e),
            }
        }
        SceneKind::Manual if scene == PROGRAM_INPUT_LABEL => switch_input(ctx, scene, via).await,
        SceneKind::Manual => switch_manual(ctx, scene, via).await,
    }
}

/// #221 L4a: switch `SP-program` to a SOURCE (the dashboard's
/// `POST /api/v1/program/cut`, which validated it: a known playlist, or the
/// NDI input while it is a source) through the same path as a press, under
/// the same `switch_order`, recorded with `via`:
/// - a playlist is cut, published with its catalog scene;
/// - one whose catalog names no scene is REFUSED ([`cut_scene`]: inactive,
///   or no / a shared NDI output name), recorded as a keep with its reason
///   (#221 ROZHODNUTÉ 6022247729; it was cut with no scene before);
/// - -1 ("OBS manuál") is cut with no scene, which the resolver names.
///
/// cg OBS is told nothing (#221 B4 step 6: no mirror). Returns the program
/// state of THIS cut (a later switch may already run when the caller reads
/// the bus). `Err` when the cut is refused, or the playlists could not be
/// read or the source could not be persisted: then nothing was cut, and it
/// is recorded as a keep.
pub async fn switch_source(
    pool: &SqlitePool,
    bus: &ProgramBus,
    source: i64,
    via: Via,
) -> Result<ProgramStatus, SourceError> {
    let _order = bus.switch_order().lock().await;
    if source == PROGRAM_INPUT_ID {
        let cut = cut_and_record(pool, bus, PROGRAM_INPUT_LABEL, via, source, None, None);
        return cut.await.map_err(SourceError::Store);
    }
    let catalog = match load_catalog(pool).await {
        Ok(catalog) => catalog,
        Err(e) => {
            warn!(source, %e, "program switch: reading the playlists failed — SP-program unchanged");
            let record = kept(&source.to_string(), via, CATALOG_FAILED, None);
            bus.remote().record_cut(record);
            return Err(SourceError::Store(e));
        }
    };
    let name = match cut_scene(&catalog, source) {
        Ok(name) => name.to_string(),
        Err(refusal) => {
            warn!(
                source,
                reason = refusal.reason(),
                "program switch: a cut to this playlist is refused ({}) — SP-program unchanged",
                refusal.message()
            );
            let record = kept(&source.to_string(), via, refusal.reason(), None);
            bus.remote().record_cut(record);
            return Err(SourceError::Refused(refusal));
        }
    };
    let cut = cut_and_record(pool, bus, &name, via, source, Some(&name), None);
    cut.await.map_err(SourceError::Store)
}

/// "OBS manuál" itself: the NDI input, with no scene for cg OBS (module doc,
/// step 4).
async fn switch_input(ctx: &SwitchCtx<'_>, scene: &str, via: Via) -> Switched {
    let input_active = load_input_settings(ctx.pool)
        .await
        .is_ok_and(|s| s.active());
    if !input_active {
        warn!(
            "program switch: \"OBS manuál\" is not a source (the NDI input is off or has no source) — SP-program unchanged"
        );
        let record = kept(scene, via, INPUT_INACTIVE, None);
        ctx.bus.remote().record_cut(record);
        return Switched::Kept;
    }
    let cut = cut_and_record(ctx.pool, ctx.bus, scene, via, PROGRAM_INPUT_ID, None, None);
    match cut.await {
        Ok(_) => Switched::Cut,
        Err(e) => Switched::StoreFailed(e),
    }
}

/// A manual scene: cg OBS first (awaited), then "OBS manuál" while the NDI
/// input is a source.
async fn switch_manual(ctx: &SwitchCtx<'_>, scene: &str, via: Via) -> Switched {
    let data = json!({ "sceneName": scene });
    let answer = ctx
        .upstream
        .request("SetCurrentProgramScene", Some(data))
        .await;
    let forward = cg_forward_label(answer.as_ref());
    let accepted = answer
        .as_ref()
        .map(Reply::from_upstream)
        .is_some_and(|reply| reply.succeeded());
    if !accepted {
        warn!(scene = %clip(scene), cg_forward = %forward, "program switch: cg OBS did not switch to the manual scene — SP-program unchanged");
        let record = kept(scene, via, NOT_SWITCHED, Some(forward));
        ctx.bus.remote().record_cut(record);
        return Switched::NotSwitched(answer);
    }
    let input_active = load_input_settings(ctx.pool)
        .await
        .is_ok_and(|s| s.active());
    if !input_active {
        warn!(scene = %clip(scene), "program switch: a manual scene, but the NDI input \"OBS manuál\" is not a source — SP-program unchanged");
        let record = kept(scene, via, INPUT_INACTIVE, Some(forward));
        ctx.bus.remote().record_cut(record);
        return Switched::Kept;
    }
    let cut = cut_and_record(
        ctx.pool,
        ctx.bus,
        scene,
        via,
        PROGRAM_INPUT_ID,
        Some(scene),
        Some(forward),
    );
    match cut.await {
        Ok(_) => Switched::Cut,
        Err(e) => Switched::StoreFailed(e),
    }
}

/// Persist + cut to `source` (published as on air for `name`, when there is
/// one) and record it. A failed persist cuts nothing and is recorded as
/// `persist_failed`. Returns the program state of this cut.
async fn cut_and_record(
    pool: &SqlitePool,
    bus: &ProgramBus,
    pressed: &str,
    via: Via,
    source: i64,
    name: Option<&str>,
    cg_forward: Option<String>,
) -> Result<ProgramStatus, sqlx::Error> {
    let shared = bus.remote();
    match persist_and_cut(pool, bus, source, name).await {
        Ok(status) => {
            info!(
                scene = %clip(pressed),
                source,
                via = via.label(),
                cut_boundary_100ns = ?status.cut_boundary_100ns,
                "program switch: SP-program cut"
            );
            shared.record_cut(cut_done(pressed, via, source, &status, cg_forward));
            Ok(status)
        }
        Err(e) => {
            warn!(scene = %clip(pressed), source, %e, "program switch: persisting the program source failed — nothing cut");
            shared.record_cut(kept(pressed, via, PERSIST_FAILED, cg_forward));
            Err(e)
        }
    }
}

#[cfg(test)]
#[path = "program_switch_tests.rs"]
mod tests;
