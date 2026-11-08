//! The MCP endpoint (plan §9.1): agents inspect the model, query metrics,
//! and plan, propose or apply changes.
//!
//! The transport is MCP's Streamable HTTP in its plainest form: JSON-RPC
//! 2.0 requests `POST`ed to `/mcp`, answered with one JSON body each (no
//! event stream; notifications get `202`). Hand-written rather than through
//! the `rmcp` crate: it's a few methods, and keeping the dependency out keeps
//! the server's stack one axum.
//!
//! Every tool is an in-process call of the documented HTTP API with the
//! caller's credentials, so a tool does exactly what its endpoint does,
//! checked the same way: `query_metrics` is `POST /v1/query`,
//! `apply_change` is `POST /v1/config/apply`, and so on. The one rule of its
//! own is the agents' policy: `apply_change` applies a change whose class
//! `agents.auto_apply` lists, and proposes anything else for a person to
//! approve. Resources are the config files' JSON Schemas
//! (`tidepool://schemas/<kind>`), read through `GET /v1/schemas/{kind}`.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::AppState;
use crate::auth::Listener;

/// Protocol versions this endpoint speaks, newest first.
const VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "Tidepool is a live analytics server: tables of data, a semantic \
model (datasets, relationships, metrics) and dashboards. Start with describe_model to see \
what exists, query_metrics to compute metrics grouped by fields under filters, \
read_rows for the newest rows that pass filters (a `contains` filter searches text, as in \
logs), and preview_table to look at rows. show_view shows an answer as a live view (drawn in \
the conversation where the host shows MCP Apps); save_view keeps it as a dashboard. To change the model, get_config for the files and their \
version, plan_change to see what an edit does, then apply_change: safe changes apply at \
once, others wait for a person's approval.";

/// The tools, as `tools/list` describes them. The doc site's reference is
/// rendered from the same list (`tidepool-docsgen`).
#[must_use]
#[allow(clippy::too_many_lines)] // one schema per tool, as a list
pub fn tools() -> Value {
    let files = json!({
        "type": "object",
        "description": "Config files to set (path → YAML text) or remove (path → null). \
                        Paths: tidepool.yaml, model.ossie.yaml, tables/<name>.yaml, \
                        dashboards/<name>.yaml.",
        "additionalProperties": { "type": ["string", "null"] },
    });
    json!([
        {
            "name": "describe_model",
            "title": "Describe the model",
            "description": "Tables with their columns and row counts, datasets and fields \
                            (with labels, grains and the model's ai_context: instructions, \
                            synonyms and examples), relationships, metrics with their SQL, \
                            dashboards and their visuals, the visual types there are, and how \
                            answers are computed (`semantics`). Read this first: names in \
                            queries are the ones it lists.",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": { "readOnlyHint": true },
        },
        {
            "name": "preview_table",
            "title": "Preview a table",
            "description": "The newest rows of a table.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "table": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 20 },
                },
                "required": ["table"],
            },
            "annotations": { "readOnlyHint": true },
        },
        {
            "name": "query_metrics",
            "title": "Query metrics",
            "description": "Metrics grouped by fields (`dataset.field`, time fields with a \
                            grain: minute, hour, day, week, month, quarter, year), under filters \
                            (`in` a list, `from`/`to` a half-open range, or `contains` a text, for a \
                            string field, ignoring the case of ASCII letters), sorted and \
                            limited. The body of POST /v1/query.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "metrics": { "type": "array", "items": { "type": "string" } },
                    "dimensions": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": { "field": { "type": "string" }, "grain": { "type": "string" } },
                            "required": ["field"],
                        },
                    },
                    "filters": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "field": { "type": "string" },
                                "in": { "type": "array" },
                                "from": {}, "to": {},
                                "contains": { "type": "string" },
                            },
                            "required": ["field"],
                        },
                    },
                    "sort": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": { "column": { "type": "string" }, "desc": { "type": "boolean" } },
                            "required": ["column"],
                        },
                    },
                    "limit": { "type": "integer", "minimum": 1 },
                },
                "required": ["metrics"],
            },
            "outputSchema": output_schema("QueryResult"),
            "annotations": { "readOnlyHint": true },
        },
        {
            "name": "read_rows",
            "title": "Read the newest rows",
            "description": "The newest rows of a table that pass filters, newest first: the \
                            fields to show (`dataset.field`, of one table and the tables it \
                            reaches), and filters as in query_metrics. A `contains` filter on \
                            a log line's text finds the lines that mention something. The \
                            body of POST /v1/rows.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "fields": { "type": "array", "items": { "type": "string" } },
                    "filters": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "field": { "type": "string" },
                                "in": { "type": "array" },
                                "from": {}, "to": {},
                                "contains": { "type": "string" },
                            },
                            "required": ["field"],
                        },
                    },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 100 },
                },
                "required": ["fields"],
            },
            "outputSchema": output_schema("QueryResult"),
            "annotations": { "readOnlyHint": true },
        },
        {
            "name": "show_view",
            "title": "Show a live view",
            "description": "Shows an answer as a view: a dashboard that lives only in memory, \
                            drawn and kept live by Tidepool, cross-filtered by clicks, and \
                            never written to the config. Visuals as in a dashboard file \
                            (`id`, `type`, `dimensions`, `metrics`, …; see visual_types in \
                            describe_model; `at` is optional), filters as in one. The answer \
                            has the view's id, its path in the UI (`/#/v/<id>`), and each \
                            visual's numbers as of now. Hosts that show MCP Apps draw it \
                            live in the conversation (ui://tidepool/view). The view is \
                            forgotten after a day unused; save_view keeps it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Its title." },
                    "visuals": { "type": "array", "items": { "type": "object" } },
                    "filters": { "type": "array", "items": { "type": "object" } },
                },
                "required": ["visuals"],
            },
            "annotations": { "readOnlyHint": true },
            "_meta": { "ui": { "resourceUri": crate::apps::VIEW_URI } },
        },
        {
            "name": "save_view",
            "title": "Save a view as a dashboard",
            "description": "Writes a view as dashboards/<id>.yaml, through the same policy as \
                            apply_change: added at once when agents may apply safe changes, \
                            otherwise waiting for a person. Needs the admin token.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "view": { "type": "string", "description": "The view's id, from show_view." },
                    "id": { "type": "string", "description": "The dashboard's id: its file name." },
                    "summary": { "type": "string", "description": "Who or what made it, for the history." },
                },
                "required": ["view", "id"],
            },
            "annotations": { "destructiveHint": true },
        },
        {
            "name": "get_config",
            "title": "Get the config",
            "description": "Every config file of the current version, and the version: the \
                            base for plan_change and apply_change. Needs the admin token.",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": { "readOnlyHint": true },
        },
        {
            "name": "plan_change",
            "title": "Plan a change",
            "description": "What setting or removing these files would do: problems (with \
                            file and line), and every change with its class (safe, rebuild, \
                            destructive). Changes nothing. Needs the admin token.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "files": files,
                    "base_version": { "type": "integer", "description": "Defaults to the current version." },
                },
                "required": ["files"],
            },
            "outputSchema": output_schema("PlanInfo"),
            "annotations": { "readOnlyHint": true },
        },
        {
            "name": "apply_change",
            "title": "Apply a change",
            "description": "Applies the change if its class is one agents may apply on their \
                            own (tidepool.yaml `agents.auto_apply`, safe by default); otherwise \
                            it waits for a person's approval and the result says which change \
                            to point them to. Needs the admin token.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "files": files,
                    "base_version": { "type": "integer" },
                    "summary": { "type": "string", "description": "Who or what made it, for the history." },
                },
                "required": ["files", "base_version"],
            },
            "annotations": { "destructiveHint": true },
        },
        {
            "name": "ingest_rows",
            "title": "Write rows",
            "description": "Writes rows (JSON objects, column → value) to a table; visible \
                            when the call returns. Needs the ingest token.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "table": { "type": "string" },
                    "rows": { "type": "array", "items": { "type": "object" } },
                },
                "required": ["table", "rows"],
            },
        },
    ])
}

/// The JSON Schema of a named API body, as a tool's `outputSchema`: the
/// shape of its structured content when it succeeds.
fn output_schema(name: &str) -> Value {
    let mut schema = tidepool_protocol::api_docs::schemas()
        .remove(name)
        .unwrap_or_else(|| json!({ "type": "object" }));
    if let Some(object) = schema.as_object_mut() {
        object.remove("$schema");
    }
    schema
}

/// How answers are computed: what an agent needs to know to read them
/// right, and to word a question as a query.
pub(crate) const SEMANTICS: [&str; 6] = [
    "Every number is computed by the engine from the stored rows; nothing is estimated.",
    "A metric aggregates its anchor table's rows. A selection on a field filters a metric when \
     the metric's table reaches the field through many-to-one relationships (orders reach \
     customers); with filter_direction both, a selection also narrows the tables it reaches.",
    "Dividing by zero gives NULL, not an error.",
    "A row whose related row doesn't exist reads that row's fields as NULL, as a LEFT JOIN \
     does.",
    "A query without dimensions answers one row; groups are the combinations of dimension \
     values that have rows.",
    "Metrics with Tidepool settings (ignore_selection, time_offset) mean something their plain \
     SQL expression doesn't: another tool reading the same Ossie file computes them without \
     the setting.",
];

/// A JSON-RPC error.
fn error(id: &Value, code: i64, message: impl Into<String>) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message.into() } })
}

#[allow(clippy::needless_pass_by_value)] // built in place at every call
fn result(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// A tool's answer: the JSON as text, and as structured content.
fn answer(mut value: Value, is_error: bool) -> Value {
    // what only the app showing the answer gets, not the model
    let meta = value.as_object_mut().and_then(|o| o.remove("_meta"));
    let text = serde_json::to_string_pretty(&value).unwrap_or_default();
    let mut out = json!({
        "content": [{ "type": "text", "text": text }],
        "isError": is_error,
    });
    if value.is_object() {
        out["structuredContent"] = value;
    }
    if let Some(meta) = meta {
        out["_meta"] = meta;
    }
    out
}

/// Who asked: the request's credentials and the listener it came in on,
/// which every call it makes goes through too.
struct Asker<'a> {
    headers: &'a HeaderMap,
    /// A `?token=` the request came with, for clients that can't set
    /// headers: passed on as the bearer token.
    token: Option<String>,
    listener: Listener,
}

/// One call of the API, in process, with the caller's credentials.
async fn api(
    state: &Arc<AppState>,
    caller: &Asker<'_>,
    method: Method,
    path: &str,
    body: Option<&Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(path);
    for name in [header::AUTHORIZATION, header::COOKIE] {
        if let Some(value) = caller.headers.get(&name) {
            request = request.header(name, value);
        }
    }
    if !caller.headers.contains_key(header::AUTHORIZATION)
        && let Some(token) = &caller.token
    {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    if body.is_some() {
        request = request.header(header::CONTENT_TYPE, "application/json");
    }
    let body = body.map_or_else(Body::empty, |b| Body::from(b.to_string()));
    let Ok(request) = request.body(body) else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error": "bad request"}),
        );
    };
    let router = crate::endpoints::api_router_on(Arc::clone(state), caller.listener);
    let Ok(response) = router.oneshot(request).await;
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .map(http_body_util::Collected::to_bytes)
        .unwrap_or_default();
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    (status, value)
}

/// Run a tool: its answer, or an error message for the agent.
async fn call(
    state: &Arc<AppState>,
    caller: &Asker<'_>,
    name: &str,
    args: &Value,
) -> Result<Value, String> {
    let ok = |(status, value): (StatusCode, Value)| {
        if status.is_success() {
            Ok(value)
        } else {
            Err(value["error"]
                .as_str()
                .map_or_else(|| value.to_string(), str::to_string))
        }
    };
    let text = |key: &str| {
        args[key]
            .as_str()
            .map(str::to_string)
            .ok_or(format!("`{key}` is required"))
    };
    match name {
        "describe_model" => {
            let model = ok(api(state, caller, Method::GET, "/v1/model", None).await)?;
            let tables = ok(api(state, caller, Method::GET, "/v1/tables", None).await)?;
            let snapshot = state.writer.snapshot();
            let dashboards: Vec<Value> = snapshot
                .model
                .dashboards
                .iter()
                .map(|d| {
                    let info = crate::live::dashboard_info(&snapshot.model, d);
                    json!({
                        "id": info.id,
                        "name": info.name,
                        "visuals": info.visuals.iter().map(|v| json!({
                            "id": v.id,
                            "type": v.visual_type,
                            "dimensions": v.dimensions,
                            "metrics": v.metrics,
                            "compare": v.compare,
                        })).collect::<Vec<_>>(),
                    })
                })
                .collect();
            let visual_types: Vec<Value> = tidepool_config::dashboard::VisualType::ALL
                .iter()
                .map(|t| json!({ "type": t.name(), "shape": t.shape() }))
                .collect();
            Ok(json!({
                "name": model["name"],
                "description": model["description"],
                "ai_context": model["ai_context"],
                "config_version": snapshot.version,
                "filter_direction": model["filter_direction"],
                "semantics": SEMANTICS,
                "tables": tables,
                "datasets": model["datasets"],
                "relationships": model["relationships"],
                "metrics": model["metrics"],
                "dashboards": dashboards,
                "visual_types": visual_types,
            }))
        }
        "preview_table" => {
            let table = text("table")?;
            let limit = args["limit"].as_u64().unwrap_or(20).clamp(1, 1000);
            let path = format!("/v1/tables/{table}/preview?limit={limit}");
            ok(api(state, caller, Method::GET, &path, None).await)
        }
        "query_metrics" => ok(api(state, caller, Method::POST, "/v1/query", Some(args)).await),
        "read_rows" => ok(api(state, caller, Method::POST, "/v1/rows", Some(args)).await),
        "get_config" => ok(api(state, caller, Method::GET, "/v1/config", None).await),
        "plan_change" => {
            let base = match args["base_version"].as_u64() {
                Some(v) => v,
                None => ok(api(state, caller, Method::GET, "/v1/config", None).await)?["version"]
                    .as_u64()
                    .unwrap_or(0),
            };
            let body = json!({ "base_version": base, "files": args["files"] });
            ok(api(state, caller, Method::POST, "/v1/config/plan", Some(&body)).await)
        }
        "apply_change" => apply_change(state, caller, args).await,
        "show_view" => show_view(state, caller, args).await,
        "save_view" => save_view(state, caller, args).await,
        "ingest_rows" => {
            let table = text("table")?;
            let path = format!("/v1/tables/{table}/rows?wait=visible");
            ok(api(state, caller, Method::POST, &path, Some(&args["rows"])).await)
        }
        other => Err(format!("no tool `{other}`")),
    }
}

/// `save_view`: the view's YAML as a dashboard file, through
/// [`apply_change`]'s policy.
async fn save_view(
    state: &Arc<AppState>,
    caller: &Asker<'_>,
    args: &Value,
) -> Result<Value, String> {
    let text = |key: &str| {
        args[key]
            .as_str()
            .map(str::to_string)
            .ok_or(format!("`{key}` is required"))
    };
    let (view, id) = (text("view")?, text("id")?);
    let (status, found) = api(
        state,
        caller,
        Method::GET,
        &format!("/v1/views/{view}"),
        None,
    )
    .await;
    if !status.is_success() {
        return Err(found["error"]
            .as_str()
            .unwrap_or("no such view")
            .to_string());
    }
    let (status, config) = api(state, caller, Method::GET, "/v1/config", None).await;
    if !status.is_success() {
        return Err(config["error"]
            .as_str()
            .unwrap_or("the config can't be read")
            .to_string());
    }
    let mut files = serde_json::Map::new();
    files.insert(format!("dashboards/{id}.yaml"), found["yaml"].clone());
    let args = json!({
        "files": files,
        "base_version": config["version"],
        "summary": args["summary"].as_str().unwrap_or("saved a view"),
    });
    apply_change(state, caller, &args).await
}

/// `show_view`: make the view, then answer each of its visuals' queries
/// through the API, so the agent sees what the user sees.
async fn show_view(
    state: &Arc<AppState>,
    caller: &Asker<'_>,
    args: &Value,
) -> Result<Value, String> {
    let body = json!({
        "name": args["name"].as_str().unwrap_or("View"),
        "visuals": args["visuals"],
        "filters": args.get("filters").cloned().unwrap_or_else(|| json!([])),
    });
    let (status, made) = api(state, caller, Method::POST, "/v1/views", Some(&body)).await;
    if !status.is_success() {
        let problems = made["problems_text"].as_str().unwrap_or_default();
        let error = made["error"]
            .as_str()
            .unwrap_or("the view couldn't be made");
        return Err(if problems.is_empty() {
            error.to_string()
        } else {
            format!("{error}:\n{problems}")
        });
    }
    let id = made["id"].as_str().unwrap_or_default().to_string();
    let snapshot = state.writer.snapshot();
    let mut answers = serde_json::Map::new();
    if let Some(Ok(dashboard)) = state.views.get(state, &snapshot, &id) {
        for visual in &dashboard.visuals {
            let request = crate::live::request(&snapshot.model, visual);
            let (path, body) = match visual.visual_type {
                tidepool_config::dashboard::VisualType::Filter => continue,
                tidepool_config::dashboard::VisualType::Rows => (
                    "/v1/rows",
                    json!({
                        "fields": request.dimensions.iter().map(|d| d.field.clone()).collect::<Vec<_>>(),
                        "limit": visual.limit,
                    }),
                ),
                _ => (
                    "/v1/query",
                    serde_json::to_value(&request).unwrap_or_default(),
                ),
            };
            let (status, answer) = api(state, caller, Method::POST, path, Some(&body)).await;
            let answer = if status.is_success() {
                answer
            } else {
                json!({ "error": answer["error"] })
            };
            answers.insert(visual.id.clone(), answer);
        }
    }
    // for an MCP App to show it live: a token for this view alone
    let body = json!({ "views": [id] });
    let (status, embed) = api(state, caller, Method::POST, "/v1/auth/embed", Some(&body)).await;
    let origin = crate::apps::origin(state, caller.headers);
    Ok(json!({
        "view": id,
        "path": format!("/#/v/{id}"),
        "dashboard": made["dashboard"],
        "answers": answers,
        "_meta": {
            "tidepool": {
                "view": id,
                "origin": origin,
                "token": status.is_success().then(|| embed["token"].clone()),
            }
        },
    }))
}

/// `apply_change`: the agents' policy, then the config API.
async fn apply_change(
    state: &Arc<AppState>,
    caller: &Asker<'_>,
    args: &Value,
) -> Result<Value, String> {
    let Some(base) = args["base_version"].as_u64() else {
        return Err("`base_version` is required: get it from get_config".into());
    };
    let plan_body = json!({ "base_version": base, "files": args["files"] });
    let (status, plan) = api(
        state,
        caller,
        Method::POST,
        "/v1/config/plan",
        Some(&plan_body),
    )
    .await;
    if !status.is_success() {
        return Err(plan["error"]
            .as_str()
            .map_or_else(|| plan.to_string(), str::to_string));
    }
    if plan["valid"] != true {
        return Ok(json!({ "status": "invalid", "plan": plan }));
    }
    let allowed = state
        .writer
        .snapshot()
        .model
        .server
        .agents
        .auto_apply
        .clone();
    let class = plan["class"].as_str().unwrap_or("safe");
    let auto = allowed.iter().any(|c| {
        serde_json::to_value(c)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .as_deref()
            == Some(class)
    });
    let author = args["summary"]
        .as_str()
        .map_or_else(|| "an agent".to_string(), |s| format!("an agent: {s}"));
    let body = json!({
        "base_version": base,
        "files": args["files"],
        "propose": !auto,
        "approve": auto,
        "author": author,
    });
    let (status, applied) = api(state, caller, Method::POST, "/v1/config/apply", Some(&body)).await;
    if status.is_success() {
        Ok(applied)
    } else {
        Err(applied["error"]
            .as_str()
            .map_or_else(|| applied.to_string(), str::to_string))
    }
}

/// Where a config kind's JSON Schema is, as a resource.
const SCHEMA_URI: &str = "tidepool://schemas/";

/// The resources, as `resources/list` describes them: the JSON Schema of
/// every kind of config file, for agents writing one.
fn resources() -> Vec<Value> {
    let app = json!({
        "uri": crate::apps::VIEW_URI,
        "name": "Live view",
        "description": "A view from show_view, drawn live: an MCP App.",
        "mimeType": crate::apps::MIME,
    });
    std::iter::once(app)
        .chain(tidepool_config::schema::KINDS.iter().map(|kind| {
            json!({
                "uri": format!("{SCHEMA_URI}{kind}"),
                "name": format!("{kind} schema"),
                "description": format!("The JSON Schema of a `{kind}` config file or setting."),
                "mimeType": "application/schema+json",
            })
        }))
        .collect()
}

/// One resource's contents, read through the API like a tool's answer.
async fn read_resource(
    state: &Arc<AppState>,
    caller: &Asker<'_>,
    uri: &str,
) -> Result<Vec<Value>, String> {
    if uri == crate::apps::VIEW_URI {
        let origin = crate::apps::origin(state, caller.headers);
        return Ok(vec![json!({
            "uri": uri,
            "mimeType": crate::apps::MIME,
            "text": crate::apps::view_html(state, &origin),
            "_meta": crate::apps::view_meta(&origin),
        })]);
    }
    let Some(kind) = uri.strip_prefix(SCHEMA_URI) else {
        return Err(format!("no resource `{uri}`"));
    };
    if !tidepool_config::schema::KINDS.contains(&kind) {
        return Err(format!("no resource `{uri}`"));
    }
    let (status, schema) = api(
        state,
        caller,
        Method::GET,
        &format!("/v1/schemas/{kind}"),
        None,
    )
    .await;
    if !status.is_success() {
        return Err(schema["error"]
            .as_str()
            .map_or_else(|| schema.to_string(), str::to_string));
    }
    Ok(vec![json!({
        "uri": uri,
        "mimeType": "application/schema+json",
        "text": serde_json::to_string_pretty(&schema).unwrap_or_default(),
    })])
}

/// One JSON-RPC message: the reply, or `None` for a notification.
async fn handle(state: &Arc<AppState>, caller: &Asker<'_>, message: &Value) -> Option<Value> {
    let id = message.get("id")?.clone();
    let method = message["method"].as_str().unwrap_or_default();
    let params = &message["params"];
    Some(match method {
        "initialize" => {
            let asked = params["protocolVersion"].as_str().unwrap_or(VERSIONS[0]);
            let version = VERSIONS
                .iter()
                .find(|v| **v == asked)
                .unwrap_or(&VERSIONS[0]);
            result(
                &id,
                json!({
                    "protocolVersion": version,
                    "capabilities": {
                        "tools": { "listChanged": false },
                        "resources": { "listChanged": false },
                    },
                    "serverInfo": { "name": "tidepool", "title": "Tidepool", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": INSTRUCTIONS,
                }),
            )
        }
        "ping" => result(&id, json!({})),
        "tools/list" => result(&id, json!({ "tools": tools() })),
        "resources/list" => result(&id, json!({ "resources": resources() })),
        "resources/read" => {
            let Some(uri) = params["uri"].as_str() else {
                return Some(error(&id, -32602, "resources/read needs a `uri`"));
            };
            match read_resource(state, caller, uri).await {
                Ok(contents) => result(&id, json!({ "contents": contents })),
                Err(message) => error(&id, -32002, message),
            }
        }
        "tools/call" => {
            let Some(name) = params["name"].as_str() else {
                return Some(error(&id, -32602, "tools/call needs a `name`"));
            };
            if !tools()
                .as_array()
                .is_some_and(|t| t.iter().any(|t| t["name"] == name))
            {
                return Some(error(&id, -32602, format!("no tool `{name}`")));
            }
            state.counters.tool(name);
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            match call(state, caller, name, &args).await {
                Ok(value) => result(&id, answer(value, false)),
                Err(message) => result(&id, answer(json!({ "error": message }), true)),
            }
        }
        _ => error(&id, -32601, format!("no method `{method}`")),
    })
}

/// `POST /mcp`. See `Operation::Mcp` in the endpoint table.
pub async fn mcp(
    State(state): State<Arc<AppState>>,
    Extension(listener): Extension<Listener>,
    uri: axum::http::Uri,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let token = uri.query().and_then(|q| {
        q.split('&')
            .find_map(|pair| pair.strip_prefix("token="))
            .map(str::to_string)
    });
    let asker = Asker {
        headers: &headers,
        token,
        listener,
    };
    let Ok(message) = serde_json::from_slice::<Value>(&body) else {
        return Json(error(&Value::Null, -32700, "not JSON")).into_response();
    };
    // a batch (2025-03-26) or one message
    if let Some(batch) = message.as_array() {
        let mut replies = Vec::new();
        for one in batch {
            replies.extend(handle(&state, &asker, one).await);
        }
        return if replies.is_empty() {
            StatusCode::ACCEPTED.into_response()
        } else {
            Json(Value::Array(replies)).into_response()
        };
    }
    match handle(&state, &asker, &message).await {
        Some(reply) => Json(reply).into_response(),
        None => StatusCode::ACCEPTED.into_response(),
    }
}
