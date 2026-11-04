//! Text search and `rows` (plan §5.7) on a small log model: every path
//! (fast, general, cached and kept up to date) gives the same answer, and
//! that answer is the one written out here.

use std::sync::Arc;

use tidepool_config::ConfigSet;
use tidepool_engine::{
    Condition, Engine, Needle, Query, RowsQuery, Selection, rows, rows_general, run, run_general,
};
use tidepool_expr::Scalar;
use tidepool_model::ir::{FieldRef, Model};
use tidepool_model::{Catalog, compile};
use tidepool_store::batch::{Shape, json_array};
use tidepool_store::{Op, Snapshot, Store};

const LOGS: &str = "name: logs
mode: upsert
primary_key: [id]
columns:
  id: { type: int64, encoding: dict }
  level: { type: string }
  service: { type: string, nullable: true }
  message: { type: string, encoding: plain, nullable: true }
";

const SERVICES: &str = "name: services
mode: upsert
primary_key: [key]
columns:
  key: { type: string }
  team: { type: string }
";

const MODEL: &str = r#"version: "0.2.0"
name: logs
datasets:
  - name: logs
    source: logs
  - name: services
    source: services
relationships:
  - name: logs_service
    from: logs
    to: services
    from_columns: [service]
    to_columns: [key]
metrics:
  - name: lines
    expression: { dialects: [{ dialect: OSSIE_SQL_2026, expression: "COUNT(*)" }] }
    custom_extensions:
      - vendor_name: TIDEPOOL
        data: '{"table": "logs"}'
"#;

fn model() -> Arc<Model> {
    let (set, diagnostics) = ConfigSet::from_files(vec![
        ("tables/logs.yaml".to_string(), LOGS.to_string()),
        ("tables/services.yaml".to_string(), SERVICES.to_string()),
        ("model.ossie.yaml".to_string(), MODEL.to_string()),
    ]);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    match compile(&set, &Catalog::default()) {
        (Some(compiled), _) => Arc::new(compiled.model),
        (None, d) => panic!("the log model should compile: {d:?}"),
    }
}

fn write(store: &mut Store, table: &str, op: Op, json: &str) {
    let model = Arc::clone(store.model());
    let Some(spec) = model.table_named(table) else {
        panic!("no table {table}");
    };
    let shape = if op == Op::Delete {
        Shape::Keys
    } else {
        Shape::Rows
    };
    let batch = match json_array(spec, shape, json.as_bytes()) {
        Ok(batch) => batch,
        Err(rejected) => panic!("{rejected:?}"),
    };
    store.apply(spec.id, op, &batch);
}

fn publish(store: &mut Store) -> Snapshot {
    match store.publish() {
        Some(snapshot) => snapshot,
        None => panic!("expected a new epoch"),
    }
}

fn field(model: &Model, table: &str, name: &str) -> FieldRef {
    match model.table_named(table) {
        Some(t) => FieldRef {
            table: t.id,
            field: name.to_string(),
        },
        None => panic!("no table {table}"),
    }
}

fn contains(text: &str) -> Condition {
    Condition::Contains(Arc::new(Needle::new(text)))
}

/// Lines `from..to`: every seventh times out, every fifth is an error.
fn lines(from: i64, to: i64) -> String {
    let rows: Vec<String> = (from..to)
        .map(|i| {
            let level = if i % 5 == 0 { "ERROR" } else { "LOG" };
            let service = ["api", "db", "worker"][usize::try_from(i % 3).unwrap_or(0)];
            let message = match i {
                _ if i % 11 == 0 => "null".to_string(),
                _ if i % 7 == 0 => format!("\"canceling statement {i} due to Lock Timeout\""),
                _ => format!("\"statement {i} took {} ms\"", i * 3),
            };
            format!(
                r#"{{"id": {i}, "level": "{level}", "service": "{service}", "message": {message}}}"#
            )
        })
        .collect();
    format!("[{}]", rows.join(", "))
}

fn ids(result: &tidepool_engine::ResultSet) -> Vec<i64> {
    result
        .rows
        .iter()
        .map(|row| match row.first() {
            Some(Scalar::Int(id)) => *id,
            other => panic!("expected an id first, got {other:?}"),
        })
        .collect()
}

/// The rows every way they're computed, which must agree.
fn agreed(snapshot: &Snapshot, engine: &Engine, query: &RowsQuery) -> Vec<i64> {
    let fast = match rows(snapshot, query) {
        Ok(r) => r,
        Err(e) => panic!("rows: {e}"),
    };
    let general = match rows_general(snapshot, query) {
        Ok(r) => r,
        Err(e) => panic!("rows the general way: {e}"),
    };
    let cached = match engine.rows(snapshot, query) {
        Ok(r) => r,
        Err(e) => panic!("rows cached: {e}"),
    };
    assert_eq!(fast, general, "fast and general rows differ");
    assert_eq!(fast, cached, "fast and cached rows differ");
    ids(&fast)
}

/// What `rows` must answer: the newest `limit` ids passing `keep`.
fn expected(live: &[i64], limit: usize, keep: impl Fn(i64) -> bool) -> Vec<i64> {
    live.iter()
        .rev()
        .copied()
        .filter(|i| keep(*i))
        .take(limit)
        .collect()
}

fn timeout(i: i64) -> bool {
    i % 7 == 0 && i % 11 != 0
}

#[test]
fn rows_are_the_newest_that_pass_and_stay_so_as_data_arrives() {
    let model = model();
    let mut store = Store::with_seal_rows(Arc::clone(&model), 40);
    write(
        &mut store,
        "services",
        Op::Write,
        r#"[{"key": "api", "team": "web"}, {"key": "db", "team": "data"}, {"key": "worker", "team": "web"}]"#,
    );
    let engine = Engine::default();
    let id = field(&model, "logs", "id");
    let message = field(&model, "logs", "message");
    let level = field(&model, "logs", "level");
    let team = field(&model, "services", "team");
    let query = |selection: Selection, limit| RowsQuery {
        fields: vec![id.clone(), message.clone(), team.clone()],
        selection,
        limit,
    };
    let search = |text: &str| Selection([(message.clone(), contains(text))].into_iter().collect());

    let mut live: Vec<i64> = Vec::new();
    for (round, (from, to)) in [(0, 30), (30, 95), (95, 96), (96, 260)]
        .into_iter()
        .enumerate()
    {
        write(&mut store, "logs", Op::Write, &lines(from, to));
        live.extend(from..to);
        // the second round deletes some timed-out lines already shown
        if round == 2 {
            write(
                &mut store,
                "logs",
                Op::Delete,
                r#"[{"id": 91}, {"id": 84}, {"id": 7}]"#,
            );
            live.retain(|i| ![91, 84, 7].contains(i));
        }
        let snapshot = publish(&mut store);

        // a phrase in other case, in a plain column
        assert_eq!(
            agreed(&snapshot, &engine, &query(search("lock timeout"), 4)),
            expected(&live, 4, timeout),
            "round {round}"
        );
        // more than there are: every one
        assert_eq!(
            agreed(&snapshot, &engine, &query(search("LOCK"), 1000)),
            expected(&live, 1000, timeout),
            "round {round}"
        );
        // no selection: the newest rows
        assert_eq!(
            agreed(&snapshot, &engine, &query(Selection::default(), 3)),
            expected(&live, 3, |_| true),
            "round {round}"
        );
        // with a dictionary condition, and a search of a related field
        let mut both = search("timeout");
        both.0.insert(level.clone(), contains("err"));
        both.0.insert(team.clone(), contains("WE"));
        assert_eq!(
            agreed(&snapshot, &engine, &query(both, 2)),
            expected(&live, 2, |i| timeout(i) && i % 5 == 0 && i % 3 != 1),
            "round {round}"
        );
        // nothing matches
        assert_eq!(
            agreed(&snapshot, &engine, &query(search("deadlock"), 5)),
            Vec::<i64>::new()
        );
    }
}

#[test]
fn a_search_is_a_selection_for_every_metric() {
    let model = model();
    let mut store = Store::with_seal_rows(Arc::clone(&model), 25);
    write(&mut store, "logs", Op::Write, &lines(0, 120));
    let first = publish(&mut store);
    write(&mut store, "logs", Op::Write, &lines(120, 300));
    let second = publish(&mut store);
    let Some(lines_metric) = model.metrics.iter().find(|m| m.name == "lines") else {
        panic!("the model has `lines`");
    };
    let message = field(&model, "logs", "message");
    let engine = Engine::default();
    // typing: each needle narrows the one before
    for (snapshot, upto) in [(&first, 120), (&second, 300)] {
        for text in ["t", "ti", "tim", "TIMEOUT", "timeouts", "", "1 "] {
            let query = Query {
                metrics: vec![lines_metric.id],
                selection: Selection([(message.clone(), contains(text))].into_iter().collect()),
                ..Query::default()
            };
            let expected = (0..upto)
                .filter(|i| i % 11 != 0)
                .filter(|i| {
                    let line = if i % 7 == 0 {
                        format!("canceling statement {i} due to Lock Timeout")
                    } else {
                        format!("statement {i} took {} ms", i * 3)
                    };
                    line.to_ascii_lowercase()
                        .contains(&text.to_ascii_lowercase())
                })
                .count();
            let want = vec![vec![Scalar::Int(i64::try_from(expected).unwrap_or(0))]];
            for (path, answer) in [
                ("fast", run(snapshot, &query)),
                ("general", run_general(snapshot, &query)),
                ("cached", engine.run(snapshot, &query)),
            ] {
                match answer {
                    Ok(answer) => assert_eq!(answer.rows, want, "{path}, `{text}`"),
                    Err(e) => panic!("{path}: {e}"),
                }
            }
        }
    }
}
