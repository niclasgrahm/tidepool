//! Shared by the server's integration tests: models, server state, and
//! requests driven in process through `tower::ServiceExt::oneshot`.

#![allow(dead_code)] // each test file uses a different part

use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tidepool_config::ConfigSet;
use tidepool_model::{Catalog, Model, compile};
use tidepool_server::{AppState, Assets, Listener, Tokens, router, router_on};
use tidepool_store::WriterConfig;
use tower::ServiceExt;

pub struct Reply {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn json(&self) -> Value {
        match serde_json::from_slice(&self.body) {
            Ok(value) => value,
            Err(err) => panic!(
                "body should be JSON ({err}): {}",
                String::from_utf8_lossy(&self.body)
            ),
        }
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn header(&self, name: &str) -> Option<String> {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(String::from)
    }
}

/// The retail example's model.
pub fn retail() -> Arc<Model> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/retail");
    let (set, _) = match ConfigSet::load_dir(&dir) {
        Ok(loaded) => loaded,
        Err(err) => panic!("reading the example: {err}"),
    };
    match compile(&set, &Catalog::default()) {
        (Some(compiled), _) => Arc::new(compiled.model),
        (None, d) => panic!("the example should compile: {d:?}"),
    }
}

/// The retail example's model with one edit: `from` replaced by `to` in
/// the file at `path` (relative to the example).
pub fn retail_with(path: &str, from: &str, to: &str) -> Arc<Model> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/retail");
    let mut files = Vec::new();
    for sub in ["", "tables", "dashboards"] {
        let entries = match std::fs::read_dir(dir.join(sub)) {
            Ok(entries) => entries,
            Err(err) => panic!("reading the example: {err}"),
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let relative = if sub.is_empty() {
                name
            } else {
                format!("{sub}/{name}")
            };
            if !std::path::Path::new(&relative)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("yaml"))
            {
                continue;
            }
            let mut text = std::fs::read_to_string(entry.path()).unwrap_or_default();
            if relative == path {
                assert!(text.contains(from), "{path} has no `{from}` to replace");
                text = text.replace(from, to);
            }
            files.push((relative, text));
        }
    }
    let (set, _) = ConfigSet::from_files(files);
    match compile(&set, &Catalog::default()) {
        (Some(compiled), _) => Arc::new(compiled.model),
        (None, d) => panic!("the edited example should compile: {d:?}"),
    }
}

/// A model with no tables.
pub fn empty() -> Arc<Model> {
    match compile(&ConfigSet::default(), &Catalog::default()) {
        (Some(compiled), _) => Arc::new(compiled.model),
        (None, d) => panic!("the empty project should compile: {d:?}"),
    }
}

/// A ready server for `model`. Epochs are frequent so `?wait=visible`
/// answers quickly; tests never depend on a write *not* being visible yet.
pub fn state(model: Arc<Model>, tokens: Tokens, ui: Option<Arc<dyn Assets>>) -> Arc<AppState> {
    let config = WriterConfig {
        epoch_interval: std::time::Duration::from_millis(10),
        ..WriterConfig::default()
    };
    let state = match AppState::new(model, config, ui, tokens) {
        Ok(state) => Arc::new(state),
        Err(err) => panic!("starting the writer: {err}"),
    };
    state.set_ready();
    state
}

/// The retail model, no tokens, no UI.
pub fn ready_state() -> Arc<AppState> {
    state(retail(), Tokens::default(), None)
}

pub fn request(
    method: Method,
    path: &str,
    headers: &[(&str, &str)],
    body: impl Into<Body>,
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(path);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    match builder.body(body.into()) {
        Ok(request) => request,
        Err(err) => panic!("building a request for {path}: {err}"),
    }
}

pub async fn send(state: Arc<AppState>, request: Request<Body>) -> Reply {
    send_on(state, Listener::ALL, request).await
}

/// `request`, come in on `listener`.
pub async fn send_on(state: Arc<AppState>, listener: Listener, request: Request<Body>) -> Reply {
    let app = if listener == Listener::ALL {
        router(state)
    } else {
        router_on(state, listener)
    };
    let response = match app.oneshot(request).await {
        Ok(response) => response,
        Err(err) => panic!("the router is infallible: {err}"),
    };
    let status = response.status();
    let headers = response.headers().clone();
    let body = match response.into_body().collect().await {
        Ok(collected) => collected.to_bytes().to_vec(),
        Err(err) => panic!("reading the body: {err}"),
    };
    Reply {
        status,
        headers,
        body,
    }
}

pub async fn call(state: Arc<AppState>, method: Method, path: &str) -> Reply {
    send(state, request(method, path, &[], Body::empty())).await
}
