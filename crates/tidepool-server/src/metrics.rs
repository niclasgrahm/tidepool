//! `GET /metrics`: the server in the Prometheus text format (plan §9, Ops).
//!
//! Gauges are read from the newest snapshot and the writer when scraped;
//! the counters below are kept as requests come in. Names follow
//! Prometheus conventions: `tidepool_` prefix, `_total` for counters,
//! `_seconds` and `_bytes` units.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use tidepool_store::Timing;

use crate::AppState;

/// What happened since the server started.
#[derive(Default)]
pub struct Counters {
    /// Ingest requests by (table, op, status).
    requests: Mutex<BTreeMap<(String, &'static str, u16), u64>>,
    /// Rows written (or deleted) by table.
    rows: Mutex<BTreeMap<String, u64>>,
    /// `POST /v1/query`.
    pub queries: Timing,
    pub query_errors: AtomicU64,
    /// Refreshing every live session after an epoch.
    pub refresh: Timing,
    /// MCP tool calls by tool.
    tools: Mutex<BTreeMap<String, u64>>,
}

impl Counters {
    pub fn request(&self, table: &str, op: &'static str, status: StatusCode) {
        *self
            .requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry((table.to_string(), op, status.as_u16()))
            .or_default() += 1;
    }

    pub fn rows(&self, table: &str, rows: u64) {
        *self
            .rows
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(table.to_string())
            .or_default() += rows;
    }

    pub fn tool(&self, tool: &str) {
        *self
            .tools
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(tool.to_string())
            .or_default() += 1;
    }
}

/// A label value, escaped as the format wants.
fn label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// Writes metric families: `# HELP`, `# TYPE`, then the samples.
struct Out(String);

impl Out {
    fn family(&mut self, name: &str, kind: &str, help: &str) {
        let _ = writeln!(self.0, "# HELP {name} {help}\n# TYPE {name} {kind}");
    }

    fn sample(&mut self, name: &str, labels: &[(&str, &str)], value: f64) {
        self.0.push_str(name);
        if !labels.is_empty() {
            let inner: Vec<String> = labels
                .iter()
                .map(|(k, v)| format!("{k}=\"{}\"", label(v)))
                .collect();
            let _ = write!(self.0, "{{{}}}", inner.join(","));
        }
        let _ = writeln!(self.0, " {value}");
    }

    fn one(&mut self, name: &str, kind: &str, help: &str, value: f64) {
        self.family(name, kind, help);
        self.sample(name, &[], value);
    }

    fn timing(&mut self, name: &str, help: &str, timing: &Timing) {
        self.family(name, "summary", help);
        self.sample(&format!("{name}_sum"), &[], timing.total_seconds());
        #[allow(clippy::cast_precision_loss)]
        self.sample(&format!("{name}_count"), &[], timing.count() as f64);
    }
}

/// The exposition, as of now.
#[must_use]
#[allow(clippy::cast_precision_loss, clippy::too_many_lines)] // one family after another
pub fn render(state: &AppState) -> String {
    let snapshot = state.writer.snapshot();
    let mut out = Out(String::new());
    out.family("tidepool_build_info", "gauge", "The running version.");
    out.sample(
        "tidepool_build_info",
        &[("version", env!("CARGO_PKG_VERSION"))],
        1.0,
    );
    out.one(
        "tidepool_uptime_seconds",
        "gauge",
        "Seconds since the server started.",
        state.started.elapsed().as_secs_f64(),
    );
    out.one(
        "tidepool_epoch",
        "gauge",
        "The newest published epoch.",
        snapshot.epoch as f64,
    );
    out.one(
        "tidepool_config_version",
        "gauge",
        "The config version the server runs.",
        snapshot.version as f64,
    );

    let mut tables: Vec<(&str, &tidepool_store::TableSnapshot)> = snapshot
        .model
        .tables
        .iter()
        .filter_map(|t| snapshot.table(t.id).map(|s| (t.name.as_str(), s)))
        .collect();
    tables.sort_by_key(|(name, _)| *name);
    out.family("tidepool_table_rows", "gauge", "Live rows per table.");
    for (name, table) in &tables {
        out.sample(
            "tidepool_table_rows",
            &[("table", name)],
            table.live_rows as f64,
        );
    }
    out.family(
        "tidepool_table_bytes",
        "gauge",
        "Memory held by each table's segments.",
    );
    for (name, table) in &tables {
        out.sample(
            "tidepool_table_bytes",
            &[("table", name)],
            table.bytes() as f64,
        );
    }
    out.family("tidepool_table_segments", "gauge", "Segments per table.");
    for (name, table) in &tables {
        out.sample(
            "tidepool_table_segments",
            &[("table", name)],
            table.segments.len() as f64,
        );
    }
    out.family(
        "tidepool_rows_written_total",
        "counter",
        "Rows written (or deleted) through the ingest API, per table.",
    );
    for (table, rows) in state
        .counters
        .rows
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
    {
        out.sample(
            "tidepool_rows_written_total",
            &[("table", table)],
            *rows as f64,
        );
    }
    out.family(
        "tidepool_ingest_requests_total",
        "counter",
        "Ingest requests, per table, operation and status.",
    );
    for ((table, op, status), n) in state
        .counters
        .requests
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
    {
        out.sample(
            "tidepool_ingest_requests_total",
            &[
                ("table", table),
                ("op", op),
                ("status", &status.to_string()),
            ],
            *n as f64,
        );
    }
    out.one(
        "tidepool_dictionary_bytes",
        "gauge",
        "Memory held by dictionary values.",
        snapshot
            .dicts
            .values()
            .map(|d| d.value_bytes() as f64)
            .sum(),
    );
    out.one(
        "tidepool_cache_bytes",
        "gauge",
        "Memory held by the query engine's caches.",
        state.engine.cache_bytes() as f64,
    );
    out.one(
        "tidepool_sessions",
        "gauge",
        "Live dashboard sessions.",
        state.hub.live_sessions() as f64,
    );
    out.timing(
        "tidepool_epoch_publish_seconds",
        "Time to publish an epoch: freezing new rows, sealing, snapshots.",
        state.writer.publishing(),
    );
    out.timing(
        "tidepool_live_refresh_seconds",
        "Time to bring every live session up to date after an epoch.",
        &state.counters.refresh,
    );
    out.timing(
        "tidepool_query_seconds",
        "Time to answer POST /v1/query.",
        &state.counters.queries,
    );
    out.one(
        "tidepool_query_errors_total",
        "counter",
        "Queries refused (a 400).",
        state.counters.query_errors.load(Ordering::Relaxed) as f64,
    );
    out.family(
        "tidepool_mcp_tool_calls_total",
        "counter",
        "MCP tool calls, per tool.",
    );
    for (tool, n) in state
        .counters
        .tools
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
    {
        out.sample(
            "tidepool_mcp_tool_calls_total",
            &[("tool", tool)],
            *n as f64,
        );
    }
    if let Some((_, done, rows)) = state.writer.rebuild() {
        out.one(
            "tidepool_rebuild_rows",
            "gauge",
            "Rows a running model change rewrites.",
            rows as f64,
        );
        out.one(
            "tidepool_rebuild_rows_done",
            "gauge",
            "Rows rewritten so far.",
            done as f64,
        );
    }
    if let Some(log) = state.writer.log() {
        out.one(
            "tidepool_wal_last_lsn",
            "gauge",
            "The last write logged.",
            log.last_lsn.load(Ordering::Relaxed) as f64,
        );
        out.one(
            "tidepool_checkpoint_lsn",
            "gauge",
            "The last write the newest checkpoint holds.",
            log.checkpoint_lsn.load(Ordering::Relaxed) as f64,
        );
        if let Some(age) = log
            .checkpoint_at
            .lock()
            .ok()
            .and_then(|at| at.map(|at| at.elapsed().as_secs_f64()))
        {
            out.one(
                "tidepool_checkpoint_age_seconds",
                "gauge",
                "Seconds since the newest checkpoint was written.",
                age,
            );
        }
    }
    out.0
}

/// `GET /metrics`. See `Operation::Metrics` in the endpoint table.
pub async fn metrics(State(state): State<Arc<AppState>>) -> Response {
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        render(&state),
    )
        .into_response()
}
