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
