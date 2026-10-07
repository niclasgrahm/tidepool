//! Test-only: checks Tidepool's query results against DuckDB on random
//! models, data, queries and selections (plan §11).
//!
//! [`check_seed`] is one round: a random project from the seed, written to a
//! Tidepool store in random batches (with upserts, deletes and epochs in
//! between) and loaded into DuckDB, then many random queries compared. A
//! failure names the seed and the query, so it reproduces exactly.

pub mod duck;
pub mod generate;

use std::sync::Arc;

use serde_json::Value as Json;
use tidepool_config::ConfigSet;
use tidepool_config::diag::render_all;
use tidepool_engine::{Condition, Dim, Engine, Query, Selection, run, run_general};
use tidepool_expr::Scalar;
use tidepool_model::ir::{FieldRef, Model};
use tidepool_model::{Catalog, compile};
use tidepool_store::batch::{Shape, json_array};
use tidepool_store::{Op, Snapshot, Store};

use crate::duck::Canon;
use crate::generate::{Cond, Gen, Rng, Schema, Write};

/// A Tidepool scalar as a canonical value.
fn canon(value: &Scalar) -> Canon {
    match value {
        Scalar::Null => Canon::Null,
        Scalar::Bool(b) => Canon::Bool(*b),
        Scalar::String(s) => Canon::Text(s.clone()),
        Scalar::Date(d) => Canon::Date(*d),
        Scalar::Timestamp(m) | Scalar::TimestampTz(m) => Canon::Micros(*m),
        number => Canon::Num(number.as_f64().unwrap_or(f64::NAN)),
    }
}

/// A selection value (JSON, as in the data) as a scalar of its column.
fn scalar(value: &Json, values: &Gen) -> Scalar {
    let text = value.as_str().unwrap_or_default();
    match values {
        Gen::Int(..) => value.as_i64().map_or(Scalar::Null, Scalar::Int),
        Gen::Decimal(..) => {
            // the text's own scale: range bounds may be finer than the column
            let scale = text
                .split_once('.')
                .map_or(0, |(_, fraction)| fraction.len());
            let digits: String = text.chars().filter(|c| *c != '.').collect();
            digits.parse().map_or(Scalar::Null, |v| {
                Scalar::Decimal(v, u8::try_from(scale).unwrap_or(0))
            })
        }
        Gen::Float => value.as_f64().map_or(Scalar::Null, Scalar::Float),
        Gen::Date(..) => {
            tidepool_expr::temporal::parse_date(text).map_or(Scalar::Null, Scalar::Date)
        }
        Gen::Timestamp(..) => tidepool_expr::temporal::parse_timestamptz(text)
            .map_or(Scalar::Null, Scalar::TimestampTz),
        _ => Scalar::String(text.to_string()),
    }
}

/// The engine's form of a generated query.
fn engine_query(model: &Model, schema: &Schema, query: &generate::Query) -> Result<Query, String> {
    let field = |table: &str, field: &str| -> Result<FieldRef, String> {
        let table = model.table_named(table).ok_or("unknown table")?;
        Ok(FieldRef {
            table: table.id,
            field: field.to_string(),
        })
    };
    let metrics = query
        .metrics
        .iter()
        .map(|name| {
            model
                .metrics
                .iter()
                .find(|m| m.name == *name)
                .map(|m| m.id)
                .ok_or(format!("no metric {name}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let dimensions = query
        .dims
        .iter()
        .map(|(table, name, grain)| {
            Ok(Dim {
                field: field(table, name)?,
                grain: grain.map(|g| match g {
                    "day" => tidepool_config::ossie::TimeGrain::Day,
                    "week" => tidepool_config::ossie::TimeGrain::Week,
                    "month" => tidepool_config::ossie::TimeGrain::Month,
                    "quarter" => tidepool_config::ossie::TimeGrain::Quarter,
                    _ => tidepool_config::ossie::TimeGrain::Year,
                }),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let mut selection = Selection::default();
    for (table, column, cond) in &query.selection {
        let source = schema
            .table(table)
            .and_then(|t| t.column(column))
            .map(|c| c.values.clone())
            .ok_or("unknown column")?;
        let condition = match cond {
            Cond::In(values) => Condition::In(values.iter().map(|v| scalar(v, &source)).collect()),
            Cond::Range(from, to) => Condition::Range {
                from: Some(scalar(from, &source)),
                to: Some(scalar(to, &source)),
            },
        };
        selection.0.insert(field(table, column)?, condition);
    }
    Ok(Query {
        dimensions,
        metrics,
        selection,
        order: Vec::new(),
        limit: None,
    })
}

/// Write `data` to a fresh store for `model`, the way clients would.
/// Every snapshot published along the way, the final one last.
fn store(model: &Arc<Model>, rng: &mut Rng, writes: &[Write]) -> Result<Vec<Snapshot>, String> {
    let mut store = Store::with_seal_rows(Arc::clone(model), 8 + rng.below(200));
    let mut history: Vec<Snapshot> = store.publish().into_iter().collect();
    for write in writes {
        let (table, op, rows) = match write {
            Write::Publish => {
                history.extend(store.publish());
                continue;
            }
            Write::Rows { table, rows } => (table, Op::Write, rows),
            Write::Delete { table, keys } => (table, Op::Delete, keys),
        };
        let spec = model.table_named(table).ok_or("unknown table")?;
        let shape = if op == Op::Delete {
            Shape::Keys
        } else {
            Shape::Rows
        };
        let body = serde_json::to_vec(rows).map_err(|e| e.to_string())?;
        let batch =
            json_array(spec, shape, &body).map_err(|r| format!("{} rejected: {r:?}", spec.name))?;
        store.apply(spec.id, op, &batch);
    }
    history.extend(store.publish());
    if history.is_empty() {
        return Err("nothing published".to_string());
    }
    Ok(history)
}

fn sorted(mut rows: Vec<Vec<Canon>>) -> Vec<Vec<Canon>> {
    rows.sort_by(|a, b| {
        a.iter()
            .zip(b)
            .map(|(x, y)| x.order(y))
            .find(|o| *o != std::cmp::Ordering::Equal)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    rows
}

/// Run one seed: a project, its data, and `queries` random queries, each
/// checked against DuckDB. Returns how many queries were compared.
///
/// # Errors
/// A report of the first mismatch (or of a generator problem).
pub fn check_seed(seed: u64, queries: usize) -> Result<usize, String> {
    let mut rng = Rng::new(seed);
    let schema = generate::schema(&mut rng);
    let files = generate::files(&schema);
    let (set, mut diagnostics) = ConfigSet::from_files(files.clone());
    let (compiled, more) = compile(&set, &Catalog::default());
    diagnostics.extend(more);
    let Some(compiled) = compiled else {
        return Err(format!(
            "seed {seed}: the generated project doesn't compile:\n{}",
            render_all(&diagnostics, &set.files)
        ));
    };
    let model = Arc::new(compiled.model);
    let data = generate::data(&mut rng, &schema);
    let history = store(&model, &mut rng, &data.writes).map_err(|e| format!("seed {seed}: {e}"))?;
    let Some(snapshot) = history.last() else {
        return Err(format!("seed {seed}: nothing published"));
    };
    let db = duck::load(&schema, &data).map_err(|e| format!("seed {seed}: loading DuckDB: {e}"))?;

    let cached = Engine::default();
    let mut replayed = Vec::new();
    for q in 0..queries {
        let query = generate::query(&mut rng, &schema, &data);
        let engine = engine_query(&model, &schema, &query)
            .map_err(|e| format!("seed {seed} query {q}: {e}"))?;
        if q < HISTORY_QUERIES {
            replayed.push(engine.clone());
        }
        let sql = duck::sql(&schema, &query);
        let width = query.dims.len() + query.metrics.len();
        let theirs = duck::run(&db, &sql, width)
            .map_err(|e| format!("seed {seed} query {q}: DuckDB: {e}"))?;
        // every execution path must agree with DuckDB: the fast one, the
        // general one, and the fast one with caches built by earlier queries
        for (path, ours) in [
            ("fast", run(snapshot, &engine)),
            ("general", run_general(snapshot, &engine)),
            ("cached", cached.run(snapshot, &engine)),
        ] {
            let ours = ours
                .map_err(|e| format!("seed {seed} query {q}: Tidepool refused {query:?}: {e}"))?;
            let ours: Vec<Vec<Canon>> = ours
                .rows
                .iter()
                .map(|row| row.iter().map(canon).collect())
                .collect();
            compare(seed, q, path, &query, &sql, ours, theirs.clone())?;
        }
    }
    replay(seed, &history, &replayed)?;
    Ok(queries)
}

/// Queries also run over every intermediate snapshot.
const HISTORY_QUERIES: usize = 4;

/// Run `queries` over each snapshot in turn with one cached engine, as a
/// server would while data arrives, and check every answer against the
/// uncached engine's: caches built on one epoch must never leak into the next.
fn replay(seed: u64, history: &[Snapshot], queries: &[Query]) -> Result<(), String> {
    let cached = Engine::default();
    let canon_rows = |rows: &[Vec<Scalar>]| -> Vec<Vec<Canon>> {
        rows.iter()
            .map(|row| row.iter().map(canon).collect())
            .collect()
    };
    for (e, snapshot) in history.iter().enumerate() {
        for (q, query) in queries.iter().enumerate() {
            let fresh = run(snapshot, query).map_err(|err| format!("seed {seed}: {err}"))?;
            let reused = cached
                .run(snapshot, query)
                .map_err(|err| format!("seed {seed}: {err}"))?;
            let (fresh, reused) = (canon_rows(&fresh.rows), canon_rows(&reused.rows));
            let same = fresh.len() == reused.len()
                && fresh
                    .iter()
                    .zip(&reused)
                    .all(|(a, b)| a.iter().zip(b).all(|(x, y)| x.close(y)));
            if !same {
                return Err(format!(
                    "seed {seed} snapshot {e} (epoch {}) query {q}: the cached engine differs\nquery: {query:?}\nuncached: {fresh:?}\ncached: {reused:?}",
                    snapshot.epoch
                ));
            }
        }
    }
    Ok(())
}

fn compare(
    seed: u64,
    q: usize,
    path: &str,
    query: &generate::Query,
    sql: &str,
    ours: Vec<Vec<Canon>>,
    theirs: Vec<Vec<Canon>>,
) -> Result<(), String> {
    {
        let (ours, theirs) = (sorted(ours), sorted(theirs));
        let same = ours.len() == theirs.len()
            && ours
                .iter()
                .zip(&theirs)
                .all(|(a, b)| a.iter().zip(b).all(|(x, y)| x.close(y)));
        if !same {
            let show = |rows: &[Vec<Canon>]| -> String {
                rows.iter()
                    .take(12)
                    .map(|r| format!("  {r:?}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            return Err(format!(
                "seed {seed} query {q}: results differ ({path} path)\nquery: {query:?}\nsql: {sql}\ntidepool ({} rows):\n{}\nduckdb ({} rows):\n{}",
                ours.len(),
                show(&ours),
                theirs.len(),
                show(&theirs)
            ));
        }
    }
    Ok(())
}
