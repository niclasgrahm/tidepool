//! `POST /v1/query`: the engine over HTTP. Documented in the endpoint table,
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
use tidepool_engine::{Condition, Dim, Order, Query, ResultSet, Role, Selection};
use tidepool_expr::temporal::{parse_date, parse_timestamp, parse_timestamptz};
use tidepool_expr::{Scalar, Type};
use tidepool_model::ir::{FieldRef, Model};
use tidepool_protocol::api_docs::ApiError;
use tidepool_protocol::{QueryColumn, QueryDimension, QueryFilter, QueryRequest, QueryResult};
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
fn field(model: &Model, name: &str) -> Result<(FieldRef, Type), String> {
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
        "day" => Ok(TimeGrain::Day),
        "week" => Ok(TimeGrain::Week),
        "month" => Ok(TimeGrain::Month),
        "quarter" => Ok(TimeGrain::Quarter),
        "year" => Ok(TimeGrain::Year),
        other => Err(format!(
            "unknown grain `{other}`: use day, week, month, quarter or year"
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

/// A JSON value as a value of type `ty`.
fn scalar(value: &Json_, ty: Type) -> Option<Scalar> {
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
        Type::Date => value.as_str().and_then(parse_date).map(Scalar::Date),
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

fn condition(filter: &QueryFilter, ty: Type) -> Result<Condition, String> {
    let convert = |v: &Json_| {
        scalar(v, ty)
            .ok_or_else(|| format!("`{v}` isn't a value of `{}` ({})", filter.field, ty.a()))
    };
    match (&filter.values, &filter.from, &filter.to) {
        (Some(values), None, None) => Ok(Condition::In(
            values.iter().map(convert).collect::<Result<_, _>>()?,
        )),
        (None, from, to) if from.is_some() || to.is_some() => Ok(Condition::Range {
            from: from.as_ref().map(convert).transpose()?,
            to: to.as_ref().map(convert).transpose()?,
        }),
        _ => Err(format!(
            "the filter on `{}` needs either `in`, or `from` and/or `to`",
            filter.field
        )),
    }
}

fn dimension_name(dim: &QueryDimension) -> String {
    match &dim.grain {
        Some(grain) => format!("{}:{grain}", dim.field),
        None => dim.field.clone(),
    }
}

/// The engine's query for a request.
fn resolve(model: &Model, request: &QueryRequest) -> Result<Query, String> {
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
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let mut selection = Selection::default();
    for filter in &request.filters {
        let (at, ty) = field(model, &filter.field)?;
        if selection.0.insert(at, condition(filter, ty)?).is_some() {
            return Err(format!("`{}` is filtered twice", filter.field));
        }
    }
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

fn type_name(ty: Type) -> String {
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

fn json(value: &Scalar) -> Json_ {
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

fn result(request: &QueryRequest, answer: &ResultSet, epoch: u64) -> QueryResult {
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
        epoch,
    }
}

/// Run a query. See `Operation::Query` in the endpoint table.
pub async fn query(
    State(state): State<Arc<AppState>>,
    Json(request): Json<QueryRequest>,
) -> Response {
    let snapshot = state.writer.snapshot();
    let query = match resolve(&snapshot.model, &request) {
        Ok(query) => query,
        Err(message) => return bad_request(message),
    };
    // scans are CPU-bound and parallel: keep them off the async workers
    let epoch = snapshot.epoch;
    let answer = tokio::task::spawn_blocking(move || tidepool_engine::run(&snapshot, &query)).await;
    match answer {
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
