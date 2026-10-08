//! The Tidepool web UI: a client-side-rendered Leptos app.
//!
//! Dashboards kept live over the WebSocket (`/v1/ws`), the model browser,
//! and a status page. Protocol types are `tidepool-protocol`'s own. The
//! logic that needs no DOM lives in [`board`], [`charts`] and [`route`],
//! where `cargo test` reaches it.

mod api;
mod board;
mod charts;
mod dashboard;
mod echarts;
mod live;
mod pages;
mod route;

use leptos::prelude::*;
use serde_json::json;
use tidepool_protocol::ServerInfo;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::Closure;

use crate::board::{ago, display, range_label};
use crate::live::{Live, now};
use crate::route::{Route, Tab};

/// The line the footer shows for a server.
fn describe(info: &ServerInfo) -> String {
    format!("{} {} · API v{}", info.name, info.version, info.api_version)
}

fn current_hash() -> String {
    web_sys::window()
        .and_then(|w| w.location().hash().ok())
        .unwrap_or_default()
}

#[component]
fn App() -> impl IntoView {
    let live = Live::new();
    provide_context(live);
    live.connect();

    let route = RwSignal::new(Route::parse(&current_hash()));
    if let Some(window) = web_sys::window() {
        let on_hash = Closure::<dyn Fn()>::new(move || route.set(Route::parse(&current_hash())));
        let _ =
            window.add_event_listener_with_callback("hashchange", on_hash.as_ref().unchecked_ref());
        on_hash.forget(); // lives as long as the page
    }
    let info = LocalResource::new(|| api::get_json::<ServerInfo>("/v1/info"));
    let on_dashboard = move || matches!(route.get(), Route::Dashboard(_));
    let dashboard_name = move || match route.get() {
        Route::Dashboard(slug) => live
            .dashboards
            .with(|d| d.iter().find(|d| d.id == slug).map(|d| d.name.clone()))
            .unwrap_or(slug),
        _ => String::new(),
    };

    view! {
        <header class="strip">
            <a class="brand" href="#/">"tidepool"</a>
            <nav class="main-nav" aria-label="Sections">
                <a href=Route::Home.href() class:current=move || matches!(route.get(), Route::Home | Route::Dashboard(_))>"Dashboards"</a>
                <a href=Route::Model(Tab::Tables).href() class:current=move || matches!(route.get(), Route::Model(_) | Route::Table(_))>"Model"</a>
                <a href=Route::Status.href() class:current=move || route.get() == Route::Status>"Status"</a>
            </nav>
            <Pulse on_dashboard=Signal::derive(on_dashboard) />
        </header>
        <Show when=on_dashboard>
            <div class="toolbar">
                <h1>{dashboard_name}</h1>
                <Selection />
            </div>
        </Show>
        {move || live.problem.get().map(|p| view! {
            <p class="problem banner" role="alert">
                {p}" "<button on:click=move |_| live.problem.set(None)>"Dismiss"</button>
            </p>
        })}
        {move || match route.get() {
            Route::Home => view! { <pages::Home /> }.into_any(),
            Route::Dashboard(slug) => view! { <dashboard::Dashboard slug=slug /> }.into_any(),
            Route::Model(tab) => view! { <pages::Model tab=tab /> }.into_any(),
            Route::Table(name) => view! { <pages::TablePage name=name /> }.into_any(),
            Route::Status => view! { <pages::Status /> }.into_any(),
        }}
        <footer>
            <Suspense fallback=|| view! { <span>"…"</span> }>
                {move || info.get().map(|result| match result {
                    Ok(info) => view! { <span>{describe(&info)}</span> }.into_any(),
                    Err(err) => view! { <span class="error">{format!("server unreachable: {err}")}</span> }.into_any(),
                })}
            </Suspense>
        </footer>
    }
}

/// The live indicator: how fresh the page is, the ingest rate, and pause.
#[component]
fn Pulse(#[prop(into)] on_dashboard: Signal<bool>) -> impl IntoView {
    let live = expect_context::<Live>();
    let waiting = move || {
        let behind = live
            .server_epoch
            .get()
            .saturating_sub(live.board.with(|b| b.epoch));
        (live.paused.get() && behind > 0).then_some(behind)
    };
    view! {
        <div class="pulse" class:offline=move || !live.connected.get() class:paused=move || live.paused.get()>
            <span class="dot" aria-hidden="true"></span>
            <span>
                {move || if !live.connected.get() {
                    "Connecting…".to_string()
                } else if !on_dashboard.get() || live.updated.get() == 0.0 {
                    // nothing on screen is updating: say what the server is doing
                    format!(
                        "Live · epoch {} · {:.0} rows/s",
                        display(&json!(live.server_epoch.get())),
                        live.rate.get()
                    )
                } else {
                    format!(
                        "Updated {} · epoch {} · {:.0} rows/s",
                        ago(live.clock.get().max(now()) - live.updated.get()),
                        display(&json!(live.board.with(|b| b.epoch))),
                        live.rate.get()
                    )
                }}
            </span>
            {move || waiting().map(|n| view! { <span class="badge">{format!("{n} updates waiting")}</span> })}
            <Show when=move || on_dashboard.get()>
                <button on:click=move |_| live.toggle_pause() aria-pressed=move || live.paused.get().to_string()>
                    {move || if live.paused.get() { "Resume" } else { "Pause" }}
                </button>
            </Show>
        </div>
    }
}

/// What's selected, with back, forward and clear.
#[component]
fn Selection() -> impl IntoView {
    let live = expect_context::<Live>();
    let chips = move || {
        live.selection
            .get()
            .into_iter()
            .map(|s| {
                let what = match (&s.values, &s.from, &s.to) {
                    (Some(values), _, _) => {
                        values.iter().map(display).collect::<Vec<_>>().join(", ")
                    }
                    (None, from, to) => range_label(from.as_ref(), to.as_ref()),
                };
                let field = s.field.clone();
                let short = s
                    .field
                    .split_once('.')
                    .map_or(s.field.clone(), |(_, f)| f.to_string());
                view! {
                    <li class="chip">
                        <span class="chip-field">{short}</span>
                        <span>{what}</span>
                        <button
                            aria-label=format!("Clear {}", s.field)
                            on:click=move |_| live.send(&json!({"type": "clear", "field": field}))
                        >"×"</button>
                    </li>
                }
            })
            .collect_view()
    };
    view! {
        <nav class="selection" aria-label="Selection">
            <button disabled=move || !live.can_back.get() on:click=move |_| live.send(&json!({"type": "back"}))>"Back"</button>
            <button disabled=move || !live.can_forward.get() on:click=move |_| live.send(&json!({"type": "forward"}))>"Forward"</button>
            <ul class="chips">{chips}</ul>
            {move || live.selection.with(Vec::is_empty).then(|| view! {
                <span class="quiet hint">"Click a value or a bar to select it; Ctrl/⌘-click adds to the selection."</span>
            })}
            <button
                disabled=move || live.selection.with(Vec::is_empty)
                on:click=move |_| live.send(&json!({"type": "clear"}))
            >"Clear all"</button>
        </nav>
    }
}

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(App);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_footer_names_server_version_and_api() {
        let info = ServerInfo {
            name: "tidepool".into(),
            version: "1.2.3".into(),
            api_version: 1,
        };
        assert_eq!(describe(&info), "tidepool 1.2.3 · API v1");
    }
}
