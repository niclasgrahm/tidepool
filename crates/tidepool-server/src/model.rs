//! `GET /v1/model` and `GET /v1/status`: what the model browser and the
//! status page show. Documented in the endpoint table, like every handler.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::Json;
use axum::extract::State;
use tidepool_config::ossie::TimeGrain;
use tidepool_config::server::Durability;
use tidepool_expr::Type;
use tidepool_model::ir::{
    Direction, FieldSource, Hierarchy, IgnoreSelection, MetricBody, Model, TableId, Total,
    WindowSpec,
};
use tidepool_protocol::{
    DatasetInfo, DurabilityStatus, FieldInfo, HierarchyInfo, MetricInfo, ModelInfo,
    RelationshipInfo, StatusInfo, TableStatus,
};

use crate::AppState;
use crate::query::type_name;

/// A grain as a word: `month`.
fn word(grain: TimeGrain) -> String {
    match serde_json::to_value(grain) {
        Ok(serde_json::Value::String(word)) => word,
        _ => format!("{grain:?}").to_lowercase(),
    }
}

/// `dataset.field`, or `table.column` for a table without a dataset.
fn label(model: &Model, table: tidepool_model::ir::TableId, field: &str) -> String {
    let name = model
        .dataset(table)
        .map(|d| d.name.as_str())
        .or_else(|| model.table(table).map(|t| t.name.as_str()))
        .unwrap_or("?");
    format!("{name}.{field}")
}

fn table_name(model: &Model, table: tidepool_model::ir::TableId) -> String {
    model
        .table(table)
        .map_or_else(|| "?".into(), |t| t.name.clone())
}

fn hierarchy(model: &Model, table: TableId, h: &Hierarchy) -> HierarchyInfo {
    let name = |id| {
        model
            .table(table)
            .and_then(|t| t.column(id))
            .map_or_else(|| "?".into(), |c| c.name.clone())
    };
    HierarchyInfo {
        key: name(h.key),
        parent: name(h.parent),
        label: h.label.map(name),
        kind: h.kind.map(name),
    }
}

/// The grains a visual may group `field` by: those it declares, or every
/// grain its type has (sub-day ones need a time of day). None for a field
/// that isn't a time.
fn grains(field: &tidepool_model::ir::Field) -> Vec<String> {
    if !field.is_time {
        return Vec::new();
    }
    let all = [
        TimeGrain::Minute,
        TimeGrain::Hour,
        TimeGrain::Day,
        TimeGrain::Week,
        TimeGrain::Month,
        TimeGrain::Quarter,
        TimeGrain::Year,
    ];
    let allowed: Vec<TimeGrain> = if field.grains.is_empty() {
        all.into_iter()
            .filter(|g| !g.is_sub_day() || field.ty != Type::Date)
            .collect()
    } else {
        field.grains.clone()
    };
    allowed
        .into_iter()
        .filter_map(|g| match serde_json::to_value(g) {
            Ok(serde_json::Value::String(name)) => Some(name),
            _ => None,
        })
        .collect()
}

pub(crate) fn info(model: &Model) -> ModelInfo {
    let columns = |table, ids: &[tidepool_model::ir::ColumnId]| -> Vec<String> {
        ids.iter()
            .map(|id| {
                model
                    .table(table)
                    .and_then(|t| t.column(*id))
                    .map_or_else(|| "?".into(), |c| c.name.clone())
            })
            .collect()
    };
    ModelInfo {
        name: model.name.clone(),
        description: model.description.clone(),
        ai_context: model.ai_context.clone(),
        datasets: model
            .datasets
            .iter()
            .map(|d| DatasetInfo {
                name: d.name.clone(),
                table: table_name(model, d.table),
                description: d.description.clone(),
                ai_context: d.ai_context.clone(),
                fields: d
                    .fields
                    .iter()
                    .map(|f| FieldInfo {
                        name: f.name.clone(),
                        ty: type_name(f.ty),
                        expression: match &f.source {
                            FieldSource::Derived { expression, .. } => Some(expression.clone()),
                            FieldSource::Column(_) | FieldSource::Hierarchy(_) => None,
                        },
                        is_time: f.is_time,
                        grains: grains(f),
                        label: f.label.clone(),
                        description: f.description.clone(),
                        ai_context: f.ai_context.clone(),
                        hierarchy: f.hierarchy().map(|h| hierarchy(model, d.table, h)),
                    })
                    .collect(),
            })
            .collect(),
        relationships: model
            .relationships
            .iter()
            .map(|r| RelationshipInfo {
                name: r.name.clone(),
                from: table_name(model, r.from),
                from_columns: columns(r.from, &r.from_columns),
                to: table_name(model, r.to),
                to_columns: columns(r.to, &r.to_columns),
                role: r.role.clone(),
                ai_context: r.ai_context.clone(),
            })
            .collect(),
        metrics: model
            .metrics
            .iter()
            .map(|m| metric_info(model, m))
            .collect(),
        filter_direction: match model.direction {
            Direction::OneToMany => "one_to_many",
            Direction::Both => "both",
        }
        .into(),
    }
}

/// One metric as the model browser and agents see it.
fn metric_info(model: &Model, m: &tidepool_model::ir::Metric) -> MetricInfo {
    let (expression, kind, anchor) = match &m.body {
        MetricBody::Base {
            expression, anchor, ..
        } => (expression.clone(), "base", Some(table_name(model, *anchor))),
        MetricBody::Derived { expression, .. } => (expression.clone(), "derived", None),
    };
    MetricInfo {
        name: m.name.clone(),
        description: m.description.clone(),
        expression,
        kind: kind.into(),
        anchor,
        ignores: match &m.ignore_selection {
            IgnoreSelection::None => Vec::new(),
            IgnoreSelection::All => vec!["*".into()],
            IgnoreSelection::Fields(fields) => fields
                .iter()
                .map(|f| label(model, f.table, &f.field))
                .collect(),
        },
        totals: match &m.total {
            Total::None => Vec::new(),
            Total::All => vec!["*".into()],
            Total::Fields(fields) => fields
                .iter()
                .map(|f| label(model, f.table, &f.field))
                .collect(),
        },
        time_offset: m.time_offset.as_ref().map(|t| {
            format!(
                "{} {:?} on {}",
                t.amount,
                t.unit,
                label(model, t.field.table, &t.field.field)
            )
            .to_lowercase()
        }),
        time_window: m.time_window.as_ref().map(|w| {
            let span = match w.spec {
                WindowSpec::ToDate(grain) => format!("{} to date", word(grain)),
                WindowSpec::Last(n, grain) => format!("last {n} {}", word(grain)),
            };
            format!("{span} on {}", label(model, w.field.table, &w.field.field))
        }),
        format: m.format.clone(),
        ai_context: m.ai_context.clone(),
        dashboards: model
            .dashboards
            .iter()
            .filter(|d| {
                d.visuals
                    .iter()
                    .any(|v| v.metrics.contains(&m.id) || v.compare == Some(m.id))
            })
            .map(|d| d.slug.clone())
            .collect(),
    }
}

/// The semantic model. See `Operation::GetModel` in the endpoint table.
pub async fn model(State(state): State<Arc<AppState>>) -> Json<ModelInfo> {
    Json(info(&state.writer.snapshot().model))
}

/// What the server is doing. See `Operation::GetStatus` in the endpoint table.
pub async fn status(State(state): State<Arc<AppState>>) -> Json<StatusInfo> {
    let snapshot = state.writer.snapshot();
    let mut tables: Vec<TableStatus> = snapshot
        .model
        .tables
        .iter()
        .filter_map(|t| {
            let stored = snapshot.table(t.id)?;
            Some(TableStatus {
                name: t.name.clone(),
                rows: stored.live_rows,
                bytes: stored.bytes() as u64,
                segments: stored.segments.len(),
                rows_per_second: stored.rows_per_second,
            })
        })
        .collect();
    tables.sort_by(|a, b| a.name.cmp(&b.name));
    let durability = state.writer.log().map(|log| DurabilityStatus {
        data_dir: log.data_dir.display().to_string(),
        mode: match log.durability {
            Durability::Fsync => "fsync".into(),
            Durability::Async => "async".into(),
        },
        last_lsn: log.last_lsn.load(Ordering::Relaxed),
        checkpoint_lsn: log.checkpoint_lsn.load(Ordering::Relaxed),
        checkpoint_age_seconds: log
            .checkpoint_at
            .lock()
            .ok()
            .and_then(|at| at.map(|at| at.elapsed().as_secs())),
    });
    Json(StatusInfo {
        epoch: snapshot.epoch,
        uptime_seconds: state.started.elapsed().as_secs(),
        tables,
        dictionary_bytes: snapshot
            .dicts
            .values()
            .map(|d| d.value_bytes() as u64)
            .sum(),
        cache_bytes: state.engine.cache_bytes() as u64,
        sessions: state.hub.live_sessions(),
        durability,
        config_version: snapshot.version,
        rebuild: crate::config::rebuild_info(&state),
        epoch_publish_ms: state.writer.publishing().last_ms(),
        live_refresh_ms: state.counters.refresh.last_ms(),
    })
}
