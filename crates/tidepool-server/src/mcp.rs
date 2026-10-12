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
//! approve.

use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::AppState;

/// Protocol versions this endpoint speaks, newest first.
const VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "Tidepool is a live analytics server: tables of data, a semantic \
model (datasets, relationships, metrics) and dashboards. Start with describe_model to see \
what exists, query_metrics to compute metrics grouped by fields under filters, and \
preview_table to look at rows. To change the model, get_config for the files and their \
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
            "description": "Tables with their columns and row counts, datasets and fields, \
                            relationships, metrics with their SQL, and dashboards. Read this \
                            first: names in queries are the ones it lists.",
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
                            grain: day, week, month, quarter, year), under filters (`in` a \
                            list, or `from`/`to` a half-open range), sorted and limited. The \
                            body of POST /v1/query.",
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
            "annotations": { "readOnlyHint": true },
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

/// A JSON-RPC error.
fn error(id: &Value, code: i64, message: impl Into<String>) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message.into() } })
}

#[allow(clippy::needless_pass_by_value)] // built in place at every call
fn result(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// A tool's answer: the JSON as text, and as structured content.
fn answer(value: Value, is_error: bool) -> Value {
    let text = serde_json::to_string_pretty(&value).unwrap_or_default();
    let mut out = json!({
        "content": [{ "type": "text", "text": text }],
        "isError": is_error,
    });
    if value.is_object() {
        out["structuredContent"] = value;
    }
    out
}

/// One call of the API, in process, with the caller's credentials.
async fn api(
    state: &Arc<AppState>,
    caller: &HeaderMap,
    method: Method,
    path: &str,
    body: Option<&Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(path);
    for name in [header::AUTHORIZATION, header::COOKIE] {
        if let Some(value) = caller.get(&name) {
            request = request.header(name, value);
        }
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
    let router = crate::api_router(Arc::clone(state));
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
    caller: &HeaderMap,
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
            Ok(json!({
                "name": snapshot.model.name,
                "config_version": snapshot.version,
                "tables": tables,
                "datasets": model["datasets"],
                "relationships": model["relationships"],
                "metrics": model["metrics"],
                "dashboards": snapshot.model.dashboards.iter().map(|d| json!({"id": d.slug, "name": d.name})).collect::<Vec<_>>(),
            }))
        }
        "preview_table" => {
            let table = text("table")?;
            let limit = args["limit"].as_u64().unwrap_or(20).clamp(1, 1000);
            let path = format!("/v1/tables/{table}/preview?limit={limit}");
            ok(api(state, caller, Method::GET, &path, None).await)
        }
        "query_metrics" => ok(api(state, caller, Method::POST, "/v1/query", Some(args)).await),
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
        "ingest_rows" => {
            let table = text("table")?;
            let path = format!("/v1/tables/{table}/rows?wait=visible");
            ok(api(state, caller, Method::POST, &path, Some(&args["rows"])).await)
        }
        other => Err(format!("no tool `{other}`")),
    }
}

/// `apply_change`: the agents' policy, then the config API.
async fn apply_change(
    state: &Arc<AppState>,
    caller: &HeaderMap,
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

/// One JSON-RPC message: the reply, or `None` for a notification.
async fn handle(state: &Arc<AppState>, caller: &HeaderMap, message: &Value) -> Option<Value> {
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
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": { "name": "tidepool", "title": "Tidepool", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": INSTRUCTIONS,
                }),
            )
        }
        "ping" => result(&id, json!({})),
        "tools/list" => result(&id, json!({ "tools": tools() })),
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
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let Ok(message) = serde_json::from_slice::<Value>(&body) else {
        return Json(error(&Value::Null, -32700, "not JSON")).into_response();
    };
    // a batch (2025-03-26) or one message
    if let Some(batch) = message.as_array() {
        let mut replies = Vec::new();
        for one in batch {
            replies.extend(handle(&state, &headers, one).await);
        }
        return if replies.is_empty() {
            StatusCode::ACCEPTED.into_response()
        } else {
            Json(Value::Array(replies)).into_response()
        };
    }
    match handle(&state, &headers, &message).await {
        Some(reply) => Json(reply).into_response(),
        None => StatusCode::ACCEPTED.into_response(),
    }
}
