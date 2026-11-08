//! What agents may change (plan §9.1): the agent token reads and submits
//! changes, the server applies them only as `agents.auto_apply` allows and
//! never to a dashboard a person wrote last, and an agent never approves.

mod common;

use std::sync::Arc;

use axum::http::{Method, StatusCode};
use common::{request, send};
use serde_json::{Value, json};
use tidepool_server::{AppState, Tokens};

const AGENT: &str = "agent-secret";
const ADMIN: &str = "admin-secret";

async fn server() -> Arc<AppState> {
    common::project_state_with(
        Tokens::levels(Some("ingest-secret"), Some("read-secret"), Some(ADMIN)).with_agent(AGENT),
        |_| {},
    )
    .await
}

async fn call(
    state: &Arc<AppState>,
    token: &str,
    method: Method,
    path: &str,
    body: &Value,
) -> (StatusCode, Value) {
    let bearer = format!("Bearer {token}");
    let reply = send(
        Arc::clone(state),
        request(
            method,
            path,
            &[
                ("content-type", "application/json"),
                ("authorization", &bearer),
            ],
            if body.is_null() {
                String::new()
            } else {
                body.to_string()
            },
        ),
    )
    .await;
    let value = if reply.text().is_empty() {
        Value::Null
    } else {
        reply.json()
    };
    (reply.status, value)
}

async fn version(state: &Arc<AppState>) -> u64 {
    let (_, config) = call(state, ADMIN, Method::GET, "/v1/config", &Value::Null).await;
    config["version"].as_u64().unwrap_or(0)
}

async fn apply(state: &Arc<AppState>, token: &str, files: Value, extra: Value) -> Value {
    let mut body = json!({ "base_version": version(state).await, "files": files });
    if let (Some(body), Some(extra)) = (body.as_object_mut(), extra.as_object()) {
        body.extend(extra.clone());
    }
    let (status, applied) = call(state, token, Method::POST, "/v1/config/apply", &body).await;
    assert_eq!(status, StatusCode::OK, "{applied}");
    applied
}

const DASHBOARD: &str = "name: Agents' own\nvisuals:\n  - id: total\n    type: kpi\n    metrics: [revenue]\n    at: { x: 0, y: 0, w: 3, h: 2 }\n";

#[tokio::test]
async fn the_agent_token_reads_and_changes_but_never_approves() {
    let state = server().await;
    let may = |method: Method, path: &'static str| {
        let state = Arc::clone(&state);
        async move { call(&state, AGENT, method, path, &Value::Null).await.0 }
    };
    assert_eq!(may(Method::GET, "/v1/model").await, StatusCode::OK);
    assert_eq!(may(Method::GET, "/v1/config").await, StatusCode::OK);
    assert_eq!(
        may(Method::GET, "/v1/config/versions").await,
        StatusCode::OK
    );
    assert_eq!(may(Method::GET, "/v1/changes").await, StatusCode::OK);
    // a person approves and rejects, never an agent
    assert_eq!(
        may(Method::POST, "/v1/changes/1/approve").await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        may(Method::POST, "/v1/changes/1/reject").await,
        StatusCode::UNAUTHORIZED
    );
    // and it's no ingest credential
    let (status, _) = call(
        &state,
        AGENT,
        Method::POST,
        "/v1/tables/customers/rows",
        &json!([{"customer_id": "c1", "name": "Ada", "region": "north"}]),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn an_agents_safe_change_applies_and_the_rest_waits_whatever_it_asks() {
    let state = server().await;
    // a new dashboard: safe, applied, and the history says an agent wrote it
    let added = apply(
        &state,
        AGENT,
        json!({"dashboards/agents.yaml": DASHBOARD}),
        json!({}),
    )
    .await;
    assert_eq!(added["status"], "applied", "{added}");
    let (_, versions) = call(
        &state,
        ADMIN,
        Method::GET,
        "/v1/config/versions",
        &Value::Null,
    )
    .await;
    assert_eq!(versions[0]["by"], "agent", "{versions}");
    // removing a column destroys its data: proposed, even when it asks to
    // be applied and approved
    let orders = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/retail/tables/orders.yaml"
    ))
    .unwrap_or_default()
    .replacen("  quantity:    { type: int32 }\n", "", 1);
    let removed = apply(
        &state,
        AGENT,
        json!({"tables/orders.yaml": orders}),
        json!({"approve": true, "propose": false}),
    )
    .await;
    assert_eq!(removed["status"], "queued", "{removed}");
    assert_eq!(removed["plan"]["class"], "destructive", "{removed}");
    let queued = apply(
        &state,
        AGENT,
        json!({"dashboards/agents.yaml": null}),
        json!({"approve": true}),
    )
    .await;
    // its own dashboard: removing it is safe and it wrote it, so applied
    assert_eq!(queued["status"], "applied", "{queued}");
}

#[tokio::test]
async fn an_agent_doesnt_change_a_dashboard_a_person_wrote_without_approval() {
    let state = server().await;
    let (_, config) = call(&state, ADMIN, Method::GET, "/v1/config", &Value::Null).await;
    let Some(sales) = config["files"]
        .as_array()
        .and_then(|f| f.iter().find(|f| f["path"] == "dashboards/sales.yaml"))
        .and_then(|f| f["text"].as_str())
        .map(str::to_string)
    else {
        panic!("no sales dashboard in {config}")
    };
    let renamed = sales.replacen("name: Sales overview", "name: Sales, by an agent", 1);
    // written at startup, from the directory people edit: proposed, and why
    let proposed = apply(
        &state,
        AGENT,
        json!({"dashboards/sales.yaml": renamed}),
        json!({}),
    )
    .await;
    assert_eq!(proposed["status"], "queued", "{proposed}");
    assert!(
        proposed["plan"]["warnings"]
            .as_array()
            .is_some_and(|w| w.iter().any(|w| w
                .as_str()
                .is_some_and(|w| w.contains("dashboards/sales.yaml")))),
        "{proposed}"
    );
    let (_, pending) = call(&state, ADMIN, Method::GET, "/v1/changes", &Value::Null).await;
    assert_eq!(pending[0]["by"], "agent", "{pending}");
    // a person approves it: applied, and still the agent's change
    let id = proposed["change"].as_u64().unwrap_or(0);
    let (status, approved) = call(
        &state,
        ADMIN,
        Method::POST,
        &format!("/v1/changes/{id}/approve"),
        &Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{approved}");
    let (_, versions) = call(
        &state,
        ADMIN,
        Method::GET,
        "/v1/config/versions",
        &Value::Null,
    )
    .await;
    assert_eq!(versions[0]["by"], "agent", "{versions}");
    // a person's own change applies as always
    let by_person = apply(
        &state,
        ADMIN,
        json!({"dashboards/sales.yaml": sales}),
        json!({}),
    )
    .await;
    assert_eq!(by_person["status"], "applied", "{by_person}");
    let (_, versions) = call(
        &state,
        ADMIN,
        Method::GET,
        "/v1/config/versions",
        &Value::Null,
    )
    .await;
    assert_eq!(versions[0]["by"], "person", "{versions}");
}

#[tokio::test]
async fn mcp_changes_are_an_agents_whatever_token_makes_them() {
    let state = server().await;
    let (_, config) = call(&state, ADMIN, Method::GET, "/v1/config", &Value::Null).await;
    let sales = config["files"]
        .as_array()
        .and_then(|f| f.iter().find(|f| f["path"] == "dashboards/sales.yaml"))
        .and_then(|f| f["text"].as_str())
        .unwrap_or_default()
        .replacen("name: Sales overview", "name: Sales, by an agent", 1);
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "apply_change", "arguments": {
            "files": {"dashboards/sales.yaml": sales}, "base_version": config["version"]}}});
    // even with the admin token: the agents' policy, so a person approves
    let (status, reply) = call(&state, ADMIN, Method::POST, "/mcp", &body).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        reply["result"]["structuredContent"]["status"], "queued",
        "{reply}"
    );
}
