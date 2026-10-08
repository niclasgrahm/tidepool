//! The MCP endpoint (plan §9.1): the protocol's handshake, the tools, the
//! agents' apply policy, and tools checked with the caller's credentials.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::http::{Method, StatusCode};
use common::{request, send};
use serde_json::{Value, json};
use tidepool_server::config::read_dir;
use tidepool_server::{AppState, Project, Tokens};
use tidepool_store::WriterConfig;

async fn server(tokens: Tokens) -> Arc<AppState> {
    let files = match read_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/retail"))
    {
        Ok(files) => files,
        Err(err) => panic!("{err}"),
    };
    let writer = WriterConfig {
        epoch_interval: Duration::from_millis(10),
        ..WriterConfig::default()
    };
    let state =
        match AppState::open(Project { dir: None, files }, writer, None, tokens, false).await {
            Ok(state) => Arc::new(state),
            Err(err) => panic!("{err:#}"),
        };
    state.set_ready();
    state
}

/// One JSON-RPC call; the reply (or the status, for notifications).
async fn rpc(state: &Arc<AppState>, token: Option<&str>, body: &Value) -> (StatusCode, Value) {
    let bearer = token.map(|t| format!("Bearer {t}"));
    let mut headers = vec![("content-type", "application/json")];
    if let Some(bearer) = &bearer {
        headers.push(("authorization", bearer.as_str()));
    }
    let reply = send(
        Arc::clone(state),
        request(Method::POST, "/mcp", &headers, body.to_string()),
    )
    .await;
    let status = reply.status;
    let value = if reply.text().is_empty() {
        Value::Null
    } else {
        reply.json()
    };
    (status, value)
}

/// `tools/call`: (is an error, the structured answer).
async fn tool(
    state: &Arc<AppState>,
    token: Option<&str>,
    name: &str,
    args: Value,
) -> (bool, Value) {
    let (status, reply) = rpc(
        state,
        token,
        &json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call",
                "params": {"name": name, "arguments": args}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    let result = &reply["result"];
    let text = result["content"][0]["text"].as_str().unwrap_or_default();
    assert!(!text.is_empty(), "a tool answers in text too: {reply}");
    (
        result["isError"] == true,
        result
            .get("structuredContent")
            .cloned()
            .unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn the_handshake_lists_the_tools() {
    let state = server(Tokens::default()).await;
    let (_, reply) = rpc(
        &state,
        None,
        &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                           "clientInfo": {"name": "test", "version": "1"}}}),
    )
    .await;
    assert_eq!(reply["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(reply["result"]["serverInfo"]["name"], "tidepool");
    assert!(reply["result"]["capabilities"]["tools"].is_object());

    let (status, _) = rpc(
        &state,
        None,
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let (_, reply) = rpc(
        &state,
        None,
        &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    )
    .await;
    let names: Vec<&str> = reply["result"]["tools"]
        .as_array()
        .map(|t| t.iter().filter_map(|t| t["name"].as_str()).collect())
        .unwrap_or_default();
    assert_eq!(
        names,
        [
            "describe_model",
            "preview_table",
            "query_metrics",
            "get_config",
            "plan_change",
            "apply_change",
            "ingest_rows"
        ]
    );
    let (_, reply) = rpc(
        &state,
        None,
        &json!({"jsonrpc": "2.0", "id": 3, "method": "nope"}),
    )
    .await;
    assert_eq!(reply["error"]["code"], -32601);
}

#[tokio::test]
async fn an_agent_writes_rows_and_queries_them() {
    let state = server(Tokens::default()).await;
    let (failed, model) = tool(&state, None, "describe_model", json!({})).await;
    assert!(!failed);
    assert!(
        model["metrics"]
            .as_array()
            .is_some_and(|m| m.iter().any(|m| m["name"] == "revenue"))
    );
    let (failed, _) = tool(
        &state,
        None,
        "ingest_rows",
        json!({"table": "customers", "rows": [{"customer_id": "c1", "name": "Ada", "region": "north"}]}),
    )
    .await;
    assert!(!failed);
    let (failed, _) = tool(
        &state,
        None,
        "ingest_rows",
        json!({"table": "orders", "rows": [
            {"order_id": "o1", "customer_id": "c1", "product_id": "p1", "status": "paid",
             "ordered_at": "2026-02-01T10:00:00Z", "quantity": 1, "amount": "10.00"}
        ]}),
    )
    .await;
    assert!(!failed);
    let (failed, answer) = tool(
        &state,
        None,
        "query_metrics",
        json!({"metrics": ["revenue"], "dimensions": [{"field": "customers.region"}]}),
    )
    .await;
    assert!(!failed, "{answer}");
    assert_eq!(answer["rows"], json!([["north", "10.00"]]));
    // a mistake comes back as a tool error the agent can read
    let (failed, answer) = tool(
        &state,
        None,
        "query_metrics",
        json!({"metrics": ["revnue"]}),
    )
    .await;
    assert!(failed);
    assert!(
        answer["error"]
            .as_str()
            .is_some_and(|e| e.contains("revenue")),
        "{answer}"
    );
}

#[tokio::test]
async fn safe_changes_apply_and_the_rest_wait_for_a_person() {
    let state = server(Tokens::default()).await;
    let (_, config) = tool(&state, None, "get_config", json!({})).await;
    let version = config["version"].as_u64().unwrap_or(0);
    let returns = "name: returns\nmode: append\ncolumns:\n  id: { type: string }\n";
    let (failed, plan) = tool(
        &state,
        None,
        "plan_change",
        json!({"files": {"tables/returns.yaml": returns}}),
    )
    .await;
    assert!(!failed);
    assert_eq!(plan["class"], "safe");
    let (failed, applied) = tool(
        &state,
        None,
        "apply_change",
        json!({"base_version": version, "files": {"tables/returns.yaml": returns}, "summary": "returns"}),
    )
    .await;
    assert!(!failed, "{applied}");
    assert_eq!(applied["status"], "applied");

    // removing it destroys data: proposed, not applied
    let (failed, proposed) = tool(
        &state,
        None,
        "apply_change",
        json!({"base_version": version + 1, "files": {"tables/returns.yaml": null}}),
    )
    .await;
    assert!(!failed, "{proposed}");
    assert_eq!(proposed["status"], "queued");
    assert!(
        state
            .writer()
            .snapshot()
            .model
            .table_named("returns")
            .is_some()
    );
    let pending = send(
        Arc::clone(&state),
        request(Method::GET, "/v1/changes", &[], ""),
    )
    .await;
    assert!(
        pending.json()[0]["author"]
            .as_str()
            .is_some_and(|a| a.starts_with("an agent"))
    );
}

#[tokio::test]
async fn tools_need_the_level_their_endpoint_needs() {
    let tokens = Tokens {
        read: Some("read-secret".into()),
        ingest: Some("ingest-secret".into()),
        admin: Some("admin-secret".into()),
    };
    let state = server(tokens).await;
    let (status, _) = rpc(
        &state,
        None,
        &json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "the endpoint itself wants the read token"
    );
    let (failed, answer) = tool(&state, Some("read-secret"), "get_config", json!({})).await;
    assert!(failed, "the config is the admin's: {answer}");
    let (failed, _) = tool(&state, Some("admin-secret"), "get_config", json!({})).await;
    assert!(!failed);
    let rows = json!({"table": "customers", "rows": [{"customer_id": "c1", "name": "Ada", "region": "north"}]});
    let (failed, _) = tool(&state, Some("read-secret"), "ingest_rows", rows.clone()).await;
    assert!(failed);
    // the MCP endpoint takes the read level; ingesting needs the ingest
    // token, which the admin token covers
    let (failed, _) = tool(&state, Some("admin-secret"), "ingest_rows", rows).await;
    assert!(!failed);
}
