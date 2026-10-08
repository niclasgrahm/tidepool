//! The `kill -9` loop (plan §11.4): the real binary with a data directory,
//! orders posted to it, killed without warning at a random moment, started
//! again, over and over. After every restart the table must hold every
//! order that was acknowledged, and none that wasn't sent.
//!
//! `TIDEPOOL_KILL_LOOPS` sets the number of kills (default 4).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

fn free_port() -> SocketAddr {
    match TcpListener::bind("127.0.0.1:0").and_then(|l| l.local_addr()) {
        Ok(address) => address,
        Err(err) => panic!("finding a free port: {err}"),
    }
}

/// One HTTP request: the status and the body.
fn http(address: SocketAddr, method: &str, path: &str, body: &str) -> Option<(u16, String)> {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    let status = response.split(' ').nth(1)?.parse().ok()?;
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())?;
    Some((status, body))
}

fn start(data: &Path, address: SocketAddr) -> Child {
    let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/retail");
    let child = Command::new(env!("CARGO_BIN_EXE_tidepool"))
        .args(["serve", "--config"])
        .arg(&config)
        .arg("--data")
        .arg(data)
        .args(["--listen", &address.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let child = match child {
        Ok(child) => child,
        Err(err) => panic!("starting tidepool: {err}"),
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if http(address, "GET", "/readyz", "").is_some_and(|(status, _)| status == 200) {
            return child;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("tidepool didn't become ready on {address}");
}

/// Orders in the table, as the server counts them.
fn orders(address: SocketAddr) -> u64 {
    let Some((200, body)) = http(address, "GET", "/v1/tables/orders", "") else {
        panic!("reading the orders table");
    };
    let value: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
    value["rows"].as_u64().unwrap_or(0)
}

#[test]
fn acknowledged_orders_survive_kill_9() {
    let loops: u64 = std::env::var("TIDEPOOL_KILL_LOOPS")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(4);
    let data = match tempfile::tempdir() {
        Ok(dir) => dir,
        Err(err) => panic!("{err}"),
    };
    let (mut acked, mut sent) = (0u64, 0u64);
    for round in 0..loops {
        let address = free_port();
        let mut server = start(data.path(), address);
        let stored = orders(address);
        assert!(
            acked <= stored && stored <= sent,
            "round {round}: {stored} orders after restart; {acked} were acknowledged, {sent} sent"
        );
        // writes from here may or may not land: only acks count
        acked = stored;
        sent = stored;

        let stop = Arc::new(AtomicBool::new(false));
        let (confirmed, posted) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
        let writer = {
            let (stop, confirmed, posted) = (
                Arc::clone(&stop),
                Arc::clone(&confirmed),
                Arc::clone(&posted),
            );
            std::thread::spawn(move || {
                let mut n = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    let rows: Vec<String> = (0..50)
                        .map(|i| {
                            format!(
                                r#"{{"order_id":"r{round}-{n}-{i}","customer_id":"c1","product_id":"p1","status":"paid","ordered_at":"2026-10-01T00:00:00Z","quantity":1,"amount":"1.00"}}"#
                            )
                        })
                        .collect();
                    n += 1;
                    posted.fetch_add(50, Ordering::Relaxed);
                    let body = format!("[{}]", rows.join(","));
                    match http(address, "POST", "/v1/tables/orders/rows", &body) {
                        Some((200, _)) => {
                            confirmed.fetch_add(50, Ordering::Relaxed);
                        }
                        Some((503, _)) => {
                            posted.fetch_sub(50, Ordering::Relaxed);
                        }
                        // killed mid-request: it may or may not have landed
                        _ => {}
                    }
                }
            })
        };
        std::thread::sleep(Duration::from_millis(150 + 97 * (round % 7)));
        let _ = server.kill(); // SIGKILL: no shutdown, no flushing
        let _ = server.wait();
        stop.store(true, Ordering::Relaxed);
        let _ = writer.join();
        acked += confirmed.load(Ordering::Relaxed);
        sent += posted.load(Ordering::Relaxed);
    }
    let address = free_port();
    let mut server = start(data.path(), address);
    let stored = orders(address);
    let _ = server.kill();
    let _ = server.wait();
    assert!(
        acked <= stored && stored <= sent,
        "after {loops} kills: {stored} orders; {acked} were acknowledged, {sent} sent"
    );
    assert!(
        acked > 0,
        "nothing was acknowledged: the test measured nothing"
    );
}
