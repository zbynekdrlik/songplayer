//! #194 ROUND 2: the ONE status-chips renderer for every song row.
//!
//! It renders whatever `StatusChip`s the caller built from `sp_core::status_chip`
//! (the single label/tone vocabulary), so the Dashboard list, the Live set list +
//! catalog, the Lyrics list and the Dabing list all show the SAME Slovak chip,
//! same colour, same test id for a given state. The pure model decides the words
//! and colours; this component only paints them.

use leptos::prelude::*;
use sp_core::status_chip::StatusChip;

/// One chip plus an optional longer tooltip (e.g. the dabing chain detail, which
/// the Dabing row folds into the dub chip's `title`).
#[derive(Clone)]
pub struct ChipView {
    pub chip: StatusChip,
    pub tip: Option<String>,
}

impl ChipView {
    pub fn new(chip: StatusChip) -> Self {
        Self { chip, tip: None }
    }

    pub fn with_tip(chip: StatusChip, tip: impl Into<String>) -> Self {
        Self {
            chip,
            tip: Some(tip.into()),
        }
    }
}

/// Renders a row of status chips. The whole group carries `data-testid="status-chips"`
/// and each chip carries its kind's stable id (`chip-stems`/`chip-text`/`chip-dub`/`chip-file`).
#[component]
pub fn StatusChips(chips: Vec<ChipView>) -> impl IntoView {
    let rendered = chips
        .into_iter()
        .map(|cv| {
            let testid = cv.chip.kind.testid();
            let class = format!("status-chip {}", cv.chip.tone.css_class());
            let title = cv.tip.unwrap_or_else(|| cv.chip.label_sk.clone());
            let label = cv.chip.label_sk;
            view! {
                <span class=class data-testid=testid title=title>
                    {label}
                </span>
            }
        })
        .collect::<Vec<_>>();

    view! {
        <span class="status-chips" data-testid="status-chips">
            {rendered}
        </span>
    }
}
