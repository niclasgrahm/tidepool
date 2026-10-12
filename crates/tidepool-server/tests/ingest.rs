//! Writing rows over HTTP, and reading tables back, against the retail model.

mod common;

use std::sync::Arc;

use arrow_array::{ArrayRef, Int32Array, RecordBatch, StringArray, TimestampMicrosecondArray};
use arrow_ipc::writer::StreamWriter;
use arrow_schema::{DataType, Field, Schema, TimeUnit};
use axum::http::{Method, StatusCode};
use common::{call, ready_state, request, send};
use serde_json::{Value, json};
use tidepool_server::{AppState, Tokens};
use tidepool_store::WriterHandle;

const JSON: (&str, &str) = ("content-type", "application/json");
const NDJSON: (&str, &str) = ("content-type", "application/x-ndjson");
const ARROW: (&str, &str) = ("content-type", "application/vnd.apache.arrow.stream");

fn order(id: u32) -> Value {
    json!({
        "order_id": format!("o{id}"),
        "customer_id": "c1",
        "product_id": "p1",
        "status": "paid",
        "ordered_at": "2026-10-07T12:00:00Z",
        "quantity": id,
        "amount": format!("{id}.25"),
    })
}

async fn post(
    state: &Arc<AppState>,
    path: &str,
    headers: &[(&str, &str)],
    body: impl Into<axum::body::Body>,
) -> common::Reply {
    send(
        Arc::clone(state),
        request(Method::POST, path, headers, body),
    )
    .await
}

async fn preview(state: &Arc<AppState>, table: &str) -> Value {
    call(
        Arc::clone(state),
        Method::GET,
        &format!("/v1/tables/{table}/preview"),
    )
    .await
    .json()
}

#[tokio::test]
async fn a_json_batch_is_written_and_visible_when_asked_to_wait() {
    let state = ready_state();
    let body = Value::Array(vec![order(1), order(2)]).to_string();
    let reply = post(&state, "/v1/tables/orders/rows?wait=visible", &[JSON], body).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let response = reply.json();
    assert_eq!(response["written"], 2);
    assert!(response["epoch"].as_u64().is_some_and(|e| e > 1));

    let rows = preview(&state, "orders").await;
    assert_eq!(rows["rows"].as_array().map(Vec::len), Some(2));
    assert_eq!(rows["rows"][0][0], "o2", "newest first");
    assert_eq!(
        rows["rows"][0][6], "2.25",
        "decimals come back exact, as strings"
    );
    assert_eq!(rows["columns"][0], "order_id");
}

#[tokio::test]
async fn ndjson_and_arrow_are_written_too() {
    let state = ready_state();
    let ndjson = format!("{}\n{}\n", order(1), order(2));
    let reply = post(&state, "/v1/tables/orders/rows", &[NDJSON], ndjson).await;
    assert_eq!(reply.json()["written"], 2, "{}", reply.text());

    let schema = Arc::new(Schema::new(vec![
        Field::new("order_id", DataType::Utf8, false),
        Field::new("customer_id", DataType::Utf8, false),
        Field::new("product_id", DataType::Utf8, false),
        Field::new("status", DataType::Utf8, false),
        Field::new(
            "ordered_at",
            DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
            false,
        ),
        Field::new("quantity", DataType::Int32, false),
        Field::new("amount", DataType::Utf8, false),
    ]));
    let columns: Vec<ArrayRef> = vec![
        Arc::new(StringArray::from(vec!["o3"])),
        Arc::new(StringArray::from(vec!["c1"])),
        Arc::new(StringArray::from(vec!["p1"])),
        Arc::new(StringArray::from(vec!["paid"])),
        Arc::new(TimestampMicrosecondArray::from(vec![0]).with_timezone("UTC")),
        Arc::new(Int32Array::from(vec![3])),
        Arc::new(StringArray::from(vec!["3.5"])),
    ];
    let record = match RecordBatch::try_new(schema, columns) {
        Ok(record) => record,
        Err(err) => panic!("{err}"),
    };
    let mut body = Vec::new();
    {
        let Ok(mut writer) = StreamWriter::try_new(&mut body, &record.schema()) else {
            panic!("arrow writer");
        };
        if writer
            .write(&record)
            .and_then(|()| writer.finish())
            .is_err()
        {
            panic!("writing arrow");
        }
    }
    let reply = post(
        &state,
        "/v1/tables/orders/rows?wait=visible",
        &[ARROW],
        body,
    )
    .await;
    assert_eq!(reply.json()["written"], 1, "{}", reply.text());
    let rows = preview(&state, "orders").await;
    assert_eq!(rows["rows"][0][4], "1970-01-01T00:00:00Z");
    assert_eq!(rows["rows"].as_array().map(Vec::len), Some(3));
}

#[tokio::test]
async fn a_bad_batch_is_rejected_whole_with_every_problem_listed() {
    let state = ready_state();
    let mut bad = order(2);
    bad["amount"] = json!("1.234");
    bad["colour"] = json!("red");
    let body = Value::Array(vec![order(1), bad]).to_string();
    let reply = post(&state, "/v1/tables/orders/rows", &[JSON], body).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    let error = reply.json();
    assert_eq!(
        error["error"],
        "2 problems in the batch; nothing was written"
    );
    let listed: Vec<(Value, Value)> = error["errors"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| (e["row"].clone(), e["column"].clone()))
        .collect();
    assert_eq!(
        listed,
        [(json!(1), json!("colour")), (json!(1), json!("amount"))]
    );

    let _ = state.writer().publish().await;
    assert_eq!(
        preview(&state, "orders").await["rows"],
        json!([]),
        "nothing was written"
    );
}

#[tokio::test]
async fn unknown_tables_formats_and_waits_are_refused() {
    let state = ready_state();
    let reply = post(&state, "/v1/tables/order/rows", &[JSON], "[]").await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert!(
        reply.json()["error"]
            .as_str()
            .is_some_and(|e| e.contains("did you mean `orders`?"))
    );

    let reply = post(
        &state,
        "/v1/tables/orders/rows",
        &[("content-type", "text/csv")],
        "a,b",
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);

    let reply = post(&state, "/v1/tables/orders/rows?wait=soon", &[JSON], "[]").await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn upserts_replace_and_deletes_remove_by_key() {
    let state = ready_state();
    let customer = |region: &str| {
        json!([{ "customer_id": "c1", "name": "Ann", "region": region }]).to_string()
    };
    post(
        &state,
        "/v1/tables/customers/rows",
        &[JSON],
        customer("north"),
    )
    .await;
    let reply = post(
        &state,
        "/v1/tables/customers/rows?wait=visible",
        &[JSON],
        customer("south"),
    )
    .await;
    assert_eq!(
        (
            reply.json()["written"].clone(),
            reply.json()["replaced"].clone()
        ),
        (json!(1), json!(1))
    );
    let table = call(Arc::clone(&state), Method::GET, "/v1/tables/customers")
        .await
        .json();
    assert_eq!(table["rows"], 1);
    assert_eq!(table["written_total"], 2);

    let delete = |body: &str| {
        request(
            Method::DELETE,
            "/v1/tables/customers/rows?wait=visible",
            &[JSON],
            body.to_string(),
        )
    };
    let reply = send(
        Arc::clone(&state),
        delete(r#"[{"customer_id": "c1"}, {"customer_id": "zz"}]"#),
    )
    .await;
    assert_eq!(
        (
            reply.json()["replaced"].clone(),
            reply.json()["missing"].clone()
        ),
        (json!(1), json!(1))
    );
    assert_eq!(preview(&state, "customers").await["rows"], json!([]));

    let append = send(
        Arc::clone(&state),
        request(Method::DELETE, "/v1/tables/orders/rows", &[JSON], "[]"),
    )
    .await;
    assert_eq!(append.status, StatusCode::BAD_REQUEST);
    assert!(
        append.json()["error"]
            .as_str()
            .is_some_and(|e| e.contains("append table"))
    );
}

#[tokio::test]
async fn a_put_replaces_a_tables_contents() {
    let state = ready_state();
    let customer =
        |id: &str, region: &str| json!({ "customer_id": id, "name": "Ann", "region": region });
    post(
        &state,
        "/v1/tables/customers/rows",
        &[JSON],
        json!([customer("c1", "north"), customer("c2", "south")]).to_string(),
    )
    .await;
    let put = |body: Value, headers: &[(&'static str, &'static str)]| {
        request(
            Method::PUT,
            "/v1/tables/customers/rows?wait=visible",
            headers,
            body.to_string(),
        )
    };
    // c2 isn't listed any more, c3 is new: c1 replaced, c2 deleted
    let body = json!([customer("c1", "east"), customer("c3", "west")]);
    let reply = send(
        Arc::clone(&state),
        put(body.clone(), &[JSON, ("idempotency-key", "sync-1")]),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(
        (
            reply.json()["written"].clone(),
            reply.json()["replaced"].clone()
        ),
        (json!(2), json!(2))
    );
    let rows = preview(&state, "customers").await["rows"].clone();
    let mut ids: Vec<String> = rows
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| r[0].to_string())
        .collect();
    ids.sort();
    assert_eq!(ids, [r#""c1""#, r#""c3""#]);

    // a retry gets the first answer and changes nothing
    let retry = send(
        Arc::clone(&state),
        put(body, &[JSON, ("idempotency-key", "sync-1")]),
    )
    .await;
    assert_eq!(retry.json(), reply.json());

    // only upsert tables have keys to replace by
    let append = send(
        Arc::clone(&state),
        request(Method::PUT, "/v1/tables/orders/rows", &[JSON], "[]"),
    )
    .await;
    assert_eq!(append.status, StatusCode::BAD_REQUEST);
    assert!(
        append.json()["error"]
            .as_str()
            .is_some_and(|e| e.contains("append table")),
        "{}",
        append.text()
    );
}

#[tokio::test]
async fn a_retry_with_the_same_idempotency_key_writes_nothing() {
    let state = ready_state();
    let body = Value::Array(vec![order(1)]).to_string();
    let headers = [JSON, ("idempotency-key", "batch-1")];
    let first = post(
        &state,
        "/v1/tables/orders/rows?wait=visible",
        &headers,
        body.clone(),
    )
    .await
    .json();
    let second = post(
        &state,
        "/v1/tables/orders/rows?wait=visible",
        &headers,
        body.clone(),
    )
    .await
    .json();
    assert_eq!(first, second);
    let _ = state.writer().publish().await;
    assert_eq!(
        preview(&state, "orders").await["rows"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );

    // a different key is a different write
    let other = post(
        &state,
        "/v1/tables/orders/rows?wait=visible",
        &[JSON, ("idempotency-key", "batch-2")],
        body,
    )
    .await;
    assert_eq!(other.json()["written"], 1);
}

/// A server on the data directory `data`, checkpointing every `every`.
fn on_disk(data: &std::path::Path, every: std::time::Duration) -> Arc<AppState> {
    let config = tidepool_store::WriterConfig {
        data: Some(data.to_path_buf()),
        checkpoint_interval: every,
        ..tidepool_store::WriterConfig::default()
    };
    match AppState::new(common::retail(), config, None, Tokens::default()) {
        Ok(state) => {
            state.set_ready();
            Arc::new(state)
        }
        Err(err) => panic!("opening {}: {err}", data.display()),
    }
}

async fn order_count(state: &Arc<AppState>) -> u64 {
    let table = call(Arc::clone(state), Method::GET, "/v1/tables/orders")
        .await
        .json();
    table["rows"].as_u64().unwrap_or(u64::MAX)
}

#[tokio::test]
async fn a_retry_after_a_restart_gets_the_first_response_and_writes_nothing() {
    use std::time::Duration;
    let data = match tempfile::tempdir() {
        Ok(dir) => dir,
        Err(err) => panic!("{err}"),
    };
    let rows = "/v1/tables/orders/rows?wait=visible";
    let batch_a = Value::Array(vec![order(1), order(2)]).to_string();
    let batch_b = Value::Array(vec![order(3)]).to_string();
    let key_a = [JSON, ("idempotency-key", "batch-a")];
    let key_b = [JSON, ("idempotency-key", "batch-b")];

    // `batch-a`, then a checkpoint that holds it
    let first_a = {
        let state = on_disk(data.path(), Duration::ZERO);
        let reply = post(&state, rows, &key_a, batch_a.clone()).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
        let Some(log) = state.writer().log() else {
            panic!("a data directory has a log");
        };
        let last = log.last_lsn.load(std::sync::atomic::Ordering::Relaxed);
        for _ in 0..200 {
            if log
                .checkpoint_lsn
                .load(std::sync::atomic::Ordering::Relaxed)
                >= last
            {
                break;
            }
            let _ = state.writer().publish().await;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            log.checkpoint_lsn
                .load(std::sync::atomic::Ordering::Relaxed)
                >= last,
            "a checkpoint should cover `batch-a`"
        );
        reply.json()
    };
    // the writer stops with the last handle; give it a moment
    tokio::time::sleep(Duration::from_millis(100)).await;

    // restarted: `batch-b`, in the log only
    let first_b = {
        let state = on_disk(data.path(), Duration::from_hours(1));
        let reply = post(&state, rows, &key_b, batch_b.clone()).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
        assert_eq!(order_count(&state).await, 3);
        reply.json()
    };
    tokio::time::sleep(Duration::from_millis(100)).await;

    // restarted again: both retries get their first response, nothing is written
    let state = on_disk(data.path(), Duration::from_hours(1));
    let retry_a = post(&state, rows, &key_a, batch_a).await;
    assert_eq!(retry_a.status, StatusCode::OK, "{}", retry_a.text());
    assert_eq!(retry_a.json(), first_a, "the key from the checkpoint");
    let retry_b = post(&state, rows, &key_b, batch_b.clone()).await;
    assert_eq!(retry_b.status, StatusCode::OK, "{}", retry_b.text());
    assert_eq!(retry_b.json(), first_b, "the key from the log");
    let _ = state.writer().publish().await;
    assert_eq!(order_count(&state).await, 3, "the retries wrote nothing");

    // a new key still writes
    let other = post(
        &state,
        rows,
        &[JSON, ("idempotency-key", "batch-c")],
        batch_b,
    )
    .await;
    assert_eq!(other.json()["written"], 1);
}

#[tokio::test]
async fn a_full_queue_answers_503_with_retry_after() {
    let model = common::retail();
    let (writer, _guard) = WriterHandle::unconsumed(model, 0);
    let state = Arc::new(AppState::with_writer(writer, None, Tokens::default()));
    state.set_ready();
    let body = Value::Array(vec![order(1)]).to_string();
    let headers = [JSON, ("idempotency-key", "k")];
    let reply = post(&state, "/v1/tables/orders/rows", &headers, body.clone()).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reply.header("retry-after").as_deref(), Some("1"));
    // the key was released, so the retry is tried again rather than refused as in flight
    let retry = post(&state, "/v1/tables/orders/rows", &headers, body).await;
    assert_eq!(retry.status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn tables_are_listed_with_their_columns() {
    let state = ready_state();
    let tables = call(Arc::clone(&state), Method::GET, "/v1/tables")
        .await
        .json();
    let names: Vec<&str> = tables
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert_eq!(names, ["categories", "customers", "orders", "products"]);
    let orders = &tables[2];
    assert_eq!(orders["mode"], "append");
    assert_eq!(
        orders["columns"][6],
        json!({"name": "amount", "type": "decimal(14,2)", "nullable": false, "encoding": "plain"})
    );
    let missing = call(Arc::clone(&state), Method::GET, "/v1/tables/nope").await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_spec_has_a_typed_rows_endpoint_per_table() {
    let spec = call(ready_state(), Method::GET, "/v1/openapi.json")
        .await
        .json();
    let paths = &spec["paths"];
    assert!(
        paths.get("/v1/tables/{table}/rows").is_none(),
        "replaced by the per-table paths"
    );
    assert!(paths["/v1/tables/orders/rows"]["post"].is_object());
    assert!(
        paths["/v1/tables/orders/rows"].get("delete").is_none(),
        "orders is append-only"
    );
    assert!(paths["/v1/tables/customers/rows"]["delete"].is_object());
    assert!(paths["/v1/tables/customers/rows"]["put"].is_object());
    assert!(
        paths["/v1/tables/orders/rows"].get("put").is_none(),
        "an append table can't be replaced"
    );
    assert_eq!(
        paths["/v1/tables/orders/rows"]["post"]["operationId"],
        "ingestRows_orders"
    );

    let row = &spec["components"]["schemas"]["Row_orders"];
    let required: Vec<&str> = row["required"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    assert!(required.contains(&"order_id"));
    assert!(!required.contains(&"note"), "nullable columns are optional");
    assert_eq!(row["properties"]["quantity"]["maximum"], i32::MAX);
    assert_eq!(
        spec["components"]["schemas"]["Key_customers"]["required"],
        json!(["customer_id"])
    );
}
