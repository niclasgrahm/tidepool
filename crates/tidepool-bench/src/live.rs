//! Freshness with live sessions (plan §2.3): `sessions` WebSocket clients
//! on the retail dashboard, clicking a region now and then, while orders
//! arrive at `rate` rows a second. For every acknowledged batch and every
//! session, freshness is the time from the acknowledgement to the first
//! frame at or after the epoch the batch became visible in.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tidepool_model::Model;
use tidepool_server::{AppState, Tokens, router};
use tidepool_store::WriterConfig;
use tokio_tungstenite::tungstenite::Message;

use crate::arrow_body;

const REGIONS: [&str; 6] = ["north", "south", "east", "west", "central", "islands"];

/// When each epoch's first frame reached a session.
type Arrivals = Arc<Mutex<BTreeMap<u64, Instant>>>;

async fn post(client: &reqwest::Client, url: &str, content_type: &str, body: Vec<u8>) -> Value {
    loop {
        let response = client
            .post(url)
            .header("content-type", content_type)
            .body(body.clone())
            .send()
            .await
            .unwrap_or_else(|e| panic!("request failed: {e}"));
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if status.as_u16() == 503 {
            tokio::time::sleep(Duration::from_millis(5)).await;
            continue;
        }
        assert!(status.is_success(), "{status}: {text}");
        return serde_json::from_str(&text).unwrap_or_default();
    }
}

fn ndjson(rows: impl Iterator<Item = Value>) -> Vec<u8> {
    rows.map(|r| r.to_string() + "\n")
        .collect::<String>()
        .into_bytes()
}

/// One session: open the dashboard, record when frames arrive, and click
/// a region every few seconds.
async fn session(url: String, number: usize, arrivals: Arrivals, until: Instant) {
    let (mut socket, _) = tokio_tungstenite::connect_async(&url)
        .await
        .unwrap_or_else(|e| panic!("connecting: {e}"));
    let say = |v: Value| Message::text(v.to_string());
    socket
        .send(say(json!({"type": "open_dashboard", "id": "sales"})))
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let mut click = tokio::time::interval(Duration::from_millis(2000 + 37 * number as u64));
    let mut clicks = number;
    loop {
        if Instant::now() > until {
            break;
        }
        tokio::select! {
            message = socket.next() => {
                let Some(Ok(Message::Text(text))) = message else { break };
                let value: Value = serde_json::from_str(text.as_str()).unwrap_or_default();
                if value["type"] == "frame" {
                    let epoch = value["epoch"].as_u64().unwrap_or(0);
                    arrivals
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .entry(epoch)
                        .or_insert_with(Instant::now);
                }
            }
            _ = click.tick() => {
                clicks += 1;
                let selection = if clicks.is_multiple_of(3) {
                    json!({"type": "clear"})
                } else {
                    json!({"type": "select", "field": "customers.region", "values": [REGIONS[clicks % REGIONS.len()]]})
                };
                if socket.send(say(selection)).await.is_err() {
                    break;
                }
            }
        }
    }
}

/// The freshness run.
#[allow(clippy::too_many_lines)]
pub async fn measure(model: Arc<Model>, base: usize, sessions: usize, rate: usize, seconds: u64) {
    let config = WriterConfig {
        epoch_interval: Duration::from_millis(200),
        ..WriterConfig::default()
    };
    let state = Arc::new(
        AppState::new(model, config, None, Tokens::default()).unwrap_or_else(|e| panic!("{e}")),
    );
    state.set_ready();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let address = listener.local_addr().unwrap_or_else(|e| panic!("{e}"));
    let app = router(Arc::clone(&state));
    tokio::spawn(async move { axum::serve(listener, app).await });
    let client = reqwest::Client::new();
    let http = format!("http://{address}");

    // dimensions, then a base of orders
    let customers = ndjson((0..50_000).map(|c| {
        json!({"customer_id": format!("c{c}"), "name": format!("Customer {c}"), "region": REGIONS[c % 6]})
    }));
    post(
        &client,
        &format!("{http}/v1/tables/customers/rows"),
        "application/x-ndjson",
        customers,
    )
    .await;
    let products = ndjson((0..2_000).map(|p| {
        json!({"product_id": format!("p{p}"), "name": format!("Product {p}"), "category": format!("category {}", p % 20), "list_price": "10.00"})
    }));
    post(
        &client,
        &format!("{http}/v1/tables/products/rows"),
        "application/x-ndjson",
        products,
    )
    .await;
    let arrow = "application/vnd.apache.arrow.stream";
    let orders_url = format!("{http}/v1/tables/orders/rows");
    let batch = 10_000;
    let started = Instant::now();
    let mut last = 0;
    for b in 0..base / batch {
        let reply = post(&client, &orders_url, arrow, arrow_body(b * batch, batch)).await;
        last = reply["epoch"].as_u64().unwrap_or(0);
    }
    let _ = state.writer().visible(last).await;
    println!(
        "  loaded {base} orders in {:.1}s",
        started.elapsed().as_secs_f64()
    );

    // sessions
    let until = Instant::now() + Duration::from_secs(seconds + 3);
    let ws = format!("ws://{address}/v1/ws");
    let arrivals: Vec<Arrivals> = (0..sessions).map(|_| Arrivals::default()).collect();
    let tasks: Vec<_> = arrivals
        .iter()
        .enumerate()
        .map(|(n, a)| tokio::spawn(session(ws.clone(), n, Arc::clone(a), until)))
        .collect();
    tokio::time::sleep(Duration::from_secs(1)).await;

    // ingest at `rate`, in batches of a tenth of a second's rows
    let per_batch = (rate / 10).max(1);
    let bodies: Vec<Vec<u8>> = (0..usize::try_from(seconds).unwrap_or(0) * 10)
        .map(|b| arrow_body(base + b * per_batch, per_batch))
        .collect();
    let mut acks: Vec<(Instant, u64)> = Vec::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let ingest_started = Instant::now();
    for body in bodies {
        tick.tick().await;
        let reply = post(&client, &orders_url, arrow, body).await;
        acks.push((Instant::now(), reply["epoch"].as_u64().unwrap_or(0)));
    }
    let ingest_time = ingest_started.elapsed();
    for task in tasks {
        let _ = task.await;
    }

    let mut freshness: Vec<Duration> = Vec::new();
    let mut missed = 0;
    for arrivals in &arrivals {
        let arrivals = arrivals.lock().unwrap_or_else(PoisonError::into_inner);
        for (acked, epoch) in &acks {
            match arrivals.range(epoch..).next() {
                Some((_, arrived)) => freshness.push(arrived.saturating_duration_since(*acked)),
                None => missed += 1,
            }
        }
    }
    freshness.sort();
    let at = |p: f64| {
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let i = ((freshness.len().max(1) - 1) as f64 * p).round() as usize;
        freshness
            .get(i)
            .map_or(f64::NAN, |d| d.as_secs_f64() * 1000.0)
    };
    #[allow(clippy::cast_precision_loss)]
    let achieved = (acks.len() * per_batch) as f64 / ingest_time.as_secs_f64();
    println!(
        "  {sessions} sessions, {achieved:.0} rows/s ingested for {seconds}s: freshness p50 {:.0} ms, p95 {:.0} ms, max {:.0} ms ({} measured, {missed} never seen)",
        at(0.5),
        at(0.95),
        at(1.0),
        freshness.len()
    );
}

/// Keep a running server busy: the retail dimensions once, then `rate`
/// orders a second until stopped. For looking at a live dashboard.
pub async fn feed(server: &str, rate: usize) {
    let client = reqwest::Client::new();
    let customers = ndjson((0..50_000).map(|c| {
        json!({"customer_id": format!("c{c}"), "name": format!("Customer {c}"), "region": REGIONS[c % 6]})
    }));
    post(
        &client,
        &format!("{server}/v1/tables/customers/rows"),
        "application/x-ndjson",
        customers,
    )
    .await;
    let products = ndjson((0..2_000).map(|p| {
        json!({"product_id": format!("p{p}"), "name": format!("Product {p}"), "category": format!("category {}", p % 20), "list_price": "10.00"})
    }));
    post(
        &client,
        &format!("{server}/v1/tables/products/rows"),
        "application/x-ndjson",
        products,
    )
    .await;
    println!("  feeding {rate} orders/s to {server}; stop with ctrl-c");
    let per_tick = (rate / 10).max(1);
    let url = format!("{server}/v1/tables/orders/rows");
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let mut next = 0;
    loop {
        tick.tick().await;
        post(
            &client,
            &url,
            "application/vnd.apache.arrow.stream",
            arrow_body(next, per_tick),
        )
        .await;
        next += per_tick;
    }
}
