//! The wire vocabulary of Tidepool: request and response types, and the table
//! of HTTP endpoints the server is built from.
//!
//! This crate is Apache-2.0 (see licensing.md) because anything talking to
//! Tidepool needs it, starting with Kayak's `tidepool` output. It carries no
//! engine, no runtime and no network code, only types and descriptions.

pub mod api_docs;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// What `GET /v1/info` answers: which server this is.
///
/// The UI shows it in its footer, and a client can check `api_version` before
/// it relies on anything else.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ServerInfo {
    /// Always `"tidepool"`.
    pub name: String,
    /// The server's crate version, e.g. `0.0.1`.
    pub version: String,
    /// The version of the HTTP API this server speaks. Bumped on breaking
    /// changes to anything under `/v1`; currently `1`.
    pub api_version: u32,
}

/// What the probes (`GET /healthz`, `GET /readyz`) answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProbeStatus {
    pub status: ProbeState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProbeState {
    /// The process is up and serving HTTP.
    Ok,
    /// Ready to take traffic: config applied, data recovered.
    Ready,
    /// Up, but not ready yet (still starting, or recovering).
    Starting,
}
