//! The model browser's relationship diagram: a card per table, lines
//! between their key rows. Where everything goes is `diagram.rs`; this only
//! draws it, and keeps the numbers on the cards live.
//!
//! The structure (the model and the tables' columns) is read once and drawn
//! once; the numbers are polled and read through a `Memo` per card, so the
//! cards and lines that take the pointer are never rebuilt under it.

use std::collections::{HashMap, HashSet};

use leptos::prelude::*;
use serde_json::json;
use tidepool_protocol::{ModelInfo, RelationshipInfo, TableInfo};

use crate::api::get_json;
use crate::board::{bytes, display};
use crate::diagram::{self, Card, Line, Placed, RowKind};
use crate::live::Live;
use crate::route::{ConfigTab, Route};

/// What the pointer (or keyboard focus) is on.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Focus {
    Table(String),
    Line(usize),
}

/// How many polls of the write rate a sparkline shows.
const HISTORY: usize = 40;

#[component]
pub fn Relationships() -> impl IntoView {
    let live = expect_context::<Live>();
    let model = LocalResource::new(|| get_json::<ModelInfo>("/v1/model"));
    let shape = LocalResource::new(|| get_json::<Vec<TableInfo>>("/v1/tables"));
    // the numbers, every couple of seconds, as the status page does
    let tick = Memo::new(move |_| (live.clock.get() / 2000.0).floor());
    let numbers = LocalResource::new(move || {
        tick.track();
        get_json::<Vec<TableInfo>>("/v1/tables")
    });
    let history = RwSignal::new(HashMap::<String, Vec<f64>>::new());
    Effect::new(move |_| {
        if let Some(Ok(tables)) = numbers.get() {
            history.update(|h| {
                for t in tables {
                    let rates = h.entry(t.name).or_default();
                    rates.push(t.rows_per_second);
                    if rates.len() > HISTORY {
                        rates.remove(0);
                    }
                }
            });
        }
    });
    let latest = Memo::new(move |_| match numbers.get() {
        Some(Ok(tables)) => tables,
        _ => Vec::new(),
    });

    view! {
        <Suspense fallback=|| view! { <p class="quiet">"Loading…"</p> }>
            {move || match (model.get(), shape.get()) {
                (Some(Err(err)), _) | (_, Some(Err(err))) => view! { <p class="problem">{err}</p> }.into_any(),
                (Some(Ok(model)), Some(Ok(tables))) => view! {
                    <Diagram model=model tables=tables latest=latest history=history />
                }.into_any(),
                _ => ().into_any(),
            }}
        </Suspense>
    }
}

#[component]
fn Diagram(
    model: ModelInfo,
    tables: Vec<TableInfo>,
    latest: Memo<Vec<TableInfo>>,
    history: RwSignal<HashMap<String, Vec<f64>>>,
) -> impl IntoView {
    let both = model.filter_direction == "both";
    let cards = diagram::cards(&tables, &model);
    let placed = diagram::layout(&cards, &model.relationships);
    let lines = diagram::lines(&cards, &placed, &model.relationships);
    let (width, height) = diagram::extent(&placed);
    let relationships = StoredValue::new(model.relationships.clone());

    let focus = RwSignal::new(None::<Focus>);
    let chosen = RwSignal::new(None::<usize>);
    // what the focus lights: its lines, and the tables at their ends
    let lit = Memo::new(move |_| {
        let mut lines = HashSet::new();
        let mut tables = HashSet::new();
        relationships.with_value(|rels| match focus.get() {
            Some(Focus::Table(t)) => {
                for (i, r) in rels.iter().enumerate() {
                    if r.from == t || r.to == t {
                        lines.insert(i);
                        tables.insert(r.from.clone());
                        tables.insert(r.to.clone());
                    }
                }
                tables.insert(t);
            }
            Some(Focus::Line(i)) => {
                if let Some(r) = rels.get(i) {
                    lines.insert(i);
                    tables.insert(r.from.clone());
                    tables.insert(r.to.clone());
                }
            }
            None => {}
        });
        (lines, tables)
    });
    // which lines attach at each (table, row)
    let mut ports: HashMap<(String, String), Vec<usize>> = HashMap::new();
    for line in &lines {
        let r = &model.relationships[line.index];
        for (table, column) in [
            (&r.from, r.from_columns.first()),
            (&r.to, r.to_columns.first()),
        ] {
            if let Some(column) = column {
                ports
                    .entry((table.clone(), column.clone()))
                    .or_default()
                    .push(line.index);
            }
        }
    }

    let note = if both {
        view! { "This model filters both ways ("<code>"filter_direction: both"</code>"): a selection on any table narrows every table joined to it, in both directions." }.into_any()
    } else {
        view! { "Selections flow from the one side to the many side ("<code>"filter_direction: one_to_many"</code>", the default): selecting a customer filters orders, but selecting an order status leaves customers as they are." }.into_any()
    };

    view! {
        <div class="er-bar">
            <Legend />
            <p class="quiet er-note">{note}</p>
        </div>
        <div class="er-wrap">
            <div class="er-canvas">
                <div class="er-surface" class:dim=move || focus.get().is_some()
                     style=format!("width:{width}px;height:{height}px")>
                    <svg class="er-lines" width=width.to_string() height=height.to_string()
                         role="img" aria-label="How the tables relate">
                        {lines.into_iter().map(|line| view! {
                            <LineView line=line both=both relationships=relationships focus=focus chosen=chosen lit=lit />
                        }).collect_view()}
                    </svg>
                    {cards.into_iter().map(|card| {
                        let at = placed.get(&card.table).copied().unwrap_or(Placed { x: 0.0, y: 0.0, height: card.height });
                        let info = tables.iter().find(|t| t.name == card.table).cloned();
                        view! {
                            <CardView card=card at=at info=info latest=latest history=history
                                      focus=focus chosen=chosen lit=lit ports=ports.clone() />
                        }
                    }).collect_view()}
                </div>
            </div>
            {move || chosen.get().and_then(|i| relationships.with_value(|r| r.get(i).cloned())).map(|r| view! {
                <Detail r=r both=both close=move || chosen.set(None) />
            })}
        </div>
    }
}

/// A chevron centred on (x, y), pointing along `angle` degrees.
fn chevron(x: f64, y: f64, angle: f64) -> impl IntoView {
    view! { <path class="er-chev" d="M -4 -5 L 4 0 L -4 5 L -2 0 Z" transform=format!("translate({x} {y}) rotate({angle})") /> }
}

#[component]
fn Legend() -> impl IntoView {
    view! {
        <ul class="er-legend" aria-label="What the lines mean">
            <li><svg width="26" height="14" aria-hidden="true"><path class="er-end" d="M 2 7 H 24 M 24 1 L 12 7 L 24 13" /></svg>"many rows"</li>
            <li><svg width="26" height="14" aria-hidden="true"><path class="er-end" d="M 2 7 H 24 M 8 1 V 13" /></svg>"one row"</li>
            <li><svg width="22" height="14" aria-hidden="true"><path class="er-end" d="M 1 7 H 21" />{chevron(11.0, 7.0, 0.0)}</svg>"selections flow this way"</li>
            <li><svg width="30" height="14" aria-hidden="true"><path class="er-end" d="M 1 7 H 29" />{chevron(10.0, 7.0, 180.0)}{chevron(20.0, 7.0, 0.0)}</svg>"both ways"</li>
        </ul>
    }
}

#[component]
fn LineView(
    line: Line,
    both: bool,
    relationships: StoredValue<Vec<RelationshipInfo>>,
    focus: RwSignal<Option<Focus>>,
    chosen: RwSignal<Option<usize>>,
    lit: Memo<(HashSet<usize>, HashSet<String>)>,
) -> impl IntoView {
    let i = line.index;
    let Some(r) = relationships.with_value(|r| r.get(i).cloned()) else {
        return ().into_any();
    };
    let ((fx, fy), (tx, ty)) = (line.from, line.to);
    let (mx, my, angle) = line.mark;
    let (ux, uy) = (angle.to_radians().cos(), angle.to_radians().sin());
    // the marks point the way selections flow: from the one side to the many
    let marks = if both {
        view! { {chevron(mx - ux * 7.0, my - uy * 7.0, angle + 180.0)}{chevron(mx + ux * 7.0, my + uy * 7.0, angle)} }.into_any()
    } else {
        chevron(mx, my, angle + 180.0).into_any()
    };
    let (lx, ly, end) = line.label;
    let (one, other) = diagram::flow(&r, both);
    let mut name = r.name.clone();
    if let Some(role) = &r.role {
        name = format!("{name} ({role})");
    }
    let title = format!(
        "{}: many {} rows to one {} row, on {}. {one} {other}",
        r.name,
        r.from,
        r.to,
        r.from_columns.join(", ")
    );
    view! {
        <g class="er-line"
           class:lit=move || lit.with(|(l, _)| l.contains(&i))
           class:chosen=move || chosen.get() == Some(i)>
            <path class="er-path" d=line.path.clone() />
            <path class="er-flow back" d=line.path.clone() />
            {both.then(|| view! { <path class="er-flow" d=line.path.clone() /> })}
            <path class="er-end" d=format!("M {} {fy} L {fx} {} M {} {fy} L {fx} {} M {} {fy} L {fx} {fy}", fx - 14.0, fy - 6.0, fx - 14.0, fy + 6.0, fx - 14.0) />
            <path class="er-end" d=format!("M {} {} L {} {}", tx + 8.0, ty - 6.0, tx + 8.0, ty + 6.0) />
            {marks}
            <text x=lx.to_string() y=ly.to_string() text-anchor=if end { "end" } else { "start" }>{name}</text>
            <path class="er-hit" d=line.path
                  on:mouseenter=move |_| focus.set(Some(Focus::Line(i)))
                  on:mouseleave=move |_| focus.set(None)
                  on:click=move |_| chosen.update(|c| *c = if *c == Some(i) { None } else { Some(i) })>
                <title>{title}</title>
            </path>
        </g>
    }
    .into_any()
}

#[component]
#[allow(clippy::needless_pass_by_value)]
fn CardView(
    card: Card,
    at: Placed,
    info: Option<TableInfo>,
    latest: Memo<Vec<TableInfo>>,
    history: RwSignal<HashMap<String, Vec<f64>>>,
    focus: RwSignal<Option<Focus>>,
    chosen: RwSignal<Option<usize>>,
    lit: Memo<(HashSet<usize>, HashSet<String>)>,
    ports: HashMap<(String, String), Vec<usize>>,
) -> impl IntoView {
    let name = card.table.clone();
    let now = {
        let name = name.clone();
        let info = info.clone();
        Memo::new(move |_| {
            latest
                .with(|ts| ts.iter().find(|t| t.name == name).cloned())
                .or_else(|| info.clone())
        })
    };
    let mode = info.as_ref().map(|t| t.mode.clone()).unwrap_or_default();
    let retention = info.as_ref().and_then(|t| t.retention.clone());
    let rates = {
        let name = name.clone();
        Memo::new(move |_| history.with(|h| h.get(&name).cloned().unwrap_or_default()))
    };
    let rows = card.rows.into_iter().map(|row| {
        let attached = ports.get(&(name.clone(), row.name.clone())).cloned().unwrap_or_default();
        let lit_here = {
            let attached = attached.clone();
            move || lit.with(|(l, _)| attached.iter().any(|i| l.contains(i)))
        };
        let chosen_here = move || chosen.get().is_some_and(|c| attached.contains(&c));
        let (glyph, note) = match &row.kind {
            RowKind::Key => (view! { <KeyGlyph /> }.into_any(), String::new()),
            RowKind::Refers(to) => (view! { <svg width="12" height="12" viewBox="0 0 12 12" aria-label="refers to"><circle cx="6" cy="6" r="2.5" fill="currentColor"/></svg> }.into_any(), to.clone()),
            RowKind::Time => (view! { <svg width="12" height="12" viewBox="0 0 12 12" aria-label="event time"><circle cx="6" cy="6" r="4.5" fill="none" stroke="currentColor" stroke-width="1.3"/><path d="M6 3.5 V6 L7.8 7.2" stroke="currentColor" stroke-width="1.3" fill="none"/></svg> }.into_any(), "event time".into()),
            RowKind::Tree { parent, key } => (view! { <svg width="12" height="12" viewBox="0 0 12 12" aria-label="hierarchy"><path d="M3 2 V10 M3 6 H8 M3 10 H8" stroke="currentColor" stroke-width="1.3" fill="none"/><circle cx="3" cy="2" r="1.4" fill="currentColor"/></svg> }.into_any(), format!("{parent} to {key}")),
        };
        view! {
            <li class:port-lit=lit_here class:port-chosen=chosen_here>
                <span class="er-glyph">{glyph}</span>{row.name}<span class="er-row-note">{note}</span>
            </li>
        }
    }).collect_view();
    let more = card.more;
    let enter = {
        let name = name.clone();
        move || focus.set(Some(Focus::Table(name.clone())))
    };
    let enter2 = enter.clone();
    let lit_card = {
        let name = name.clone();
        move || lit.with(|(_, t)| t.contains(&name))
    };
    view! {
        <a class="er-card" class:lit=lit_card href=Route::Table(name.clone()).href()
           style=format!("left:{}px;top:{}px;height:{}px", at.x, at.y, at.height)
           aria-label=format!("{name}: open the table")
           on:mouseenter=move |_| enter()
           on:mouseleave=move |_| focus.set(None)
           on:focus=move |_| enter2()
           on:blur=move |_| focus.set(None)>
            <div class="er-head"><h3>{name.clone()}</h3><span class="er-mode">{mode}</span></div>
            <div class="er-stats">
                <div class="er-count">{move || now.get().map(|t| display(&json!(t.rows)))}<small>"rows"</small></div>
                <div class="er-rate">{move || {
                    let rate = now.get().map_or(0.0, |t| t.rows_per_second);
                    if rate > 0.0 {
                        view! {
                            <svg width="72" height="18" aria-hidden="true"><path d=diagram::sparkline(&rates.get(), 72.0, 18.0) /></svg>
                            <span class="er-live">{format!("{} rows/s", if rate < 10.0 { format!("{rate:.1}") } else { format!("{rate:.0}") })}</span>
                        }.into_any()
                    } else {
                        view! { <span>"no rows arriving"</span> }.into_any()
                    }
                }}</div>
            </div>
            <ul class="er-rows">
                {rows}
                {(more > 0).then(|| view! { <li class="er-more"><span class="er-glyph"></span>{format!("{more} more column{}", if more == 1 { "" } else { "s" })}</li> })}
            </ul>
            <div class="er-foot">
                {move || now.get().map(|t| view! {
                    <span>{format!("{} segment{}", t.segments, if t.segments == 1 { "" } else { "s" })}</span>
                    <span>{bytes(t.bytes)}</span>
                })}
                {retention.map(|r| view! { <span>{format!("keeps {r}")}</span> })}
            </div>
        </a>
    }
}

#[component]
fn KeyGlyph() -> impl IntoView {
    view! {
        <svg width="12" height="12" viewBox="0 0 12 12" aria-label="primary key">
            <circle cx="4" cy="6" r="2.6" fill="none" stroke="currentColor" stroke-width="1.4"/>
            <path d="M6.6 6 H11 M9.4 6 V8.2" stroke="currentColor" stroke-width="1.4" fill="none"/>
        </svg>
    }
}

#[component]
fn Detail(r: RelationshipInfo, both: bool, close: impl Fn() + 'static) -> impl IntoView {
    let (one, other) = diagram::flow(&r, both);
    let mark = |angle: f64, on: bool| {
        view! {
            <svg width="22" height="12" aria-hidden="true">
                <path class=if on { "er-end" } else { "er-end off" } d="M 1 6 H 21" />
                {on.then(|| chevron(11.0, 6.0, angle))}
            </svg>
        }
    };
    view! {
        <aside class="er-detail" aria-live="polite">
            <h3>{r.name.clone()}<button aria-label="Close" on:click=move |_| close()>"×"</button></h3>
            {r.role.clone().map(|role| view! { <p class="quiet">{format!("Role: {role}")}</p> })}
            <dl>
                <dt>"many"</dt><dd><code>{format!("{}.{}", r.from, r.from_columns.join(", "))}</code></dd>
                <dt>"one"</dt><dd><code>{format!("{}.{}", r.to, r.to_columns.join(", "))}</code></dd>
            </dl>
            <p class="er-flowline">{mark(0.0, true)}<span>{one}</span></p>
            <p class="er-flowline">{mark(180.0, both)}<span>{other}</span></p>
            <p><a href=Route::Config(ConfigTab::Edit).href()>"Edit the model"</a></p>
        </aside>
    }
}
