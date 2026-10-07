//! The HTTP surface, driven in process through `tower::ServiceExt::oneshot`.

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method as HttpMethod, Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::Value;
use tidepool_protocol::api_docs::{Access, Method, endpoints};
use tidepool_server::{AppState, DirAssets, router};
use tower::ServiceExt;

struct Reply {
    status: StatusCode,
    content_type: Option<String>,
    cache_control: Option<String>,
    body: Vec<u8>,
}

impl Reply {
    fn json(&self) -> Value {
        match serde_json::from_slice(&self.body) {
            Ok(value) => value,
            Err(err) => panic!(
                "body should be JSON ({err}): {}",
                String::from_utf8_lossy(&self.body)
            ),
        }
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

fn ready_state() -> Arc<AppState> {
    let state = Arc::new(AppState::new(None));
    state.set_ready();
    state
}

async fn call(state: Arc<AppState>, method: HttpMethod, path: &str) -> Reply {
    let request = match Request::builder()
        .method(method)
        .uri(path)
        .body(Body::empty())
    {
        Ok(request) => request,
        Err(err) => panic!("building a request for {path}: {err}"),
    };
    let response = match router(state).oneshot(request).await {
        Ok(response) => response,
        Err(err) => panic!("the router is infallible: {err}"),
    };
    let header = |name| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(String::from)
    };
    let (status, content_type, cache_control) = (
        response.status(),
        header(header::CONTENT_TYPE),
        header(header::CACHE_CONTROL),
    );
    let body = match response.into_body().collect().await {
        Ok(collected) => collected.to_bytes().to_vec(),
        Err(err) => panic!("reading the body of {path}: {err}"),
    };
    Reply {
        status,
        content_type,
        cache_control,
        body,
    }
}

fn http_method(method: Method) -> HttpMethod {
    match method {
        Method::Get => HttpMethod::GET,
        Method::Post => HttpMethod::POST,
        Method::Put => HttpMethod::PUT,
        Method::Delete => HttpMethod::DELETE,
    }
}

/// Replace `{placeholders}` with a value so the path can be requested.
fn concrete(path: &str) -> String {
    path.split('/')
        .map(|segment| {
            if segment.starts_with('{') {
                "x"
            } else {
                segment
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[tokio::test]
async fn every_documented_endpoint_is_routed_at_its_documented_method() {
    for endpoint in endpoints() {
        let path = concrete(endpoint.path);
        let reply = call(ready_state(), http_method(endpoint.method), &path).await;
        assert!(
            reply.status != StatusCode::NOT_FOUND && reply.status != StatusCode::METHOD_NOT_ALLOWED,
            "{} {} came back {}",
            endpoint.method.label(),
            path,
            reply.status
        );

        // and only at that method: a different one is a 405, not a fallthrough to the UI
        let other = if endpoint.method == Method::Delete {
            HttpMethod::PUT
        } else {
            HttpMethod::DELETE
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
async fn every_endpoint_is_public_until_authentication_exists() {
    // Access is declared per endpoint but nothing enforces it yet (M8). This
    // test is the reminder: the first protected endpoint has to come with the
    // middleware that protects it.
    for endpoint in endpoints() {
        assert_eq!(
            endpoint.access,
            Access::Public,
            "{} is protected, but nothing checks it",
            endpoint.path
        );
    }
}

#[tokio::test]
async fn health_answers_ok() {
    let reply = call(ready_state(), HttpMethod::GET, "/healthz").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json()["status"], "ok");
}

#[tokio::test]
async fn ready_is_503_until_the_server_says_so() {
    let state = Arc::new(AppState::new(None));
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
    let spec = call(ready_state(), HttpMethod::GET, "/v1/openapi.json")
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
    let state = Arc::new(AppState::new(Some(Arc::new(DirAssets::new(dir.path())))));
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
        reply.content_type.as_deref(),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(reply.cache_control.as_deref(), Some("no-cache"));
}

#[tokio::test]
async fn hashed_assets_are_cached_forever() {
    let dir = ui_dir();
    let reply = call(state_with_ui(&dir), HttpMethod::GET, "/assets/app-1234.js").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.content_type.as_deref(),
        Some("text/javascript; charset=utf-8")
    );
    assert!(reply.cache_control.is_some_and(|c| c.contains("immutable")));
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
