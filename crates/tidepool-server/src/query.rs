//! `POST /v1/query` and `POST /v1/rows`: the engine over HTTP. Documented in the endpoint table,
//! like every handler.
//!
//! Requests name things the way the semantic layer does (`dataset.field`,
//! metric names); this module resolves them against the running model,
//! converts JSON values to the fields' types, and runs the query off the
//! async runtime.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::Value as Json_;
use tidepool_config::diag::suggest;
use tidepool_config::ossie::TimeGrain;
use tidepool_engine::{
    Condition, Dim, MAX_ROWS, Needle, Order, Query, Relative, ResultSet, Role, RowsQuery, Selection,
};
use tidepool_expr::temporal::{Part, parse_date, parse_timestamp, parse_timestamptz};
use tidepool_expr::{Scalar, Type};
use tidepool_model::ir::{FieldRef, Model};
use tidepool_protocol::api_docs::ApiError;
use tidepool_protocol::live::{RelativeUnit, RelativeWindow};
use tidepool_protocol::{
    QueryColumn, QueryDimension, QueryFilter, QueryRequest, QueryResult, RowsRequest,
};
use tidepool_store::value::{format_date, format_decimal, format_timestamp};

use crate::AppState;

fn bad_request(message: impl Into<String>) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(ApiError {
            error: message.into(),
        }),
    )
        .into_response()
}

/// `; did you mean …?`, or what there is.
fn hint<'a>(
    what: &str,
    wrong: &str,
    candidates: impl IntoIterator<Item = &'a str> + Clone,
) -> String {
    format!("; {}", suggest(what, wrong, candidates))
}

/// `dataset.field`, resolved, with the field's type.
pub(crate) fn field(model: &Model, name: &str) -> Result<(FieldRef, Type), String> {
    let Some((dataset, field)) = name.split_once('.') else {
        return Err(format!("`{name}` should be `dataset.field`"));
    };
    let Some(found) = model.datasets.iter().find(|d| d.name == dataset) else {
        return Err(format!(
            "no dataset `{dataset}`{}",
            hint(
                "datasets",
                dataset,
                model.datasets.iter().map(|d| d.name.as_str())
            )
        ));
    };
    let Some(f) = found.fields.iter().find(|f| f.name == field) else {
        return Err(format!(
            "dataset `{dataset}` has no field `{field}`{}",
            hint(
                "fields",
                field,
                found.fields.iter().map(|f| f.name.as_str())
            )
        ));
    };
    Ok((
        FieldRef {
            table: found.table,
            field: field.to_string(),
        },
        f.ty,
    ))
}

fn grain(name: &str) -> Result<TimeGrain, String> {
    match name {
        "minute" => Ok(TimeGrain::Minute),
        "hour" => Ok(TimeGrain::Hour),
        "day" => Ok(TimeGrain::Day),
        "week" => Ok(TimeGrain::Week),
        "month" => Ok(TimeGrain::Month),
        "quarter" => Ok(TimeGrain::Quarter),
        "year" => Ok(TimeGrain::Year),
        other => Err(format!(
            "unknown grain `{other}`: use minute, hour, day, week, month, quarter or year"
        )),
    }
}

/// A decimal written as text (`"12.50"`), with the scale it's written at.
fn decimal(text: &str) -> Option<Scalar> {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    let digits = whole.trim_start_matches(['-', '+']);
    if digits.is_empty() && fraction.is_empty()
        || !digits
            .chars()
            .chain(fraction.chars())
            .all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let value: i128 = format!("{whole}{fraction}").parse().ok()?;
    Some(Scalar::Decimal(value, u8::try_from(fraction.len()).ok()?))
}

const MICROS_PER_DAY: i64 = 86_400_000_000;

/// A JSON value as a value of type `ty`.
pub(crate) fn scalar(value: &Json_, ty: Type) -> Option<Scalar> {
    if value.is_null() {
        return Some(Scalar::Null);
    }
    let text = match value {
        Json_::String(s) => s.clone(),
        Json_::Number(n) => n.to_string(),
        _ => String::new(),
    };
    match ty {
        Type::Bool => value.as_bool().map(Scalar::Bool),
        Type::Int => text.parse().ok().map(Scalar::Int),
        Type::Float => text.parse().ok().map(Scalar::Float),
        Type::Decimal(_) => decimal(&text),
        Type::String => value.as_str().map(|s| Scalar::String(s.to_string())),
        // a date, or midnight UTC of one: what a brush across days at a
        // grain sends, whatever the field's type
        Type::Date => value
            .as_str()
            .and_then(|text| {
                parse_date(text).or_else(|| {
                    let micros = parse_timestamptz(text)?;
                    (micros.rem_euclid(MICROS_PER_DAY) == 0)
                        .then(|| i32::try_from(micros.div_euclid(MICROS_PER_DAY)).ok())
                        .flatten()
                })
            })
            .map(Scalar::Date),
        Type::Timestamp => value
            .as_str()
            .and_then(parse_timestamp)
            .map(Scalar::Timestamp),
        Type::TimestampTz => value
            .as_str()
            .and_then(parse_timestamptz)
            .map(Scalar::TimestampTz),
        Type::Null => None,
    }
}

/// The engine's form of a window relative to now.
pub(crate) fn relative(window: RelativeWindow) -> Relative {
    Relative {
        unit: match window.unit {
            RelativeUnit::Minute => Part::Minute,
            RelativeUnit::Hour => Part::Hour,
            RelativeUnit::Day => Part::Day,
            RelativeUnit::Week => Part::Week,
            RelativeUnit::Month => Part::Month,
            RelativeUnit::Quarter => Part::Quarter,
            RelativeUnit::Year => Part::Year,
        },
        last: window.last,
        ago: window.ago,
    }
}

/// The range `window` selects of `name` (of type `ty`) at `now`.
pub(crate) fn window_condition(
    name: &str,
    window: RelativeWindow,
    ty: Type,
    now: i64,
) -> Result<Condition, String> {
    if window.last == 0 {
        return Err(format!(
            "the window on `{name}` is no periods long: `last` should be 1 or more"
        ));
    }
    relative(window)
        .condition(now, ty)
        .map_err(|e| format!("`{name}` ({}): {e}", ty.a()))
}

pub(crate) fn condition(filter: &QueryFilter, ty: Type, now: i64) -> Result<Condition, String> {
    let convert = |v: &Json_| {
        scalar(v, ty)
            .ok_or_else(|| format!("`{v}` isn't a value of `{}` ({})", filter.field, ty.a()))
    };
    if let Some(text) = &filter.contains {
        if filter.values.is_some()
            || filter.from.is_some()
            || filter.to.is_some()
            || filter.relative.is_some()
        {
            return Err(format!(
                "the filter on `{}` has `contains` and another condition: one per field",
                filter.field
            ));
        }
        return contains(&filter.field, text, ty);
    }
    match (&filter.values, &filter.from, &filter.to, filter.relative) {
        (Some(values), None, None, None) => Ok(Condition::In(
            values.iter().map(convert).collect::<Result<_, _>>()?,
        )),
        (None, from, to, None) if from.is_some() || to.is_some() => Ok(Condition::Range {
            from: from.as_ref().map(convert).transpose()?,
            to: to.as_ref().map(convert).transpose()?,
        }),
        (None, None, None, Some(window)) => window_condition(&filter.field, window, ty, now),
        _ => Err(format!(
            "the filter on `{}` needs one of `in`, `from` and/or `to`, `relative` or `contains`",
            filter.field
        )),
    }
}

/// A search of string field `name` for `text` (plan §5.7).
pub(crate) fn contains(name: &str, text: &str, ty: Type) -> Result<Condition, String> {
    if ty != Type::String {
        return Err(format!(
            "`{name}` is {}, not a string, so it can't be searched",
            ty.a()
        ));
    }
    Ok(Condition::Contains(Arc::new(Needle::new(text))))
}

fn dimension_name(dim: &QueryDimension) -> String {
    match &dim.grain {
        Some(grain) => format!("{}:{grain}", dim.field),
        None => dim.field.clone(),
    }
}

/// The engine's selection for a request's filters, with windows relative
/// to `now`.
fn selection(model: &Model, filters: &[QueryFilter], now: i64) -> Result<Selection, String> {
    let mut selection = Selection::default();
    for filter in filters {
        let (at, ty) = field(model, &filter.field)?;
        if selection
            .0
            .insert(at, condition(filter, ty, now)?)
            .is_some()
        {
            return Err(format!("`{}` is filtered twice", filter.field));
        }
    }
    Ok(selection)
}

/// The engine's query for a request, with windows relative to `now`
/// (microseconds since the Unix epoch, UTC).
pub(crate) fn resolve(model: &Model, request: &QueryRequest, now: i64) -> Result<Query, String> {
    let metrics = request
        .metrics
        .iter()
        .map(|name| {
            model
                .metrics
                .iter()
                .find(|m| m.name == *name)
                .map(|m| m.id)
                .ok_or_else(|| {
                    format!(
                        "no metric `{name}`{}",
                        hint(
                            "metrics",
                            name,
                            model.metrics.iter().map(|m| m.name.as_str())
                        )
                    )
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let dimensions = request
        .dimensions
        .iter()
        .map(|d| {
            Ok(Dim {
                field: field(model, &d.field)?.0,
                grain: d.grain.as_deref().map(grain).transpose()?,
                level: d.level,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let selection = selection(model, &request.filters, now)?;
    let names: Vec<String> = request
        .dimensions
        .iter()
        .map(dimension_name)
        .chain(request.metrics.iter().cloned())
        .collect();
    let order = request
        .sort
        .iter()
        .map(|sort| {
            names
                .iter()
                .position(|n| *n == sort.column)
                .map(|column| Order {
                    column,
                    desc: sort.desc,
                })
                .ok_or_else(|| {
                    format!(
                        "no result column `{}` to sort by{}",
                        sort.column,
                        hint("columns", &sort.column, names.iter().map(String::as_str))
                    )
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Query {
        dimensions,
        metrics,
        selection,
        order,
        limit: request.limit,
    })
}

pub(crate) fn type_name(ty: Type) -> String {
    match ty {
        Type::Null => "null".into(),
        Type::Bool => "bool".into(),
        Type::Int => "int".into(),
        Type::Float => "float".into(),
        Type::Decimal(scale) => format!("decimal({scale})"),
        Type::String => "string".into(),
        Type::Date => "date".into(),
        Type::Timestamp => "timestamp".into(),
        Type::TimestampTz => "timestamptz".into(),
    }
}

pub(crate) fn json(value: &Scalar) -> Json_ {
    match value {
        Scalar::Null => Json_::Null,
        Scalar::Bool(b) => Json_::Bool(*b),
        Scalar::Int(i) => Json_::from(*i),
        Scalar::Float(f) => serde_json::Number::from_f64(*f).map_or(Json_::Null, Json_::Number),
        Scalar::Decimal(v, s) => Json_::String(format_decimal(*v, *s)),
        Scalar::String(s) => Json_::String(s.clone()),
        Scalar::Date(d) => Json_::String(format_date(*d)),
        Scalar::Timestamp(t) => Json_::String(format_timestamp(*t, false)),
        Scalar::TimestampTz(t) => Json_::String(format_timestamp(*t, true)),
    }
}

/// Which data an answer is from: its epoch, and when that was published.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Stamp {
    pub epoch: u64,
    pub at: i64,
}

impl Stamp {
    pub(crate) fn of(snapshot: &tidepool_store::Snapshot) -> Self {
        Self {
            epoch: snapshot.epoch,
            at: snapshot.published_at,
        }
    }

    fn as_of(self) -> Option<String> {
        (self.at > 0).then(|| tidepool_store::value::format_timestamp(self.at, true))
    }
}

pub(crate) fn result(request: &QueryRequest, answer: &ResultSet, stamp: Stamp) -> QueryResult {
    // the request's spelling of the dimensions, which is also how they sort
    let names = request
        .dimensions
        .iter()
        .map(dimension_name)
        .chain(request.metrics.iter().cloned());
    QueryResult {
        columns: answer
            .columns
            .iter()
            .zip(names)
            .map(|(column, name)| QueryColumn {
                name,
                ty: type_name(column.ty),
                role: match column.role {
                    Role::Dimension => "dimension".into(),
                    Role::Metric => "metric".into(),
                },
            })
            .collect(),
        rows: answer
            .rows
            .iter()
            .map(|row| row.iter().map(json).collect())
            .collect(),
        epoch: stamp.epoch,
        as_of: stamp.as_of(),
    }
}

/// A query result as CSV (RFC 4180): a header of column names, then one
/// line per row. NULL is an empty field; text with a comma, a quote or a
/// line break is quoted.
#[must_use]
pub fn csv(result: &QueryResult) -> String {
    fn field(out: &mut String, text: &str) {
        if text.contains([',', '"', '\n', '\r']) || text.starts_with(' ') || text.ends_with(' ') {
            out.push('"');
            out.push_str(&text.replace('"', "\"\""));
            out.push('"');
        } else {
            out.push_str(text);
        }
    }
    let mut out = String::new();
    let mut line = |fields: Vec<String>| {
        for (i, f) in fields.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            field(&mut out, f);
        }
        out.push_str("\r\n");
    };
    line(result.columns.iter().map(|c| c.name.clone()).collect());
    for row in &result.rows {
        line(
            row.iter()
                .map(|v| match v {
                    serde_json::Value::Null => String::new(),
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect(),
        );
    }
    out
}

/// `?format=` of the query endpoint.
#[derive(serde::Deserialize, Default)]
pub struct Format {
    #[serde(default)]
    format: Option<String>,
}

/// Run a query. See `Operation::Query` in the endpoint table.
pub async fn query(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(format): axum::extract::Query<Format>,
    Json(request): Json<QueryRequest>,
) -> Response {
    let as_csv = match format.format.as_deref() {
        None | Some("json") => false,
        Some("csv") => true,
        Some(other) => return bad_request(format!("unknown format `{other}`: json or csv")),
    };
    let snapshot = state.writer.snapshot();
    let query = match resolve(&snapshot.model, &request, state.clock.now()) {
        Ok(query) => query,
        Err(message) => return bad_request(message),
    };
    // scans are CPU-bound and parallel: keep them off the async workers
    let epoch = Stamp::of(&snapshot);
    let engine = Arc::clone(&state.engine);
    let started = std::time::Instant::now();
    let answer = tokio::task::spawn_blocking(move || engine.run(&snapshot, &query)).await;
    state.counters.queries.record(started.elapsed());
    if matches!(answer, Ok(Err(_))) {
        state
            .counters
            .query_errors
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    match answer {
        Ok(Ok(answer)) if as_csv => (
            [(axum::http::header::CONTENT_TYPE, "text/csv; charset=utf-8")],
            csv(&result(&request, &answer, epoch)),
        )
            .into_response(),
        Ok(Ok(answer)) => Json(result(&request, &answer, epoch)).into_response(),
        Ok(Err(error)) => bad_request(error.to_string()),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError {
                error: "the query failed".into(),
            }),
        )
            .into_response(),
    }
}

/// The engine's `rows` query for a request.
pub(crate) fn resolve_rows(
    model: &Model,
    request: &RowsRequest,
    now: i64,
) -> Result<RowsQuery, String> {
    if request.fields.is_empty() {
        return Err("`fields` is empty: name the fields to show of each row".into());
    }
    if let Some(limit) = request.limit
        && limit > MAX_ROWS
    {
        return Err(format!("`limit` is {limit}: {MAX_ROWS} rows at most"));
    }
    Ok(RowsQuery {
        fields: request
            .fields
            .iter()
            .map(|name| field(model, name).map(|(at, _)| at))
            .collect::<Result<_, _>>()?,
        selection: selection(model, &request.filters, now)?,
        limit: request.limit.unwrap_or(DEFAULT_ROWS),
    })
}

/// Rows a `rows` request or visual shows when it doesn't say.
pub(crate) const DEFAULT_ROWS: usize = 100;

/// Rows as a result: one column per field, named as the request names it.
pub(crate) fn rows_result(fields: &[String], answer: &ResultSet, stamp: Stamp) -> QueryResult {
    QueryResult {
        columns: answer
            .columns
            .iter()
            .zip(fields)
            .map(|(column, name)| QueryColumn {
                name: name.clone(),
                ty: type_name(column.ty),
                role: "dimension".into(),
            })
            .collect(),
        rows: answer
            .rows
            .iter()
            .map(|row| row.iter().map(json).collect())
            .collect(),
        epoch: stamp.epoch,
        as_of: stamp.as_of(),
    }
}

/// The newest rows. See `Operation::Rows` in the endpoint table.
pub async fn rows(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(format): axum::extract::Query<Format>,
    Json(request): Json<RowsRequest>,
) -> Response {
    let as_csv = match format.format.as_deref() {
        None | Some("json") => false,
        Some("csv") => true,
        Some(other) => return bad_request(format!("unknown format `{other}`: json or csv")),
    };
    let snapshot = state.writer.snapshot();
    let query = match resolve_rows(&snapshot.model, &request, state.clock.now()) {
        Ok(query) => query,
        Err(message) => return bad_request(message),
    };
    let epoch = Stamp::of(&snapshot);
    let engine = Arc::clone(&state.engine);
    let started = std::time::Instant::now();
    let answer = tokio::task::spawn_blocking(move || engine.rows(&snapshot, &query)).await;
    state.counters.queries.record(started.elapsed());
    match answer {
        Ok(Ok(answer)) if as_csv => (
            [(axum::http::header::CONTENT_TYPE, "text/csv; charset=utf-8")],
            csv(&rows_result(&request.fields, &answer, epoch)),
        )
            .into_response(),
        Ok(Ok(answer)) => Json(rows_result(&request.fields, &answer, epoch)).into_response(),
        Ok(Err(error)) => {
            state
                .counters
                .query_errors
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            bad_request(error.to_string())
        }
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError {
                error: "reading the rows failed".into(),
            }),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tidepool_protocol::QueryColumn;

    #[test]
    fn a_date_is_a_date_or_its_midnight() {
        let date = |text: &str| scalar(&Json_::String(text.into()), Type::Date);
        let day = parse_date("2026-09-22").map(Scalar::Date);
        assert!(day.is_some());
        assert_eq!(date("2026-09-22"), day);
        assert_eq!(date("2026-09-22T00:00:00Z"), day);
        assert_eq!(date("2026-09-22T00:00:00"), day);
        // any other time isn't a date
        assert_eq!(date("2026-09-22T10:30:00Z"), None);
        assert_eq!(date("2026-09-22T00:00:00+02:00"), None);
        assert_eq!(date("soon"), None);
    }

    #[test]
    fn csv_quotes_what_needs_it_and_leaves_nulls_empty() {
        let column = |name: &str| QueryColumn {
            name: name.into(),
            ty: "string".into(),
            role: "dimension".into(),
        };
        let result = QueryResult {
            columns: vec![column("customers.name"), column("revenue")],
            rows: vec![
                vec![serde_json::json!("Ada, Ltd"), serde_json::json!("10.50")],
                vec![serde_json::json!("say \"hi\""), serde_json::Value::Null],
                vec![serde_json::json!("plain"), serde_json::json!(3)],
            ],
            epoch: 1,
            as_of: None,
        };
        assert_eq!(
            csv(&result),
            "customers.name,revenue\r\n\"Ada, Ltd\",10.50\r\n\"say \"\"hi\"\"\",\r\nplain,3\r\n"
        );
    }
}
