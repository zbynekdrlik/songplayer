//! #278: the playlist EQ's curve — an SVG in an 800×300 box that takes the
//! card's width (iemmixer's `eq_modal.rs` model).
//!
//! A log frequency grid 20 Hz … 20 kHz and a dB grid with the 0 dB line;
//! the summed curve (volume + every enabled band) with a soft fill to 0 dB;
//! each enabled band's own curve in its type's colour; one handle per band
//! at (frequency, gain). A handle is dragged with the pointer (captured, so
//! the drag survives leaving the circle), by the pointer's travel from
//! where it took hold (`grab_offset`): a shelf or a peak on both axes, a
//! high-/low-pass on its frequency only (its handle sits on the 0 dB line).
//! The wheel over a handle changes its Q. Every number comes from
//! `sp_core::audio_fx` / `audio_fx_view` — the server's own formulas.

use leptos::prelude::*;
use leptos::svg::Svg;
use sp_core::audio_fx::{curve_path, db_y, freq_x};
use sp_core::audio_fx_view::{
    BOX_H, BOX_W, CURVE_POINTS, DB_GRID, FREQ_GRID, WheelTurn, band_path, dragged_from, fill_path,
    grab_offset, handle_xy, to_box,
};
use wasm_bindgen::JsCast;

use crate::components::playlist_audio::EqEdit;

/// A handle's radius and how far above it its number sits (box units).
const HANDLE_R: f64 = 9.0;
/// The handle's invisible grip around its dot (box units): a phone draws
/// the 800-unit box ~340 px wide, where the dot alone is ~4 px.
const GRIP_R: f64 = 22.0;
const NUMBER_ABOVE: f64 = 14.0;
/// Where the grid's labels sit, off the box's edges.
const LABEL_EDGE: f64 = 14.0;

/// The grid: a line per marked frequency and level, the 0 dB line brighter.
fn grid() -> impl IntoView {
    let freqs = FREQ_GRID
        .iter()
        .map(|(freq, label)| {
            let x = freq_x(*freq, BOX_W);
            let label_x = x.clamp(LABEL_EDGE, BOX_W - LABEL_EDGE);
            view! {
                <line class="eq-grid" x1=x y1=0.0 x2=x y2=BOX_H />
                <text class="eq-grid-label" x=label_x y={BOX_H - 6.0} text-anchor="middle">
                    {*label}
                </text>
            }
        })
        .collect_view();
    let levels = DB_GRID
        .iter()
        .map(|(level, label)| {
            let y = db_y(*level, BOX_H);
            let class = if *level == 0.0 { "eq-zero" } else { "eq-grid" };
            view! {
                <line class=class x1=0.0 y1=y x2=BOX_W y2=y />
                <text class="eq-grid-label" x=4.0 y={y - 3.0}>
                    {*label}
                </text>
            }
        })
        .collect_view();
    view! {
        {freqs}
        {levels}
    }
}

/// One band's own curve (none while it is off).
fn band_curve(edit: EqEdit, id: u64) -> impl IntoView {
    let band = Memo::new(move |_| edit.band(id));
    view! {
        <path
            class=move || {
                let kind = band.get().map(|b| b.kind.name()).unwrap_or("peak");
                format!("eq-band-curve kind-{kind}")
            }
            data-testid="playlist-audio-band-curve"
            data-index=move || edit.index_of(id)
            d=move || {
                band.get().map(|b| band_path(&b, BOX_W, BOX_H, CURVE_POINTS)).unwrap_or_default()
            }
        />
    }
}

/// One band's handle: dragged with the pointer, its Q on the wheel.
fn handle(
    edit: EqEdit,
    id: u64,
    svg: NodeRef<Svg>,
    dragging: RwSignal<Option<u64>>,
) -> impl IntoView {
    let band = Memo::new(move |_| edit.band(id));
    let at = move || band.get().map(|b| handle_xy(&b, BOX_W, BOX_H));
    // The wheel's turn over this handle, carried between events (a
    // touchpad's small events add up).
    let turn = StoredValue::new(WheelTurn::default());
    // Where the drag took hold, off the handle's centre (box units).
    let grab = StoredValue::new((0.0_f64, 0.0_f64));
    // The pointer in the box's units, measured on the SVG as drawn now.
    let pointer = move |ev: &web_sys::PointerEvent| -> Option<(f64, f64)> {
        let rect = svg.get_untracked()?.get_bounding_client_rect();
        let x = to_box(f64::from(ev.client_x()) - rect.left(), rect.width(), BOX_W);
        let y = to_box(f64::from(ev.client_y()) - rect.top(), rect.height(), BOX_H);
        Some((x, y))
    };
    // The drag ends once: the release, a cancel, or the capture lost.
    let end = move || {
        if dragging.get_untracked() == Some(id) {
            dragging.set(None);
            edit.finish();
        }
    };
    view! {
        <g
            class=move || {
                let (kind, on) = band
                    .get()
                    .map(|b| (b.kind.name(), b.enabled))
                    .unwrap_or(("peak", true));
                let dragged_now = if dragging.get() == Some(id) { " dragging" } else { "" };
                let off = if on { "" } else { " off" };
                format!("eq-handle kind-{kind}{off}{dragged_now}")
            }
            data-testid="playlist-audio-handle"
            data-index=move || edit.index_of(id)
            data-kind=move || { band.get().map(|b| b.kind.name()) }
        >
            <circle
                class="eq-handle-dot"
                r=HANDLE_R
                cx=move || { at().map(|p| p.0) }
                cy=move || { at().map(|p| p.1) }
            />
            <text
                class="eq-handle-number"
                x=move || { at().map(|p| p.0) }
                y=move || { at().map(|p| p.1 - NUMBER_ABOVE) }
            >
                {move || edit.number(id).map(|n| n.to_string()).unwrap_or_default()}
            </text>
            // The grip on top takes the pointer and the wheel.
            <circle
                class="eq-handle-grip"
                r=GRIP_R
                cx=move || { at().map(|p| p.0) }
                cy=move || { at().map(|p| p.1) }
                on:pointerdown=move |ev: web_sys::PointerEvent| {
                    if ev.button() != 0 {
                        return;
                    }
                    ev.prevent_default();
                    if let Some(target) = ev
                        .current_target()
                        .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
                    {
                        let _ = target.set_pointer_capture(ev.pointer_id());
                    }
                    if let (Some((x, y)), Some(b)) = (pointer(&ev), band.get_untracked()) {
                        grab.set_value(grab_offset(&b, x, y, BOX_W, BOX_H));
                    }
                    dragging.set(Some(id));
                }
                on:pointermove=move |ev: web_sys::PointerEvent| {
                    if dragging.get_untracked() != Some(id) {
                        return;
                    }
                    if let Some((x, y)) = pointer(&ev) {
                        let held = grab.get_value();
                        edit.edit_band(
                            id,
                            |b| *b = dragged_from(b, held, x, y, BOX_W, BOX_H),
                            false,
                        );
                    }
                }
                on:pointerup=move |_| end()
                on:pointercancel=move |_| end()
                on:lostpointercapture=move |_| end()
                on:wheel=move |ev: web_sys::WheelEvent| {
                    ev.prevent_default();
                    let delta_y = ev.delta_y();
                    edit.edit_band(
                        id,
                        |b| b.q = turn.try_update_value(|t| t.turn(b.q, delta_y)).unwrap_or(b.q),
                        false,
                    );
                }
            />
        </g>
    }
}

/// The whole curve.
pub(crate) fn eq_curve(edit: EqEdit) -> impl IntoView {
    let svg = NodeRef::<Svg>::new();
    let dragging = RwSignal::new(None::<u64>);
    let summed = Memo::new(move |_| edit.fx.with(|f| curve_path(f, BOX_W, BOX_H, CURVE_POINTS)));
    // The fill closes the summed curve: computed once per change.
    let fill = Memo::new(move |_| summed.with(|curve| fill_path(curve, BOX_W, BOX_H)));
    view! {
        <svg
            class="playlist-audio-curve"
            data-testid="playlist-audio-curve"
            node_ref=svg
            viewBox=format!("0 0 {BOX_W} {BOX_H}")
            preserveAspectRatio="none"
        >
            {grid()}
            <path class="eq-fill" data-testid="playlist-audio-fill" d=move || fill.get() />
            <For
                each=move || edit.ids.get()
                key=|id: &u64| *id
                children=move |id: u64| band_curve(edit, id)
            />
            <path class="eq-sum" data-testid="playlist-audio-path" d=move || summed.get() />
            <For
                each=move || edit.ids.get()
                key=|id: &u64| *id
                children=move |id: u64| handle(edit, id, svg, dragging)
            />
        </svg>
    }
}
