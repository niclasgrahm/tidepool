//! Request handlers. Each documents itself in the endpoint table
//! (`tidepool_protocol::api_docs::endpoints`), not here.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::Value;
use tidepool_protocol::{ProbeState, ProbeStatus, ServerInfo};

use crate::AppState;

/// The HTTP API version, as reported by `GET /v1/info`.
pub const API_VERSION: u32 = 1;

/// `GET /healthz`
pub async fn health() -> Json<ProbeStatus> {
    Json(ProbeStatus {
        status: ProbeState::Ok,
    })
}

/// `GET /readyz`
pub async fn ready(State(state): State<Arc<AppState>>) -> (StatusCode, Json<ProbeStatus>) {
    if state.is_ready() {
        (
            StatusCode::OK,
            Json(ProbeStatus {
                status: ProbeState::Ready,
            }),
        )
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ProbeStatus {
                status: ProbeState::Starting,
            }),
        )
    }
}

/// `GET /v1/info`
pub async fn info() -> Json<ServerInfo> {
    Json(ServerInfo {
        name: "tidepool".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        api_version: API_VERSION,
    })
}

/// `GET /v1/openapi.json`
pub async fn openapi() -> Json<Value> {
    Json(crate::openapi::document())
}
