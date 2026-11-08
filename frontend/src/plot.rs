//! The charts drawn: SVG from the layouts in `charts.rs`, sized by their
//! box, moving to new data, and selected from with the pointer (a click, or
//! a drag across bars or periods) and the keyboard (arrows, Shift to
//! extend, Enter).
//!
//! Pointer events are handled once, on the chart's box, and turned into a
//! category by the layout (`hit`, `nearest`); the marks themselves carry no
//! handlers, so a frame may rebuild them while a click is in flight.

use std::cell::RefCell;
use std::sync::atomic::{AtomicUsize, Ordering};

use leptos::html::Div;
use leptos::prelude::*;
use wasm_bindgen::prelude::*;

use crate::charts::{
    BarLayout, Data, LineLayout, PieLayout, Rect, TWEEN_MS, area_path, ease, line_path, step_key,
};

/// The charts' text, as `style.css` sets it on `.plot`.
const FONT: &str = "12px \"Atkinson Hyperlegible\", \"Segoe UI\", system-ui, sans-serif";

thread_local! {
    static CONTEXT: RefCell<Option<web_sys::CanvasRenderingContext2d>> = const { RefCell::new(None) };
}

/// How wide `text` is in the charts' font, measured by a canvas.
#[must_use]
pub fn measure(text: &str) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let guess = text.chars().count() as f64 * 6.6;
    CONTEXT.with(|cell| {
        let mut cell = cell.borrow_mut();
        if cell.is_none() {
            *cell = document()
                .create_element("canvas")
                .ok()
                .and_then(|c| c.dyn_into::<web_sys::HtmlCanvasElement>().ok())
                .and_then(|c| c.get_context("2d").ok().flatten())
                .and_then(|c| c.dyn_into::<web_sys::CanvasRenderingContext2d>().ok());
            if let Some(context) = cell.as_ref() {
                context.set_font(FONT);
            }
        }
        cell.as_ref()
            .and_then(|c| c.measure_text(text).ok())
            .map_or(guess, |m| m.width())
    })
}

/// An id no other chart on the page has, for `aria-activedescendant`.
fn unique(kind: &str) -> String {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    format!("{kind}-{}", NEXT.fetch_add(1, Ordering::Relaxed))
}

/// The element's size, kept current: grid and panel layouts resize charts
/// without the window changing.
fn observe_size(node: NodeRef<Div>) -> ReadSignal<(f64, f64)> {
    let (size, set_size) = signal((0.0, 0.0));
    let observer = StoredValue::new_local(None::<web_sys::ResizeObserver>);
    Effect::new(move |_| {
        let Some(el) = node.get() else { return };
        let read = {
            let el = el.clone();
            move || set_size.set((f64::from(el.client_width()), f64::from(el.client_height())))
        };
        read();
        let on_resize = Closure::<dyn Fn()>::new(read);
        if let Ok(created) = web_sys::ResizeObserver::new(on_resize.as_ref().unchecked_ref()) {
            created.observe(&el);
            observer.set_value(Some(created));
        }
        on_resize.forget(); // lives as long as the observer, which owns it
    });
    on_cleanup(move || {
        observer.with_value(|o| {
            if let Some(o) = o {
                o.disconnect();
            }
        });
    });
    size
}

/// A layout that can be drawn part of the way to another.
trait Blend: Clone + PartialEq + Send + Sync + 'static {
    fn blend(&self, to: &Self, t: f64) -> Self;
    fn size(&self) -> (f64, f64);
}

impl Blend for BarLayout {
    fn blend(&self, to: &Self, t: f64) -> Self {
        BarLayout::blend(self, to, t)
    }
    fn size(&self) -> (f64, f64) {
        (self.width, self.height)
    }
}

impl Blend for PieLayout {
    fn blend(&self, to: &Self, t: f64) -> Self {
        PieLayout::blend(self, to, t)
    }
    fn size(&self) -> (f64, f64) {
        (self.width, self.height)
    }
}

impl Blend for LineLayout {
    fn blend(&self, to: &Self, t: f64) -> Self {
        LineLayout::blend(self, to, t)
    }
    fn size(&self) -> (f64, f64) {
        (self.width, self.height)
    }
}

fn reduced_motion() -> bool {
    window()
        .match_media("(prefers-reduced-motion: reduce)")
        .ok()
        .flatten()
        .is_some_and(|m| m.matches())
}

fn now() -> f64 {
    window().performance().map_or(0.0, |p| p.now())
}

/// The layout to draw: `target`, reached over `TWEEN_MS` from wherever the
/// drawing is when it changes. A new size or the first data is drawn at
/// once: only data that changed moves.
fn tween<T: Blend>(target: Memo<T>) -> ReadSignal<T> {
    let (shown, set_shown) = signal(target.get_untracked());
    let run = StoredValue::new(0_u64);
    Effect::new(move |_| {
        let to = target.get();
        let from = shown.get_untracked();
        run.update_value(|r| *r += 1);
        if from.size() != to.size() || reduced_motion() {
            set_shown.set(to);
            return;
        }
        step(set_shown, from, to, run, run.get_value(), now());
    });
    shown
}

fn step<T: Blend>(
    shown: WriteSignal<T>,
    from: T,
    to: T,
    run: StoredValue<u64>,
    this: u64,
    start: f64,
) {
    request_animation_frame(move || {
        if run.try_get_value() != Some(this) {
            return; // newer data took over, or the chart is gone
        }
        let t = (now() - start) / TWEEN_MS;
        if t >= 1.0 {
            shown.set(to);
            return;
        }
        shown.set(from.blend(&to, ease(t)));
        step(shown, from, to, run, this, start);
    });
}

/// The pointer's place in the chart's box.
fn local(node: NodeRef<Div>, ev: &web_sys::MouseEvent) -> Option<(f64, f64)> {
    let el = node.get_untracked()?;
    let r = el.get_bounding_client_rect();
    Some((
        f64::from(ev.client_x()) - r.x(),
        f64::from(ev.client_y()) - r.y(),
    ))
}

fn capture(node: NodeRef<Div>, ev: &web_sys::PointerEvent) {
    if let Some(el) = node.get_untracked() {
        let _ = el.set_pointer_capture(ev.pointer_id());
    }
}

fn additive(ev: &web_sys::MouseEvent) -> bool {
    ev.ctrl_key() || ev.meta_key()
}

fn navigation(key: &str) -> bool {
    matches!(
        key,
        "ArrowUp" | "ArrowDown" | "ArrowLeft" | "ArrowRight" | "Home" | "End"
    )
}

/// The colour class of metric `m`: the first in teal, the rest soft.
fn metric_class(m: usize) -> &'static str {
    if m == 0 { "m0" } else { "m1" }
}

/// The metrics' names and colours, when there is more than one.
fn legend(data: Memo<Data>, most: usize) -> impl IntoView {
    move || {
        data.with(|d| {
            (d.names.len() > 1).then(|| {
                view! {
                    <ul class="legend">
                        {d.names.iter().take(most).enumerate().map(|(m, name)| view! {
                            <li><span class=format!("swatch {}", metric_class(m))></span>{name.clone()}</li>
                        }).collect_view()}
                    </ul>
                }
            })
        })
    }
}

/// The tooltip for category `i`, beside `(x, y)`; on the left of it in the
/// right part of the chart.
fn tip(data: Memo<Data>, at: Option<(usize, (f64, f64))>, width: f64) -> impl IntoView {
    let (i, (x, y)) = at?;
    let (title, rows) = data.with_untracked(|d| d.tip(i))?;
    let flip = x > width * 0.6;
    Some(view! {
        <div class="tip" class:flip=flip style:left=format!("{x}px") style:top=format!("{y}px")>
            <strong>{title}</strong>
            {rows.into_iter().enumerate().map(|(m, (name, value))| view! {
                <div class="tip-row">
                    <span class=format!("swatch {}", metric_class(m))></span>
                    <span>{name}</span>
                    <b>{value}</b>
                </div>
            }).collect_view()}
        </div>
    })
}

fn brush_rect(span: Option<Rect>) -> impl IntoView {
    span.map(|r| view! { <rect class="brush" x=r.x y=r.y width=r.w height=r.h /> })
}

/// A bar chart of one dimension.
///
/// - `selected` are the categories (as shown) the selection holds: they
///   are drawn in the selection colour.
/// - `on_select` makes the chart selectable: a click anywhere in a bar's
///   band, or a drag across several, or Enter on the keyboard's place,
///   gives the categories and whether Ctrl/Cmd was held (add, don't
///   replace).
#[allow(clippy::too_many_lines)] // one chart: its state, its handlers, its drawing
#[component]
pub fn BarChart(
    #[prop(into)] data: Memo<Data>,
    #[prop(into, optional)] selected: Signal<Vec<String>>,
    #[prop(into, optional_no_strip)] on_select: Option<Callback<(Vec<usize>, bool)>>,
    #[prop(into, optional)] label: String,
) -> impl IntoView {
    let node = NodeRef::<Div>::new();
    let size = observe_size(node);
    let target = Memo::new(move |_| {
        let (w, h) = size.get();
        if w <= 0.0 || h <= 0.0 {
            return BarLayout::default();
        }
        data.with(|d| selected.with(|s| BarLayout::new(d, (w, h), s, &measure)))
    });
    let shown = tween(target);
    let hover = RwSignal::new(None::<(usize, (f64, f64))>);
    let active = RwSignal::new(None::<usize>);
    let press = RwSignal::new(None::<usize>);
    let brush = RwSignal::new(None::<(usize, usize)>);
    let interactive = on_select.is_some();
    let id = unique("bars");

    let select = move |categories: Vec<usize>, add: bool| {
        if let Some(on_select) = on_select {
            on_select.run((categories, add));
        }
    };
    let down = move |ev: web_sys::PointerEvent| {
        if !interactive || ev.button() != 0 {
            return;
        }
        let Some((x, y)) = local(node, &ev) else {
            return;
        };
        if let Some(i) = target.with_untracked(|l| l.hit(x, y)) {
            press.set(Some(i));
            capture(node, &ev);
        }
    };
    let moved = move |ev: web_sys::PointerEvent| {
        let Some((x, y)) = local(node, &ev) else {
            return;
        };
        if let Some(from) = press.get_untracked() {
            let to = target.with_untracked(|l| l.nearest(x, y)).unwrap_or(from);
            if to != from || brush.get_untracked().is_some() {
                brush.set(Some((from, to)));
            }
            hover.set(None);
            return;
        }
        hover.set(target.with_untracked(|l| l.hit(x, y)).map(|i| (i, (x, y))));
    };
    let up = move |ev: web_sys::PointerEvent| {
        let Some(from) = press.get_untracked() else {
            return;
        };
        press.set(None);
        match brush.get_untracked() {
            Some((a, b)) => select((a.min(b)..=a.max(b)).collect(), additive(&ev)),
            None => select(vec![from], additive(&ev)),
        }
        brush.set(None);
    };
    let cancel = move |_| {
        press.set(None);
        brush.set(None);
    };
    let key = move |ev: web_sys::KeyboardEvent| {
        let key = ev.key();
        let n = data.with_untracked(Data::len);
        match key.as_str() {
            "Enter" | " " => {
                if let Some(i) = active.get_untracked().filter(|&i| i < n) {
                    ev.prevent_default();
                    select(vec![i], ev.ctrl_key() || ev.meta_key());
                }
            }
            "Escape" => active.set(None),
            _ if navigation(&key) => {
                ev.prevent_default();
                active.set(step_key(&key, active.get_untracked().filter(|&i| i < n), n));
            }
            _ => {}
        }
    };

    // the keyboard's place, or the pointer's: shaded, with its tooltip
    let pointed = move || {
        let n = data.with(Data::len);
        match (active.get().filter(|&i| i < n), hover.get()) {
            (Some(i), _) => shown.with(|l| {
                let band = l.span(i, i);
                Some((i, (band.x + band.w / 2.0, band.y + band.h / 2.0)))
            }),
            (None, at) => at,
        }
    };
    let descendant = {
        let id = id.clone();
        move || active.get().map(|i| format!("{id}-{i}"))
    };
    let options = move || {
        data.with(|d| {
            selected.with(|s| {
                (0..d.len())
                    .map(|i| {
                        view! {
                            <g id=format!("{id}-{i}") role="option" aria-label=d.describe(i)
                                aria-selected=s.contains(&d.keys[i]).to_string()></g>
                        }
                    })
                    .collect_view()
            })
        })
    };

    view! {
        <div class="plot-wrap">
            {legend(data, usize::MAX)}
            <div class="plot-area" node_ref=node class:interactive=interactive
                on:pointerdown=down on:pointermove=moved on:pointerup=up
                on:pointercancel=cancel on:pointerleave=move |_| hover.set(None)>
                <svg class="plot" width=move || shown.with(|l| l.width) height=move || shown.with(|l| l.height)
                    role=if interactive { "listbox" } else { "img" } aria-label=label
                    aria-multiselectable=interactive.then_some("true")
                    tabindex=interactive.then_some("0")
                    aria-activedescendant=descendant
                    on:keydown=key on:blur=move |_| active.set(None)>
                    {move || shown.with(|l| {
                        let horizontal = l.horizontal;
                        let plot = l.plot;
                        let band = pointed().map(|(i, _)| l.span(i, i));
                        view! {
                            <g class="grid">
                                {l.ticks.iter().map(|t| if horizontal {
                                    view! { <line x1=t.at x2=t.at y1=plot.y y2=plot.bottom() /> }
                                } else {
                                    view! { <line x1=plot.x x2=plot.right() y1=t.at y2=t.at /> }
                                }).collect_view()}
                            </g>
                            {band.map(|r| view! { <rect class="band" x=r.x y=r.y width=r.w height=r.h /> })}
                            {brush_rect(brush.get().map(|(a, b)| l.span(a, b)))}
                            <g class="bars">
                                {l.bars.iter().map(|b| view! {
                                    <rect class=format!("bar {}", metric_class(b.metric)) class:selected=b.selected
                                        x=b.rect.x y=b.rect.y width=b.rect.w height=b.rect.h />
                                }).collect_view()}
                            </g>
                            {if horizontal {
                                view! { <line class="baseline" x1=l.zero x2=l.zero y1=plot.y y2=plot.bottom() /> }
                            } else {
                                view! { <line class="baseline" x1=plot.x x2=plot.right() y1=l.zero y2=l.zero /> }
                            }}
                            <g class="ticks">
                                {l.ticks.iter().map(|t| if horizontal {
                                    view! { <text x=t.at y=plot.bottom() + 15.0 text-anchor="middle">{t.text.clone()}</text> }
                                } else {
                                    view! { <text x=plot.x - 6.0 y=t.at + 4.0 text-anchor="end">{t.text.clone()}</text> }
                                }).collect_view()}
                            </g>
                            <g class="labels">
                                {l.labels.iter().map(|t| view! {
                                    <text x=t.x y=t.y + 4.0 text-anchor=t.anchor>{t.text.clone()}</text>
                                }).collect_view()}
                            </g>
                        }
                    })}
                    {interactive.then(|| view! { <g class="a11y">{options}</g> })}
                </svg>
                {move || tip(data, if press.get().is_some() { None } else { pointed() }, shown.with(|l| l.width))}
            </div>
        </div>
    }
}

/// A pie (a ring) of one metric by one dimension.
///
/// - `selected` are the categories (as shown) the selection holds: their
///   slices are edged in the selection colour, the others dimmed.
/// - `whole` is the metric over everything, when the rest of it can be a
///   slice (a sum or a count).
/// - `on_select` makes it selectable: a click on a slice, or Enter on the
///   keyboard's place, gives the category and whether Ctrl/Cmd was held.
#[allow(clippy::too_many_lines)] // one chart: its state, its handlers, its drawing
#[component]
pub fn PieChart(
    #[prop(into)] data: Memo<Data>,
    #[prop(into, optional)] selected: Signal<Vec<String>>,
    #[prop(into, optional)] whole: Signal<Option<f64>>,
    #[prop(into, optional_no_strip)] on_select: Option<Callback<(Vec<usize>, bool)>>,
    #[prop(into, optional)] label: String,
) -> impl IntoView {
    let node = NodeRef::<Div>::new();
    let size = observe_size(node);
    let target = Memo::new(move |_| {
        let (w, h) = size.get();
        if w <= 0.0 || h <= 0.0 {
            return PieLayout::default();
        }
        data.with(|d| selected.with(|s| PieLayout::new(d, (w, h), s, whole.get(), &measure)))
    });
    let shown = tween(target);
    let hover = RwSignal::new(None::<(usize, (f64, f64))>);
    let active = RwSignal::new(None::<usize>);
    let interactive = on_select.is_some();
    let id = unique("slices");
    let select = move |categories: Vec<usize>, add: bool| {
        if let Some(on_select) = on_select {
            on_select.run((categories, add));
        }
    };
    let click = move |ev: web_sys::MouseEvent| {
        if !interactive {
            return;
        }
        let Some((x, y)) = local(node, &ev) else {
            return;
        };
        if let Some(i) = target.with_untracked(|l| l.hit(x, y)) {
            select(vec![i], additive(&ev));
        }
    };
    let moved = move |ev: web_sys::PointerEvent| {
        let Some((x, y)) = local(node, &ev) else {
            return;
        };
        hover.set(target.with_untracked(|l| l.hit(x, y)).map(|i| (i, (x, y))));
    };
    let key = move |ev: web_sys::KeyboardEvent| {
        let key = ev.key();
        let n = data.with_untracked(Data::len);
        match key.as_str() {
            "Enter" | " " => {
                if let Some(i) = active.get_untracked().filter(|&i| i < n) {
                    ev.prevent_default();
                    select(vec![i], ev.ctrl_key() || ev.meta_key());
                }
            }
            "Escape" => active.set(None),
            _ if navigation(&key) => {
                ev.prevent_default();
                active.set(step_key(&key, active.get_untracked().filter(|&i| i < n), n));
            }
            _ => {}
        }
    };
    let pointed = move || match active.get() {
        Some(i) => shown.with(|l| l.middle(i)).map(|at| (i, at)),
        None => hover.get(),
    };
    let descendant = {
        let id = id.clone();
        move || active.get().map(|i| format!("{id}-{i}"))
    };
    let options = move || {
        data.with(|d| {
            selected.with(|s| {
                (0..d.len())
                    .map(|i| {
                        view! {
                            <g id=format!("{id}-{i}") role="option" aria-label=d.describe(i)
                                aria-selected=s.contains(&d.keys[i]).to_string()></g>
                        }
                    })
                    .collect_view()
            })
        })
    };
    view! {
        <div class="plot-wrap">
            <div class="plot-area" node_ref=node class:interactive=interactive
                on:click=click on:pointermove=moved on:pointerleave=move |_| hover.set(None)>
                <svg class="plot pie" class:has-selection=move || selected.with(|s| !s.is_empty())
                    width=move || shown.with(|l| l.width) height=move || shown.with(|l| l.height)
                    role=if interactive { "listbox" } else { "img" } aria-label=label
                    aria-multiselectable=interactive.then_some("true")
                    tabindex=interactive.then_some("0")
                    aria-activedescendant=descendant
                    on:keydown=key on:blur=move |_| active.set(None)>
                    {move || shown.with(|l| {
                        let on = pointed().map(|(i, _)| i);
                        view! {
                            <g class="slices">
                                {l.slices.iter().map(|slice| view! {
                                    <path class=format!("slice c{}", slice.colour)
                                        class:selected=slice.selected
                                        class:pointed=slice.category.is_some() && slice.category == on
                                        d=l.path(slice) />
                                }).collect_view()}
                            </g>
                            <g class="labels">
                                {l.labels.iter().map(|t| view! {
                                    <text x=t.x y=t.y + 4.0 text-anchor=t.anchor>{t.text.clone()}</text>
                                }).collect_view()}
                            </g>
                            {(l.left_out > 0).then(|| view! {
                                <text class="note" x=4.0 y=l.height - 4.0>
                                    {format!("{} not shown: no value, or 0 or less", l.left_out)}
                                </text>
                            })}
                        }
                    })}
                    {interactive.then(|| view! { <g class="a11y">{options}</g> })}
                </svg>
                {move || tip(data, pointed(), shown.with(|l| l.width))}
            </div>
        </div>
    }
}

/// A line over periods: the first metric with an area under it, a second
/// on its own axis.
///
/// `on_brush` makes periods selectable: a drag across them, a click on
/// one, or Shift+arrows and Enter give the first and last period chosen.
#[allow(clippy::too_many_lines)] // one chart: its state, its handlers, its drawing
#[component]
pub fn LineChart(
    #[prop(into)] data: Memo<Data>,
    #[prop(into, optional)] grain: Signal<Option<String>>,
    #[prop(into, optional_no_strip)] on_brush: Option<Callback<(usize, usize)>>,
    #[prop(into, optional)] label: String,
) -> impl IntoView {
    let node = NodeRef::<Div>::new();
    let size = observe_size(node);
    let target = Memo::new(move |_| {
        let (w, h) = size.get();
        if w <= 0.0 || h <= 0.0 {
            return LineLayout::default();
        }
        data.with(|d| grain.with(|g| LineLayout::new(d, (w, h), g.as_deref(), &measure)))
    });
    let shown = tween(target);
    let hover = RwSignal::new(None::<(usize, (f64, f64))>);
    let active = RwSignal::new(None::<usize>);
    let anchor = RwSignal::new(None::<usize>);
    let press = RwSignal::new(None::<(usize, f64)>);
    let brush = RwSignal::new(None::<(usize, usize)>);
    let interactive = on_brush.is_some();
    let id = unique("periods");

    let choose = move |(a, b): (usize, usize)| {
        if let Some(on_brush) = on_brush {
            on_brush.run((a.min(b), a.max(b)));
        }
    };
    let inside = move |x: f64, y: f64| {
        target.with_untracked(|l| {
            let p = l.plot;
            (p.x - 8.0..=p.right() + 8.0).contains(&x) && (p.y..=p.bottom()).contains(&y)
        })
    };
    let down = move |ev: web_sys::PointerEvent| {
        if !interactive || ev.button() != 0 {
            return;
        }
        let Some((x, y)) = local(node, &ev) else {
            return;
        };
        if !inside(x, y) {
            return;
        }
        if let Some(i) = target.with_untracked(|l| l.nearest(x)) {
            press.set(Some((i, x)));
            capture(node, &ev);
        }
    };
    let moved = move |ev: web_sys::PointerEvent| {
        let Some((x, y)) = local(node, &ev) else {
            return;
        };
        let at = target.with_untracked(|l| l.nearest(x));
        if let Some((from, x0)) = press.get_untracked() {
            // a few pixels of slack: a click that wobbles is still a click
            if (x - x0).abs() > 4.0 || brush.get_untracked().is_some() {
                brush.set(Some((from, at.unwrap_or(from))));
            }
            hover.set(at.map(|i| (i, (x, y))));
            return;
        }
        hover.set(at.filter(|_| inside(x, y)).map(|i| (i, (x, y))));
    };
    let up = move |_: web_sys::PointerEvent| {
        let Some((from, _)) = press.get_untracked() else {
            return;
        };
        press.set(None);
        choose(brush.get_untracked().unwrap_or((from, from)));
        brush.set(None);
    };
    let cancel = move |_| {
        press.set(None);
        brush.set(None);
    };
    let key = move |ev: web_sys::KeyboardEvent| {
        let key = ev.key();
        let n = data.with_untracked(Data::len);
        match key.as_str() {
            "Enter" | " " => {
                let at = active.get_untracked().filter(|&i| i < n);
                if let Some(i) = at {
                    ev.prevent_default();
                    choose(brush.get_untracked().unwrap_or((i, i)));
                    brush.set(None);
                    anchor.set(None);
                }
            }
            "Escape" => {
                active.set(None);
                anchor.set(None);
                brush.set(None);
            }
            _ if navigation(&key) => {
                ev.prevent_default();
                let from = active.get_untracked().filter(|&i| i < n);
                let to = step_key(&key, from, n);
                if ev.shift_key() && interactive {
                    let start = anchor.get_untracked().or(from).or(to);
                    anchor.set(start);
                    brush.set(start.zip(to));
                } else {
                    anchor.set(None);
                    brush.set(None);
                }
                active.set(to);
            }
            _ => {}
        }
    };

    let pointed = move || {
        let n = data.with(Data::len);
        match (active.get().filter(|&i| i < n), hover.get()) {
            (Some(i), _) => shown.with(|l| {
                let x = l.points.map_or(0.0, |p| p.at(i));
                Some((i, (x, l.plot.y + l.plot.h / 3.0)))
            }),
            (None, at) => at,
        }
    };
    let descendant = {
        let id = id.clone();
        move || active.get().map(|i| format!("{id}-{i}"))
    };
    let options = move || {
        data.with(|d| {
            (0..d.len())
                .map(|i| view! { <g id=format!("{id}-{i}") role="option" aria-label=d.describe(i)></g> })
                .collect_view()
        })
    };

    view! {
        <div class="plot-wrap">
            {legend(data, 2)}
            <div class="plot-area" node_ref=node class:brushable=interactive
                on:pointerdown=down on:pointermove=moved on:pointerup=up
                on:pointercancel=cancel on:pointerleave=move |_| hover.set(None)>
                <svg class="plot" width=move || shown.with(|l| l.width) height=move || shown.with(|l| l.height)
                    role="listbox" aria-label=label tabindex="0"
                    aria-activedescendant=descendant
                    on:keydown=key on:blur=move |_| { active.set(None); anchor.set(None); brush.set(None); }>
                    {move || shown.with(|l| {
                        let plot = l.plot;
                        let cursor = pointed().map(|(i, _)| i);
                        let base = l.axes.first().map_or(plot.bottom(), |a| a.zero);
                        view! {
                            <g class="grid">
                                {l.axes.first().map(|a| a.ticks.iter().map(|t| view! {
                                    <line x1=plot.x x2=plot.right() y1=t.at y2=t.at />
                                }).collect_view())}
                            </g>
                            <line class="baseline" x1=plot.x x2=plot.right() y1=plot.bottom() y2=plot.bottom() />
                            {l.series.first().map(|s| view! { <path class="area" d=area_path(s, base) /> })}
                            {brush_rect(brush.get().map(|(a, b)| l.span(a, b)))}
                            {cursor.and_then(|i| l.points.map(|p| p.at(i))).map(|x| view! {
                                <line class="cursor" x1=x x2=x y1=plot.y y2=plot.bottom() />
                            })}
                            {l.series.iter().enumerate().map(|(m, s)| view! {
                                <path class=format!("line {}", metric_class(m)) d=line_path(s) />
                            }).collect_view()}
                            {l.series.iter().enumerate().map(|(m, s)| {
                                s.iter().enumerate().filter_map(|(i, at)| {
                                    let (x, y) = (*at)?;
                                    let on = cursor == Some(i);
                                    (l.dots || on).then(|| view! {
                                        <circle class=format!("dot {}", metric_class(m)) class:on=on cx=x cy=y r=if on { 4.0 } else { 2.5 } />
                                    })
                                }).collect_view()
                            }).collect_view()}
                            {l.axes.iter().map(|a| a.ticks.iter().map(|t| if a.right {
                                view! { <text x=plot.right() + 6.0 y=t.at + 4.0 text-anchor="start">{t.text.clone()}</text> }
                            } else {
                                view! { <text x=plot.x - 6.0 y=t.at + 4.0 text-anchor="end">{t.text.clone()}</text> }
                            }).collect_view()).collect_view()}
                            <g class="labels">
                                {l.labels.iter().map(|t| view! {
                                    <text x=t.x y=t.y + 4.0 text-anchor=t.anchor>{t.text.clone()}</text>
                                }).collect_view()}
                            </g>
                        }
                    })}
                    <g class="a11y">{options}</g>
                </svg>
                {move || tip(data, if brush.get().is_some() { None } else { pointed() }, shown.with(|l| l.width))}
            </div>
        </div>
    }
}
