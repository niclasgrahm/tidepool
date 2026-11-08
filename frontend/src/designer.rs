//! The dashboard editor: visuals placed on the dashboard's grid, moved by
//! dragging them and resized by dragging their corner (or with the arrow
//! keys, Shift to resize), always snapped to whole cells and never on top
//! of each other. A new visual starts from the fields and metrics it
//! shows; its type can be changed after.
//!
//! The editor edits the dashboard's file: it reads `dashboards/<id>.yaml`
//! with the server's own types, plans each draft on the server as it
//! changes (so a problem shows before saving), and saves through the same
//! config pipeline as every other change. Saving writes the file in the
//! examples' style; comments in it aren't kept, and the editor says so.

use std::collections::{BTreeMap, BTreeSet};

use indexmap::IndexMap;

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::{Value, json};
use tidepool_config::dashboard::{
    DashboardFile, DimensionRef, GridPosition as Rect, Layout, Visual, VisualType,
};
use tidepool_config::format::{MAX_DECIMALS, NumberFormat, Style};
use tidepool_config::ossie::TimeGrain;
use tidepool_protocol::config::{ConfigState, PlanInfo};
use tidepool_protocol::{ModelInfo, QueryResult};
use wasm_bindgen::JsCast;

use crate::api::{get_json, post_json};
use crate::board::{display, formatted};
use crate::charts::Data;
use crate::configui::{SignIn, error_of, sleep};
use crate::design::{
    Reach, TYPES, default_type, fits, format_of, formats_of, retype, set_fields, set_format, short,
    size_of, title, type_label, visual_id,
};
use crate::layout::{cells, depth, free, moved, nudge, place, resized};
use crate::plot::{BarChart, HeatChart, LineChart, PieChart, ScatterChart};
use crate::route::Route;

/// The CSS gap between grid cells, in pixels (`.grid`).
const GAP: f64 = 16.0;

/// A drag in progress: which visual, moving or resizing, where it started
/// (rectangle and pointer) and the cell size then, and where it would land.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Drag {
    index: usize,
    resize: bool,
    start: Rect,
    pointer: (f64, f64),
    cell: (f64, f64),
    now: Rect,
}

/// The fields being chosen for a visual: a new one (`target: None`) or one
/// on the grid.
#[derive(Clone, Debug, PartialEq)]
struct Pick {
    target: Option<usize>,
    dimensions: Vec<DimensionRef>,
    metrics: Vec<String>,
}

/// The plan of the draft, as the server sees it.
#[derive(Clone, Debug, PartialEq)]
enum Checked {
    Nothing,
    Checking,
    Plan(PlanInfo),
    Failed(String),
}

/// Go to a page.
fn go(route: &Route) {
    if let Some(window) = web_sys::window() {
        let _ = window.location().set_hash(&route.href());
    }
}

/// Whether the person confirms `question`.
fn confirm(question: &str) -> bool {
    web_sys::window().is_some_and(|w| w.confirm_with_message(question).unwrap_or(false))
}

/// The grid placement of a rectangle.
fn place_style(at: Rect) -> String {
    format!(
        "grid-column: {} / span {}; grid-row: {} / span {};",
        at.x + 1,
        at.w,
        at.y + 1,
        at.h
    )
}

/// The editor: `slug` is the dashboard to edit, `None` a new one.
#[component]
pub fn Designer(slug: Option<String>) -> impl IntoView {
    let reload = RwSignal::new(0u64);
    let config = LocalResource::new(move || {
        reload.track();
        get_json::<ConfigState>("/v1/config")
    });
    let model = LocalResource::new(move || {
        reload.track();
        get_json::<ModelInfo>("/v1/model")
    });
    let editing = slug;
    view! {
        <Suspense fallback=|| view! { <p class="quiet page-note">"Loading…"</p> }>
            {move || {
                let (config, model) = (config.get()?, model.get()?);
                let editing = editing.clone();
                Some(match (config, model) {
                    (Err(problem), _) | (_, Err(problem)) => view! {
                        <main class="page-single">
                            <h1 class="page-title">"Edit dashboard"</h1>
                            <SignIn on_done=Callback::new(move |()| reload.update(|n| *n += 1)) problem=problem />
                        </main>
                    }.into_any(),
                    (Ok(config), Ok(model)) => start(&config, model, editing),
                })
            }}
        </Suspense>
    }
}

/// The editor on its file, or why it can't open.
fn start(config: &ConfigState, model: ModelInfo, slug: Option<String>) -> AnyView {
    let taken: BTreeSet<String> = config
        .files
        .iter()
        .filter_map(|f| f.path.strip_prefix("dashboards/")?.strip_suffix(".yaml"))
        .map(str::to_string)
        .collect();
    let (file, original) = match &slug {
        None => (
            DashboardFile {
                name: String::new(),
                layout: Layout::default(),
                filters: Vec::new(),
                visuals: Vec::new(),
            },
            None,
        ),
        Some(slug) => {
            let path = format!("dashboards/{slug}.yaml");
            let Some(text) = config
                .files
                .iter()
                .find(|f| f.path == path)
                .map(|f| f.text.clone())
            else {
                return view! {
                    <main class="page-single">
                        <h1 class="page-title">"Edit dashboard"</h1>
                        <p class="problem">{format!("There is no {path}.")}</p>
                    </main>
                }
                .into_any();
            };
            match DashboardFile::from_yaml(&text) {
                Ok(file) => (file, Some(text)),
                Err(err) => {
                    return view! {
                        <main class="page-single">
                            <h1 class="page-title">"Edit dashboard"</h1>
                            <p class="problem">{format!("{path} can't be read: {err}. Fix it in the config editor first.")}</p>
                            <p><a href=Route::Config(crate::route::ConfigTab::Edit).href()>"Open the config editor"</a></p>
                        </main>
                    }
                    .into_any();
                }
            }
        }
    };
    view! { <Editor file=file original=original slug=slug taken=taken version=config.version model=model /> }
        .into_any()
}

#[component]
#[allow(clippy::too_many_lines)] // one page: toolbar, grid, side panel
fn Editor(
    file: DashboardFile,
    original: Option<String>,
    slug: Option<String>,
    taken: BTreeSet<String>,
    version: u64,
    model: ModelInfo,
) -> impl IntoView {
    // what's saved now, to tell whether anything changed (the text can
    // differ by comments and spacing alone)
    let saved = StoredValue::new(original.as_ref().map(|_| file.clone()));
    let draft = RwSignal::new(file);
    let selected = RwSignal::new(None::<usize>);
    let picking = RwSignal::new(None::<Pick>);
    let drag = RwSignal::new(None::<Drag>);
    let checked = RwSignal::new(Checked::Nothing);
    let outcome = RwSignal::new(None::<Result<String, String>>);
    let generation = StoredValue::new(0u64);
    let canvas = NodeRef::<leptos::html::Section>::new();
    let reach = StoredValue::new(Reach::of(&model));
    // each metric's own format, which a visual's says again or not
    let own = StoredValue::new(
        model
            .metrics
            .iter()
            .filter_map(|m| Some((m.name.clone(), m.format.clone()?)))
            .collect::<BTreeMap<String, String>>(),
    );
    let model = StoredValue::new(model);
    let commented = original
        .as_deref()
        .is_some_and(|text| text.lines().any(|l| l.contains('#')));
    let is_new = slug.is_none();

    // the file this saves: the dashboard's, or one named after the title
    let path = {
        let slug = slug.clone();
        Memo::new(move |_| match &slug {
            Some(slug) => format!("dashboards/{slug}.yaml"),
            None => format!(
                "dashboards/{}.yaml",
                draft.with(|d| crate::design::slug(&d.name, &taken))
            ),
        })
    };
    let text = Memo::new(move |_| draft.with(DashboardFile::to_yaml));
    let changed = Memo::new(move |_| draft.with(|d| saved.with_value(|s| s.as_ref() != Some(d))));
    let is_time = move |field: &str| -> bool {
        let Some((dataset, name)) = field.split_once('.') else {
            return false;
        };
        model.with_value(|m| {
            m.datasets
                .iter()
                .find(|d| d.name == dataset)
                .and_then(|d| d.fields.iter().find(|f| f.name == name))
                .is_some_and(|f| f.is_time)
        })
    };
    let rects = move || draft.with(|d| d.visuals.iter().map(|v| v.at).collect::<Vec<_>>());
    let others = move |index: usize| {
        draft.with(|d| {
            d.visuals
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != index)
                .map(|(_, v)| v.at)
                .collect::<Vec<_>>()
        })
    };
    let columns = move || draft.with(|d| d.layout.columns);

    // plan the draft a moment after the last change
    Effect::new(move |_| {
        let (path, text, changed) = (path.get(), text.get(), changed.get());
        let named = draft.with(|d| !d.name.trim().is_empty());
        let n = generation.get_value() + 1;
        generation.set_value(n);
        if !changed || !named {
            checked.set(Checked::Nothing);
            return;
        }
        checked.set(Checked::Checking);
        spawn_local(async move {
            sleep(400).await;
            if generation.get_value() != n {
                return;
            }
            let reply = post_json(
                "/v1/config/plan",
                &json!({ "base_version": version, "files": { path: text } }),
            )
            .await;
            if generation.get_value() != n {
                return;
            }
            checked.set(match reply {
                Ok((200, value)) => match serde_json::from_value::<PlanInfo>(value) {
                    Ok(plan) => Checked::Plan(plan),
                    Err(err) => Checked::Failed(err.to_string()),
                },
                Ok((_, value)) => Checked::Failed(error_of(&value)),
                Err(err) => Checked::Failed(err),
            });
        });
    });

    let save = move |_| {
        let (path, text) = (path.get_untracked(), text.get_untracked());
        let slug = path
            .trim_start_matches("dashboards/")
            .trim_end_matches(".yaml")
            .to_string();
        outcome.set(Some(Ok("Saving…".into())));
        spawn_local(async move {
            let reply = post_json(
                "/v1/config/apply",
                &json!({
                    "base_version": version, "files": { path: text },
                    "author": "the dashboard editor",
                }),
            )
            .await;
            match reply {
                Ok((200, value)) if value["status"] == "queued" => outcome.set(Some(Ok(format!(
                    "Proposed: change {} is waiting for approval.",
                    value["change"]
                )))),
                Ok((200, _)) => {
                    outcome.set(None);
                    go(&Route::Dashboard(slug));
                }
                Ok((409, value)) => outcome.set(Some(Err(format!(
                    "{} Someone changed the config since this editor opened; reload it to start from their version.",
                    error_of(&value)
                )))),
                Ok((_, value)) => outcome.set(Some(Err(error_of(&value)))),
                Err(err) => outcome.set(Some(Err(err))),
            }
        });
    };
    let remove_dashboard = move |_| {
        if !confirm("Remove this dashboard? Its file is removed from the config.") {
            return;
        }
        let path = path.get_untracked();
        spawn_local(async move {
            let reply = post_json(
                "/v1/config/apply",
                &json!({
                    "base_version": version, "files": { path: Value::Null },
                    "author": "the dashboard editor",
                }),
            )
            .await;
            match reply {
                Ok((200, _)) => go(&Route::Home),
                Ok((_, value)) => outcome.set(Some(Err(error_of(&value)))),
                Err(err) => outcome.set(Some(Err(err))),
            }
        });
    };
    let leave = {
        let slug = slug.clone();
        move |_| {
            if changed.get_untracked() && !confirm("Leave without saving your changes?") {
                return;
            }
            go(&slug.clone().map_or(Route::Home, Route::Dashboard));
        }
    };

    // what a picked set of fields does: a new visual in the first gap, or
    // the chosen one changed
    let done_picking = Callback::new(move |pick: Pick| {
        picking.set(None);
        if let Some(index) = pick.target {
            draft.update(|d| {
                if let Some(v) = d.visuals.get_mut(index) {
                    set_fields(v, pick.dimensions, pick.metrics, &is_time);
                }
            });
        } else {
            let t = default_type(&pick.dimensions, pick.metrics.len());
            let (w, h) = size_of(t);
            draft.update(|d| {
                let taken: BTreeSet<String> = d.visuals.iter().map(|v| v.id.clone()).collect();
                let at = place(
                    w,
                    h,
                    &d.visuals.iter().map(|v| v.at).collect::<Vec<_>>(),
                    d.layout.columns,
                );
                let mut visual = Visual {
                    id: visual_id(&pick.metrics, &pick.dimensions, &taken),
                    visual_type: t,
                    title: None,
                    description: None,
                    dimensions: Vec::new(),
                    metrics: Vec::new(),
                    compare: None,
                    sort: None,
                    limit: None,
                    on_click: None,
                    on_brush: None,
                    format: IndexMap::new(),
                    at,
                };
                set_fields(&mut visual, pick.dimensions, pick.metrics, &is_time);
                d.visuals.push(visual);
                selected.set(Some(d.visuals.len() - 1));
            });
        }
    });

    // dragging: the canvas follows the pointer captured by a tile
    let start_drag = move |ev: &web_sys::PointerEvent, index: usize, resize: bool| {
        if ev.button() != 0 {
            return;
        }
        let Some(area) = canvas.get_untracked() else {
            return;
        };
        let Some(start) = draft.with_untracked(|d| d.visuals.get(index).map(|v| v.at)) else {
            return;
        };
        let width = area.get_bounding_client_rect().width();
        let (cols, row_height) = draft.with_untracked(|d| (d.layout.columns, d.layout.row_height));
        let cell = (
            (width + GAP) / f64::from(cols.max(1)),
            f64::from(row_height) + GAP,
        );
        if let Some(target) = ev
            .current_target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
        {
            let _ = target.set_pointer_capture(ev.pointer_id());
        }
        ev.prevent_default();
        selected.set(Some(index));
        drag.set(Some(Drag {
            index,
            resize,
            start,
            pointer: (f64::from(ev.client_x()), f64::from(ev.client_y())),
            cell,
            now: start,
        }));
    };
    let follow = move |ev: web_sys::PointerEvent| {
        let Some(mut d) = drag.get_untracked() else {
            return;
        };
        let dx = cells(f64::from(ev.client_x()) - d.pointer.0, d.cell.0);
        let dy = cells(f64::from(ev.client_y()) - d.pointer.1, d.cell.1);
        let now = if d.resize {
            resized(d.start, dx, dy, columns())
        } else {
            moved(d.start, dx, dy, columns())
        };
        if now != d.now {
            d.now = now;
            drag.set(Some(d));
        }
    };
    let drop_it = move |_| {
        let Some(d) = drag.get_untracked() else {
            return;
        };
        drag.set(None);
        if d.now != d.start && free(d.now, &others(d.index), columns()) {
            draft.update(|f| {
                if let Some(v) = f.visuals.get_mut(d.index) {
                    v.at = d.now;
                }
            });
        }
    };
    let on_key = move |ev: web_sys::KeyboardEvent, index: usize| {
        let key = ev.key();
        if key == "Delete" {
            ev.prevent_default();
            remove_visual(draft, selected, index);
            return;
        }
        if key == "Enter" {
            open_fields(draft, picking, index);
            return;
        }
        let Some(at) = draft.with_untracked(|d| d.visuals.get(index).map(|v| v.at)) else {
            return;
        };
        if let Some(next) = nudge(at, &key, ev.shift_key(), columns()) {
            ev.prevent_default();
            if free(next, &others(index), columns()) {
                draft.update(|d| d.visuals[index].at = next);
            }
        }
    };

    let tiles = move || {
        let count = draft.with(|d| d.visuals.len());
        (0..count)
            .map(|index| {
                let visual = Memo::new(move |_| draft.with(|d| d.visuals.get(index).cloned()));
                let at = move || visual.with(|v| v.as_ref().map(|v| v.at));
                let name = move || {
                    visual.with(|v| v.as_ref().map(|v| title(&v.metrics, &v.dimensions)).unwrap_or_default())
                };
                view! {
                    <article
                        class="visual tile"
                        class:chosen=move || selected.get() == Some(index)
                        class:moving=move || drag.with(|d| d.is_some_and(|d| d.index == index))
                        style=move || at().map(place_style).unwrap_or_default()
                        tabindex="0"
                        aria-label=move || format!("{}: arrows move it, Shift and arrows resize it, Enter changes its fields, Delete removes it", name())
                        on:pointerdown=move |ev| start_drag(&ev, index, false)
                        on:focus=move |_| selected.set(Some(index))
                        on:keydown=move |ev| on_key(ev, index)
                    >
                        <header class="visual-head">
                            <h2>{name}</h2>
                            <span class="type-tag">{move || visual.with(|v| v.as_ref().map(|v| type_label(v.visual_type)).unwrap_or_default())}</span>
                        </header>
                        <Preview visual=visual own=own />
                        <span
                            class="handle"
                            aria-hidden="true"
                            title="Drag to resize"
                            on:pointerdown=move |ev| { ev.stop_propagation(); start_drag(&ev, index, true); }
                        ></span>
                    </article>
                }
            })
            .collect_view()
    };
    let background = move || {
        let cols = columns();
        let shown = depth(&rects()).max(drag.with(|d| d.map_or(0, |d| d.now.y + d.now.h)));
        let rows = (shown + 3).max(6);
        (0..rows)
            .flat_map(|y| (0..cols).map(move |x| (x, y)))
            .map(|(x, y)| {
                view! {
                    <div
                        class="cell"
                        aria-hidden="true"
                        style=place_style(Rect { x, y, w: 1, h: 1 })
                        on:pointerdown=move |_| selected.set(None)
                    ></div>
                }
            })
            .collect_view()
    };
    let ghost = move || {
        drag.get().filter(|d| d.now != d.start).map(|d| {
            let blocked = !free(d.now, &others(d.index), columns());
            view! { <div class="ghost" class:blocked=blocked style=place_style(d.now)></div> }
        })
    };

    let panel = move || {
        if let Some(pick) = picking.get() {
            return view! { <Picker pick=pick model=model reach=reach on_done=done_picking on_cancel=Callback::new(move |()| picking.set(None)) /> }.into_any();
        }
        match selected.get().and_then(|i| draft.with(|d| d.visuals.get(i).cloned()).map(|v| (i, v))) {
            Some((index, visual)) => view! { <Properties index=index visual=visual own=own draft=draft selected=selected picking=picking is_time=Callback::new(move |f: String| is_time(&f)) /> }.into_any(),
            None => view! {
                <div class="panel-note">
                    <h2>"Visuals"</h2>
                    <p class="quiet">"Add a visual, then drag it where it goes; drag its corner to resize it. It snaps to the grid and can't cover another. A visual's arrow keys move it, Shift and the arrows resize it."</p>
                    <p class="quiet">"One dimension on its own makes a filter: a list to select values from. Anything else starts as a table; change its type after."</p>
                </div>
            }.into_any(),
        }
    };
    let plan_view = move || match checked.get() {
        Checked::Nothing => ().into_any(),
        Checked::Checking => view! { <p class="quiet">"Checking…"</p> }.into_any(),
        Checked::Failed(message) => view! { <p class="problem">{message}</p> }.into_any(),
        Checked::Plan(plan) if plan.valid => {
            view! { <p class="ok">"Ready to save."</p> }.into_any()
        }
        Checked::Plan(plan) => view! {
            <h3>"Problems"</h3>
            <ul class="problems">
                {plan.problems.into_iter().map(|p| view! {
                    <li class=p.severity.clone()>
                        {p.message}
                        {p.help.map(|h| view! { <span class="quiet">" — "{h}</span> })}
                    </li>
                }).collect_view()}
            </ul>
        }
        .into_any(),
    };
    let can_save = move || {
        changed.get()
            && draft.with(|d| !d.name.trim().is_empty())
            && matches!(checked.get(), Checked::Plan(plan) if plan.valid)
    };
    let heading = if is_new {
        "New dashboard"
    } else {
        "Edit dashboard"
    };

    view! {
        <div class="toolbar designer-bar">
            <h1>{heading}</h1>
            <label class="name">
                <span class="quiet">"Title"</span>
                <input
                    type="text"
                    placeholder="Sales overview"
                    prop:value=move || draft.with(|d| d.name.clone())
                    on:input=move |ev| draft.update(|d| d.name = event_target_value(&ev))
                />
            </label>
            <button class="primary" on:click=move |_| picking.set(Some(Pick { target: None, dimensions: Vec::new(), metrics: Vec::new() }))>"Add visual"</button>
            <span class="spacer"></span>
            {move || outcome.get().map(|o| match o {
                Ok(m) => view! { <span class="ok">{m}</span> }.into_any(),
                Err(m) => view! { <span class="problem" role="alert">{m}</span> }.into_any(),
            })}
            {(!is_new).then(|| view! { <button on:click=remove_dashboard>"Remove dashboard"</button> })}
            <button on:click=leave>"Cancel"</button>
            <button class="primary" disabled=move || !can_save() on:click=save>"Save"</button>
        </div>
        <div class="page designer">
            <section
                class="grid canvas"
                node_ref=canvas
                style=move || draft.with(|d| format!(
                    "grid-template-columns: repeat({}, minmax(0, 1fr)); grid-auto-rows: {}px;",
                    d.layout.columns, d.layout.row_height
                ))
                on:pointermove=follow
                on:pointerup=drop_it
                on:pointercancel=move |_| drag.set(None)
            >
                {background}
                {tiles}
                {ghost}
                {move || draft.with(|d| d.visuals.is_empty()).then(|| view! {
                    <p class="canvas-empty quiet">"No visuals yet. "
                        <button class="link" on:click=move |_| picking.set(Some(Pick { target: None, dimensions: Vec::new(), metrics: Vec::new() }))>"Add one"</button>
                    </p>
                })}
            </section>
            <aside class="side" aria-label="Visual">
                {panel}
                <div class="plan" aria-live="polite">
                    {move || path.with(|p| view! { <p class="quiet">"Saves "<code>{p.clone()}</code></p> })}
                    {commented.then(|| view! { <p class="notice">"This file has comments. Saving rewrites it, and they aren't kept."</p> })}
                    {plan_view}
                </div>
            </aside>
        </div>
    }
}

fn remove_visual(draft: RwSignal<DashboardFile>, selected: RwSignal<Option<usize>>, index: usize) {
    selected.set(None);
    draft.update(|d| {
        if index < d.visuals.len() {
            d.visuals.remove(index);
        }
    });
}

fn open_fields(draft: RwSignal<DashboardFile>, picking: RwSignal<Option<Pick>>, index: usize) {
    if let Some(v) = draft.with_untracked(|d| d.visuals.get(index).cloned()) {
        picking.set(Some(Pick {
            target: Some(index),
            dimensions: v.dimensions,
            metrics: v.metrics,
        }));
    }
}

/// The chosen visual: its type (those that fit what it shows), its fields,
/// its place, and removing it.
#[component]
fn Properties(
    index: usize,
    visual: Visual,
    own: StoredValue<BTreeMap<String, String>>,
    draft: RwSignal<DashboardFile>,
    selected: RwSignal<Option<usize>>,
    picking: RwSignal<Option<Pick>>,
    is_time: Callback<String, bool>,
) -> impl IntoView {
    let types = TYPES
        .iter()
        .map(|t| {
            let t = *t;
            let ok = fits(t, &visual.dimensions, visual.metrics.len());
            let current = visual.visual_type == t;
            view! {
                <button
                    class="type"
                    class:current=current
                    aria-pressed=current.to_string()
                    disabled=!ok
                    title=if ok { String::new() } else { why_not(t) }
                    on:click=move |_| draft.update(|d| {
                        if let Some(v) = d.visuals.get_mut(index) {
                            retype(v, t, &|f| is_time.run(f.to_string()));
                        }
                    })
                >{type_label(t)}</button>
            }
        })
        .collect_view();
    let dims: Vec<String> = visual
        .dimensions
        .iter()
        .map(|d| match d.grain() {
            Some(g) => format!("{} by {}", d.field(), format!("{g:?}").to_lowercase()),
            None => d.field().to_string(),
        })
        .collect();
    let at = visual.at;
    view! {
        <div class="properties">
            <h2>{title(&visual.metrics, &visual.dimensions)}</h2>
            <p class="quiet"><code>{visual.id.clone()}</code></p>
            <h3>"Type"</h3>
            <div class="types" role="group" aria-label="Type">{types}</div>
            <h3>"Shows"</h3>
            <dl class="shows">
                <dt>"Dimensions"</dt>
                <dd>{if dims.is_empty() { "none".to_string() } else { dims.join(", ") }}</dd>
                <dt>"Metrics"</dt>
                <dd>{if visual.metrics.is_empty() { "none".to_string() } else { visual.metrics.join(", ") }}</dd>
            </dl>
            {formats(index, &visual, own, draft)}
            <p class="quiet">{format!("Column {}, row {}; {} wide, {} tall", at.x + 1, at.y + 1, at.w, at.h)}</p>
            <div class="actions">
                <button on:click=move |_| open_fields(draft, picking, index)>"Change fields"</button>
                <button on:click=move |_| remove_visual(draft, selected, index)>"Remove"</button>
            </div>
        </div>
    }
}

/// The styles a format offers, by their word in the file.
const STYLES: [(&str, &str); 5] = [
    ("", "As it comes"),
    ("int", "Whole number"),
    ("decimal", "Decimal"),
    ("percent", "Percent"),
    ("currency", "Currency"),
];

fn style_word(style: &Style) -> &'static str {
    match style {
        Style::Plain => "",
        Style::Int => "int",
        Style::Decimal => "decimal",
        Style::Percent => "percent",
        Style::Currency(_) => "currency",
    }
}

/// How each metric the visual shows reads in it: a style, compact,
/// decimals, and what a value then looks like. The metric's own format
/// applies until this says something else.
#[allow(clippy::too_many_lines)] // one row of controls per metric, and what each changes
fn formats(
    index: usize,
    visual: &Visual,
    own: StoredValue<BTreeMap<String, String>>,
    draft: RwSignal<DashboardFile>,
) -> impl IntoView + use<> {
    let shown: Vec<String> = visual
        .metrics
        .iter()
        .chain(&visual.compare)
        .cloned()
        .collect();
    if shown.is_empty() {
        return ().into_any();
    }
    let rows = shown
        .into_iter()
        .map(|metric| {
            let inherited = own.with_value(|o| o.get(&metric).cloned());
            let current = format_of(visual, &metric, inherited.as_deref());
            let overridden = visual.format.contains_key(&metric);
            let change = {
                let (metric, inherited) = (metric.clone(), inherited.clone());
                move |edit: &dyn Fn(&mut NumberFormat)| {
                    draft.update(|d| {
                        if let Some(v) = d.visuals.get_mut(index) {
                            let mut format = format_of(v, &metric, inherited.as_deref());
                            edit(&mut format);
                            set_format(v, &metric, &format, inherited.as_deref());
                        }
                    });
                }
            };
            let on_style = {
                let change = change.clone();
                move |ev: leptos::ev::Event| {
                    let word = event_target_value(&ev);
                    change(&|f: &mut NumberFormat| {
                        f.style = match word.as_str() {
                            "int" => Style::Int,
                            "decimal" => Style::Decimal,
                            "percent" => Style::Percent,
                            "currency" => match &f.style {
                                Style::Currency(code) => Style::Currency(code.clone()),
                                _ => Style::Currency("EUR".into()),
                            },
                            _ => Style::Plain,
                        };
                    });
                }
            };
            let on_code = {
                let change = change.clone();
                move |ev: leptos::ev::Event| {
                    let code = event_target_value(&ev).trim().to_ascii_uppercase();
                    if code.len() == 3 && code.chars().all(|c| c.is_ascii_uppercase()) {
                        change(&|f: &mut NumberFormat| f.style = Style::Currency(code.clone()));
                    }
                }
            };
            let on_compact = {
                let change = change.clone();
                move |ev: leptos::ev::Event| {
                    let on = event_target_checked(&ev);
                    change(&|f: &mut NumberFormat| f.compact = on);
                }
            };
            let on_decimals = {
                let change = change.clone();
                move |ev: leptos::ev::Event| {
                    let places = event_target_value(&ev).parse::<u8>().ok();
                    change(&|f: &mut NumberFormat| f.decimals = places);
                }
            };
            let reset = {
                let metric = metric.clone();
                move |_| {
                    draft.update(|d| {
                        if let Some(v) = d.visuals.get_mut(index) {
                            v.format.shift_remove(&metric);
                        }
                    });
                }
            };
            let sample = if current.style == Style::Percent {
                Value::from(0.4567)
            } else {
                Value::from("1234567.891")
            };
            let example = formatted(&sample, Some(&current.to_string()));
            let code = match &current.style {
                Style::Currency(code) => Some(code.clone()),
                _ => None,
            };
            let word = style_word(&current.style);
            let decimals = current.decimals;
            let source = if overridden {
                "set here".to_string()
            } else {
                match &inherited {
                    Some(f) => format!("the metric's: {f}"),
                    None => "as it comes".to_string(),
                }
            };
            view! {
                <div class="format-row" data-metric=metric.clone()>
                    <div class="format-head">
                        <code>{metric.clone()}</code>
                        <span class="quiet">{source}</span>
                        {overridden.then(|| view! {
                            <button class="ghost-button" on:click=reset title="Read as the metric says">"Reset"</button>
                        })}
                    </div>
                    <div class="format-controls">
                        <select class="format-style" aria-label=format!("How {metric} reads") on:change=on_style>
                            {STYLES.iter().map(|(w, label)| view! { <option value=*w selected=*w == word>{*label}</option> }).collect_view()}
                        </select>
                        {code.map(|code| view! {
                            <input class="format-code" aria-label="Currency code" maxlength="3" spellcheck="false"
                                prop:value=code on:change=on_code />
                        })}
                        <select class="format-decimals" aria-label="Decimals" on:change=on_decimals>
                            <option value="" selected=decimals.is_none()>"Auto"</option>
                            {(0..=MAX_DECIMALS).map(|n| view! {
                                <option value=n.to_string() selected=decimals == Some(n)>{format!("{n} dp")}</option>
                            }).collect_view()}
                        </select>
                        <label class="format-compact">
                            <input type="checkbox" prop:checked=current.compact on:change=on_compact />
                            "Compact"
                        </label>
                    </div>
                    <p class="format-example">"e.g. "<span>{example}</span></p>
                </div>
            }
        })
        .collect_view();
    view! {
        <h3>"Format"</h3>
        <div class="formats">{rows}</div>
    }
    .into_any()
}

/// Why a type isn't offered for what a visual shows.
fn why_not(t: VisualType) -> String {
    match t {
        VisualType::Table => "A table shows dimensions, metrics or both",
        VisualType::Filter => "A filter lists one dimension's values, without metrics",
        VisualType::Bar | VisualType::Line => "A chart shows metrics by one dimension",
        VisualType::Kpi => "A KPI shows metrics, without dimensions",
        VisualType::Rows => "Rows show fields of each row, without metrics or grains",
        VisualType::Pie => "A pie shows one metric by one dimension",
        VisualType::Scatter => "A scatter shows two or three metrics by one dimension",
        VisualType::Heatmap => "A heatmap shows one metric by two dimensions",
    }
    .to_string()
}

/// Choosing a visual's dimensions and metrics. Those that can't go with
/// what's chosen already are greyed out: no relationship leads there.
#[component]
fn Picker(
    pick: Pick,
    model: StoredValue<ModelInfo>,
    reach: StoredValue<Reach>,
    on_done: Callback<Pick>,
    on_cancel: Callback<()>,
) -> impl IntoView {
    let target = pick.target;
    let dims = RwSignal::new(pick.dimensions);
    let metrics = RwSignal::new(pick.metrics);
    let search = RwSignal::new(String::new());
    let matches = move |text: &str| {
        let wanted = search.get().to_lowercase();
        wanted.is_empty() || text.to_lowercase().contains(&wanted)
    };
    let toggle_field = move |field: String, time: bool| {
        dims.update(|d| {
            if let Some(i) = d.iter().position(|x| x.field() == field) {
                d.remove(i);
            } else if time {
                d.push(DimensionRef::Grained {
                    field,
                    grain: TimeGrain::Day,
                });
            } else {
                d.push(DimensionRef::Field(field));
            }
        });
    };
    let set_grain = move |field: String, grain: String| {
        let grain = match grain.as_str() {
            "minute" => Some(TimeGrain::Minute),
            "hour" => Some(TimeGrain::Hour),
            "day" => Some(TimeGrain::Day),
            "week" => Some(TimeGrain::Week),
            "month" => Some(TimeGrain::Month),
            "quarter" => Some(TimeGrain::Quarter),
            "year" => Some(TimeGrain::Year),
            _ => None,
        };
        dims.update(|d| {
            if let Some(slot) = d.iter_mut().find(|x| x.field() == field) {
                *slot = match grain {
                    Some(grain) => DimensionRef::Grained { field, grain },
                    None => DimensionRef::Field(field),
                };
            }
        });
    };
    let fields = move || {
        model.with_value(|m| {
            m.datasets
                .iter()
                .map(|dataset| {
                    let rows = dataset
                        .fields
                        .iter()
                        .filter(|f| matches(&format!("{}.{}", dataset.name, f.name)))
                        .map(|f| {
                            let name = format!("{}.{}", dataset.name, f.name);
                            let chosen = dims.with(|d| d.iter().find(|x| x.field() == name).cloned());
                            let ok = chosen.is_some()
                                || reach.with_value(|r| {
                                    dims.with(|d| metrics.with(|m| r.field_fits(&name, d, m)))
                                });
                            let time = f.is_time;
                            let timed = f.ty != "date";
                            let grain = chosen
                                .as_ref()
                                .and_then(DimensionRef::grain)
                                .map_or_else(String::new, |g| format!("{g:?}").to_lowercase());
                            let (toggle, regrain) = (name.clone(), name.clone());
                            view! {
                                <li class:far=!ok>
                                    <label title=if ok { String::new() } else { "No relationship leads here from what's chosen".to_string() }>
                                        <input type="checkbox" aria-label=name.clone() prop:checked=chosen.is_some() disabled=!ok
                                            on:change=move |_| toggle_field(toggle.clone(), time) />
                                        " "{f.name.clone()}
                                        <span class="quiet">{format!(" {}", f.ty)}</span>
                                    </label>
                                    {(time && chosen.is_some()).then(|| view! {
                                        <select aria-label=format!("Grain of {name}") prop:value=grain
                                            on:change=move |ev| set_grain(regrain.clone(), event_target_value(&ev))>
                                            <option value="">"each value"</option>
                                            {timed.then(|| view! {
                                                <option value="minute">"by minute"</option>
                                                <option value="hour">"by hour"</option>
                                            })}
                                            <option value="day">"by day"</option>
                                            <option value="week">"by week"</option>
                                            <option value="month">"by month"</option>
                                            <option value="quarter">"by quarter"</option>
                                            <option value="year">"by year"</option>
                                        </select>
                                    })}
                                </li>
                            }
                        })
                        .collect_view();
                    view! {
                        <li class="group">
                            <h4>{dataset.name.clone()}</h4>
                            <ul>{rows}</ul>
                        </li>
                    }
                })
                .collect_view()
        })
    };
    let metric_list = move || {
        model.with_value(|m| {
            m.metrics
                .iter()
                .filter(|metric| matches(&metric.name))
                .map(|metric| {
                    let name = metric.name.clone();
                    let chosen = metrics.with(|m| m.contains(&name));
                    let ok = chosen || reach.with_value(|r| dims.with(|d| r.metric_fits(&name, d)));
                    let toggle = name.clone();
                    view! {
                        <li class:far=!ok>
                            <label title=metric.description.clone().unwrap_or_default()>
                                <input type="checkbox" aria-label=name.clone() prop:checked=chosen disabled=!ok
                                    on:change=move |_| metrics.update(|m| {
                                        if let Some(i) = m.iter().position(|x| *x == toggle) {
                                            m.remove(i);
                                        } else {
                                            m.push(toggle.clone());
                                        }
                                    }) />
                                " "{name.clone()}
                            </label>
                        </li>
                    }
                })
                .collect_view()
        })
    };
    let becomes = move || {
        dims.with(|d| {
            metrics.with(|m| {
                if d.is_empty() && m.is_empty() {
                    "Choose dimensions, metrics or both.".to_string()
                } else if target.is_some() {
                    format!("Shows {}.", title(m, d))
                } else {
                    format!(
                        "A {} of {}.",
                        type_label(default_type(d, m.len())).to_lowercase(),
                        title(m, d)
                    )
                }
            })
        })
    };
    let empty = move || dims.with(Vec::is_empty) && metrics.with(Vec::is_empty);
    view! {
        <div class="picker">
            <h2>{if target.is_some() { "Change fields" } else { "Add a visual" }}</h2>
            <input type="search" placeholder="Search fields and metrics" aria-label="Search fields and metrics"
                on:input=move |ev| search.set(event_target_value(&ev)) />
            <h3>"Dimensions"</h3>
            <ul class="choices">{fields}</ul>
            <h3>"Metrics"</h3>
            <ul class="choices">{metric_list}</ul>
            <p class="quiet">{becomes}</p>
            <div class="actions">
                <button class="primary" disabled=empty on:click=move |_| on_done.run(Pick {
                    target,
                    dimensions: dims.get_untracked(),
                    metrics: metrics.get_untracked(),
                })>{if target.is_some() { "Apply" } else { "Add" }}</button>
                <button on:click=move |_| on_cancel.run(())>"Cancel"</button>
            </div>
        </div>
    }
}

/// What a visual would show now, without a selection: its query run once
/// whenever what it asks changes.
#[component]
fn Preview(
    visual: Memo<Option<Visual>>,
    own: StoredValue<BTreeMap<String, String>>,
) -> impl IntoView {
    let request = Memo::new(move |_| visual.with(|v| v.as_ref().map(crate::design::request)));
    let kind = Memo::new(move |_| visual.with(|v| v.as_ref().map(|v| v.visual_type)));
    let result = RwSignal::new(None::<Result<QueryResult, String>>);
    let generation = StoredValue::new(0u64);
    Effect::new(move |_| {
        let Some(request) = request.get() else { return };
        let n = generation.get_value() + 1;
        generation.set_value(n);
        if request.metrics.is_empty() && request.dimensions.is_empty() {
            result.set(None);
            return;
        }
        let rows = kind.get_untracked() == Some(VisualType::Rows);
        spawn_local(async move {
            // rows are read, not queried: the newest ones
            let reply = if rows {
                let request = tidepool_protocol::RowsRequest {
                    fields: request.dimensions.iter().map(|d| d.field.clone()).collect(),
                    filters: Vec::new(),
                    limit: request.limit,
                };
                post_json("/v1/rows", &request).await
            } else {
                post_json("/v1/query", &request).await
            };
            if generation.get_value() != n {
                return;
            }
            result.set(Some(match reply {
                Ok((200, value)) => serde_json::from_value(value).map_err(|e| e.to_string()),
                Ok((_, value)) => Err(error_of(&value)),
                Err(err) => Err(err),
            }));
        });
    });
    let rows = Memo::new(move |_| {
        result.with(|r| match r {
            Some(Ok(result)) => result.rows.clone(),
            _ => Vec::new(),
        })
    });
    let metrics = Memo::new(move |_| {
        visual.with(|v| v.as_ref().map(|v| v.metrics.clone()).unwrap_or_default())
    });
    let formats = Memo::new(move |_| {
        visual.with(|v| {
            v.as_ref()
                .map(|v| own.with_value(|own| formats_of(v, own)))
                .unwrap_or_default()
        })
    });
    let data = Memo::new(move |_| {
        rows.with(|r| metrics.with(|m| formats.with(|f| Data::formatted(r, m, f))))
    });
    // a metric column's value as the dashboard would show it
    let shown = move |metric: Option<&String>, value: &Value| {
        let format = metric.and_then(|m| formats.with(|f| f.get(m).cloned()));
        formatted(value, format.as_deref())
    };
    let grain = Memo::new(move |_| {
        visual.with(|v| {
            v.as_ref()
                .and_then(|v| v.dimensions.first())
                .and_then(DimensionRef::grain)
                .map(|g| format!("{g:?}").to_lowercase())
        })
    });
    move || {
        if let Some(Err(err)) = result.get() {
            return view! { <p class="problem preview">{format!("Can't show this: {err}")}</p> }
                .into_any();
        }
        match kind.get() {
            Some(VisualType::Bar) => view! { <div class="chart preview"><BarChart data=data /></div> }.into_any(),
            Some(VisualType::Pie) => view! { <div class="chart preview"><PieChart data=data /></div> }.into_any(),
            Some(VisualType::Scatter) => view! { <div class="chart preview"><ScatterChart data=data /></div> }.into_any(),
            Some(VisualType::Heatmap) => view! { <div class="chart preview"><HeatChart rows=rows /></div> }.into_any(),
            Some(VisualType::Line) => view! { <div class="chart preview"><LineChart data=data grain=grain /></div> }.into_any(),
            Some(VisualType::Kpi) => view! {
                <p class="kpi-value preview">{move || {
                    let metric = metrics.with(|m| m.first().cloned());
                    rows.with(|r| r.first().and_then(|r| r.first()).map(|v| shown(metric.as_ref(), v)).unwrap_or_default())
                }}</p>
            }.into_any(),
            Some(VisualType::Filter) => view! {
                <ul class="preview values-static">
                    {move || rows.with(|r| r.iter().take(50).map(|row| view! {
                        <li class="value">{row.first().map(display).unwrap_or_default()}</li>
                    }).collect_view())}
                </ul>
            }.into_any(),
            Some(VisualType::Table | VisualType::Rows) => view! {
                <div class="table-wrap preview">
                    <table>
                        <thead><tr>
                            {move || result.with(|r| match r {
                                Some(Ok(result)) => result.columns.iter().map(|c| {
                                    let numeric = c.role == "metric";
                                    let label = if numeric { c.name.clone() } else { short(&c.name) };
                                    view! { <th class:num=numeric>{label}</th> }
                                }).collect_view().into_any(),
                                _ => ().into_any(),
                            })}
                        </tr></thead>
                        <tbody>
                            {move || {
                                let dims = visual.with(|v| v.as_ref().map_or(0, |v| v.dimensions.len()));
                                let names = metrics.get();
                                rows.with(|r| r.iter().take(50).map(|row| view! {
                                    <tr>{row.iter().enumerate().map(|(c, v)| {
                                        let numeric = c >= dims;
                                        let text = if numeric { shown(names.get(c - dims), v) } else { display(v) };
                                        view! { <td class:num=numeric>{text}</td> }
                                    }).collect_view()}</tr>
                                }).collect_view())
                            }}
                        </tbody>
                    </table>
                </div>
            }.into_any(),
            None => ().into_any(),
        }
    }
}
