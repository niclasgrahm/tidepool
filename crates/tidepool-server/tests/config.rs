//! The config pipeline (plan §8, M7) over HTTP: plan and apply, history,
//! the approval queue, write-back, the directory watcher, startup, and the
//! exit criterion: a table added, fed, joined and shown on a dashboard
//! without a restart.

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::http::{Method, StatusCode};
use common::{request, send};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tidepool_config::ConfigSet;
use tidepool_model::compile;
use tidepool_server::config::{Files, read_dir};
use tidepool_server::{AppState, Project, Tokens};
use tidepool_store::WriterConfig;
use tokio_tungstenite::tungstenite::Message;

fn tempdir() -> tempfile::TempDir {
    match tempfile::tempdir() {
        Ok(dir) => dir,
        Err(err) => panic!("{err}"),
    }
}

/// A copy of the retail example in `dir`.
fn copy_example(dir: &Path) {
    let from = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/retail");
    let files = match read_dir(&from) {
        Ok(files) => files,
        Err(err) => panic!("reading the example: {err}"),
    };
    for (path, text) in files {
        let to = dir.join(path);
        if let Some(parent) = to.parent()
            && let Err(err) = std::fs::create_dir_all(parent)
        {
            panic!("{err}");
        }
        if let Err(err) = std::fs::write(&to, text) {
            panic!("{err}");
        }
    }
}

async fn open(
    config: &Path,
    data: Option<&Path>,
    allow_destructive: bool,
) -> anyhow::Result<Arc<AppState>> {
    let files = read_dir(config)?;
    let writer = WriterConfig {
        epoch_interval: Duration::from_millis(10),
        data: data.map(Path::to_path_buf),
        ..WriterConfig::default()
    };
    let state = AppState::open(
        Project {
            dir: Some(config.to_path_buf()),
            files,
        },
        writer,
        None,
        Tokens::default(),
        allow_destructive,
    )
    .await?;
    state.set_ready();
    Ok(Arc::new(state))
}

async fn opened(config: &Path, data: Option<&Path>) -> Arc<AppState> {
    match open(config, data, false).await {
        Ok(state) => state,
        Err(err) => panic!("starting: {err:#}"),
    }
}

async fn call(
    state: &Arc<AppState>,
    method: Method,
    path: &str,
    body: &Value,
) -> (StatusCode, Value) {
    let reply = send(
        Arc::clone(state),
        request(
            method,
            path,
            &[("content-type", "application/json")],
            body.to_string(),
        ),
    )
    .await;
    let status = reply.status;
    (status, reply.json())
}

async fn get(state: &Arc<AppState>, path: &str) -> Value {
    let reply = send(Arc::clone(state), request(Method::GET, path, &[], "")).await;
    assert_eq!(reply.status, StatusCode::OK, "GET {path}: {}", reply.text());
    reply.json()
}

async fn version(state: &Arc<AppState>) -> u64 {
    get(state, "/v1/config").await["version"]
        .as_u64()
        .unwrap_or(0)
}

/// Apply `files` (path → text or null) on top of the current version; the
/// reply. Waits until a rebuild is live.
async fn apply(state: &Arc<AppState>, files: Value, extra: Value) -> (StatusCode, Value) {
    let mut body = json!({ "base_version": version(state).await, "files": files });
    if let (Some(body), Some(extra)) = (body.as_object_mut(), extra.as_object()) {
        body.extend(extra.clone());
    }
    let (status, reply) = call(state, Method::POST, "/v1/config/apply", &body).await;
    if status == StatusCode::OK {
        live(state, reply["version"].as_u64().unwrap_or(0)).await;
    }
    (status, reply)
}

/// Wait until the server runs `version`.
async fn live(state: &Arc<AppState>, version: u64) {
    for _ in 0..500 {
        if state.writer().snapshot().version >= version {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("version {version} never went live");
}

async fn post_rows(state: &Arc<AppState>, table: &str, rows: &Value) -> StatusCode {
    let path = format!("/v1/tables/{table}/rows?wait=visible");
    let (status, _) = call(state, Method::POST, &path, rows).await;
    status
}

async fn seed(state: &Arc<AppState>) {
    let customers = json!([
        {"customer_id": "c1", "name": "Ada", "region": "north"},
        {"customer_id": "c2", "name": "Bo", "region": "south"},
        {"customer_id": "c3", "name": "Cy", "region": "south"},
    ]);
    assert_eq!(
        post_rows(state, "customers", &customers).await,
        StatusCode::OK
    );
    let products =
        json!([{"product_id": "p1", "name": "Kayak", "category": "boats", "list_price": "900.00"}]);
    assert_eq!(
        post_rows(state, "products", &products).await,
        StatusCode::OK
    );
    let orders = json!([
        {"order_id": "o1", "customer_id": "c1", "product_id": "p1", "status": "paid",
         "ordered_at": "2026-02-01T10:00:00Z", "quantity": 1, "amount": "10.00"},
    ]);
    assert_eq!(post_rows(state, "orders", &orders).await, StatusCode::OK);
}

const RETURNS_TABLE: &str = "name: returns
mode: append
columns:
  return_id:   { type: string, encoding: plain }
  customer_id: { type: string }
  refunded:    { type: \"decimal(14,2)\" }
";

/// The model with returns: a dataset, a relationship to customers and a
/// metric over it.
fn with_returns(model: &str) -> String {
    model
        .replace(
            "relationships:\n",
            "  - name: returns\n    source: returns\n\nrelationships:\n  - name: return_customer\n    from: returns\n    to: customers\n    from_columns: [customer_id]\n    to_columns: [customer_id]\n",
        )
        .replace(
            "metrics:\n",
            "metrics:\n  - name: refunds\n    expression: { dialects: [{ dialect: ANSI_SQL, expression: \"SUM(returns.refunded)\" }] }\n",
        )
}

const REFUNDS_VISUAL: &str = "  - id: refunds_by_region
    type: bar
    dimensions: [customers.region]
    metrics: [refunds]
    at: { x: 0, y: 8, w: 6, h: 3 }
";

#[tokio::test]
#[allow(clippy::too_many_lines)] // the M7 exit criterion, start to end
async fn a_table_is_added_fed_joined_and_shown_without_a_restart() {
    let (config, data) = (tempdir(), tempdir());
    copy_example(config.path());
    let state = opened(config.path(), Some(data.path())).await;
    seed(&state).await;
    assert_eq!(version(&state).await, 1);

    // a table: safe, live at once
    let (status, reply) = apply(
        &state,
        json!({"tables/returns.yaml": RETURNS_TABLE}),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    assert_eq!(reply["status"], "applied");
    assert_eq!(reply["plan"]["class"], "safe");
    let returns = json!([
        {"return_id": "r1", "customer_id": "c2", "refunded": "4.00"},
        {"return_id": "r2", "customer_id": "c3", "refunded": "1.50"},
        {"return_id": "r3", "customer_id": "c1", "refunded": "2.00"},
    ]);
    assert_eq!(post_rows(&state, "returns", &returns).await, StatusCode::OK);

    // a relationship over populated tables, a metric, a visual: a rebuild
    let config_now = get(&state, "/v1/config").await;
    let text = |path: &str| -> String {
        config_now["files"]
            .as_array()
            .and_then(|f| f.iter().find(|f| f["path"] == path))
            .and_then(|f| f["text"].as_str())
            .unwrap_or_default()
            .to_string()
    };
    let dashboard = format!("{}{REFUNDS_VISUAL}", text("dashboards/sales.yaml"));
    let (status, reply) = apply(
        &state,
        json!({
            "model.ossie.yaml": with_returns(&text("model.ossie.yaml")),
            "dashboards/sales.yaml": dashboard,
        }),
        json!({"author": "a test"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    assert_eq!(
        reply["plan"]["class"], "rebuild",
        "{}",
        reply["plan"]["text"]
    );
    assert_eq!(reply["version"], 3);

    // on the dashboard, live
    let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
        Ok(listener) => listener,
        Err(err) => panic!("{err}"),
    };
    let address = listener.local_addr().unwrap_or_else(|e| panic!("{e}"));
    let app = tidepool_server::router(Arc::clone(&state));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let (mut socket, _) =
        match tokio_tungstenite::connect_async(format!("ws://{address}/v1/ws")).await {
            Ok(connected) => connected,
            Err(err) => panic!("{err}"),
        };
    let say = |m: Value| Message::text(m.to_string());
    if let Err(err) = socket.send(say(json!({"type": "hello"}))).await {
        panic!("{err}");
    }
    if let Err(err) = socket
        .send(say(json!({"type": "open_dashboard", "id": "sales"})))
        .await
    {
        panic!("{err}");
    }
    let frame = loop {
        let message = match tokio::time::timeout(Duration::from_secs(5), socket.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => text,
            Ok(Some(Ok(_))) => continue,
            other => panic!("waiting for a frame: {other:?}"),
        };
        let value: Value = serde_json::from_str(message.as_str()).unwrap_or_default();
        if value["type"] == "frame" {
            break value;
        }
    };
    let mut refunds: Vec<Value> = frame["visuals"]
        .as_array()
        .and_then(|v| v.iter().find(|v| v["id"] == "refunds_by_region"))
        .and_then(|v| v["result"]["rows"].as_array().cloned())
        .unwrap_or_default();
    refunds.sort_by_key(ToString::to_string);
    assert_eq!(
        refunds,
        vec![json!(["north", "2.00"]), json!(["south", "5.50"])],
        "{frame}"
    );

    // the directory holds what runs, and compiles to the same model
    let on_disk = match read_dir(config.path()) {
        Ok(files) => files,
        Err(err) => panic!("{err}"),
    };
    let served: Files = get(&state, "/v1/config").await["files"]
        .as_array()
        .map(|files| {
            files
                .iter()
                .map(|f| {
                    (
                        f["path"].as_str().unwrap_or_default().to_string(),
                        f["text"].as_str().unwrap_or_default().to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(on_disk, served);
    let snapshot = state.writer().snapshot();
    let (set, _) = ConfigSet::from_files(on_disk);
    let (compiled, d) = compile(&set, &snapshot.catalog);
    let Some(compiled) = compiled else {
        panic!("the written config should compile: {d:?}");
    };
    assert_eq!(compiled.model, *snapshot.model);

    let versions = get(&state, "/v1/config/versions").await;
    assert_eq!(versions[0]["version"], 3);
    assert_eq!(versions[0]["author"], "a test");
    assert_eq!(versions[2]["source"], "startup");
}

#[tokio::test]
async fn a_restart_comes_back_at_the_same_version_with_the_data() {
    let (config, data) = (tempdir(), tempdir());
    copy_example(config.path());
    {
        let state = opened(config.path(), Some(data.path())).await;
        seed(&state).await;
        let (status, reply) = apply(
            &state,
            json!({"tables/returns.yaml": RETURNS_TABLE}),
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{reply}");
        let returns = json!([{"return_id": "r1", "customer_id": "c2", "refunded": "4.00"}]);
        assert_eq!(post_rows(&state, "returns", &returns).await, StatusCode::OK);
    }
    // the writer stops with the last handle; give it a moment
    tokio::time::sleep(Duration::from_millis(100)).await;
    let state = opened(config.path(), Some(data.path())).await;
    assert_eq!(version(&state).await, 2);
    let tables = get(&state, "/v1/tables/returns").await;
    assert_eq!(tables["rows"], 1);
    assert_eq!(
        get(&state, "/v1/config/versions")
            .await
            .as_array()
            .map(Vec::len),
        Some(2)
    );
}

#[tokio::test]
async fn a_config_changed_while_down_is_applied_at_startup_and_destruction_needs_a_flag() {
    let (config, data) = (tempdir(), tempdir());
    copy_example(config.path());
    {
        let state = opened(config.path(), Some(data.path())).await;
        seed(&state).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    // a column removed while the server was down
    let path = config.path().join("tables/orders.yaml");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let edited = text.replace(
        "  note:        { type: string, nullable: true, encoding: plain }\n",
        "",
    );
    assert_ne!(text, edited);
    if let Err(err) = std::fs::write(&path, edited) {
        panic!("{err}");
    }
    match open(config.path(), Some(data.path()), false).await {
        Ok(_) => panic!("removing a column with data shouldn't start without the flag"),
        Err(err) => assert!(
            format!("{err:#}").contains("--allow-destructive"),
            "{err:#}"
        ),
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let state = match open(config.path(), Some(data.path()), true).await {
        Ok(state) => state,
        Err(err) => panic!("{err:#}"),
    };
    assert_eq!(version(&state).await, 2);
    let versions = get(&state, "/v1/config/versions").await;
    assert_eq!(versions[0]["source"], "startup");
    assert_eq!(versions[0]["class"], "destructive");
    let orders = get(&state, "/v1/tables/orders").await;
    assert_eq!(orders["rows"], 1, "the rows stay, without the column");
}

#[tokio::test]
async fn stale_invalid_and_destructive_changes_are_refused() {
    let config = tempdir();
    copy_example(config.path());
    let state = opened(config.path(), None).await;

    let (status, reply) = call(
        &state,
        Method::POST,
        "/v1/config/apply",
        &json!({"base_version": 7, "files": {"tables/returns.yaml": RETURNS_TABLE}}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(reply["current_version"], 1);

    let (status, reply) = apply(
        &state,
        json!({"tables/returns.yaml": "name: returns\nmode: append\ncolumns:\n  id: { type: strng }\n"}),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{reply}");
    let problem = &reply["plan"]["problems"][0];
    assert_eq!(problem["file"], "tables/returns.yaml");
    assert_eq!(problem["line"], 4, "{problem}");

    // removing a table the model still uses doesn't compile
    let (status, reply) = apply(&state, json!({"tables/products.yaml": null}), json!({})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{reply}");
    let (status, reply) = call(
        &state,
        Method::POST,
        "/v1/config/plan",
        &json!({"base_version": 1, "files": {"tables/../x.yaml": "x"}}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{reply}");

    let orders =
        std::fs::read_to_string(config.path().join("tables/orders.yaml")).unwrap_or_default();
    let narrowed = orders.replace(
        r#"amount:      { type: "decimal(14,2)" }"#,
        r#"amount:      { type: "decimal(6,0)" }"#,
    );
    let (status, reply) = apply(&state, json!({"tables/orders.yaml": &narrowed}), json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{reply}");
    assert_eq!(reply["plan"]["class"], "destructive");
    let (status, reply) = apply(
        &state,
        json!({"tables/orders.yaml": &narrowed}),
        json!({"approve": true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    assert_eq!(version(&state).await, 2);
}

#[tokio::test]
async fn changes_to_who_can_reach_the_server_need_approval() {
    let config = tempdir();
    copy_example(config.path());
    let state = opened(config.path(), None).await;
    let yaml = std::fs::read_to_string(config.path().join("tidepool.yaml")).unwrap_or_default();
    let granted = format!(
        "{yaml}auth:\n  ingest:\n    - name: sensors\n      token_env: SENSOR_TOKEN\n      public: true\n      tables: [orders]\n"
    );
    let (status, reply) = apply(&state, json!({"tidepool.yaml": &granted}), json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{reply}");
    assert_eq!(reply["plan"]["class"], "security");
    assert!(
        reply["error"]
            .as_str()
            .is_some_and(|e| e.contains("who can reach")),
        "{reply}"
    );
    let (status, reply) = apply(
        &state,
        json!({"tidepool.yaml": &granted}),
        json!({"approve": true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    assert_eq!(version(&state).await, 2);

    // the same kind of edit in the directory waits in the queue
    let wider = granted.replace("tables: [orders]", "tables: [orders, customers]");
    if let Err(err) = std::fs::write(config.path().join("tidepool.yaml"), &wider) {
        panic!("{err}");
    }
    tidepool_server::config::look(&state).await;
    let state_now = get(&state, "/v1/config").await;
    assert!(
        state_now["external"]["change"].as_u64().is_some(),
        "{state_now}"
    );
    assert_eq!(state_now["version"], 2);
    assert_eq!(
        get(&state, "/v1/changes").await[0]["plan"]["class"],
        "security"
    );
}

#[tokio::test]
async fn proposals_wait_for_approval_and_versions_can_be_reverted_to() {
    let config = tempdir();
    copy_example(config.path());
    let state = opened(config.path(), None).await;

    let (status, reply) = apply(
        &state,
        json!({"tables/returns.yaml": RETURNS_TABLE}),
        json!({"propose": true, "author": "an agent"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    assert_eq!(reply["status"], "queued");
    let id = reply["change"].as_u64().unwrap_or(0);
    let pending = get(&state, "/v1/changes").await;
    assert_eq!(pending[0]["author"], "an agent");
    assert_eq!(version(&state).await, 1, "nothing applied yet");

    let (status, reply) = call(
        &state,
        Method::POST,
        &format!("/v1/changes/{id}/approve"),
        &json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    assert_eq!(reply["version"], 2);
    assert_eq!(get(&state, "/v1/changes").await, json!([]));
    assert!(
        state
            .writer()
            .snapshot()
            .model
            .table_named("returns")
            .is_some()
    );

    // another proposal, rejected
    let (_, reply) = apply(
        &state,
        json!({"tables/returns.yaml": null}),
        json!({"propose": true}),
    )
    .await;
    let id = reply["change"].as_u64().unwrap_or(0);
    let (status, _) = call(
        &state,
        Method::POST,
        &format!("/v1/changes/{id}/reject"),
        &json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(get(&state, "/v1/changes").await, json!([]));

    // back to version 1: its files, replacing the set
    let v1 = get(&state, "/v1/config/versions/1").await;
    let files: BTreeMap<String, Value> = v1["files"]
        .as_array()
        .map(|f| {
            f.iter()
                .map(|f| {
                    (
                        f["path"].as_str().unwrap_or_default().to_string(),
                        f["text"].clone(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let (status, reply) = apply(
        &state,
        json!(files),
        json!({"replace": true, "approve": true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    assert_eq!(reply["version"], 3);
    assert!(
        state
            .writer()
            .snapshot()
            .model
            .table_named("returns")
            .is_none()
    );
    assert!(!config.path().join("tables/returns.yaml").exists());
    assert_eq!(
        get(&state, "/v1/config/versions").await[0]["source"],
        "revert"
    );
}

#[tokio::test]
async fn edits_in_the_directory_are_applied_queued_or_reported() {
    let config = tempdir();
    copy_example(config.path());
    let state = opened(config.path(), None).await;
    let write = |path: &str, text: &str| {
        if let Err(err) = std::fs::write(config.path().join(path), text) {
            panic!("{err}");
        }
    };

    // safe: applied
    write("tables/returns.yaml", RETURNS_TABLE);
    tidepool_server::config::look(&state).await;
    assert_eq!(version(&state).await, 2);
    assert_eq!(
        get(&state, "/v1/config/versions").await[0]["source"],
        "files"
    );
    // nothing new: nothing happens
    tidepool_server::config::look(&state).await;
    assert_eq!(version(&state).await, 2);

    // broken: reported, nothing applied
    write("tables/returns.yaml", "name: returns\ncolumns: [");
    tidepool_server::config::look(&state).await;
    let state_now = get(&state, "/v1/config").await;
    assert!(
        state_now["external"]["problems"]
            .as_str()
            .is_some_and(|p| !p.is_empty()),
        "{state_now}"
    );
    assert_eq!(state_now["version"], 2);

    // destructive: waits for approval
    std::fs::remove_file(config.path().join("tables/returns.yaml")).unwrap_or_default();
    tidepool_server::config::look(&state).await;
    let state_now = get(&state, "/v1/config").await;
    let id = state_now["external"]["change"].as_u64();
    assert!(id.is_some(), "{state_now}");
    let pending = get(&state, "/v1/changes").await;
    assert_eq!(pending[0]["source"], "files");
    assert_eq!(
        pending[0]["files"],
        json!({"tables/returns.yaml": null}),
        "only what changed"
    );
    let (status, reply) = call(
        &state,
        Method::POST,
        &format!("/v1/changes/{}/approve", id.unwrap_or(0)),
        &json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    assert!(
        state
            .writer()
            .snapshot()
            .model
            .table_named("returns")
            .is_none()
    );
    assert!(get(&state, "/v1/config").await.get("external").is_none());
}

#[tokio::test]
async fn renames_keep_the_data_and_the_hint_is_written_out() {
    let (config, data) = (tempdir(), tempdir());
    copy_example(config.path());
    let state = opened(config.path(), Some(data.path())).await;
    seed(&state).await;
    let orders =
        std::fs::read_to_string(config.path().join("tables/orders.yaml")).unwrap_or_default();
    let renamed = orders.replace(
        "  quantity:    { type: int32 }",
        "  units:       { type: int32, renamed_from: quantity }",
    );
    let (status, reply) = apply(&state, json!({"tables/orders.yaml": renamed}), json!({})).await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    assert_eq!(reply["plan"]["class"], "safe", "{}", reply["plan"]["text"]);
    let written =
        std::fs::read_to_string(config.path().join("tables/orders.yaml")).unwrap_or_default();
    assert!(
        written.contains("  units:       { type: int32 }"),
        "{written}"
    );
    assert!(!written.contains("renamed_from"));
    let preview = get(&state, "/v1/tables/orders/preview?limit=1").await;
    assert_eq!(preview["columns"][5], "units", "{preview}");
    assert_eq!(
        preview["rows"][0][5], 1,
        "the old values, under the new name"
    );
}

#[tokio::test]
async fn applied_changes_are_committed_in_a_git_repository() {
    let config = tempdir();
    copy_example(config.path());
    let path = config.path().join("tidepool.yaml");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    if let Err(err) = std::fs::write(&path, text.replace("git_commit: false", "git_commit: true")) {
        panic!("{err}");
    }
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(config.path())
            .args(args)
            .output()
    };
    for args in [
        vec!["init", "-q"],
        vec!["config", "user.email", "test@example.com"],
        vec!["config", "user.name", "Test"],
        vec!["add", "-A"],
        vec!["commit", "-q", "-m", "start"],
    ] {
        match git(&args) {
            Ok(out) if out.status.success() => {}
            other => panic!("git {args:?}: {other:?}"),
        }
    }
    let state = opened(config.path(), None).await;
    let (status, reply) = apply(
        &state,
        json!({"tables/returns.yaml": RETURNS_TABLE}),
        json!({"author": "Ada"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    let log = match git(&["log", "-1", "--format=%s"]) {
        Ok(out) => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        Err(err) => panic!("{err}"),
    };
    assert_eq!(log, "tidepool: add table returns [by Ada]");
    let clean = git(&["status", "--porcelain"])
        .map(|o| o.stdout)
        .unwrap_or_default();
    assert!(
        clean.is_empty(),
        "everything committed: {}",
        String::from_utf8_lossy(&clean)
    );
}

#[tokio::test]
async fn sessions_follow_a_model_change() {
    let config = tempdir();
    copy_example(config.path());
    let state = opened(config.path(), None).await;
    seed(&state).await;
    let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
        Ok(listener) => listener,
        Err(err) => panic!("{err}"),
    };
    let address = listener.local_addr().unwrap_or_else(|e| panic!("{e}"));
    let app = tidepool_server::router(Arc::clone(&state));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let (mut socket, _) =
        match tokio_tungstenite::connect_async(format!("ws://{address}/v1/ws")).await {
            Ok(connected) => connected,
            Err(err) => panic!("{err}"),
        };
    let next = async |socket: &mut _, kind: &str| -> Value {
        let socket: &mut tokio_tungstenite::WebSocketStream<_> = socket;
        loop {
            let message = match tokio::time::timeout(Duration::from_secs(5), socket.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => text,
                Ok(Some(Ok(_))) => continue,
                other => panic!("waiting for `{kind}`: {other:?}"),
            };
            let value: Value = serde_json::from_str(message.as_str()).unwrap_or_default();
            if value["type"] == kind {
                return value;
            }
        }
    };
    for message in [
        json!({"type": "hello"}),
        json!({"type": "open_dashboard", "id": "sales"}),
        json!({"type": "select", "field": "customers.coast", "values": ["unknown"]}),
    ] {
        if let Err(err) = socket.send(Message::text(message.to_string())).await {
            panic!("{err}");
        }
    }
    assert_eq!(next(&mut socket, "model").await["version"], 1);
    let selection = next(&mut socket, "selection").await;
    assert_eq!(selection["fields"], json!([]));
    let selection = next(&mut socket, "selection").await;
    assert_eq!(selection["fields"][0]["field"], "customers.coast");

    // the selected field and the open dashboard go
    let model = std::fs::read_to_string(config.path().join("model.ossie.yaml")).unwrap_or_default();
    let without_coast = model.replace(
        "      - name: coast                      # derived over one dictionary column: evaluated per
        expression:                      # region, not per row (a derived dictionary, plan §4.4)
          dialects:
            - dialect: ANSI_SQL
              expression: \"CASE WHEN region IN ('east', 'west') THEN 'coastal' ELSE COALESCE(region, 'unknown') END\"
",
        "",
    );
    assert_ne!(model, without_coast);
    let (status, reply) = apply(
        &state,
        json!({"model.ossie.yaml": without_coast, "dashboards/sales.yaml": null}),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    let model = next(&mut socket, "model").await;
    assert_eq!(model["version"], 2);
    let left: Vec<&Value> = model["dashboards"]
        .as_array()
        .map(|d| d.iter().map(|d| &d["id"]).collect())
        .unwrap_or_default();
    assert_eq!(left, vec![&json!("sales_recent")], "only the other one");
    assert_eq!(next(&mut socket, "selection").await["fields"], json!([]));
    let error = next(&mut socket, "error").await;
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|m| m.contains("`sales` was removed")),
        "{error}"
    );
}
