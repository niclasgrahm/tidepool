//! Time grains shorter than a day: every path (fast, general, cached) groups
//! by the same buckets, of the field's own type, and a date has none.

use std::sync::Arc;

use tidepool_config::ConfigSet;
use tidepool_config::ossie::TimeGrain;
use tidepool_engine::{Dim, Engine, Query, QueryError, ResultSet, run, run_general};
use tidepool_expr::Scalar;
use tidepool_expr::temporal::{parse_timestamp, parse_timestamptz};
use tidepool_model::ir::{FieldRef, Model};
use tidepool_model::{Catalog, compile};
use tidepool_store::batch::{Shape, json_array};
use tidepool_store::{Op, Snapshot, Store};

const EVENTS: &str = "name: events
mode: append
columns:
  plain_at: { type: timestamp, encoding: plain }
  coded_at: { type: timestamptz, encoding: dict }
  day: { type: date }
";

const MODEL: &str = r#"version: "0.2.0"
name: events
datasets:
  - name: events
    source: events
metrics:
  - name: rows
    expression: { dialects: [{ dialect: OSSIE_SQL_2026, expression: "COUNT(*)" }] }
    custom_extensions:
      - vendor_name: TIDEPOOL
        data: '{"table": "events"}'
"#;

fn model() -> Arc<Model> {
    let (set, diagnostics) = ConfigSet::from_files(vec![
        ("tables/events.yaml".to_string(), EVENTS.to_string()),
        ("model.ossie.yaml".to_string(), MODEL.to_string()),
    ]);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    match compile(&set, &Catalog::default()) {
        (Some(compiled), _) => Arc::new(compiled.model),
        (None, d) => panic!("the events model should compile: {d:?}"),
    }
}

/// Rows at these times of day, across a seal, one before 1970 (buckets start
/// earlier, not later) and one at the very start of a bucket.
fn snapshot(model: &Arc<Model>) -> Snapshot {
    let times = [
        "1969-12-31T23:30:15",
        "2026-11-03T19:05:30",
        "2026-11-03T19:05:59.999999",
        "2026-11-03T19:06:00",
        "2026-11-03T20:00:00",
    ];
    let rows: Vec<String> = times
        .iter()
        .map(|t| format!(r#"{{"plain_at": "{t}", "coded_at": "{t}Z", "day": "2026-11-03"}}"#))
        .collect();
    let json = format!("[{}]", rows.join(","));
    let mut store = Store::with_seal_rows(Arc::clone(model), 2);
    let Some(spec) = model.table_named("events") else {
        panic!("no table events");
    };
    let batch = match json_array(spec, Shape::Rows, json.as_bytes()) {
        Ok(batch) => batch,
        Err(rejected) => panic!("{rejected:?}"),
    };
    store.apply(spec.id, Op::Write, &batch);
    match store.publish() {
        Some(snapshot) => snapshot,
        None => panic!("expected a new epoch"),
    }
}

fn query(model: &Model, column: &str, grain: TimeGrain) -> Query {
    let (Some(table), Some(rows)) = (
        model.table_named("events"),
        model.metrics.iter().find(|m| m.name == "rows"),
    ) else {
        panic!("the model has `events` and `rows`");
    };
    Query {
        dimensions: vec![Dim {
            field: FieldRef {
                table: table.id,
                field: column.to_string(),
            },
            grain: Some(grain),
            level: None,
        }],
        metrics: vec![rows.id],
        ..Query::default()
    }
}

/// `(bucket, rows)`, sorted, the same on every path.
fn answer(snapshot: &Snapshot, query: &Query) -> Vec<(Scalar, i64)> {
    let of = |result: Result<ResultSet, QueryError>| -> Vec<(Scalar, i64)> {
        let result = match result {
            Ok(result) => result,
            Err(e) => panic!("{e:?}"),
        };
        let mut rows: Vec<(Scalar, i64)> = result
            .rows
            .iter()
            .map(|row| match row.as_slice() {
                [bucket, Scalar::Int(n)] => (bucket.clone(), *n),
                other => panic!("a bucket and a count, not {other:?}"),
            })
            .collect();
        rows.sort_by_key(|(bucket, _)| match bucket {
            Scalar::Timestamp(m) | Scalar::TimestampTz(m) => *m,
            other => panic!("a bucket with a time, not {other:?}"),
        });
        rows
    };
    let fast = of(run(snapshot, query));
    assert_eq!(fast, of(run_general(snapshot, query)), "general path");
    assert_eq!(fast, of(Engine::default().run(snapshot, query)), "cached");
    fast
}

#[test]
fn hours_and_minutes_are_buckets_of_the_fields_own_type() {
    let model = model();
    let snapshot = snapshot(&model);
    let plain = |t: &str| match parse_timestamp(t) {
        Some(m) => Scalar::Timestamp(m),
        None => panic!("{t} is a timestamp"),
    };
    let zoned = |t: &str| match parse_timestamptz(t) {
        Some(m) => Scalar::TimestampTz(m),
        None => panic!("{t} is a timestamp"),
    };
    assert_eq!(
        answer(&snapshot, &query(&model, "plain_at", TimeGrain::Hour)),
        vec![
            (plain("1969-12-31T23:00:00"), 1),
            (plain("2026-11-03T19:00:00"), 3),
            (plain("2026-11-03T20:00:00"), 1),
        ]
    );
    assert_eq!(
        answer(&snapshot, &query(&model, "plain_at", TimeGrain::Minute)),
        vec![
            (plain("1969-12-31T23:30:00"), 1),
            (plain("2026-11-03T19:05:00"), 2),
            (plain("2026-11-03T19:06:00"), 1),
            (plain("2026-11-03T20:00:00"), 1),
        ]
    );
    // a dictionary column, with a zone
    assert_eq!(
        answer(&snapshot, &query(&model, "coded_at", TimeGrain::Hour)),
        vec![
            (zoned("1969-12-31T23:00:00Z"), 1),
            (zoned("2026-11-03T19:00:00Z"), 3),
            (zoned("2026-11-03T20:00:00Z"), 1),
        ]
    );
}

#[test]
fn a_date_has_no_hours() {
    let model = model();
    let snapshot = snapshot(&model);
    for grain in [TimeGrain::Hour, TimeGrain::Minute] {
        let Err(QueryError(message)) = run(&snapshot, &query(&model, "day", grain)) else {
            panic!("{grain:?} of a date should be refused");
        };
        assert!(message.contains("no time of day"), "{message}");
    }
}
