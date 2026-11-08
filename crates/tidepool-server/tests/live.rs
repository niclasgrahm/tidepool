//! Live dashboards over a real WebSocket: the retail dashboard opened,
//! selections made and undone, and data arriving while it's open.

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::http::{Method, StatusCode};
use common::{ready_state, request, retail, retail_with, send, state};
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
    // its own selection doesn't narrow a field's alternatives, nor their
    // measures (the region filter shows revenue)
    assert_eq!(
        regions,
        json!([
            {"value": "north", "state": "selected", "rows": 1, "measure": "10.00"},
            {"value": "south", "state": "possible", "rows": 1, "measure": "5.00"},
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
    // neither; but the region list shows revenue per region, which it does
    // change, so that one alone is resent
    assert_eq!(
        frame["filters"],
        json!([{"field": "customers.region", "offset": 0, "total": 2, "values": [
            {"value": "north", "state": "possible", "rows": 1, "measure": "10.00"},
            {"value": "south", "state": "possible", "rows": 1, "measure": "12.50"},
        ]}]),
        "{frame}"
    );
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
async fn the_example_kpi_compares_with_the_year_before_the_selected_dates() {
    // `revenue_prev_year` is revenue with the time selection a year back
    let (_state, url) = server().await;
    let mut socket = connect(&url).await;
    say(&mut socket, json!({"type": "hello"})).await;
    say(
        &mut socket,
        json!({"type": "open_dashboard", "id": "sales"}),
    )
    .await;
    let frame = next(&mut socket, "frame").await;
    // no time selection: every row, shifted or not
    assert_eq!(rows(&frame, "kpi_revenue"), json!([["15.00"]]));
    assert_eq!(visual(&frame, "kpi_revenue")["compare"], "15.00");

    // 2027 has no orders; its year before (2026) has them all
    say(
        &mut socket,
        json!({"type": "select_range", "field": "orders.ordered_at",
               "from": "2027-01-01T00:00:00Z", "to": "2028-01-01T00:00:00Z"}),
    )
    .await;
    let frame = next(&mut socket, "frame").await;
    assert_eq!(rows(&frame, "kpi_revenue"), json!([[null]]));
    assert_eq!(visual(&frame, "kpi_revenue")["compare"], "15.00");
}

/// A filter's values in a frame; `None` when they didn't change.
fn filter_values(frame: &Value, field: &str) -> Option<Value> {
    frame["filters"]
        .as_array()
        .and_then(|f| f.iter().find(|f| f["field"] == field))
        .map(|filter| filter["values"].clone())
}

/// Selecting a category with `filter_direction: both` narrows regions to
/// those of customers who bought from it, through orders.
async fn regions_after_selecting_paddles(direction: &str) -> (Option<Value>, Value) {
    let model = retail_with(
        "model.ossie.yaml",
        r#"{"filter_direction": "one_to_many"}"#,
        &format!(r#"{{"filter_direction": "{direction}"}}"#),
    );
    let (state, url) = server_for(state(model, tidepool_server::Tokens::default(), None)).await;
    post(
        &state,
        "/v1/tables/products/rows?wait=visible",
        &json!([{"product_id": "p2", "name": "Paddle", "category": "paddles", "list_price": "90.00"}]),
    )
    .await;
    let mut paddle = order("o3", "c1", "paid", "3.00");
    paddle["product_id"] = json!("p2");
    post(
        &state,
        "/v1/tables/orders/rows?wait=visible",
        &json!([paddle]),
    )
    .await;

    let mut socket = connect(&url).await;
    say(&mut socket, json!({"type": "hello"})).await;
    say(
        &mut socket,
        json!({"type": "open_dashboard", "id": "sales"}),
    )
    .await;
    next(&mut socket, "frame").await;
    say(
        &mut socket,
        json!({"type": "select", "field": "products.category", "values": ["paddles"]}),
    )
    .await;
    let frame = next(&mut socket, "frame").await;
    (
        filter_values(&frame, "customers.region"),
        rows(&frame, "by_region"),
    )
}

#[tokio::test]
async fn selections_flow_from_orders_to_customers_when_the_model_says_both() {
    let (regions, by_region) = regions_after_selecting_paddles("both").await;
    assert_eq!(
        regions,
        Some(json!([
            {"value": "north", "state": "possible", "rows": 1, "measure": "3.00"},
            {"value": "south", "state": "excluded", "rows": 0, "measure": null},
        ]))
    );
    assert_eq!(by_region, json!([["north", "3.00"]]));

    // one way only, every customer stays possible, though their revenue
    // (the list's measure) is of paddles now: none for the south
    let (regions, by_region) = regions_after_selecting_paddles("one_to_many").await;
    assert_eq!(
        regions,
        Some(json!([
            {"value": "north", "state": "possible", "rows": 1, "measure": "3.00"},
            {"value": "south", "state": "possible", "rows": 1, "measure": null},
        ]))
    );
    assert_eq!(by_region, json!([["north", "3.00"]]));
}

/// Microseconds for an RFC 3339 time, as the server's clock counts.
fn at(text: &str) -> i64 {
    match chrono::DateTime::parse_from_rfc3339(text) {
        Ok(time) => time.timestamp_micros(),
        Err(err) => panic!("{text}: {err}"),
    }
}

#[tokio::test]
async fn a_relative_window_moves_with_the_clock_without_new_data() {
    let (state, url) = server().await;
    // the orders are from 10:00 on 1 February; it's just before midnight
    state.set_clock(Some(at("2026-02-01T23:59:30Z")));
    let mut socket = connect(&url).await;
    say(
        &mut socket,
        json!({"type": "open_dashboard", "id": "sales"}),
    )
    .await;
    let _ = next(&mut socket, "frame").await;

    say(
        &mut socket,
        json!({"type": "select_relative", "field": "orders.ordered_at",
               "window": {"unit": "day", "last": 1}}),
    )
    .await;
    let selection = next(&mut socket, "selection").await;
    let today = &selection["fields"][0];
    assert_eq!(
        today["relative"],
        json!({"unit": "day", "last": 1}),
        "{selection}"
    );
    assert_eq!(today["from"], "2026-02-01T00:00:00Z", "where it is now");
    assert_eq!(today["to"], "2026-02-02T00:00:00Z");
    let frame = next(&mut socket, "frame").await;
    assert_eq!(rows(&frame, "kpi_revenue"), json!([["15.00"]]));

    // midnight passes: "today" is the 2nd now, and nothing was ordered then
    state.set_clock(Some(at("2026-02-02T00:00:30Z")));
    let selection = next(&mut socket, "selection").await;
    assert_eq!(selection["fields"][0]["from"], "2026-02-02T00:00:00Z");
    assert_eq!(selection["fields"][0]["relative"]["unit"], "day");
    let frame = next(&mut socket, "frame").await;
    assert_eq!(rows(&frame, "kpi_revenue"), json!([[null]]));

    // and a window a date range can't be is refused, the session unchanged
    say(
        &mut socket,
        json!({"type": "select_relative", "field": "customers.region",
               "window": {"unit": "day", "last": 7}}),
    )
    .await;
    let error = next(&mut socket, "error").await;
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|m| m.contains("only dates and times")),
        "{error}"
    );
}

#[tokio::test]
async fn a_dashboard_opens_on_its_default_window_the_first_time_only() {
    let (state, url) = server().await;
    // the last seven days reach back to 28 January
    state.set_clock(Some(at("2026-02-03T12:00:00Z")));
    let mut socket = connect(&url).await;
    say(&mut socket, json!({"type": "hello"})).await;
    let model = next(&mut socket, "model").await;
    let recent = match model["dashboards"]
        .as_array()
        .and_then(|d| d.iter().find(|d| d["id"] == "sales_recent"))
    {
        Some(recent) => recent.clone(),
        None => panic!("no sales_recent in {model}"),
    };
    assert_eq!(
        recent["filters"][0],
        json!({"field": "orders.ordered_at", "widget": "date_range", "timed": true,
               "default": {"unit": "day", "last": 7}})
    );
    let _ = next(&mut socket, "selection").await;

    say(
        &mut socket,
        json!({"type": "open_dashboard", "id": "sales_recent"}),
    )
    .await;
    let selection = next(&mut socket, "selection").await;
    assert_eq!(
        selection["fields"][0]["relative"],
        json!({"unit": "day", "last": 7})
    );
    assert_eq!(selection["fields"][0]["from"], "2026-01-28T00:00:00Z");
    assert_eq!(
        selection["can_back"], false,
        "the default isn't a step to go back from"
    );
    let frame = next(&mut socket, "frame").await;
    assert_eq!(rows(&frame, "kpi_revenue"), json!([["15.00"]]));

    // cleared, it stays cleared when the dashboard is opened again
    say(
        &mut socket,
        json!({"type": "clear", "field": "orders.ordered_at"}),
    )
    .await;
    let selection = next(&mut socket, "selection").await;
    assert_eq!(selection["fields"], json!([]));
    for id in ["sales", "sales_recent"] {
        say(&mut socket, json!({"type": "open_dashboard", "id": id})).await;
        let _ = next(&mut socket, "frame").await;
    }
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
}

// --- tree filters (plan §5.6) ------------------------------------------------------

/// The retail server (`model`) with a category tree and two products in
/// it, and a tent order beside the kayak ones:
///
/// ```text
/// navigation ─ maps
/// outdoor ─┬─ camping ─ tents (p2: o3, 7.00)
///          └─ water ─┬─ kayaks (p1: o1, o2, 15.00)
///                    └─ paddles
/// ```
async fn categories(model: Arc<tidepool_model::ir::Model>) -> (Arc<AppState>, Socket) {
    let (state, url) = server_for(state(model, tidepool_server::Tokens::default(), None)).await;
    let node = |category: &str, parent: Option<&str>, level: &str| json!({"category": category, "parent": parent, "level": level});
    post(
        &state,
        "/v1/tables/categories/rows?wait=visible",
        &json!([
            node("outdoor", None, "department"),
            node("water", Some("outdoor"), "aisle"),
            node("camping", Some("outdoor"), "aisle"),
            node("kayaks", Some("water"), "category"),
            node("paddles", Some("water"), "category"),
            node("tents", Some("camping"), "category"),
            node("navigation", None, "department"),
            node("maps", Some("navigation"), "category"),
        ]),
    )
    .await;
    post(
        &state,
        "/v1/tables/products/rows?wait=visible",
        &json!([
            {"product_id": "p1", "name": "Kayak", "category": "kayaks", "list_price": "900.00"},
            {"product_id": "p2", "name": "Tent", "category": "tents", "list_price": "300.00"},
        ]),
    )
    .await;
    let mut tent = order("o3", "c2", "paid", "7.00");
    tent["product_id"] = json!("p2");
    post(
        &state,
        "/v1/tables/orders/rows?wait=visible",
        &json!([tent]),
    )
    .await;
    let mut socket = connect(&url).await;
    say(&mut socket, json!({"type": "hello"})).await;
    say(
        &mut socket,
        json!({"type": "open_dashboard", "id": "sales"}),
    )
    .await;
    (state, socket)
}

/// The tree filter in a frame: (key, depth, state, flags) per visible
/// node, flags being `o`pen, `i`nside a selection, `h`olds one.
fn tree(frame: &Value) -> Vec<String> {
    let Some(filter) = frame["filters"]
        .as_array()
        .and_then(|f| f.iter().find(|f| f["field"] == "categories.tree"))
    else {
        panic!("no tree in {frame}");
    };
    filter["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|n| {
            let flag = |name: &str, c: char| if n[name] == true { c } else { '.' };
            format!(
                "{}{} {} {}{}{} {}",
                "  ".repeat(usize::try_from(n["depth"].as_u64().unwrap_or(0)).unwrap_or(0)),
                n["key"].as_str().unwrap_or("?"),
                n["state"].as_str().unwrap_or("?"),
                flag("open", 'o'),
                flag("inside", 'i'),
                flag("holds", 'h'),
                n["rows"],
            )
        })
        .collect()
}

/// The next frame that has the tree in it.
async fn next_tree(socket: &mut Socket) -> Value {
    loop {
        let frame = next(socket, "frame").await;
        if frame["filters"]
            .as_array()
            .is_some_and(|f| f.iter().any(|f| f["field"] == "categories.tree"))
        {
            return frame;
        }
    }
}

#[tokio::test]
async fn a_tree_filter_opens_selects_subtrees_and_follows_moves() {
    let (state, mut socket) = categories(retail()).await;
    let frame = next_tree(&mut socket).await;
    // roots only, by label; rows count the nodes below that pass
    assert_eq!(
        tree(&frame),
        ["navigation possible ... 2", "outdoor possible ... 6"]
    );

    say(
        &mut socket,
        json!({"type": "filter_open", "field": "categories.tree", "keys": ["outdoor", "water"]}),
    )
    .await;
    let frame = next_tree(&mut socket).await;
    assert_eq!(
        tree(&frame),
        [
            "navigation possible ... 2",
            "outdoor possible o.. 6",
            "  camping possible ... 2",
            "  water possible o.. 3",
            "    kayaks possible ... 1",
            "    paddles possible ... 1",
        ]
    );

    // selecting a node selects everything below it
    say(
        &mut socket,
        json!({"type": "select", "field": "categories.tree", "values": ["water"]}),
    )
    .await;
    let selection = next(&mut socket, "selection").await;
    assert_eq!(
        selection["fields"],
        json!([{"field": "categories.tree", "values": ["water"]}])
    );
    let frame = next_tree(&mut socket).await;
    assert_eq!(
        rows(&frame, "by_region"),
        json!([["north", "10.00"], ["south", "5.00"]])
    );
    assert_eq!(
        tree(&frame),
        [
            "navigation possible ... 2",
            "outdoor possible o.h 6",
            "  camping possible ... 2",
            "  water selected o.. 3",
            "    kayaks possible .i. 1",
            "    paddles possible .i. 1",
        ]
    );
    let chosen = frame["filters"]
        .as_array()
        .and_then(|f| f.iter().find(|f| f["field"] == "categories.tree"))
        .map(|f| f["chosen"].clone());
    assert_eq!(
        chosen,
        Some(json!([{"key": "water", "path": ["outdoor", "water"]}]))
    );

    // kayaks move to camping: the same selection, a new answer
    post(
        &state,
        "/v1/tables/categories/rows",
        &json!([{"category": "kayaks", "parent": "camping", "level": "category"}]),
    )
    .await;
    let frame = next_tree(&mut socket).await;
    assert_eq!(rows(&frame, "by_region"), json!([]));
    assert_eq!(
        tree(&frame),
        [
            "navigation possible ... 2",
            "outdoor possible o.h 6",
            "  camping possible ... 3",
            "  water selected o.. 2",
            "    paddles possible .i. 1",
        ]
    );

    // a search shows the matches and what's above them, open
    say(
        &mut socket,
        json!({"type": "filter_window", "field": "categories.tree", "search": "AYA"}),
    )
    .await;
    let frame = next_tree(&mut socket).await;
    assert_eq!(
        tree(&frame),
        [
            "outdoor possible o.h 6",
            "  camping possible o.. 3",
            "    kayaks possible o.. 1",
        ]
    );
}

#[tokio::test]
async fn a_tree_rolls_up_what_other_selections_leave() {
    let model = retail_with(
        "model.ossie.yaml",
        r#"{"filter_direction": "one_to_many"}"#,
        r#"{"filter_direction": "both"}"#,
    );
    let (_state, mut socket) = categories(model).await;
    next_tree(&mut socket).await;
    say(
        &mut socket,
        json!({"type": "filter_open", "field": "categories.tree",
               "keys": ["outdoor", "water", "camping", "navigation"]}),
    )
    .await;
    next_tree(&mut socket).await;
    // the tent: only its branch of the tree is left, possible because
    // something below passes (rows count the passing nodes below)
    say(
        &mut socket,
        json!({"type": "select", "field": "products.name", "values": ["Tent"]}),
    )
    .await;
    let frame = next_tree(&mut socket).await;
    assert_eq!(
        tree(&frame),
        [
            "navigation excluded o.. 0",
            "  maps excluded ... 0",
            "outdoor possible o.. 1",
            "  camping possible o.. 1",
            "    tents possible ... 1",
            "  water excluded o.. 0",
            "    kayaks excluded ... 0",
            "    paddles excluded ... 0",
        ]
    );
}

#[tokio::test]
async fn a_tree_measure_is_computed_per_level_not_added_up() {
    // the average order value, a ratio: outdoor's is over all three orders
    // (22.00 / 3), not the mean of its aisles' (7.50 and 7.00)
    let model = retail_with(
        "dashboards/sales.yaml",
        "  - field: categories.tree               # a hierarchy: shown as a tree\n",
        "  - field: categories.tree\n    measure: avg_order_value\n",
    );
    let (_state, mut socket) = categories(model).await;
    next_tree(&mut socket).await;
    say(
        &mut socket,
        json!({"type": "filter_open", "field": "categories.tree", "keys": ["outdoor"]}),
    )
    .await;
    let frame = next_tree(&mut socket).await;
    let Some(nodes) = frame["filters"]
        .as_array()
        .and_then(|f| f.iter().find(|f| f["field"] == "categories.tree"))
        .and_then(|f| f["nodes"].as_array())
    else {
        panic!("no tree in {frame}");
    };
    let measures: Vec<(String, Value)> = nodes
        .iter()
        .map(|n| {
            (
                n["key"].as_str().unwrap_or("?").to_string(),
                n["measure"].clone(),
            )
        })
        .collect();
    let third = 22.0 / 3.0;
    assert_eq!(
        measures,
        [
            ("navigation".to_string(), Value::Null),
            ("outdoor".to_string(), json!(third)),
            ("camping".to_string(), json!(7.0)),
            ("water".to_string(), json!(7.5)),
        ]
    );
}

#[tokio::test]
async fn a_measured_list_measures_its_null_value_too() {
    // no customer has a segment: the list's one value is null, and the
    // revenue measured for it is all of it
    let model = retail_with(
        "dashboards/sales.yaml",
        "  - field: products.category\n",
        "  - field: products.category\n  - field: customers.segment\n    measure: revenue\n",
    );
    let (_state, url) = server_for(state(model, tidepool_server::Tokens::default(), None)).await;
    let mut socket = connect(&url).await;
    say(&mut socket, json!({"type": "hello"})).await;
    say(
        &mut socket,
        json!({"type": "open_dashboard", "id": "sales"}),
    )
    .await;
    let frame = next(&mut socket, "frame").await;
    assert_eq!(
        filter_values(&frame, "customers.segment"),
        Some(json!([{"value": null, "state": "possible", "rows": 2, "measure": "15.00"}]))
    );
}

#[tokio::test]
async fn a_filter_visual_is_a_list_and_a_table_without_metrics_shows_what_there_is() {
    let model = retail_with(
        "dashboards/sales.yaml",
        "  - id: top_products
    type: table
    dimensions: [products.name, products.category]
    metrics: [revenue, order_count, avg_order_value]
    sort: { by: revenue, desc: true }
    limit: 50
    at: { x: 8, y: 0, w: 4, h: 4 }",
        "  - id: pick_status
    type: filter
    dimensions: [orders.status]
    at: { x: 8, y: 0, w: 4, h: 2 }
  - id: who_buys_what
    type: table
    dimensions: [customers.region, products.category]
    at: { x: 8, y: 2, w: 4, h: 2 }",
    );
    let (state, url) = server_for(state(model, tidepool_server::Tokens::default(), None)).await;
    // a customer without orders: a region no order has
    post(
        &state,
        "/v1/tables/customers/rows?wait=visible",
        &json!([{"customer_id": "c3", "name": "Cy", "region": "east"}]),
    )
    .await;
    let mut socket = connect(&url).await;
    say(&mut socket, json!({"type": "hello"})).await;
    let model = next(&mut socket, "model").await;
    let visuals = &model["dashboards"][0]["visuals"];
    let Some(filter) = visuals
        .as_array()
        .and_then(|v| v.iter().find(|v| v["id"] == "pick_status"))
    else {
        panic!("no filter visual in {model}");
    };
    assert_eq!(filter["type"], "filter");
    assert_eq!(filter["widget"], "list");

    say(
        &mut socket,
        json!({"type": "open_dashboard", "id": "sales"}),
    )
    .await;
    let frame = next(&mut socket, "frame").await;
    // the filter visual is shown from its field's list, not a query
    assert_eq!(
        filter_values(&frame, "orders.status"),
        Some(json!([{"value": "paid", "state": "possible", "rows": 2}]))
    );
    assert!(
        frame["visuals"]
            .as_array()
            .is_some_and(|v| v.iter().all(|v| v["id"] != "pick_status")),
        "a filter visual has no query frame: {frame}"
    );
    // combinations orders have: the east region has no orders, so no row
    assert_eq!(
        rows(&frame, "who_buys_what"),
        json!([["north", "boats"], ["south", "boats"]])
    );

    // and they follow the selection
    say(
        &mut socket,
        json!({"type": "select", "field": "orders.status", "values": ["paid"]}),
    )
    .await;
    say(
        &mut socket,
        json!({"type": "select", "field": "customers.region", "values": ["south"]}),
    )
    .await;
    // frames carry only what changed: wait for the one with the table
    loop {
        let frame = next(&mut socket, "frame").await;
        let shown = frame["visuals"]
            .as_array()
            .and_then(|v| v.iter().find(|v| v["id"] == "who_buys_what"))
            .map(|v| v["result"]["rows"].clone());
        if shown == Some(json!([["south", "boats"]])) {
            break;
        }
    }
}

/// The type of every message up to the next `done`, skipping status ticks.
async fn until_done(socket: &mut Socket) -> Vec<String> {
    let mut kinds = Vec::new();
    loop {
        let message = match tokio::time::timeout(Duration::from_secs(5), socket.next()).await {
            Ok(Some(Ok(message))) => message,
            other => panic!("waiting for `done` after {kinds:?}: {other:?}"),
        };
        let Message::Text(text) = message else {
            continue;
        };
        let value: Value = match serde_json::from_str(text.as_str()) {
            Ok(value) => value,
            Err(err) => panic!("unreadable message {text}: {err}"),
        };
        match value["type"].as_str() {
            Some("done") => return kinds,
            Some("status") => {}
            Some(kind) => kinds.push(kind.to_string()),
            None => panic!("a message without a type: {value}"),
        }
    }
}

#[tokio::test]
async fn every_message_is_done_after_what_it_caused() {
    let (_state, url) = server().await;
    let mut socket = connect(&url).await;
    say(&mut socket, json!({"type": "hello"})).await;
    assert_eq!(
        until_done(&mut socket).await,
        ["welcome", "model", "selection"]
    );
    say(
        &mut socket,
        json!({"type": "open_dashboard", "id": "sales"}),
    )
    .await;
    assert_eq!(until_done(&mut socket).await, ["frame"]);
    let north = json!({"type": "select", "field": "customers.region", "values": ["north"]});
    say(&mut socket, north.clone()).await;
    assert_eq!(until_done(&mut socket).await, ["selection", "frame"]);
    // the same selection again changes nothing, and is still answered
    say(&mut socket, north).await;
    assert_eq!(until_done(&mut socket).await, Vec::<String>::new());
    say(&mut socket, json!({"type": "back"})).await;
    say(&mut socket, json!({"type": "forward"})).await;
    assert_eq!(until_done(&mut socket).await, ["selection", "frame"]);
    assert_eq!(until_done(&mut socket).await, ["selection", "frame"]);
    // so are messages the server can't act on
    say(&mut socket, json!({"type": "open_dashboard", "id": "nope"})).await;
    assert_eq!(until_done(&mut socket).await, ["error"]);
    if let Err(err) = socket.send(Message::text("not json")).await {
        panic!("sending: {err}");
    }
    assert_eq!(until_done(&mut socket).await, ["error"]);
}

#[tokio::test]
async fn a_visual_says_how_its_metrics_read() {
    // revenue reads in euros everywhere; the sales KPI says compact instead
    let model = retail_with(
        "model.ossie.yaml",
        "expression: \"SUM(orders.amount)\" }] }\n  - name: paid_revenue",
        "expression: \"SUM(orders.amount)\" }] }
    custom_extensions:
      - vendor_name: TIDEPOOL
        data: '{\"format\": \"currency:EUR\"}'
  - name: paid_revenue",
    );
    let (_state, url) = server_for(state(model, tidepool_server::Tokens::default(), None)).await;
    let mut socket = connect(&url).await;
    say(&mut socket, json!({"type": "hello"})).await;
    let model = next(&mut socket, "model").await;
    let Some(sales) = model["dashboards"]
        .as_array()
        .and_then(|d| d.iter().find(|d| d["id"] == "sales"))
    else {
        panic!("no sales dashboard in {model}");
    };
    let formats = |id: &str| match sales["visuals"]
        .as_array()
        .and_then(|v| v.iter().find(|v| v["id"] == id))
    {
        Some(visual) => visual["formats"].clone(),
        None => panic!("no visual {id} in {sales}"),
    };
    // the visual's own format wins over the metric's
    assert_eq!(formats("kpi_revenue"), json!({"revenue": "compact"}));
    // elsewhere the metric's own; metrics without one aren't listed
    assert_eq!(formats("top_products"), json!({"revenue": "currency:EUR"}));
    // a filter's measure reads by the metric's format
    let Some(region) = sales["filters"]
        .as_array()
        .and_then(|f| f.iter().find(|f| f["field"] == "customers.region"))
    else {
        panic!("no region filter in {sales}");
    };
    assert_eq!(region["measure_format"], "currency:EUR");
}

#[tokio::test]
async fn a_search_selects_as_it_is_typed_and_rows_follow() {
    let (state, url) = server().await;
    post(
        &state,
        "/v1/tables/orders/rows?wait=visible",
        &json!([
            {"order_id": "o3", "customer_id": "c1", "product_id": "p1", "status": "paid",
             "ordered_at": "2026-02-02T10:00:00Z", "quantity": 1, "amount": "3.00",
             "note": "Leave at the DOOR"},
            {"order_id": "o4", "customer_id": "c2", "product_id": "p1", "status": "shipped",
             "ordered_at": "2026-02-03T10:00:00Z", "quantity": 1, "amount": "4.00",
             "note": "door code 4417"},
        ]),
    )
    .await;
    let mut socket = connect(&url).await;
    say(
        &mut socket,
        json!({"type": "open_dashboard", "id": "shipping_notes"}),
    )
    .await;
    let frame = next(&mut socket, "frame").await;
    // the newest orders first, every one of them
    let ids = |frame: &Value| -> Vec<Value> {
        rows(frame, "latest")
            .as_array()
            .map(|r| r.iter().map(|row| row[4].clone()).collect())
            .unwrap_or_default()
    };
    assert_eq!(
        ids(&frame),
        vec![
            json!("door code 4417"),
            json!("Leave at the DOOR"),
            json!(null),
            json!(null)
        ]
    );
    assert_eq!(rows(&frame, "kpi_orders"), json!([[4]]));

    // typed a key at a time: each is the selection, but one step back;
    // a frame only when the answer changes (these all find the same rows)
    for text in ["d", "do", "door"] {
        say(
            &mut socket,
            json!({"type": "search", "field": "orders.note", "text": text}),
        )
        .await;
        let selection = next(&mut socket, "selection").await;
        assert_eq!(
            selection["fields"],
            json!([{"field": "orders.note", "contains": text}])
        );
        let _ = next(&mut socket, "done").await;
    }
    say(
        &mut socket,
        json!({"type": "search", "field": "orders.note", "text": "CODE"}),
    )
    .await;
    let _ = next(&mut socket, "selection").await;
    let frame = next(&mut socket, "frame").await;
    assert_eq!(ids(&frame), vec![json!("door code 4417")]);
    assert_eq!(rows(&frame, "kpi_orders"), json!([[1]]));
    assert_eq!(rows(&frame, "by_region"), json!([["south", 1]]));
    let _ = next(&mut socket, "done").await;

    // a click elsewhere narrows the search's rows too
    say(
        &mut socket,
        json!({"type": "search", "field": "orders.note", "text": "door"}),
    )
    .await;
    let _ = next(&mut socket, "done").await;
    say(
        &mut socket,
        json!({"type": "select", "field": "customers.region", "values": ["north"]}),
    )
    .await;
    let _ = next(&mut socket, "selection").await;
    let frame = next(&mut socket, "frame").await;
    assert_eq!(ids(&frame), vec![json!("Leave at the DOOR")]);

    // back: the region goes, then the whole search at once
    say(&mut socket, json!({"type": "back"})).await;
    let selection = next(&mut socket, "selection").await;
    assert_eq!(
        selection["fields"],
        json!([{"field": "orders.note", "contains": "door"}])
    );
    say(&mut socket, json!({"type": "back"})).await;
    let selection = next(&mut socket, "selection").await;
    assert_eq!(selection["fields"], json!([]));
    assert_eq!(selection["can_back"], false);

    // an empty search clears it; a number can't be searched
    say(
        &mut socket,
        json!({"type": "search", "field": "orders.quantity", "text": "1"}),
    )
    .await;
    let error = next(&mut socket, "error").await;
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|m| m.contains("can't be searched")),
        "{error}"
    );
}

#[tokio::test]
async fn a_rows_visual_shows_new_rows_as_they_arrive() {
    let (state, url) = server().await;
    let mut socket = connect(&url).await;
    say(
        &mut socket,
        json!({"type": "open_dashboard", "id": "shipping_notes"}),
    )
    .await;
    let frame = next(&mut socket, "frame").await;
    assert_eq!(
        rows(&frame, "latest").as_array().map(Vec::len),
        Some(2),
        "{frame}"
    );
    post(
        &state,
        "/v1/tables/orders/rows",
        &json!([{"order_id": "o9", "customer_id": "c2", "product_id": "p1", "status": "paid",
                 "ordered_at": "2026-02-09T10:00:00Z", "quantity": 2, "amount": "9.00",
                 "note": "newest"}]),
    )
    .await;
    let frame = next(&mut socket, "frame").await;
    let latest = rows(&frame, "latest");
    assert_eq!(latest[0][4], "newest", "{frame}");
    assert_eq!(latest[0][1], "Bo");
    assert_eq!(latest.as_array().map(Vec::len), Some(3));
}

/// `POST /v1/views`: the status and the answer.
async fn make_view(state: &Arc<AppState>, body: &Value) -> (StatusCode, Value) {
    let reply = send(
        Arc::clone(state),
        request(
            Method::POST,
            "/v1/views",
            &[("content-type", "application/json")],
            body.to_string(),
        ),
    )
    .await;
    (reply.status, reply.json())
}

async fn get(state: &Arc<AppState>, path: &str) -> (StatusCode, Value) {
    let reply = send(Arc::clone(state), request(Method::GET, path, &[], "")).await;
    (reply.status, reply.json())
}

fn revenue_by_region() -> Value {
    json!({"name": "Revenue by region", "visuals": [
        {"id": "regions", "type": "bar", "dimensions": ["customers.region"],
         "metrics": ["revenue"], "on_click": "select"}
    ]})
}

#[tokio::test]
async fn a_view_is_a_live_dashboard_that_is_never_written() {
    let (state, url) = server_for(common::project_state().await).await;
    let (_, before) = get(&state, "/v1/config").await;
    let (status, view) = make_view(&state, &revenue_by_region()).await;
    assert_eq!(status, StatusCode::OK, "{view}");
    let Some(id) = view["id"].as_str().map(str::to_string) else {
        panic!("a view has an id: {view}")
    };
    assert!(id.starts_with('~'), "{id}");
    // placed for us, and its YAML ready to save
    assert_eq!(view["dashboard"]["visuals"][0]["at"]["w"], 12);
    assert!(
        view["yaml"]
            .as_str()
            .is_some_and(|y| y.contains("customers.region")),
        "{view}"
    );

    let mut socket = connect(&url).await;
    say(&mut socket, json!({"type": "open_dashboard", "id": id})).await;
    let shown = next(&mut socket, "view").await;
    assert_eq!(shown["dashboard"]["id"], id.as_str());
    assert_eq!(shown["dashboard"]["name"], "Revenue by region");
    let frame = next(&mut socket, "frame").await;
    assert_eq!(
        rows(&frame, "regions"),
        json!([["north", "10.00"], ["south", "5.00"]])
    );

    // live: new data arrives as a frame
    post(
        &state,
        "/v1/tables/orders/rows",
        &json!([order("o3", "c2", "paid", "7.50")]),
    )
    .await;
    let frame = next(&mut socket, "frame").await;
    assert_eq!(
        rows(&frame, "regions"),
        json!([["north", "10.00"], ["south", "12.50"]])
    );
    // and selections work as on any dashboard, with a sort of its own
    say(
        &mut socket,
        json!({"type": "sort", "visual": "regions", "column": "revenue", "desc": true}),
    )
    .await;
    let frame = next(&mut socket, "frame").await;
    assert_eq!(
        rows(&frame, "regions"),
        json!([["south", "12.50"], ["north", "10.00"]])
    );
    say(
        &mut socket,
        json!({"type": "select", "field": "customers.region", "values": ["north"]}),
    )
    .await;
    let selection = next(&mut socket, "selection").await;
    assert_eq!(selection["fields"][0]["field"], "customers.region");

    // never in the config: same files, same version, no history
    let (_, after) = get(&state, "/v1/config").await;
    assert_eq!(after["version"], before["version"]);
    assert_eq!(after["files"], before["files"]);
    let (_, versions) = get(&state, "/v1/config/versions").await;
    assert_eq!(versions.as_array().map(Vec::len), Some(1), "{versions}");
    // and it reads back
    let (status, again) = get(&state, &format!("/v1/views/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again["dashboard"], view["dashboard"]);
}

#[tokio::test]
async fn a_view_that_doesnt_compile_says_why_and_where() {
    let state = common::project_state().await;
    let (status, problems) = make_view(
        &state,
        &json!({"name": "v", "visuals": [
            {"id": "a", "type": "bar", "dimensions": ["customers.region"], "metrics": ["revnue"]}
        ]}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{problems}");
    let text = problems["problems_text"].as_str().unwrap_or_default();
    assert!(
        text.contains("revnue") && text.contains("revenue"),
        "{text}"
    );
    // a bar with two dimensions breaks the same rule a file would
    let (status, problems) = make_view(
        &state,
        &json!({"name": "v", "visuals": [
            {"id": "a", "type": "bar", "dimensions": ["customers.region", "products.category"], "metrics": ["revenue"]}
        ]}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{problems}");
    assert!(
        problems["problems_text"]
            .as_str()
            .is_some_and(|t| t.contains("exactly one dimension")),
        "{problems}"
    );
    // not a dashboard at all
    let (status, _) = make_view(&state, &json!({"visuals": 3})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // and a server with no config files can't make views
    let (status, refused) = make_view(&ready_state(), &revenue_by_region()).await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
}

#[tokio::test]
async fn a_view_follows_the_model_and_says_when_it_no_longer_fits() {
    let (state, url) = server_for(common::project_state().await).await;
    let (_, view) = make_view(
        &state,
        &json!({"name": "Customers", "visuals": [
            {"id": "buyers", "type": "kpi", "metrics": ["customer_count"]}
        ]}),
    )
    .await;
    let id = view["id"].as_str().unwrap_or_default().to_string();
    let mut socket = connect(&url).await;
    say(&mut socket, json!({"type": "open_dashboard", "id": id})).await;
    let _ = next(&mut socket, "view").await;
    let _ = next(&mut socket, "frame").await;

    // the metric it shows is removed: the view says so, once
    let (_, config) = get(&state, "/v1/config").await;
    let Some(model) = config["files"]
        .as_array()
        .and_then(|f| f.iter().find(|f| f["path"] == "model.ossie.yaml"))
        .and_then(|f| f["text"].as_str())
    else {
        panic!("no model file in {config}")
    };
    let start = model.find("  - name: customer_count").unwrap_or(0);
    let end = model[start..]
        .find("  - name: avg_order_value")
        .map_or(start, |e| start + e);
    assert!(end > start, "the example's metrics moved");
    let edited = format!("{}{}", &model[..start], &model[end..]);
    let reply = send(
        Arc::clone(&state),
        request(
            Method::POST,
            "/v1/config/apply",
            &[("content-type", "application/json")],
            json!({"base_version": config["version"], "files": {"model.ossie.yaml": edited}})
                .to_string(),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let error = next(&mut socket, "error").await;
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|m| m.contains("no longer fits") && m.contains("customer_count")),
        "{error}"
    );
    let (status, _) = get(&state, &format!("/v1/views/{id}")).await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn a_view_nobody_looks_at_for_a_day_is_forgotten() {
    let state = common::project_state().await;
    state.set_clock(Some(1_000_000));
    let (_, view) = make_view(&state, &revenue_by_region()).await;
    let path = format!("/v1/views/{}", view["id"].as_str().unwrap_or_default());
    state.set_clock(Some(1_000_000 + 23 * 3_600_000_000));
    assert_eq!(get(&state, &path).await.0, StatusCode::OK);
    // looking at it kept it: a day from then, not from when it was made
    state.set_clock(Some(1_000_000 + 46 * 3_600_000_000));
    assert_eq!(get(&state, &path).await.0, StatusCode::OK);
    state.set_clock(Some(1_000_000 + 71 * 3_600_000_000));
    assert_eq!(get(&state, &path).await.0, StatusCode::NOT_FOUND);
    // and a view that never was is no dashboard either
    let (_, url) = server_for(state).await;
    let mut socket = connect(&url).await;
    say(
        &mut socket,
        json!({"type": "open_dashboard", "id": "~nope"}),
    )
    .await;
    let error = next(&mut socket, "error").await;
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|m| m.contains("~nope")),
        "{error}"
    );
}
