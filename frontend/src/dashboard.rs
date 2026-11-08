//! A dashboard: its filter lists down the side, its visuals on the grid
//! from the YAML layout. Everything here reads the live [`Board`]; clicks
//! become selections on the server, which answers with a new frame.
//!
//! [`Board`]: crate::board::Board

use leptos::ev;
use leptos::prelude::*;
use serde_json::{Value, json};
use tidepool_protocol::live::{FilterFrame, FilterInfo, PivotTotals, VisualFrame, VisualInfo};

use crate::api::{post_text, save_file};
use crate::board::{
    beside, brush_range, display, export_request, export_rows_request, filter_meta, formatted,
    marks, next_sort, node_class, number, share, value_class, visual_tags, wanted_window,
};
use crate::charts::Data;
use crate::datepick::DateRange;
use crate::live::Live;
use crate::plot::{BarChart, HeatChart, LineChart, PieChart, ScatterChart};

/// Whether the page is in dark mode (charts can't read CSS).
/// The part of `dataset.field` people read.
fn short(field: &str) -> String {
    field.split_once('.').map_or(field, |(_, f)| f).to_string()
}

/// Save view `id` as a dashboard file, under an id the person chooses
/// (from its title, at first), through `/v1/config/apply` like every
/// change; then open it. Saving needs the admin token.
pub fn save_view(id: String, name: &str) {
    let live = expect_context::<Live>();
    let Some(window) = web_sys::window() else {
        return;
    };
    let chosen = window
        .prompt_with_message_and_default(
            "Save as dashboards/<id>.yaml. Its id:",
            &crate::route::id_from(name),
        )
        .ok()
        .flatten();
    let Some(chosen) = chosen.filter(|c| !c.trim().is_empty()) else {
        return;
    };
    let chosen = chosen.trim().to_string();
    leptos::task::spawn_local(async move {
        let view = crate::api::get_json::<Value>(&format!("/v1/views/{id}")).await;
        let config = crate::api::get_json::<Value>("/v1/config").await;
        let (view, config) = match (view, config) {
            (Ok(view), Ok(config)) => (view, config),
            (Err(err), _) | (_, Err(err)) => {
                live.problem.set(Some(format!(
                    "Can't save the view ({err}): saving needs the admin token."
                )));
                return;
            }
        };
        let path = format!("dashboards/{chosen}.yaml");
        let reply = crate::api::post_json(
            "/v1/config/apply",
            &json!({
                "base_version": config["version"],
                "files": { path: view["yaml"] },
                "author": "saved from a view",
            }),
        )
        .await;
        match reply {
            Ok((200, value)) if value["status"] == "queued" => live.problem.set(Some(format!(
                "Proposed: change {} is waiting for approval.",
                value["change"]
            ))),
            Ok((200, _)) => {
                let _ = window
                    .location()
                    .set_hash(&crate::route::Route::Dashboard(chosen).href());
            }
            Ok((_, value)) => live.problem.set(Some(
                value["error"]
                    .as_str()
                    .map_or_else(|| value.to_string(), str::to_string),
            )),
            Err(err) => live.problem.set(Some(err)),
        }
    });
}

#[component]
pub fn Dashboard(slug: String) -> impl IntoView {
    let live = expect_context::<Live>();
    {
        let slug = slug.clone();
        // once connected, ask for this one
        Effect::new(move |_| {
            if live.connected.get() {
                live.open_dashboard(&slug);
            }
        });
    }
    let info = Memo::new(move |_| live.board_info(&slug));
    // the visual shown alone, as large as the grid; the rail and the
    // selection bar stay, and selections go on working
    let maximized = RwSignal::new(None::<String>);
    let escape = window_event_listener(ev::keydown, move |e| {
        // a popover's Escape closes the popover only
        if e.key() == "Escape"
            && !e.default_prevented()
            && maximized.with_untracked(Option::is_some)
        {
            maximized.set(None);
        }
    });
    on_cleanup(move || escape.remove());
    view! {
        {move || match info.get() {
            None => view! { <p class="quiet page-note">"Loading the dashboard…"</p> }.into_any(),
            Some(dashboard) => view! {
                <div class="page" class:busy=move || live.busy() aria-busy=move || live.busy().to_string()>
                    // no rail without filters for it: the grid takes the room
                    {(!dashboard.filters.is_empty()).then(|| view! {
                        <aside class="rail" aria-label="Filters">
                            {dashboard.filters.into_iter().map(|f| view! { <Filter info=f height=LIST_HEIGHT /> }).collect_view()}
                            <ul class="legend states" aria-label="What the colours mean">
                                <li><span class="mark selected"></span>"selected"</li>
                                <li><span class="mark possible"></span>"possible"</li>
                                <li><span class="mark excluded"></span>"excluded"</li>
                            </ul>
                        </aside>
                    })}
                    <section
                        class="grid"
                        class:alone=move || maximized.with(Option::is_some)
                        style=format!(
                            "grid-template-columns: repeat({}, minmax(0, 1fr)); grid-auto-rows: {}px;",
                            dashboard.columns, dashboard.row_height
                        )
                    >
                        {dashboard.visuals.into_iter().map(|v| view! { <Visual info=v row_height=dashboard.row_height maximized=maximized /> }).collect_view()}
                    </section>
                </div>
            }
            .into_any(),
        }}
    }
}

/// A filter beside the grid, or a `filter` visual on it: `height` is what
/// its values may take before they scroll.
#[component]
fn Filter(info: FilterInfo, height: f64) -> impl IntoView {
    match info.widget.as_str() {
        "date_range" => view! { <DateRange title=short(&info.field) field=info.field timed=info.timed /> }.into_any(),
        "tree" => view! { <FilterTree field=info.field measure=info.measure format=info.measure_format height=height /> }.into_any(),
        "search" => view! { <SearchBox field=info.field /> }.into_any(),
        _ => view! { <FilterList field=info.field measure=info.measure format=info.measure_format height=height /> }.into_any(),
    }
}

/// How long typing pauses before a search goes to the server.
const TYPING: std::time::Duration = std::time::Duration::from_millis(150);

/// A search box (`widget: search`): what's typed selects the rows whose
/// value contains it, sent once typing pauses. It follows the selection
/// (back, clear) unless the viewer is typing something newer.
#[component]
fn SearchBox(field: String) -> impl IntoView {
    let live = expect_context::<Live>();
    let text = RwSignal::new(String::new());
    // texts sent and not answered yet, oldest first: the selection
    // catching up with one of them isn't news, and mustn't undo what was
    // typed since
    let pending = StoredValue::new(Vec::<String>::new());
    let shown = StoredValue::new(String::new());
    let typed = StoredValue::new(0u64);
    {
        let field = field.clone();
        Effect::new(move |_| {
            let chosen = live.selection.with(|s| {
                s.iter()
                    .find(|f| f.field == field)
                    .and_then(|f| f.contains.clone())
                    .unwrap_or_default()
            });
            let ours = pending.with_value(|p| p.iter().position(|t| *t == chosen));
            match ours {
                Some(at) => pending.update_value(|p| {
                    p.drain(..=at);
                }),
                // changed elsewhere (back, clear): the box follows
                None if chosen != shown.get_value() => {
                    pending.update_value(Vec::clear);
                    text.set(chosen.clone());
                }
                None => {}
            }
            shown.set_value(chosen);
        });
    }
    let name = StoredValue::new(field.clone());
    let send = move |value: String| {
        pending.update_value(|p| p.push(value.clone()));
        live.send(&json!({"type": "search", "field": name.get_value(), "text": value}));
    };
    // what was sent last, or what the selection says when nothing is on
    // its way
    let last = move || {
        pending
            .with_value(|p| p.last().cloned())
            .unwrap_or_else(|| shown.get_value())
    };
    let on_input = move |event| {
        let value = event_target_value(&event);
        text.set(value.clone());
        let n = typed.get_value() + 1;
        typed.set_value(n);
        set_timeout(
            move || {
                if typed.get_value() == n && value != last() {
                    send(value);
                }
            },
            TYPING,
        );
    };
    let title = short(&field);
    view! {
        <section class="filter search-filter" data-field=field.clone()>
            <header class="filter-head"><h2>{title.clone()}</h2></header>
            <input
                type="search"
                placeholder="Contains…"
                aria-label=format!("Search {title}")
                prop:value=move || text.get()
                on:input=on_input
                on:keydown=move |ev: web_sys::KeyboardEvent| {
                    // Enter sends at once
                    if ev.key() == "Enter" {
                        typed.set_value(typed.get_value() + 1);
                        let value = text.get_untracked();
                        if value != last() {
                            send(value);
                        }
                    }
                }
            />
        </section>
    }
}

/// Ask for the window of a list or tree the scroll position needs.
fn follow_scroll(
    live: Live,
    frame: Memo<Option<FilterFrame>>,
    scroll: RwSignal<f64>,
    height: f64,
    window: impl Fn(usize, usize) -> Value + 'static,
) {
    Effect::new(move |_| {
        let top = scroll.get();
        let Some(frame) = frame.get() else { return };
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let first = (top / ROW).floor() as usize;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let visible = (height / ROW).ceil() as usize + 1;
        let have = frame.values.len().max(frame.nodes.len());
        if let Some((offset, limit)) =
            wanted_window(first, visible, (frame.offset, have), frame.total)
        {
            live.send(&window(offset, limit));
        }
    });
}

const ROW: f64 = 26.0;
const LIST_HEIGHT: f64 = 300.0;

/// The room a filter visual `rows` tall has for its values: its cells and
/// the gaps between them, less its padding, title and search box.
fn list_height(rows: u16, row_height: u16) -> f64 {
    let cells = f64::from(rows) * f64::from(row_height) + f64::from(rows.saturating_sub(1)) * 16.0;
    (cells - 92.0).max(ROW * 2.0)
}

/// A filter's header: its name, and how many values, how many selected and
/// what's measured.
fn filter_head(
    live: Live,
    field: &str,
    frame: Memo<Option<FilterFrame>>,
    measure: Option<String>,
    tree: bool,
) -> impl IntoView + use<> {
    let key = field.to_string();
    let meta = move || {
        let selected = live.selection.with(|s| {
            s.iter()
                .find(|f| f.field == key)
                .map_or(0, |f| f.values.as_ref().map_or(1, Vec::len))
        });
        let total = frame.with(|f| f.as_ref().map(|f| f.total));
        filter_meta(total, selected, measure.as_deref(), tree)
    };
    view! {
        <header class="filter-head">
            <h2>{short(field)}</h2>
            <span class="filter-meta">{meta}</span>
        </header>
    }
}

/// The largest measure among the values shown, for their meters.
fn largest(frame: &FilterFrame) -> f64 {
    frame
        .values
        .iter()
        .map(|v| v.measure.as_ref())
        .chain(frame.nodes.iter().map(|n| n.measure.as_ref()))
        .filter_map(|m| m.and_then(number))
        .fold(0.0, f64::max)
}

/// A filter list: every value with its state (selected, possible,
/// excluded), searchable, only the rows on screen in the DOM, fetched from
/// the server a window at a time.
#[component]
fn FilterList(
    field: String,
    measure: Option<String>,
    format: Option<String>,
    height: f64,
) -> impl IntoView {
    let measured = measure.is_some();
    let live = expect_context::<Live>();
    let key = field.clone();
    // only this list's frame: others mustn't re-render (and so swallow
    // clicks on) its values
    let frame = Memo::new(move |_| live.board.with(|b| b.filters.get(&key).cloned()));
    let search = RwSignal::new(String::new());
    let scroll = RwSignal::new(0.0_f64);
    // stored, so both closures below can use it
    let field_name = StoredValue::new(field.clone());
    let window = move |offset: usize, limit: usize| {
        json!({
            "type": "filter_window", "field": field_name.get_value(),
            "offset": offset, "limit": limit, "search": search.get_untracked(),
        })
    };
    // ask for the rows on screen when scrolling leaves the window there is
    follow_scroll(live, frame, scroll, height, window);
    let on_search = move |event| {
        search.set(event_target_value(&event));
        scroll.set(0.0);
        live.send(&window(0, 120));
    };
    let title = short(&field);
    let select_field = field.clone();
    view! {
        <section class="filter" data-field=field.clone()>
            {filter_head(live, &field, frame, measure, false)}
            <input type="search" placeholder="Search" aria-label=format!("Search {title}") on:input=on_search />
            <div
                class="values"
                style=move || {
                    // as tall as its values, up to a scrolling window
                    #[allow(clippy::cast_precision_loss)]
                    let rows = frame.with(|f| f.as_ref().map_or(0, |f| f.total)) as f64;
                    format!("height: {}px", (rows * ROW).min(height))
                }
                on:scroll=move |ev| scroll.set(f64::from(event_target::<web_sys::Element>(&ev).scroll_top()))
            >
                {move || frame.get().map(|f: FilterFrame| {
                    #[allow(clippy::cast_precision_loss)]
                    let height = f.total as f64 * ROW;
                    let field = select_field.clone();
                    let format = format.clone();
                    let most = largest(&f);
                    view! {
                        <ul style=format!("height: {height}px")>
                            {f.values.into_iter().enumerate().map(|(i, v)| {
                                let class = value_class(v.state, false, false);
                                let pressed = v.state == tidepool_protocol::live::ValueState::Selected;
                                #[allow(clippy::cast_precision_loss)]
                                let top = (f.offset + i) as f64 * ROW;
                                let (field, value) = (field.clone(), v.value.clone());
                                view! {
                                    <li style=format!("top: {top}px")>
                                        <button
                                            class=class
                                            aria-pressed=pressed.to_string()
                                            on:click=move |_| live.select(&field, std::slice::from_ref(&value), true)
                                        >
                                            {measured.then(|| view! { <span class="meter" style=format!("width: {:.1}%", share(v.measure.as_ref(), most))></span> })}
                                            <span class="label">{display(&v.value)}</span>
                                            <span class="rows">{beside(measured, v.measure.as_ref(), v.rows, format.as_deref())}</span>
                                        </button>
                                    </li>
                                }
                            }).collect_view()}
                        </ul>
                    }
                })}
            </div>
            {move || frame.get().filter(|f| f.total == 0).map(|_| view! { <p class="quiet">"No values match."</p> })}
        </section>
    }
}

/// A tree filter (a hierarchy field): nodes indented by depth, opened with
/// their twist, selected with a click (a node selects everything below
/// it). A node inside a selected branch is washed in the selection colour;
/// one with a selection somewhere below it carries a dot, so a selection
/// stays findable with its branch closed.
#[component]
fn FilterTree(
    field: String,
    measure: Option<String>,
    format: Option<String>,
    height: f64,
) -> impl IntoView {
    let measured = measure.is_some();
    let live = expect_context::<Live>();
    let key = field.clone();
    let frame = Memo::new(move |_| live.board.with(|b| b.filters.get(&key).cloned()));
    let search = RwSignal::new(String::new());
    let scroll = RwSignal::new(0.0_f64);
    let field_name = StoredValue::new(field.clone());
    let window = move |offset: usize, limit: usize| {
        json!({
            "type": "filter_window", "field": field_name.get_value(),
            "offset": offset, "limit": limit, "search": search.get_untracked(),
        })
    };
    follow_scroll(live, frame, scroll, height, window);
    let on_search = move |event| {
        search.set(event_target_value(&event));
        scroll.set(0.0);
        live.send(&window(0, 120));
    };
    let title = short(&field);
    view! {
        <section class="filter tree" data-field=field.clone()>
            {filter_head(live, &field, frame, measure, true)}
            <input type="search" placeholder="Search" aria-label=format!("Search {title}") on:input=on_search />
            <div
                class="values"
                role="tree"
                aria-label=title.clone()
                style=move || {
                    #[allow(clippy::cast_precision_loss)]
                    let rows = frame.with(|f| f.as_ref().map_or(0, |f| f.total)) as f64;
                    format!("height: {}px", (rows * ROW).min(height))
                }
                on:scroll=move |ev| scroll.set(f64::from(event_target::<web_sys::Element>(&ev).scroll_top()))
            >
                {move || frame.get().map(|f: FilterFrame| {
                    #[allow(clippy::cast_precision_loss)]
                    let height = f.total as f64 * ROW;
                    let format = format.clone();
                    let most = largest(&f);
                    view! {
                        <ul style=format!("height: {height}px")>
                            {f.nodes.into_iter().enumerate().map(|(i, node)| {
                                #[allow(clippy::cast_precision_loss)]
                                let top = (f.offset + i) as f64 * ROW;
                                let class = node_class(&node);
                                let selected = node.state == tidepool_protocol::live::ValueState::Selected;
                                let (open, has_children) = (node.open, node.has_children);
                                let (twist_key, select_key) = (node.key.clone(), node.key.clone());
                                let twist = move |_| live.send(&json!({
                                    "type": "filter_open", "field": field_name.get_value(),
                                    "keys": [twist_key.clone()], "open": !open,
                                }));
                                let select = move |ev: web_sys::MouseEvent| {
                                    live.select(
                                        &field_name.get_value(),
                                        std::slice::from_ref(&select_key),
                                        ev.ctrl_key() || ev.meta_key() || selected,
                                    );
                                };
                                view! {
                                    <li
                                        style=format!("top: {top}px; padding-left: {}px", node.depth * 12)
                                        role="treeitem"
                                        aria-expanded=has_children.then(|| open.to_string())
                                        attr:aria-level=(node.depth + 1).to_string()
                                    >
                                        {if has_children {
                                            view! {
                                                <button class="twist" aria-label=if open { "Close" } else { "Open" } on:click=twist>
                                                    <svg viewBox="0 0 10 10" aria-hidden="true" class:open=open><path d="M3.5 2 7 5 3.5 8" /></svg>
                                                </button>
                                            }.into_any()
                                        } else {
                                            view! { <span class="twist" aria-hidden="true"></span> }.into_any()
                                        }}
                                        <button class=class aria-pressed=selected.to_string() on:click=select>
                                            {measured.then(|| view! { <span class="meter" style=format!("width: {:.1}%", share(node.measure.as_ref(), most))></span> })}
                                            <span class="node-label">
                                                {node.holds.then(|| view! { <span class="dot" aria-label="selection below"></span> })}
                                                {display(&node.label)}
                                                {node.kind.as_ref().map(|k| view! { <span class="kind">{display(k)}</span> })}
                                            </span>
                                            <span class="rows">{beside(measured, node.measure.as_ref(), node.rows, format.as_deref())}</span>
                                        </button>
                                    </li>
                                }
                            }).collect_view()}
                        </ul>
                    }
                })}
            </div>
            {move || frame.get().filter(|f| f.total == 0).map(|_| view! { <p class="quiet">"No values match."</p> })}
        </section>
    }
}

/// A visual on the grid. `maximized` names the visual shown alone, if
/// any: this one is then the whole grid, or hidden. It stays the same
/// element either way, so a chart keeps its state and only resizes.
#[component]
fn Visual(info: VisualInfo, row_height: u16, maximized: RwSignal<Option<String>>) -> AnyView {
    let live = expect_context::<Live>();
    let id = info.id.clone();
    let frame = Memo::new(move |_| live.board.with(|b| b.visuals.get(&id).cloned()));
    let alone = {
        let id = info.id.clone();
        move || maximized.with(|m| m.as_deref() == Some(id.as_str()))
    };
    let place = format!(
        "grid-column: {} / span {}; grid-row: {} / span {};",
        info.at.x + 1,
        info.at.w,
        info.at.y + 1,
        info.at.h
    );
    if info.visual_type == "filter"
        && let Some(dimension) = info.dimensions.first()
    {
        // its list is the filter frames of its field, as beside the grid
        let filter = FilterInfo {
            field: dimension.field.clone(),
            widget: info.widget.clone().unwrap_or_else(|| "list".into()),
            timed: false,
            default: None,
            measure: None,
            measure_format: None,
        };
        return view! {
            <article class="visual filter-visual" style=place data-visual=info.id.clone()>
                <Filter info=filter height=list_height(info.at.h, row_height) />
            </article>
        }
        .into_any();
    }
    let dimensions = info.dimensions.iter().map(|d| short(&d.field));
    let title = match info.dimensions.first() {
        _ if info.title.is_some() => info.title.clone().unwrap_or_default(),
        _ if info.metrics.is_empty() => dimensions.collect::<Vec<_>>().join(", "),
        Some(first) => format!("{} by {}", info.metrics.join(", "), short(&first.field)),
        None => info.metrics.join(", "),
    };
    let class = format!("visual {}", info.visual_type);
    let dims = info.dimensions.len();
    let chart = (matches!(
        info.visual_type.as_str(),
        "bar" | "line" | "pie" | "scatter"
    ) && dims == 1)
        || (info.visual_type == "heatmap" && dims == 2);
    if chart {
        // one chart for the visual's whole life, fed new options: re-made
        // per frame, it would replay its intro and lose clicks
        let rows = Memo::new(move |_| {
            frame.with(|f| {
                f.as_ref()
                    .and_then(|f| f.result.as_ref())
                    .map(|r| r.rows.clone())
                    .unwrap_or_default()
            })
        });
        let note = move || match frame.get() {
            None => Some(view! { <p class="quiet">"Waiting for data"</p> }.into_any()),
            Some(VisualFrame {
                error: Some(error), ..
            }) => Some(
                view! { <p class="problem">{format!("Can't show this: {error}")}</p> }.into_any(),
            ),
            Some(_) => None,
        };
        let content = match info.visual_type.as_str() {
            "bar" => bar_chart(live, &info, rows, title.clone()),
            "scatter" => scatter_chart(live, &info, rows, title.clone()),
            "heatmap" => heat_chart(live, &info, rows, title.clone()),
            "pie" => {
                let whole = Signal::derive(move || {
                    frame.with(|f| f.as_ref().and_then(|f| f.compare.as_ref()).and_then(number))
                });
                pie_chart(live, &info, rows, whole, title.clone())
            }
            _ => line_chart(live, &info, rows, title.clone()),
        };
        return view! {
            <article class=class class:maximized=alone style=place data-visual=info.id.clone()>
                {visual_head(live, &info, &title, maximized)}
                {note}
                {content}
            </article>
        }
        .into_any();
    }
    let id = info.id.clone();
    let head = visual_head(live, &info, &title, maximized);
    view! {
        <article class=class class:maximized=alone style=place data-visual=id>
            {head}
            {move || match frame.get() {
                None => view! { <p class="quiet">"Waiting for data"</p> }.into_any(),
                Some(VisualFrame { error: Some(error), .. }) => {
                    view! { <p class="problem">{format!("Can't show this: {error}")}</p> }.into_any()
                }
                Some(VisualFrame { result: Some(result), compare, totals, .. }) => {
                    body(live, &info, result.rows, compare.as_ref(), totals)
                }
                Some(_) => view! { <p class="quiet">"No result"</p> }.into_any(),
            }}
        </article>
    }
    .into_any()
}

/// A visual's header: its title, how it's set up, its export, maximize.
fn visual_head(
    live: Live,
    info: &VisualInfo,
    title: &str,
    maximized: RwSignal<Option<String>>,
) -> impl IntoView + use<> {
    let tags = visual_tags(info);
    let description = info.description.clone();
    view! {
        <header class="visual-head" title=description.clone()>
            <h2>{title.to_string()}</h2>
            {tags.into_iter().map(|t| view! { <code class="tag">{t}</code> }).collect_view()}
            <span class="grow"></span>
            {export_button(live, info)}
            {maximize_button(&info.id, maximized)}
        </header>
    }
}

/// Show the visual alone, as large as the grid, or put it back (Escape
/// does too).
fn maximize_button(id: &str, maximized: RwSignal<Option<String>>) -> impl IntoView + use<> {
    let id = id.to_string();
    let alone = {
        let id = id.clone();
        Memo::new(move |_| maximized.with(|m| m.as_ref() == Some(&id)))
    };
    let toggle = move |_| maximized.set((!alone.get_untracked()).then(|| id.clone()));
    view! {
        <button
            class="maximize icon-button"
            aria-pressed=move || alone.get().to_string()
            title=move || if alone.get() { "Restore (Esc)" } else { "Maximize" }
            on:click=toggle
        >
            {move || if alone.get() {
                view! { <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M6 2v4H2M10 2v4h4M14 10h-4v4M2 10h4v4" /></svg> }.into_any()
            } else {
                view! { <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M2 6V2h4M10 2h4v4M14 10v4h-4M6 14H2v-4" /></svg> }.into_any()
            }}
            <span class="sr">{move || if alone.get() { "Restore" } else { "Maximize" }}</span>
        </button>
    }
}

/// "CSV": the visual's query under the current selection, downloaded.
fn export_button(live: Live, info: &VisualInfo) -> impl IntoView + use<> {
    let info = info.clone();
    let failed = RwSignal::new(None::<String>);
    let export = move |_| {
        let name = format!("{}.csv", info.id);
        // a rows visual exports its rows, as many as it can show
        let rows = (info.visual_type == "rows").then(|| {
            live.selection
                .with_untracked(|s| export_rows_request(&info, s, 1000))
        });
        let request = live.selection.with_untracked(|s| export_request(&info, s));
        leptos::task::spawn_local(async move {
            let reply = match rows {
                Some(rows) => post_text("/v1/rows?format=csv", &rows).await,
                None => post_text("/v1/query?format=csv", &request).await,
            };
            let outcome = match reply {
                Ok((200, text)) => save_file(&name, &text, "text/csv"),
                Ok((status, text)) => Err(format!("the server answered {status}: {text}")),
                Err(err) => Err(err),
            };
            failed.set(outcome.err());
        });
    };
    view! {
        <button class="export icon-button" title="Download this visual's data as CSV, under the current selection" on:click=export>
            <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M8 2v8M4.5 6.5 8 10l3.5-3.5M3 13.5h10" /></svg>
            <span class="sr">"CSV"</span>
        </button>
        {move || failed.get().map(|e| view! { <span class="problem" role="alert">{e}</span> })}
    }
}

/// A visual's content, by type.
fn body(
    live: Live,
    info: &VisualInfo,
    rows: Vec<Vec<Value>>,
    compare: Option<&Value>,
    totals: Option<PivotTotals>,
) -> AnyView {
    match info.visual_type.as_str() {
        "pivot" => view! { <PivotTable info=info.clone() rows=rows totals=totals /> }.into_any(),
        "kpi" => kpi(live, info, &rows, compare),
        "rows" => view! { <RowsTable info=info.clone() rows=rows /> }.into_any(),
        "table" | "bar" | "line" => view! { <Table info=info.clone() rows=rows /> }.into_any(),
        // a type this page doesn't know (a newer server): say so, rather
        // than draw it as something it isn't
        other => view! {
            <p class="problem" role="alert">
                {format!("This page can't show a `{other}` visual: reload it to get the newer UI.")}
            </p>
        }
        .into_any(),
    }
}

fn kpi(live: Live, info: &VisualInfo, rows: &[Vec<Value>], compare: Option<&Value>) -> AnyView {
    let dims = info.dimensions.len();
    let value = rows
        .first()
        .and_then(|r| r.get(dims))
        .cloned()
        .unwrap_or(Value::Null);
    let fresh = live
        .board
        .with_untracked(|b| b.fresh.contains(&(info.id.clone(), 0, dims)));
    let delta = match (number(&value), compare.and_then(number)) {
        (Some(now), Some(then)) if then != 0.0 => Some((now - then) / then.abs() * 100.0),
        _ => None,
    };
    let against = info.compare.clone().unwrap_or_default();
    let format = info.metrics.first().and_then(|m| info.formats.get(m));
    let shown = formatted(&value, format.map(String::as_str));
    // the whole value, for when a format shortens it
    let whole = (format.is_some() && shown != display(&value)).then(|| display(&value));
    view! {
        <p class="kpi-value" class:fresh=fresh title=whole>{shown}</p>
        {delta.map(|d| view! {
            <p class="kpi-delta">
                <span class="delta" class:down=d < 0.0>{format!("{}{d:.1}%", if d >= 0.0 { "+" } else { "−" }, d = d.abs())}</span>
                {format!(" vs {against}")}
            </p>
        })}
    }
    .into_any()
}

fn bar_chart(live: Live, info: &VisualInfo, rows: Memo<Vec<Vec<Value>>>, title: String) -> AnyView {
    let field = info.dimensions[0].field.clone();
    let (metrics, formats) = (info.metrics.clone(), info.formats.clone());
    let data = Memo::new(move |_| rows.with(|rows| Data::formatted(rows, &metrics, &formats)));
    let shown = field.clone();
    // the selection colours the bars too
    let selected = Signal::derive(move || {
        live.selection.with(|s| {
            s.iter()
                .find(|f| f.field == shown)
                .and_then(|f| f.values.as_ref())
                .map(|v| v.iter().map(display).collect::<Vec<_>>())
                .unwrap_or_default()
        })
    });
    let on_select = info.on_click.then(|| {
        Callback::new(move |(categories, additive): (Vec<usize>, bool)| {
            let values: Vec<Value> = rows.with_untracked(|rows| {
                categories
                    .iter()
                    .filter_map(|&i| rows.get(i).and_then(|r| r.first()).cloned())
                    .collect()
            });
            if !values.is_empty() {
                live.select(&field, &values, additive);
            }
        })
    });
    view! {
        <div class="chart">
            <BarChart data=data selected=selected on_select=on_select label=title />
        </div>
    }
    .into_any()
}

fn heat_chart(
    live: Live,
    info: &VisualInfo,
    rows: Memo<Vec<Vec<Value>>>,
    title: String,
) -> AnyView {
    let (x_field, y_field) = (
        info.dimensions[0].field.clone(),
        info.dimensions[1].field.clone(),
    );
    let metric = info.metrics.first().cloned().unwrap_or_default();
    let format = info.formats.get(&metric).cloned();
    let picked = |field: String| {
        move || {
            live.selection.with(|s| {
                s.iter()
                    .find(|f| f.field == field)
                    .and_then(|f| f.values.as_ref())
                    .map(|v| v.iter().map(display).collect::<Vec<_>>())
                    .unwrap_or_default()
            })
        }
    };
    let (sx, sy) = (picked(x_field.clone()), picked(y_field.clone()));
    let selected = Signal::derive(move || (sx(), sy()));
    let on_select = info.on_click.then(|| {
        Callback::new(move |(xs, ys, additive): (Vec<usize>, Vec<usize>, bool)| {
            let data = rows.with_untracked(|r| crate::charts::HeatData::new(r, "", None));
            let pick = |values: &[Value], at: &[usize]| -> Vec<Value> {
                at.iter().filter_map(|i| values.get(*i).cloned()).collect()
            };
            live.send(&json!({
                "type": "select_many",
                "mode": if additive { "add" } else { "replace" },
                "fields": [
                    {"field": x_field, "values": pick(&data.x_values, &xs)},
                    {"field": y_field, "values": pick(&data.y_values, &ys)},
                ],
            }));
        })
    });
    view! {
        <div class="chart">
            <HeatChart rows=rows metric=metric format=format selected=selected on_select=on_select label=title />
        </div>
    }
    .into_any()
}

fn scatter_chart(
    live: Live,
    info: &VisualInfo,
    rows: Memo<Vec<Vec<Value>>>,
    title: String,
) -> AnyView {
    let field = info.dimensions[0].field.clone();
    let (metrics, formats) = (info.metrics.clone(), info.formats.clone());
    let data = Memo::new(move |_| rows.with(|rows| Data::formatted(rows, &metrics, &formats)));
    let shown = field.clone();
    let selected = Signal::derive(move || {
        live.selection.with(|s| {
            s.iter()
                .find(|f| f.field == shown)
                .and_then(|f| f.values.as_ref())
                .map(|v| v.iter().map(display).collect::<Vec<_>>())
                .unwrap_or_default()
        })
    });
    let on_select = info.on_click.then(|| {
        Callback::new(move |(categories, additive): (Vec<usize>, bool)| {
            let values: Vec<Value> = rows.with_untracked(|rows| {
                categories
                    .iter()
                    .filter_map(|&i| rows.get(i).and_then(|r| r.first()).cloned())
                    .collect()
            });
            if !values.is_empty() {
                live.select(&field, &values, additive);
            }
        })
    });
    view! {
        <div class="chart">
            <ScatterChart data=data selected=selected on_select=on_select label=title />
        </div>
    }
    .into_any()
}

fn pie_chart(
    live: Live,
    info: &VisualInfo,
    rows: Memo<Vec<Vec<Value>>>,
    whole: Signal<Option<f64>>,
    title: String,
) -> AnyView {
    let field = info.dimensions[0].field.clone();
    let (metrics, formats) = (info.metrics.clone(), info.formats.clone());
    let data = Memo::new(move |_| rows.with(|rows| Data::formatted(rows, &metrics, &formats)));
    let shown = field.clone();
    let selected = Signal::derive(move || {
        live.selection.with(|s| {
            s.iter()
                .find(|f| f.field == shown)
                .and_then(|f| f.values.as_ref())
                .map(|v| v.iter().map(display).collect::<Vec<_>>())
                .unwrap_or_default()
        })
    });
    let on_select = info.on_click.then(|| {
        Callback::new(move |(categories, additive): (Vec<usize>, bool)| {
            let values: Vec<Value> = rows.with_untracked(|rows| {
                categories
                    .iter()
                    .filter_map(|&i| rows.get(i).and_then(|r| r.first()).cloned())
                    .collect()
            });
            if !values.is_empty() {
                live.select(&field, &values, additive);
            }
        })
    });
    view! {
        <div class="chart">
            <PieChart data=data selected=selected whole=whole on_select=on_select label=title />
        </div>
    }
    .into_any()
}

fn line_chart(
    live: Live,
    info: &VisualInfo,
    rows: Memo<Vec<Vec<Value>>>,
    title: String,
) -> AnyView {
    let (metrics, formats) = (info.metrics.clone(), info.formats.clone());
    let data = Memo::new(move |_| rows.with(|rows| Data::formatted(rows, &metrics, &formats)));
    let dimension = info.dimensions[0].clone();
    let grain = dimension.grain.clone();
    if !info.on_brush {
        return view! { <div class="chart"><LineChart data=data grain=grain label=title /></div> }
            .into_any();
    }
    // a brushed span selects its range: whole periods at a grain, the
    // values themselves otherwise (`brush_range`)
    let on_brush = Callback::new(move |(start, end): (usize, usize)| {
        let at = |i: usize| {
            rows.with_untracked(|rows| {
                rows.get(i)
                    .and_then(|r| r.first())
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
        };
        let (Some(first), Some(last)) = (at(start), at(end.max(start))) else {
            return;
        };
        let Some((from, to)) = brush_range(&first, &last, dimension.grain.as_deref()) else {
            return;
        };
        live.send(&json!({
            "type": "select_range", "field": dimension.field, "from": from, "to": to,
        }));
    });
    view! { <div class="chart"><LineChart data=data grain=grain on_brush=on_brush label=title /></div> }.into_any()
}

/// A table: sortable by any column (the server sorts), a row click selects
/// its first dimension's value, changed cells flash.
/// A `rows` visual: the newest rows, newest first, each as it is. Text a
/// search selects is marked where it's found.
#[component]
fn RowsTable(info: VisualInfo, rows: Vec<Vec<Value>>) -> impl IntoView {
    let live = expect_context::<Live>();
    let fields: Vec<String> = info.dimensions.iter().map(|d| d.field.clone()).collect();
    // the text searched for in each column, if any
    let searched: Vec<Option<String>> = live.selection.with_untracked(|s| {
        fields
            .iter()
            .map(|f| {
                s.iter()
                    .find(|c| c.field == *f)
                    .and_then(|c| c.contains.clone())
            })
            .collect()
    });
    let empty = rows.is_empty();
    view! {
        <div class="table-wrap rows-wrap">
            <table class="rows-table">
                <thead><tr>
                    {fields.iter().map(|f| view! { <th>{short(f)}</th> }).collect_view()}
                </tr></thead>
                <tbody>
                    {rows.into_iter().map(|row| view! {
                        <tr>
                            {row.into_iter().enumerate().map(|(c, v)| {
                                let text = display(&v);
                                let numeric = v.is_number();
                                let parts = match searched.get(c).cloned().flatten() {
                                    Some(needle) => marks(&text, &needle),
                                    None => vec![(text, false)],
                                };
                                view! {
                                    <td class:num=numeric class:null=v.is_null()>
                                        {parts.into_iter().map(|(part, hit)| if hit {
                                            view! { <mark>{part}</mark> }.into_any()
                                        } else {
                                            part.into_any()
                                        }).collect_view()}
                                    </td>
                                }
                            }).collect_view()}
                        </tr>
                    }).collect_view()}
                </tbody>
            </table>
            {empty.then(|| view! { <p class="quiet">"No rows match."</p> })}
        </div>
    }
}

#[component]
fn Table(info: VisualInfo, rows: Vec<Vec<Value>>) -> impl IntoView {
    let live = expect_context::<Live>();
    let dims = info.dimensions.len();
    let headers: Vec<String> = info
        .dimensions
        .iter()
        .map(|d| match &d.grain {
            Some(g) => format!("{}:{g}", d.field),
            None => d.field.clone(),
        })
        .chain(info.metrics.iter().cloned())
        .collect();
    let visual = info.id.clone();
    let field = info.dimensions.first().map(|d| d.field.clone());
    // a bar behind the metric it's sorted by (the first metric otherwise):
    // its share of the largest shown
    let barred = live
        .sorts
        .with_untracked(|s| s.get(&visual).map(|(c, _)| c.clone()))
        .and_then(|c| headers.iter().position(|h| *h == c))
        .filter(|&i| i >= dims)
        .or_else(|| (headers.len() > dims).then_some(dims));
    // each metric column's format, in column order after the dimensions
    let formats: Vec<Option<String>> = info
        .metrics
        .iter()
        .map(|m| info.formats.get(m).cloned())
        .collect();
    let most = barred.map_or(0.0, |c| {
        rows.iter()
            .filter_map(|r| r.get(c).and_then(number))
            .fold(0.0, f64::max)
    });
    view! {
        <div class="table-wrap">
            <table>
                <thead><tr>
                    {headers.iter().enumerate().map(|(i, h)| {
                        let (column, visual) = (h.clone(), visual.clone());
                        let numeric = i >= dims;
                        let label = if numeric { h.clone() } else { short(h) };
                        let sorted = {
                            let (column, visual) = (column.clone(), visual.clone());
                            move || live.sorts.with(|s| s.get(&visual).filter(|(c, _)| *c == column).map(|(_, desc)| *desc))
                        };
                        let mark = {
                            let sorted = sorted.clone();
                            move || match sorted() {
                                Some(true) => " ↓",
                                Some(false) => " ↑",
                                None => "",
                            }
                        };
                        let aria = move || match sorted() {
                            Some(true) => "descending",
                            Some(false) => "ascending",
                            None => "none",
                        };
                        view! {
                            <th class:num=numeric aria-sort=aria>
                                <button on:click=move |_| {
                                    let desc = live.sorts.with_untracked(|s| next_sort(s.get(&visual), &column, numeric));
                                    live.sorts.update(|s| { s.insert(visual.clone(), (column.clone(), desc)); });
                                    live.send(&json!({"type": "sort", "visual": visual, "column": column, "desc": desc}));
                                }>{label}{mark}</button>
                            </th>
                        }
                    }).collect_view()}
                </tr></thead>
                <tbody>
                    {rows.into_iter().enumerate().map(|(r, row)| {
                        let (field, value) = (field.clone(), row.first().cloned());
                        let visual = info.id.clone();
                        view! {
                            <tr on:click=move |ev| {
                                if let (Some(field), Some(value)) = (&field, &value) {
                                    live.select(field, std::slice::from_ref(value), ev.ctrl_key() || ev.meta_key());
                                }
                            }>
                                {row.into_iter().enumerate().map(|(c, v)| {
                                    let numeric = c >= dims;
                                    let fresh = live.board.with_untracked(|b| b.fresh.contains(&(visual.clone(), r, c)));
                                    let bar = (barred == Some(c)).then(|| view! {
                                        <span class="cell-bar" style=format!("width: {:.1}%", share(Some(&v), most))></span>
                                    });
                                    let format = c.checked_sub(dims).and_then(|m| formats.get(m)).map(|f| f.as_deref());
                                    let text = formatted(&v, format.flatten());
                                    view! { <td class:num=numeric class:fresh=fresh class:barred=barred == Some(c)>{bar}<span class="cell">{text}</span></td> }
                                }).collect_view()}
                            </tr>
                        }
                    }).collect_view()}
                </tbody>
            </table>
        </div>
    }
}

/// Whether every one of `fields` (with its value) is selected.
fn all_chosen(live: Live, fields: &[(String, Value)]) -> bool {
    !fields.is_empty()
        && live.selection.with_untracked(|s| {
            fields.iter().all(|(field, value)| {
                let shown = display(value);
                s.iter().any(|f| {
                    f.field == *field
                        && f.values
                            .as_ref()
                            .is_some_and(|v| v.iter().any(|v| display(v) == shown))
                })
            })
        })
}

/// Select `fields`' values as one step (Ctrl or ⌘ adds them).
fn pick(live: Live, fields: &[(String, Value)], add: bool) {
    let fields: Vec<Value> = fields
        .iter()
        .map(|(field, value)| json!({"field": field, "values": [value]}))
        .collect();
    live.send(&json!({
        "type": "select_many",
        "mode": if add { "add" } else { "replace" },
        "fields": fields,
    }));
}

/// A pivot table: its dimensions down the side as nested row headers, the
/// ones across as column headers, every metric in each cell, and the
/// totals the server computed. A cell selects its row's and column's
/// values; a header, its own.
#[component]
#[allow(clippy::too_many_lines)] // one table, drawn in order
fn PivotTable(
    info: VisualInfo,
    rows: Vec<Vec<Value>>,
    totals: Option<PivotTotals>,
) -> impl IntoView {
    use crate::pivot::Pivot;
    let live = expect_context::<Live>();
    let (down, across) = (info.dimensions.len(), info.across.len());
    let grid = Pivot::new(&rows, down, across, totals.as_ref());
    let metrics = info.metrics.clone();
    let m = metrics.len().max(1);
    let formats: Vec<Option<String>> = metrics
        .iter()
        .map(|n| info.formats.get(n).cloned())
        .collect();
    let down_fields: Vec<String> = info.dimensions.iter().map(|d| d.field.clone()).collect();
    let across_fields: Vec<String> = info.across.iter().map(|d| d.field.clone()).collect();
    let clicks = info.on_click;
    let named = |fields: &[String], values: &[Value]| -> Vec<(String, Value)> {
        fields.iter().cloned().zip(values.iter().cloned()).collect()
    };
    // a clickable label: a button when clicks select
    let label = move |text: String, chosen: Vec<(String, Value)>| -> AnyView {
        if clicks {
            view! {
                <button class="pick" on:click=move |ev| pick(live, &chosen, ev.ctrl_key() || ev.meta_key())>{text}</button>
            }
            .into_any()
        } else {
            view! { <span>{text}</span> }.into_any()
        }
    };
    let values = |cell: Option<&Vec<Value>>| -> Vec<String> {
        (0..m)
            .map(|i| {
                cell.and_then(|c| c.get(i))
                    .map(|v| formatted(v, formats.get(i).cloned().flatten().as_deref()))
                    .unwrap_or_default()
            })
            .collect()
    };
    let total_column = grid.row_totals.is_some();
    let metric_row = m > 1 || across == 0;
    let header_rows = across + usize::from(metric_row);
    let mut head: Vec<Vec<AnyView>> = (0..header_rows).map(|_| Vec::new()).collect();
    // the corner: the names of the dimensions down the side
    for field in &down_fields {
        head[0].push(
            view! { <th class="corner" scope="col" rowspan=header_rows>{short(field)}</th> }
                .into_any(),
        );
    }
    let column_spans = grid.column_spans();
    for level in 0..across {
        for (c, spans) in column_spans.iter().enumerate() {
            let Some(span) = spans[level] else { continue };
            let value = grid.columns[c][level].clone();
            let chosen = named(&across_fields[..=level], &grid.columns[c][..=level]);
            let selected = all_chosen(live, &chosen);
            head[level].push(view! {
                <th scope="colgroup" colspan=span * m class:chosen=selected>{label(display(&value), chosen)}</th>
            }.into_any());
        }
        if level == 0 && total_column {
            head[0].push(
                view! { <th class="total" scope="colgroup" rowspan=across colspan=m>"Total"</th> }
                    .into_any(),
            );
        }
    }
    if metric_row {
        let names = || {
            metrics
                .iter()
                .map(|n| view! { <th class="num" scope="col">{n.clone()}</th> }.into_any())
        };
        let row = head.last_mut();
        if let Some(row) = row {
            for _ in &grid.columns {
                row.extend(names());
            }
            if total_column {
                row.extend(names());
            }
        }
    }
    let row_spans = grid.row_spans();
    let body = grid.rows.iter().enumerate().map(|(r, key)| {
        let mut cells: Vec<AnyView> = Vec::new();
        for (level, span) in row_spans[r].iter().enumerate() {
            let Some(span) = *span else { continue };
            let chosen = named(&down_fields[..=level], &key[..=level]);
            let selected = all_chosen(live, &chosen);
            cells.push(view! {
                <th scope="rowgroup" rowspan=span class:chosen=selected>{label(display(&key[level]), chosen)}</th>
            }.into_any());
        }
        for (c, column) in grid.columns.iter().enumerate() {
            let cell = grid.cells[r][c].as_ref();
            let mut chosen = named(&down_fields, key);
            chosen.extend(named(&across_fields, column));
            let selected = all_chosen(live, &chosen);
            for (i, text) in values(cell).into_iter().enumerate() {
                let chosen = chosen.clone();
                let empty = cell.is_none();
                cells.push(view! {
                    <td class="num" class:chosen=selected class:empty=empty data-row=r data-column=c data-metric=i>
                        {if empty { ().into_any() } else { label(text, chosen) }}
                    </td>
                }.into_any());
            }
        }
        if let Some(totals) = &grid.row_totals {
            for text in values(totals[r].as_ref()) {
                cells.push(view! { <td class="num total">{text}</td> }.into_any());
            }
        }
        view! { <tr>{cells}</tr> }
    }).collect_view();
    // the total row: each column's, and the whole in the corner
    let foot = (grid.column_totals.is_some() || grid.all.is_some()).then(|| {
        let mut cells: Vec<AnyView> =
            vec![view! { <th class="total" scope="row" colspan=down>"Total"</th> }.into_any()];
        match &grid.column_totals {
            Some(totals) => {
                for total in totals {
                    for text in values(total.as_ref()) {
                        cells.push(view! { <td class="num total">{text}</td> }.into_any());
                    }
                }
                if total_column {
                    for text in values(grid.all.as_ref()) {
                        cells.push(view! { <td class="num total all">{text}</td> }.into_any());
                    }
                }
            }
            // without across, the whole is the one column's total
            None => {
                for text in values(grid.all.as_ref()) {
                    cells.push(view! { <td class="num total all">{text}</td> }.into_any());
                }
            }
        }
        view! { <tfoot><tr>{cells}</tr></tfoot> }
    });
    let hidden = grid.hidden;
    view! {
        <div class="table-wrap pivot-wrap">
            <table class="pivot">
                <thead>{head.into_iter().map(|row| view! { <tr>{row}</tr> }).collect_view()}</thead>
                <tbody>{body}</tbody>
                {foot}
            </table>
            {(hidden > 0).then(|| view! {
                <p class="quiet">{format!("{hidden} more columns not shown: select some to narrow it")}</p>
            })}
        </div>
    }
}
