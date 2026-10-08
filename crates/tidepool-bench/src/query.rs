//! Query latency on the plan's reference dataset (§2.3): retail `orders`
//! with 1M customers and 100k products, built straight into a store (no
//! HTTP, no JSON) so building it doesn't dominate the run.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tidepool_engine::{Condition, Dim, Engine, Query, Selection};
use tidepool_expr::Scalar;
use tidepool_model::Model;
use tidepool_model::ir::FieldRef;
use tidepool_store::value::Parsed;
use tidepool_store::{Batch, Op, Snapshot, Store};

const REGIONS: [&str; 6] = ["north", "south", "east", "west", "central", "islands"];
const STATUSES: [&str; 4] = ["paid", "shipped", "returned", "cancelled"];

fn mix(i: u64) -> u64 {
    let mut z = i.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn batch(
    model: &Model,
    table: &str,
    rows: std::ops::Range<u64>,
    cell: impl Fn(&str, u64) -> Option<Parsed>,
) -> Batch {
    let Some(spec) = model.table_named(table) else {
        panic!("no table {table}");
    };
    let columns = spec
        .columns
        .iter()
        .map(|c| rows.clone().map(|i| cell(&c.name, i)).collect())
        .collect();
    Batch {
        rows: usize::try_from(rows.end - rows.start).unwrap_or(0),
        columns,
    }
}

const CUSTOMERS: u64 = 1_000_000;
const PRODUCTS: u64 = 100_000;

/// Orders with ids `ids`.
fn orders_batch(model: &Model, ids: std::ops::Range<u64>) -> Batch {
    let s = |text: String| Some(Parsed::Str(text));
    batch(model, "orders", ids, |column, i| {
        let m = mix(i);
        match column {
            "order_id" => s(format!("o{i}")),
            "customer_id" => s(format!("c{}", m % CUSTOMERS)),
            "product_id" => s(format!("p{}", (m >> 20) % PRODUCTS)),
            "status" => s(STATUSES[((m >> 40) % 4) as usize].to_string()),
            "ordered_at" => Some(Parsed::I64(
                1_767_225_600_000_000 + i64::try_from(i % 31_536_000).unwrap_or(0) * 1_000_000,
            )),
            "quantity" => Some(Parsed::I64(i64::try_from((m >> 44) % 9 + 1).unwrap_or(1))),
            "amount" => Some(Parsed::I64(i64::try_from((m >> 24) % 50_000).unwrap_or(0))),
            _ => None,
        }
    })
}

/// The reference dataset, with `orders` orders, and the store holding it.
pub fn build(model: &Arc<Model>, orders: u64) -> (Store, Snapshot) {
    let mut store = Store::new(Arc::clone(model));
    let id = |name: &str| {
        model
            .table_named(name)
            .map_or_else(|| panic!("no {name}"), |t| t.id)
    };
    let s = |text: String| Some(Parsed::Str(text));

    let (customers, products) = (CUSTOMERS, PRODUCTS);
    let started = Instant::now();
    let b = batch(model, "customers", 0..customers, |column, i| match column {
        "customer_id" => s(format!("c{i}")),
        "name" => s(format!("Customer {i}")),
        "region" => s(REGIONS[(mix(i) % 6) as usize].to_string()),
        "segment" => {
            (!mix(i + 7).is_multiple_of(5)).then(|| Parsed::Str(format!("seg{}", mix(i + 7) % 4)))
        }
        _ => None,
    });
    store.apply(id("customers"), Op::Write, &b);
    let b = batch(model, "products", 0..products, |column, i| match column {
        "product_id" => s(format!("p{i}")),
        "name" => s(format!("Product {i}")),
        "category" => s(format!("category {}", mix(i) % 20)),
        "list_price" => Some(Parsed::I64(i64::try_from(mix(i) % 100_000).unwrap_or(0))),
        _ => None,
    });
    store.apply(id("products"), Op::Write, &b);
    let mut last = store.publish();

    let chunk = 1_000_000u64;
    let mut start = 0;
    while start < orders {
        let end = (start + chunk).min(orders);
        store.apply(id("orders"), Op::Write, &orders_batch(model, start..end));
        last = store.publish().or(last);
        start = end;
    }
    // publish() is None when nothing changed: keep the last real snapshot
    let snapshot = last.unwrap_or_else(|| Snapshot::empty(Arc::clone(model)));
    let live = model
        .table_named("orders")
        .and_then(|t| snapshot.table(t.id))
        .map_or(0, |t| t.live_rows);
    assert_eq!(live, orders, "every order should be in the snapshot");
    println!(
        "  built {orders} orders in {:.1}s",
        started.elapsed().as_secs_f64()
    );
    (store, snapshot)
}

/// Per-epoch cost of keeping the reference query live: each epoch appends
/// `rows` orders, and the engine brings its kept answer up to date. Should
/// depend on `rows`, not on how many orders there already are.
pub fn measure_epochs(store: &mut Store, snapshot: Snapshot, epochs: usize, rows: u64) {
    let model = Arc::clone(&snapshot.model);
    let metric = |name: &str| {
        model
            .metrics
            .iter()
            .find(|m| m.name == name)
            .map_or_else(|| panic!("no metric {name}"), |m| m.id)
    };
    let orders_id = model
        .table_named("orders")
        .map_or_else(|| panic!("no orders"), |t| t.id);
    let mut next = snapshot.table(orders_id).map_or(0, |t| t.live_rows);
    let mut current = snapshot;
    for metrics in [
        ["revenue", "order_count", "customer_count"],
        ["revenue", "order_count", "paid_revenue"],
    ] {
        let query = Query {
            dimensions: vec![
                Dim {
                    field: field(&model, "customers", "region"),
                    grain: None,
                },
                Dim {
                    field: field(&model, "products", "category"),
                    grain: None,
                },
            ],
            metrics: metrics.iter().map(|m| metric(m)).collect(),
            ..Query::default()
        };
        let engine = Engine::default();
        let _ = engine
            .run(&current, &query)
            .unwrap_or_else(|e| panic!("{e}"));
        let mut times = Vec::new();
        for _ in 0..epochs {
            store.apply(
                orders_id,
                Op::Write,
                &orders_batch(&model, next..next + rows),
            );
            next += rows;
            if let Some(published) = store.publish() {
                current = published;
            }
            let started = Instant::now();
            let _ = engine
                .run(&current, &query)
                .unwrap_or_else(|e| panic!("{e}"));
            times.push(started.elapsed());
        }
        println!(
            "  epoch of {rows} new orders ({}):  p50 {:>6.1} ms   p95 {:>6.1} ms",
            metrics.join(", "),
            percentile(&mut times, 0.5),
            percentile(&mut times, 0.95)
        );
    }
}

fn field(model: &Model, dataset: &str, field: &str) -> FieldRef {
    FieldRef {
        table: model
            .table_named(dataset)
            .map_or_else(|| panic!("no {dataset}"), |t| t.id),
        field: field.to_string(),
    }
}

fn percentile(times: &mut [Duration], p: f64) -> f64 {
    times.sort();
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let index = ((times.len() as f64 - 1.0) * p).round() as usize;
    times[index].as_secs_f64() * 1000.0
}

/// Time the reference query (two dimensions, three metrics) with and
/// without a distinct count, the most expensive aggregate.
pub fn measure(snapshot: &Snapshot, runs: usize, only: Option<&str>) {
    let sets: Vec<Vec<&str>> = match only {
        Some(list) => vec![list.split(',').map(str::trim).collect()],
        None => vec![
            vec!["revenue", "order_count", "customer_count"],
            vec!["revenue", "order_count", "paid_revenue"],
        ],
    };
    for metrics in sets {
        println!("  metrics: {}", metrics.join(", "));
        measure_metrics(snapshot, &metrics, "region", runs);
    }
    if only.is_none() {
        // a derived field over a dictionary column groups like a stored one
        println!("  metrics: revenue, order_count, paid_revenue (by a derived field)");
        measure_metrics(
            snapshot,
            &["revenue", "order_count", "paid_revenue"],
            "coast",
            runs,
        );
    }
}

fn measure_metrics(snapshot: &Snapshot, metrics: &[&str], by: &str, runs: usize) {
    let model = &snapshot.model;
    let metric = |name: &str| {
        model
            .metrics
            .iter()
            .find(|m| m.name == name)
            .map_or_else(|| panic!("no metric {name}"), |m| m.id)
    };
    let query = Query {
        dimensions: vec![
            Dim {
                field: field(model, "customers", by),
                grain: None,
            },
            Dim {
                field: field(model, "products", "category"),
                grain: None,
            },
        ],
        metrics: metrics.iter().map(|m| metric(m)).collect(),
        ..Query::default()
    };
    let started = Instant::now();
    // a fresh engine: nothing cached from the other metric set
    let engine = Engine::default();
    let result = engine
        .run(snapshot, &query)
        .unwrap_or_else(|e| panic!("{e}"));
    println!(
        "    cold: {by} × category, all orders     {:>8.1} ms  ({} groups)",
        started.elapsed().as_secs_f64() * 1000.0,
        result.rows.len()
    );

    // every selection a distinct (regions, statuses) combination, in a
    // scrambled order: each one new to the engine, as a click would be
    let mut combos: Vec<(u64, u64)> = (1..64u64)
        .flat_map(|r| (0..16u64).map(move |s| (r, s)))
        .collect();
    combos.sort_by_key(|(r, s)| mix(r * 16 + s));
    combos.truncate(runs);
    let selection = |(regions, statuses): (u64, u64)| {
        let pick = |mask: u64, names: &[&str]| -> Vec<Scalar> {
            names
                .iter()
                .enumerate()
                .filter(|(i, _)| mask >> i & 1 == 1)
                .map(|(_, n)| Scalar::String((*n).into()))
                .collect()
        };
        let mut selection = Selection::default();
        selection.0.insert(
            field(model, "customers", "region"),
            Condition::In(pick(regions, &REGIONS)),
        );
        if statuses != 0 {
            selection.0.insert(
                field(model, "orders", "status"),
                Condition::In(pick(statuses, &STATUSES)),
            );
        }
        Query {
            selection,
            ..query.clone()
        }
    };
    let time = |label: &str, combos: &[(u64, u64)]| {
        let mut times: Vec<Duration> = combos
            .iter()
            .map(|combo| {
                let selected = selection(*combo);
                let started = Instant::now();
                let _ = engine
                    .run(snapshot, &selected)
                    .unwrap_or_else(|e| panic!("{e}"));
                started.elapsed()
            })
            .collect();
        println!(
            "    {label:<32} min {:>6.1} ms   p50 {:>6.1} ms   p95 {:>6.1} ms",
            percentile(&mut times, 0.0),
            percentile(&mut times, 0.5),
            percentile(&mut times, 0.95)
        );
    };
    // join columns are reused (they depend on the data only); answers aren't
    time(&format!("fresh selections ({}):", combos.len()), &combos);
    time(&format!("toggled back ({}):", combos.len()), &combos);
}

/// Restart time (plan §2.3): the reference dataset checkpointed to `dir`,
/// everything dropped, then recovered from disk.
pub fn measure_restart(model: &Arc<Model>, orders: u64, dir: &std::path::Path) {
    use tidepool_config::server::Durability;
    use tidepool_store::persist::Durable;
    let (store, snapshot) = build(model, orders);
    drop(store);
    let (durable, _, mut checkpoints) = Durable::open(
        Arc::clone(model),
        dir,
        tidepool_store::store::DEFAULT_SEAL_ROWS,
        Durability::Fsync,
    )
    .unwrap_or_else(|e| panic!("{e}"));
    drop(durable);
    let started = Instant::now();
    checkpoints
        .write(&snapshot, 0)
        .unwrap_or_else(|e| panic!("{e}"));
    println!(
        "  checkpoint written in {:.1}s",
        started.elapsed().as_secs_f64()
    );
    drop(snapshot);
    drop(checkpoints);

    let started = Instant::now();
    let (_durable, recovered, _) = Durable::open(
        Arc::clone(model),
        dir,
        tidepool_store::store::DEFAULT_SEAL_ROWS,
        Durability::Fsync,
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let elapsed = started.elapsed();
    let live = model
        .table_named("orders")
        .and_then(|t| recovered.table(t.id))
        .map_or(0, |t| t.live_rows);
    assert_eq!(live, orders, "every order should be recovered");
    println!(
        "  restart: {orders} orders recovered in {:.2}s",
        elapsed.as_secs_f64()
    );
}
