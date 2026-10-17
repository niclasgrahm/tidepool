//! The pages that aren't a dashboard: the dashboard list, the model
//! browser, and the status page.

use leptos::prelude::*;
use serde_json::json;
use tidepool_protocol::{ModelInfo, Preview, StatusInfo, TableInfo};

use crate::api::get_json;
use crate::board::{bytes, display, sample_row};
use crate::live::Live;
use crate::relationships::Relationships;
use crate::route::{Route, Tab};

#[component]
pub fn Home() -> impl IntoView {
    let live = expect_context::<Live>();
    view! {
        <main class="page-single">
            <div class="title-row">
                <h1 class="page-title">"Dashboards"</h1>
                <a class="button primary" href=Route::NewDashboard.href()>"New dashboard"</a>
            </div>
            {move || {
                let all = live.dashboards.get();
                if all.is_empty() {
                    return view! {
                        <p class="quiet">"No dashboards yet. Make one here, or add a file to the config's "<code>"dashboards/"</code>" directory."</p>
                    }.into_any();
                }
                view! {
                    <ul class="cards">
                        {all.into_iter().map(|d| view! {
                            <li>
                                <a href=Route::Dashboard(d.id.clone()).href()>
                                    <strong>{d.name}</strong>
                                    <span class="quiet">{format!("{} visuals, {} filters", d.visuals.len(), d.filters.len())}</span>
                                </a>
                            </li>
                        }).collect_view()}
                    </ul>
                }.into_any()
            }}
        </main>
    }
}

#[component]
fn ModelTabs(at: Tab) -> impl IntoView {
    let tab = |tab: Tab, label: &'static str| {
        view! {
            <a href=Route::Model(tab).href() class:current=at == tab aria-current=(at == tab).then_some("page")>{label}</a>
        }
    };
    view! {
        <nav class="tabs" aria-label="Model">
            {tab(Tab::Tables, "Tables")}
            {tab(Tab::Relationships, "Relationships")}
            {tab(Tab::Metrics, "Metrics")}
        </nav>
    }
}

#[component]
pub fn Model(tab: Tab) -> impl IntoView {
    view! {
        <main class="page-single" class:wide=tab == Tab::Relationships>
            <h1 class="page-title">"Model"</h1>
            <ModelTabs at=tab />
            {match tab {
                Tab::Tables => view! { <Tables /> }.into_any(),
                Tab::Relationships => view! { <Relationships /> }.into_any(),
                Tab::Metrics => view! { <Metrics /> }.into_any(),
            }}
        </main>
    }
}

#[component]
fn Tables() -> impl IntoView {
    let tables = LocalResource::new(|| get_json::<Vec<TableInfo>>("/v1/tables"));
    view! {
        <Suspense fallback=|| view! { <p class="quiet">"Loading…"</p> }>
            {move || tables.get().map(|r| match r {
                Err(err) => view! { <p class="problem">{err}</p> }.into_any(),
                Ok(tables) => view! {
                    <table class="listing">
                        <thead><tr><th>"Table"</th><th>"Mode"</th><th class="num">"Rows"</th><th class="num">"Rows/s"</th><th class="num">"Memory"</th></tr></thead>
                        <tbody>
                            {tables.into_iter().map(|t| view! {
                                <tr>
                                    <td><a href=Route::Table(t.name.clone()).href()>{t.name.clone()}</a></td>
                                    <td>{t.mode}</td>
                                    <td class="num">{display(&json!(t.rows))}</td>
                                    <td class="num">{format!("{:.0}", t.rows_per_second)}</td>
                                    <td class="num">{bytes(t.bytes)}</td>
                                </tr>
                            }).collect_view()}
                        </tbody>
                    </table>
                }.into_any(),
            })}
        </Suspense>
    }
}

#[component]
pub fn TablePage(name: String) -> impl IntoView {
    let info = {
        let name = name.clone();
        LocalResource::new(move || {
            let name = name.clone();
            async move { get_json::<TableInfo>(&format!("/v1/tables/{name}")).await }
        })
    };
    let preview = {
        let name = name.clone();
        LocalResource::new(move || {
            let name = name.clone();
            async move { get_json::<Preview>(&format!("/v1/tables/{name}/preview?limit=20")).await }
        })
    };
    let origin = web_sys::window()
        .and_then(|w| w.location().origin().ok())
        .unwrap_or_default();
    view! {
        <main class="page-single">
            <p><a href=Route::Model(Tab::Tables).href()>"Tables"</a></p>
            <h1 class="page-title">{name}</h1>
            <Suspense fallback=|| view! { <p class="quiet">"Loading…"</p> }>
                {move || info.get().map(|r| match r {
                    Err(err) => view! { <p class="problem">{err}</p> }.into_any(),
                    Ok(t) => {
                        let sample = sample_row(&t.columns.iter().map(|c| (c.name.clone(), c.column_type.clone())).collect::<Vec<_>>());
                        let curl = format!(
                            "curl -X POST {origin}/v1/tables/{}/rows \\\n  -H 'content-type: application/json' \\\n  -d '[{}]'",
                            t.name, sample
                        );
                        view! {
                            <p class="quiet">{format!(
                                "{} table · {} rows · key {}",
                                t.mode, display(&json!(t.rows)),
                                if t.primary_key.is_empty() { "none".to_string() } else { t.primary_key.join(", ") }
                            )}</p>
                            <h2>"Columns"</h2>
                            <table class="listing">
                                <thead><tr><th>"Column"</th><th>"Type"</th><th>"Encoding"</th><th>"Nullable"</th></tr></thead>
                                <tbody>
                                    {t.columns.into_iter().map(|c| view! {
                                        <tr><td>{c.name}</td><td><code>{c.column_type}</code></td><td>{c.encoding}</td><td>{if c.nullable { "yes" } else { "no" }}</td></tr>
                                    }).collect_view()}
                                </tbody>
                            </table>
                            <h2>"Sending rows"</h2>
                            <p>"Post rows as a JSON array, NDJSON or an Arrow stream. Every column is described in the "
                               <a href="/v1/openapi.json">"OpenAPI document"</a>"."</p>
                            <pre class="code">{curl}</pre>
                        }.into_any()
                    }
                })}
            </Suspense>
            <h2>"Newest rows"</h2>
            <Suspense fallback=|| view! { <p class="quiet">"Loading…"</p> }>
                {move || preview.get().map(|r| match r {
                    Err(err) => view! { <p class="problem">{err}</p> }.into_any(),
                    Ok(p) if p.rows.is_empty() => view! { <p class="quiet">"No rows yet."</p> }.into_any(),
                    Ok(p) => view! {
                        <div class="table-wrap">
                            <table class="listing">
                                <thead><tr>{p.columns.into_iter().map(|c| view! { <th>{c}</th> }).collect_view()}</tr></thead>
                                <tbody>
                                    {p.rows.into_iter().map(|row| view! {
                                        <tr>{row.into_iter().map(|v| view! { <td>{display(&v)}</td> }).collect_view()}</tr>
                                    }).collect_view()}
                                </tbody>
                            </table>
                        </div>
                    }.into_any(),
                })}
            </Suspense>
        </main>
    }
}

#[component]
fn Metrics() -> impl IntoView {
    let model = LocalResource::new(|| get_json::<ModelInfo>("/v1/model"));
    view! {
        <Suspense fallback=|| view! { <p class="quiet">"Loading…"</p> }>
            {move || model.get().map(|r| match r {
                Err(err) => view! { <p class="problem">{err}</p> }.into_any(),
                Ok(model) => view! {
                    <table class="listing metrics">
                        <thead><tr><th>"Metric"</th><th>"SQL"</th><th>"Rows of"</th><th>"Shown on"</th></tr></thead>
                        <tbody>
                            {model.metrics.into_iter().map(|m| {
                                let mut notes = Vec::new();
                                if !m.ignores.is_empty() {
                                    notes.push(format!("ignores the selection on {}", m.ignores.join(", ")));
                                }
                                if let Some(t) = &m.time_offset {
                                    notes.push(format!("shifted {t}"));
                                }
                                view! {
                                    <tr>
                                        <td>
                                            <strong>{m.name}</strong>
                                            {m.description.map(|d| view! { <p class="quiet">{d}</p> })}
                                            {(!notes.is_empty()).then(|| view! { <p class="quiet">{notes.join("; ")}</p> })}
                                        </td>
                                        <td><code>{m.expression}</code></td>
                                        <td>{m.anchor.unwrap_or_else(|| "combines other metrics".into())}</td>
                                        <td>{if m.dashboards.is_empty() {
                                            view! { <span class="quiet">"none"</span> }.into_any()
                                        } else {
                                            m.dashboards.into_iter().map(|d| view! {
                                                <a href=Route::Dashboard(d.clone()).href()>{d.clone()}</a>" "
                                            }).collect_view().into_any()
                                        }}</td>
                                    </tr>
                                }
                            }).collect_view()}
                        </tbody>
                    </table>
                }.into_any(),
            })}
        </Suspense>
    }
}

#[component]
pub fn Status() -> impl IntoView {
    let live = expect_context::<Live>();
    // refresh with the clock, every couple of seconds
    let tick = Memo::new(move |_| (live.clock.get() / 2000.0).floor());
    let status = LocalResource::new(move || {
        tick.track();
        get_json::<StatusInfo>("/v1/status")
    });
    view! {
        <main class="page-single">
            <h1 class="page-title">"Status"</h1>
            <Transition fallback=|| view! { <p class="quiet">"Loading…"</p> }>
                {move || status.get().map(|r| match r {
                    Err(err) => view! { <p class="problem">{err}</p> }.into_any(),
                    Ok(s) => {
                        let data: u64 = s.tables.iter().map(|t| t.bytes).sum();
                        view! {
                            <dl class="facts">
                                <div><dt>"Epoch"</dt><dd>{display(&json!(s.epoch))}</dd></div>
                                <div><dt>"Up for"</dt><dd>{format!("{} min", s.uptime_seconds / 60)}</dd></div>
                                <div><dt>"Live sessions"</dt><dd>{s.sessions}</dd></div>
                                <div><dt>"Table data"</dt><dd>{bytes(data)}</dd></div>
                                <div><dt>"Dictionaries"</dt><dd>{bytes(s.dictionary_bytes)}</dd></div>
                                <div><dt>"Caches"</dt><dd>{bytes(s.cache_bytes)}</dd></div>
                                <div><dt>"Last epoch published in"</dt><dd>{format!("{:.1} ms", s.epoch_publish_ms)}</dd></div>
                                <div><dt>"Live sessions refreshed in"</dt><dd>{format!("{:.1} ms", s.live_refresh_ms)}</dd></div>
                                <div><dt>"Config version"</dt><dd><a href=Route::Config(crate::route::ConfigTab::History).href()>{format!("v{}", s.config_version)}</a></dd></div>
                            </dl>
                            {s.rebuild.map(|r| view! {
                                <h2>"Background work"</h2>
                                <p>{format!("Rebuilding data for config version {}: {} of {} rows.", r.version, display(&json!(r.rows_done)), display(&json!(r.rows)))}</p>
                            })}
                            <h2>"Tables"</h2>
                            <table class="listing">
                                <thead><tr><th>"Table"</th><th class="num">"Rows"</th><th class="num">"Segments"</th><th class="num">"Rows/s"</th><th class="num">"Memory"</th></tr></thead>
                                <tbody>
                                    {s.tables.into_iter().map(|t| view! {
                                        <tr>
                                            <td>{t.name}</td>
                                            <td class="num">{display(&json!(t.rows))}</td>
                                            <td class="num">{t.segments}</td>
                                            <td class="num">{format!("{:.0}", t.rows_per_second)}</td>
                                            <td class="num">{bytes(t.bytes)}</td>
                                        </tr>
                                    }).collect_view()}
                                </tbody>
                            </table>
                            <h2>"Durability"</h2>
                            {match s.durability {
                                None => view! { <p class="problem">"No data directory: data lives in memory and is gone when the server stops. Start it with "<code>"--data"</code>"."</p> }.into_any(),
                                Some(d) => view! {
                                    <dl class="facts">
                                        <div><dt>"Data directory"</dt><dd><code>{d.data_dir}</code></dd></div>
                                        <div><dt>"Acknowledged after"</dt><dd>{if d.mode == "fsync" { "fsync" } else { "write (async)" }}</dd></div>
                                        <div><dt>"Writes logged"</dt><dd>{display(&json!(d.last_lsn))}</dd></div>
                                        <div><dt>"In the last checkpoint"</dt><dd>{display(&json!(d.checkpoint_lsn))}</dd></div>
                                        <div><dt>"Checkpoint written"</dt><dd>{d.checkpoint_age_seconds.map_or_else(|| "not since the start".into(), |a| format!("{a} s ago"))}</dd></div>
                                    </dl>
                                }.into_any(),
                            }}
                        }.into_any()
                    }
                })}
            </Transition>
        </main>
    }
}
