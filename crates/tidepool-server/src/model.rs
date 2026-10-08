//! `GET /v1/model` and `GET /v1/status`: what the model browser and the
//! status page show. Documented in the endpoint table, like every handler.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::Json;
use axum::extract::State;
use tidepool_config::server::Durability;
use tidepool_model::ir::{FieldSource, IgnoreSelection, MetricBody, Model};
use tidepool_protocol::{
    DatasetInfo, DurabilityStatus, FieldInfo, MetricInfo, ModelInfo, RelationshipInfo, StatusInfo,
    TableStatus,
};

use crate::AppState;
use crate::query::type_name;

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
        datasets: model
            .datasets
            .iter()
            .map(|d| DatasetInfo {
                name: d.name.clone(),
                table: table_name(model, d.table),
                description: d.description.clone(),
                fields: d
                    .fields
                    .iter()
                    .map(|f| FieldInfo {
                        name: f.name.clone(),
                        ty: type_name(f.ty),
                        expression: match &f.source {
                            FieldSource::Derived { expression, .. } => Some(expression.clone()),
                            FieldSource::Column(_) => None,
                        },
                        is_time: f.is_time,
                        description: f.description.clone(),
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
            })
            .collect(),
        metrics: model
            .metrics
            .iter()
            .map(|m| {
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
                    time_offset: m.time_offset.as_ref().map(|t| {
                        format!(
                            "{} {:?} on {}",
                            t.amount,
                            t.unit,
                            label(model, t.field.table, &t.field.field)
                        )
                        .to_lowercase()
                    }),
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
            })
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
    })
}
