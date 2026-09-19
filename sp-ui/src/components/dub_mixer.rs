//! Dub-video adapter for the modern mixer (#181 D2). Binds one dub video's blend
//! to `PATCH /api/v1/videos/{id}/dub-mix {ratio}` through the shared `Mixer`.
//!
//! The live control is the **dabing** fader (= the mix ratio `r`); the
//! **originál hlas** fader is a read-only display of the resulting bed (`1−r`,
//! floored to `DUB_ORIGINAL_FLOOR` when the video has no stems — honest about the
//! 2-stream mix), and **ambient** is a fixed reference. Three presets set `r`
//! directly: Len dabing (1.0) / 50 : 50 / Originál (0.0). Until the dub is
//! generated (`dub_status != "ready"`) the mixer is locked with the reason.

use leptos::prelude::*;
use sp_core::mixer_model::{
    MixerKind, dub_channel_labels, faders_to_ratio, gains_for_preset, preset_for_gains, presets,
    ratio_to_faders,
};

use crate::api;
use crate::components::mixer::{Mixer, PresetSpec};
use crate::components::mixer_channel::ChannelSpec;

/// PATCH the current ratio to the server (live for the playing video; persisted
/// otherwise). Signals are `Copy`, so each callback captures them independently.
fn patch_ratio(video_id: i64, r: RwSignal<f32>, status: RwSignal<String>) {
    let ratio = r.get_untracked() as f64;
    leptos::task::spawn_local(async move {
        match api::patch_dub_mix(video_id, ratio).await {
            Ok(()) => status.set("Uložené".into()),
            Err(e) => status.set(format!("Chyba: {e}")),
        }
    });
}

#[component]
pub fn DubMixer(
    video_id: i64,
    title: String,
    dub_status: String,
    dub_mix_ratio: f64,
    stem_status: Option<String>,
) -> impl IntoView {
    let ready = dub_status == "ready";
    let has_stems = stem_status.as_deref() == Some("done");
    // Seed the ratio from the row; default 1.0 (dub only) when absent/invalid.
    let init_r = if dub_mix_ratio.is_finite() {
        dub_mix_ratio.clamp(0.0, 1.0) as f32
    } else {
        1.0
    };
    let r = RwSignal::new(init_r);
    let orig = RwSignal::new(ratio_to_faders(init_r, has_stems)[0]);
    let ambient = RwSignal::new(1.0_f32);
    let status = RwSignal::new(String::new());

    // Keep the read-only original-bed display in sync with the live ratio.
    Effect::new(move |_| {
        orig.set(ratio_to_faders(r.get(), has_stems)[0]);
    });

    // #182: with stems the full 3-fader strip; without stems the ambient bed does
    // not exist (the 2-stream DubOverOriginal mix), so only originál + dabing show.
    let labels = dub_channel_labels(has_stems);
    let ready_sig = Signal::derive(move || ready);
    let orig_note = if has_stems { "podklad" } else { "podklad (min.)" };
    let mut channels = vec![
        // originál (hlas) — read-only display of the resulting bed level.
        ChannelSpec {
            label: labels[0].to_string(),
            gain: orig,
            enabled: Signal::derive(|| false),
            fixed_note: Some(orig_note.to_string()),
            on_change: Callback::new(|_v: f32| {}),
            testid: None,
        },
        // dabing — the live blend fader (the mix ratio).
        ChannelSpec {
            label: labels[1].to_string(),
            gain: r,
            enabled: ready_sig,
            fixed_note: None,
            on_change: Callback::new(move |_v: f32| patch_ratio(video_id, r, status)),
            testid: Some("dub-mix-fader".to_string()),
        },
    ];
    if has_stems {
        // ambient — fixed reference (1.0), meaningful only once stems exist.
        channels.push(ChannelSpec {
            label: labels[2].to_string(),
            gain: ambient,
            enabled: Signal::derive(|| false),
            fixed_note: Some("pevné".to_string()),
            on_change: Callback::new(|_v: f32| {}),
            testid: None,
        });
    }

    let preset_specs: Vec<PresetSpec> = presets(MixerKind::Dub)
        .iter()
        .map(|p| {
            let id = p.id.to_string();
            let pid = id.clone();
            PresetSpec {
                id,
                label: p.label.to_string(),
                on_select: Callback::new(move |_| {
                    // The preset's ratio via the unified model: its fader set's
                    // `dabing` channel (index 1) is the ratio.
                    r.set(faders_to_ratio(&gains_for_preset(MixerKind::Dub, &pid, 0.0)));
                    patch_ratio(video_id, r, status);
                }),
            }
        })
        .collect();

    // The active preset via the unified model: match the current fader set back to
    // a preset id (uses the same round-trip the pure model is tested on).
    let active_preset = Signal::derive(move || {
        preset_for_gains(MixerKind::Dub, &ratio_to_faders(r.get(), has_stems)).map(str::to_string)
    });
    let state_line = Signal::derive(move || {
        if ready {
            "Dabing pripravený — nastav pomer dabing / originál".to_string()
        } else {
            "Dabing ešte nie je vygenerovaný".to_string()
        }
    });
    let disabled_reason = Signal::derive(move || {
        if ready {
            None
        } else {
            Some("dabing ešte nie je vygenerovaný".to_string())
        }
    });
    let title_sig = Signal::derive(move || format!("Mix dabingu — {title}"));

    view! {
        <Mixer
            title=title_sig
            state_line=state_line
            state_testid="dub-mixer-state"
            disabled_reason=disabled_reason
            channels=channels
            presets=preset_specs
            active_preset=active_preset
            presets_testid="dub-mixer-presets"
            extra_class="mixer-dub"
        >
            <span class="mixer-status">{move || status.get()}</span>
        </Mixer>
    }
}
