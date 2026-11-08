//! Who can write what, from where (plan §9.2): endpoints placed per
//! listener, named ingest credentials scoped to tables and operations,
//! public grants with their limits and origins, idempotency keys per
//! credential, columns the server fills, and the fail-closed start.

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method as HttpMethod, StatusCode};
use common::{Reply, request, send_on};
use serde_json::{Value, json};
use tidepool_config::server::{IngestCredential, Surface};
use tidepool_protocol::api_docs::{Access, Method, Operation, endpoints};
use tidepool_server::{AppState, Credential, InsecureOpen, Listener, Tokens, exposure};

const PUBLIC: Listener = Listener {
    public: true,
    ingest: false,
    admin: false,
};
const INGEST: Listener = Listener {
    public: false,
    ingest: true,
    admin: false,
};
const ADMIN: Listener = Listener {
    public: false,
    ingest: false,
    admin: true,
};

/// A credential from its `tidepool.yaml` shape; each variable `X` it names
/// holds the token `X-secret`.
fn credential(spec: Value) -> Arc<Credential> {
    let spec: IngestCredential = match serde_json::from_value(spec) {
        Ok(spec) => spec,
        Err(err) => panic!("not a credential: {err}"),
    };
    Arc::new(Credential::from_settings(&spec, |name| {
        Some(format!("{name}-secret"))
    }))
}

/// Kayak writes everything, privately; sensors append orders from the
/// internet; crm only upserts customers; web appends orders from a browser,
/// without a token.
fn tokens() -> Tokens {
    Tokens {
        agent: None,
        read: None,
        admin: Some("admin-secret".into()),
        ingest: vec![
            credential(json!({"name": "kayak", "token_env": "KAYAK"})),
            credential(json!({
                "name": "sensors", "token_env": "SENSORS", "public": true,
                "tables": ["orders"], "limits": {"rate": "1000/s"},
            })),
            credential(json!({
                "name": "crm", "token_env": "CRM", "tables": ["customers"],
                "operations": ["upsert"],
            })),
            credential(json!({
                "name": "web", "token": "none", "public": true, "tables": ["orders"],
                "origins": ["https://shop.example.com"],
            })),
        ],
    }
}

fn server(tokens: Tokens) -> Arc<AppState> {
    common::state(common::retail(), tokens, None)
}

fn order(id: &str) -> String {
    format!(
        r#"{{"order_id":"{id}","customer_id":"c1","product_id":"p1","status":"paid","ordered_at":"2026-10-01T00:00:00Z","quantity":1,"amount":"1.00"}}"#
    )
}

const CUSTOMER: &str = r#"[{"customer_id": "c9", "name": "Ada", "region": "north"}]"#;

/// `method` on `table`'s rows through `listener`, with `token` if any.
async fn write(
    server: &Arc<AppState>,
    listener: Listener,
    method: HttpMethod,
    table: &str,
    token: Option<&str>,
    body: &str,
) -> Reply {
    let bearer = token.map(|t| format!("Bearer {t}"));
    let mut headers = vec![("content-type", "application/json")];
    if let Some(bearer) = &bearer {
        headers.push(("authorization", bearer));
    }
    let path = format!("/v1/tables/{table}/rows?wait=visible");
    send_on(
        Arc::clone(server),
        listener,
        request(method, &path, &headers, body.to_string()),
    )
    .await
}

fn concrete(endpoint: &tidepool_protocol::api_docs::ApiDoc) -> String {
    endpoint
        .params
        .iter()
        .fold(endpoint.path.to_string(), |path, param| {
            path.replace(&format!("{{{}}}", param.name), param.example)
        })
}

fn http_method(method: Method) -> HttpMethod {
    match method {
        Method::Get => HttpMethod::GET,
        Method::Post => HttpMethod::POST,
        Method::Put => HttpMethod::PUT,
        Method::Delete => HttpMethod::DELETE,
    }
}

/// Whether `reply` says the listener doesn't serve the path at all.
fn unserved(reply: &Reply) -> bool {
    reply.status == StatusCode::NOT_FOUND
        && (reply.text().contains("isn't served on this listener")
            || reply.text().contains("no endpoint"))
}

#[tokio::test]
async fn every_endpoint_is_on_the_listeners_its_access_says() {
    for (with_public, tokens) in [
        (true, tokens()),
        (false, Tokens::levels(Some("i"), None, Some("admin-secret"))),
    ] {
        let server = server(tokens);
        for endpoint in endpoints() {
            let path = concrete(&endpoint);
            let method = http_method(endpoint.method);
            for (name, listener) in [("public", PUBLIC), ("ingest", INGEST), ("admin", ADMIN)] {
                let expected = match (endpoint.operation, endpoint.access) {
                    (Operation::Metrics, _) | (_, Access::Admin) => listener.admin,
                    (_, Access::Public | Access::Read) => true,
                    (_, Access::Ingest) => !listener.public_only() || with_public,
                };
                let reply = send_on(
                    Arc::clone(&server),
                    listener,
                    request(
                        method.clone(),
                        &path,
                        &[("authorization", "Bearer admin-secret")],
                        Body::empty(),
                    ),
                )
                .await;
                assert_eq!(
                    !unserved(&reply),
                    expected,
                    "{method} {path} on the {name} listener (public credentials: {with_public}): {} {}",
                    reply.status,
                    reply.text()
                );
            }
        }
    }
}

const ORDERS: &str = r#"[{"order_id":"o1","customer_id":"c1","product_id":"p1","status":"paid","ordered_at":"2026-10-01T00:00:00Z","quantity":1,"amount":"1.00"}]"#;
const KEYS: &str = r#"[{"customer_id": "c9"}]"#;

/// (listener, method, table, token, body, status)
type Case = (
    Listener,
    HttpMethod,
    &'static str,
    Option<&'static str>,
    &'static str,
    StatusCode,
);

fn credential_cases() -> Vec<Case> {
    use HttpMethod as M;
    use StatusCode as S;
    let (kayak, sensors, crm, admin) = (
        Some("KAYAK-secret"),
        Some("SENSORS-secret"),
        Some("CRM-secret"),
        Some("admin-secret"),
    );
    vec![
        // kayak: everything, but not from the internet
        (INGEST, M::POST, "orders", kayak, ORDERS, S::OK),
        (INGEST, M::PUT, "customers", kayak, CUSTOMER, S::OK),
        (PUBLIC, M::POST, "orders", kayak, ORDERS, S::NOT_FOUND),
        // sensors: appends orders, from anywhere
        (PUBLIC, M::POST, "orders", sensors, ORDERS, S::OK),
        (INGEST, M::POST, "orders", sensors, ORDERS, S::OK),
        (
            PUBLIC,
            M::POST,
            "customers",
            sensors,
            CUSTOMER,
            S::NOT_FOUND,
        ),
        (
            INGEST,
            M::POST,
            "customers",
            sensors,
            CUSTOMER,
            S::FORBIDDEN,
        ),
        // crm: upserts customers, deletes nothing
        (INGEST, M::POST, "customers", crm, CUSTOMER, S::OK),
        (INGEST, M::DELETE, "customers", crm, KEYS, S::FORBIDDEN),
        // no token: web appends orders; customers want a token
        (PUBLIC, M::POST, "orders", None, ORDERS, S::OK),
        (INGEST, M::POST, "orders", None, ORDERS, S::OK),
        (
            INGEST,
            M::POST,
            "customers",
            None,
            CUSTOMER,
            S::UNAUTHORIZED,
        ),
        (PUBLIC, M::POST, "customers", None, CUSTOMER, S::NOT_FOUND),
        // the admin token writes anything, but not through the public listener
        (INGEST, M::POST, "customers", admin, CUSTOMER, S::OK),
        (PUBLIC, M::POST, "customers", admin, CUSTOMER, S::NOT_FOUND),
        // the public listener knows no more tables than a bad name
        (PUBLIC, M::POST, "nonsense", sensors, ORDERS, S::NOT_FOUND),
    ]
}

#[tokio::test]
async fn credentials_write_what_they_are_granted_where_they_are_allowed() {
    let server = server(tokens());
    for (i, (listener, method, table, token, body, status)) in
        credential_cases().into_iter().enumerate()
    {
        let reply = write(&server, listener, method.clone(), table, token, body).await;
        assert_eq!(
            reply.status,
            status,
            "case {i}: {method} {table} with {token:?}: {}",
            reply.text()
        );
        if listener == PUBLIC && status == StatusCode::NOT_FOUND {
            // the same answer as for a table that doesn't exist: no names
            assert_eq!(reply.json()["error"], format!("no table `{table}`"));
        }
    }
}

#[tokio::test]
async fn without_any_open_credential_a_missing_token_is_401_before_the_table() {
    let tokens = Tokens {
        ingest: vec![credential(json!({"name": "kayak", "token_env": "KAYAK"}))],
        ..Tokens::default()
    };
    let server = server(tokens);
    let reply = write(
        &server,
        INGEST,
        HttpMethod::POST,
        "no_such_table",
        None,
        "[]",
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{}", reply.text());
    let reply = write(
        &server,
        INGEST,
        HttpMethod::POST,
        "orders",
        Some("nope"),
        "[]",
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{}", reply.text());
}

#[tokio::test]
async fn idempotency_keys_belong_to_the_credential_that_sent_them() {
    let server = server(tokens());
    let keyed = |token: &'static str, id: &'static str| {
        let server = Arc::clone(&server);
        async move {
            let bearer = format!("Bearer {token}");
            send_on(
                server,
                INGEST,
                request(
                    HttpMethod::POST,
                    "/v1/tables/orders/rows?wait=visible",
                    &[
                        ("content-type", "application/json"),
                        ("authorization", &bearer),
                        ("idempotency-key", "batch-1"),
                    ],
                    format!("[{}]", order(id)),
                ),
            )
            .await
        }
    };
    let kayak = keyed("KAYAK-secret", "k1").await;
    // the same key from another credential is another key: written
    let sensors = keyed("SENSORS-secret", "s1").await;
    assert_eq!(kayak.json()["written"], 1);
    assert_eq!(sensors.json()["written"], 1);
    // and each one's retry gets its own first answer
    assert_eq!(keyed("KAYAK-secret", "k2").await.json(), kayak.json());
    assert_eq!(keyed("SENSORS-secret", "s2").await.json(), sensors.json());
    let rows = common::call(Arc::clone(&server), HttpMethod::GET, "/v1/tables/orders").await;
    assert_eq!(rows.json()["rows"], 2);
}

#[tokio::test]
async fn a_public_credential_is_held_to_its_limits() {
    let server = server(Tokens {
        ingest: vec![credential(json!({
            "name": "sensors", "token_env": "SENSORS", "public": true, "tables": ["orders"],
            "limits": {"rate": "2/min", "batch_rows": 2, "body": "1KiB", "new_values": "3/h"},
        }))],
        ..Tokens::default()
    });
    let token = Some("SENSORS-secret");
    let post = |body: String| {
        let server = Arc::clone(&server);
        async move { write(&server, PUBLIC, HttpMethod::POST, "orders", token, &body).await }
    };
    // three rows: over batch_rows
    let three = format!("[{},{},{}]", order("a"), order("b"), order("c"));
    let reply = post(three).await;
    assert_eq!(
        reply.status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "{}",
        reply.text()
    );
    // four new customers in two rows... over new_values (3/h): refused whole
    let new = format!(
        "[{},{}]",
        order("d")
            .replace("\"c1\"", "\"c7\"")
            .replace("\"p1\"", "\"p7\""),
        order("e").replace("\"c1\"", "\"c8\"")
    );
    let reply = post(new).await;
    assert_eq!(
        reply.status,
        StatusCode::TOO_MANY_REQUESTS,
        "{}",
        reply.text()
    );
    assert!(reply.header("retry-after").is_some());
    assert!(reply.text().contains("new values"), "{}", reply.text());
    // two a minute: that was the second request
    let reply = post(format!("[{}]", order("f"))).await;
    assert_eq!(
        reply.status,
        StatusCode::TOO_MANY_REQUESTS,
        "{}",
        reply.text()
    );
    assert!(reply.text().contains("rate"), "{}", reply.text());
    let rows = common::call(Arc::clone(&server), HttpMethod::GET, "/v1/tables/orders").await;
    assert_eq!(rows.json()["rows"], 0, "nothing refused was written");
    let metrics = common::call(Arc::clone(&server), HttpMethod::GET, "/metrics").await;
    for limit in ["batch_rows", "new_values", "rate"] {
        let line =
            format!("tidepool_ingest_refused_total{{credential=\"sensors\",limit=\"{limit}\"}} 1");
        assert!(metrics.text().contains(&line), "no `{line}`");
    }
}

#[tokio::test]
async fn a_public_body_too_large_is_refused_unread() {
    let server = server(Tokens {
        ingest: vec![credential(json!({
            "name": "sensors", "token_env": "SENSORS", "public": true, "tables": ["orders"],
            "limits": {"body": "1KiB"},
        }))],
        ..Tokens::default()
    });
    let big = format!("[{}]", vec![order("x"); 20].join(","));
    let reply = write(
        &server,
        PUBLIC,
        HttpMethod::POST,
        "orders",
        Some("SENSORS-secret"),
        &big,
    )
    .await;
    assert_eq!(
        reply.status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "{}",
        reply.text()
    );
}

#[tokio::test]
async fn public_producers_get_a_quarter_of_the_queue() {
    // a writer that never takes anything off its queue: every batch waits
    let (writer, _guard) =
        tidepool_store::WriterHandle::unconsumed(common::retail(), tidepool_store::writer::QUEUE);
    let server = Arc::new(AppState::with_writer(
        writer,
        None,
        Tokens {
            ingest: vec![
                credential(json!({"name": "kayak", "token_env": "KAYAK"})),
                credential(json!({
                    "name": "sensors", "token_env": "SENSORS", "public": true, "tables": ["orders"],
                    "limits": {"rate": "1000/s"},
                })),
            ],
            ..Tokens::default()
        },
    ));
    let one = |token: &'static str, listener: Listener| {
        let server = Arc::clone(&server);
        tokio::spawn(async move {
            write(
                &server,
                listener,
                HttpMethod::POST,
                "orders",
                Some(token),
                &format!("[{}]", order("q")),
            )
            .await
        })
    };
    let waiting: Vec<_> = (0..16).map(|_| one("SENSORS-secret", PUBLIC)).collect();
    tokio::time::sleep(Duration::from_millis(200)).await;
    // the share is full: the next public batch is turned away at once
    let Ok(Ok(reply)) =
        tokio::time::timeout(Duration::from_secs(5), one("SENSORS-secret", PUBLIC)).await
    else {
        panic!("a public batch over the share should be answered at once");
    };
    assert_eq!(
        reply.status,
        StatusCode::TOO_MANY_REQUESTS,
        "{}",
        reply.text()
    );
    // while a private producer still gets in line
    let private = one("KAYAK-secret", INGEST);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !private.is_finished(),
        "a private batch waits in the queue, not refused"
    );
    for task in waiting {
        assert!(!task.is_finished());
        task.abort();
    }
    private.abort();
}

#[tokio::test]
async fn browsers_write_from_the_origins_a_credential_lists() {
    let server = server(tokens());
    let from = |origin: &str, method: HttpMethod, body: &str| {
        send_on(
            Arc::clone(&server),
            PUBLIC,
            request(
                method,
                "/v1/tables/orders/rows",
                &[("content-type", "application/json"), ("origin", origin)],
                body.to_string(),
            ),
        )
    };
    let shop = "https://shop.example.com";
    let preflight = from(shop, HttpMethod::OPTIONS, "").await;
    assert_eq!(
        preflight.status,
        StatusCode::NO_CONTENT,
        "{}",
        preflight.text()
    );
    assert_eq!(
        preflight.header("access-control-allow-origin").as_deref(),
        Some(shop)
    );
    assert!(
        preflight
            .header("access-control-allow-headers")
            .is_some_and(|h| h.contains("idempotency-key"))
    );
    let written = from(shop, HttpMethod::POST, &format!("[{}]", order("w1"))).await;
    assert_eq!(written.status, StatusCode::OK, "{}", written.text());
    assert_eq!(
        written.header("access-control-allow-origin").as_deref(),
        Some(shop)
    );

    let evil = "https://evil.example.com";
    assert_eq!(
        from(evil, HttpMethod::OPTIONS, "").await.status,
        StatusCode::FORBIDDEN
    );
    let refused = from(evil, HttpMethod::POST, &format!("[{}]", order("w2"))).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.text());
    assert!(refused.header("access-control-allow-origin").is_none());
}

#[tokio::test]
async fn filled_columns_carry_the_credential_and_the_time() {
    let model = common::retail_with(
        "tables/orders.yaml",
        "  note:        { type: string, nullable: true, encoding: plain }\n",
        "  note:        { type: string, nullable: true, encoding: plain }\n  written_by:  { type: string, filled: credential }\n  received_at: { type: timestamptz, filled: received }\n",
    );
    let server = common::state(model, tokens(), None);
    let reply = write(
        &server,
        PUBLIC,
        HttpMethod::POST,
        "orders",
        Some("SENSORS-secret"),
        &format!("[{}]", order("f1")),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let reply = write(
        &server,
        INGEST,
        HttpMethod::POST,
        "orders",
        Some("KAYAK-secret"),
        &format!("[{}]", order("f2")),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let preview = common::call(
        Arc::clone(&server),
        HttpMethod::GET,
        "/v1/tables/orders/preview",
    )
    .await;
    let preview = preview.json();
    let column = |name: &str| {
        preview["columns"]
            .as_array()
            .and_then(|c| c.iter().position(|c| c == name))
            .unwrap_or_else(|| panic!("no column {name}"))
    };
    let (by, at) = (column("written_by"), column("received_at"));
    let rows = preview["rows"].as_array().cloned().unwrap_or_default();
    let mut writers: Vec<&str> = rows.iter().filter_map(|r| r[by].as_str()).collect();
    writers.sort_unstable();
    assert_eq!(writers, ["kayak", "sensors"]);
    assert!(
        rows.iter()
            .all(|r| r[at].as_str().is_some_and(|t| t.starts_with("20")))
    );

    // a producer can't send them
    let forged = format!(
        "[{}]",
        order("f3").replace('}', r#","written_by":"admin"}"#)
    );
    let reply = write(
        &server,
        INGEST,
        HttpMethod::POST,
        "orders",
        Some("KAYAK-secret"),
        &forged,
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.text());
    // the table says so, for producers (Kayak) that check before writing
    let info = common::call(Arc::clone(&server), HttpMethod::GET, "/v1/tables/orders").await;
    let filled: Vec<Value> = info.json()["columns"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c.get("filled").cloned())
        .collect();
    assert_eq!(filled, [json!("credential"), json!("received")]);
}

#[tokio::test]
async fn the_public_openapi_lists_only_what_the_public_may_write() {
    let server = server(tokens());
    let public = send_on(
        Arc::clone(&server),
        PUBLIC,
        request(HttpMethod::GET, "/v1/openapi.json", &[], Body::empty()),
    )
    .await
    .json();
    let paths = public["paths"].as_object().cloned().unwrap_or_default();
    assert!(paths["/v1/tables/orders/rows"].get("post").is_some());
    assert!(!paths.contains_key("/v1/tables/customers/rows"));
    assert!(!paths.contains_key("/v1/config/apply"));
    assert!(!paths.contains_key("/metrics"));
    // and the ingest listener's has every table
    let private = send_on(
        Arc::clone(&server),
        INGEST,
        request(HttpMethod::GET, "/v1/openapi.json", &[], Body::empty()),
    )
    .await
    .json();
    assert!(private["paths"].get("/v1/tables/customers/rows").is_some());
}

#[tokio::test]
async fn mcp_on_the_public_listener_does_only_what_it_does_there() {
    let server = server(tokens());
    let call = |listener: Listener, token: &'static str, table: &'static str| {
        let server = Arc::clone(&server);
        async move {
            let body = json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {"name": "ingest_rows", "arguments": {"table": table, "rows": [{
                    "customer_id": "c5", "name": "Bo", "region": "south",
                }]}},
            });
            let bearer = format!("Bearer {token}");
            send_on(
                server,
                listener,
                request(
                    HttpMethod::POST,
                    "/mcp",
                    &[
                        ("content-type", "application/json"),
                        ("authorization", &bearer),
                    ],
                    body.to_string(),
                ),
            )
            .await
            .json()
        }
    };
    // the admin token writes customers through MCP on the ingest listener...
    let reply = call(INGEST, "admin-secret", "customers").await;
    assert_eq!(reply["result"]["isError"], false, "{reply}");
    // ...but not on the public one, where the rows endpoint hides the table
    let reply = call(PUBLIC, "admin-secret", "customers").await;
    assert_eq!(reply["result"]["isError"], true, "{reply}");
}

#[test]
fn ingest_or_admin_open_away_from_loopback_refuses_to_start() {
    let address = |text: &str| match text.parse() {
        Ok(address) => address,
        Err(err) => panic!("{text}: {err}"),
    };
    let everything = vec![Surface::Public, Surface::Ingest, Surface::Admin];
    let open = Tokens::default();
    // on loopback anything goes, quietly
    let loopback = vec![(address("127.0.0.1:7070"), everything.clone())];
    assert_eq!(
        exposure(&loopback, &open, InsecureOpen::default())
            .map(|w| w.len())
            .ok(),
        Some(0)
    );
    // everywhere else, open ingest or admin needs saying so
    let exposed = vec![(address("0.0.0.0:7070"), everything.clone())];
    let Err(err) = exposure(&exposed, &open, InsecureOpen::default()) else {
        panic!("open ingest on 0.0.0.0 should refuse to start");
    };
    assert!(err.to_string().contains("with ingest open"), "{err}");
    let Err(err) = exposure(
        &exposed,
        &open,
        InsecureOpen {
            ingest: true,
            admin: false,
        },
    ) else {
        panic!("open admin on 0.0.0.0 should refuse to start");
    };
    assert!(err.to_string().contains("with admin open"), "{err}");
    let insecure = InsecureOpen {
        ingest: true,
        admin: true,
    };
    match exposure(&exposed, &open, insecure) {
        Ok(warnings) => assert!(
            warnings.iter().any(|w| w.contains("read, ingest, admin")),
            "{warnings:?}"
        ),
        Err(err) => panic!("--insecure-open should let it start: {err}"),
    }
    // a public listener on 0.0.0.0 with ingest and admin on loopback is the
    // intended shape: only read is open there, which is a warning
    let split = vec![
        (address("0.0.0.0:7070"), vec![Surface::Public]),
        (
            address("127.0.0.1:7071"),
            vec![Surface::Ingest, Surface::Admin],
        ),
    ];
    match exposure(&split, &open, InsecureOpen::default()) {
        Ok(warnings) => assert_eq!(warnings.len(), 1, "{warnings:?}"),
        Err(err) => panic!("a public listener with private ingest should start: {err}"),
    }
    // tokens set: no complaint at all
    let closed = Tokens::levels(Some("i"), Some("r"), Some("a"));
    assert_eq!(
        exposure(&exposed, &closed, InsecureOpen::default())
            .map(|w| w.len())
            .ok(),
        Some(0)
    );
    // two credentials with one token can't be told apart
    let shared = Tokens {
        ingest: vec![
            credential(json!({"name": "a", "token_env": "SAME"})),
            credential(json!({"name": "b", "token_env": "SAME"})),
        ],
        ..closed
    };
    let Err(err) = exposure(&loopback, &shared, InsecureOpen::default()) else {
        panic!("a shared token should refuse to start");
    };
    assert!(err.to_string().contains("the same token"), "{err}");
}
