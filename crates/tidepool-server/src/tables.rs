//! The tables endpoints: writing rows, deleting by key, and looking at what
//! a table holds. Documented in the endpoint table, like every handler.

use std::sync::Arc;
use std::time::Instant;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use tidepool_config::diag::suggest;
use tidepool_model::ir::Table;
use tidepool_protocol::api_docs::ApiError;
use tidepool_protocol::{ColumnInfo, IngestError, IngestResponse, Preview, RowError, TableInfo};
use tidepool_store::batch::{self, Shape};
use tidepool_store::{Op, Rejected, Snapshot};

use crate::AppState;
use crate::idempotency::Begin;

/// Largest ingest body accepted.
pub const MAX_BODY: usize = 256 * 1024 * 1024;

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(ApiError {
            error: message.into(),
        }),
    )
        .into_response()
}

/// The table named `name` in the snapshot's model, or the 404 to answer.
fn find<'a>(snapshot: &'a Snapshot, name: &str) -> Result<&'a Table, Box<Response>> {
    snapshot.model.table_named(name).ok_or_else(|| {
        let names = snapshot.model.tables.iter().map(|t| t.name.as_str());
        Box::new(error(
            StatusCode::NOT_FOUND,
            format!("no table `{name}`; {}", suggest("tables", name, names)),
        ))
    })
}

#[derive(Debug, Deserialize)]
pub struct WriteQuery {
    wait: Option<String>,
}

#[derive(Clone, Copy)]
enum Format {
    Json,
    Ndjson,
    Arrow,
}

fn format_of(headers: &HeaderMap) -> Result<Format, Box<Response>> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
        });
    match content_type.as_deref() {
        None | Some("application/json") => Ok(Format::Json),
        Some("application/x-ndjson" | "application/jsonl" | "application/ndjson") => {
            Ok(Format::Ndjson)
        }
        Some("application/vnd.apache.arrow.stream") => Ok(Format::Arrow),
        Some(other) => Err(Box::new(error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            format!(
                "`{other}` isn't an ingest format; use application/json, application/x-ndjson or application/vnd.apache.arrow.stream"
            ),
        ))),
    }
}

fn rejected(rejected: &Rejected) -> Response {
    let body = IngestError {
        error: rejected.summary(),
        errors: rejected
            .errors
            .iter()
            .map(|e| RowError {
                row: e.row.map(|r| r as u64),
                column: e.column.clone(),
                message: e.message.clone(),
            })
            .collect(),
        more: rejected.more as u64,
    };
    (StatusCode::BAD_REQUEST, Json(body)).into_response()
}

/// `POST /v1/tables/{table}/rows`
pub async fn ingest_rows(
    State(state): State<Arc<AppState>>,
    Path(table): Path<String>,
    Query(query): Query<WriteQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    counted(&state, &table, Op::Write, &query, &headers, body).await
}

/// `PUT /v1/tables/{table}/rows`
pub async fn replace_rows(
    State(state): State<Arc<AppState>>,
    Path(table): Path<String>,
    Query(query): Query<WriteQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    counted(&state, &table, Op::Replace, &query, &headers, body).await
}

/// `DELETE /v1/tables/{table}/rows`
pub async fn delete_rows(
    State(state): State<Arc<AppState>>,
    Path(table): Path<String>,
    Query(query): Query<WriteQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    counted(&state, &table, Op::Delete, &query, &headers, body).await
}

async fn write(
    state: &AppState,
    name: &str,
    op: Op,
    query: &WriteQuery,
    headers: &HeaderMap,
    body: Bytes,
) -> Response {
    let (table, format, wait) = match prepare(state, name, op, query, headers) {
        Ok(prepared) => prepared,
        Err(response) => return *response,
    };
    let batch = match decode(&table, op, format, body).await {
        Ok(batch) => batch,
        Err(response) => return *response,
    };
    let key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    commit(state, &table, op, batch, wait, key).await
}

/// [`write`], counted for `/metrics`.
async fn counted(
    state: &AppState,
    name: &str,
    op: Op,
    query: &WriteQuery,
    headers: &HeaderMap,
    body: Bytes,
) -> Response {
    let response = write(state, name, op, query, headers, body).await;
    let label = match op {
        Op::Write => "write",
        Op::Delete => "delete",
        Op::Replace => "replace",
    };
    state.counters.request(name, label, response.status());
    response
}

/// Everything about the request that can be checked before reading the body:
/// the table, the format, and whether to wait.
fn prepare(
    state: &AppState,
    name: &str,
    op: Op,
    query: &WriteQuery,
    headers: &HeaderMap,
) -> Result<(Table, Format, bool), Box<Response>> {
    let snapshot = state.writer.snapshot();
    let table = find(&snapshot, name)?.clone();
    if op != Op::Write && table.mode != tidepool_config::table::TableMode::Upsert {
        let message = if op == Op::Delete {
            format!("`{name}` is an append table; only upsert tables have keys to delete by")
        } else {
            format!("`{name}` is an append table; only upsert tables have keys to replace by")
        };
        return Err(Box::new(rejected(&batch::rejected(message))));
    }
    let format = format_of(headers)?;
    let wait = match query.wait.as_deref() {
        None => false,
        Some("visible") => true,
        Some(other) => {
            return Err(Box::new(error(
                StatusCode::BAD_REQUEST,
                format!("`wait={other}`: the only value is `visible`"),
            )));
        }
    };
    Ok((table, format, wait))
}

/// Decode the body, off the async threads: it is CPU work proportional to
/// its size.
async fn decode(
    table: &Table,
    op: Op,
    format: Format,
    body: Bytes,
) -> Result<tidepool_store::Batch, Box<Response>> {
    let shape = if op == Op::Delete {
        Shape::Keys
    } else {
        Shape::Rows
    };
    let table = table.clone();
    let decoded = tokio::task::spawn_blocking(move || match format {
        Format::Json => batch::json_array(&table, shape, &body),
        Format::Ndjson => batch::ndjson(&table, shape, &body),
        Format::Arrow => batch::arrow(&table, shape, &body),
    })
    .await;
    match decoded {
        Ok(Ok(batch)) => Ok(batch),
        Ok(Err(problems)) => Err(Box::new(rejected(&problems))),
        Err(_) => Err(Box::new(error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "decoding the body failed",
        ))),
    }
}

/// Hand the batch to the writer, honouring the idempotency key.
async fn commit(
    state: &AppState,
    table: &Table,
    op: Op,
    batch: tidepool_store::Batch,
    wait: bool,
    key: Option<String>,
) -> Response {
    if let Some(key) = &key {
        match state.keys.begin(table.id, key, Instant::now()) {
            Begin::Fresh => {}
            Begin::Done(response) => return (StatusCode::OK, Json(response)).into_response(),
            Begin::InFlight => {
                return error(
                    StatusCode::CONFLICT,
                    "a request with this Idempotency-Key is still being processed",
                );
            }
        }
    }
    let abandon = || {
        if let Some(key) = &key {
            state.keys.abandon(table.id, key);
        }
    };

    let Ok(receipt) = state.writer.apply(table.id, op, batch, key.clone()) else {
        abandon();
        let mut response = error(
            StatusCode::SERVICE_UNAVAILABLE,
            "the write queue is full; retry shortly",
        );
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
        return response;
    };
    let Ok(receipt) = receipt.await else {
        abandon();
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "the server is shutting down",
        );
    };
    if receipt.applied.table_gone {
        abandon();
        return error(
            StatusCode::CONFLICT,
            format!(
                "`{}` was removed by a config change while this request was on its way",
                table.name
            ),
        );
    }
    if wait && state.writer.visible(receipt.epoch).await.is_err() {
        abandon();
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "the server is shutting down",
        );
    }

    state.counters.rows(
        &table.name,
        (receipt.applied.written + receipt.applied.replaced) as u64,
    );
    let response = IngestResponse {
        written: receipt.applied.written as u64,
        replaced: receipt.applied.replaced as u64,
        missing: receipt.applied.missing as u64,
        epoch: receipt.epoch,
    };
    if let Some(key) = &key {
        state
            .keys
            .finish(table.id, key, response.clone(), Instant::now());
    }
    (StatusCode::OK, Json(response)).into_response()
}

fn info(snapshot: &Snapshot, table: &Table) -> TableInfo {
    let stored = snapshot.table(table.id);
    let name_of = |id| {
        table
            .column(id)
            .map_or("?", |c| c.name.as_str())
            .to_string()
    };
    TableInfo {
        name: table.name.clone(),
        mode: format!("{:?}", table.mode).to_lowercase(),
        primary_key: table.primary_key.iter().map(|id| name_of(*id)).collect(),
        columns: table
            .columns
            .iter()
            .map(|c| ColumnInfo {
                name: c.name.clone(),
                column_type: c.column_type.to_string(),
                nullable: c.nullable,
                encoding: format!("{:?}", c.encoding).to_lowercase(),
            })
            .collect(),
        rows: stored.map_or(0, |t| t.live_rows),
        written_total: stored.map_or(0, |t| t.written_total),
        rows_per_second: stored.map_or(0.0, |t| (t.rows_per_second * 10.0).round() / 10.0),
        bytes: stored.map_or(0, |t| t.bytes() as u64),
        epoch: snapshot.epoch,
    }
}

/// `GET /v1/tables`
pub async fn list_tables(State(state): State<Arc<AppState>>) -> Json<Vec<TableInfo>> {
    let snapshot = state.writer.snapshot();
    let mut tables: Vec<TableInfo> = snapshot
        .model
        .tables
        .iter()
        .map(|t| info(&snapshot, t))
        .collect();
    tables.sort_by(|a, b| a.name.cmp(&b.name));
    Json(tables)
}

/// `GET /v1/tables/{table}`
pub async fn get_table(State(state): State<Arc<AppState>>, Path(name): Path<String>) -> Response {
    let snapshot = state.writer.snapshot();
    match find(&snapshot, &name) {
        Ok(table) => Json(info(&snapshot, table)).into_response(),
        Err(response) => *response,
    }
}

#[derive(Debug, Deserialize)]
pub struct PreviewQuery {
    limit: Option<usize>,
}

/// `GET /v1/tables/{table}/preview`
pub async fn preview_table(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(query): Query<PreviewQuery>,
) -> Response {
    let snapshot = state.writer.snapshot();
    let table = match find(&snapshot, &name) {
        Ok(table) => table,
        Err(response) => return *response,
    };
    let limit = query.limit.unwrap_or(100).min(1000);
    let rows = snapshot
        .newest_rows(table.id, limit)
        .into_iter()
        .map(|row| {
            row.iter()
                .map(tidepool_store::value::Value::to_json)
                .collect()
        })
        .collect();
    Json(Preview {
        columns: table.columns.iter().map(|c| c.name.clone()).collect(),
        rows,
        epoch: snapshot.epoch,
    })
    .into_response()
}
