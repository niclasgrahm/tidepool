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
        // the filter list of the query's first plain dimension, under its
        // selection
        if let Some((table, name, None)) = query.dims.first() {
            let sql = duck::values_sql(&schema, table, name, &query);
            let theirs = duck::run(&db, &sql, 2)
                .map_err(|e| format!("seed {seed} query {q}: DuckDB: {e}"))?;
            let at = FieldRef {
                table: model
                    .table_named(table)
                    .map(|t| t.id)
                    .ok_or("unknown table")?,
                field: name.clone(),
            };
            for (path, ours) in [
                (
                    "values",
                    tidepool_engine::values(snapshot, &at, &engine.selection),
                ),
                (
                    "cached values",
                    cached.values(snapshot, &at, &engine.selection),
                ),
            ] {
                let ours = ours.map_err(|e| format!("seed {seed} query {q}: {e}"))?;
                let ours: Vec<Vec<Canon>> = ours
                    .iter()
                    .map(|(v, n)| vec![canon(v), canon(&Scalar::Int(*n))])
                    .collect();
                compare(seed, q, path, &query, &sql, ours, theirs.clone())?;
            }
        }
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

/// The incremental oracle (plan §11): a few queries kept live on one cached
/// engine while the data is written, write by write, to Tidepool and to
/// DuckDB; after every published epoch each live answer must equal DuckDB's
/// cold evaluation of the data as of that epoch. Returns how many
/// (epoch, query) answers were compared.
///
/// # Errors
/// A report of the first mismatch.
pub fn check_live(seed: u64, live: usize) -> Result<usize, String> {
    let mut rng = Rng::new(seed);
    let schema = generate::schema(&mut rng);
    let files = generate::files(&schema);
    let (set, diagnostics) = ConfigSet::from_files(files);
    let (compiled, more) = compile(&set, &Catalog::default());
    let Some(compiled) = compiled else {
        let mut all = diagnostics;
        all.extend(more);
        return Err(format!(
            "seed {seed}: the generated project doesn't compile:\n{}",
            render_all(&all, &set.files)
        ));
    };
    let model = Arc::new(compiled.model);
    let data = generate::data(&mut rng, &schema);
    let queries: Vec<(generate::Query, Query)> = (0..live)
        .map(|_| {
            let query = generate::query(&mut rng, &schema, &data);
            let engine = engine_query(&model, &schema, &query)?;
            Ok((query, engine))
        })
        .collect::<Result<_, String>>()
        .map_err(|e| format!("seed {seed}: {e}"))?;

    let db = duck::empty(&schema).map_err(|e| format!("seed {seed}: {e}"))?;
    let mut store = Store::with_seal_rows(Arc::clone(&model), 8 + rng.below(200));
    let engine = Engine::default();
    let mut compared = 0;
    let check = |snapshot: &Snapshot, compared: &mut usize| -> Result<(), String> {
        for (q, (query, live)) in queries.iter().enumerate() {
            let sql = duck::sql(&schema, query);
            let width = query.dims.len() + query.metrics.len();
            let theirs = duck::run(&db, &sql, width)
                .map_err(|e| format!("seed {seed} epoch {}: DuckDB: {e}", snapshot.epoch))?;
            let ours = engine
                .run(snapshot, live)
                .map_err(|e| format!("seed {seed} live query {q}: {e}"))?;
            let ours: Vec<Vec<Canon>> = ours
                .rows
                .iter()
                .map(|r| r.iter().map(canon).collect())
                .collect();
            let path = format!("live, epoch {}", snapshot.epoch);
            compare(seed, q, &path, query, &sql, ours, theirs)?;
            *compared += 1;
        }
        Ok(())
    };
    for write in data.writes.iter().chain(std::iter::once(&Write::Publish)) {
        duck::apply(&db, &schema, write).map_err(|e| format!("seed {seed}: DuckDB: {e}"))?;
        let (table, op, rows) = match write {
            Write::Publish => {
                if let Some(snapshot) = store.publish() {
                    check(&snapshot, &mut compared)?;
                }
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
    Ok(compared)
}

/// The project as it was before the generated one: the same tables, each
/// with an extra column and with some columns stored the other way, and no
/// relationships or metrics. Moving from it to the generated project merges
/// the dictionaries of every relationship's columns over data, converts
/// columns between encodings and drops the extra columns.
fn before(schema: &Schema, rng: &mut Rng) -> Vec<(String, String)> {
    let keys: Vec<(String, String)> = schema
        .relationships
        .iter()
        .map(|r| (r.from.clone(), r.fk.clone()))
        .collect();
    let mut files = Vec::new();
    for (path, text) in generate::files(schema) {
        if let Some(name) = path
            .strip_prefix("tables/")
            .and_then(|p| p.strip_suffix(".yaml"))
        {
            let mut out = String::new();
            for line in text.lines() {
                let column = line
                    .trim_start()
                    .split_once(':')
                    .map(|(c, _)| c.to_string())
                    .filter(|_| line.starts_with("  ") && line.ends_with(" }"));
                match column {
                    Some(c)
                        if c != "key"
                            && !keys.iter().any(|(t, k)| t == name && *k == c)
                            && rng.chance(50) =>
                    {
                        let encoding = if rng.chance(50) { "plain" } else { "dict" };
                        out.push_str(&line.replacen(
                            " }",
                            &format!(", encoding: {encoding} }}"),
                            1,
                        ));
                    }
                    _ => out.push_str(line),
                }
                out.push('\n');
            }
            out.push_str("  scratch: { type: string, nullable: true }\n");
            files.push((path, out));
        } else if path == "model.ossie.yaml" {
            let datasets = text.split("relationships:").next().unwrap_or_default();
            files.push((path, format!("{datasets}relationships: []\nmetrics: []\n")));
        } else {
            files.push((path, text));
        }
    }
    files
}

/// Apply one write to `store`, decoded under `model` (which may be older
/// than the store's: the store fits it).
fn write_to(
    store: &mut Store,
    model: &Model,
    write: &Write,
    last: &mut Option<Snapshot>,
) -> Result<(), String> {
    let (table, op, rows) = match write {
        Write::Publish => {
            if let Some(snapshot) = store.publish() {
                *last = Some(snapshot);
            }
            return Ok(());
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
    Ok(())
}

/// The model-change oracle (M7): the data is written first under an
/// earlier version of the project ([`before`]); midway the store moves to
/// the generated project the way the writer does it: a build from a
/// snapshot, writes going on meanwhile (some decoded under the old model
/// and applied after the swap), then the swap. Every query must then agree
/// with DuckDB on the fast, general and cached paths. Returns how many
/// queries were compared.
///
/// # Errors
/// A report of the first mismatch.
pub fn check_migration(seed: u64, queries: usize) -> Result<usize, String> {
    let mut rng = Rng::new(seed);
    let schema = generate::schema(&mut rng);
    let compile_files = |files: Vec<(String, String)>, catalog: &Catalog| {
        let (set, mut diagnostics) = ConfigSet::from_files(files);
        let (compiled, more) = compile(&set, catalog);
        diagnostics.extend(more);
        compiled.ok_or_else(|| {
            format!(
                "seed {seed}: a generated project doesn't compile:\n{}",
                render_all(&diagnostics, &set.files)
            )
        })
    };
    let old = compile_files(before(&schema, &mut rng), &Catalog::default())?;
    let new = compile_files(generate::files(&schema), &old.catalog)?;
    let (old_model, model) = (Arc::new(old.model), Arc::new(new.model));
    let data = generate::data(&mut rng, &schema);

    let mut store = Store::with_version(
        tidepool_store::ModelVersion {
            version: 1,
            model: Arc::clone(&old_model),
            catalog: Arc::new(old.catalog),
        },
        8 + rng.below(200),
    );
    let n = data.writes.len();
    let start = rng.below(n.max(1));
    let swap = (start + rng.below(n - start + 1)).min(n);
    let late = (swap + rng.below(n - swap + 1)).min(n);
    let mut last = None;
    for write in &data.writes[..start] {
        write_to(&mut store, &old_model, write, &mut last)?;
    }
    let snapshot = store
        .publish()
        .unwrap_or_else(|| Snapshot::empty(Arc::clone(&old_model)));
    let migration = store.plan_migration(tidepool_store::ModelVersion {
        version: 2,
        model: Arc::clone(&model),
        catalog: Arc::new(new.catalog),
    });
    let rebuilt = if rng.chance(75) {
        tidepool_store::migrate::build(&migration, &snapshot, &std::sync::atomic::AtomicU64::new(0))
    } else {
        tidepool_store::Rebuilt::default()
    };
    for write in &data.writes[start..swap] {
        write_to(&mut store, &old_model, write, &mut last)?;
    }
    store.migrate(migration, rebuilt);
    // decoded before the change, applied after it
    for write in &data.writes[swap..late] {
        write_to(&mut store, &old_model, write, &mut last)?;
    }
    for write in &data.writes[late..] {
        write_to(&mut store, &model, write, &mut last)?;
    }
    let snapshot = store
        .publish()
        .or(last)
        .filter(|s| s.version == 2)
        .ok_or_else(|| format!("seed {seed}: nothing published after the change"))?;
    let db = duck::load(&schema, &data).map_err(|e| format!("seed {seed}: loading DuckDB: {e}"))?;
    let cached = Engine::default();
    for q in 0..queries {
        let query = generate::query(&mut rng, &schema, &data);
        let engine = engine_query(&model, &schema, &query)
            .map_err(|e| format!("seed {seed} query {q}: {e}"))?;
        let sql = duck::sql(&schema, &query);
        let width = query.dims.len() + query.metrics.len();
        let theirs = duck::run(&db, &sql, width)
            .map_err(|e| format!("seed {seed} query {q}: DuckDB: {e}"))?;
        for (path, ours) in [
            ("migrated, fast", run(&snapshot, &engine)),
            ("migrated, general", run_general(&snapshot, &engine)),
            ("migrated, cached", cached.run(&snapshot, &engine)),
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
    Ok(queries)
}

/// Queries also run over every intermediate snapshot.
const HISTORY_QUERIES: usize = 12;

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
