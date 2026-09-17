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
    create_input_request, get_input_settings_request, get_scene_item_transform_request,
    get_scene_items_request, get_scene_list_request, remove_input_request,
    set_scene_item_enabled_request, set_scene_item_index_request, set_scene_item_transform_request,
};

/// The resolved location of an NDI input's scene item.
struct SceneItemLocation {
    scene_name: String,
    scene_item_id: i64,
    scene_item_index: i64,
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

    let off = send_ok(
        write,
        dispatcher,
        set_scene_item_enabled_request(&new_id(), &loc.scene_name, loc.scene_item_id, false),
    )
    .await;
    let on = send_ok(
        write,
        dispatcher,
        set_scene_item_enabled_request(&new_id(), &loc.scene_name, loc.scene_item_id, true),
    )
    .await;

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

/// Rung 2: remove the input and recreate it with the identical settings
/// (advertised, case-correct `ndi_source_name`), restoring the saved scene-item
/// transform and z-order index — the strongest receiver-side remedy short of
/// restarting OBS.
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
            warn!(input_name = %input_name, "ndi-recovery: rung 2 — GetInputSettings failed; aborting recreate");
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
            warn!(input_name = %input_name, "ndi-recovery: rung 2 — input is not a scene item in any scene; aborting recreate");
            return;
        }
    };

    // Save the transform (best-effort — a missing transform still lets the
    // recreate re-attach the receiver, which is the point).
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

    info!(
        input_name = %input_name,
        scene = %loc.scene_name,
        input_kind = %input_kind,
        restore_to = %restore_to,
        "ndi-recovery: rung 2 — remove + recreate input to clear a wedged receiver"
    );

    if !send_ok(
        write,
        dispatcher,
        remove_input_request(&new_id(), &input_name),
    )
    .await
    {
        warn!(input_name = %input_name, "ndi-recovery: rung 2 — RemoveInput failed; aborting recreate");
        return;
    }

    let create_resp = send(
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
    let new_item_id = match create_resp
        .as_ref()
        .and_then(|v| v["d"]["responseData"]["sceneItemId"].as_i64())
    {
        Some(id) => id,
        None => {
            warn!(
                input_name = %input_name,
                "ndi-recovery: rung 2 — CreateInput did not return a sceneItemId; the input may be missing until the next OBS map rebuild"
            );
            return;
        }
    };

    // Restore transform + z-order index (both best-effort; the receiver is
    // already re-created at this point).
    if let Some(xf) = transform {
        let applied = send_ok(
            write,
            dispatcher,
            set_scene_item_transform_request(
                &new_id(),
                &loc.scene_name,
                new_item_id,
                &transform_for_set(xf),
            ),
        )
        .await;
        if !applied {
            warn!(input_name = %input_name, "ndi-recovery: rung 2 — restoring the scene-item transform did not apply");
        }
    }
    let indexed = send_ok(
        write,
        dispatcher,
        set_scene_item_index_request(
            &new_id(),
            &loc.scene_name,
            new_item_id,
            loc.scene_item_index,
        ),
    )
    .await;
    if !indexed {
        warn!(input_name = %input_name, "ndi-recovery: rung 2 — restoring the scene-item index did not apply");
    }

    info!(
        input_name = %input_name,
        new_scene_item_id = new_item_id,
        "ndi-recovery: rung 2 (recreate) applied"
    );
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

/// Send a request and return whether OBS acknowledged success.
async fn send_ok(write: &SharedWrite, dispatcher: &Dispatcher, req: serde_json::Value) -> bool {
    match send(write, dispatcher, req).await {
        Some(v) => v["d"]["requestStatus"]["result"].as_bool().unwrap_or(false),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
