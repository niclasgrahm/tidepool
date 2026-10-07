//! The UI compiled into the binary. Only meaningful with `embed-assets`, which
//! needs `frontend/dist` to exist: run with `just test-embed` after
//! `just build`. Without the feature this file compiles to nothing.
#![cfg(feature = "embed-assets")]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use tidepool_server::{AppState, router, ui};
use tower::ServiceExt;

async fn get(path: &str) -> (StatusCode, Option<String>, String) {
    let state = Arc::new(AppState::new(ui::embedded()));
    state.set_ready();
    let request = match Request::builder().uri(path).body(Body::empty()) {
        Ok(request) => request,
        Err(err) => panic!("building a request for {path}: {err}"),
    };
    let response = match router(state).oneshot(request).await {
        Ok(response) => response,
        Err(err) => panic!("the router is infallible: {err}"),
    };
    let status = response.status();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let body = match response.into_body().collect().await {
        Ok(collected) => String::from_utf8_lossy(&collected.to_bytes()).into_owned(),
        Err(err) => panic!("reading {path}: {err}"),
    };
    (status, content_type, body)
}

#[tokio::test]
async fn the_embedded_app_is_served_at_the_root() {
    let (status, content_type, body) = get("/").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type.as_deref(), Some("text/html; charset=utf-8"));
    assert!(
        body.contains("<title>tidepool</title>"),
        "not the app's index.html: {body}"
    );
}

#[tokio::test]
async fn everything_the_index_references_is_embedded() {
    let (_, _, index) = get("/").await;
    let referenced: Vec<&str> = index
        .split(['"', '\''])
        .filter(|s| s.starts_with("/assets/"))
        .collect();
    assert!(
        referenced.iter().any(|s| std::path::Path::new(s)
            .extension()
            .is_some_and(|e| e == "wasm")),
        "the index should reference the wasm bundle: {index}"
    );
    for path in referenced {
        let (status, _, _) = get(path).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{path} is referenced but not embedded"
        );
    }
}
