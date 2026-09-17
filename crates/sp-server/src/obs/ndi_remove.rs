//! #173 round 5: HARD-remove of a DistroAV NDI input.
//!
//! A bare `RemoveInput` of an ACTIVELY-RECEIVING DistroAV `ndi_source` returns
//! obs-websocket success yet the input persists — the libobs source destroy
//! blocks on the receiver thread, which never joins while the sender is up (box
//! 17.9.2026 round 4: `sp-youth_video__recover_f0831b7d` stayed listed 2.75+ min
//! after a "successful" `RemoveInput`, and a second manual `RemoveInput` also
//! "succeeded" without effect). Only clearing the input's `ndi_source_name` to
//! `""` first — which stops the receiver — then `RemoveInput` (+ a
//! `RemoveSceneItem` fallback), with the removal READ BACK, actually removes it.
//!
//! Both rung-2 removal call-sites route through here (`remove_input_for_site`):
//! the recreate's final `RemoveRenamedOld` step and the start-of-attempt
//! stale-`__recover_*` sweep — otherwise the recreate leaves a duplicate
//! receiver and the sweep never clears it, accruing one orphan per attempt.
//!
//! I/O only (`obs/` is excluded from the mutation gate); the ordered sub-plan
//! and the both-sites-hard-remove contract are unit-tested on the TIER-0 box.

use tracing::{info, warn};

use crate::obs::SharedWrite;
use crate::obs::dispatcher::Dispatcher;
use crate::obs::ndi_discovery::fetch_ndi_input_names;
use crate::obs::ndi_recovery_io::{new_id, send, send_ok_logged};
use crate::obs::text::{
    clear_ndi_source_name_request, get_scene_items_request, remove_input_request,
    remove_scene_item_request,
};

/// One ordered step of the round-5 hard-remove sub-plan. The order is the
/// contract (`removal_plan_stops_the_receiver_before_removing`): the receiver
/// MUST be stopped before `RemoveInput`, and every removal is READ BACK — never
/// trusted from the obs-websocket response code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RemovalStep {
    /// `SetInputSettings(name, {ndi_source_name: ""})` — DistroAV stops the
    /// receiver thread on an empty source, so the libobs source destroy below is
    /// no longer blocked on it.
    StopReceiver,
    /// `RemoveInput(name)` — now the source destroy can complete.
    RemoveInput,
    /// Read `GetInputList` (kind `ndi_source`) + `GetSceneItemList(scene)` back to
    /// learn whether the input / its scene item are actually gone.
    VerifyGone,
    /// If still present and a scene-item id is known, `RemoveSceneItem(scene, id)`
    /// — the fallback that clears an operator-visible duplicate.
    RemoveSceneItemFallback,
    /// Read back once more; still present → a loud WARN with both listings.
    VerifyGoneFinal,
}

/// The ordered hard-remove sub-plan the executor follows. Pure so a unit test
/// locks the order (`removal_plan_stops_the_receiver_before_removing`).
pub(crate) fn removal_plan() -> [RemovalStep; 5] {
    use RemovalStep::*;
    // NOTE(RED): StopReceiver must come FIRST — a bare RemoveInput before the
    // receiver stops is the round-4 no-op. GREEN reorders StopReceiver first.
    [
        RemoveInput,
        StopReceiver,
        VerifyGone,
        RemoveSceneItemFallback,
        VerifyGoneFinal,
    ]
}

/// The two rung-2 call-sites that must remove a wedged / renamed-away DistroAV
/// NDI input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RemovalSite {
    /// The rung-2 recreate's final `RecreateStep::RemoveRenamedOld` step.
    RecreateRemoveRenamedOld,
    /// The start-of-attempt stale-`__recover_*` sweep.
    StaleRecoverSweep,
}

/// The dispatch marker `remove_input_for_site` consults: does this call-site take
/// the hard-remove path? A bare `RemoveInput` is ineffective against a live
/// DistroAV receiver (round 4), so BOTH sites must — the recreate would else leave
/// a duplicate and the sweep would never clear one. Unit-tested by
/// `both_remove_sites_use_the_hard_remove_path`.
pub(crate) fn uses_hard_remove_path(site: RemovalSite) -> bool {
    match site {
        RemovalSite::RecreateRemoveRenamedOld => true,
        // NOTE(RED): the sweep must ALSO hard-remove, or orphans accrue one per
        // attempt (the sweep used a bare RemoveInput before round 5). GREEN → true.
        RemovalSite::StaleRecoverSweep => false,
    }
}

/// Remove an NDI input on behalf of `site`, dispatching through the hard-remove
/// path when the site opts in (both do today — see `uses_hard_remove_path`). The
/// bare fallback is retained only so the marker genuinely gates behavior; no site
/// takes it now (round 4 proved a bare remove ineffective vs a live receiver).
pub(crate) async fn remove_input_for_site(
    write: &SharedWrite,
    dispatcher: &Dispatcher,
    site: RemovalSite,
    scene_name: &str,
    scene_item_id: Option<i64>,
    input_name: &str,
) {
    if uses_hard_remove_path(site) {
        remove_ndi_input_hard(write, dispatcher, scene_name, scene_item_id, input_name).await;
    } else {
        send_ok_logged(
            write,
            dispatcher,
            "hard-remove disabled — bare RemoveInput (no site opts out today)",
            remove_input_request(&new_id(), input_name),
        )
        .await;
    }
}

/// Hard-remove a DistroAV NDI input that a bare `RemoveInput` cannot destroy while
/// its receiver is live: stop the receiver (clear `ndi_source_name`), remove the
/// input, READ BACK, fall back to `RemoveSceneItem`, and read back once more —
/// logging a loud WARN if the input still lingers (operator garbage; the
/// replacement under the correct name is serving). `scene_item_id == None` (the
/// input is not a known scene item) skips the fallback; an empty `scene_name`
/// skips the scene-item reads. Best-effort — never panics.
pub(crate) async fn remove_ndi_input_hard(
    write: &SharedWrite,
    dispatcher: &Dispatcher,
    scene_name: &str,
    scene_item_id: Option<i64>,
    input_name: &str,
) {
    let mut input_listed = true;
    let mut item_listed = true;
    for step in removal_plan() {
        match step {
            RemovalStep::StopReceiver => {
                // Clear ndi_source_name (overlay merge) — stops the receiver thread
                // so the RemoveInput below is not blocked on it. A failed read is
                // logged by send_ok_logged; the remove still proceeds.
                send_ok_logged(
                    write,
                    dispatcher,
                    "hard-remove SetInputSettings(clear ndi_source_name)",
                    clear_ndi_source_name_request(&new_id(), input_name),
                )
                .await;
            }
            RemovalStep::RemoveInput => {
                send_ok_logged(
                    write,
                    dispatcher,
                    "hard-remove RemoveInput",
                    remove_input_request(&new_id(), input_name),
                )
                .await;
            }
            RemovalStep::VerifyGone => {
                input_listed = input_still_listed(write, dispatcher, input_name)
                    .await
                    .unwrap_or(false);
                item_listed = scene_item_still_listed(write, dispatcher, scene_name, input_name)
                    .await
                    .unwrap_or(false);
                info!(
                    input_name,
                    scene = scene_name,
                    input_listed,
                    item_listed,
                    "ndi-recovery: hard-remove — read-back after stop-receiver + RemoveInput"
                );
            }
            RemovalStep::RemoveSceneItemFallback => {
                if input_listed || item_listed {
                    match (scene_item_id, scene_name.is_empty()) {
                        (Some(id), false) => {
                            send_ok_logged(
                                write,
                                dispatcher,
                                "hard-remove RemoveSceneItem(fallback)",
                                remove_scene_item_request(&new_id(), scene_name, id),
                            )
                            .await;
                        }
                        _ => {
                            info!(
                                input_name,
                                "ndi-recovery: hard-remove — still listed but no scene-item id to fall back on"
                            );
                        }
                    }
                }
            }
            RemovalStep::VerifyGoneFinal => {
                let final_input = input_still_listed(write, dispatcher, input_name)
                    .await
                    .unwrap_or(false);
                let final_item = scene_item_still_listed(write, dispatcher, scene_name, input_name)
                    .await
                    .unwrap_or(false);
                if final_input || final_item {
                    warn!(
                        input_name,
                        scene = scene_name,
                        final_input,
                        final_item,
                        "ndi-recovery: hard-remove — input STILL PRESENT after stop-receiver + RemoveInput + RemoveSceneItem fallback (operator garbage; the replacement under the correct name is serving)"
                    );
                } else {
                    info!(
                        input_name,
                        "ndi-recovery: hard-remove — input gone (read back)"
                    );
                }
            }
        }
    }
}

/// True if `input_name` is still in OBS's `ndi_source` input list. `None` on a
/// read failure (cannot prove either way — the caller treats it as "gone" so a
/// transient read error never fires the still-present WARN).
async fn input_still_listed(
    write: &SharedWrite,
    dispatcher: &Dispatcher,
    input_name: &str,
) -> Option<bool> {
    fetch_ndi_input_names(write, dispatcher)
        .await
        .map(|names| names.iter().any(|n| n == input_name))
}

/// True if a scene item in `scene_name` still references `input_name`. An empty
/// `scene_name` (the input is not a known scene item) is trivially `Some(false)`.
/// `None` on a read failure.
async fn scene_item_still_listed(
    write: &SharedWrite,
    dispatcher: &Dispatcher,
    scene_name: &str,
    input_name: &str,
) -> Option<bool> {
    if scene_name.is_empty() {
        return Some(false);
    }
    let resp = send(
        write,
        dispatcher,
        get_scene_items_request(&new_id(), scene_name),
    )
    .await?;
    let items = resp["d"]["responseData"]["sceneItems"].as_array()?;
    Some(
        items
            .iter()
            .any(|it| it["sourceName"].as_str() == Some(input_name)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removal_plan_stops_the_receiver_before_removing() {
        use RemovalStep::*;
        let plan = removal_plan();
        // The whole sub-plan must be exactly the stop-then-remove-then-verify order.
        assert_eq!(
            plan,
            [
                StopReceiver,
                RemoveInput,
                VerifyGone,
                RemoveSceneItemFallback,
                VerifyGoneFinal,
            ],
            "the hard-remove sub-plan must stop the receiver, remove, verify, fall back to RemoveSceneItem, then verify again",
        );
        let pos = |s: RemovalStep| plan.iter().position(|&p| p == s).expect("step present");
        // The load-bearing invariant (box round 4): a bare RemoveInput while the
        // receiver is live is a no-op, so the receiver is stopped FIRST.
        assert!(
            pos(StopReceiver) < pos(RemoveInput),
            "must stop the DistroAV receiver (clear ndi_source_name) BEFORE RemoveInput",
        );
        // Every removal is read back, and the scene-item fallback runs on the
        // read-back result before the final verify.
        assert!(
            pos(RemoveInput) < pos(VerifyGone),
            "read back after RemoveInput"
        );
        assert!(
            pos(VerifyGone) < pos(RemoveSceneItemFallback),
            "the RemoveSceneItem fallback runs on the read-back result",
        );
        assert!(
            pos(RemoveSceneItemFallback) < pos(VerifyGoneFinal),
            "verify gone once more AFTER the scene-item fallback",
        );
    }

    #[test]
    fn both_remove_sites_use_the_hard_remove_path() {
        // Both rung-2 removal call-sites must stop the receiver then read back — a
        // bare RemoveInput is ineffective against a live DistroAV receiver (round 4).
        assert!(
            uses_hard_remove_path(RemovalSite::RecreateRemoveRenamedOld),
            "RemoveRenamedOld must hard-remove, or the recreate leaves a duplicate receiver",
        );
        assert!(
            uses_hard_remove_path(RemovalSite::StaleRecoverSweep),
            "the stale-recover sweep must hard-remove too, or orphans accrue one per attempt",
        );
    }
}
