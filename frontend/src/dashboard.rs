//! A dashboard: its filter lists down the side, its visuals on the grid
//! from the YAML layout. Everything here reads the live [`Board`]; clicks
//! become selections on the server, which answers with a new frame.
//!
//! [`Board`]: crate::board::Board

use leptos::prelude::*;
use serde_json::{Value, json};
use tidepool_protocol::live::{FilterFrame, FilterInfo, VisualFrame, VisualInfo};

use crate::api::{post_text, save_file};
use crate::board::{
    beside, display, export_request, next_period, next_sort, node_class, number, presets,
    range_label, value_class, wanted_window,
};
use crate::charts::{bar, line, palette};
use crate::echarts::{Click, EChart};
use crate::live::Live;

/// Whether the page is in dark mode (charts can't read CSS).
fn dark() -> bool {
    web_sys::window()
        .and_then(|w| w.match_media("(prefers-color-scheme: dark)").ok().flatten())
        .is_some_and(|m| m.matches())
}

/// The part of `dataset.field` people read.
fn short(field: &str) -> String {
    field.split_once('.').map_or(field, |(_, f)| f).to_string()
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
    let info = Memo::new(move |_| {
        live.dashboards
            .with(|all| all.iter().find(|d| d.id == slug).cloned())
    });
    view! {
        {move || match info.get() {
            None => view! { <p class="quiet page-note">"Loading the dashboard…"</p> }.into_any(),
            Some(dashboard) => view! {
                <div class="page">
                    <aside class="rail" aria-label="Filters">
                        {dashboard.filters.into_iter().map(|f| view! { <Filter info=f /> }).collect_view()}
                    </aside>
                    <section
                        class="grid"
                        style=format!(
                            "grid-template-columns: repeat({}, minmax(0, 1fr)); grid-auto-rows: {}px;",
                            dashboard.columns, dashboard.row_height
                        )
                    >
                        {dashboard.visuals.into_iter().map(|v| view! { <Visual info=v /> }).collect_view()}
                    </section>
                </div>
            }
            .into_any(),
        }}
    }
}

#[component]
fn Filter(info: FilterInfo) -> impl IntoView {
    match info.widget.as_str() {
        "date_range" => view! { <DateRange field=info.field timed=info.timed /> }.into_any(),
        "tree" => view! { <FilterTree field=info.field measured=info.measure.is_some() format=info.measure_format /> }.into_any(),
        _ => view! { <FilterList field=info.field measured=info.measure.is_some() format=info.measure_format /> }.into_any(),
    }
}

/// Ask for the window of a list or tree the scroll position needs.
fn follow_scroll(
    live: Live,
    frame: Memo<Option<FilterFrame>>,
    scroll: RwSignal<f64>,
    window: impl Fn(usize, usize) -> Value + 'static,
) {
    Effect::new(move |_| {
        let top = scroll.get();
        let Some(frame) = frame.get() else { return };
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let first = (top / ROW).floor() as usize;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let visible = (LIST_HEIGHT / ROW).ceil() as usize + 1;
        let have = frame.values.len().max(frame.nodes.len());
        if let Some((offset, limit)) =
            wanted_window(first, visible, (frame.offset, have), frame.total)
        {
            live.send(&window(offset, limit));
        }
    });
}

const ROW: f64 = 30.0;
const LIST_HEIGHT: f64 = 300.0;

/// A filter list: every value with its state (selected, possible,
/// excluded), searchable, only the rows on screen in the DOM, fetched from
/// the server a window at a time.
#[component]
fn FilterList(field: String, measured: bool, format: Option<String>) -> impl IntoView {
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
    follow_scroll(live, frame, scroll, window);
    let on_search = move |event| {
        search.set(event_target_value(&event));
        scroll.set(0.0);
        live.send(&window(0, 120));
    };
    let title = short(&field);
    let select_field = field.clone();
    view! {
        <section class="filter">
            <h2>{title.clone()}</h2>
            <input type="search" placeholder="Search" aria-label=format!("Search {title}") on:input=on_search />
            <div
                class="values"
                style=move || {
                    // as tall as its values, up to a scrolling window
                    #[allow(clippy::cast_precision_loss)]
                    let rows = frame.with(|f| f.as_ref().map_or(0, |f| f.total)) as f64;
                    format!("height: {}px", (rows * ROW).min(LIST_HEIGHT))
                }
                on:scroll=move |ev| scroll.set(f64::from(event_target::<web_sys::Element>(&ev).scroll_top()))
            >
                {move || frame.get().map(|f: FilterFrame| {
                    #[allow(clippy::cast_precision_loss)]
                    let height = f.total as f64 * ROW;
                    let field = select_field.clone();
                    let format = format.clone();
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
                                            <span>{display(&v.value)}</span>
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
fn FilterTree(field: String, measured: bool, format: Option<String>) -> impl IntoView {
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
    follow_scroll(live, frame, scroll, window);
    let on_search = move |event| {
        search.set(event_target_value(&event));
        scroll.set(0.0);
        live.send(&window(0, 120));
    };
    let title = short(&field);
    view! {
        <section class="filter tree">
            <h2>{title.clone()}</h2>
            <input type="search" placeholder="Search" aria-label=format!("Search {title}") on:input=on_search />
            <div
                class="values"
                role="tree"
                aria-label=title.clone()
                style=move || {
                    #[allow(clippy::cast_precision_loss)]
                    let rows = frame.with(|f| f.as_ref().map_or(0, |f| f.total)) as f64;
                    format!("height: {}px", (rows * ROW).min(LIST_HEIGHT))
                }
                on:scroll=move |ev| scroll.set(f64::from(event_target::<web_sys::Element>(&ev).scroll_top()))
            >
                {move || frame.get().map(|f: FilterFrame| {
                    #[allow(clippy::cast_precision_loss)]
                    let height = f.total as f64 * ROW;
                    let format = format.clone();
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
                                        style=format!("top: {top}px; padding-left: {}px", node.depth * 14)
                                        role="treeitem"
                                        aria-expanded=has_children.then(|| open.to_string())
                                        attr:aria-level=(node.depth + 1).to_string()
                                    >
                                        {if has_children {
                                            view! {
                                                <button class="twist" aria-label=if open { "Close" } else { "Open" } on:click=twist>
                                                    {if open { "▾" } else { "▸" }}
                                                </button>
                                            }.into_any()
                                        } else {
                                            view! { <span class="twist" aria-hidden="true"></span> }.into_any()
                                        }}
                                        <button class=class aria-pressed=selected.to_string() on:click=select>
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

/// A from/to range on a date or time field, or a window relative to now
/// ("Last 7 days") that moves as time passes.
#[component]
fn DateRange(field: String, timed: bool) -> impl IntoView {
    let live = expect_context::<Live>();
    let key = field.clone();
    let selected = Memo::new(move |_| {
        live.selection
            .with(|s| s.iter().find(|f| f.field == key).cloned())
    });
    let current = Memo::new(move |_| selected.get().map(|f| (f.from, f.to)));
    let windows = presets(timed);
    // the period picker's value: a preset's index, "custom" or "" (all time)
    let period = {
        let windows = windows.clone();
        move || match selected.get() {
            None => String::new(),
            Some(s) => s
                .relative
                .and_then(|w| windows.iter().position(|p| *p == w))
                .map_or_else(|| "custom".to_string(), |i| i.to_string()),
        }
    };
    // a window that isn't a preset (a dashboard's default) by its name
    let custom = move || {
        selected.get().map_or_else(
            || "Custom".to_string(),
            |s| match s.relative {
                Some(w) => w.label(),
                None => range_label(s.from.as_ref(), s.to.as_ref()),
            },
        )
    };
    let choose = {
        let (field, windows) = (field.clone(), windows.clone());
        move |ev| {
            let value = event_target_value(&ev);
            if value.is_empty() {
                live.send(&json!({"type": "clear", "field": field}));
            } else if let Some(window) = value.parse::<usize>().ok().and_then(|i| windows.get(i)) {
                live.send(&json!({"type": "select_relative", "field": field, "window": window}));
            }
        }
    };
    let date = |v: &Option<Value>| {
        v.as_ref()
            .and_then(Value::as_str)
            .map(|s| s.get(..10).unwrap_or(s).to_string())
            .unwrap_or_default()
    };
    let from = RwSignal::new(String::new());
    let to = RwSignal::new(String::new());
    Effect::new(move |_| {
        let (f, t) = current.get().unwrap_or_default();
        from.set(date(&f));
        // the range is half-open; the picker shows the last day inside it
        to.set(date(&t));
    });
    let apply = {
        let field = field.clone();
        move || {
            let (f, t) = (from.get_untracked(), to.get_untracked());
            if f.is_empty() && t.is_empty() {
                live.send(&json!({"type": "clear", "field": field}));
            } else {
                live.send(&json!({
                    "type": "select_range", "field": field,
                    "from": (!f.is_empty()).then(|| format!("{f}T00:00:00Z")),
                    "to": (!t.is_empty()).then(|| format!("{t}T00:00:00Z")),
                }));
            }
        }
    };
    let apply_from = apply.clone();
    view! {
        <section class="filter">
            <h2>{short(&field)}</h2>
            <div class="range">
                <label>"Period"
                    <select prop:value=period on:change=choose>
                        <option value="">"All time"</option>
                        {windows.iter().enumerate().map(|(i, w)| view! {
                            <option value=i.to_string()>{w.label()}</option>
                        }).collect_view()}
                        <option value="custom" disabled=true>{custom}</option>
                    </select>
                </label>
                <label>"From" <input type="date" prop:value=move || from.get()
                    on:change=move |ev| { from.set(event_target_value(&ev)); apply_from(); } /></label>
                <label>"Until" <input type="date" prop:value=move || to.get()
                    on:change=move |ev| { to.set(event_target_value(&ev)); apply(); } /></label>
            </div>
        </section>
    }
}

#[component]
fn Visual(info: VisualInfo) -> AnyView {
    let live = expect_context::<Live>();
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
        + &info
            .dimensions
            .first()
            .map_or_else(String::new, |d| format!(" by {}", short(&d.field)));
    let class = format!("visual {}", info.visual_type);
    let dims = info.dimensions.len();
    let chart = matches!(info.visual_type.as_str(), "bar" | "line") && dims == 1;
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
        let content = if info.visual_type == "bar" {
            bar_chart(live, &info, rows)
        } else {
            line_chart(live, &info, rows)
        };
        return view! {
            <article class=class style=place>
                <header class="visual-head"><h2>{title.clone()}</h2>{export_button(live, &info)}</header>
                {note}
                {content}
            </article>
        }
        .into_any();
    }
    view! {
        <article class=class style=place>
            <header class="visual-head"><h2>{title.clone()}</h2>{export_button(live, &info)}</header>
            {move || match frame.get() {
                None => view! { <p class="quiet">"Waiting for data"</p> }.into_any(),
                Some(VisualFrame { error: Some(error), .. }) => {
                    view! { <p class="problem">{format!("Can't show this: {error}")}</p> }.into_any()
                }
                Some(VisualFrame { result: Some(result), compare, .. }) => {
                    body(live, &info, result.rows, compare.as_ref())
                }
                Some(_) => view! { <p class="quiet">"No result"</p> }.into_any(),
            }}
        </article>
    }
    .into_any()
}

/// "CSV": the visual's query under the current selection, downloaded.
fn export_button(live: Live, info: &VisualInfo) -> impl IntoView + use<> {
    let info = info.clone();
    let failed = RwSignal::new(None::<String>);
    let export = move |_| {
        let request = live.selection.with_untracked(|s| export_request(&info, s));
        let name = format!("{}.csv", info.id);
        leptos::task::spawn_local(async move {
            let outcome = match post_text("/v1/query?format=csv", &request).await {
                Ok((200, text)) => save_file(&name, &text, "text/csv"),
                Ok((status, text)) => Err(format!("the server answered {status}: {text}")),
                Err(err) => Err(err),
            };
            failed.set(outcome.err());
        });
    };
    view! {
        <button class="export" title="Download this visual's data as CSV, under the current selection" on:click=export>"CSV"</button>
        {move || failed.get().map(|e| view! { <span class="problem" role="alert">{e}</span> })}
    }
}

/// A visual's content, by type.
fn body(live: Live, info: &VisualInfo, rows: Vec<Vec<Value>>, compare: Option<&Value>) -> AnyView {
    match info.visual_type.as_str() {
        "kpi" => kpi(live, info, &rows, compare),
        _ => view! { <Table info=info.clone() rows=rows /> }.into_any(),
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
    view! {
        <p class="kpi-value" class:fresh=fresh>{display(&value)}</p>
        {delta.map(|d| view! {
            <p class="kpi-delta" class:down=d < 0.0>
                {format!("{}{d:.1}% vs {against}", if d >= 0.0 { "+" } else { "" })}
            </p>
        })}
    }
    .into_any()
}

fn bar_chart(live: Live, info: &VisualInfo, rows: Memo<Vec<Vec<Value>>>) -> AnyView {
    let field = info.dimensions[0].field.clone();
    let metrics = info.metrics.clone();
    let shown = field.clone();
    let option = Signal::derive(move || {
        // the selection colours the bars too
        let chosen = live.selection.with(|s| {
            s.iter()
                .find(|f| f.field == shown)
                .and_then(|f| f.values.as_ref())
                .map(|v| v.iter().map(display).collect::<Vec<_>>())
                .unwrap_or_default()
        });
        rows.with(|rows| bar(rows, &metrics, &chosen, palette(dark())))
    });
    let clickable = info.on_click;
    let on_click = Callback::new(move |click: Click| {
        if !clickable {
            return;
        }
        let value =
            rows.with_untracked(|rows| rows.get(click.data_index).and_then(|r| r.first()).cloned());
        if let Some(value) = value {
            live.select(&field, &[value], click.additive);
        }
    });
    view! { <div class="chart"><EChart option=option on_click=on_click /></div> }.into_any()
}

fn line_chart(live: Live, info: &VisualInfo, rows: Memo<Vec<Vec<Value>>>) -> AnyView {
    let metrics = info.metrics.clone();
    let option = Signal::derive(move || rows.with(|rows| line(rows, &metrics, palette(dark()))));
    let dimension = info.dimensions[0].clone();
    if !info.on_brush {
        return view! { <div class="chart"><EChart option=option /></div> }.into_any();
    }
    // a brushed span of periods selects their range: from the first one's
    // start to the start of the period after the last
    let on_brush = Callback::new(move |(start, end): (usize, usize)| {
        let at = |i: usize| {
            rows.with_untracked(|rows| {
                rows.get(i)
                    .and_then(|r| r.first())
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
        };
        let (Some(from), Some(last)) = (at(start), at(end.max(start))) else {
            return;
        };
        let to = next_period(&last, dimension.grain.as_deref().unwrap_or("day"));
        live.send(&json!({
            "type": "select_range", "field": dimension.field,
            "from": format!("{from}T00:00:00Z"),
            "to": to.map(|t| format!("{t}T00:00:00Z")),
        }));
    });
    view! { <div class="chart"><EChart option=option on_brush_x=on_brush /></div> }.into_any()
}

/// A table: sortable by any column (the server sorts), a row click selects
/// its first dimension's value, changed cells flash.
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
                                    view! { <td class:num=numeric class:fresh=fresh>{display(&v)}</td> }
                                }).collect_view()}
                            </tr>
                        }
                    }).collect_view()}
                </tbody>
            </table>
        </div>
    }
}
