//! The wire vocabulary of Tidepool: request and response types, and the table
//! of HTTP endpoints the server is built from.
//!
//! This crate is Apache-2.0 (see licensing.md) because anything talking to
//! Tidepool needs it, starting with Kayak's `tidepool` output. It carries no
//! engine, no runtime and no network code, only types and descriptions.

pub mod api_docs;
pub mod config;
pub mod live;

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

/// What a successful write answers (`POST`/`DELETE /v1/tables/{table}/rows`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct IngestResponse {
    /// Rows written.
    pub written: u64,
    /// Existing rows replaced (upserts) or deleted.
    pub replaced: u64,
    /// Keys that matched no row (deletes only).
    pub missing: u64,
    /// The epoch from which the change is visible to queries. With
    /// `?wait=visible` it already is when the response arrives.
    pub epoch: u64,
}

/// A rejected write: nothing was written.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct IngestError {
    /// One line: the problem, or how many there are.
    pub error: String,
    /// The problems, by row and column (at most 100).
    pub errors: Vec<RowError>,
    /// Problems beyond the ones listed.
    pub more: u64,
}

/// One problem with a write.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RowError {
    /// The row's position in the batch, from 0; absent for problems with the
    /// body as a whole (or, for Arrow, a whole column).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<String>,
    pub message: String,
}

/// A table, as listed by `GET /v1/tables`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TableInfo {
    pub name: String,
    /// `append` or `upsert`.
    pub mode: String,
    pub primary_key: Vec<String>,
    pub columns: Vec<ColumnInfo>,
    /// Rows currently stored (not counting deleted ones).
    pub rows: u64,
    /// Rows written since the server started.
    pub written_total: u64,
    /// Write rate over the last ten seconds.
    pub rows_per_second: f64,
    /// Approximate memory held by the table's segments.
    pub bytes: u64,
    /// The epoch these numbers are from.
    pub epoch: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ColumnInfo {
    pub name: String,
    /// The column type as written in the table file, e.g. `decimal(14,2)`.
    #[serde(rename = "type")]
    pub column_type: String,
    pub nullable: bool,
    /// `dict` or `plain`.
    pub encoding: String,
}

/// The newest rows of a table (`GET /v1/tables/{table}/preview`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Preview {
    pub columns: Vec<String>,
    /// Newest first; values as JSON (decimals, dates and timestamps as strings).
    pub rows: Vec<Vec<serde_json::Value>>,
    pub epoch: u64,
}

/// A query: metrics grouped by dimensions, under a selection. The same
/// engine dashboards use, without a subscription.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QueryRequest {
    /// Metrics by name, as in the model.
    pub metrics: Vec<String>,
    /// Fields to group by, in result order.
    #[serde(default)]
    pub dimensions: Vec<QueryDimension>,
    /// The selection: conditions on fields. A metric is filtered by the
    /// fields its table reaches; the others don't apply to it.
    #[serde(default)]
    pub filters: Vec<QueryFilter>,
    /// Result columns to sort by. Default: the dimensions, ascending.
    #[serde(default)]
    pub sort: Vec<QuerySort>,
    /// At most this many rows.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// A field to group by.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QueryDimension {
    /// `dataset.field`.
    pub field: String,
    /// For time fields: `day`, `week`, `month`, `quarter` or `year`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grain: Option<String>,
    /// For hierarchy fields: group by each node's ancestor at this depth
    /// (0 is the roots); nodes not that deep group as null.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<u32>,
}

/// A condition on a field: `in`, a half-open range `from <= value < to`
/// (either end may be left out), or a window of time `relative` to now
/// (`{"unit": "day", "last": 7}`). Values are written as in ingested rows:
/// decimals, dates and timestamps as strings or numbers.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QueryFilter {
    /// `dataset.field`.
    pub field: String,
    #[serde(default, rename = "in", skip_serializing_if = "Option::is_none")]
    pub values: Option<Vec<serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relative: Option<live::RelativeWindow>,
}

/// A result column to sort by, by its name in [`QueryResult::columns`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QuerySort {
    pub column: String,
    #[serde(default)]
    pub desc: bool,
}

/// A query's answer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct QueryResult {
    /// Dimensions first, then metrics, in request order.
    pub columns: Vec<QueryColumn>,
    /// Values as JSON: decimals, dates and timestamps as strings, so nothing
    /// is lost on the way through.
    pub rows: Vec<Vec<serde_json::Value>>,
    /// The data epoch the answer was computed from.
    pub epoch: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct QueryColumn {
    /// `dataset.field` (with `:grain`) for dimensions, the metric's name for
    /// metrics.
    pub name: String,
    /// `bool`, `int`, `float`, `decimal(s)`, `string`, `date`, `timestamp`
    /// or `timestamptz`.
    #[serde(rename = "type")]
    pub ty: String,
    /// `dimension` or `metric`.
    pub role: String,
}

/// The semantic model as the UI shows it (`GET /v1/model`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelInfo {
    pub datasets: Vec<DatasetInfo>,
    pub relationships: Vec<RelationshipInfo>,
    pub metrics: Vec<MetricInfo>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DatasetInfo {
    pub name: String,
    /// The table it reads.
    pub table: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub fields: Vec<FieldInfo>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FieldInfo {
    pub name: String,
    /// `string`, `int`, `decimal(2)`, `date`, …
    #[serde(rename = "type")]
    pub ty: String,
    /// The expression, for a derived field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expression: Option<String>,
    pub is_time: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// For a hierarchy field: the columns its tree is made of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hierarchy: Option<HierarchyInfo>,
}

/// A hierarchy field's columns (plan §5.6).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HierarchyInfo {
    /// The key: a node's identity, and the field's values.
    pub key: String,
    /// The parent's key.
    pub parent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RelationshipInfo {
    pub name: String,
    /// The many side.
    pub from: String,
    pub from_columns: Vec<String>,
    /// The one side.
    pub to: String,
    pub to_columns: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MetricInfo {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The SQL expression.
    pub expression: String,
    /// `base` (aggregates rows) or `derived` (combines other metrics).
    pub kind: String,
    /// The table whose rows a base metric aggregates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<String>,
    /// Fields whose selection it ignores (`*` for all).
    pub ignores: Vec<String>,
    /// E.g. `-1 year on orders.ordered_at`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_offset: Option<String>,
    /// Dashboards that show it.
    pub dashboards: Vec<String>,
}

/// What the server is doing (`GET /v1/status`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct StatusInfo {
    pub epoch: u64,
    pub uptime_seconds: u64,
    pub tables: Vec<TableStatus>,
    /// Memory held by dictionary values.
    pub dictionary_bytes: u64,
    /// Memory held by the engine's caches (join columns, kept answers).
    pub cache_bytes: u64,
    /// Live dashboard sessions.
    pub sessions: usize,
    /// `None` when data lives in memory only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub durability: Option<DurabilityStatus>,
    /// The config version the server runs.
    #[serde(default)]
    pub config_version: u64,
    /// A model change being rebuilt in the background.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rebuild: Option<config::RebuildInfo>,
    /// The last epoch's publish, in milliseconds.
    #[serde(default)]
    pub epoch_publish_ms: f64,
    /// The last refresh of every live session after an epoch, in
    /// milliseconds.
    #[serde(default)]
    pub live_refresh_ms: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TableStatus {
    pub name: String,
    pub rows: u64,
    pub bytes: u64,
    pub segments: usize,
    pub rows_per_second: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DurabilityStatus {
    pub data_dir: String,
    /// `fsync` or `async`.
    pub mode: String,
    /// The last write logged.
    pub last_lsn: u64,
    /// The last write the newest checkpoint holds.
    pub checkpoint_lsn: u64,
    /// Seconds since that checkpoint was written (`None`: none yet).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint_age_seconds: Option<u64>,
}

/// What a request may do: `GET`/`POST /v1/auth/session`. A level that
/// needs no token is `true` for everyone.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionInfo {
    pub read: bool,
    pub ingest: bool,
    pub admin: bool,
}
