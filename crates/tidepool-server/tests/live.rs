//! Live dashboards over a real WebSocket: the retail dashboard opened,
//! selections made and undone, and data arriving while it's open.

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::http::{Method, StatusCode};
use common::{ready_state, request, retail, retail_edited, retail_with, send, state};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tidepool_server::{AppState, Tokens};
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
    let url = listen(&state).await;
    (state, url)
}

/// Serve a state on a free port, whatever it holds: the address of its socket.
async fn listen(state: &Arc<AppState>) -> String {
    let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
        Ok(listener) => listener,
        Err(err) => panic!("binding: {err}"),
    };
    let address = match listener.local_addr() {
        Ok(address) => address,
        Err(err) => panic!("the bound address: {err}"),
    };
    let app = tidepool_server::router(Arc::clone(state));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("ws://{address}/v1/ws")
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

/// How long a test waits for a message. A deadline over the whole wait: status
/// ticks arrive every second, so a timeout per message would never fire.
const PATIENCE: Duration = Duration::from_secs(10);

/// The next message of type `kind`, skipping others (status ticks).
async fn next(socket: &mut Socket, kind: &str) -> Value {
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        let message = match tokio::time::timeout_at(deadline, socket.next()).await {
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
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        let message = match tokio::time::timeout_at(deadline, socket.next()).await {
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

/// Orders at these times, each worth 1.00.
async fn orders_at(state: &Arc<AppState>, times: &[&str]) {
    let rows: Vec<Value> = times
        .iter()
        .enumerate()
        .map(|(i, time)| {
            json!({
                "order_id": format!("t{i}"), "customer_id": "c1", "product_id": "p1",
                "status": "paid", "ordered_at": time, "quantity": 1, "amount": "1.00",
            })
        })
        .collect();
    post(
        state,
        "/v1/tables/orders/rows?wait=visible",
        &Value::Array(rows),
    )
    .await;
}

/// The next frame that has visual `id` in it.
async fn frame_of(socket: &mut Socket, id: &str) -> Value {
    let wait = async {
        loop {
            let frame = next(socket, "frame").await;
            if frame["visuals"]
                .as_array()
                .is_some_and(|v| v.iter().any(|v| v["id"] == id))
            {
                return frame;
            }
        }
    };
    match tokio::time::timeout(PATIENCE, wait).await {
        Ok(frame) => frame,
        Err(err) => panic!("no frame with visual {id}: {err}"),
    }
}

/// What a frame says about visual `id`'s time axis.
fn zoom(frame: &Value, id: &str) -> Value {
    visual(frame, id)["zoom"].clone()
}

#[tokio::test]
async fn an_auto_grain_refines_with_a_brush_and_back_coarsens_it() {
    let (state, url) = server().await;
    // four months of orders (with the two from 1 February)
    orders_at(&state, &["2026-06-01T12:00:00Z"]).await;
    state.set_clock(Some(at("2026-11-01T00:00:00Z")));
    let mut socket = connect(&url).await;
    say(
        &mut socket,
        json!({"type": "open_dashboard", "id": "sales"}),
    )
    .await;
    let opened = frame_of(&mut socket, "trend").await;
    let axis = zoom(&opened, "trend");
    assert_eq!(axis["grain"], "day", "2,900 hours are too many: {axis}");
    assert_eq!(axis["auto"], true);
    assert_eq!(axis["open"], json!(["day", "week", "month"]));
    assert_eq!(axis["from"], "2026-02-01");
    assert_eq!(axis["to"], "2026-06-02", "as far as the values go");
    assert_eq!(
        visual(&opened, "trend")["result"]["columns"][0]["name"],
        "orders.ordered_at:day"
    );
    assert_eq!(visual(&opened, "by_region")["zoom"], Value::Null);

    // the complaint: eleven hours shouldn't be two points of a daily trend
    say(
        &mut socket,
        json!({"type": "select_range", "field": "orders.ordered_at",
               "from": "2026-02-01T05:00:00Z", "to": "2026-02-01T16:00:00Z"}),
    )
    .await;
    let brushed = frame_of(&mut socket, "trend").await;
    let axis = zoom(&brushed, "trend");
    assert_eq!(axis["grain"], "hour", "{axis}");
    assert_eq!(axis["auto"], true);
    assert_eq!(axis["open"], json!(["hour"]), "660 minutes are too many");
    assert_eq!(axis["from"], "2026-02-01T05:00:00Z");
    assert_eq!(axis["to"], "2026-02-01T16:00:00Z");
    assert_eq!(
        rows(&brushed, "trend"),
        json!([["2026-02-01T10:00:00Z", "15.00", 2]])
    );

    // back: the whole range, and days again
    say(&mut socket, json!({"type": "back"})).await;
    let back = frame_of(&mut socket, "trend").await;
    assert_eq!(zoom(&back, "trend")["grain"], "day");
    assert_eq!(
        rows(&back, "trend"),
        json!([["2026-02-01", "15.00", 2], ["2026-06-01", "1.00", 1]])
    );

    // an open end reaches to where the values do
    say(
        &mut socket,
        json!({"type": "select_range", "field": "orders.ordered_at",
               "from": "2026-05-31T00:00:00Z"}),
    )
    .await;
    let open_ended = frame_of(&mut socket, "trend").await;
    assert_eq!(zoom(&open_ended, "trend")["grain"], "hour");
    assert_eq!(zoom(&open_ended, "trend")["from"], "2026-05-31T00:00:00Z");
    assert_eq!(zoom(&open_ended, "trend")["to"], "2026-06-01T13:00:00Z");
}

#[tokio::test]
async fn a_window_resolves_with_the_clock_and_stops_at_now() {
    let (state, url) = server().await;
    state.set_clock(Some(at("2026-02-03T14:30:00Z")));
    let mut socket = connect(&url).await;
    say(
        &mut socket,
        json!({"type": "open_dashboard", "id": "sales_recent"}),
    )
    .await;
    // the dashboard opens on the last seven days: hours, up to this one
    let opened = frame_of(&mut socket, "over_time").await;
    let axis = zoom(&opened, "over_time");
    assert_eq!(axis["grain"], "hour", "{axis}");
    assert_eq!(axis["from"], "2026-01-28T00:00:00Z");
    assert_eq!(axis["to"], "2026-02-03T15:00:00Z");
    assert_eq!(axis["current"], "2026-02-03T14:00:00Z");
    assert!(
        axis["open"]
            .as_array()
            .is_some_and(|o| o.contains(&json!("hour")) && !o.contains(&json!("minute"))),
        "{axis}"
    );

    // a window of hours: minutes
    say(
        &mut socket,
        json!({"type": "select_relative", "field": "orders.ordered_at",
               "window": {"unit": "hour", "last": 2}}),
    )
    .await;
    let frame = frame_of(&mut socket, "over_time").await;
    assert_eq!(zoom(&frame, "over_time")["grain"], "minute");

    // no selection: the extent is one instant, and nothing is open
    say(&mut socket, json!({"type": "clear"})).await;
    let frame = frame_of(&mut socket, "over_time").await;
    let axis = zoom(&frame, "over_time");
    assert_eq!(axis["grain"], "minute", "{axis}");
    assert_eq!(axis["open"], json!([]));
}

#[tokio::test]
async fn the_bucket_that_holds_now_moves_without_new_data() {
    let (state, url) = server().await;
    state.set_clock(Some(at("2026-02-03T10:00:00Z")));
    orders_at(&state, &["2026-02-02T10:00:00Z"]).await;
    let mut socket = connect(&url).await;
    say(
        &mut socket,
        json!({"type": "open_dashboard", "id": "sales"}),
    )
    .await;
    let frame = frame_of(&mut socket, "trend").await;
    assert_eq!(zoom(&frame, "trend")["current"], "2026-02-03T10:00:00Z");
    state.set_clock(Some(at("2026-02-03T11:05:00Z")));
    let frame = frame_of(&mut socket, "trend").await;
    assert_eq!(zoom(&frame, "trend")["current"], "2026-02-03T11:00:00Z");
}

#[tokio::test]
async fn a_pin_is_this_visuals_alone_and_no_step_in_history() {
    let (state, url) = server().await;
    orders_at(&state, &["2026-06-01T12:00:00Z"]).await;
    let mut socket = connect(&url).await;
    say(
        &mut socket,
        json!({"type": "open_dashboard", "id": "sales"}),
    )
    .await;
    assert!(until_done(&mut socket).await.ends_with(&["frame".into()]));

    // only the pinned visual changes, and no selection message says so
    say(
        &mut socket,
        json!({"type": "grain", "visual": "trend", "grain": "month"}),
    )
    .await;
    let frame = frame_of(&mut socket, "trend").await;
    assert_eq!(
        frame["visuals"].as_array().map(Vec::len),
        Some(1),
        "the other visuals are as they were: {frame}"
    );
    assert_eq!(frame["filters"], json!([]));
    let axis = zoom(&frame, "trend");
    assert_eq!(axis["grain"], "month");
    assert_eq!(axis["auto"], false);
    assert_eq!(axis["pinned"], "month");
    assert_eq!(axis["from"], "2026-02-01");
    assert_eq!(axis["to"], "2026-07-01");
    assert_eq!(
        visual(&frame, "trend")["result"]["columns"][0]["name"],
        "orders.ordered_at:month"
    );
    assert_eq!(until_done(&mut socket).await, Vec::<String>::new());

    // nothing to go back to: a grain isn't a selection
    say(&mut socket, json!({"type": "back"})).await;
    assert_eq!(until_done(&mut socket).await, Vec::<String>::new());

    // a narrower range leaves the pin where it was, as long as it's open
    say(
        &mut socket,
        json!({"type": "select_range", "field": "orders.ordered_at",
               "from": "2026-02-01T00:00:00Z", "to": "2026-04-01T00:00:00Z"}),
    )
    .await;
    let frame = frame_of(&mut socket, "trend").await;
    assert_eq!(zoom(&frame, "trend")["grain"], "month");
    say(&mut socket, json!({"type": "back"})).await;
    let _ = frame_of(&mut socket, "trend").await;

    // no grain goes back to the dashboard's
    say(&mut socket, json!({"type": "grain", "visual": "trend"})).await;
    let frame = frame_of(&mut socket, "trend").await;
    let axis = zoom(&frame, "trend");
    assert_eq!(axis["grain"], "day");
    assert_eq!(axis["auto"], true);
    assert_eq!(axis["pinned"], Value::Null);

    // sorted by its time column, a sort follows the grain
    say(
        &mut socket,
        json!({"type": "sort", "visual": "trend", "column": "orders.ordered_at:day", "desc": true}),
    )
    .await;
    let frame = frame_of(&mut socket, "trend").await;
    assert_eq!(rows(&frame, "trend")[0][0], "2026-06-01");
    say(
        &mut socket,
        json!({"type": "grain", "visual": "trend", "grain": "month"}),
    )
    .await;
    let frame = frame_of(&mut socket, "trend").await;
    assert_eq!(
        rows(&frame, "trend")[0][0],
        "2026-06-01",
        "still newest first, and no error: {frame}"
    );
    assert_eq!(rows(&frame, "trend")[1][0], "2026-02-01");
}

#[tokio::test]
async fn a_pin_outside_the_open_steps_falls_back_and_stays_pinned() {
    let (state, url) = server().await;
    orders_at(&state, &["2026-06-01T12:00:00Z"]).await;
    let mut socket = connect(&url).await;
    say(
        &mut socket,
        json!({"type": "open_dashboard", "id": "sales"}),
    )
    .await;
    let _ = frame_of(&mut socket, "trend").await;

    // minutes over four months: too many, and hours too, so days
    say(
        &mut socket,
        json!({"type": "grain", "visual": "trend", "grain": "minute"}),
    )
    .await;
    // the answer is the same: the frame only says what's pinned
    let frame = frame_of(&mut socket, "trend").await;
    let axis = zoom(&frame, "trend");
    assert_eq!(axis["grain"], "day", "{axis}");
    assert_eq!(axis["auto"], false);
    assert_eq!(axis["pinned"], "minute");
    assert_eq!(axis["open"], json!(["day", "week", "month"]));

    // an hour's brush opens the pin's step: there it is
    say(
        &mut socket,
        json!({"type": "select_range", "field": "orders.ordered_at",
               "from": "2026-02-01T10:00:00Z", "to": "2026-02-01T11:00:00Z"}),
    )
    .await;
    let frame = frame_of(&mut socket, "trend").await;
    assert_eq!(zoom(&frame, "trend")["grain"], "minute");

    // and back it falls back again
    say(&mut socket, json!({"type": "back"})).await;
    let frame = frame_of(&mut socket, "trend").await;
    assert_eq!(zoom(&frame, "trend")["grain"], "day");
    assert_eq!(zoom(&frame, "trend")["pinned"], "minute");
}

#[tokio::test]
async fn sessions_share_a_grain_and_keep_their_pins_to_themselves() {
    let (state, url) = server().await;
    orders_at(&state, &["2026-06-01T12:00:00Z"]).await;
    let open = |id: &'static str| json!({"type": "open_dashboard", "id": id});
    let mut one = connect(&url).await;
    let mut two = connect(&url).await;
    say(&mut one, open("sales")).await;
    say(&mut two, open("sales")).await;
    let (first, second) = (
        frame_of(&mut one, "trend").await,
        frame_of(&mut two, "trend").await,
    );
    assert_eq!(zoom(&first, "trend"), zoom(&second, "trend"));
    assert_eq!(rows(&first, "trend"), rows(&second, "trend"));

    // a selection elsewhere changes neither the range nor the grain
    say(
        &mut two,
        json!({"type": "select", "field": "customers.region", "values": ["north"]}),
    )
    .await;
    let selected = frame_of(&mut two, "trend").await;
    assert_eq!(zoom(&selected, "trend"), zoom(&first, "trend"));

    // the same range is the same grain in another session
    let brush = json!({"type": "select_range", "field": "orders.ordered_at",
                       "from": "2026-02-01T05:00:00Z", "to": "2026-02-01T16:00:00Z"});
    say(&mut one, brush.clone()).await;
    let in_one = frame_of(&mut one, "trend").await;
    say(&mut two, json!({"type": "clear"})).await;
    let _ = frame_of(&mut two, "trend").await;
    say(&mut two, brush).await;
    let in_two = frame_of(&mut two, "trend").await;
    assert_eq!(zoom(&in_one, "trend"), zoom(&in_two, "trend"));
    assert_eq!(rows(&in_one, "trend"), rows(&in_two, "trend"));

    // a pin stays in the session that made it: the other goes on as it was,
    // new data and all
    say(
        &mut one,
        json!({"type": "grain", "visual": "trend", "grain": "minute"}),
    )
    .await;
    let pinned = frame_of(&mut one, "trend").await;
    assert_eq!(zoom(&pinned, "trend")["pinned"], "minute");
    orders_at(&state, &["2026-02-01T11:00:00Z"]).await;
    let other = frame_of(&mut two, "trend").await;
    assert_eq!(zoom(&other, "trend")["grain"], "hour");
    assert_eq!(zoom(&other, "trend")["pinned"], Value::Null);
}

#[tokio::test]
async fn a_fixed_grain_stays_and_zoom_false_refuses_a_pin() {
    let (state, url) = server().await;
    orders_at(&state, &["2026-06-01T12:00:00Z"]).await;
    let mut socket = connect(&url).await;
    say(&mut socket, json!({"type": "hello"})).await;
    let model = next(&mut socket, "model").await;
    let info = |dashboard: &str, id: &str| -> Value {
        model["dashboards"]
            .as_array()
            .and_then(|d| d.iter().find(|d| d["id"] == dashboard))
            .and_then(|d| d["visuals"].as_array())
            .and_then(|v| v.iter().find(|v| v["id"] == id))
            .cloned()
            .unwrap_or_else(|| panic!("no visual {id} in {dashboard}"))
    };
    assert_eq!(
        info("sales", "trend")["zoom"],
        json!(["minute", "hour", "day", "week", "month"]),
        "the dashboard's list"
    );
    assert_eq!(info("sales", "trend")["dimensions"][0]["grain"], "auto");
    assert_eq!(
        info("sales_recent", "over_time")["zoom"],
        json!(["minute", "hour", "day", "week", "month", "quarter", "year"]),
        "every grain its field has"
    );
    assert_eq!(info("sales_recent", "by_week")["zoom"], Value::Null);
    assert_eq!(info("sales", "by_region")["zoom"], Value::Null);

    say(
        &mut socket,
        json!({"type": "open_dashboard", "id": "sales_recent"}),
    )
    .await;
    let frame = frame_of(&mut socket, "by_week").await;
    let axis = zoom(&frame, "by_week");
    assert_eq!(axis["grain"], "week");
    assert_eq!(axis["auto"], false);
    assert_eq!(axis["open"], json!([]), "nothing to offer");

    for (message, wants) in [
        (
            json!({"type": "grain", "visual": "by_week", "grain": "day"}),
            "can't be zoomed",
        ),
        (
            json!({"type": "grain", "visual": "who_buys_what", "grain": "day"}),
            "can't be zoomed",
        ),
        (
            json!({"type": "grain", "visual": "nope", "grain": "day"}),
            "no visual `nope`",
        ),
        (
            json!({"type": "grain", "visual": "over_time", "grain": "fortnight"}),
            "unknown grain",
        ),
    ] {
        say(&mut socket, message).await;
        let error = next(&mut socket, "error").await;
        assert!(
            error["message"].as_str().is_some_and(|m| m.contains(wants)),
            "{error}, wanted {wants}"
        );
    }

    // a step of the field that the visual's list leaves out
    say(
        &mut socket,
        json!({"type": "open_dashboard", "id": "sales"}),
    )
    .await;
    let _ = frame_of(&mut socket, "trend").await;
    say(
        &mut socket,
        json!({"type": "grain", "visual": "trend", "grain": "year"}),
    )
    .await;
    let error = next(&mut socket, "error").await;
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|m| m.contains("doesn't zoom to `year`") && m.contains("minute, hour")),
        "{error}"
    );
}

#[tokio::test]
async fn without_a_dashboard_there_is_nothing_to_pin() {
    let (_state, url) = server().await;
    let mut socket = connect(&url).await;
    say(
        &mut socket,
        json!({"type": "grain", "visual": "trend", "grain": "day"}),
    )
    .await;
    let error = next(&mut socket, "error").await;
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|m| m.contains("no dashboard is open")),
        "{error}"
    );
}

#[tokio::test]
async fn an_empty_table_has_a_grain_and_no_axis() {
    let mut socket = connect(&listen(&ready_state()).await).await;
    say(
        &mut socket,
        json!({"type": "open_dashboard", "id": "sales"}),
    )
    .await;
    let frame = frame_of(&mut socket, "trend").await;
    let axis = zoom(&frame, "trend");
    assert_eq!(axis["grain"], "minute", "{axis}");
    assert_eq!(axis["open"], json!([]));
    assert_eq!(axis["from"], Value::Null);
    assert_eq!(axis["to"], Value::Null);
    assert_eq!(rows(&frame, "trend"), json!([]));
}

#[tokio::test]
async fn the_extent_is_of_live_values() {
    // orders with a key, so the newest can be replaced and the extent shrink;
    // read through a dictionary, or from plain values
    for encoding in ["", ", encoding: plain"] {
        let model = retail_edited(&[
            (
                "tables/orders.yaml",
                "mode: append\nevent_time: ordered_at\nretention: 730d\n",
                "mode: upsert\nprimary_key: [order_id]\n",
            ),
            (
                "tables/orders.yaml",
                "order_id:    { type: string, encoding: plain }",
                "order_id:    { type: string }",
            ),
            (
                "tables/orders.yaml",
                "ordered_at:  { type: timestamptz }",
                &format!("ordered_at:  {{ type: timestamptz{encoding} }}"),
            ),
        ]);
        let (running, url) = server_for(state(model, Tokens::default(), None)).await;
        orders_at(&running, &["2026-02-01T12:00:00Z", "2026-02-02T12:00:00Z"]).await;
        let mut socket = connect(&url).await;
        say(
            &mut socket,
            json!({"type": "open_dashboard", "id": "sales"}),
        )
        .await;
        let frame = frame_of(&mut socket, "trend").await;
        assert_eq!(zoom(&frame, "trend")["to"], "2026-02-02T13:00:00Z");

        // a row added past the end moves it
        orders_at(
            &running,
            &[
                "2026-02-01T12:00:00Z",
                "2026-02-02T12:00:00Z",
                "2026-07-01T12:00:00Z",
            ],
        )
        .await;
        let frame = frame_of(&mut socket, "trend").await;
        assert_eq!(zoom(&frame, "trend")["grain"], "day");
        assert_eq!(zoom(&frame, "trend")["to"], "2026-07-02");

        // and the same row moved back takes it with it
        orders_at(
            &running,
            &[
                "2026-02-01T12:00:00Z",
                "2026-02-02T12:00:00Z",
                "2026-02-01T13:00:00Z",
            ],
        )
        .await;
        let frame = frame_of(&mut socket, "trend").await;
        assert_eq!(zoom(&frame, "trend")["grain"], "hour", "{encoding}");
        assert_eq!(zoom(&frame, "trend")["to"], "2026-02-02T13:00:00Z");
    }
}

#[tokio::test]
async fn a_date_field_and_a_derived_field_have_extents_too() {
    // dates: days are the finest there is
    let dates = retail_edited(&[
        (
            "tables/orders.yaml",
            "ordered_at:  { type: timestamptz }",
            "ordered_at:  { type: date }",
        ),
        (
            "dashboards/sales.yaml",
            "    zoom: [minute, hour, day, week, month]   # what a viewer may zoom between\n",
            "",
        ),
    ]);
    let running = state(dates, Tokens::default(), None);
    orders_at(&running, &["2026-02-01", "2026-02-10"]).await;
    let mut socket = connect(&listen(&running).await).await;
    say(
        &mut socket,
        json!({"type": "open_dashboard", "id": "sales"}),
    )
    .await;
    let frame = frame_of(&mut socket, "trend").await;
    let axis = zoom(&frame, "trend");
    assert_eq!(axis["grain"], "day", "{axis}");
    assert_eq!(axis["from"], "2026-02-01");
    assert_eq!(axis["to"], "2026-02-11");
    assert_eq!(axis["open"], json!(["day", "week"]), "no hours");

    // a time field worked out from a column: the engine says how far it goes
    let derived = retail_edited(&[
        (
            "model.ossie.yaml",
            "      - name: status\n",
            "      - name: placed\n        expression: { dialects: [{ dialect: ANSI_SQL, expression: \"DATE_TRUNC('day', ordered_at)\" }] }\n        dimension: { is_time: true }\n      - name: status\n",
        ),
        (
            "dashboards/sales.yaml",
            "    dimensions: [{ field: orders.ordered_at, grain: auto }]",
            "    dimensions: [{ field: orders.placed, grain: auto }]",
        ),
        (
            "dashboards/sales.yaml",
            "    zoom: [minute, hour, day, week, month]   # what a viewer may zoom between\n",
            "",
        ),
    ]);
    let (running, url) = server_for(state(derived, Tokens::default(), None)).await;
    orders_at(&running, &["2026-02-10T12:00:00Z"]).await;
    let mut socket = connect(&url).await;
    say(
        &mut socket,
        json!({"type": "open_dashboard", "id": "sales"}),
    )
    .await;
    let frame = frame_of(&mut socket, "trend").await;
    let axis = zoom(&frame, "trend");
    assert_eq!(axis["grain"], "day", "{axis}");
    assert_eq!(axis["from"], "2026-02-01");
    assert_eq!(axis["to"], "2026-02-11");
}
