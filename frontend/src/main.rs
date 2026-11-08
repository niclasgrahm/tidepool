//! The Tidepool web UI: a client-side-rendered Leptos app.
//!
//! Dashboards kept live over the WebSocket (`/v1/ws`), the model browser,
//! and a status page. Protocol types are `tidepool-protocol`'s own. The
//! logic that needs no DOM lives in [`board`], [`calendar`], [`charts`],
//! [`commands`], [`scale`] and [`route`],
//! where `cargo test` reaches it.

mod api;
mod board;
mod calendar;
mod charts;
mod commands;
mod configui;
mod dashboard;
mod datepick;
mod design;
mod designer;
mod diagram;
mod highlight;
mod layout;
mod live;
mod pages;
mod palette;
mod plot;
mod relationships;
mod route;
mod scale;
mod textdiff;

use leptos::prelude::*;
use serde_json::json;
use tidepool_protocol::{ServerInfo, StatusInfo};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::Closure;

use crate::board::{InFlight, ago, chip_label, compact, data_summary, display, millis};
use crate::live::{Live, now};
use crate::route::{ConfigTab, Route, Tab};

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
    // a `?token=` becomes a session first; the socket then rides the cookie
    leptos::task::spawn_local(async move {
        api::adopt_token().await;
        live.connect();
    });

    let route = RwSignal::new(Route::parse(&current_hash()));
    if let Some(window) = web_sys::window() {
        let on_hash = Closure::<dyn Fn()>::new(move || route.set(Route::parse(&current_hash())));
        let _ =
            window.add_event_listener_with_callback("hashchange", on_hash.as_ref().unchecked_ref());
        on_hash.forget(); // lives as long as the page
    }
    let on_dashboard = move || route.with(Route::shows_a_board);
    let on_view = move || matches!(route.get(), Route::View(_));
    let slug = move || match route.get() {
        Route::Dashboard(slug) | Route::View(slug) => slug,
        _ => String::new(),
    };
    let dashboard_name = move || {
        let slug = slug();
        live.board_info(&slug).map_or(slug, |d| d.name)
    };

    view! {
        <header class="strip">
            <a class="brand" href="#/">
                <svg class="brand-mark" viewBox="0 0 20 20" aria-hidden="true">
                    <rect x="1" y="1" width="18" height="18" rx="5" />
                    <path d="M4 12c2-2 4-2 6 0s4 2 6 0" />
                    <path class="faint" d="M4 8c2-2 4-2 6 0s4 2 6 0" />
                </svg>
                "tidepool"
            </a>
            <nav class="main-nav" aria-label="Sections">
                <a href=Route::Home.href() class:current=move || matches!(route.get(), Route::Home | Route::Dashboard(_) | Route::View(_) | Route::Edit(_) | Route::NewDashboard)>"Dashboards"</a>
                <a href=Route::Model(Tab::Tables).href() class:current=move || matches!(route.get(), Route::Model(_) | Route::Table(_))>"Model"</a>
                <a href=Route::Config(ConfigTab::Edit).href() class:current=move || matches!(route.get(), Route::Config(_))>"Config"</a>
                <a href=Route::Status.href() class:current=move || route.get() == Route::Status>"Status"</a>
            </nav>
            <span class="grow"></span>
            <palette::Palette />
            <Pulse on_dashboard=Signal::derive(on_dashboard) />
        </header>
        <Show when=on_dashboard>
            <div class="toolbar">
                <div class="crumbs">
                    <a href=Route::Home.href()>"Dashboards"</a>
                    <span class="crumb-sep" aria-hidden="true">"/"</span>
                    <h1>{dashboard_name}</h1>
                    <code class="tag">{move || if on_view() { "view".to_string() } else { format!("{}.yaml", slug()) }}</code>
                </div>
                <span class="divider" aria-hidden="true"></span>
                <Selection />
                {move || live.in_flight.with(InFlight::answered).map(|ms| view! {
                    <span class="answered" title="From sending the selection to the last visual's answer">
                        "answered in "<code>{millis(ms)}</code>
                    </span>
                })}
                <Show
                    when=on_view
                    fallback=move || view! {
                        <a class="button edit-link" href=move || Route::Edit(slug()).href()>
                            <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M11 2.5l2.5 2.5L6 12.5H3.5V10z" /></svg>
                            "Edit"
                        </a>
                    }
                >
                    <button
                        class="button save-view"
                        title="Write this view to the config as a dashboard file"
                        on:click=move |_| dashboard::save_view(slug(), &dashboard_name())
                    >
                        "Save as dashboard"
                    </button>
                </Show>
            </div>
        </Show>
        {move || live.problem.get().map(|p| view! {
            <p class="problem banner" role="alert">
                {p}" "<button on:click=move |_| live.problem.set(None)>"Dismiss"</button>
            </p>
        })}
        {move || match route.get() {
            Route::Home => view! { <pages::Home /> }.into_any(),
            Route::Dashboard(slug) | Route::View(slug) => view! { <dashboard::Dashboard slug=slug /> }.into_any(),
            Route::Edit(slug) => view! { <designer::Designer slug=Some(slug) /> }.into_any(),
            Route::NewDashboard => view! { <designer::Designer slug=None /> }.into_any(),
            Route::Model(tab) => view! { <pages::Model tab=tab /> }.into_any(),
            Route::Table(name) => view! { <pages::TablePage name=name /> }.into_any(),
            Route::Status => view! { <pages::Status /> }.into_any(),
            Route::Config(tab) => view! { <configui::Config tab=tab /> }.into_any(),
        }}
        <StatusBar />
    }
}

/// The live indicator: connected, paused or not, the epoch on screen, and
/// on a dashboard, pause.
#[component]
fn Pulse(#[prop(into)] on_dashboard: Signal<bool>) -> impl IntoView {
    let live = expect_context::<Live>();
    let waiting = move || {
        let behind = live
            .server
            .get()
            .0
            .saturating_sub(live.board.with(|b| b.epoch));
        (live.paused.get() && behind > 0).then_some(behind)
    };
    let state = move || {
        if !live.connected.get() {
            "Connecting…"
        } else if live.paused.get() {
            "Paused"
        } else {
            "Live"
        }
    };
    let epoch = move || {
        let shown = if on_dashboard.get() && live.updated.get() > 0.0 {
            live.board.with(|b| b.epoch)
        } else {
            live.server.get().0
        };
        format!("epoch {}", display(&json!(shown)))
    };
    view! {
        <div class="pulse" class:offline=move || !live.connected.get() class:paused=move || live.paused.get()>
            <span class="pill">
                <span class="dot" aria-hidden="true"></span>
                {state}
                <code>{epoch}</code>
            </span>
            {move || waiting().map(|n| view! { <span class="badge">{format!("{n} waiting")}</span> })}
            <Show when=move || on_dashboard.get()>
                <button class="icon-button" on:click=move |_| live.toggle_pause()
                    aria-pressed=move || live.paused.get().to_string()
                    aria-label=move || if live.paused.get() { "Resume" } else { "Pause" }
                    title=move || if live.paused.get() { "Resume updates" } else { "Pause updates" }>
                    {move || if live.paused.get() {
                        view! { <svg viewBox="0 0 12 12" aria-hidden="true"><path class="solid" d="M3 1.5v9l7-4.5z" /></svg> }.into_any()
                    } else {
                        view! { <svg viewBox="0 0 12 12" aria-hidden="true"><rect class="solid" x="2" y="1.5" width="2.5" height="9" rx="0.5" /><rect class="solid" x="7.5" y="1.5" width="2.5" height="9" rx="0.5" /></svg> }.into_any()
                    }}
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
                let what = live.board.with(|b| chip_label(&s, b.filters.get(&s.field)));
                let field = s.field.clone();
                view! {
                    <li class="chip">
                        <span class="chip-field">{s.field.clone()}</span>
                        <span class="chip-value">{what}</span>
                        <button
                            aria-label=format!("Clear {}", s.field)
                            on:click=move |_| live.send(&json!({"type": "clear", "field": field}))
                        ><svg viewBox="0 0 10 10" aria-hidden="true"><path d="M2 2l6 6M8 2 2 8" /></svg></button>
                    </li>
                }
            })
            .collect_view()
    };
    view! {
        <nav class="selection" aria-label="Selection">
            <span class="history">
                <button class="icon-button" title="Back" disabled=move || !live.history.get().0 on:click=move |_| live.send(&json!({"type": "back"}))>
                    <svg viewBox="0 0 12 12" aria-hidden="true"><path d="M7.5 2.5 4 6l3.5 3.5" /></svg><span class="sr">"Back"</span>
                </button>
                <button class="icon-button" title="Forward" disabled=move || !live.history.get().1 on:click=move |_| live.send(&json!({"type": "forward"}))>
                    <svg viewBox="0 0 12 12" aria-hidden="true"><path d="M4.5 2.5 8 6 4.5 9.5" /></svg><span class="sr">"Forward"</span>
                </button>
            </span>
            <ul class="chips">{chips}</ul>
            {move || live.selection.with(Vec::is_empty).then(|| view! {
                <span class="quiet hint">"Click a value or a bar to select it; Ctrl/⌘-click adds to the selection."</span>
            })}
            <button
                class="ghost-button"
                disabled=move || live.selection.with(Vec::is_empty)
                on:click=move |_| live.send(&json!({"type": "clear"}))
            >"Clear all"</button>
            // a slow selection says so (shown after a moment: see style.css)
            <span class="updating" role="status">
                {move || live.busy().then(|| view! { <span class="spinner" aria-hidden="true"></span>"Updating…" })}
            </span>
        </nav>
    }
}

/// The bar along the bottom: the connection, the snapshot on screen and
/// how old it is, what the server holds, ingest, sessions, the model and
/// the server's version. Data counts come from `/v1/status`, every 5 s.
#[component]
fn StatusBar() -> impl IntoView {
    let live = expect_context::<Live>();
    let info = LocalResource::new(|| api::get_json::<ServerInfo>("/v1/info"));
    let status = RwSignal::new(None::<StatusInfo>);
    let poll = move || {
        leptos::task::spawn_local(async move {
            // a page without read access just shows less
            status.set(api::get_json::<StatusInfo>("/v1/status").await.ok());
        });
    };
    poll();
    if let Some(window) = web_sys::window() {
        let tick = Closure::<dyn Fn()>::new(poll);
        let _ = window.set_interval_with_callback_and_timeout_and_arguments_0(
            tick.as_ref().unchecked_ref(),
            5000,
        );
        tick.forget(); // lives as long as the page
    }
    let age = move || {
        let updated = live.updated.get();
        (updated > 0.0).then(|| ago(live.clock.get().max(now()) - updated))
    };
    view! {
        <footer class="status-bar">
            <span class="conn" class:off=move || !live.connected.get()>
                <span class="dot" aria-hidden="true"></span>
                {move || if live.connected.get() { "ws connected" } else { "ws disconnected" }}
            </span>
            <span>
                {move || format!("snapshot {}", display(&json!(live.server.get().0)))}
                {move || age().map(|a| format!(" · updated {a}"))}
            </span>
            {move || status.get().map(|s| view! {
                <span>{data_summary(&s.tables)}</span>
                <span>{format!("{} sessions", s.sessions)}</span>
                <span>{format!("publish {} · refresh {}", millis(s.epoch_publish_ms), millis(s.live_refresh_ms))}</span>
            })}
            <span>{move || format!("ingest {} rows/s", compact(live.server.get().1))}</span>
            <span class="grow"></span>
            {move || status.get().map(|s| view! { <span>{format!("model v{}", s.config_version)}</span> })}
            <Suspense fallback=|| view! { <span>"…"</span> }>
                {move || info.get().map(|result| match result {
                    Ok(info) => view! { <span>{describe(&info)}</span> }.into_any(),
                    Err(err) => view! { <span class="error">{format!("server unreachable: {err}")}</span> }.into_any(),
                })}
            </Suspense>
        </footer>
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
