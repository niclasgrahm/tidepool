//! The date range filter: a trigger in the rail that opens a popover of
//! relative windows, a month calendar and the exact bounds, typed. All of
//! it drawn here: native date inputs look different in every browser and
//! can't show a range. The arithmetic is in [`crate::calendar`].
//!
//! Choosing acts at once, as everything on a dashboard does: a window, or
//! the second click on the calendar, is sent and closes the popover; typed
//! bounds are sent on Enter or when the field is left.

use leptos::{ev, html, prelude::*};
use serde_json::{Value, json};
use tidepool_protocol::live::SelectedField;
use wasm_bindgen::JsCast;

use crate::board::presets;
use crate::calendar::{self, End, Month, Pick};
use crate::live::Live;

const WEEKDAYS: [&str; 7] = ["Mo", "Tu", "We", "Th", "Fr", "Sa", "Su"];

/// Today by the browser's clock, as a UTC day number.
fn today() -> i64 {
    #[allow(clippy::cast_possible_truncation)] // days since 1970 fit easily
    let day = (js_sys::Date::now() / 86_400_000.0).floor() as i64;
    day
}

/// The ends a selection shows as in the picker.
fn ends_of(selected: Option<&SelectedField>) -> (Option<End>, Option<End>) {
    selected.map_or((None, None), |s| {
        calendar::ends(
            s.from.as_ref().and_then(Value::as_str),
            s.to.as_ref().and_then(Value::as_str),
        )
    })
}

/// One typed bound: `None` when it's empty, an error when it doesn't
/// parse.
fn typed(day: &str, time: &str) -> Result<Option<End>, ()> {
    if day.trim().is_empty() {
        return Ok(None);
    }
    let day = calendar::parse_day(day).ok_or(())?;
    let time = match time.trim() {
        "" => None,
        t => Some(calendar::parse_time(t).ok_or(())?),
    };
    Ok(Some(End { day, time }))
}

/// Moves focus to the calendar's cell for `day` once it's drawn.
fn focus_day(grid: NodeRef<html::Div>, day: i64) {
    request_animation_frame(move || {
        let cell = grid
            .get_untracked()
            .and_then(|g| {
                g.query_selector(&format!("[data-day='{day}']"))
                    .ok()
                    .flatten()
            })
            .and_then(|c| c.dyn_into::<web_sys::HtmlElement>().ok());
        if let Some(cell) = cell {
            let _ = cell.focus();
        }
    });
}

/// A from/to range on a date or time field, or a window relative to now
/// ("Last 7 days") that moves as time passes.
#[allow(clippy::too_many_lines)] // one control: its state, its keys, its popover
#[component]
pub fn DateRange(field: String, title: String, timed: bool) -> impl IntoView {
    let live = expect_context::<Live>();
    let selected = {
        let key = field.clone();
        Memo::new(move |_| {
            live.selection
                .with(|s| s.iter().find(|f| f.field == key).cloned())
        })
    };
    let field = StoredValue::new(field);
    let windows = StoredValue::new(presets(timed));
    let now = today();

    let open = RwSignal::new(false);
    let pick = RwSignal::new(Pick::default());
    let pointed = RwSignal::new(None::<i64>);
    let month = RwSignal::new(Month::of(now));
    let cursor = RwSignal::new(now);
    let from_day = RwSignal::new(String::new());
    let from_time = RwSignal::new(String::new());
    let until_day = RwSignal::new(String::new());
    let until_time = RwSignal::new(String::new());
    let root = NodeRef::<html::Div>::new();
    let trigger = NodeRef::<html::Button>::new();
    let grid = NodeRef::<html::Div>::new();

    // show what the server holds: on every change while closed (a window
    // moves with the clock; it mustn't overwrite what's being typed), and
    // on opening
    let reset = move || {
        let (from, until) = selected.with_untracked(|s| ends_of(s.as_ref()));
        pick.set(match (from, until) {
            (Some(f), Some(u)) => Pick {
                start: Some(f.day),
                end: Some(u.day),
            },
            _ => Pick::default(),
        });
        let day = |e: Option<End>| e.map(|e| calendar::iso(e.day)).unwrap_or_default();
        let time = |e: Option<End>| {
            e.and_then(|e| e.time)
                .map(calendar::clock)
                .unwrap_or_default()
        };
        from_day.set(day(from));
        from_time.set(time(from));
        until_day.set(day(until));
        until_time.set(time(until));
        let shown = until.or(from).map_or(now, |e| e.day);
        month.set(Month::of(shown));
        cursor.set(shown);
        pointed.set(None);
    };
    Effect::new(move |_| {
        selected.track();
        if !open.get_untracked() {
            reset();
        }
    });

    let close = move || {
        open.set(false);
        if let Some(t) = trigger.get_untracked() {
            let _ = t.focus();
        }
    };
    let toggle = move |_| {
        if open.get_untracked() {
            open.set(false);
        } else {
            reset();
            open.set(true);
        }
    };
    // a press anywhere else closes it
    let outside = window_event_listener(ev::pointerdown, move |e| {
        if !open.get_untracked() {
            return;
        }
        let target = e.target().and_then(|t| t.dyn_into::<web_sys::Node>().ok());
        let inside = root
            .get_untracked()
            .zip(target)
            .is_some_and(|(r, t)| r.contains(Some(&t)));
        if !inside {
            open.set(false);
        }
    });
    on_cleanup(move || outside.remove());

    let send_range = move |from: Option<End>, until: Option<End>| {
        // the same again (a field left unchanged) would only add history
        if selected.with_untracked(|s| s.is_some() && ends_of(s.as_ref()) == (from, until)) {
            return;
        }
        let field = field.get_value();
        if from.is_none() && until.is_none() {
            live.send(&json!({"type": "clear", "field": field}));
        } else {
            let (from, to) = calendar::bounds(from, until);
            live.send(&json!({"type": "select_range", "field": field, "from": from, "to": to}));
        }
    };
    let clear = move |_| {
        live.send(&json!({"type": "clear", "field": field.get_value()}));
        close();
    };
    let commit = move || {
        let from = typed(&from_day.get_untracked(), &from_time.get_untracked());
        let until = typed(&until_day.get_untracked(), &until_time.get_untracked());
        if let (Ok(from), Ok(until)) = (from, until) {
            send_range(from, until);
        }
    };
    let click = move |day: i64| {
        let next = pick.get_untracked().click(day);
        pick.set(next);
        cursor.set(day);
        from_time.set(String::new());
        until_time.set(String::new());
        if let (Some(start), Some(end)) = (next.start, next.end) {
            from_day.set(calendar::iso(start));
            until_day.set(calendar::iso(end));
            send_range(
                Some(End {
                    day: start,
                    time: None,
                }),
                Some(End {
                    day: end,
                    time: None,
                }),
            );
            close();
        } else {
            from_day.set(calendar::iso(day));
            until_day.set(String::new());
            pointed.set(Some(day));
        }
    };
    let key = move |e: web_sys::KeyboardEvent| {
        let at = cursor.get_untracked();
        let in_month = |n: i64| {
            let m = Month::of(at).add(n);
            (m.first() + calendar::day_of_month(at) - 1).min(m.add(1).first() - 1)
        };
        let to = match e.key().as_str() {
            "ArrowLeft" => at - 1,
            "ArrowRight" => at + 1,
            "ArrowUp" => at - 7,
            "ArrowDown" => at + 7,
            "PageUp" => in_month(-1),
            "PageDown" => in_month(1),
            "Home" => at - (at + 3).rem_euclid(7),
            "End" => at + 6 - (at + 3).rem_euclid(7),
            "Enter" | " " => {
                e.prevent_default();
                click(at);
                return;
            }
            _ => return,
        };
        e.prevent_default();
        cursor.set(to);
        if pick.get_untracked().picking() {
            pointed.set(Some(to));
        }
        if !month.get_untracked().contains(to) {
            month.set(Month::of(to));
        }
        focus_day(grid, to);
    };
    let escape = move |e: web_sys::KeyboardEvent| {
        if e.key() == "Escape" {
            e.prevent_default();
            close();
        }
    };

    let label = move || {
        selected.with(|s| match s {
            None => "All time".to_string(),
            Some(s) => s.relative.map_or_else(
                || {
                    let (f, u) = ends_of(Some(s));
                    calendar::short_span(f, u, now)
                },
                |w| w.label(),
            ),
        })
    };
    // beside the label, what it doesn't say: a window's dates, a range's length
    let span = move || {
        selected.with(|s| {
            let (f, u) = ends_of(s.as_ref());
            match (s, f, u) {
                (Some(s), ..) if s.relative.is_some() => calendar::short_span(f, u, now),
                (Some(_), Some(f), Some(u)) => calendar::length(f, u),
                _ => String::new(),
            }
        })
    };
    let mode = move || {
        if pick.with(|p| p.picking()) {
            return ("picking", "pick the other end");
        }
        selected.with(|s| match s {
            None => ("none", "no filter"),
            Some(s) if s.relative.is_some() => ("relative", "relative · moves with the clock"),
            Some(_) => ("fixed", "fixed range"),
        })
    };
    let length = move || {
        let picked = pick.with(|p| p.span(pointed.get()));
        match picked {
            Some((a, b)) if pick.with(|p| p.picking()) => {
                calendar::length(End { day: a, time: None }, End { day: b, time: None })
            }
            _ => match selected.with(|s| ends_of(s.as_ref())) {
                (Some(f), Some(u)) => calendar::length(f, u),
                _ => "—".to_string(),
            },
        }
    };
    let bad_day = move |s: RwSignal<String>| {
        move || s.with(|t| !t.trim().is_empty() && calendar::parse_day(t).is_none())
    };
    let bad_time = move |s: RwSignal<String>| {
        move || s.with(|t| !t.trim().is_empty() && calendar::parse_time(t).is_none())
    };
    let enter = move |e: web_sys::KeyboardEvent| {
        if e.key() == "Enter" {
            e.prevent_default();
            commit();
            close();
        }
    };
    let input = move |text: RwSignal<String>,
                      label: &'static str,
                      placeholder: &'static str,
                      class: &'static str,
                      bad: Signal<bool>| {
        view! {
            <input class=class class:bad=move || bad.get() aria-label=label placeholder=placeholder
                spellcheck="false" autocomplete="off" aria-invalid=move || bad.get().to_string()
                prop:value=move || text.get()
                on:input=move |e| text.set(event_target_value(&e))
                on:change=move |_| commit()
                on:keydown=enter />
        }
    };
    let bound = move |name: &'static str, day: RwSignal<String>, time: RwSignal<String>| {
        let day_label = if name == "From" {
            "First day"
        } else {
            "Last day"
        };
        let time_label = if name == "From" {
            "From time"
        } else {
            "Until time"
        };
        let placeholder = if name == "From" { "00:00" } else { "24:00" };
        view! {
            <span class="picker-bound-name">{name}</span>
            {input(day, day_label, "YYYY-MM-DD", "picker-input", Signal::derive(bad_day(day)))}
            {timed.then(|| input(time, time_label, placeholder, "picker-input picker-time", Signal::derive(bad_time(time))))}
        }
    };

    view! {
        <section class="filter date-range" data-field=field.get_value()>
            <header class="filter-head">
                <h2>{title}</h2>
                <span class="filter-meta">{if timed { "timestamp · UTC" } else { "date · UTC" }}</span>
            </header>
            <div class="date-picker" node_ref=root>
                <button type="button" class="picker-trigger" node_ref=trigger
                    class:chosen=move || selected.with(Option::is_some) class:open=move || open.get()
                    aria-haspopup="dialog" aria-expanded=move || open.get().to_string() on:click=toggle>
                    <svg class="picker-icon" viewBox="0 0 16 16" aria-hidden="true">
                        <rect x="2" y="3" width="12" height="11" rx="2" />
                        <path d="M2 6.5h12M5.5 1.5v3M10.5 1.5v3" />
                    </svg>
                    <span class="picker-label">{label}</span>
                    <span class="picker-span">{span}</span>
                    <svg class="picker-chevron" viewBox="0 0 10 10" aria-hidden="true"><path d="M2 3.5 5 6.5 8 3.5" /></svg>
                </button>
                <Show when=move || open.get()>
                    <div class="picker-pop" role="dialog" aria-label="Choose a period" on:keydown=escape>
                        <div class="picker-body">
                            <div class="picker-presets" role="group" aria-label="Relative periods">
                                <span class="picker-heading">"Relative"</span>
                                {windows.get_value().into_iter().map(|w| {
                                    let chosen = move || selected.with(|s| s.as_ref().and_then(|s| s.relative) == Some(w));
                                    view! {
                                        <button type="button" class="picker-preset" class:chosen=chosen
                                            aria-pressed=move || chosen().to_string()
                                            on:click=move |_| {
                                                live.send(&json!({"type": "select_relative", "field": field.get_value(), "window": w}));
                                                close();
                                            }>
                                            <span>{w.label()}</span>
                                            <code>{calendar::window_code(&w)}</code>
                                        </button>
                                    }
                                }).collect_view()}
                                <button type="button" class="picker-preset" class:chosen=move || selected.with(Option::is_none)
                                    aria-pressed=move || selected.with(Option::is_none).to_string() on:click=clear>
                                    <span>"All time"</span>
                                    <code>"∞"</code>
                                </button>
                            </div>
                            <div class="picker-calendar">
                                <div class="picker-month">
                                    <button type="button" class="picker-step" aria-label="Previous month"
                                        on:click=move |_| month.update(|m| *m = m.add(-1))>
                                        <svg viewBox="0 0 12 12" aria-hidden="true"><path d="M7.5 2.5 4 6l3.5 3.5" /></svg>
                                    </button>
                                    <strong aria-live="polite">{move || month.get().label()}</strong>
                                    <button type="button" class="picker-step" aria-label="Next month"
                                        on:click=move |_| month.update(|m| *m = m.add(1))>
                                        <svg viewBox="0 0 12 12" aria-hidden="true"><path d="M4.5 2.5 8 6 4.5 9.5" /></svg>
                                    </button>
                                    <button type="button" class="picker-today"
                                        on:click=move |_| { month.set(Month::of(now)); cursor.set(now); }>"Today"</button>
                                </div>
                                <div class="picker-grid" node_ref=grid on:keydown=key
                                    on:mouseleave=move |_| if pick.get_untracked().picking() { pointed.set(None) }>
                                    {WEEKDAYS.iter().map(|d| view! { <span class="picker-weekday" aria-hidden="true">{*d}</span> }).collect_view()}
                                    {move || {
                                        let shown = month.get();
                                        shown.grid().into_iter().map(|n| {
                                            let span = move || pick.with(|p| p.span(pointed.get()));
                                            let inside = move || span().is_some_and(|(a, b)| (a..=b).contains(&n));
                                            view! {
                                                <button type="button" class="picker-day"
                                                    class:outside=!shown.contains(n) class:today=n == now
                                                    class:inside=inside
                                                    class:first=move || span().is_some_and(|(a, _)| a == n)
                                                    class:last=move || span().is_some_and(|(_, b)| b == n)
                                                    class:tentative=move || pick.with(|p| p.picking())
                                                    data-day=n.to_string() aria-label=calendar::iso(n)
                                                    aria-pressed=move || inside().to_string()
                                                    tabindex=move || if cursor.get() == n { "0" } else { "-1" }
                                                    on:click=move |_| click(n)
                                                    on:mouseenter=move |_| if pick.get_untracked().picking() { pointed.set(Some(n)) }>
                                                    {calendar::day_of_month(n)}
                                                </button>
                                            }
                                        }).collect_view()
                                    }}
                                </div>
                                <div class="picker-bounds" class:timed=timed>
                                    {bound("From", from_day, from_time)}
                                    {bound("Until", until_day, until_time)}
                                </div>
                            </div>
                        </div>
                        <footer class="picker-foot">
                            <span class=move || format!("picker-mode {}", mode().0)>{move || mode().1}</span>
                            <code>{length}</code>
                            <code class="quiet">"UTC"</code>
                            <span class="picker-keys"><kbd>"↵"</kbd>" apply "<kbd>"esc"</kbd>" close"</span>
                            <button type="button" class="picker-clear" on:click=clear>"Clear"</button>
                        </footer>
                    </div>
                </Show>
            </div>
        </section>
    }
}
