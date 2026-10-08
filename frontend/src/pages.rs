//! The pages that aren't a dashboard: the dashboard list, the model
//! browser, and the status page.

use leptos::prelude::*;
use serde_json::json;
use tidepool_protocol::{ModelInfo, Preview, StatusInfo, TableInfo};

use crate::api::get_json;
use crate::board::{bytes, display, layers, sample_row};
use crate::live::Live;
use crate::route::{Route, Tab};

#[component]
pub fn Home() -> impl IntoView {
    let live = expect_context::<Live>();
    view! {
        <main class="page-single">
            <h1 class="page-title">"Dashboards"</h1>
            {move || {
                let all = live.dashboards.get();
                if all.is_empty() {
                    return view! {
                        <p class="quiet">"No dashboards yet. Add a file to the config's "<code>"dashboards/"</code>" directory."</p>
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
        <main class="page-single">
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

const BOX_W: f64 = 180.0;
const BOX_H: f64 = 44.0;
const GAP_X: f64 = 230.0;
const GAP_Y: f64 = 36.0;

#[component]
fn Relationships() -> impl IntoView {
    let model = LocalResource::new(|| get_json::<ModelInfo>("/v1/model"));
    view! {
        <Suspense fallback=|| view! { <p class="quiet">"Loading…"</p> }>
            {move || model.get().map(|r| match r {
                Err(err) => view! { <p class="problem">{err}</p> }.into_any(),
                Ok(model) => {
                    let mut tables: Vec<String> = model.datasets.iter().map(|d| d.table.clone()).collect();
                    for r in &model.relationships {
                        for t in [&r.from, &r.to] {
                            if !tables.contains(t) {
                                tables.push(t.clone());
                            }
                        }
                    }
                    let edges: Vec<(String, String)> = model.relationships.iter().map(|r| (r.from.clone(), r.to.clone())).collect();
                    let placed = layers(&tables, &edges);
                    let at = |name: &str| placed.iter().find(|(t, _, _)| t == name).map_or((0.0, 0.0), |(_, c, r)| {
                        #[allow(clippy::cast_precision_loss)]
                        let (c, r) = (*c as f64, *r as f64);
                        (c * (BOX_W + GAP_X), r * (BOX_H + GAP_Y))
                    });
                    #[allow(clippy::cast_precision_loss)]
                    let width = placed.iter().map(|(_, c, _)| *c).max().unwrap_or(0) as f64 * (BOX_W + GAP_X) + BOX_W + 2.0;
                    #[allow(clippy::cast_precision_loss)]
                    let height = placed.iter().map(|(_, _, r)| *r).max().unwrap_or(0) as f64 * (BOX_H + GAP_Y) + BOX_H + 2.0;
                    view! {
                        <p class="quiet">"Arrows point from the many side to the one side: selections flow along them, from the one side to the many."</p>
                        <svg class="diagram" viewBox=format!("-1 -1 {width} {height}") width=width.to_string() role="img"
                             aria-label="How the tables relate">
                            <defs>
                                <marker id="arrow" viewBox="0 0 10 10" refX="10" refY="5" markerWidth="8" markerHeight="8" orient="auto-start-reverse">
                                    <path d="M 0 0 L 10 5 L 0 10 z" />
                                </marker>
                            </defs>
                            {model.relationships.iter().map(|r| {
                                let (fx, fy) = at(&r.from);
                                let (tx, ty) = at(&r.to);
                                let label = format!("{} → {}", r.from_columns.join(", "), r.to_columns.join(", "));
                                view! {
                                    <g class="edge">
                                        <line x1=(fx + BOX_W).to_string() y1=(fy + BOX_H / 2.0).to_string()
                                              x2=tx.to_string() y2=(ty + BOX_H / 2.0).to_string() marker-end="url(#arrow)" />
                                        <text x=(f64::midpoint(fx + BOX_W, tx)).to_string() y=(f64::midpoint(fy, ty) + BOX_H / 2.0 - 6.0).to_string()
                                              text-anchor="middle">{label}</text>
                                    </g>
                                }
                            }).collect_view()}
                            {placed.iter().map(|(t, _, _)| {
                                let (x, y) = at(t);
                                view! {
                                    <g class="node">
                                        <rect x=x.to_string() y=y.to_string() width=BOX_W.to_string() height=BOX_H.to_string() rx="4" />
                                        <text x=(x + BOX_W / 2.0).to_string() y=(y + BOX_H / 2.0 + 5.0).to_string() text-anchor="middle">{t.clone()}</text>
                                    </g>
                                }
                            }).collect_view()}
                        </svg>
                    }.into_any()
                }
            })}
        </Suspense>
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
