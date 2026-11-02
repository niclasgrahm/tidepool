//! The command palette: ⌘K (Ctrl+K) or the search box in the top bar
//! opens it; typing narrows [`crate::commands`], arrows move, Enter goes.

use leptos::{ev, html, prelude::*};
use tidepool_protocol::TableInfo;
use wasm_bindgen::JsCast;

use crate::api::get_json;
use crate::commands::{Command, Target, commands, rank};
use crate::live::Live;

/// Whether this page runs on a Mac, for the shortcut's name.
fn mac() -> bool {
    web_sys::window()
        .and_then(|w| w.navigator().platform().ok())
        .is_some_and(|p| p.starts_with("Mac"))
}

fn element(selector: &str) -> Option<web_sys::HtmlElement> {
    web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.query_selector(selector).ok().flatten())
        .and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok())
}

/// Do what a command says.
fn run(target: &Target) {
    match target {
        Target::Go(href) => {
            if let Some(w) = web_sys::window() {
                let _ = w.location().set_hash(href);
            }
        }
        Target::Filter(field) => {
            let at = format!("[data-field=\"{field}\"]");
            // its search box, or a date range's trigger
            let found = element(&format!("{at} input[type=search]"))
                .or_else(|| element(&format!("{at} .picker-trigger")));
            if let Some(e) = found {
                e.scroll_into_view_with_bool(false);
                let _ = e.focus();
            }
        }
        Target::Visual(id) => {
            if let Some(e) = element(&format!("[data-visual=\"{id}\"]")) {
                e.scroll_into_view_with_bool(true);
                let _ = e.class_list().add_1("flash");
                request_animation_frame(move || {
                    let e = e.clone();
                    set_timeout(
                        move || {
                            let _ = e.class_list().remove_1("flash");
                        },
                        std::time::Duration::from_millis(900),
                    );
                });
            }
        }
    }
}

/// The search box that opens the palette, and the palette itself.
#[allow(clippy::too_many_lines)] // one control: its keys, its list, its dialog
#[component]
pub fn Palette() -> impl IntoView {
    let live = expect_context::<Live>();
    let open = RwSignal::new(false);
    let query = RwSignal::new(String::new());
    let active = RwSignal::new(0_usize);
    let tables = RwSignal::new(Vec::<String>::new());
    let input = NodeRef::<html::Input>::new();

    let show = move || {
        query.set(String::new());
        active.set(0);
        open.set(true);
        if tables.with_untracked(Vec::is_empty) {
            leptos::task::spawn_local(async move {
                if let Ok(list) = get_json::<Vec<TableInfo>>("/v1/tables").await {
                    tables.set(list.into_iter().map(|t| t.name).collect());
                }
            });
        }
        request_animation_frame(move || {
            if let Some(i) = input.get_untracked() {
                let _ = i.focus();
            }
        });
    };
    let shortcut = window_event_listener(ev::keydown, move |e| {
        if e.key().eq_ignore_ascii_case("k") && (e.meta_key() || e.ctrl_key()) {
            e.prevent_default();
            if open.get_untracked() {
                open.set(false);
            } else {
                show();
            }
        }
    });
    on_cleanup(move || shortcut.remove());

    let all = Memo::new(move |_| {
        let open_board = live.open.get();
        live.dashboards
            .with(|d| tables.with(|t| commands(d, open_board.as_deref(), t)))
    });
    let shown = Memo::new(move |_| {
        let q = query.get();
        all.with(|all| {
            rank(all, &q)
                .into_iter()
                .take(40)
                .map(|i| all[i].clone())
                .collect::<Vec<Command>>()
        })
    });
    let choose = move |i: usize| {
        if let Some(c) = shown.with_untracked(|s| s.get(i).cloned()) {
            open.set(false);
            // after the dialog is gone, so focus can land
            request_animation_frame(move || run(&c.target));
        }
    };
    let key = move |e: web_sys::KeyboardEvent| {
        let count = shown.with_untracked(Vec::len);
        match e.key().as_str() {
            "ArrowDown" => {
                e.prevent_default();
                active.update(|a| *a = if count == 0 { 0 } else { (*a + 1) % count });
            }
            "ArrowUp" => {
                e.prevent_default();
                active.update(|a| {
                    *a = if count == 0 {
                        0
                    } else {
                        (*a + count - 1) % count
                    }
                });
            }
            "Enter" => {
                e.prevent_default();
                choose(active.get_untracked());
            }
            "Escape" => {
                e.prevent_default();
                open.set(false);
            }
            _ => {}
        }
    };
    let shortcut_name = if mac() { "⌘K" } else { "Ctrl K" };

    view! {
        <button type="button" class="search-trigger" on:click=move |_| show() aria-label="Search dashboards, filters, tables and pages">
            <svg viewBox="0 0 16 16" aria-hidden="true"><circle cx="7" cy="7" r="4.5" /><path d="M10.5 10.5 14 14" /></svg>
            <span>"Search…"</span>
            <kbd>{shortcut_name}</kbd>
        </button>
        <Show when=move || open.get()>
            <div class="palette-backdrop" on:pointerdown=move |e| {
                // a press on the backdrop itself, not the box
                if e.target() == e.current_target() { open.set(false); }
            }>
                <div class="palette" role="dialog" aria-modal="true" aria-label="Command palette" on:keydown=key>
                    <div class="palette-input">
                        <svg viewBox="0 0 16 16" aria-hidden="true"><circle cx="7" cy="7" r="4.5" /><path d="M10.5 10.5 14 14" /></svg>
                        <input node_ref=input placeholder="Search dashboards, filters, tables, pages…"
                            role="combobox" aria-expanded="true" aria-controls="palette-list"
                            aria-activedescendant=move || format!("palette-{}", active.get())
                            prop:value=move || query.get()
                            on:input=move |e| { query.set(event_target_value(&e)); active.set(0); } />
                        <kbd>"esc"</kbd>
                    </div>
                    <ul class="palette-list" id="palette-list" role="listbox">
                        {move || {
                            let found = shown.get();
                            if found.is_empty() {
                                return view! { <li class="palette-empty">"Nothing matches."</li> }.into_any();
                            }
                            let mut group = "";
                            found.into_iter().enumerate().map(|(i, c)| {
                                let heading = (c.group != group).then(|| view! { <li class="palette-group" role="presentation">{c.group}</li> });
                                group = c.group;
                                view! {
                                    {heading}
                                    <li id=format!("palette-{i}") role="option" class="palette-item"
                                        class:active=move || active.get() == i
                                        aria-selected=move || (active.get() == i).to_string()
                                        on:mousemove=move |_| if active.get_untracked() != i { active.set(i) }
                                        on:click=move |_| choose(i)>
                                        <span class="palette-label">{c.label}</span>
                                        <span class="palette-detail">{c.detail}</span>
                                    </li>
                                }
                            }).collect_view().into_any()
                        }}
                    </ul>
                    <footer class="palette-foot">
                        <span><kbd>"↑"</kbd><kbd>"↓"</kbd>" move"</span>
                        <span><kbd>"↵"</kbd>" go"</span>
                        <span><kbd>"esc"</kbd>" close"</span>
                    </footer>
                </div>
            </div>
        </Show>
    }
}
