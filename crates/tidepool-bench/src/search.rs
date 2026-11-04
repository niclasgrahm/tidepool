//! Text search and `rows` (plan §5.7) on the Postgres logs example's model:
//! synthetic log lines of about 150 bytes, built straight into a store,
//! then searched the ways a dashboard does. Every search is a fresh needle
//! on a fresh engine unless it says otherwise, so the numbers are a scan,
//! not a cached answer.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tidepool_config::ConfigSet;
use tidepool_engine::{Condition, Engine, Needle, Query, RowsQuery, Selection};
use tidepool_model::ir::FieldRef;
use tidepool_model::{Catalog, Model, compile};
use tidepool_store::value::Parsed;
use tidepool_store::{Batch, Op, Snapshot, Store};

const SEVERITIES: [&str; 6] = ["LOG", "LOG", "LOG", "DEBUG", "WARNING", "ERROR"];
const TABLES: [&str; 5] = ["orders", "customers", "payments", "sessions", "audit"];

fn mix(i: u64) -> u64 {
    let mut z = i.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Line `i`'s text: mostly statement durations, now and then an error. One
/// in a thousand is a deadlock.
fn message(i: u64) -> String {
    let m = mix(i);
    let table = TABLES[(m % 5) as usize];
    match (m >> 8) % 1000 {
        0 => format!(
            "deadlock detected: process {} waits for ShareLock on transaction {}; blocked by process {}",
            m % 9000,
            i,
            (m >> 20) % 9000
        ),
        1..=20 => format!(
            "duplicate key value violates unique constraint \"{table}_pkey\" (id)=({})",
            m % 100_000
        ),
        21..=60 => format!(
            "canceling statement due to statement timeout while scanning relation \"{table}\" at block {}",
            m % 4096
        ),
        _ => format!(
            "duration: {}.{:03} ms  statement: SELECT id, status, amount FROM {table} WHERE customer_id = {} ORDER BY id LIMIT 20",
            (m >> 12) % 40,
            (m >> 24) % 1000,
            m % 1_000_000
        ),
    }
}

/// The first line's time: 2026-11-04, in microseconds.
const START: i64 = 1_793_750_400_000_000;

fn batch(model: &Model, rows: std::ops::Range<u64>) -> Batch {
    let Some(spec) = model.table_named("logs") else {
        panic!("no logs table");
    };
    let columns = spec
        .columns
        .iter()
        .map(|c| {
            rows.clone()
                .map(|i| {
                    let m = mix(i);
                    let at = START + i64::try_from(i).unwrap_or(0) * 1_000;
                    let s = |text: &str| Some(Parsed::Str(text.to_string()));
                    match c.name.as_str() {
                        "logged_at" => Some(Parsed::I64(at)),
                        "minute" => Some(Parsed::I64(at - at.rem_euclid(60_000_000))),
                        "source" => s(if m.is_multiple_of(7) {
                            "app"
                        } else {
                            "postgres"
                        }),
                        "severity" => s(SEVERITIES[((m >> 33) % 6) as usize]),
                        "database" => s(["shop", "shop", "postgres"][((m >> 40) % 3) as usize]),
                        "application" => s(["web", "api", "worker"][((m >> 44) % 3) as usize]),
                        "pid" => Some(Parsed::I64(i64::try_from(m % 9000).unwrap_or(0))),
                        "message" => Some(Parsed::Str(message(i))),
                        _ => None,
                    }
                })
                .collect()
        })
        .collect();
    Batch {
        rows: usize::try_from(rows.end - rows.start).unwrap_or(0),
        columns,
        shape: Vec::new(),
    }
}

fn model() -> Arc<Model> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/pglogs/tidepool");
    let (set, _) = ConfigSet::load_dir(&dir).unwrap_or_else(|e| panic!("reading the example: {e}"));
    match compile(&set, &Catalog::default()) {
        (Some(compiled), _) => Arc::new(compiled.model),
        (None, d) => panic!("the example should compile: {d:?}"),
    }
}

fn ms(d: Duration) -> String {
    format!("{:.1} ms", d.as_secs_f64() * 1000.0)
}

/// The best of `runs` timings of `f`, each on what `fresh` gives.
fn best<T, R>(
    runs: usize,
    mut fresh: impl FnMut(usize) -> T,
    mut f: impl FnMut(T) -> R,
) -> Duration {
    (0..runs.max(1))
        .map(|run| {
            let input = fresh(run);
            let started = Instant::now();
            std::hint::black_box(f(input));
            started.elapsed()
        })
        .min()
        .unwrap_or_default()
}

#[allow(clippy::too_many_lines, clippy::cast_precision_loss)] // one measurement after another
pub fn measure(lines: u64, runs: usize) {
    let model = model();
    let field = |table: &str, name: &str| FieldRef {
        table: model
            .table_named(table)
            .map_or_else(|| panic!("no {table}"), |t| t.id),
        field: name.to_string(),
    };
    let (message, severity) = (field("logs", "message"), field("logs", "severity"));
    let Some(lines_metric) = model.metrics.iter().find(|m| m.name == "lines") else {
        panic!("the example has `lines`");
    };
    let logs = model
        .table_named("logs")
        .map_or_else(|| panic!("no logs"), |t| t.id);

    let started = Instant::now();
    let mut store = Store::new(Arc::clone(&model));
    let mut last = None;
    let chunk = 1_000_000u64;
    let mut start = 0;
    while start < lines {
        let end = (start + chunk).min(lines);
        store.apply(logs, Op::Write, &batch(&model, start..end));
        last = store.publish().or(last);
        start = end;
    }
    let snapshot: Snapshot = last.unwrap_or_else(|| Snapshot::empty(Arc::clone(&model)));
    let stored = snapshot
        .table(logs)
        .map_or(0, tidepool_store::TableSnapshot::bytes);
    // the text searched: the message column's bytes
    let position = model
        .table_named("logs")
        .and_then(|t| t.columns.iter().position(|c| c.name == "message"))
        .unwrap_or(0);
    let bytes: usize = snapshot.table(logs).map_or(0, |t| {
        t.segments
            .iter()
            .map(|s| s.data.columns[position].bytes())
            .sum()
    });
    println!(
        "  built {lines} lines ({:.0} MB stored, {:.0} MB of it the text) in {:.1}s; {} cores\n",
        stored as f64 / 1e6,
        bytes as f64 / 1e6,
        started.elapsed().as_secs_f64(),
        std::thread::available_parallelism().map_or(1, std::num::NonZero::get)
    );

    let search = |text: &str| Condition::Contains(Arc::new(Needle::new(text)));
    let count = |text: &str, extra: Option<(&FieldRef, Condition)>| {
        let mut selection = Selection::default();
        selection.0.insert(message.clone(), search(text));
        if let Some((f, c)) = extra {
            selection.0.insert(f.clone(), c);
        }
        Query {
            metrics: vec![lines_metric.id],
            selection,
            ..Query::default()
        }
    };
    let counted =
        |answer: Result<tidepool_engine::ResultSet, tidepool_engine::QueryError>| match answer {
            Ok(a) => a.rows.first().and_then(|r| r.first()).cloned(),
            Err(e) => panic!("{e}"),
        };

    println!("  a new search, counted (a fresh engine each run):");
    for (text, what) in [
        ("deadlock", "rare, letters"),
        ("Statement Timeout", "4%, letters"),
        ("duration", "94%, letters"),
        ("=(4", "rare, no letters"),
        ("zzqx", "nowhere"),
    ] {
        let found = counted(tidepool_engine::run(&snapshot, &count(text, None)));
        let took = best(
            runs,
            |_| Engine::default(),
            |engine| engine.run(&snapshot, &count(text, None)),
        );
        let gbs = bytes as f64 / took.as_secs_f64() / 1e9;
        println!(
            "    {text:<20} {what:<18} {:>12}  {:>9}  (~{gbs:.1} GB/s of text)",
            format!("{found:?}"),
            ms(took)
        );
    }

    println!("\n  the same needle under another selection (its hits cached):");
    let engine = Engine::default();
    let _ = engine.run(&snapshot, &count("timeout", None));
    for level in ["ERROR", "LOG", "WARNING"] {
        let level_is = Condition::In(vec![tidepool_expr::Scalar::String(level.into())]);
        let query = count("timeout", Some((&severity, level_is)));
        let started = Instant::now();
        let found = counted(engine.run(&snapshot, &query));
        println!(
            "    severity {level:<8} {:>12}  {:>9}",
            format!("{found:?}"),
            ms(started.elapsed())
        );
    }

    println!("\n  typing `deadlock`, a key at a time (one engine, as a session):");
    let engine = Engine::default();
    let word = "deadlock";
    let mut total = Duration::ZERO;
    for n in 1..=word.len() {
        let started = Instant::now();
        let found = counted(engine.run(&snapshot, &count(&word[..n], None)));
        let took = started.elapsed();
        total += took;
        println!(
            "    {:<10} {:>12}  {:>9}",
            &word[..n],
            format!("{found:?}"),
            ms(took)
        );
    }
    println!("    all {} keys: {}", word.len(), ms(total));

    let rows = |text: &str, limit: usize| RowsQuery {
        fields: vec![
            field("logs", "logged_at"),
            field("logs", "severity"),
            field("logs", "message"),
        ],
        selection: Selection([(message.clone(), search(text))].into_iter().collect()),
        limit,
    };
    println!("\n  the newest 200 rows (a fresh engine each run):");
    for text in ["duration", "statement timeout", "deadlock", "zzqx"] {
        let took = best(
            runs,
            |_| Engine::default(),
            |engine| engine.rows(&snapshot, &rows(text, 200)),
        );
        let shown = Engine::default()
            .rows(&snapshot, &rows(text, 200))
            .map_or(0, |r| r.rows.len());
        println!("    {text:<20} {shown:>4} rows  {:>9}", ms(took));
    }

    println!("\n  live: 100k lines more, then the same views (one engine kept):");
    let engine = Engine::default();
    let views = [count("deadlock", None), count("duration", None)];
    for view in &views {
        let _ = engine.run(&snapshot, view);
    }
    let _ = engine.rows(&snapshot, &rows("deadlock", 200));
    store.apply(logs, Op::Write, &batch(&model, lines..lines + 100_000));
    let next = store.publish().unwrap_or(snapshot);
    for (view, name) in views.iter().zip(["count `deadlock`", "count `duration`"]) {
        let started = Instant::now();
        let found = counted(engine.run(&next, view));
        println!(
            "    {name:<20} {:>12}  {:>9}",
            format!("{found:?}"),
            ms(started.elapsed())
        );
    }
    let started = Instant::now();
    let shown = engine
        .rows(&next, &rows("deadlock", 200))
        .map_or(0, |r| r.rows.len());
    println!(
        "    {:<20} {shown:>4} rows  {:>9}",
        "rows `deadlock`",
        ms(started.elapsed())
    );
}
