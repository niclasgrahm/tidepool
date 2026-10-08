//! Live dashboards over a real WebSocket: the retail dashboard opened,
//! selections made and undone, and data arriving while it's open.

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::http::{Method, StatusCode};
use common::{ready_state, request, retail_with, send, state};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tidepool_server::AppState;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn post(state: &Arc<AppState>, path: &str, body: &Value) {
    let reply = send(
        Arc::clone(state),
        request(
            Method::POST,
            path,
            &[("content-type", "application/json")],
            body.to_string(),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
}

fn order(id: &str, customer: &str, status: &str, amount: &str) -> Value {
    json!({
        "order_id": id, "customer_id": customer, "product_id": "p1", "status": status,
        "ordered_at": "2026-02-01T10:00:00Z", "quantity": 1, "amount": amount,
    })
}

/// A server on a free port with the retail data loaded, and its address.
async fn server() -> (Arc<AppState>, String) {
    server_for(ready_state()).await
}

/// [`server`] for a state of its own (an edited model).
async fn server_for(state: Arc<AppState>) -> (Arc<AppState>, String) {
    post(
        &state,
        "/v1/tables/customers/rows?wait=visible",
        &json!([
            {"customer_id": "c1", "name": "Ada", "region": "north"},
            {"customer_id": "c2", "name": "Bo", "region": "south"},
        ]),
    )
    .await;
    post(
        &state,
        "/v1/tables/products/rows?wait=visible",
        &json!([{"product_id": "p1", "name": "Kayak", "category": "boats", "list_price": "900.00"}]),
    )
    .await;
    post(
        &state,
        "/v1/tables/orders/rows?wait=visible",
        &json!([
            order("o1", "c1", "paid", "10.00"),
            order("o2", "c2", "paid", "5.00")
        ]),
    )
    .await;
    let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
        Ok(listener) => listener,
        Err(err) => panic!("binding: {err}"),
    };
    let address = match listener.local_addr() {
        Ok(address) => address,
        Err(err) => panic!("the bound address: {err}"),
    };
    let app = tidepool_server::router(Arc::clone(&state));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (state, format!("ws://{address}/v1/ws"))
}

async fn connect(url: &str) -> Socket {
    match tokio_tungstenite::connect_async(url).await {
        Ok((socket, _)) => socket,
        Err(err) => panic!("connecting to {url}: {err}"),
    }
}

async fn say(socket: &mut Socket, message: Value) {
    if let Err(err) = socket.send(Message::text(message.to_string())).await {
        panic!("sending {message}: {err}");
    }
}

/// The next message of type `kind`, skipping others (status ticks).
async fn next(socket: &mut Socket, kind: &str) -> Value {
    loop {
        let message = match tokio::time::timeout(Duration::from_secs(5), socket.next()).await {
            Ok(Some(Ok(message))) => message,
            other => panic!("waiting for `{kind}`: {other:?}"),
        };
        let Message::Text(text) = message else {
            continue;
        };
        let value: Value = match serde_json::from_str(text.as_str()) {
            Ok(value) => value,
            Err(err) => panic!("unreadable message {text}: {err}"),
        };
        if value["type"] == kind {
            return value;
        }
    }
}

/// The visual `id` in a frame.
fn visual<'a>(frame: &'a Value, id: &str) -> &'a Value {
    match frame["visuals"]
        .as_array()
        .and_then(|v| v.iter().find(|v| v["id"] == id))
    {
        Some(visual) => visual,
        None => panic!("no visual {id} in {frame}"),
    }
}

/// The rows of a visual's result.
fn rows(frame: &Value, id: &str) -> Value {
    visual(frame, id)["result"]["rows"].clone()
}

#[tokio::test]
async fn a_dashboard_opens_follows_selections_and_goes_back() {
    let (_state, url) = server().await;
    let mut socket = connect(&url).await;
    say(&mut socket, json!({"type": "hello"})).await;
    let welcome = next(&mut socket, "welcome").await;
    assert!(welcome["session_id"].is_string());
    let model = next(&mut socket, "model").await;
    assert_eq!(model["dashboards"][0]["id"], "sales");

    say(
        &mut socket,
        json!({"type": "open_dashboard", "id": "sales"}),
    )
    .await;
    let frame = next(&mut socket, "frame").await;
    assert_eq!(
        rows(&frame, "by_region"),
        json!([["north", "10.00"], ["south", "5.00"]])
    );
    assert_eq!(rows(&frame, "kpi_revenue"), json!([["15.00"]]));

    say(
        &mut socket,
        json!({"type": "select", "field": "customers.region", "values": ["north"]}),
    )
    .await;
    let selection = next(&mut socket, "selection").await;
    assert_eq!(
        selection["fields"],
        json!([{"field": "customers.region", "values": ["north"]}])
    );
    assert_eq!(selection["can_back"], true);
    let frame = next(&mut socket, "frame").await;
    assert_eq!(rows(&frame, "by_region"), json!([["north", "10.00"]]));
    let regions = match frame["filters"]
        .as_array()
        .and_then(|f| f.iter().find(|f| f["field"] == "customers.region"))
    {
        Some(filter) => filter["values"].clone(),
        None => panic!("no region filter in {frame}"),
    };
    // its own selection doesn't narrow a field's alternatives
    assert_eq!(
        regions,
        json!([
            {"value": "north", "state": "selected", "rows": 1},
            {"value": "south", "state": "possible", "rows": 1},
        ])
    );

    say(&mut socket, json!({"type": "back"})).await;
    let selection = next(&mut socket, "selection").await;
    assert_eq!(selection["fields"], json!([]));
    assert_eq!(selection["can_forward"], true);
    let frame = next(&mut socket, "frame").await;
    assert_eq!(
        rows(&frame, "by_region"),
        json!([["north", "10.00"], ["south", "5.00"]])
    );
}

#[tokio::test]
async fn new_data_arrives_as_a_frame() {
    let (state, url) = server().await;
    let mut socket = connect(&url).await;
    say(
        &mut socket,
        json!({"type": "open_dashboard", "id": "sales"}),
    )
    .await;
    let _ = next(&mut socket, "frame").await;

    post(
        &state,
        "/v1/tables/orders/rows",
        &json!([order("o3", "c2", "paid", "7.50")]),
    )
    .await;
    let frame = next(&mut socket, "frame").await;
    // by revenue, descending
    assert_eq!(
        rows(&frame, "by_region"),
        json!([["south", "12.50"], ["north", "10.00"]])
    );
    // only what changed: filter lists count rows of their own tables
    // (customers per region, products per category), and an order changes
    // neither, so none is resent
    assert_eq!(frame["filters"], json!([]), "{frame}");
}

#[tokio::test]
async fn a_paused_session_gets_no_frames_until_it_resumes() {
    let (state, url) = server().await;
    let mut socket = connect(&url).await;
    say(
        &mut socket,
        json!({"type": "open_dashboard", "id": "sales"}),
    )
    .await;
    let _ = next(&mut socket, "frame").await;
    say(&mut socket, json!({"type": "pause"})).await;
    post(
        &state,
        "/v1/tables/orders/rows?wait=visible",
        &json!([order("o3", "c1", "paid", "1.00")]),
    )
    .await;
    say(&mut socket, json!({"type": "resume"})).await;
    let frame = next(&mut socket, "frame").await;
    assert_eq!(
        rows(&frame, "by_region"),
        json!([["north", "11.00"], ["south", "5.00"]])
    );
}

#[tokio::test]
async fn unknown_names_are_errors_and_the_session_carries_on() {
    let (_state, url) = server().await;
    let mut socket = connect(&url).await;
    say(
        &mut socket,
        json!({"type": "select", "field": "customers.regoin", "values": ["x"]}),
    )
    .await;
    let error = next(&mut socket, "error").await;
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|m| m.contains("did you mean `region`")),
        "{error}"
    );
    say(&mut socket, json!({"type": "open_dashboard", "id": "nope"})).await;
    let error = next(&mut socket, "error").await;
    assert_eq!(error["message"], "no dashboard `nope`");
    say(
        &mut socket,
        json!({"type": "open_dashboard", "id": "sales"}),
    )
    .await;
    let _ = next(&mut socket, "frame").await;
}

#[tokio::test]
async fn a_session_can_sort_a_visual_its_own_way() {
    let (_state, url) = server().await;
    let mut socket = connect(&url).await;
    say(
        &mut socket,
        json!({"type": "open_dashboard", "id": "sales"}),
    )
    .await;
    let frame = next(&mut socket, "frame").await;
    assert_eq!(
        rows(&frame, "by_region"),
        json!([["north", "10.00"], ["south", "5.00"]])
    );
    say(
        &mut socket,
        json!({"type": "sort", "visual": "by_region", "column": "revenue"}),
    )
    .await;
    let frame = next(&mut socket, "frame").await;
    assert_eq!(
        rows(&frame, "by_region"),
        json!([["south", "5.00"], ["north", "10.00"]])
    );
    say(
        &mut socket,
        json!({"type": "sort", "visual": "by_region", "column": "nope"}),
    )
    .await;
    let error = next(&mut socket, "error").await;
    assert_eq!(error["message"], "visual `by_region` has no column `nope`");
}

#[tokio::test]
async fn a_kpi_carries_its_comparison_under_the_selection() {
    let model = retail_with(
        "dashboards/sales.yaml",
        "compare: revenue_prev_year",
        "compare: paid_revenue",
    );
    let (state, url) = server_for(state(model, tidepool_server::Tokens::default(), None)).await;
    post(
        &state,
        "/v1/tables/orders/rows?wait=visible",
        &json!([order("o3", "c1", "pending", "7.00")]),
    )
    .await;
    let mut socket = connect(&url).await;
    say(&mut socket, json!({"type": "hello"})).await;
    say(
        &mut socket,
        json!({"type": "open_dashboard", "id": "sales"}),
    )
    .await;
    let frame = next(&mut socket, "frame").await;
    assert_eq!(rows(&frame, "kpi_revenue"), json!([["22.00"]]));
    assert_eq!(visual(&frame, "kpi_revenue")["compare"], "15.00");

    say(
        &mut socket,
        json!({"type": "select", "field": "customers.region", "values": ["north"]}),
    )
    .await;
    let frame = next(&mut socket, "frame").await;
    assert_eq!(rows(&frame, "kpi_revenue"), json!([["17.00"]]));
    assert_eq!(visual(&frame, "kpi_revenue")["compare"], "10.00");
}

#[tokio::test]
async fn a_comparison_that_cant_be_computed_yet_is_left_out() {
    // the example compares with a time-shifted metric, which arrives in M8
    let (_state, url) = server().await;
    let mut socket = connect(&url).await;
    say(&mut socket, json!({"type": "hello"})).await;
    say(
        &mut socket,
        json!({"type": "open_dashboard", "id": "sales"}),
    )
    .await;
    let frame = next(&mut socket, "frame").await;
    assert_eq!(rows(&frame, "kpi_revenue"), json!([["15.00"]]));
    assert!(
        visual(&frame, "kpi_revenue")
            .get("compare")
            .is_none_or(Value::is_null)
    );
}
