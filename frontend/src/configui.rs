//! The config pages (plan §10.2, Config): the files edited in place with
//! the plan of the change checked as you type, applying it, the versions so
//! far with what each changed and a revert, and the changes waiting for
//! approval.
//!
//! Edits are kept as a draft (files to set or remove) against the version
//! they started from; the server plans and applies exactly that draft, so
//! what the page shows is what the server will do.

use std::collections::BTreeMap;

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::{Value, json};
use tidepool_protocol::config::{ConfigState, ConfigVersion, PendingChange, PlanInfo, VersionInfo};
use wasm_bindgen::JsCast;

use crate::api::{get_json, post_json};
use crate::live::Live;
use crate::route::{ConfigTab, Route};
use crate::textdiff::{Line, config_path, diff, hunks, offset_of_line};

/// Files to set (`Some`) or remove (`None`).
type Draft = BTreeMap<String, Option<String>>;

/// Wait `ms` milliseconds.
async fn sleep(ms: i32) {
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        if let Some(window) = web_sys::window() {
            let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms);
        }
    });
    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
}

/// The message of an error reply, or the reply itself.
fn error_of(reply: &Value) -> String {
    reply["error"]
        .as_str()
        .map_or_else(|| reply.to_string(), str::to_string)
}

/// The class of a change, as a badge.
fn badge(class: &str) -> impl IntoView + use<> {
    view! { <span class=format!("class-badge {class}")>{class.to_string()}</span> }
}

/// The config, reloaded with the clock (rebuild progress, edits in the
/// directory) and when the model changes.
#[derive(Clone, Copy)]
struct Shared {
    config: RwSignal<Option<Result<ConfigState, String>>>,
    reload: RwSignal<u64>,
}

impl Shared {
    fn refresh(self) {
        self.reload.update(|n| *n += 1);
    }

    fn version(self) -> u64 {
        self.config.with(|c| {
            c.as_ref()
                .and_then(|c| c.as_ref().ok())
                .map_or(0, |c| c.version)
        })
    }
}

#[component]
pub fn Config(tab: ConfigTab) -> impl IntoView {
    let live = expect_context::<Live>();
    let shared = Shared {
        config: RwSignal::new(None),
        reload: RwSignal::new(0),
    };
    let tick = Memo::new(move |_| (live.clock.get() / 2000.0).floor());
    Effect::new(move |_| {
        tick.track();
        shared.reload.track();
        live.model_version.track();
        spawn_local(async move {
            let got = get_json::<ConfigState>("/v1/config").await;
            if shared.config.with_untracked(|c| c.as_ref() != Some(&got)) {
                shared.config.set(Some(got));
            }
        });
    });
    let tabs = [
        (ConfigTab::Edit, "Files"),
        (ConfigTab::History, "History"),
        (ConfigTab::Changes, "Waiting for approval"),
    ];
    view! {
        <main class="page-single wide">
            <h1 class="page-title">"Config"</h1>
            <nav class="tabs" aria-label="Config">
                {tabs.into_iter().map(|(t, label)| view! {
                    <a href=Route::Config(t).href() class:current=t == tab>{label}</a>
                }).collect_view()}
            </nav>
            <Banner shared=shared />
            {match tab {
                ConfigTab::Edit => view! { <Editor shared=shared /> }.into_any(),
                ConfigTab::History => view! { <History shared=shared /> }.into_any(),
                ConfigTab::Changes => view! { <Changes shared=shared /> }.into_any(),
            }}
        </main>
    }
}

/// What's going on: the version, a rebuild in progress, and edits in the
/// config directory that aren't applied.
#[component]
fn Banner(shared: Shared) -> impl IntoView {
    move || {
        shared.config.with(|c| match c {
            None => view! { <p class="quiet">"Loading…"</p> }.into_any(),
            Some(Err(err)) => view! {
                <p class="problem">{format!("Can't read the config: {err}. Config pages need the admin token (open the page with ?token=…).")}</p>
            }.into_any(),
            Some(Ok(state)) => {
                let rebuild = state.rebuild.clone().map(|r| {
                    #[allow(clippy::cast_precision_loss)]
                    let share = if r.rows == 0 { 0.0 } else { r.rows_done as f64 / r.rows as f64 };
                    view! {
                        <div class="rebuild">
                            <p>{format!("Rebuilding for version {}: {} of {} rows. Version {} serves until it's done.", r.version, r.rows_done, r.rows, state.live_version)}</p>
                            <progress max="1" value=share.to_string()></progress>
                        </div>
                    }
                });
                let external = state.external.clone().map(|e| match (e.change, e.problems) {
                    (Some(id), _) => view! {
                        <p class="notice">
                            "The config directory has edits that change data. "
                            <a href=Route::Config(ConfigTab::Changes).href()>{format!("Change {id} is waiting for approval.")}</a>
                        </p>
                    }.into_any(),
                    (None, Some(problems)) => view! {
                        <div class="notice">
                            <p>"The config directory has edits that don't compile, so they aren't applied:"</p>
                            <pre class="code">{problems}</pre>
                        </div>
                    }.into_any(),
                    (None, None) => ().into_any(),
                });
                view! {
                    <p class="quiet">
                        {format!("Version {}", state.version)}
                        {if state.writeback { ". Applied changes are written back to the config directory." } else { ". Changes are not written back to a config directory." }}
                    </p>
                    {rebuild}
                    {external}
                }.into_any()
            }
        })
    }
}

/// The plan of a draft, as the server sees it.
#[derive(Clone, Debug, PartialEq)]
enum Checked {
    Nothing,
    Checking,
    Plan(PlanInfo),
    Failed(String),
}

#[component]
#[allow(clippy::too_many_lines)] // one page: files, editor, plan, actions
fn Editor(shared: Shared) -> impl IntoView {
    let draft = RwSignal::new(Draft::new());
    let base = RwSignal::new(0u64);
    let selected = RwSignal::new(Some("model.ossie.yaml".to_string()));
    let checked = RwSignal::new(Checked::Nothing);
    let generation = StoredValue::new(0u64);
    let outcome = RwSignal::new(None::<Result<String, String>>);
    let destroy_ok = RwSignal::new(false);
    let new_path = RwSignal::new(String::new());
    let editor = NodeRef::<leptos::html::Textarea>::new();

    // a draft is against the version it started from
    Effect::new(move |_| {
        let version = shared.version();
        if draft.with(BTreeMap::is_empty) {
            base.set(version);
        }
    });
    let server_text = move |path: &str| -> Option<String> {
        shared.config.with(|c| {
            c.as_ref().and_then(|c| c.as_ref().ok()).and_then(|c| {
                c.files
                    .iter()
                    .find(|f| f.path == path)
                    .map(|f| f.text.clone())
            })
        })
    };
    let text_of = move |path: &str| -> Option<String> {
        match draft.with(|d| d.get(path).cloned()) {
            Some(edited) => edited,
            None => server_text(path),
        }
    };
    let paths = Memo::new(move |_| {
        let mut paths: Vec<String> = shared.config.with(|c| {
            c.as_ref()
                .and_then(|c| c.as_ref().ok())
                .map(|c| c.files.iter().map(|f| f.path.clone()).collect())
                .unwrap_or_default()
        });
        draft.with(|d| {
            for path in d.keys() {
                if !paths.contains(path) {
                    paths.push(path.clone());
                }
            }
        });
        paths.sort();
        paths
    });

    // plan the draft a moment after the last keystroke
    Effect::new(move |_| {
        let files = draft.get();
        let version = base.get();
        let n = generation.get_value() + 1;
        generation.set_value(n);
        if files.is_empty() {
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
                &json!({ "base_version": version, "files": files }),
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
                Ok((409, value)) => Checked::Failed(format!(
                    "{} Your edits are against version {version}; discard them to start from the current one.",
                    error_of(&value)
                )),
                Ok((_, value)) => Checked::Failed(error_of(&value)),
                Err(err) => Checked::Failed(err),
            });
        });
    });

    let edit = move |text: String| {
        let Some(path) = selected.get_untracked() else {
            return;
        };
        let unchanged = server_text(&path).as_deref() == Some(text.as_str());
        draft.update(|d| {
            if unchanged {
                d.remove(&path);
            } else {
                d.insert(path, Some(text));
            }
        });
        outcome.set(None);
    };
    let jump = move |file: Option<String>, line: Option<usize>| {
        if let Some(file) = file {
            selected.set(Some(file));
        }
        let Some(line) = line else { return };
        // after the textarea shows the file
        spawn_local(async move {
            sleep(0).await;
            if let Some(area) = editor.get_untracked() {
                let at = u32::try_from(offset_of_line(&area.value(), line)).unwrap_or(0);
                let _ = area.focus();
                let _ = area.set_selection_range(at, at);
            }
        });
    };
    let submit = move |propose: bool| {
        let files = draft.get_untracked();
        let version = base.get_untracked();
        let approve = destroy_ok.get_untracked();
        outcome.set(Some(Ok("Applying…".into())));
        spawn_local(async move {
            let reply = post_json(
                "/v1/config/apply",
                &json!({
                    "base_version": version, "files": files,
                    "approve": approve, "propose": propose, "author": "the config page",
                }),
            )
            .await;
            match reply {
                Ok((200, value)) => {
                    let message = match value["status"].as_str() {
                        Some("applied") => format!("Version {} is live.", value["version"]),
                        Some("rebuilding") => format!(
                            "Version {} is being rebuilt in the background; the current version serves until it's done.",
                            value["version"]
                        ),
                        Some("queued") => format!(
                            "Proposed: change {} is waiting for approval.",
                            value["change"]
                        ),
                        _ => "Nothing to change.".to_string(),
                    };
                    draft.set(Draft::new());
                    destroy_ok.set(false);
                    outcome.set(Some(Ok(message)));
                    shared.refresh();
                }
                Ok((_, value)) => outcome.set(Some(Err(error_of(&value)))),
                Err(err) => outcome.set(Some(Err(err))),
            }
        });
    };

    let file_list = move || {
        paths.get().into_iter().map(|path| {
            let state = draft.with(|d| match d.get(&path) {
                None => "",
                Some(None) => "removed",
                Some(Some(_)) if server_text(&path).is_none() => "new",
                Some(Some(_)) => "edited",
            });
            let current = selected.get().as_deref() == Some(path.as_str());
            let (label, target) = (path.clone(), path.clone());
            view! {
                <li>
                    <button class="file" class:current=current on:click=move |_| selected.set(Some(target.clone()))>
                        <span>{label}</span>
                        {(!state.is_empty()).then(|| view! { <span class=format!("file-state {state}")>{state}</span> })}
                    </button>
                </li>
            }
        }).collect_view()
    };
    let add_file = move |_| {
        let path = new_path.get_untracked().trim().to_string();
        if !config_path(&path) {
            outcome.set(Some(Err(format!(
                "`{path}` can't be a config file: use tables/<name>.yaml or dashboards/<name>.yaml"
            ))));
            return;
        }
        let start = if path.starts_with("tables/") {
            let name = path.trim_start_matches("tables/").trim_end_matches(".yaml");
            format!("name: {name}\nmode: append\ncolumns:\n  id: {{ type: string }}\n")
        } else {
            let name = path
                .trim_start_matches("dashboards/")
                .trim_end_matches(".yaml");
            format!("name: {name}\nlayout: {{ columns: 12, row_height: 80 }}\nvisuals: []\n")
        };
        if text_of(&path).is_none() {
            draft.update(|d| {
                d.insert(path.clone(), Some(start));
            });
        }
        selected.set(Some(path));
        new_path.set(String::new());
    };

    let plan_view = move || match checked.get() {
        Checked::Nothing => {
            view! { <p class="quiet">"Edit a file: the plan of the change shows here."</p> }
                .into_any()
        }
        Checked::Checking => view! { <p class="quiet">"Checking…"</p> }.into_any(),
        Checked::Failed(message) => view! { <p class="problem">{message}</p> }.into_any(),
        Checked::Plan(plan) => {
            let problems = plan.problems.iter().map(|p| {
                let (file, line) = (p.file.clone(), p.line);
                let at = match (&p.file, p.line) {
                    (Some(f), Some(l)) => format!("{f}:{l}"),
                    (Some(f), None) => f.clone(),
                    _ => "the project".to_string(),
                };
                let help = p.help.clone();
                view! {
                    <li class=p.severity.clone()>
                        <button class="link" on:click=move |_| jump(file.clone(), line)>{at}</button>
                        " "{p.message.clone()}
                        {help.map(|h| view! { <span class="quiet">" — "{h}</span> })}
                    </li>
                }
            }).collect_view();
            let changes = plan.changes.iter().map(|c| view! {
                <li>
                    {badge(&c.class)}
                    <span class="op">{c.op.clone()}</span>" "{c.kind.clone()}" "<strong>{c.subject.clone()}</strong>
                    {c.detail.clone().map(|d| view! { " "<span>{d}</span> })}
                    {c.note.clone().map(|n| view! { <span class="quiet">" ("{n}")"</span> })}
                </li>
            }).collect_view();
            let warnings = plan
                .warnings
                .iter()
                .map(|w| view! { <li class="warning">{w.clone()}</li> })
                .collect_view();
            let destructive = plan.class.as_deref() == Some("destructive");
            let valid = plan.valid;
            let empty = plan.changes.is_empty();
            view! {
                {(!plan.problems.is_empty()).then(|| view! { <h3>"Problems"</h3><ul class="problems">{problems}</ul> })}
                {valid.then(|| view! {
                    <h3>"What it changes"</h3>
                    {if empty {
                        view! { <p class="quiet">"Nothing in the model: the files change, the data and dashboards don't."</p> }.into_any()
                    } else {
                        view! { <ul class="changes">{changes}</ul> }.into_any()
                    }}
                    <ul class="warnings">{warnings}</ul>
                    {destructive.then(|| view! {
                        <label class="confirm">
                            <input type="checkbox" prop:checked=move || destroy_ok.get()
                                on:change=move |ev| destroy_ok.set(event_target_checked(&ev)) />
                            " Apply it although it deletes data"
                        </label>
                    })}
                    <div class="actions">
                        <button class="primary" disabled=move || destructive && !destroy_ok.get() on:click=move |_| submit(false)>"Apply"</button>
                        <button on:click=move |_| submit(true)>"Propose for approval"</button>
                    </div>
                })}
            }.into_any()
        }
    };

    view! {
        <div class="config-edit">
            <aside class="files" aria-label="Config files">
                <ul>{file_list}</ul>
                <div class="new-file">
                    <input type="text" placeholder="tables/new.yaml" prop:value=move || new_path.get()
                        on:input=move |ev| new_path.set(event_target_value(&ev)) />
                    <button on:click=add_file>"Add"</button>
                </div>
            </aside>
            <section class="editor">
                {move || selected.get().map(|path| {
                    let removed = draft.with(|d| d.get(&path) == Some(&None));
                    let target = path.clone();
                    let restore = path.clone();
                    view! {
                        <div class="editor-head">
                            <code>{path.clone()}</code>
                            {if removed {
                                view! { <button on:click=move |_| draft.update(|d| { d.remove(&restore); })>"Keep the file"</button> }.into_any()
                            } else {
                                view! { <button on:click=move |_| draft.update(|d| { d.insert(target.clone(), None); })>"Remove the file"</button> }.into_any()
                            }}
                        </div>
                        {if removed {
                            view! { <p class="quiet">"Removed in your edits."</p> }.into_any()
                        } else {
                            let text = text_of(&path).unwrap_or_default();
                            view! {
                                <textarea node_ref=editor class="code-editor" spellcheck="false" wrap="off" aria-label=format!("Contents of {path}")
                                    prop:value=text on:input=move |ev| edit(event_target_value(&ev))></textarea>
                            }.into_any()
                        }}
                    }
                })}
            </section>
            <section class="plan" aria-live="polite">
                <div class="plan-head">
                    <h2>"Your changes"</h2>
                    <button disabled=move || draft.with(BTreeMap::is_empty) on:click=move |_| { draft.set(Draft::new()); outcome.set(None); }>"Discard"</button>
                </div>
                {move || outcome.get().map(|o| match o {
                    Ok(message) => view! { <p class="ok">{message}</p> }.into_any(),
                    Err(message) => view! { <p class="problem">{message}</p> }.into_any(),
                })}
                {plan_view}
            </section>
        </div>
    }
}

#[component]
fn History(shared: Shared) -> impl IntoView {
    let versions = LocalResource::new(move || {
        shared.reload.track();
        get_json::<Vec<VersionInfo>>("/v1/config/versions")
    });
    let open = RwSignal::new(None::<u64>);
    view! {
        <Transition fallback=|| view! { <p class="quiet">"Loading…"</p> }>
            {move || versions.get().map(|r| match r {
                Err(err) => view! { <p class="problem">{err}</p> }.into_any(),
                Ok(list) => {
                    let current = list.first().map_or(0, |v| v.version);
                    view! {
                        <ol class="versions" reversed>
                            {list.into_iter().map(|v| {
                                let number = v.version;
                                view! {
                                    <li class:open=move || open.get() == Some(number)>
                                        <button class="version" on:click=move |_| open.update(|o| *o = if *o == Some(number) { None } else { Some(number) })>
                                            <strong>{format!("v{number}")}</strong>
                                            <span>{v.summary.clone()}</span>
                                            {v.class.clone().map(|c| badge(&c))}
                                            <span class="quiet">{format!("{} · {} · {}", v.author, v.source, v.at.replace('T', " ").trim_end_matches('Z'))}</span>
                                        </button>
                                        {move || (open.get() == Some(number)).then(|| view! {
                                            <VersionDetail shared=shared version=number current=current />
                                        })}
                                    </li>
                                }
                            }).collect_view()}
                        </ol>
                    }.into_any()
                }
            })}
        </Transition>
    }
}

/// One version against the current one: what reverting to it changes.
#[component]
fn VersionDetail(shared: Shared, version: u64, current: u64) -> impl IntoView {
    let detail = LocalResource::new(move || async move {
        get_json::<ConfigVersion>(&format!("/v1/config/versions/{version}")).await
    });
    let outcome = RwSignal::new(None::<Result<String, String>>);
    let destroy_ok = RwSignal::new(false);
    let revert = move |files: BTreeMap<String, String>| {
        let approve = destroy_ok.get_untracked();
        spawn_local(async move {
            let reply = post_json(
                "/v1/config/apply",
                &json!({
                    "base_version": shared.version(), "files": files, "replace": true,
                    "approve": approve, "author": "the config page",
                }),
            )
            .await;
            outcome.set(Some(match reply {
                Ok((200, value)) => {
                    shared.refresh();
                    Ok(format!(
                        "Reverted: version {} has version {version}'s files.",
                        value["version"]
                    ))
                }
                Ok((_, value)) => Err(error_of(&value)),
                Err(err) => Err(err),
            }));
        });
    };
    view! {
        <Transition fallback=|| view! { <p class="quiet">"Loading…"</p> }>
            {move || detail.get().map(|r| match r {
                Err(err) => view! { <p class="problem">{err}</p> }.into_any(),
                Ok(v) => {
                    let then: BTreeMap<String, String> = v.files.iter().map(|f| (f.path.clone(), f.text.clone())).collect();
                    let now: BTreeMap<String, String> = shared.config.with_untracked(|c| {
                        c.as_ref().and_then(|c| c.as_ref().ok())
                            .map(|c| c.files.iter().map(|f| (f.path.clone(), f.text.clone())).collect())
                            .unwrap_or_default()
                    });
                    let mut paths: Vec<&String> = then.keys().chain(now.keys()).collect();
                    paths.sort();
                    paths.dedup();
                    let files = paths.into_iter().filter_map(|path| {
                        let (a, b) = (now.get(path).map_or("", String::as_str), then.get(path).map_or("", String::as_str));
                        if a == b {
                            return None;
                        }
                        let lines = diff(a, b);
                        let shown: Vec<_> = hunks(&lines, 2).into_iter().map(|l| match l {
                            None => view! { <div class="gap">"…"</div> }.into_any(),
                            Some(Line::Same(t)) => view! { <div class="same">{format!("  {t}")}</div> }.into_any(),
                            Some(Line::Added(t)) => view! { <div class="added">{format!("+ {t}")}</div> }.into_any(),
                            Some(Line::Removed(t)) => view! { <div class="removed">{format!("- {t}")}</div> }.into_any(),
                        }).collect();
                        Some(view! { <h4><code>{path.clone()}</code></h4><div class="diff">{shown}</div> })
                    }).collect_view();
                    let same = then == now;
                    view! {
                        <div class="version-detail">
                            {if same {
                                view! { <p class="quiet">"The current files are this version's."</p> }.into_any()
                            } else {
                                view! {
                                    <p class="quiet">{format!("Reverting to v{version} from v{current} changes:")}</p>
                                    {files}
                                    <label class="confirm">
                                        <input type="checkbox" prop:checked=move || destroy_ok.get()
                                            on:change=move |ev| destroy_ok.set(event_target_checked(&ev)) />
                                        " Apply it even if it deletes data"
                                    </label>
                                    <div class="actions">
                                        <button on:click=move |_| revert(then.clone())>{format!("Revert to v{version}")}</button>
                                    </div>
                                }.into_any()
                            }}
                            {move || outcome.get().map(|o| match o {
                                Ok(m) => view! { <p class="ok">{m}</p> }.into_any(),
                                Err(m) => view! { <p class="problem">{m}</p> }.into_any(),
                            })}
                        </div>
                    }.into_any()
                }
            })}
        </Transition>
    }
}

#[component]
fn Changes(shared: Shared) -> impl IntoView {
    let pending = LocalResource::new(move || {
        shared.reload.track();
        get_json::<Vec<PendingChange>>("/v1/changes")
    });
    let outcome = RwSignal::new(None::<Result<String, String>>);
    let act = move |id: u64, verb: &'static str| {
        spawn_local(async move {
            let reply = post_json(&format!("/v1/changes/{id}/{verb}"), &json!({})).await;
            outcome.set(Some(match reply {
                Ok((200, value)) if verb == "approve" => {
                    Ok(format!("Approved: version {} applied.", value["version"]))
                }
                Ok((200, _)) => Ok(format!("Change {id} rejected.")),
                Ok((_, value)) => Err(error_of(&value)),
                Err(err) => Err(err),
            }));
            shared.refresh();
        });
    };
    view! {
        {move || outcome.get().map(|o| match o {
            Ok(m) => view! { <p class="ok">{m}</p> }.into_any(),
            Err(m) => view! { <p class="problem">{m}</p> }.into_any(),
        })}
        <Transition fallback=|| view! { <p class="quiet">"Loading…"</p> }>
            {move || pending.get().map(|r| match r {
                Err(err) => view! { <p class="problem">{err}</p> }.into_any(),
                Ok(list) if list.is_empty() => view! {
                    <p class="quiet">"Nothing is waiting. Changes proposed through the API, and edits in the config directory that change data, wait here."</p>
                }.into_any(),
                Ok(list) => list.into_iter().map(|p| {
                    let id = p.id;
                    let files: Vec<String> = p.files.iter().map(|(path, text)| {
                        if text.is_some() { path.clone() } else { format!("{path} (removed)") }
                    }).collect();
                    view! {
                        <article class="pending">
                            <h3>
                                {format!("Change {id}")}" "
                                {p.plan.class.clone().map(|c| badge(&c))}
                            </h3>
                            <p class="quiet">{format!("By {} from {}, {}, against version {}", p.author, p.source, p.at.replace('T', " ").trim_end_matches('Z'), p.base_version)}</p>
                            <p>"Files: "<code>{files.join(", ")}</code></p>
                            <pre class="code">{p.plan.text.clone()}</pre>
                            <div class="actions">
                                <button class="primary" on:click=move |_| act(id, "approve")>"Approve and apply"</button>
                                <button on:click=move |_| act(id, "reject")>"Reject"</button>
                            </div>
                        </article>
                    }
                }).collect_view().into_any(),
            })}
        </Transition>
    }
}

/// Whether a checkbox event's target is checked.
fn event_target_checked(ev: &leptos::ev::Event) -> bool {
    ev.target()
        .and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok())
        .is_some_and(|i| i.checked())
}
