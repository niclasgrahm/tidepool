//! A dashboard: its filter lists down the side, its visuals on the grid
//! from the YAML layout. Everything here reads the live [`Board`]; clicks
//! become selections on the server, which answers with a new frame.
//!
//! [`Board`]: crate::board::Board

use leptos::prelude::*;
use serde_json::{Value, json};
use tidepool_protocol::live::{FilterFrame, FilterInfo, ValueState, VisualFrame, VisualInfo};

use crate::api::{post_text, save_file};
use crate::board::{display, export_request, next_period, next_sort, number, wanted_window};
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
    if info.widget == "date_range" {
        view! { <DateRange field=info.field /> }.into_any()
    } else {
        view! { <FilterList field=info.field /> }.into_any()
    }
}

const ROW: f64 = 30.0;
const LIST_HEIGHT: f64 = 300.0;

/// A filter list: every value with its state (selected, possible,
/// excluded), searchable, only the rows on screen in the DOM, fetched from
/// the server a window at a time.
#[component]
fn FilterList(field: String) -> impl IntoView {
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
    Effect::new(move |_| {
        let top = scroll.get();
        let Some(frame) = frame.get() else { return };
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let first = (top / ROW).floor() as usize;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let visible = (LIST_HEIGHT / ROW).ceil() as usize + 1;
        if let Some((offset, limit)) = wanted_window(
            first,
            visible,
            (frame.offset, frame.values.len()),
            frame.total,
        ) {
            live.send(&window(offset, limit));
        }
    });
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
                    view! {
                        <ul style=format!("height: {height}px")>
                            {f.values.into_iter().enumerate().map(|(i, v)| {
                                let state = match v.state {
                                    ValueState::Selected => "selected",
                                    ValueState::Possible => "possible",
                                    ValueState::Excluded => "excluded",
                                };
                                #[allow(clippy::cast_precision_loss)]
                                let top = (f.offset + i) as f64 * ROW;
                                let (field, value) = (field.clone(), v.value.clone());
                                view! {
                                    <li style=format!("top: {top}px")>
                                        <button
                                            class=format!("value {state}")
                                            aria-pressed=(state == "selected").to_string()
                                            on:click=move |_| live.select(&field, std::slice::from_ref(&value), true)
                                        >
                                            <span>{display(&v.value)}</span>
                                            <span class="rows">{if v.rows > 0 { display(&json!(v.rows)) } else { String::new() }}</span>
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

/// A from/to range on a date or time field.
#[component]
fn DateRange(field: String) -> impl IntoView {
    let live = expect_context::<Live>();
    let key = field.clone();
    let current = Memo::new(move |_| {
        live.selection.with(|s| {
            s.iter()
                .find(|f| f.field == key)
                .map(|f| (f.from.clone(), f.to.clone()))
        })
    });
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
