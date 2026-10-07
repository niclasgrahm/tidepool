//! The Tidepool web UI: a client-side-rendered Leptos app.
//!
//! One page so far (M4's slice of M6): a dashboard kept live over the
//! WebSocket (`/v1/ws`), with filter lists that show Qlik's states and
//! selections made by clicking values and bars. The protocol types are
//! `tidepool-protocol`'s own; [`board`] holds the logic that needs no DOM.

mod api;
mod board;

use leptos::prelude::*;
use serde_json::{Value, json};
use tidepool_protocol::ServerInfo;
use tidepool_protocol::live::{
    DashboardInfo, FilterFrame, SelectedField, ServerMessage, ValueState, VisualFrame, VisualInfo,
};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::Closure;

use crate::board::{Board, display, line_points, shares, socket_url};

/// The line the footer shows for a server.
fn describe(info: &ServerInfo) -> String {
    format!("{} {} · API v{}", info.name, info.version, info.api_version)
}

/// Everything the page shows, updated by server messages.
#[derive(Clone, Copy)]
struct Live {
    dashboards: RwSignal<Vec<DashboardInfo>>,
    open: RwSignal<Option<String>>,
    board: RwSignal<Board>,
    selection: RwSignal<Vec<SelectedField>>,
    can_back: RwSignal<bool>,
    can_forward: RwSignal<bool>,
    rate: RwSignal<f64>,
    connected: RwSignal<bool>,
    problem: RwSignal<Option<String>>,
    socket: StoredValue<Option<web_sys::WebSocket>, LocalStorage>,
}

impl Live {
    fn send(self, message: &Value) {
        self.socket.with_value(|socket| {
            if let Some(socket) = socket {
                let _ = socket.send_with_str(&message.to_string());
            }
        });
    }

    fn receive(self, message: ServerMessage) {
        match message {
            ServerMessage::Welcome { .. } => self.connected.set(true),
            ServerMessage::Model { dashboards } => {
                if self.open.get_untracked().is_none()
                    && let Some(first) = dashboards.first()
                {
                    self.open.set(Some(first.id.clone()));
                    self.send(&json!({"type": "open_dashboard", "id": first.id}));
                }
                self.dashboards.set(dashboards);
            }
            ServerMessage::Selection {
                fields,
                can_back,
                can_forward,
            } => {
                self.selection.set(fields);
                self.can_back.set(can_back);
                self.can_forward.set(can_forward);
            }
            message @ ServerMessage::Frame { .. } => {
                self.board.update(|board| {
                    board.apply(&message);
                });
            }
            ServerMessage::Status {
                rows_per_second, ..
            } => self.rate.set(rows_per_second),
            ServerMessage::Error { message } => self.problem.set(Some(message)),
        }
    }

    /// Open the socket to the server this page came from.
    fn connect(self) {
        let Some(window) = web_sys::window() else {
            return;
        };
        let location = window.location();
        let url = socket_url(
            &location.protocol().unwrap_or_default(),
            &location.host().unwrap_or_default(),
            &location.search().unwrap_or_default(),
        );
        let Ok(socket) = web_sys::WebSocket::new(&url) else {
            self.problem.set(Some(format!("can't open {url}")));
            return;
        };
        let on_open = Closure::<dyn Fn()>::new(move || self.send(&json!({"type": "hello"})));
        socket.set_onopen(Some(on_open.as_ref().unchecked_ref()));
        on_open.forget();
        let on_message =
            Closure::<dyn Fn(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
                let Some(text) = event.data().as_string() else {
                    return;
                };
                match serde_json::from_str::<ServerMessage>(&text) {
                    Ok(message) => self.receive(message),
                    Err(err) => self.problem.set(Some(format!("unreadable message: {err}"))),
                }
            });
        socket.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        on_message.forget();
        let on_close = Closure::<dyn Fn()>::new(move || {
            self.connected.set(false);
            self.problem.set(Some(
                "Disconnected from the server. Reload to reconnect.".into(),
            ));
        });
        socket.set_onclose(Some(on_close.as_ref().unchecked_ref()));
        on_close.forget();
        self.socket.set_value(Some(socket));
    }
}

#[component]
fn App() -> impl IntoView {
    let live = Live {
        dashboards: RwSignal::new(Vec::new()),
        open: RwSignal::new(None),
        board: RwSignal::new(Board::default()),
        selection: RwSignal::new(Vec::new()),
        can_back: RwSignal::new(false),
        can_forward: RwSignal::new(false),
        rate: RwSignal::new(0.0),
        connected: RwSignal::new(false),
        problem: RwSignal::new(None),
        socket: StoredValue::new_local(None),
    };
    live.connect();
    let info = LocalResource::new(|| api::get_json::<ServerInfo>("/v1/info"));
    let dashboard = move || {
        let open = live.open.get()?;
        live.dashboards
            .with(|d| d.iter().find(|d| d.id == open).cloned())
    };

    view! {
        <header class="strip">
            <h1>{move || dashboard().map_or_else(|| "tidepool".to_string(), |d| d.name)}</h1>
            <Selection live=live />
            <p class="pulse" class:offline=move || !live.connected.get()>
                {move || if live.connected.get() {
                    format!("Live · epoch {} · {:.0} rows/s", live.board.get().epoch, live.rate.get())
                } else {
                    "Connecting…".to_string()
                }}
            </p>
        </header>
        {move || live.problem.get().map(|p| view! { <p class="problem">{p}</p> })}
        <div class="page">
            <aside class="rail">
                {move || dashboard().map(|d| d.filters.into_iter().map(|f| view! {
                    <FilterList live=live field=f.field />
                }).collect_view())}
            </aside>
            <main
                class="grid"
                style=move || dashboard().map_or_else(String::new, |d| format!(
                    "grid-template-columns: repeat({}, 1fr); grid-auto-rows: {}px;", d.columns, d.row_height
                ))
            >
                {move || dashboard().map(|d| d.visuals.into_iter().map(|v| view! {
                    <Visual live=live info=v />
                }).collect_view())}
            </main>
        </div>
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

/// What's selected, with back, forward and clear.
#[component]
fn Selection(live: Live) -> impl IntoView {
    let chips =
        move || {
            live.selection.get().into_iter().map(|s| {
            let what = match (&s.values, &s.from, &s.to) {
                (Some(values), _, _) => values.iter().map(display).collect::<Vec<_>>().join(", "),
                (None, from, to) => format!(
                    "{} to {}",
                    from.as_ref().map_or_else(|| "…".into(), display),
                    to.as_ref().map_or_else(|| "…".into(), display)
                ),
            };
            let field = s.field.clone();
            view! {
                <li class="chip">
                    <span class="chip-field">{s.field.clone()}</span>
                    <span>{what}</span>
                    <button
                        aria-label=format!("Clear {}", s.field)
                        on:click=move |_| live.send(&json!({"type": "clear", "field": field}))
                    >"×"</button>
                </li>
            }
        }).collect_view()
        };
    view! {
        <nav class="selection" aria-label="Selection">
            <button disabled=move || !live.can_back.get() on:click=move |_| live.send(&json!({"type": "back"}))>"Back"</button>
            <button disabled=move || !live.can_forward.get() on:click=move |_| live.send(&json!({"type": "forward"}))>"Forward"</button>
            <ul class="chips">{chips}</ul>
            <button
                disabled=move || live.selection.with(Vec::is_empty)
                on:click=move |_| live.send(&json!({"type": "clear"}))
            >"Clear all"</button>
        </nav>
    }
}

/// A filter list: every value with its state; click toggles it.
#[component]
fn FilterList(live: Live, field: String) -> impl IntoView {
    let key = field.clone();
    // only this list's frame: other frames mustn't re-render (and so
    // swallow clicks on) its values
    let frame = Memo::new(move |_| live.board.with(|b| b.filters.get(&key).cloned()));
    let search_field = field.clone();
    let on_search = move |event| {
        live.send(&json!({
            "type": "filter_window", "field": search_field, "search": event_target_value(&event),
        }));
    };
    let title = field
        .split_once('.')
        .map_or(field.clone(), |(_, f)| f.to_string());
    view! {
        <section class="filter">
            <h2>{title}</h2>
            <input type="search" placeholder="Search" aria-label=format!("Search {field}") on:input=on_search />
            <ul>
                {move || frame.get().map(|f: FilterFrame| f.values.into_iter().map(|v| {
                    let state = match v.state {
                        ValueState::Selected => "selected",
                        ValueState::Possible => "possible",
                        ValueState::Excluded => "excluded",
                    };
                    let (field, value) = (field.clone(), v.value.clone());
                    view! {
                        <li>
                            <button class=format!("value {state}") on:click=move |_| live.send(&json!({
                                "type": "select", "field": field, "values": [value], "mode": "toggle",
                            }))>
                                <span>{display(&v.value)}</span>
                                <span class="rows">{if v.rows > 0 { display(&json!(v.rows)) } else { String::new() }}</span>
                            </button>
                        </li>
                    }
                }).collect_view())}
            </ul>
        </section>
    }
}

/// One visual, placed on the dashboard grid.
#[component]
fn Visual(live: Live, info: VisualInfo) -> impl IntoView {
    let id = info.id.clone();
    let frame = Memo::new(move |_| live.board.with(|b| b.visuals.get(&id).cloned()));
    let place = format!(
        "grid-column: {} / span {}; grid-row: {} / span {};",
        info.at.x + 1,
        info.at.w,
        info.at.y + 1,
        info.at.h
    );
    let title = info.metrics.join(", ")
        + &info.dimensions.first().map_or_else(String::new, |d| {
            format!(
                " by {}",
                d.field.split_once('.').map_or(d.field.as_str(), |(_, f)| f)
            )
        });
    let class = format!("visual {}", info.visual_type);
    view! {
        <article class=class style=place>
            <h2>{title}</h2>
            {move || match frame.get() {
                None => view! { <p class="quiet">"Waiting for data"</p> }.into_any(),
                Some(VisualFrame { error: Some(error), .. }) => view! { <p class="problem">{error}</p> }.into_any(),
                Some(VisualFrame { result: Some(result), .. }) => body(live, &info, result.rows),
                Some(_) => view! { <p class="quiet">"No result"</p> }.into_any(),
            }}
        </article>
    }
}

/// A visual's content by type.
fn body(live: Live, info: &VisualInfo, rows: Vec<Vec<Value>>) -> AnyView {
    let dims = info.dimensions.len();
    match info.visual_type.as_str() {
        "kpi" => {
            let value = rows.first().and_then(|r| r.get(dims)).cloned().unwrap_or(Value::Null);
            view! { <p class="kpi-value">{display(&value)}</p> }.into_any()
        }
        "bar" if dims == 1 => {
            let field = info.dimensions[0].field.clone();
            let clickable = info.on_click;
            let shares = shares(&rows, 1);
            view! {
                <ul class="bars">
                    {rows.into_iter().zip(shares).map(|(row, share)| {
                        let (field, value) = (field.clone(), row[0].clone());
                        view! {
                            <li>
                                <button disabled=!clickable on:click=move |_| live.send(&json!({
                                    "type": "select", "field": field, "values": [value], "mode": "toggle",
                                }))>
                                    <span class="bar-label">{display(&row[0])}</span>
                                    <span class="bar-fill" style=format!("width: {:.1}%", share * 100.0)></span>
                                    <span class="bar-value">{display(&row[1])}</span>
                                </button>
                            </li>
                        }
                    }).collect_view()}
                </ul>
            }
            .into_any()
        }
        "line" if dims == 1 => {
            let points = line_points(&rows, 1, 1000.0, 200.0);
            let rows_count = rows.len();
            let (first, last) = (
                rows.first().map(|r| display(&r[0])).unwrap_or_default(),
                rows.last().map(|r| display(&r[0])).unwrap_or_default(),
            );
            view! {
                <svg class="line" viewBox="0 -10 1000 220" preserveAspectRatio="none" role="img"
                     aria-label=format!("{} from {first} to {last}", info.metrics.join(", "))>
                    <polyline points=points.clone() />
                    // few points: mark them, one alone draws no line
                    {(rows_count <= 60).then(|| points.split(' ').filter_map(|p| p.split_once(',')).map(|(x, y)| view! {
                        <circle cx=x.to_string() cy=y.to_string() r="4" />
                    }).collect_view())}
                </svg>
                <p class="axis"><span>{first}</span><span>{last}</span></p>
            }
            .into_any()
        }
        _ => view! {
            <div class="table-wrap">
                <table>
                    <thead><tr>
                        {info.dimensions.iter().map(|d| view! { <th>{d.field.clone()}</th> }).collect_view()}
                        {info.metrics.iter().map(|m| view! { <th class="num">{m.clone()}</th> }).collect_view()}
                    </tr></thead>
                    <tbody>
                        {rows.into_iter().map(|row| view! { <tr>
                            {row.into_iter().enumerate().map(|(i, v)| { let numeric = i >= dims; view! {
                                <td class:num=numeric>{display(&v)}</td>
                            }}).collect_view()}
                        </tr> }).collect_view()}
                    </tbody>
                </table>
            </div>
        }
        .into_any(),
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
