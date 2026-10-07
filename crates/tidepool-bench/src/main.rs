//! The throughput and latency harness. Run with `just bench` (release
//! build); deliberately not part of `just ci`.
//!
//! `ingest` measures writes the way a client sees them: a real server on a
//! local port, the retail example's `orders` table, batches posted
//! concurrently in each format, timed until the last batch is acknowledged
//! *and visible*. (Requests don't each wait for visibility: that would
//! measure the epoch interval, not the server.) Bodies are generated before the clock
//! starts, so the numbers are the server's, not the generator's. It also times
//! the store alone (decode + apply, no HTTP) as a reference point.
//!
//! The targets these are measured against are in docs/plan.md §2.3.

mod query;

use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arrow_array::{ArrayRef, Int32Array, RecordBatch, StringArray, TimestampMicrosecondArray};
use arrow_ipc::writer::StreamWriter;
use arrow_schema::{DataType, Field, Schema, TimeUnit};
use clap::{Parser, Subcommand};
use tidepool_config::ConfigSet;
use tidepool_model::{Catalog, Model, compile};
use tidepool_server::{AppState, Tokens, router};
use tidepool_store::batch::{Shape, ndjson};
use tidepool_store::{Op, Store, WriterConfig};

#[derive(Parser)]
#[command(name = "tidepool-bench")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Write throughput, per format.
    Ingest {
        /// Rows to write per format.
        #[arg(long, default_value_t = 2_000_000)]
        rows: usize,
        /// Rows per request.
        #[arg(long, default_value_t = 10_000)]
        batch: usize,
        /// Concurrent clients.
        #[arg(long, default_value_t = 8)]
        clients: usize,
    },
    /// Query latency on the reference dataset.
    Query {
        /// Orders to build.
        #[arg(long, default_value_t = 10_000_000)]
        orders: u64,
        /// Selection changes to time.
        #[arg(long, default_value_t = 20)]
        runs: usize,
        /// Only this metric set (comma-separated) instead of the standard two.
        #[arg(long)]
        metrics: Option<String>,
    },
}

fn model() -> Arc<Model> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/retail");
    let (set, _) = ConfigSet::load_dir(&dir).unwrap_or_else(|e| panic!("reading the example: {e}"));
    match compile(&set, &Catalog::default()) {
        (Some(compiled), _) => Arc::new(compiled.model),
        (None, d) => panic!("the example should compile: {d:?}"),
    }
}

/// A deterministic, realistic-ish order: 50k customers, 2k products, a few statuses.
struct Order {
    id: usize,
    customer: usize,
    product: usize,
    status: &'static str,
    at_micros: i64,
    quantity: i32,
    cents: i64,
}

fn order(i: usize) -> Order {
    let mix = i.wrapping_mul(2_654_435_761) ^ (i >> 7);
    Order {
        id: i,
        customer: mix % 50_000,
        product: (mix >> 8) % 2_000,
        status: ["paid", "shipped", "returned", "cancelled"][mix % 4],
        at_micros: 1_790_000_000_000_000 + i64::try_from(i).unwrap_or(0) * 1_000_000,
        quantity: i32::try_from(mix % 9).unwrap_or(0) + 1,
        cents: i64::try_from(mix % 100_000).unwrap_or(0),
    }
}

fn ndjson_body(start: usize, rows: usize) -> Vec<u8> {
    let mut out = String::with_capacity(rows * 200);
    for i in start..start + rows {
        let o = order(i);
        let _ = writeln!(
            out,
            r#"{{"order_id":"o{}","customer_id":"c{}","product_id":"p{}","status":"{}","ordered_at":"{}","quantity":{},"amount":"{}.{:02}"}}"#,
            o.id,
            o.customer,
            o.product,
            o.status,
            tidepool_store::value::format_timestamp(o.at_micros, true),
            o.quantity,
            o.cents / 100,
            o.cents % 100
        );
    }
    out.into_bytes()
}

fn arrow_body(start: usize, rows: usize) -> Vec<u8> {
    let orders: Vec<Order> = (start..start + rows).map(order).collect();
    let strings = |f: &dyn Fn(&Order) -> String| -> ArrayRef {
        Arc::new(StringArray::from(orders.iter().map(f).collect::<Vec<_>>()))
    };
    let schema = Arc::new(Schema::new(vec![
        Field::new("order_id", DataType::Utf8, false),
        Field::new("customer_id", DataType::Utf8, false),
        Field::new("product_id", DataType::Utf8, false),
        Field::new("status", DataType::Utf8, false),
        Field::new(
            "ordered_at",
            DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
            false,
        ),
        Field::new("quantity", DataType::Int32, false),
        Field::new("amount", DataType::Utf8, false),
    ]));
    let columns: Vec<ArrayRef> = vec![
        strings(&|o| format!("o{}", o.id)),
        strings(&|o| format!("c{}", o.customer)),
        strings(&|o| format!("p{}", o.product)),
        strings(&|o| o.status.to_string()),
        Arc::new(
            TimestampMicrosecondArray::from(orders.iter().map(|o| o.at_micros).collect::<Vec<_>>())
                .with_timezone("UTC"),
        ),
        Arc::new(Int32Array::from(
            orders.iter().map(|o| o.quantity).collect::<Vec<_>>(),
        )),
        strings(&|o| format!("{}.{:02}", o.cents / 100, o.cents % 100)),
    ];
    let record = RecordBatch::try_new(schema, columns).unwrap_or_else(|e| panic!("{e}"));
    let mut out = Vec::new();
    let mut writer =
        StreamWriter::try_new(&mut out, &record.schema()).unwrap_or_else(|e| panic!("{e}"));
    writer.write(&record).unwrap_or_else(|e| panic!("{e}"));
    writer.finish().unwrap_or_else(|e| panic!("{e}"));
    drop(writer);
    out
}

#[allow(clippy::cast_precision_loss)]
fn rate(rows: usize, elapsed: Duration) -> String {
    let per_second = rows as f64 / elapsed.as_secs_f64();
    format!(
        "{:>9.0} rows/s  ({rows} rows in {:.2}s)",
        per_second,
        elapsed.as_secs_f64()
    )
}

/// Decode + apply in process: the store's ceiling, with no HTTP or threads.
fn store_only(model: &Arc<Model>, rows: usize, batch: usize) -> String {
    let Some(orders) = model.table_named("orders") else {
        panic!("no orders table");
    };
    let bodies: Vec<Vec<u8>> = (0..rows / batch)
        .map(|b| ndjson_body(b * batch, batch))
        .collect();
    let mut store = Store::new(Arc::clone(model));
    let started = Instant::now();
    for body in &bodies {
        let decoded = ndjson(orders, Shape::Rows, body).unwrap_or_else(|r| panic!("{r:?}"));
        store.apply(orders.id, Op::Write, &decoded);
    }
    let _ = store.publish();
    rate(bodies.len() * batch, started.elapsed())
}

async fn over_http(
    state: &AppState,
    base: &str,
    bodies: Vec<Vec<u8>>,
    content_type: &'static str,
    clients: usize,
) -> (usize, Duration) {
    let client = reqwest::Client::new();
    let rows_per_body = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let queue = Arc::new(std::sync::Mutex::new(bodies));
    let url = format!("{base}/v1/tables/orders/rows");
    let last_epoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let started = Instant::now();
    let mut tasks = Vec::new();
    for _ in 0..clients {
        let (client, queue, url, rows) = (
            client.clone(),
            Arc::clone(&queue),
            url.clone(),
            Arc::clone(&rows_per_body),
        );
        let last_epoch = Arc::clone(&last_epoch);
        tasks.push(tokio::spawn(async move {
            loop {
                let body = queue
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .pop();
                let Some(body) = body else { break };
                loop {
                    let response = client
                        .post(&url)
                        .header("content-type", content_type)
                        .body(body.clone())
                        .send()
                        .await
                        .unwrap_or_else(|e| panic!("request failed: {e}"));
                    let status = response.status();
                    let text = response.text().await.unwrap_or_default();
                    if status.as_u16() == 503 {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        continue;
                    }
                    assert!(status.is_success(), "{status}: {text}");
                    let value: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
                    let written = value["written"].as_u64().unwrap_or(0);
                    rows.fetch_add(
                        usize::try_from(written).unwrap_or(0),
                        std::sync::atomic::Ordering::Relaxed,
                    );
                    last_epoch.fetch_max(
                        value["epoch"].as_u64().unwrap_or(0),
                        std::sync::atomic::Ordering::Relaxed,
                    );
                    break;
                }
            }
        }));
    }
    for task in tasks {
        task.await.unwrap_or_else(|e| panic!("{e}"));
    }
    // done when the last write is visible
    let _ = state
        .writer()
        .visible(last_epoch.load(std::sync::atomic::Ordering::Relaxed))
        .await;
    (
        rows_per_body.load(std::sync::atomic::Ordering::Relaxed),
        started.elapsed(),
    )
}

#[tokio::main]
async fn main() {
    match Cli::parse().command {
        Command::Query {
            orders,
            runs,
            metrics,
        } => {
            let model = model();
            println!("query: reference dataset, {orders} orders\n");
            let snapshot = query::build(&model, orders);
            query::measure(&snapshot, runs, metrics.as_deref());
        }
        Command::Ingest {
            rows,
            batch,
            clients,
        } => {
            let model = model();
            println!("ingest: {rows} rows per format, {batch} per request, {clients} clients\n");
            println!(
                "  store only (NDJSON decode + apply) {}",
                store_only(&model, rows, batch)
            );

            for (name, content_type) in [
                ("ndjson", "application/x-ndjson"),
                ("arrow ", "application/vnd.apache.arrow.stream"),
            ] {
                // a fresh server per format, so each starts empty
                let state = Arc::new(
                    AppState::new(
                        Arc::clone(&model),
                        WriterConfig::default(),
                        None,
                        Tokens::default(),
                    )
                    .unwrap_or_else(|e| panic!("{e}")),
                );
                state.set_ready();
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .unwrap_or_else(|e| panic!("{e}"));
                let address = listener.local_addr().unwrap_or_else(|e| panic!("{e}"));
                let app = router(Arc::clone(&state));
                let server = tokio::spawn(async move { axum::serve(listener, app).await });

                let bodies: Vec<Vec<u8>> = (0..rows / batch)
                    .map(|b| {
                        if content_type.contains("arrow") {
                            arrow_body(b * batch, batch)
                        } else {
                            ndjson_body(b * batch, batch)
                        }
                    })
                    .collect();
                let bytes: usize = bodies.iter().map(Vec::len).sum();
                let (written, elapsed) = over_http(
                    &state,
                    &format!("http://{address}"),
                    bodies,
                    content_type,
                    clients,
                )
                .await;
                let snapshot = state.writer().snapshot();
                let table = model
                    .table_named("orders")
                    .and_then(|t| snapshot.table(t.id));
                let stored = table.map_or(0, tidepool_store::TableSnapshot::bytes);
                // the numbers only count if every row is really there
                let live = table.map_or(0, |t| t.live_rows);
                assert_eq!(live, written as u64, "rows visible at the end");
                assert_eq!(written, rows / batch * batch, "rows acknowledged");
                #[allow(clippy::cast_precision_loss)]
                let (per_row, megabytes) =
                    (stored as f64 / written.max(1) as f64, bytes as f64 / 1e6);
                println!(
                    "  http {name}                         {}  [{:.0} MB sent, {:.0} bytes/row stored]",
                    rate(written, elapsed),
                    megabytes,
                    per_row
                );
                server.abort();
            }
        }
    }
}
