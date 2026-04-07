//! OBS connection status indicator.

use leptos::prelude::*;

use crate::store::DashboardStore;

#[component]
pub fn ObsStatus() -> impl IntoView {
    let store = use_context::<DashboardStore>().expect("DashboardStore in context");

    view! {
        <div class="obs-status">
            <span class="status-dot" class:connected=move || store.obs_connected.get()></span>
            <span>
                {move || {
                    if store.obs_connected.get() {
                        let scene = store
                            .obs_scene
                            .get()
                            .unwrap_or_else(|| "—".into());
                        format!("OBS: {scene}")
                    } else {
                        "OBS: Disconnected".into()
                    }
                }}
            </span>
        </div>
    }
}
