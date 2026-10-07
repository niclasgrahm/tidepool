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
use tidepool_server::{AppState, Assets, Tokens, router};
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
    let response = match router(state).oneshot(request).await {
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
