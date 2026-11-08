//! Serving the web UI: a static single-page app, either compiled into the
//! binary (`embed-assets`) or read from a directory (`--ui-dir`, for
//! development).
//!
//! The UI is whatever the frontend build leaves in `frontend/dist`: an
//! `index.html` plus assets. Every path the API doesn't claim falls through to
//! [`serve`]:
//!
//! - `/v1/...` that matched no endpoint is a JSON 404, never the app. A typo in
//!   an API call should not come back as 200 and a page of HTML.
//! - A file that exists is served with its content type.
//! - A path with no extension that isn't a file is a client-side route
//!   (`/dashboards/sales`), so it gets `index.html` and the app routes it.
//! - Anything else is a 404.

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::http::{HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use tidepool_protocol::api_docs::ApiError;

use crate::AppState;

/// A source of UI files, keyed by path relative to the UI root
/// (`index.html`, `assets/app-3f2a.js`).
pub trait Assets: Send + Sync {
    fn get(&self, key: &str) -> Option<Cow<'static, [u8]>>;
}

/// UI files read from a directory on every request. For development, where
/// the frontend is rebuilt while the server keeps running.
pub struct DirAssets {
    root: PathBuf,
}

impl DirAssets {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl Assets for DirAssets {
    fn get(&self, key: &str) -> Option<Cow<'static, [u8]>> {
        // `key` has been through `key_for`, so it can't climb out of `root`
        let path = self.root.join(key);
        if !path.is_file() {
            return None;
        }
        std::fs::read(path).ok().map(Cow::Owned)
    }
}

#[cfg(feature = "embed-assets")]
#[derive(rust_embed::Embed)]
#[folder = "../../frontend/dist"]
struct Dist;

#[cfg(feature = "embed-assets")]
struct EmbeddedAssets;

#[cfg(feature = "embed-assets")]
impl Assets for EmbeddedAssets {
    fn get(&self, key: &str) -> Option<Cow<'static, [u8]>> {
        Dist::get(key).map(|file| file.data)
    }
}

/// The UI compiled into this binary, if it was built with `embed-assets`.
#[must_use]
pub fn embedded() -> Option<Arc<dyn Assets>> {
    #[cfg(feature = "embed-assets")]
    {
        Some(Arc::new(EmbeddedAssets))
    }
    #[cfg(not(feature = "embed-assets"))]
    {
        None
    }
}

/// The asset key for a request path, or `None` for a path that tries to leave
/// the UI root. `/` and directory paths mean their `index.html`.
#[must_use]
pub fn key_for(path: &str) -> Option<String> {
    let trimmed = path.strip_prefix('/').unwrap_or(path);
    let key = if trimmed.is_empty() || trimmed.ends_with('/') {
        format!("{trimmed}index.html")
    } else {
        trimmed.to_string()
    };
    let escapes = key.starts_with('/')
        || key.contains('\\')
        || key
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..");
    (!escapes).then_some(key)
}

/// The `Content-Type` for an asset, from its extension.
#[must_use]
pub fn content_type(key: &str) -> &'static str {
    let extension = key.rsplit_once('.').map_or("", |(_, ext)| ext);
    match extension {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "wasm" => "application/wasm",
        "json" | "map" | "webmanifest" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// The fallback for every request the API router didn't match.
pub fn serve(state: &AppState, method: &Method, uri: &Uri) -> Response {
    let path = uri.path();
    if path == "/v1" || path.starts_with("/v1/") {
        return not_found(format!("no endpoint {method} {path}")).into_response();
    }
    if method != Method::GET && method != Method::HEAD {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let Some(key) = key_for(path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(assets) = state.ui() else {
        return placeholder().into_response();
    };

    if let Some(bytes) = assets.get(&key) {
        return file(&key, bytes);
    }
    // A path without an extension is a route inside the app, not a file.
    let is_client_route = !key.rsplit('/').next().unwrap_or("").contains('.');
    if is_client_route && let Some(index) = assets.get("index.html") {
        return file("index.html", index);
    }
    StatusCode::NOT_FOUND.into_response()
}

fn file(key: &str, bytes: Cow<'static, [u8]>) -> Response {
    // Bundlers put content hashes in everything under `assets/`, so those can
    // be cached forever. `index.html` must be revalidated, or a browser would
    // keep loading the previous build's hashed files after an upgrade.
    let cache = if key.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let mut response = Response::new(Body::from(bytes));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(content_type(key)),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    // the pages may only be framed by the server's own: clickjacking is
    // a click on someone's dashboard, under someone else's page
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("frame-ancestors 'self'"),
    );
    // the hashed files are the same for everyone, and a view shown inside
    // an AI app loads them from a page of another origin (an MCP App)
    if key.starts_with("assets/") {
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            HeaderValue::from_static("*"),
        );
    }
    response
}

fn not_found(error: String) -> (StatusCode, Json<ApiError>) {
    (StatusCode::NOT_FOUND, Json(ApiError { error }))
}

/// What `/` shows on a binary built without the UI and started without
/// `--ui-dir`: enough to say the server is fine and where the API is.
fn placeholder() -> (
    StatusCode,
    [(header::HeaderName, &'static str); 1],
    &'static str,
) {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>tidepool</title></head>\
         <body><h1>tidepool is running</h1>\
         <p>This binary was built without the web UI. Build it with <code>just build</code>, \
         or start the server with <code>--ui-dir frontend/dist</code>.</p>\
         <p>The API is described at <a href=\"/v1/openapi.json\">/v1/openapi.json</a>.</p>\
         </body></html>",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_and_directories_mean_their_index() {
        assert_eq!(key_for("/").as_deref(), Some("index.html"));
        assert_eq!(key_for("/docs/").as_deref(), Some("docs/index.html"));
        assert_eq!(key_for("/assets/app.js").as_deref(), Some("assets/app.js"));
    }

    #[test]
    fn paths_that_leave_the_root_have_no_key() {
        for path in [
            "/../Cargo.toml",
            "/assets/../../secret",
            "//etc/passwd",
            "/a\\b",
            "/./x",
        ] {
            assert_eq!(key_for(path), None, "{path}");
        }
    }

    #[test]
    fn content_types_follow_the_extension() {
        assert_eq!(content_type("index.html"), "text/html; charset=utf-8");
        assert_eq!(content_type("assets/app.wasm"), "application/wasm");
        assert_eq!(content_type("README"), "application/octet-stream");
    }
}
