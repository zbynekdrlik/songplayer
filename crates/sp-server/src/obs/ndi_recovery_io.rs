//! #173: executor for the dark-wall recovery ladder.
//!
//! The pure ladder (`obs/ndi_recovery.rs`) decides WHICH rung fires; this module
//! performs the OBS-WebSocket I/O for each rung over SongPlayer's already-healthy
//! WebSocket. Every rung is receiver-side (never a per-sender NDI recreate, #60).
//!
//! * `ClearRestore` — delegates to the round-1 nudge
//!   (`ndi_discovery::reapply_ndi_input`): clear + restore the input's
//!   `ndi_source_name` to the ADVERTISED, case-correct value.
//! * `ToggleSceneItem` — disable then re-enable the input's scene item, so
//!   DistroAV tears down and recreates the receiver object; then read
//!   `GetSceneItemEnabled` back and re-enable if an operator left it hidden
//!   (round 3 — the toggle must never leave an on-program item dark).
//! * `RecreateInput` — rename-first (round 3): rename the OLD input to a unique
//!   temp name (a SYNCHRONOUS rename that frees the original name), create the
//!   replacement DIRECTLY under the original name with the identical settings
//!   (advertised name), PROVE it exists, restore the saved transform + index, THEN
//!   remove the renamed-away old input. Never empties the scene AND never reuses a
//!   name freed by a remove — so it dodges DistroAV's async-teardown `601` race
//!   that left every create-first recreate named `<input>_recover` (box 17.9.2026).
//!   The step ORDER is `recreate_plan()`, whose two safety invariants are
//!   unit-tested here.
//!
//! I/O only — `obs/` is excluded from the mutation gate; the rung SELECTION is
//! unit-tested in `ndi_recovery.rs`, the ordering + pure helpers here are
//! unit-tested, and the whole ladder is exercised on the box by E2E test 12 and
//! the `POST /api/v1/ndi/recover/{id}?step=recreate` operator trigger.

use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

use crate::obs::SharedWrite;
use crate::obs::dispatcher::{DEFAULT_RESPONSE_TIMEOUT, Dispatcher};
use crate::obs::ndi_discovery::{
    advertised_ndi_host, extract_ndi_stream_name, fetch_input_ndi_sender_name,
    fetch_ndi_input_names, reapply_ndi_input,
};
use crate::obs::ndi_recovery::RecoveryStep;
use crate::obs::ndi_remove::{RemovalSite, remove_input_for_site};
use crate::obs::text::{
    create_input_request, get_input_settings_request, get_scene_item_enabled_request,
    get_scene_item_transform_request, get_scene_items_request, get_scene_list_request,
    set_input_name_request, set_scene_item_enabled_request, set_scene_item_index_request,
    set_scene_item_transform_request,
};

/// The resolved location of an NDI input's scene item.
struct SceneItemLocation {
    scene_name: String,
    scene_item_id: i64,
    scene_item_index: i64,
}

/// One ordered step of the RENAME-FIRST rung-2 recreate (#173 round 3). The order
/// is the SAFETY contract with TWO invariants, both unit-tested by
/// `recreate_plan_is_race_free`:
///
/// 1. **Never empty the scene / prove before destroy** — the replacement is
///    created and verified before the old input is removed.
/// 2. **Never reuse a NAME freed by a REMOVE** — a `RemoveInput` frees the OBS
///    input NAME only ASYNCHRONOUSLY (DistroAV tears the `ndi_source` down on its
///    own thread), so creating / renaming-to a just-removed name races that
///    teardown and hits obs-websocket `601 "a source already exists by that new
///    input name"` (box 17.9.2026: the round-3 create-temp-then-rename executor
///    left every recreate's input named `<input>_recover` because the rename-back
///    to the just-removed original name lost that race). So the original name is
///    freed by a SYNCHRONOUS `SetInputName` (rename), never by a remove, and the
///    replacement is created DIRECTLY under it — no post-remove name reuse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecreateStep {
    /// `SetInputName` the OLD input to a unique temp name — a synchronous rename
    /// that frees the original name immediately (no `ndi_source` destroy), so the
    /// create below can reuse it without racing an async teardown.
    RenameOldAway,
    /// `CreateInput` the replacement DIRECTLY under the ORIGINAL name (now free)
    /// with the identical kind + settings (advertised sender name), same scene.
    CreateUnderOriginal,
    /// Prove the new (original-named) input exists as a scene item (`CreateInput`
    /// returned a `sceneItemId` AND `GetSceneItemList` lists it) before removing
    /// anything.
    VerifyExists,
    /// Restore the saved transform + z-order index onto the new input.
    ApplyTransformIndex,
    /// `RemoveInput` the renamed-away OLD input — its temp name is never reused,
    /// so its async teardown is harmless.
    RemoveRenamedOld,
}

/// The ordered plan the recreate executor follows. Pure + `pub(crate)` so the
/// executor consumes it (no dead code) and a unit test locks BOTH ordering
/// invariants (see `RecreateStep`).
pub(crate) fn recreate_plan() -> [RecreateStep; 5] {
    use RecreateStep::*;
    // The original name is freed by a synchronous rename BEFORE the replacement is
    // created under it, and the old is removed only after the replacement is
    // verified — `recreate_plan_is_race_free` locks both invariants.
    [
        RenameOldAway,
        CreateUnderOriginal,
        VerifyExists,
        ApplyTransformIndex,
        RemoveRenamedOld,
    ]
}

/// A unique temp name the OLD input is renamed to while recreating `<input>`.
/// UUID-suffixed so `RenameOldAway` can never collide with a leftover from a
/// prior interrupted recreate, and so the create-under-original below never
/// touches a name that any remove ever freed.
fn temp_recover_name(input_name: &str) -> String {
    let suffix: String = uuid::Uuid::new_v4().simple().to_string();
    format!("{input_name}__recover_{}", &suffix[..8])
}

/// True if `candidate` is a leftover temp name from an EARLIER interrupted
/// recreate of `base` (`<base>__recover_<...>`), so it is safe to sweep
/// best-effort at the START of a fresh attempt. Never matches `base` itself, so
/// the live input can never be swept.
fn is_stale_recover_input(candidate: &str, base: &str) -> bool {
    candidate != base && candidate.starts_with(&format!("{base}__recover_"))
}

/// Execute one rung of the dark-wall recovery ladder for `target_stream` (the
/// bare stream, e.g. `"SP-fast"`). Logs its own outcome; never panics.
pub(crate) async fn execute(
    write: &SharedWrite,
    dispatcher: &Dispatcher,
    target_stream: &str,
    step: RecoveryStep,
) {
    match step {
        RecoveryStep::ClearRestore => {
            let outcome = reapply_ndi_input(write, dispatcher, target_stream).await;
            info!(
                target = target_stream,
                ?outcome,
                "ndi-recovery: rung 0 (clear+restore) outcome"
            );
        }
        RecoveryStep::ToggleSceneItem => {
            toggle_scene_item(write, dispatcher, target_stream).await;
        }
        RecoveryStep::RecreateInput => {
            recreate_input(write, dispatcher, target_stream).await;
        }
    }
}

/// Rung 1: disable then re-enable the input's scene item so DistroAV recreates
/// the receiver object (the operator's hide/show remedy).
async fn toggle_scene_item(write: &SharedWrite, dispatcher: &Dispatcher, target_stream: &str) {
    let input_name = match resolve_input_name(write, dispatcher, target_stream).await {
        Some(n) => n,
        None => {
            warn!(
                target = target_stream,
                "ndi-recovery: rung 1 (toggle) — no OBS NDI input advertises this stream"
            );
            return;
        }
    };
    let loc = match resolve_scene_item(write, dispatcher, &input_name).await {
        Some(l) => l,
        None => {
            warn!(
                input_name = %input_name,
                "ndi-recovery: rung 1 (toggle) — input is not a scene item in any scene"
            );
            return;
        }
    };

    info!(
        input_name = %input_name,
        scene = %loc.scene_name,
        scene_item_id = loc.scene_item_id,
        "ndi-recovery: rung 1 — toggling scene item OFF→ON to recreate the receiver"
    );

    let off = send_ok_logged(
        write,
        dispatcher,
        "rung 1 SetSceneItemEnabled(off)",
        set_scene_item_enabled_request(&new_id(), &loc.scene_name, loc.scene_item_id, false),
    )
    .await;
    let on = send_ok_logged(
        write,
        dispatcher,
        "rung 1 SetSceneItemEnabled(on)",
        set_scene_item_enabled_request(&new_id(), &loc.scene_name, loc.scene_item_id, true),
    )
    .await;

    // #173 round 3: PROVE the item is enabled. An operator who hid the source on
    // program leaves it disabled; the OFF→ON toggle above re-enables it, but read
    // back to be sure — the ladder must never leave an on-program item hidden on a
    // dark wall.
    let enabled_now = send(
        write,
        dispatcher,
        get_scene_item_enabled_request(&new_id(), &loc.scene_name, loc.scene_item_id),
    )
    .await
    .and_then(|v| v["d"]["responseData"]["sceneItemEnabled"].as_bool());
    if enabled_now == Some(false) {
        let re_on = send_ok_logged(
            write,
            dispatcher,
            "rung 1 SetSceneItemEnabled(re-enable)",
            set_scene_item_enabled_request(&new_id(), &loc.scene_name, loc.scene_item_id, true),
        )
        .await;
        info!(
            input_name = %input_name,
            re_enabled_ok = re_on,
            "ndi-recovery: rung 1 — item was disabled, re-enabled"
        );
    }

    if off && on {
        info!(input_name = %input_name, "ndi-recovery: rung 1 (toggle) applied");
    } else {
        warn!(
            input_name = %input_name,
            off,
            on,
            "ndi-recovery: rung 1 (toggle) did not fully apply"
        );
    }
}

/// Rung 2 (#173 round 3): rename-first recreate. Rename the OLD input to a unique
/// temp name (a synchronous rename that frees the original name), create the
/// replacement DIRECTLY under the ORIGINAL name with the identical kind + settings
/// (advertised, case-correct `ndi_source_name`), PROVE it exists as a scene item,
/// restore the saved transform + z-order, THEN remove the renamed-away old input.
/// On any pre-remove failure the OLD content is restored to its original name (the
/// scene is never emptied — the box incident, 17.9.2026) and no name freed by a
/// remove is ever reused (dodging DistroAV's async-teardown 601 race). The step
/// ORDER is driven by `recreate_plan()`, whose two safety invariants are
/// unit-tested.
async fn recreate_input(write: &SharedWrite, dispatcher: &Dispatcher, target_stream: &str) {
    let input_name = match resolve_input_name(write, dispatcher, target_stream).await {
        Some(n) => n,
        None => {
            warn!(
                target = target_stream,
                "ndi-recovery: rung 2 (recreate) — no OBS NDI input advertises this stream"
            );
            return;
        }
    };

    // Sweep any leftover `<input>__recover_*` temps from an earlier interrupted
    // recreate BEFORE this attempt. Their names are unique-per-attempt and never
    // reused, so their async teardown is harmless — this only clears
    // operator-visible garbage that would otherwise accrue over failed attempts.
    // Best-effort: a read/remove failure is logged, never fatal.
    if let Some(inputs) = fetch_ndi_input_names(write, dispatcher).await {
        let stale: Vec<String> = inputs
            .into_iter()
            .filter(|n| is_stale_recover_input(n, &input_name))
            .collect();
        if !stale.is_empty() {
            warn!(
                input_name = %input_name,
                stale_count = stale.len(),
                stale = ?stale,
                "ndi-recovery: rung 2 — sweeping leftover <input>__recover_* temps from an earlier interrupted recreate"
            );
            for name in &stale {
                // #173 round 5: HARD-remove — a bare RemoveInput is ineffective
                // against a still-receiving temp (box round 4), so stop the
                // receiver first, remove, then read back. Resolve the stale
                // input's scene item for the RemoveSceneItem fallback; `None`
                // (not a scene item) is fine — the fallback is then skipped.
                let (scene, item) = match resolve_scene_item(write, dispatcher, name).await {
                    Some(l) => (l.scene_name, Some(l.scene_item_id)),
                    None => (String::new(), None),
                };
                remove_input_for_site(
                    write,
                    dispatcher,
                    RemovalSite::StaleRecoverSweep,
                    &scene,
                    item,
                    name,
                )
                .await;
            }
        }
    }

    // --- Read-only preconditions: everything below is captured BEFORE any
    // destructive op, so a failure here aborts with the original untouched. ---

    // Capture the current input settings + kind so the recreate is identical.
    let settings_resp = match send(
        write,
        dispatcher,
        get_input_settings_request(&new_id(), &input_name),
    )
    .await
    {
        Some(v) => v,
        None => {
            warn!(input_name = %input_name, "ndi-recovery: rung 2 — GetInputSettings failed; aborting recreate (original untouched)");
            return;
        }
    };
    let input_kind = settings_resp["d"]["responseData"]["inputKind"]
        .as_str()
        .unwrap_or("ndi_source")
        .to_string();
    let mut input_settings = settings_resp["d"]["responseData"]["inputSettings"].clone();
    if input_settings.is_null() {
        input_settings = serde_json::json!({});
    }

    // Restore the ADVERTISED (case-correct) sender name (round-1 contract), never
    // the stored lowercase form DistroAV fails to re-attach to.
    let stored = input_settings["ndi_source_name"].as_str().unwrap_or("");
    let restore_to =
        advertised_sender_name(advertised_ndi_host().as_deref(), stored, target_stream);
    input_settings["ndi_source_name"] = serde_json::Value::String(restore_to.clone());

    // Resolve the scene + item so we can restore the transform and index.
    let loc = match resolve_scene_item(write, dispatcher, &input_name).await {
        Some(l) => l,
        None => {
            warn!(input_name = %input_name, "ndi-recovery: rung 2 — input is not a scene item in any scene; aborting recreate (original untouched)");
            return;
        }
    };

    // Save the OLD item's transform (best-effort — a missing transform still lets
    // the recreate re-attach the receiver, which is the point).
    let transform = send(
        write,
        dispatcher,
        get_scene_item_transform_request(&new_id(), &loc.scene_name, loc.scene_item_id),
    )
    .await
    .and_then(|v| {
        let xf = v["d"]["responseData"]["sceneItemTransform"].clone();
        if xf.is_null() { None } else { Some(xf) }
    });

    let temp_name = temp_recover_name(&input_name);

    info!(
        input_name = %input_name,
        temp_name = %temp_name,
        scene = %loc.scene_name,
        input_kind = %input_kind,
        restore_to = %restore_to,
        "ndi-recovery: rung 2 — rename-first recreate to clear a wedged receiver"
    );

    // --- Execute the ordered plan. The order is the SAFETY contract: rename the
    // old away (frees the original name synchronously), create the replacement
    // DIRECTLY under the original name, prove it, then remove the renamed-away
    // old. No step ever reuses a name that a remove freed. ---
    let mut new_item_id: Option<i64> = None;
    for step in recreate_plan() {
        match step {
            RecreateStep::RenameOldAway => {
                // Synchronous rename — frees the ORIGINAL name immediately (no
                // ndi_source destroy) so CreateUnderOriginal below can reuse it.
                if !send_ok_logged(
                    write,
                    dispatcher,
                    "rung 2 SetInputName(old→temp)",
                    set_input_name_request(&new_id(), &input_name, &temp_name),
                )
                .await
                {
                    warn!(input_name = %input_name, "ndi-recovery: rung 2 — RenameOldAway failed; aborting recreate (original untouched)");
                    return;
                }
            }
            RecreateStep::CreateUnderOriginal => {
                let resp = send(
                    write,
                    dispatcher,
                    create_input_request(
                        &new_id(),
                        &loc.scene_name,
                        &input_name,
                        &input_kind,
                        &input_settings,
                        true,
                    ),
                )
                .await;
                match resp
                    .as_ref()
                    .and_then(|v| v["d"]["responseData"]["sceneItemId"].as_i64())
                {
                    Some(id) => new_item_id = Some(id),
                    None => {
                        log_obs_failure("rung 2 CreateUnderOriginal", &input_name, resp.as_ref());
                        // Create failed → the original name is free (freed by the
                        // rename, never by a remove), so rename the old input back to
                        // it race-free; the scene keeps serving under the correct name.
                        restore_old(write, dispatcher, &temp_name, &input_name).await;
                        return;
                    }
                }
            }
            RecreateStep::VerifyExists => {
                let listed = resolve_scene_item(write, dispatcher, &input_name)
                    .await
                    .is_some();
                if new_item_id.is_none() || !listed {
                    warn!(
                        input_name = %input_name,
                        has_scene_item_id = new_item_id.is_some(),
                        listed_by_get_scene_item_list = listed,
                        "ndi-recovery: rung 2 — replacement not proven; aborting BEFORE removing the old input"
                    );
                    // Restore the old input's original name ONLY when the create
                    // did not return an id — then `input_name` is free and the
                    // rename-back is race-free. If the create DID return an id but
                    // the list read failed, the new input already occupies
                    // `input_name`, so a restore would futilely 601; leave the new
                    // in place (it is serving) and the old lingering under the temp
                    // name (a logged duplicate) rather than churn OBS.
                    if new_item_id.is_none() {
                        restore_old(write, dispatcher, &temp_name, &input_name).await;
                    }
                    return;
                }
            }
            RecreateStep::ApplyTransformIndex => {
                let Some(id) = new_item_id else { continue };
                if let Some(xf) = transform.clone() {
                    if !send_ok_logged(
                        write,
                        dispatcher,
                        "rung 2 SetSceneItemTransform",
                        set_scene_item_transform_request(
                            &new_id(),
                            &loc.scene_name,
                            id,
                            &transform_for_set(xf),
                        ),
                    )
                    .await
                    {
                        warn!(input_name = %input_name, "ndi-recovery: rung 2 — restoring the transform did not apply (continuing)");
                    }
                }
                if !send_ok_logged(
                    write,
                    dispatcher,
                    "rung 2 SetSceneItemIndex",
                    set_scene_item_index_request(
                        &new_id(),
                        &loc.scene_name,
                        id,
                        loc.scene_item_index,
                    ),
                )
                .await
                {
                    warn!(input_name = %input_name, "ndi-recovery: rung 2 — restoring the z-order index did not apply (continuing)");
                }
            }
            RecreateStep::RemoveRenamedOld => {
                // #173 round 5: HARD-remove the renamed-away old input. A bare
                // RemoveInput of a still-receiving DistroAV ndi_source reports
                // success yet leaves the input + its scene item (box round 4), so
                // stop the receiver (clear ndi_source_name) FIRST, remove, read
                // back, and fall back to RemoveSceneItem. The rename kept the old
                // item's id (`loc.scene_item_id`) in its original scene. The new
                // input under the correct name is already serving regardless.
                remove_input_for_site(
                    write,
                    dispatcher,
                    RemovalSite::RecreateRemoveRenamedOld,
                    &loc.scene_name,
                    Some(loc.scene_item_id),
                    &temp_name,
                )
                .await;
            }
        }
    }

    info!(
        input_name = %input_name,
        new_scene_item_id = new_item_id.unwrap_or(0),
        "ndi-recovery: rung 2 (recreate) applied — new input created under the original name, old removed"
    );
}

/// Restore the OLD input's original name after an aborted recreate: rename
/// `temp_name` back to `input_name`. Race-free because the abort paths that call
/// it reach it with `input_name` free (freed by the earlier synchronous rename,
/// never by a remove). Best-effort — a failure is logged, not fatal.
async fn restore_old(
    write: &SharedWrite,
    dispatcher: &Dispatcher,
    temp_name: &str,
    input_name: &str,
) {
    if !send_ok_logged(
        write,
        dispatcher,
        "rung 2 SetInputName(restore old→original)",
        set_input_name_request(&new_id(), temp_name, input_name),
    )
    .await
    {
        warn!(
            temp_name = %temp_name,
            wanted = %input_name,
            "ndi-recovery: rung 2 — could not restore the old input's original name; it is still named with the temp suffix (matched by stream)"
        );
    }
}

/// Find the OBS NDI input whose `ndi_source_name` advertises `target_stream`.
async fn resolve_input_name(
    write: &SharedWrite,
    dispatcher: &Dispatcher,
    target_stream: &str,
) -> Option<String> {
    let input_names = fetch_ndi_input_names(write, dispatcher).await?;
    for input_name in input_names {
        if let Some(sender) = fetch_input_ndi_sender_name(write, dispatcher, &input_name).await {
            if extract_ndi_stream_name(&sender) == target_stream {
                return Some(input_name);
            }
        }
    }
    None
}

/// Find the scene + scene-item id + index for an input by scanning every scene's
/// item list. Returns the first scene that contains it (our `sp-*` inputs each
/// live in exactly one scene).
async fn resolve_scene_item(
    write: &SharedWrite,
    dispatcher: &Dispatcher,
    input_name: &str,
) -> Option<SceneItemLocation> {
    let scenes_resp = send(write, dispatcher, get_scene_list_request(&new_id())).await?;
    let scenes = scenes_resp["d"]["responseData"]["scenes"]
        .as_array()?
        .clone();
    for scene in scenes {
        let scene_name = match scene["sceneName"].as_str() {
            Some(n) => n,
            None => continue,
        };
        let items_resp = match send(
            write,
            dispatcher,
            get_scene_items_request(&new_id(), scene_name),
        )
        .await
        {
            Some(v) => v,
            None => continue,
        };
        let items = match items_resp["d"]["responseData"]["sceneItems"].as_array() {
            Some(a) => a,
            None => continue,
        };
        for item in items {
            if item["sourceName"].as_str() == Some(input_name) {
                return Some(SceneItemLocation {
                    scene_name: scene_name.to_string(),
                    scene_item_id: item["sceneItemId"].as_i64().unwrap_or(0),
                    scene_item_index: item["sceneItemIndex"].as_i64().unwrap_or(0),
                });
            }
        }
    }
    None
}

/// The advertised, case-correct sender name for `target_stream`, matching the
/// round-1 `reapply_ndi_input` contract: `"<host> (<stream>)"` when it is a pure
/// case variant of the stored value, else the stored value verbatim. Pure over
/// `host` (the advertised `COMPUTERNAME`, or `None` on Linux/CI) so it is
/// unit-testable without touching the process environment.
fn advertised_sender_name(host: Option<&str>, stored: &str, target_stream: &str) -> String {
    match host {
        Some(host) => {
            let advertised = format!("{host} ({target_stream})");
            if advertised.eq_ignore_ascii_case(stored) {
                advertised
            } else {
                stored.to_string()
            }
        }
        None => stored.to_string(),
    }
}

/// Strip the read-only / derived fields from a `sceneItemTransform` before it is
/// sent back via `SetSceneItemTransform`. OBS computes `width`/`height` from the
/// scale and `sourceWidth`/`sourceHeight` from the source, and rejects them as
/// out-of-range on write; the settable geometry (position, scale, rotation,
/// crop, bounds, alignment) is preserved.
fn transform_for_set(mut xf: serde_json::Value) -> serde_json::Value {
    if let Some(obj) = xf.as_object_mut() {
        for k in ["width", "height", "sourceWidth", "sourceHeight"] {
            obj.remove(k);
        }
    }
    xf
}

pub(crate) fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Send a request and return the full response value, or `None` on transport
/// failure.
pub(crate) async fn send(
    write: &SharedWrite,
    dispatcher: &Dispatcher,
    req: serde_json::Value,
) -> Option<serde_json::Value> {
    let req_id = req["d"]["requestId"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    match dispatcher
        .send_and_await(
            write,
            req_id,
            Message::Text(req.to_string().into()),
            DEFAULT_RESPONSE_TIMEOUT,
        )
        .await
    {
        Ok(v) => Some(v),
        Err(e) => {
            warn!(error = %e, "ndi-recovery: OBS request failed");
            None
        }
    }
}

/// Log an obs-websocket write failure with the step name AND the full
/// `requestStatus` code + comment (#173 round 3 — the box incident's CreateInput
/// error payload was never logged, so the cause was unknown). `resp == None` is a
/// transport failure (no reply); a present response with `result == false`
/// carries OBS's own diagnostic.
fn log_obs_failure(step: &str, name: &str, resp: Option<&serde_json::Value>) {
    match resp {
        Some(v) => {
            let code = v["d"]["requestStatus"]["code"].as_u64().unwrap_or(0);
            let comment = v["d"]["requestStatus"]["comment"].as_str().unwrap_or("");
            warn!(
                step,
                name, code, comment, "ndi-recovery: OBS request reported failure"
            );
        }
        None => warn!(
            step,
            name, "ndi-recovery: OBS request had no response (transport failure)"
        ),
    }
}

/// Send a request, log the full obs-websocket error on failure, and return
/// whether OBS acknowledged success.
pub(crate) async fn send_ok_logged(
    write: &SharedWrite,
    dispatcher: &Dispatcher,
    step: &str,
    req: serde_json::Value,
) -> bool {
    let name = req["d"]["requestData"]["inputName"]
        .as_str()
        .or_else(|| req["d"]["requestData"]["sceneName"].as_str())
        .unwrap_or("")
        .to_string();
    let resp = send(write, dispatcher, req).await;
    let ok = resp
        .as_ref()
        .map(|v| v["d"]["requestStatus"]["result"].as_bool().unwrap_or(false))
        .unwrap_or(false);
    if !ok {
        log_obs_failure(step, &name, resp.as_ref());
    }
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- rename-first recreate ordering safety (#173 round 3) -------------

    #[test]
    fn recreate_plan_is_race_free() {
        let plan = recreate_plan();
        let pos = |s: RecreateStep| plan.iter().position(|&p| p == s).expect("step present");
        // The whole plan must be exactly the safe rename-first order.
        assert_eq!(
            plan,
            [
                RecreateStep::RenameOldAway,
                RecreateStep::CreateUnderOriginal,
                RecreateStep::VerifyExists,
                RecreateStep::ApplyTransformIndex,
                RecreateStep::RemoveRenamedOld,
            ],
            "the recreate plan must free the original name by a rename, create+verify under it, then remove the renamed-away old",
        );
        // Invariant 1 — never reuse a name freed by a REMOVE: the original name is
        // freed by the synchronous RenameOldAway BEFORE CreateUnderOriginal reuses
        // it (the async-teardown 601 race, box 17.9.2026).
        assert!(
            pos(RecreateStep::RenameOldAway) < pos(RecreateStep::CreateUnderOriginal),
            "must free the original name (rename the old away) before creating under it",
        );
        // Invariant 2 — never empty the scene / prove before destroy: the
        // replacement is created and verified before the old input is removed.
        assert!(
            pos(RecreateStep::CreateUnderOriginal) < pos(RecreateStep::VerifyExists),
            "must create the replacement before verifying it",
        );
        assert!(
            pos(RecreateStep::VerifyExists) < pos(RecreateStep::RemoveRenamedOld),
            "must verify the replacement exists before removing the renamed-away old input",
        );
    }

    #[test]
    fn temp_recover_name_is_unique_and_distinct_from_the_original() {
        let a = temp_recover_name("sp-youth_video");
        let b = temp_recover_name("sp-youth_video");
        assert!(
            a.starts_with("sp-youth_video__recover_"),
            "carries the base + recover marker: {a}"
        );
        assert_ne!(a, "sp-youth_video");
        // UUID-suffixed → two calls never collide (so RenameOldAway can never hit a
        // leftover temp from a prior interrupted recreate).
        assert_ne!(a, b, "temp names must be unique per call");
    }

    #[test]
    fn plan_never_reuses_a_removed_name() {
        // Model each step's effect on the two input NAMES it can touch:
        //  - `acquires`: the name this step `CreateInput`s or `SetInputName`s TO
        //    (a name that MUST be free when the step runs).
        //  - `removes`: the name this step `RemoveInput`s — DistroAV frees it only
        //    ASYNCHRONOUSLY (the `ndi_source` teardown runs on its own thread), so
        //    the name stays reserved for a while after the request returns.
        // The race-free invariant (design round 4): no step may acquire a name a
        // PRIOR step removed — that is exactly the 601 async-teardown race.
        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        enum Name {
            Original,
            Temp,
        }
        fn effect(s: RecreateStep) -> (Option<Name>, Option<Name>) {
            use RecreateStep::*;
            match s {
                // Original -> Temp: a synchronous rename; it ACQUIRES the temp name
                // (frees the original synchronously, so `Original` is never a
                // remove-freed name).
                RenameOldAway => (Some(Name::Temp), None),
                // CreateInput under the (now free) original name.
                CreateUnderOriginal => (Some(Name::Original), None),
                VerifyExists => (None, None),
                ApplyTransformIndex => (None, None),
                // RemoveInput the renamed-away old; frees `Temp` only async.
                RemoveRenamedOld => (None, Some(Name::Temp)),
            }
        }
        let mut removed: Vec<Name> = Vec::new();
        for step in recreate_plan() {
            let (acquires, removes) = effect(step);
            if let Some(n) = acquires {
                assert!(
                    !removed.contains(&n),
                    "step {step:?} acquires {n:?}, which a PRIOR step removed — \
                     that reuses a remove-freed name and hits the 601 async-teardown race",
                );
            }
            if let Some(n) = removes {
                removed.push(n);
            }
        }
    }

    #[test]
    fn is_stale_recover_input_matches_only_this_bases_recover_temps() {
        let base = "sp-youth_video";
        // A leftover temp from an earlier interrupted recreate of THIS base.
        assert!(is_stale_recover_input(
            "sp-youth_video__recover_ab12cd34",
            base
        ));
        // The live input itself is never stale (must never be swept).
        assert!(!is_stale_recover_input("sp-youth_video", base));
        // A different base's recover temp is not ours.
        assert!(!is_stale_recover_input(
            "sp-slow_video__recover_ab12cd34",
            base
        ));
        // A base that is a prefix of ours must not match our temp (no cross-scope).
        assert!(!is_stale_recover_input(
            "sp-youth_video__recover_ab12cd34",
            "sp-youth"
        ));
        // An unrelated input.
        assert!(!is_stale_recover_input("sp-fast_video", base));
    }

    #[test]
    fn transform_for_set_strips_read_only_fields() {
        let xf = serde_json::json!({
            "positionX": 10.0,
            "scaleX": 1.0,
            "boundsWidth": 1920,
            "width": 1920,
            "height": 960,
            "sourceWidth": 1920,
            "sourceHeight": 960,
        });
        let out = transform_for_set(xf);
        // Settable geometry preserved.
        assert_eq!(out["positionX"], 10.0);
        assert_eq!(out["scaleX"], 1.0);
        assert_eq!(out["boundsWidth"], 1920);
        // Derived / read-only fields removed.
        assert!(out.get("width").is_none());
        assert!(out.get("height").is_none());
        assert!(out.get("sourceWidth").is_none());
        assert!(out.get("sourceHeight").is_none());
    }

    #[test]
    fn advertised_sender_name_prefers_case_correct_when_a_variant() {
        let host = Some("RESOLUME-SNV");
        // A lowercase stored value that is a pure case variant → rewrite.
        assert_eq!(
            advertised_sender_name(host, "resolume-snv (SP-slow)", "SP-slow"),
            "RESOLUME-SNV (SP-slow)"
        );
        // A genuinely different stored host (not a case variant) → keep stored.
        assert_eq!(
            advertised_sender_name(host, "OTHER-BOX (SP-slow)", "SP-slow"),
            "OTHER-BOX (SP-slow)"
        );
        // Host unknown (Linux/CI) → keep the stored value verbatim.
        assert_eq!(
            advertised_sender_name(None, "resolume-snv (SP-slow)", "SP-slow"),
            "resolume-snv (SP-slow)"
        );
    }
}
