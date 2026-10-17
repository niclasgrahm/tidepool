//! The tables endpoints: writing rows, deleting by key, and looking at what
//! a table holds. Documented in the endpoint table, like every handler.
//!
//! A write goes: who writes it ([`Caller::writer`]: the credential, the
//! table and operation it may write, the listener), the browser origin, the
//! credential's limits (rate, body, rows, its share of the queue, values
//! new to the dictionaries), the columns the server fills, then the writer
//! with an idempotency key scoped to the credential (plan §9.2).

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::Deserialize;
use tidepool_config::diag::suggest;
use tidepool_config::server::IngestOperation;
use tidepool_config::table::TableMode;
use tidepool_model::ir::Table;
use tidepool_protocol::api_docs::ApiError;
use tidepool_protocol::{ColumnInfo, IngestError, IngestResponse, Preview, RowError, TableInfo};
use tidepool_store::batch::{self, Shape};
use tidepool_store::keys::Scoped;
use tidepool_store::{Op, Rejected, Snapshot};

use crate::AppState;
use crate::auth::{Caller, Denied, Listener, Writer};
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

/// A write request as it arrived.
struct Request<'a> {
    name: &'a str,
    op: Op,
    caller: &'a Caller,
    listener: Listener,
    query: &'a WriteQuery,
    headers: &'a HeaderMap,
}

/// `POST /v1/tables/{table}/rows`
pub async fn ingest_rows(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<Caller>,
    Extension(listener): Extension<Listener>,
    Path(table): Path<String>,
    Query(query): Query<WriteQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let request = Request {
        name: &table,
        op: Op::Write,
        caller: &caller,
        listener,
        query: &query,
        headers: &headers,
    };
    counted(&state, &request, body).await
}

/// `PUT /v1/tables/{table}/rows`
pub async fn replace_rows(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<Caller>,
    Extension(listener): Extension<Listener>,
    Path(table): Path<String>,
    Query(query): Query<WriteQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let request = Request {
        name: &table,
        op: Op::Replace,
        caller: &caller,
        listener,
        query: &query,
        headers: &headers,
    };
    counted(&state, &request, body).await
}

/// `DELETE /v1/tables/{table}/rows`
pub async fn delete_rows(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<Caller>,
    Extension(listener): Extension<Listener>,
    Path(table): Path<String>,
    Query(query): Query<WriteQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let request = Request {
        name: &table,
        op: Op::Delete,
        caller: &caller,
        listener,
        query: &query,
        headers: &headers,
    };
    counted(&state, &request, body).await
}

/// The operation a request is, for a credential's grants.
fn operation(op: Op, mode: TableMode) -> IngestOperation {
    match (op, mode) {
        (Op::Write, TableMode::Append) => IngestOperation::Append,
        (Op::Write, TableMode::Upsert) => IngestOperation::Upsert,
        (Op::Delete, _) => IngestOperation::Delete,
        (Op::Replace, _) => IngestOperation::Replace,
    }
}

/// Who writes this request's batch, or the response refusing it. Comes
/// before anything about the table is said, so the public listener tells
/// nobody about tables they can't write there.
fn writer_of(state: &AppState, request: &Request) -> Result<(Table, Writer), Box<Response>> {
    let snapshot = state.writer.snapshot();
    let Some(table) = snapshot.model.table_named(request.name) else {
        if request.listener.public_only() {
            return Err(Box::new(error(
                StatusCode::NOT_FOUND,
                format!("no table `{}`", request.name),
            )));
        }
        return Err(find(&snapshot, request.name)
            .err()
            .unwrap_or_else(|| Box::new(error(StatusCode::NOT_FOUND, "no such table"))));
    };
    let operation = operation(request.op, table.mode);
    match request
        .caller
        .writer(&state.tokens, request.listener, &table.name, operation)
    {
        Ok(writer) => Ok((table.clone(), writer)),
        Err(Denied::Hidden) => Err(Box::new(error(
            StatusCode::NOT_FOUND,
            format!("no table `{}`", request.name),
        ))),
        Err(Denied::NeedsToken) => Err(Box::new(crate::auth::unauthorized(false))),
        Err(Denied::Forbidden(message)) => Err(Box::new(error(StatusCode::FORBIDDEN, message))),
    }
}

/// The browser origin a request comes from, if it may write from there:
/// `Ok(None)` for a request that isn't from a browser elsewhere.
fn origin(writer: &Writer, headers: &HeaderMap) -> Result<Option<HeaderValue>, Box<Response>> {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return Ok(None);
    };
    let allowed = writer.credential().is_some_and(|c| {
        origin
            .to_str()
            .is_ok_and(|o| c.origins.iter().any(|allowed| allowed == o))
    });
    match writer.credential() {
        Some(_) if allowed => Ok(Some(origin.clone())),
        // a credential without origins doesn't say; the browser decides
        // (and without CORS headers it won't hand the response over)
        Some(c) if c.origins.is_empty() => Ok(None),
        None => Ok(None),
        Some(c) => Err(Box::new(error(
            StatusCode::FORBIDDEN,
            format!(
                "credential `{}` may not write from origin `{}`",
                c.name,
                origin.to_str().unwrap_or("?")
            ),
        ))),
    }
}

/// 429 or 413, with when to retry for a 429.
fn refused(
    state: &AppState,
    writer: &Writer,
    reason: &'static str,
    status: StatusCode,
    message: String,
    retry: Option<Duration>,
) -> Response {
    state.counters.refused(writer.name(), reason);
    let mut response = error(status, message);
    if let Some(retry) = retry {
        let seconds = retry.as_secs() + u64::from(retry.subsec_nanos() > 0);
        if let Ok(value) = HeaderValue::from_str(&seconds.max(1).to_string()) {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
    }
    response
}

/// The credential's limits that can be checked before decoding: its rate
/// and the body's size.
fn before_decoding(state: &AppState, writer: &Writer, body: &Bytes) -> Result<(), Box<Response>> {
    let Some(credential) = writer.credential() else {
        return Ok(());
    };
    let limits = credential.limits;
    if let Some(rate) = limits.rate
        && let Err(retry) = state
            .limiter
            .request(&credential.name, rate, Instant::now())
    {
        return Err(Box::new(refused(
            state,
            writer,
            "rate",
            StatusCode::TOO_MANY_REQUESTS,
            format!(
                "credential `{}` is over its rate of {rate}",
                credential.name
            ),
            Some(retry),
        )));
    }
    if let Some(max) = limits.body
        && body.len() as u64 > max.0
    {
        return Err(Box::new(refused(
            state,
            writer,
            "body",
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "the body is {} bytes; credential `{}` may send {max} at most",
                body.len(),
                credential.name
            ),
            None,
        )));
    }
    Ok(())
}

async fn write(state: &AppState, request: &Request<'_>, body: Bytes) -> Response {
    let (table, writer) = match writer_of(state, request) {
        Ok(found) => found,
        Err(response) => return *response,
    };
    let allow_origin = match origin(&writer, request.headers) {
        Ok(origin) => origin,
        Err(response) => return *response,
    };
    let mut response = match checked(state, request, &table, &writer, body).await {
        Ok(response) => response,
        Err(response) => *response,
    };
    if let Some(origin) = allow_origin {
        response
            .headers_mut()
            .insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
        response
            .headers_mut()
            .insert(header::VARY, HeaderValue::from_static("Origin"));
    }
    response
}

/// The write, from limits to the writer.
async fn checked(
    state: &AppState,
    request: &Request<'_>,
    table: &Table,
    writer: &Writer,
    body: Bytes,
) -> Result<Response, Box<Response>> {
    let (format, wait) = prepare(table, request)?;
    before_decoding(state, writer, &body)?;
    let mut batch = decode(table, request.op, format, body).await?;
    let limits = writer.credential().map(|c| c.limits).unwrap_or_default();
    if let Some(max) = limits.batch_rows
        && batch.rows as u64 > max
    {
        return Err(Box::new(refused(
            state,
            writer,
            "batch_rows",
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "the batch has {} rows; credential `{}` may send {max} at most",
                batch.rows,
                writer.name()
            ),
            None,
        )));
    }
    if request.op != Op::Delete {
        let received = chrono::Utc::now().timestamp_micros();
        batch::fill(table, &mut batch, writer.name(), received);
    }
    let key = request
        .headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(|key| Scoped {
            credential: writer.name().to_string(),
            key: key.to_string(),
        });
    Ok(commit(state, table, writer, request.op, batch, wait, key).await)
}

/// [`write`], counted for `/metrics`.
async fn counted(state: &AppState, request: &Request<'_>, body: Bytes) -> Response {
    let response = write(state, request, body).await;
    let label = match request.op {
        Op::Write => "write",
        Op::Delete => "delete",
        Op::Replace => "replace",
    };
    // by name only for tables that exist: anyone reaching the public
    // listener could otherwise add a label per made-up name
    let snapshot = state.writer.snapshot();
    let table = if snapshot.model.table_named(request.name).is_some() {
        request.name
    } else {
        "(none)"
    };
    state.counters.request(table, label, response.status());
    response
}

/// Everything about the request that can be checked before reading the body:
/// the operation against the table's mode, the format, and whether to wait.
fn prepare(table: &Table, request: &Request) -> Result<(Format, bool), Box<Response>> {
    let (name, op, query, headers) = (request.name, request.op, request.query, request.headers);
    if op != Op::Write && table.mode != TableMode::Upsert {
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
    Ok((format, wait))
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

/// Hand the batch to the writer, honouring the idempotency key and the
/// credential's share of the queue and budget of new values.
#[allow(clippy::too_many_lines)]
async fn commit(
    state: &AppState,
    table: &Table,
    writer: &Writer,
    op: Op,
    batch: tidepool_store::Batch,
    wait: bool,
    key: Option<Scoped>,
) -> Response {
    let credential = writer.credential();
    let public = credential.is_some_and(|c| c.public);
    let new_values = credential.and_then(|c| c.limits.new_values);
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

    // a public batch waits in the queue only within the public share
    let slot = if public {
        let Some(slot) = state.limiter.public_slot() else {
            abandon();
            return refused(
                state,
                writer,
                "queue",
                StatusCode::TOO_MANY_REQUESTS,
                "public producers have filled their share of the write queue; retry shortly"
                    .to_string(),
                Some(Duration::from_secs(1)),
            );
        };
        Some(slot)
    } else {
        None
    };
    let max_new_values = new_values.map(|rate| {
        state
            .limiter
            .new_values_left(writer.name(), rate, Instant::now())
    });
    let Ok(receipt) = state
        .writer
        .apply(table.id, op, batch, key.clone(), max_new_values)
    else {
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
    drop(slot);
    if let Some(rate) = new_values {
        if let Some((column, count)) = &receipt.refused {
            abandon();
            let wait = state
                .limiter
                .new_values_wait(writer.name(), rate, *count, Instant::now());
            return refused(
                state,
                writer,
                "new_values",
                StatusCode::TOO_MANY_REQUESTS,
                format!(
                    "`{column}` would get {count} new values; credential `{}` may add {rate}",
                    writer.name()
                ),
                Some(wait),
            );
        }
        state
            .limiter
            .spend_new_values(writer.name(), rate, receipt.new_values, Instant::now());
    }
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
        writer.name(),
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
                filled: c.filled.map(|f| format!("{f:?}").to_lowercase()),
            })
            .collect(),
        event_time: table.event_time.map(name_of),
        retention: table.retention.map(|r| r.to_string()),
        rows: stored.map_or(0, |t| t.live_rows),
        written_total: stored.map_or(0, |t| t.written_total),
        rows_per_second: stored.map_or(0.0, |t| (t.rows_per_second * 10.0).round() / 10.0),
        bytes: stored.map_or(0, |t| t.bytes() as u64),
        segments: stored.map_or(0, |t| t.segments.len()),
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
