//! `POST /v1/query` against the retail model: names resolved the semantic
//! layer's way, values typed by their fields, errors as 400s that say what's
//! wrong.

mod common;

use std::sync::Arc;

use axum::http::{Method, StatusCode};
use common::{ready_state, request, send};
use serde_json::{Value, json};
use tidepool_server::AppState;

const JSON: (&str, &str) = ("content-type", "application/json");

async fn post(state: &Arc<AppState>, path: &str, body: &Value) -> common::Reply {
    send(
        Arc::clone(state),
        request(Method::POST, path, &[JSON], body.to_string()),
    )
    .await
}

/// Two customers in two regions, three orders.
async fn loaded() -> Arc<AppState> {
    let state = ready_state();
    let customers = json!([
        {"customer_id": "c1", "name": "Ada", "region": "north"},
        {"customer_id": "c2", "name": "Bo", "region": "south"},
    ]);
    let products =
        json!([{"product_id": "p1", "name": "Kayak", "category": "boats", "list_price": "900.00"}]);
    let order = |id: &str, customer: &str, status: &str, amount: &str, at: &str| {
        json!({
            "order_id": id, "customer_id": customer, "product_id": "p1", "status": status,
            "ordered_at": at, "quantity": 1, "amount": amount,
        })
    };
    let orders = json!([
        order("o1", "c1", "paid", "10.50", "2026-01-15T10:00:00Z"),
        order("o2", "c1", "returned", "4.00", "2026-02-01T10:00:00Z"),
        order("o3", "c2", "paid", "7.25", "2026-02-20T10:00:00Z"),
    ]);
    for (table, rows) in [
        ("customers", customers),
        ("products", products),
        ("orders", orders),
    ] {
        let reply = post(
            &state,
            &format!("/v1/tables/{table}/rows?wait=visible"),
            &rows,
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    }
    state
}

#[tokio::test]
async fn metrics_group_by_a_related_field() {
    let state = loaded().await;
    let reply = post(
        &state,
        "/v1/query",
        &json!({"metrics": ["revenue", "order_count"], "dimensions": [{"field": "customers.region"}]}),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let body = reply.json();
    assert_eq!(
        body["columns"],
        json!([
            {"name": "customers.region", "type": "string", "role": "dimension"},
            {"name": "revenue", "type": "decimal(2)", "role": "metric"},
            {"name": "order_count", "type": "int", "role": "metric"},
        ])
    );
    assert_eq!(
        body["rows"],
        json!([["north", "14.50", 2], ["south", "7.25", 1]])
    );
}

#[tokio::test]
async fn filters_sort_limit_and_grains() {
    let state = loaded().await;
    let reply = post(
        &state,
        "/v1/query",
        &json!({
            "metrics": ["revenue"],
            "dimensions": [{"field": "orders.ordered_at", "grain": "month"}],
            "filters": [
                {"field": "orders.status", "in": ["paid"]},
                {"field": "orders.amount", "from": "5", "to": 100},
            ],
            "sort": [{"column": "revenue", "desc": true}],
            "limit": 1,
        }),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let body = reply.json();
    assert_eq!(body["columns"][0]["name"], "orders.ordered_at:month");
    assert_eq!(body["rows"], json!([["2026-01-01", "10.50"]]));
}

#[tokio::test]
async fn no_dimensions_is_one_row_even_with_nothing_selected() {
    let state = loaded().await;
    let reply = post(
        &state,
        "/v1/query",
        &json!({"metrics": ["order_count"], "filters": [{"field": "customers.region", "in": ["nowhere"]}]}),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.json()["rows"], json!([[0]]));
}

#[tokio::test]
async fn unknown_names_are_400s_with_suggestions() {
    let state = loaded().await;
    for (body, expected) in [
        (
            json!({"metrics": ["revenu"]}),
            "no metric `revenu`; did you mean `revenue`?",
        ),
        (
            json!({"metrics": ["revenue"], "dimensions": [{"field": "customers.regoin"}]}),
            "dataset `customers` has no field `regoin`; did you mean `region`?",
        ),
        (
            json!({"metrics": ["revenue"], "dimensions": [{"field": "region"}]}),
            "`region` should be `dataset.field`",
        ),
        (
            json!({"metrics": ["revenue"], "filters": [{"field": "orders.amount", "in": ["lots"]}]}),
            "`\"lots\"` isn't a value of `orders.amount`",
        ),
        (
            json!({"metrics": ["revenue"], "sort": [{"column": "revenu"}]}),
            "no result column `revenu` to sort by; did you mean `revenue`?",
        ),
        (
            json!({"metrics": ["revenue"], "dimensions": [{"field": "orders.ordered_at", "grain": "fortnight"}]}),
            "unknown grain `fortnight`",
        ),
    ] {
        let reply = post(&state, "/v1/query", &body).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{body}");
        let error = reply.json()["error"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert!(error.starts_with(expected), "{error}");
    }
}

#[tokio::test]
async fn answers_follow_writes_to_related_tables() {
    let state = loaded().await;
    let by_region =
        json!({"metrics": ["order_count"], "dimensions": [{"field": "customers.region"}]});
    let reply = post(&state, "/v1/query", &by_region).await;
    assert_eq!(reply.json()["rows"], json!([["north", 2], ["south", 1]]));

    // Ada moves south: the cached answer, relationship map and join column
    // built for the first query all describe the old customers table
    let moved = json!([{"customer_id": "c1", "name": "Ada", "region": "south"}]);
    let reply = post(&state, "/v1/tables/customers/rows?wait=visible", &moved).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let reply = post(&state, "/v1/query", &by_region).await;
    assert_eq!(reply.json()["rows"], json!([["south", 3]]));
}

#[tokio::test]
async fn derived_fields_group_like_columns() {
    let state = loaded().await;
    // `coast` is derived from `region`: north and south aren't coastal
    let reply = post(
        &state,
        "/v1/query",
        &json!({"metrics": ["order_count"], "dimensions": [{"field": "customers.coast"}]}),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.json()["rows"], json!([["north", 2], ["south", 1]]));
    let reply = post(
        &state,
        "/v1/query",
        &json!({
            "metrics": ["order_count"],
            "filters": [{"field": "customers.coast", "in": ["south"]}],
        }),
    )
    .await;
    assert_eq!(reply.json()["rows"], json!([[1]]));
}

#[tokio::test]
async fn the_model_lists_metrics_with_where_they_are_shown() {
    let state = loaded().await;
    let reply = common::call(Arc::clone(&state), Method::GET, "/v1/model").await;
    assert_eq!(reply.status, StatusCode::OK);
    let model = reply.json();
    let revenue = model["metrics"]
        .as_array()
        .and_then(|m| m.iter().find(|m| m["name"] == "revenue"))
        .cloned()
        .unwrap_or_default();
    assert_eq!(revenue["expression"], "SUM(orders.amount)");
    assert_eq!(revenue["anchor"], "orders");
    assert_eq!(revenue["dashboards"], json!(["sales", "sales_recent"]));
    let prev = model["metrics"]
        .as_array()
        .and_then(|m| m.iter().find(|m| m["name"] == "revenue_prev_year"))
        .cloned()
        .unwrap_or_default();
    assert_eq!(prev["time_offset"], "-1 year on orders.ordered_at");
    assert_eq!(
        prev["dashboards"],
        json!(["sales"]),
        "not on the recent one"
    );
    assert_eq!(
        model["relationships"][0],
        json!({"name": "order_customer", "from": "orders", "from_columns": ["customer_id"], "to": "customers", "to_columns": ["customer_id"]})
    );
}

#[tokio::test]
async fn status_counts_rows_per_table() {
    let state = loaded().await;
    let reply = common::call(Arc::clone(&state), Method::GET, "/v1/status").await;
    assert_eq!(reply.status, StatusCode::OK);
    let status = reply.json();
    let orders = status["tables"]
        .as_array()
        .and_then(|t| t.iter().find(|t| t["name"] == "orders"))
        .cloned()
        .unwrap_or_default();
    assert_eq!(orders["rows"], 3);
    // in memory only
    assert!(status.get("durability").is_none());
}

#[tokio::test]
async fn an_answer_comes_as_csv_on_request() {
    let state = loaded().await;
    let body = json!({
        "metrics": ["revenue"],
        "dimensions": [{"field": "customers.region"}],
        "sort": [{"column": "customers.region"}],
    });
    let reply = post(&state, "/v1/query?format=csv", &body).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(
        reply.header("content-type").as_deref(),
        Some("text/csv; charset=utf-8")
    );
    assert_eq!(
        reply.text(),
        "customers.region,revenue\r\nnorth,14.50\r\nsouth,7.25\r\n"
    );
    let reply = post(&state, "/v1/query?format=xml", &body).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_time_shifted_metric_reads_the_selection_a_year_back() {
    let state = loaded().await;
    // February 2027's year before is February 2026: o2 and o3
    let reply = post(
        &state,
        "/v1/query",
        &json!({
            "metrics": ["revenue", "revenue_prev_year"],
            "filters": [{"field": "orders.ordered_at",
                         "from": "2027-02-01T00:00:00Z", "to": "2027-03-01T00:00:00Z"}],
        }),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.json()["rows"], json!([[null, "11.25"]]));
}

/// Microseconds for an RFC 3339 time, as the server's clock counts.
fn at(text: &str) -> i64 {
    match chrono::DateTime::parse_from_rfc3339(text) {
        Ok(time) => time.timestamp_micros(),
        Err(err) => panic!("{text}: {err}"),
    }
}

#[tokio::test]
async fn a_relative_filter_is_a_window_counted_back_from_the_servers_clock() {
    let state = loaded().await;
    let count = |window: Value| {
        let state = Arc::clone(&state);
        async move {
            let reply = post(
                &state,
                "/v1/query",
                &json!({
                    "metrics": ["order_count"],
                    "filters": [{"field": "orders.ordered_at", "relative": window}],
                }),
            )
            .await;
            assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
            reply.json()["rows"][0][0].clone()
        }
    };
    // late on 20 February: o3 is today's, o2 was 19 days ago, o1 in January
    state.set_clock(Some(at("2026-02-20T23:59:00Z")));
    assert_eq!(count(json!({"unit": "day", "last": 1})).await, json!(1));
    assert_eq!(count(json!({"unit": "day", "last": 20})).await, json!(2));
    assert_eq!(count(json!({"unit": "month", "last": 1})).await, json!(2));
    assert_eq!(
        count(json!({"unit": "month", "last": 1, "ago": 1})).await,
        json!(1)
    );
    assert_eq!(count(json!({"unit": "hour", "last": 1})).await, json!(0));
    // a minute later it's the 21st: today is empty, yesterday has o3
    state.set_clock(Some(at("2026-02-21T00:00:30Z")));
    assert_eq!(count(json!({"unit": "day", "last": 1})).await, json!(0));
    assert_eq!(
        count(json!({"unit": "day", "last": 1, "ago": 1})).await,
        json!(1)
    );
}

#[tokio::test]
async fn relative_filters_that_cant_be_windows_are_400s() {
    let state = loaded().await;
    for (filter, expected) in [
        (
            json!({"field": "customers.region", "relative": {"unit": "day", "last": 7}}),
            "only dates and times have a relative window",
        ),
        (
            json!({"field": "orders.ordered_at", "relative": {"unit": "day", "last": 0}}),
            "`last` should be 1 or more",
        ),
        (
            json!({"field": "orders.ordered_at", "from": "2026-01-01", "relative": {"unit": "day", "last": 7}}),
            "needs one of `in`, `from` and/or `to`, or `relative`",
        ),
    ] {
        let reply = post(
            &state,
            "/v1/query",
            &json!({"metrics": ["order_count"], "filters": [filter]}),
        )
        .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{filter}");
        assert!(
            reply.json()["error"]
                .as_str()
                .is_some_and(|e| e.contains(expected)),
            "{filter}: {}",
            reply.text()
        );
    }
}

#[tokio::test]
async fn a_hierarchy_groups_by_a_level_and_filters_by_subtree() {
    // boats sit under water, under outdoor
    let state = loaded().await;
    let reply = post(
        &state,
        "/v1/tables/categories/rows?wait=visible",
        &json!([
            {"category": "outdoor", "parent": null, "level": "department"},
            {"category": "water", "parent": "outdoor", "level": "aisle"},
            {"category": "boats", "parent": "water", "level": "category"},
        ]),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    for (level, node) in [(0, "outdoor"), (1, "water"), (2, "boats")] {
        let reply = post(
            &state,
            "/v1/query",
            &json!({"metrics": ["revenue"],
                    "dimensions": [{"field": "categories.tree", "level": level}]}),
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
        assert_eq!(
            reply.json()["rows"],
            json!([[node, "21.75"]]),
            "level {level}"
        );
    }
    // a level past the deepest node: everything groups as null
    let reply = post(
        &state,
        "/v1/query",
        &json!({"metrics": ["revenue"], "dimensions": [{"field": "categories.tree", "level": 3}]}),
    )
    .await;
    assert_eq!(reply.json()["rows"], json!([[null, "21.75"]]));
    // selecting a node filters by its subtree
    let reply = post(
        &state,
        "/v1/query",
        &json!({"metrics": ["revenue"],
                "filters": [{"field": "categories.tree", "in": ["water"]}]}),
    )
    .await;
    assert_eq!(reply.json()["rows"], json!([["21.75"]]), "{}", reply.text());
    // a level on a field that isn't a hierarchy
    let reply = post(
        &state,
        "/v1/query",
        &json!({"metrics": ["revenue"], "dimensions": [{"field": "customers.region", "level": 1}]}),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.text());
    assert!(
        reply.text().contains("isn't a hierarchy"),
        "{}",
        reply.text()
    );
}
