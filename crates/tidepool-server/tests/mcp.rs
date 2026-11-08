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
            "read_rows",
            "show_view",
            "save_view",
            "get_selection",
            "set_selection",
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

/// A field of `describe_model`'s answer, by dataset and name.
fn field<'a>(model: &'a Value, dataset: &str, name: &str) -> &'a Value {
    let found = model["datasets"]
        .as_array()
        .and_then(|d| d.iter().find(|d| d["name"] == dataset))
        .and_then(|d| d["fields"].as_array())
        .and_then(|f| f.iter().find(|f| f["name"] == name));
    match found {
        Some(field) => field,
        None => panic!("no field {dataset}.{name} in {model}"),
    }
}

#[tokio::test]
async fn describe_model_says_what_an_agent_needs_to_ask_well() {
    let state = server(Tokens::default()).await;
    let (failed, model) = tool(&state, None, "describe_model", json!({})).await;
    assert!(!failed, "{model}");
    // the Ossie ai_context, at every level it's written at
    assert!(
        model["ai_context"]["instructions"]
            .as_str()
            .is_some_and(|i| i.contains("ordered_at")),
        "{model}"
    );
    assert_eq!(
        model["description"],
        "An outdoor shop: its orders, customers and products."
    );
    let region = field(&model, "customers", "region");
    assert_eq!(region["label"], "Region");
    assert_eq!(
        region["ai_context"]["synonyms"],
        json!(["area", "territory", "market"])
    );
    let revenue = model["metrics"]
        .as_array()
        .and_then(|m| m.iter().find(|m| m["name"] == "revenue"))
        .cloned()
        .unwrap_or_default();
    assert!(
        revenue["ai_context"]["synonyms"]
            .as_array()
            .is_some_and(|s| s.contains(&json!("sales"))),
        "{revenue}"
    );
    // the grains a time field can be grouped by; none for other fields
    let ordered_at = field(&model, "orders", "ordered_at");
    assert_eq!(
        ordered_at["grains"],
        json!(["minute", "hour", "day", "week", "month", "quarter", "year"])
    );
    assert_eq!(ordered_at["label"], "Order time");
    assert!(
        field(&model, "orders", "order_year")
            .get("grains")
            .is_none()
    );
    // dashboards with what they show, and the visual types to choose from
    let sales = model["dashboards"]
        .as_array()
        .and_then(|d| d.iter().find(|d| d["id"] == "sales"))
        .cloned()
        .unwrap_or_default();
    assert!(
        sales["visuals"]
            .as_array()
            .is_some_and(|v| v.iter().any(|v| v["type"] == "bar"
                && v["metrics"] == json!(["revenue"])
                && v["dimensions"][0]["field"] == "customers.region")),
        "{sales}"
    );
    let types: Vec<&str> = model["visual_types"]
        .as_array()
        .map(|t| t.iter().filter_map(|t| t["type"].as_str()).collect())
        .unwrap_or_default();
    assert_eq!(
        types,
        ["kpi", "bar", "line", "table", "pie", "filter", "rows"]
    );
    assert!(model["visual_types"].as_array().is_some_and(|t| {
        t.iter()
            .all(|t| t["shape"].as_str().is_some_and(|s| !s.is_empty()))
    }));
    // and how answers are computed
    assert!(
        model["semantics"].as_array().is_some_and(|s| s
            .iter()
            .any(|s| s.as_str().is_some_and(|s| s.contains("NULL")))),
        "{model}"
    );
}

#[tokio::test]
async fn the_config_schemas_are_resources() {
    let state = server(Tokens::default()).await;
    let (_, reply) = rpc(
        &state,
        None,
        &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                           "clientInfo": {"name": "test", "version": "1"}}}),
    )
    .await;
    assert!(
        reply["result"]["capabilities"]["resources"].is_object(),
        "{reply}"
    );
    let (_, reply) = rpc(
        &state,
        None,
        &json!({"jsonrpc": "2.0", "id": 2, "method": "resources/list"}),
    )
    .await;
    let uris: Vec<&str> = reply["result"]["resources"]
        .as_array()
        .map(|r| r.iter().filter_map(|r| r["uri"].as_str()).collect())
        .unwrap_or_default();
    assert!(uris.contains(&"tidepool://schemas/dashboard"), "{reply}");
    assert!(
        uris.contains(&"tidepool://schemas/metric-extension"),
        "{reply}"
    );
    let (_, reply) = rpc(
        &state,
        None,
        &json!({"jsonrpc": "2.0", "id": 3, "method": "resources/read",
                "params": {"uri": "tidepool://schemas/dashboard"}}),
    )
    .await;
    let contents = &reply["result"]["contents"][0];
    assert_eq!(contents["mimeType"], "application/schema+json");
    let schema: Value = match contents["text"].as_str().map(serde_json::from_str) {
        Some(Ok(schema)) => schema,
        _ => panic!("the schema should be JSON text: {reply}"),
    };
    assert!(schema["properties"]["visuals"].is_object(), "{schema}");
    // a resource that isn't there is an error, not an empty answer
    let (_, reply) = rpc(
        &state,
        None,
        &json!({"jsonrpc": "2.0", "id": 4, "method": "resources/read",
                "params": {"uri": "tidepool://schemas/nope"}}),
    )
    .await;
    assert!(
        reply["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("nope")),
        "{reply}"
    );
}

#[tokio::test]
async fn tools_with_structured_answers_declare_their_shape() {
    let state = server(Tokens::default()).await;
    let (_, reply) = rpc(
        &state,
        None,
        &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    )
    .await;
    let tools = reply["result"]["tools"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let schema = |name: &str| {
        tools
            .iter()
            .find(|t| t["name"] == name)
            .map(|t| t["outputSchema"].clone())
            .unwrap_or_default()
    };
    for name in ["query_metrics", "read_rows"] {
        let schema = schema(name);
        assert_eq!(schema["type"], "object", "{name}: {schema}");
        assert!(
            schema["properties"]["columns"].is_object(),
            "{name}: {schema}"
        );
        assert!(schema.get("$schema").is_none(), "{name}: {schema}");
    }
    assert!(schema("plan_change")["properties"]["valid"].is_object());
    // what query_metrics answers has only keys its schema has
    let (_, answer) = tool(
        &state,
        None,
        "query_metrics",
        json!({"metrics": ["revenue"]}),
    )
    .await;
    let declared = schema("query_metrics");
    for key in answer
        .as_object()
        .map(|o| o.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default()
    {
        assert!(
            declared["properties"].get(&key).is_some(),
            "`{key}` isn't declared: {declared}"
        );
    }
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
    // and how fresh that is: when its epoch was published, in UTC
    assert!(
        answer["as_of"]
            .as_str()
            .is_some_and(|t| t.starts_with("20") && t.ends_with('Z')),
        "{answer}"
    );
    // the rows themselves, found by their text
    let (failed, _) = tool(
        &state,
        None,
        "ingest_rows",
        json!({"table": "orders", "rows": [
            {"order_id": "o2", "customer_id": "c1", "product_id": "p1", "status": "paid",
             "ordered_at": "2026-02-02T10:00:00Z", "quantity": 1, "amount": "2.00",
             "note": "Gift wrap, please"}
        ]}),
    )
    .await;
    assert!(!failed);
    let (failed, answer) = tool(
        &state,
        None,
        "read_rows",
        json!({"fields": ["orders.order_id", "customers.name"],
               "filters": [{"field": "orders.note", "contains": "GIFT"}]}),
    )
    .await;
    assert!(!failed, "{answer}");
    assert_eq!(answer["rows"], json!([["o2", "Ada"]]));
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
    let tokens = Tokens::levels(
        Some("ingest-secret"),
        Some("read-secret"),
        Some("admin-secret"),
    );
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

#[tokio::test]
async fn an_agent_never_applies_a_change_to_who_can_reach_the_server() {
    let state = server(Tokens::default()).await;
    let (_, config) = tool(&state, None, "get_config", json!({})).await;
    let version = config["version"].as_u64().unwrap_or(0);
    let yaml = config["files"]["tidepool.yaml"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    // a person lets agents apply every class there is
    let everything = format!("{yaml}agents:\n  auto_apply: [safe, rebuild, destructive]\n");
    let applied = send(
        Arc::clone(&state),
        request(
            Method::POST,
            "/v1/config/apply",
            &[("content-type", "application/json")],
            json!({"base_version": version, "files": {"tidepool.yaml": &everything}, "approve": true})
                .to_string(),
        ),
    )
    .await;
    assert_eq!(applied.status, StatusCode::OK, "{}", applied.text());
    // and still an agent's new credential waits for a person
    let credential = format!(
        "{everything}auth:\n  ingest:\n    - name: agent\n      token_env: AGENT_TOKEN\n      public: true\n      tables: [orders]\n"
    );
    let (failed, proposed) = tool(
        &state,
        None,
        "apply_change",
        json!({"base_version": version + 1, "files": {"tidepool.yaml": credential}}),
    )
    .await;
    assert!(!failed, "{proposed}");
    assert_eq!(proposed["status"], "queued", "{proposed}");
    assert_eq!(proposed["plan"]["class"], "security", "{proposed}");
}

#[tokio::test]
async fn an_agent_shows_a_live_view_and_saves_it_as_a_dashboard() {
    let state = server(Tokens::default()).await;
    let (_, _) = tool(
        &state,
        None,
        "ingest_rows",
        json!({"table": "customers", "rows": [{"customer_id": "c1", "name": "Ada", "region": "north"}]}),
    )
    .await;
    let (_, _) = tool(
        &state,
        None,
        "ingest_rows",
        json!({"table": "orders", "rows": [
            {"order_id": "o1", "customer_id": "c1", "product_id": "p1", "status": "paid",
             "ordered_at": "2026-02-01T10:00:00Z", "quantity": 1, "amount": "10.00"}
        ]}),
    )
    .await;
    let (failed, shown) = tool(
        &state,
        None,
        "show_view",
        json!({"name": "Revenue by region", "visuals": [
            {"id": "regions", "type": "bar", "dimensions": ["customers.region"], "metrics": ["revenue"]},
            {"id": "total", "type": "kpi", "metrics": ["revenue"]},
        ]}),
    )
    .await;
    assert!(!failed, "{shown}");
    let view = shown["view"].as_str().unwrap_or_default().to_string();
    assert!(view.starts_with('~'), "{shown}");
    assert_eq!(shown["path"], format!("/#/v/{view}"));
    // the agent sees the numbers the user sees
    assert_eq!(
        shown["answers"]["regions"]["rows"],
        json!([["north", "10.00"]])
    );
    assert_eq!(shown["answers"]["total"]["rows"], json!([["10.00"]]));
    // a mistake reads like a compiler's
    let (failed, wrong) = tool(
        &state,
        None,
        "show_view",
        json!({"visuals": [{"id": "a", "type": "kpi", "metrics": ["revnue"]}]}),
    )
    .await;
    assert!(failed);
    assert!(
        wrong["error"]
            .as_str()
            .is_some_and(|e| e.contains("revenue")),
        "{wrong}"
    );
    // saved: a dashboard file, applied as a safe change
    let (failed, saved) = tool(
        &state,
        None,
        "save_view",
        json!({"view": view, "id": "regions", "summary": "revenue by region"}),
    )
    .await;
    assert!(!failed, "{saved}");
    assert_eq!(saved["status"], "applied", "{saved}");
    let (_, model) = tool(&state, None, "describe_model", json!({})).await;
    assert!(
        model["dashboards"].as_array().is_some_and(|d| d
            .iter()
            .any(|d| d["id"] == "regions" && d["name"] == "Revenue by region")),
        "{model}"
    );
}

#[tokio::test]
async fn a_view_comes_with_an_app_to_show_it_live() {
    let state = common::project_state_with(Tokens::default(), |files| {
        if let Some(server) = files.get_mut("tidepool.yaml") {
            server.push_str("public_url: https://tidepool.example.com/\n");
        }
    })
    .await;
    // the tool says which app shows its answer
    let (_, listed) = rpc(
        &state,
        None,
        &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
    )
    .await;
    let show = listed["result"]["tools"]
        .as_array()
        .and_then(|t| t.iter().find(|t| t["name"] == "show_view"))
        .cloned()
        .unwrap_or_default();
    assert_eq!(show["_meta"]["ui"]["resourceUri"], "ui://tidepool/view");
    // the app: HTML for MCP Apps, loading from and connecting to the server
    let (_, resources) = rpc(
        &state,
        None,
        &json!({"jsonrpc": "2.0", "id": 2, "method": "resources/list"}),
    )
    .await;
    assert!(
        resources["result"]["resources"]
            .as_array()
            .is_some_and(|r| r.iter().any(|r| r["uri"] == "ui://tidepool/view"
                && r["mimeType"] == "text/html;profile=mcp-app")),
        "{resources}"
    );
    let (_, read) = rpc(
        &state,
        None,
        &json!({"jsonrpc": "2.0", "id": 3, "method": "resources/read",
                "params": {"uri": "ui://tidepool/view"}}),
    )
    .await;
    let app = &read["result"]["contents"][0];
    assert_eq!(app["mimeType"], "text/html;profile=mcp-app");
    let html = app["text"].as_str().unwrap_or_default();
    assert!(
        html.contains("content=\"https://tidepool.example.com\"") && html.contains("ui/initialize"),
        "{html}"
    );
    assert_eq!(
        app["_meta"]["ui"]["csp"]["connectDomains"],
        json!(["https://tidepool.example.com", "wss://tidepool.example.com"])
    );
    assert_eq!(
        app["_meta"]["ui"]["csp"]["resourceDomains"],
        json!(["https://tidepool.example.com"])
    );
    // the answer carries the view and a token for it, for the app alone
    let (status, reply) = rpc(
        &state,
        None,
        &json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call",
        "params": {"name": "show_view", "arguments": {"visuals": [
            {"id": "total", "type": "kpi", "metrics": ["revenue"]}
        ]}}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let result = &reply["result"];
    let meta = &result["_meta"]["tidepool"];
    assert_eq!(meta["view"], result["structuredContent"]["view"]);
    assert_eq!(meta["origin"], "https://tidepool.example.com");
    assert!(
        meta["token"]
            .as_str()
            .is_some_and(|t| t.starts_with("embed_")),
        "{reply}"
    );
    assert!(result["structuredContent"].get("_meta").is_none());
    assert!(
        !result["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("embed_"),
        "the model doesn't see the token"
    );
}

#[tokio::test]
async fn a_token_in_the_mcp_address_is_the_tools_token_too() {
    let state =
        common::project_state_with(Tokens::levels(None, Some("read-secret"), None), |_| {}).await;
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                      "params": {"name": "describe_model", "arguments": {}}});
    let reply = send(
        Arc::clone(&state),
        request(
            Method::POST,
            "/mcp?token=read-secret",
            &[("content-type", "application/json")],
            body.to_string(),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_ne!(reply.json()["result"]["isError"], true, "{}", reply.text());
}
