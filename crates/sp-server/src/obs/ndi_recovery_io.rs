//! #173 round 2: executor for the dark-wall recovery ladder.
//!
//! The pure ladder (`obs/ndi_recovery.rs`) decides WHICH rung fires; this module
//! performs the OBS-WebSocket I/O for each rung over SongPlayer's already-healthy
//! WebSocket. Every rung is receiver-side (never a per-sender NDI recreate, #60).
//!
//! * `ClearRestore` — delegates to the round-1 nudge
//!   (`ndi_discovery::reapply_ndi_input`): clear + restore the input's
//!   `ndi_source_name` to the ADVERTISED, case-correct value.
//! * `ToggleSceneItem` — disable then re-enable the input's scene item, so
//!   DistroAV tears down and recreates the receiver object.
//! * `RecreateInput` — remove the input and recreate it with the identical
//!   settings (advertised name), restoring the saved scene-item transform + index.
//!
//! I/O only — `obs/` is excluded from the mutation gate; the rung SELECTION is
//! unit-tested in `ndi_recovery.rs` and the whole ladder is exercised on the box
//! by E2E test 12. The one pure helper here (`transform_for_set`) is unit-tested.

use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

use crate::obs::SharedWrite;
use crate::obs::dispatcher::{DEFAULT_RESPONSE_TIMEOUT, Dispatcher};
use crate::obs::ndi_discovery::{
    advertised_ndi_host, extract_ndi_stream_name, fetch_input_ndi_sender_name,
    fetch_ndi_input_names, reapply_ndi_input,
};
use crate::obs::ndi_recovery::RecoveryStep;
use crate::obs::text::{
    create_input_request, get_input_settings_request, get_scene_item_enabled_request,
    get_scene_item_transform_request, get_scene_items_request, get_scene_list_request,
    remove_input_request, set_input_name_request, set_scene_item_enabled_request,
    set_scene_item_index_request, set_scene_item_transform_request,
};

/// The resolved location of an NDI input's scene item.
struct SceneItemLocation {
    scene_name: String,
    scene_item_id: i64,
    scene_item_index: i64,
}

/// One ordered step of the create-first-then-remove rung-2 recreate (#173 round
/// 3). The order is the SAFETY contract: the replacement input is created under a
/// temporary name and PROVEN to exist before the old input is removed, so a
/// failed `CreateInput` can never leave the scene empty (the box incident,
/// 17.9.2026). Unit-tested by `recreate_plan_never_removes_before_verify`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecreateStep {
    /// `CreateInput` the replacement under `<input>_recover` with the identical
    /// kind + settings (advertised sender name), in the same scene.
    CreateTemp,
    /// Prove the temp input exists as a scene item (`GetSceneItemList` lists it
    /// AND `CreateInput` returned a `sceneItemId`) before anything destructive.
    VerifyExists,
    /// Restore the saved transform + z-order index onto the temp item.
    ApplyTransformIndex,
    /// `RemoveInput` the old input — only reached after the replacement is proven.
    RemoveOld,
    /// `SetInputName` the temp `<input>_recover` back to the original name (the
    /// name is free once the old input is removed).
    RenameTempToOriginal,
}

/// The ordered plan the recreate executor follows. Pure + `pub(crate)` so the
/// executor consumes it (no dead code) and a unit test locks the ordering
/// invariant — the replacement is always created and verified before the old
/// input is removed.
pub(crate) fn recreate_plan() -> [RecreateStep; 5] {
    use RecreateStep::*;
    // RED (#173 round 3): the TIER-0 "one wrong constant" — RemoveOld is ordered
    // BEFORE VerifyExists, so `recreate_plan_never_removes_before_verify` fails
    // cleanly. GREEN moves RemoveOld after VerifyExists/ApplyTransformIndex.
    [
        CreateTemp,
        RemoveOld,
        VerifyExists,
        ApplyTransformIndex,
        RenameTempToOriginal,
    ]
}

/// The temporary input name used while recreating `<input>`. Distinct from the
/// original so `CreateInput` can never collide with the still-present old input
/// (the same-name collision that made `CreateInput` return no `sceneItemId` on
/// the box, 17.9.2026).
fn temp_recover_name(input_name: &str) -> String {
    format!("{input_name}_recover")
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

/// Rung 2 (#173 round 3): create-first-then-remove. Create the replacement input
/// under a TEMPORARY name (`<input>_recover`) with the identical kind + settings
/// (advertised, case-correct `ndi_source_name`), PROVE it exists as a scene item,
/// restore the saved transform + z-order, THEN remove the old input and rename
/// the temp to the original name. On any failure the ORIGINAL input is left
/// untouched (the scene is never emptied — the box incident, 17.9.2026) and the
/// temp is cleaned up. The step ORDER is driven by `recreate_plan()`, whose
/// safety invariant (never remove before verify) is unit-tested.
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
        "ndi-recovery: rung 2 — create-first recreate to clear a wedged receiver"
    );

    // Defensive: remove any stale temp left by a prior interrupted recreate so the
    // CreateTemp below can never collide with it (best-effort, result ignored — a
    // 600 "not found" is the normal case).
    let _ = send(
        write,
        dispatcher,
        remove_input_request(&new_id(), &temp_name),
    )
    .await;

    // --- Execute the ordered plan. The order is the SAFETY contract. ---
    let mut new_item_id: Option<i64> = None;
    for step in recreate_plan() {
        match step {
            RecreateStep::CreateTemp => {
                let resp = send(
                    write,
                    dispatcher,
                    create_input_request(
                        &new_id(),
                        &loc.scene_name,
                        &temp_name,
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
                        log_obs_failure("rung 2 CreateTemp", &temp_name, resp.as_ref());
                        // Original untouched; nothing created to clean up.
                        return;
                    }
                }
            }
            RecreateStep::VerifyExists => {
                let listed = resolve_scene_item(write, dispatcher, &temp_name)
                    .await
                    .is_some();
                if new_item_id.is_none() || !listed {
                    warn!(
                        temp_name = %temp_name,
                        has_scene_item_id = new_item_id.is_some(),
                        listed_by_get_scene_item_list = listed,
                        "ndi-recovery: rung 2 — replacement not proven (aborting BEFORE removing the old input; original untouched)"
                    );
                    cleanup_temp(write, dispatcher, &temp_name).await;
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
                        warn!(temp_name = %temp_name, "ndi-recovery: rung 2 — restoring the transform did not apply (continuing)");
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
                    warn!(temp_name = %temp_name, "ndi-recovery: rung 2 — restoring the z-order index did not apply (continuing)");
                }
            }
            RecreateStep::RemoveOld => {
                if !send_ok_logged(
                    write,
                    dispatcher,
                    "rung 2 RemoveInput(old)",
                    remove_input_request(&new_id(), &input_name),
                )
                .await
                {
                    // The old input survived AND the temp exists — a duplicate.
                    // Remove the temp so we do not leave two items; the original
                    // (still present) keeps serving.
                    warn!(input_name = %input_name, "ndi-recovery: rung 2 — RemoveInput(old) failed; removing the temp to avoid a duplicate (original untouched)");
                    cleanup_temp(write, dispatcher, &temp_name).await;
                    return;
                }
            }
            RecreateStep::RenameTempToOriginal => {
                if !send_ok_logged(
                    write,
                    dispatcher,
                    "rung 2 SetInputName(temp→original)",
                    set_input_name_request(&new_id(), &temp_name, &input_name),
                )
                .await
                {
                    // The old input is already gone and the temp carries the
                    // advertised name, so the receiver is attached; only the input
                    // NAME is wrong (still matched by stream on the next map
                    // rebuild). Loud so an operator can rename it by hand.
                    warn!(
                        temp_name = %temp_name,
                        wanted = %input_name,
                        "ndi-recovery: rung 2 — rename temp→original failed; the recovered input is still named '<input>_recover' (matched by stream, rename by hand)"
                    );
                    return;
                }
            }
        }
    }

    info!(
        input_name = %input_name,
        new_scene_item_id = new_item_id.unwrap_or(0),
        "ndi-recovery: rung 2 (recreate) applied — replacement proven, old removed, renamed"
    );
}

/// Best-effort removal of the temporary `<input>_recover` input on an aborted
/// recreate. Result ignored — it is a cleanup, not part of the safety contract.
async fn cleanup_temp(write: &SharedWrite, dispatcher: &Dispatcher, temp_name: &str) {
    let _ = send(
        write,
        dispatcher,
        remove_input_request(&new_id(), temp_name),
    )
    .await;
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

fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Send a request and return the full response value, or `None` on transport
/// failure.
async fn send(
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
async fn send_ok_logged(
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

    // ---- create-first-then-remove ordering safety (#173 round 3) ----------

    #[test]
    fn recreate_plan_never_removes_before_verify() {
        let plan = recreate_plan();
        let pos = |s: RecreateStep| plan.iter().position(|&p| p == s).expect("step present");
        // The whole plan must be exactly the safe order.
        assert_eq!(
            plan,
            [
                RecreateStep::CreateTemp,
                RecreateStep::VerifyExists,
                RecreateStep::ApplyTransformIndex,
                RecreateStep::RemoveOld,
                RecreateStep::RenameTempToOriginal,
            ],
            "the recreate plan must create + verify the replacement before removing the old input",
        );
        // The load-bearing invariant, asserted independently of the exact layout:
        // the old input is NEVER removed before the replacement is proven to exist.
        assert!(
            pos(RecreateStep::CreateTemp) < pos(RecreateStep::VerifyExists),
            "must create the temp before verifying it",
        );
        assert!(
            pos(RecreateStep::VerifyExists) < pos(RecreateStep::RemoveOld),
            "must verify the replacement exists before removing the old input",
        );
        assert!(
            pos(RecreateStep::RemoveOld) < pos(RecreateStep::RenameTempToOriginal),
            "must remove the old input (freeing its name) before renaming the temp to it",
        );
    }

    #[test]
    fn temp_recover_name_is_distinct_from_the_original() {
        assert_eq!(
            temp_recover_name("sp-youth_video"),
            "sp-youth_video_recover"
        );
        assert_ne!(temp_recover_name("sp-youth_video"), "sp-youth_video");
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
