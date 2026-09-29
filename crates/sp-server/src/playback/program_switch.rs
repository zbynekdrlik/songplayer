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
//! 2. A PLAYLIST scene is cut FIRST (`persist_and_cut`, published as on air
//!    with the catalog name), never gated on cg OBS. Then the legacy MIRROR:
//!    the same `SetCurrentProgramScene` is queued for cg OBS with `try_send`
//!    and is NOT awaited under the lock. It still does not overtake a later
//!    press (for switches cg OBS answers within the OBS client's 2 s; see
//!    `obs::remote_call`): the OBS client writes the facade's calls through
//!    ONE forwarder per connection (`obs::remote_call::run_calls`), which
//!    waits for a scene switch's answer before it writes the next call (cg
//!    OBS runs its messages on a thread pool) and drops a switch a later
//!    mirror supersedes. A spawned waiter (the upstream timeout + 4 s)
//!    records cg OBS's answer as `last_remote_cut.cg_forward`.
//!    The consumers that still take cg OBS's program (Arena, FOH, lv1,
//!    strih) follow it until B4 step 6 deletes it.
//! 3. A MANUAL scene goes to cg OBS FIRST, awaited under the lock: "OBS
//!    manuál" carries cg OBS's program, and a later press must not overtake
//!    it. cg OBS accepted → cut to "OBS manuál" (-1) while the NDI input is a
//!    source, else keep (`input_inactive`). Refused or not reachable → keep
//!    (`not_switched`); the caller passes cg OBS's answer through.
//! 4. "OBS manuál" itself (`PROGRAM_INPUT_LABEL`, the resolver's name for -1
//!    with no scene, e.g. a transition with no preview after the input was
//!    restored at startup) is the NDI input: cut to -1 while it is a source,
//!    with no scene to send to cg OBS (it keeps what it shows, like a
//!    dashboard cut to -1); else keep (`input_inactive`).
//! 5. The cut uses the bus's current transition spec unchanged. A manual →
//!    manual press keeps -1 (no mix) and publishes the new scene name.
//!
//! Every switch is recorded as `remote.last_remote_cut`, with `via` (what
//! triggered it) and `cg_forward` (cg OBS's answer).
//!
//! #221 L4a: `POST /api/v1/program/cut` switches a SOURCE through the same
//! path ([`switch_source`], `via=dashboard`): a playlist is cut with its
//! catalog scene first and then mirrored like a playlist press; -1 ("OBS
//! manuál") is a cut only (no scene to send, cg OBS keeps what it shows).
//! Every command to cg OBS takes a ticket of `legacy_cg` under the
//! `switch_order`, and cg OBS's OK is recorded there: a mirror → the
//! playlist, a manual scene → none (`playback::legacy_cg`).
//!
//! #221 L4b: at startup [`remirror_on_air`] tells cg OBS once, through the
//! same ticketed mirror, to show the restored playlist's scene.

use std::sync::Arc;

use serde_json::{Value, json};
use sp_core::config::{PROGRAM_INPUT_ID, PROGRAM_INPUT_LABEL};
use sqlx::SqlitePool;
use tokio::sync::oneshot;
use tracing::{debug, info, warn};

use crate::playback::legacy_cg::{LegacyCg, Ticket};
use crate::playback::ndi_input::load_input_settings;
use crate::playback::program_bus::{ProgramBus, ProgramStatus, persist_and_cut};
use crate::playback::scene_catalog::{SceneKind, load_catalog};
use crate::remote::map::KeepReason;
use crate::remote::protocol::Reply;
use crate::remote::{RemoteCut, RemoteShared, Upstream, clip, now_ms};

/// `cg_forward` while the mirror's answer is still due.
pub const CG_PENDING: &str = "pending";
/// `cg_forward` when cg OBS accepted.
pub const CG_OK: &str = "ok";
/// `cg_forward` when cg OBS is not reachable or did not answer in time.
pub const CG_NOT_READY: &str = "not_ready";
/// The keep reason of a switch whose playlists could not be read.
pub const CATALOG_FAILED: &str = "catalog_failed";
/// The keep reason of a switch whose source could not be persisted.
pub const PERSIST_FAILED: &str = "persist_failed";

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

/// What a switch works on: the store, the program bus and cg OBS.
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
        via: Some(via.label()),
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
        via: Some(via.label()),
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
            // The catalog's name: the lowercased NDI output name, the scene
            // cg OBS shows the playlist in.
            let name = catalog.scene_of(pid).unwrap_or(scene).to_string();
            match cut_and_record(ctx, scene, via, pid, Some(&name), None).await {
                Ok((cut_id, _)) => {
                    mirror(ctx, &name, pid, cut_id);
                    Switched::Cut
                }
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
/// - a playlist is cut first, published with its catalog scene, then
///   mirrored to cg OBS like a playlist press;
/// - one whose catalog names no scene (inactive, or no / a shared NDI output
///   name) is cut with no scene, and cg OBS is not told (WARN);
/// - -1 ("OBS manuál") is a cut only: no scene to send, cg OBS keeps what it
///   shows.
///
/// Returns the program state of THIS cut (a later switch may already run
/// when the caller reads the bus). `Err` when the playlists could not be
/// read or the source could not be persisted (then nothing was cut, and it
/// is recorded as a keep).
pub async fn switch_source(
    ctx: &SwitchCtx<'_>,
    source: i64,
    via: Via,
) -> Result<ProgramStatus, sqlx::Error> {
    let _order = ctx.bus.switch_order().lock().await;
    if source == PROGRAM_INPUT_ID {
        let (_, status) = cut_and_record(ctx, PROGRAM_INPUT_LABEL, via, source, None, None).await?;
        return Ok(status);
    }
    let catalog = match load_catalog(ctx.pool).await {
        Ok(catalog) => catalog,
        Err(e) => {
            warn!(source, %e, "program switch: reading the playlists failed — SP-program unchanged");
            let record = kept(&source.to_string(), via, CATALOG_FAILED, None);
            ctx.bus.remote().record_cut(record);
            return Err(e);
        }
    };
    let Some(name) = catalog.scene_of(source).map(str::to_string) else {
        warn!(
            source,
            "program switch: this playlist names no scene (inactive, or no / a shared NDI output name) — cut without a scene, cg OBS is not told"
        );
        let pressed = source.to_string();
        let (_, status) = cut_and_record(ctx, &pressed, via, source, None, None).await?;
        return Ok(status);
    };
    let (cut_id, status) = cut_and_record(ctx, &name, via, source, Some(&name), None).await?;
    mirror(ctx, &name, source, cut_id);
    Ok(status)
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
        let record = kept(scene, via, KeepReason::InputInactive.as_str(), None);
        ctx.bus.remote().record_cut(record);
        return Switched::Kept;
    }
    match cut_and_record(ctx, scene, via, PROGRAM_INPUT_ID, None, None).await {
        Ok(_) => Switched::Cut,
        Err(e) => Switched::StoreFailed(e),
    }
}

/// A manual scene: cg OBS first (awaited), then "OBS manuál" while the NDI
/// input is a source.
async fn switch_manual(ctx: &SwitchCtx<'_>, scene: &str, via: Via) -> Switched {
    let data = json!({ "sceneName": scene });
    let legacy = ctx.bus.legacy_cg();
    let ticket = legacy.ticket();
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
        let record = kept(scene, via, KeepReason::NotSwitched.as_str(), Some(forward));
        ctx.bus.remote().record_cut(record);
        return Switched::NotSwitched(answer);
    }
    // #221 L4a: cg OBS shows a manual scene now, no playlist.
    legacy.confirmed(ticket, None);
    let input_active = load_input_settings(ctx.pool)
        .await
        .is_ok_and(|s| s.active());
    if !input_active {
        warn!(scene = %clip(scene), "program switch: a manual scene, but the NDI input \"OBS manuál\" is not a source — SP-program unchanged");
        let record = kept(
            scene,
            via,
            KeepReason::InputInactive.as_str(),
            Some(forward),
        );
        ctx.bus.remote().record_cut(record);
        return Switched::Kept;
    }
    match cut_and_record(
        ctx,
        scene,
        via,
        PROGRAM_INPUT_ID,
        Some(scene),
        Some(forward),
    )
    .await
    {
        Ok(_) => Switched::Cut,
        Err(e) => Switched::StoreFailed(e),
    }
}

/// Persist + cut to `source` (published as on air for `name`, the scene cg
/// OBS shows it in, when there is one) and record it. A failed persist cuts
/// nothing and is recorded as `persist_failed`. Returns the record's id and
/// the program state of this cut.
async fn cut_and_record(
    ctx: &SwitchCtx<'_>,
    pressed: &str,
    via: Via,
    source: i64,
    name: Option<&str>,
    cg_forward: Option<String>,
) -> Result<(u64, ProgramStatus), sqlx::Error> {
    let shared = ctx.bus.remote();
    match persist_and_cut(ctx.pool, ctx.bus, source, name).await {
        Ok(status) => {
            info!(
                scene = %clip(pressed),
                source,
                via = via.label(),
                cut_boundary_100ns = ?status.cut_boundary_100ns,
                "program switch: SP-program cut"
            );
            let cut_id = shared.record_cut(cut_done(pressed, via, source, &status, cg_forward));
            Ok((cut_id, status))
        }
        Err(e) => {
            warn!(scene = %clip(pressed), source, %e, "program switch: persisting the program source failed — nothing cut");
            shared.record_cut(kept(pressed, via, PERSIST_FAILED, cg_forward));
            Err(e)
        }
    }
}

/// The legacy mirror of playlist `pid`'s switch: queue
/// `SetCurrentProgramScene {name}` for cg OBS (never awaited here) and let a
/// waiter record its answer as the cut's `cg_forward` (`pending` until then)
/// and, when cg OBS accepted, as `legacy_cg` showing `pid` (#221 L4a; by the
/// ticket taken here, under the `switch_order`). Deleted at B4 step 6.
fn mirror(ctx: &SwitchCtx<'_>, name: &str, pid: i64, cut_id: u64) {
    let shared = Arc::clone(ctx.bus.remote());
    let legacy = Arc::clone(ctx.bus.legacy_cg());
    let ticket = legacy.ticket();
    let data = json!({ "sceneName": name });
    match ctx
        .upstream
        .enqueue("SetCurrentProgramScene", Some(data), true)
    {
        Some(rx) => {
            shared.set_cg_forward(cut_id, CG_PENDING.to_string());
            let upstream = ctx.upstream.clone();
            let recorded = record_mirror(upstream, rx, shared, cut_id, clip(name));
            tokio::spawn(confirm_mirror(recorded, legacy, ticket, pid));
        }
        None => {
            warn!(scene = %clip(name), "program switch: cg OBS is not reachable (no OBS client, or its command queue is full) — its program does not follow this switch");
            shared.set_cg_forward(cut_id, CG_NOT_READY.to_string());
        }
    }
}

/// Wait for cg OBS's answer to a mirror (at most the upstream timeout plus
/// `MIRROR_EXTRA_WAIT`: the forwarder writes a mirror however late) and
/// record it as cut `cut_id`'s `cg_forward` (only while that cut is still the
/// last one). Returns whether cg OBS accepted it.
async fn record_mirror(
    upstream: Upstream,
    rx: oneshot::Receiver<Option<Value>>,
    shared: Arc<RemoteShared>,
    cut_id: u64,
    scene: String,
) -> bool {
    let answer = upstream.wait_mirror(rx).await;
    let label = cg_forward_label(answer.as_ref());
    let current = shared.set_cg_forward(cut_id, label.clone());
    log_mirror(&scene, &label, current);
    label == CG_OK
}

/// #221 L4a: once `recorded` (the mirror's waiter) says cg OBS accepted the
/// mirror of playlist `pid`, record it as shown by `ticket`.
async fn confirm_mirror(
    recorded: impl Future<Output = bool>,
    legacy: Arc<LegacyCg>,
    ticket: Ticket,
    pid: i64,
) {
    if recorded.await {
        legacy.confirmed(ticket, Some(pid));
    }
}

/// #221 L4b (main-session decision 1, comment 5884501960): at startup, tell
/// cg OBS ONCE, through the same ticketed mirror as a playlist press, to show
/// the restored program's playlist scene, so `legacy_cg.shown` (which the
/// restore seeded with that playlist) is what cg OBS was told. Nothing is
/// sent for the NDI input "OBS manuál" (-1: cg OBS keeps its manual scene),
/// when nothing was restored, or for a playlist whose catalog names no scene.
/// Nothing was pressed, so no `last_remote_cut`. Returns whether the mirror
/// was queued.
pub async fn remirror_on_air(bus: &ProgramBus, upstream: &Upstream) -> bool {
    let _order = bus.switch_order().lock().await;
    let on_air = bus.on_air_now();
    let playlist = on_air.source.filter(|&source| source == PROGRAM_INPUT_ID);
    let (Some(pid), Some(scene)) = (playlist, on_air.scene) else {
        debug!(source = ?on_air.source, "startup re-mirror: no playlist scene on program — cg OBS is not told");
        return false;
    };
    let legacy = Arc::clone(bus.legacy_cg());
    let ticket = legacy.ticket();
    let data = json!({ "sceneName": scene });
    let Some(rx) = upstream.enqueue("SetCurrentProgramScene", Some(data), true) else {
        warn!(scene = %clip(&scene), "startup re-mirror: cg OBS is not reachable (no OBS client, or its command queue is full)");
        return false;
    };
    info!(pid, scene = %clip(&scene), "startup re-mirror: cg OBS is told to show the restored playlist");
    let accepted = startup_answer(upstream.clone(), rx, clip(&scene));
    tokio::spawn(confirm_mirror(accepted, legacy, ticket, pid));
    true
}

/// The startup re-mirror's waiter: whether cg OBS accepted it, within the
/// same wait as a press's mirror (`Upstream::wait_mirror`).
async fn startup_answer(
    upstream: Upstream,
    rx: oneshot::Receiver<Option<Value>>,
    scene: String,
) -> bool {
    let label = cg_forward_label(upstream.wait_mirror(rx).await.as_ref());
    log_startup_answer(&scene, &label);
    label == CG_OK
}

/// The log line of the startup re-mirror's answer. Logging only.
#[cfg_attr(test, mutants::skip)]
fn log_startup_answer(scene: &str, label: &str) {
    if label == CG_OK {
        info!(
            scene,
            "startup re-mirror: cg OBS shows the restored playlist"
        );
    } else {
        warn!(
            scene,
            cg_forward = label,
            "startup re-mirror: cg OBS did not follow — the consumers on cg OBS keep their scene until the next switch"
        );
    }
}

/// The log line of a mirror's answer (`current`: its cut is still the last
/// one; a later press replaced — or superseded — it otherwise). Logging only.
#[cfg_attr(test, mutants::skip)]
fn log_mirror(scene: &str, label: &str, current: bool) {
    if label == CG_OK {
        debug!(scene, "program switch: cg OBS followed the mirrored press");
    } else if !current {
        debug!(
            scene,
            cg_forward = label,
            "program switch: a later press replaced this mirrored press"
        );
    } else {
        warn!(
            scene,
            cg_forward = label,
            "program switch: cg OBS did not follow the mirrored press — the consumers on cg OBS keep its previous scene"
        );
    }
}

#[cfg(test)]
#[path = "program_switch_tests.rs"]
mod tests;
