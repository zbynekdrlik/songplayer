//! CRUD list of Resolume Arena hosts.

use leptos::prelude::*;
use sp_core::models::ResolumeHost;

use crate::api;
use crate::store::DashboardStore;

#[derive(serde::Serialize)]
struct NewHost {
    name: String,
    ip: String,
    port: u16,
    enabled: bool,
}

#[component]
pub fn ResolumeHosts() -> impl IntoView {
    let store = use_context::<DashboardStore>().expect("DashboardStore in context");

    // Load hosts on mount.
    let _load = Effect::new(move |_| {
        leptos::task::spawn_local(async move {
            if let Ok(hosts) = api::get::<Vec<ResolumeHost>>("/api/v1/resolume/hosts").await {
                store.resolume_hosts.set(hosts);
            }
        });
    });

    let name_input = RwSignal::new(String::new());
    let ip_input = RwSignal::new(String::new());
    let port_input = RwSignal::new("7000".to_string());

    let on_add = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let name = name_input.get();
        let ip = ip_input.get();
        let port: u16 = port_input.get().parse().unwrap_or(7000);

        leptos::task::spawn_local(async move {
            let body = NewHost {
                name,
                ip,
                port,
                enabled: true,
            };
            if let Ok(host) =
                api::post_json::<NewHost, ResolumeHost>("/api/v1/resolume/hosts", &body).await
            {
                store.resolume_hosts.update(|hosts| hosts.push(host));
                name_input.set(String::new());
                ip_input.set(String::new());
                port_input.set("7000".into());
            }
        });
    };

    let on_delete = move |host_id: i64| {
        leptos::task::spawn_local(async move {
            let path = format!("/api/v1/resolume/hosts/{host_id}");
            if api::delete(&path).await.is_ok() {
                store
                    .resolume_hosts
                    .update(|hosts| hosts.retain(|h| h.id != host_id));
            }
        });
    };

    view! {
        <div class="resolume-hosts">
            <table>
                <thead>
                    <tr>
                        <th>"Name"</th>
                        <th>"IP"</th>
                        <th>"Port"</th>
                        <th>"Enabled"</th>
                        <th></th>
                    </tr>
                </thead>
                <tbody>
                    <For
                        each=move || store.resolume_hosts.get()
                        key=|h| h.id
                        children=move |host| {
                            let hid = host.id;
                            view! {
                                <tr>
                                    <td>{host.label.clone()}</td>
                                    <td>{host.host.clone()}</td>
                                    <td>{host.port}</td>
                                    <td>{if host.is_enabled { "Yes" } else { "No" }}</td>
                                    <td>
                                        <button class="delete-btn" on:click=move |_| on_delete(hid)>
                                            "Delete"
                                        </button>
                                    </td>
                                </tr>
                            }
                        }
                    />
                </tbody>
            </table>

            <form class="add-host-form" on:submit=on_add>
                <input
                    type="text"
                    placeholder="Name"
                    prop:value=move || name_input.get()
                    on:input=move |ev| name_input.set(event_target_value(&ev))
                />
                <input
                    type="text"
                    placeholder="IP address"
                    prop:value=move || ip_input.get()
                    on:input=move |ev| ip_input.set(event_target_value(&ev))
                />
                <input
                    type="number"
                    placeholder="Port"
                    prop:value=move || port_input.get()
                    on:input=move |ev| port_input.set(event_target_value(&ev))
                />
                <button type="submit">"Add Host"</button>
            </form>
        </div>
    }
}
