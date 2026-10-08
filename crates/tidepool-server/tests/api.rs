//! The HTTP surface, driven in process through `tower::ServiceExt::oneshot`.

mod common;

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::http::{Method as HttpMethod, StatusCode};
use common::{call, ready_state, request, send, state};
use serde_json::Value;
use tidepool_protocol::api_docs::{Access, Method, endpoints};
use tidepool_server::{AppState, DirAssets, Tokens};

fn http_method(method: Method) -> HttpMethod {
    match method {
        Method::Get => HttpMethod::GET,
        Method::Post => HttpMethod::POST,
        Method::Put => HttpMethod::PUT,
        Method::Delete => HttpMethod::DELETE,
    }
}

/// The path with every `{placeholder}` replaced by its documented example.
fn concrete(endpoint: &tidepool_protocol::api_docs::ApiDoc) -> String {
    endpoint
        .params
        .iter()
        .fold(endpoint.path.to_string(), |path, param| {
            path.replace(&format!("{{{}}}", param.name), param.example)
        })
}

#[tokio::test]
async fn every_documented_endpoint_is_routed_at_its_documented_method() {
    for endpoint in endpoints() {
        let path = concrete(&endpoint);
        let reply = call(ready_state(), http_method(endpoint.method), &path).await;
        // a handler may answer 404 itself (no such pending change); the
        // fallback's 404 is what an unrouted path gets
        let unrouted =
            reply.status == StatusCode::NOT_FOUND && reply.text().contains("no endpoint");
        assert!(
            !unrouted && reply.status != StatusCode::METHOD_NOT_ALLOWED,
            "{} {} came back {}",
            endpoint.method.label(),
            path,
            reply.status
        );

        // and only at its methods: an undocumented one is a 405, not a
        // fallthrough to the UI
        let documented: Vec<Method> = endpoints()
            .iter()
            .filter(|e| e.path == endpoint.path)
            .map(|e| e.method)
            .collect();
        let Some(other) = [Method::Put, Method::Delete, Method::Post, Method::Get]
            .into_iter()
            .find(|m| !documented.contains(m))
            .map(http_method)
        else {
            continue;
        };
        let reply = call(ready_state(), other.clone(), &path).await;
        assert_eq!(
            reply.status,
            StatusCode::METHOD_NOT_ALLOWED,
            "{other} {path}"
        );
    }
}

#[tokio::test]
async fn every_protected_endpoint_wants_its_token() {
    let tokens = Tokens {
        ingest: Some("ingest-secret".into()),
        read: Some("read-secret".into()),
        admin: Some("admin-secret".into()),
    };
    let server = state(common::retail(), tokens.clone(), None);
    for endpoint in endpoints() {
        let Some(token) = tokens.required(endpoint.access) else {
            continue;
        };
        let path = concrete(&endpoint);
        let method = http_method(endpoint.method);
        let without = call(Arc::clone(&server), method.clone(), &path).await;
        assert_eq!(
            without.status,
            StatusCode::UNAUTHORIZED,
            "{method} {path} without a token"
        );
        assert!(
            without
                .header("www-authenticate")
                .is_some_and(|h| h.starts_with("Bearer"))
        );

        let wrong = send(
            Arc::clone(&server),
            request(
                method.clone(),
                &path,
                &[("authorization", "Bearer nope")],
                "",
            ),
        )
        .await;
        assert_eq!(
            wrong.status,
            StatusCode::UNAUTHORIZED,
            "{method} {path} with a wrong token"
        );

        let right = send(
            Arc::clone(&server),
            request(
                method.clone(),
                &path,
                &[("authorization", &format!("Bearer {token}"))],
                "",
            ),
        )
        .await;
        assert_ne!(
            right.status,
            StatusCode::UNAUTHORIZED,
            "{method} {path} with its token"
        );
    }
    // and a token for one level doesn't open another
    let reply = send(
        Arc::clone(&server),
        request(
            HttpMethod::GET,
            "/v1/tables",
            &[("authorization", "Bearer ingest-secret")],
            "",
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert_eq!(Access::Read.label(), "read");
}

#[tokio::test]
async fn health_answers_ok() {
    let reply = call(ready_state(), HttpMethod::GET, "/healthz").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json()["status"], "ok");
}

#[tokio::test]
async fn ready_is_503_until_the_server_says_so() {
    let config = tidepool_store::WriterConfig::default();
    let state = match AppState::new(common::empty(), config, None, Tokens::default()) {
        Ok(state) => Arc::new(state),
        Err(err) => panic!("starting the writer: {err}"),
    };
    let reply = call(Arc::clone(&state), HttpMethod::GET, "/readyz").await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reply.json()["status"], "starting");

    state.set_ready();
    let reply = call(state, HttpMethod::GET, "/readyz").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json()["status"], "ready");
}

#[tokio::test]
async fn info_names_the_server_and_its_versions() {
    let reply = call(ready_state(), HttpMethod::GET, "/v1/info").await;
    assert_eq!(reply.status, StatusCode::OK);
    let info = reply.json();
    assert_eq!(info["name"], "tidepool");
    assert_eq!(info["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(info["api_version"], 1);
}

#[tokio::test]
async fn the_spec_lists_exactly_the_documented_endpoints() {
    let empty = state(common::empty(), Tokens::default(), None);
    let spec = call(empty, HttpMethod::GET, "/v1/openapi.json")
        .await
        .json();
    assert_eq!(spec["openapi"], "3.1.0");

    let mut in_spec = BTreeSet::new();
    if let Some(paths) = spec["paths"].as_object() {
        for (path, item) in paths {
            for (method, operation) in item.as_object().into_iter().flatten() {
                assert!(
                    operation["operationId"].is_string(),
                    "{method} {path} has no operationId"
                );
                in_spec.insert((path.clone(), method.clone()));
            }
        }
    }
    let documented: BTreeSet<_> = endpoints()
        .iter()
        .map(|e| (e.path.to_string(), e.method.key().to_string()))
        .collect();
    assert_eq!(in_spec, documented);
}

#[tokio::test]
async fn every_ref_in_the_spec_resolves() {
    fn refs(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::Object(object) => {
                for (key, value) in object {
                    match (key.as_str(), value) {
                        ("$ref", Value::String(reference)) => out.push(reference.clone()),
                        _ => refs(value, out),
                    }
                }
            }
            Value::Array(items) => items.iter().for_each(|item| refs(item, out)),
            _ => {}
        }
    }

    let spec = call(ready_state(), HttpMethod::GET, "/v1/openapi.json")
        .await
        .json();
    let mut found = Vec::new();
    refs(&spec, &mut found);
    assert!(
        !found.is_empty(),
        "the spec should reference at least one schema"
    );
    for reference in found {
        let Some(name) = reference.strip_prefix("#/components/schemas/") else {
            panic!("{reference} doesn't point into components/schemas");
        };
        assert!(
            spec["components"]["schemas"][name].is_object(),
            "{reference} doesn't resolve"
        );
    }
}

#[tokio::test]
async fn an_unknown_api_path_is_a_json_404_not_the_app() {
    let reply = call(ready_state(), HttpMethod::GET, "/v1/nope").await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert!(
        reply.json()["error"]
            .as_str()
            .is_some_and(|e| e.contains("/v1/nope"))
    );
}

#[tokio::test]
async fn without_a_ui_the_root_says_so() {
    let reply = call(ready_state(), HttpMethod::GET, "/").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.text().contains("built without the web UI"));
}

fn ui_dir() -> tempfile::TempDir {
    let dir = match tempfile::tempdir() {
        Ok(dir) => dir,
        Err(err) => panic!("creating a temp dir: {err}"),
    };
    let write = |path: &str, contents: &str| {
        let path = dir.path().join(path);
        if let Some(parent) = path.parent()
            && let Err(err) = std::fs::create_dir_all(parent)
        {
            panic!("creating {}: {err}", parent.display());
        }
        if let Err(err) = std::fs::write(&path, contents) {
            panic!("writing {}: {err}", path.display());
        }
    };
    write("index.html", "<html>the app</html>");
    write("assets/app-1234.js", "console.log('hi')");
    dir
}

fn state_with_ui(dir: &tempfile::TempDir) -> Arc<AppState> {
    let state = state(
        common::retail(),
        Tokens::default(),
        Some(Arc::new(DirAssets::new(dir.path()))),
    );
    state.set_ready();
    state
}

#[tokio::test]
async fn the_ui_is_served_from_its_root() {
    let dir = ui_dir();
    let reply = call(state_with_ui(&dir), HttpMethod::GET, "/").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.text(), "<html>the app</html>");
    assert_eq!(
        reply.header("content-type").as_deref(),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(reply.header("cache-control").as_deref(), Some("no-cache"));
}

#[tokio::test]
async fn hashed_assets_are_cached_forever() {
    let dir = ui_dir();
    let reply = call(state_with_ui(&dir), HttpMethod::GET, "/assets/app-1234.js").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.header("content-type").as_deref(),
        Some("text/javascript; charset=utf-8")
    );
    assert!(
        reply
            .header("cache-control")
            .is_some_and(|c| c.contains("immutable"))
    );
}

#[tokio::test]
async fn client_routes_get_the_app_and_missing_files_get_404() {
    let dir = ui_dir();
    let reply = call(state_with_ui(&dir), HttpMethod::GET, "/dashboards/sales").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.text(), "<html>the app</html>");

    let reply = call(state_with_ui(&dir), HttpMethod::GET, "/assets/missing.js").await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn paths_outside_the_ui_root_are_refused() {
    let dir = ui_dir();
    for path in ["/../Cargo.toml", "/assets/../../etc/passwd"] {
        let reply = call(state_with_ui(&dir), HttpMethod::GET, path).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{path}");
    }
}

#[tokio::test]
async fn the_ui_answers_only_reads() {
    let dir = ui_dir();
    let reply = call(state_with_ui(&dir), HttpMethod::POST, "/").await;
    assert_eq!(reply.status, StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn every_config_kind_has_a_schema_and_others_are_404() {
    for kind in tidepool_config::schema::KINDS {
        let reply = call(
            ready_state(),
            HttpMethod::GET,
            &format!("/v1/schemas/{kind}"),
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK, "{kind}");
        assert!(reply.json().is_object(), "{kind}");
    }
    let reply = call(ready_state(), HttpMethod::GET, "/v1/schemas/nope").await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert!(
        reply.json()["error"]
            .as_str()
            .is_some_and(|e| e.contains("kinds: server, table"))
    );
}

#[tokio::test]
async fn the_token_may_come_in_the_query_string() {
    // browsers can't set headers on a WebSocket, so `?token=` works too
    let tokens = Tokens {
        read: Some("read-secret".into()),
        ..Tokens::default()
    };
    let server = state(common::retail(), tokens, None);
    let reply = call(
        Arc::clone(&server),
        HttpMethod::GET,
        "/v1/tables?token=read-secret",
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    let reply = call(
        Arc::clone(&server),
        HttpMethod::GET,
        "/v1/tables?token=wrong",
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn status_reports_the_log_when_data_is_kept_on_disk() {
    let dir = match tempfile::tempdir() {
        Ok(dir) => dir,
        Err(err) => panic!("{err}"),
    };
    let config = tidepool_store::WriterConfig {
        data: Some(dir.path().to_path_buf()),
        epoch_interval: std::time::Duration::from_millis(10),
        ..tidepool_store::WriterConfig::default()
    };
    let server = match AppState::new(common::retail(), config, None, Tokens::default()) {
        Ok(state) => Arc::new(state),
        Err(err) => panic!("{err}"),
    };
    let body = r#"[{"customer_id": "c1", "name": "Ada", "region": "north"}]"#;
    let reply = send(
        Arc::clone(&server),
        request(
            HttpMethod::POST,
            "/v1/tables/customers/rows",
            &[("content-type", "application/json")],
            body,
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let status = call(Arc::clone(&server), HttpMethod::GET, "/v1/status")
        .await
        .json();
    assert_eq!(status["durability"]["mode"], "fsync");
    // the log starts with the model the data is in, then the write
    assert_eq!(status["durability"]["last_lsn"], 2);
    assert_eq!(status["durability"]["checkpoint_lsn"], 0);
}

#[tokio::test]
async fn metrics_count_what_happened_in_the_prometheus_format() {
    let server = ready_state();
    let body = r#"[{"customer_id": "c1", "name": "Ada", "region": "north"}]"#;
    let reply = send(
        Arc::clone(&server),
        request(
            HttpMethod::POST,
            "/v1/tables/customers/rows?wait=visible",
            &[("content-type", "application/json")],
            body,
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let reply = call(Arc::clone(&server), HttpMethod::GET, "/metrics").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(
        reply
            .header("content-type")
            .is_some_and(|t| t.starts_with("text/plain; version=0.0.4"))
    );
    let text = reply.text();
    for wanted in [
        "tidepool_table_rows{table=\"customers\"} 1",
        "tidepool_rows_written_total{table=\"customers\"} 1",
        "tidepool_ingest_requests_total{table=\"customers\",op=\"write\",status=\"200\"} 1",
        "# TYPE tidepool_epoch_publish_seconds summary",
    ] {
        assert!(text.contains(wanted), "no `{wanted}` in:\n{text}");
    }
    // every sample: a name, maybe labels, a number
    for line in text.lines().filter(|l| !l.starts_with('#')) {
        let (series, value) = line.rsplit_once(' ').unwrap_or_default();
        assert!(value.parse::<f64>().is_ok(), "not a number in `{line}`");
        let name = series.split('{').next().unwrap_or_default();
        assert!(
            name.starts_with("tidepool_")
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
            "bad name in `{line}`"
        );
    }
}

fn three_tokens() -> Tokens {
    Tokens {
        ingest: Some("ingest-secret".into()),
        read: Some("read-secret".into()),
        admin: Some("admin-secret".into()),
    }
}

#[tokio::test]
async fn the_admin_token_is_good_for_every_level() {
    let server = state(common::retail(), three_tokens(), None);
    let with = |path: &str, method: HttpMethod, body: &'static str| {
        let server = Arc::clone(&server);
        let path = path.to_string();
        async move {
            send(
                server,
                request(
                    method,
                    &path,
                    &[
                        ("authorization", "Bearer admin-secret"),
                        ("content-type", "application/json"),
                    ],
                    body,
                ),
            )
            .await
        }
    };
    assert_eq!(
        with("/v1/tables", HttpMethod::GET, "").await.status,
        StatusCode::OK
    );
    let rows = r#"[{"customer_id": "c1", "name": "Ada", "region": "north"}]"#;
    let reply = with("/v1/tables/customers/rows", HttpMethod::POST, rows).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
}

#[tokio::test]
async fn a_token_traded_for_a_session_works_until_signing_out() {
    let server = state(common::retail(), three_tokens(), None);
    let start = |token: &'static str| {
        let server = Arc::clone(&server);
        async move {
            send(
                server,
                request(
                    HttpMethod::POST,
                    "/v1/auth/session",
                    &[("authorization", &format!("Bearer {token}"))],
                    "",
                ),
            )
            .await
        }
    };
    let reply = start("read-secret").await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(
        reply.json(),
        serde_json::json!({"read": true, "ingest": false, "admin": false})
    );
    let set = reply.header("set-cookie").unwrap_or_default();
    assert!(
        set.contains("HttpOnly") && set.contains("SameSite=Strict"),
        "{set}"
    );
    let cookie = set.split(';').next().unwrap_or_default().to_string();
    let with_cookie = |method: HttpMethod, path: &'static str| {
        let (server, cookie) = (Arc::clone(&server), cookie.clone());
        async move { send(server, request(method, path, &[("cookie", &cookie)], "")).await }
    };
    assert_eq!(
        with_cookie(HttpMethod::GET, "/v1/tables").await.status,
        StatusCode::OK
    );
    // a read session is a read session
    assert_eq!(
        with_cookie(HttpMethod::GET, "/v1/config").await.status,
        StatusCode::UNAUTHORIZED
    );
    let me = with_cookie(HttpMethod::GET, "/v1/auth/session").await;
    assert_eq!(me.json()["read"], true);
    let out = with_cookie(HttpMethod::DELETE, "/v1/auth/session").await;
    assert!(
        out.header("set-cookie")
            .is_some_and(|c| c.contains("Max-Age=0"))
    );
    assert_eq!(
        with_cookie(HttpMethod::GET, "/v1/tables").await.status,
        StatusCode::UNAUTHORIZED
    );
    // an admin session opens everything; a bad token opens nothing
    assert_eq!(start("admin-secret").await.json()["admin"], true);
    assert_eq!(start("nope").await.status, StatusCode::UNAUTHORIZED);
}
