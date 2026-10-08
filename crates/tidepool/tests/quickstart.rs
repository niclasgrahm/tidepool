//! The quick-start (M8 exit criterion: zero to a live dashboard in under
//! five minutes), as the real binary runs it: `init`, `validate`, `serve`,
//! `feed`, and the dashboard's numbers coming back.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn tidepool(args: &[&str]) -> std::process::Output {
    match Command::new(env!("CARGO_BIN_EXE_tidepool"))
        .args(args)
        .output()
    {
        Ok(output) => output,
        Err(err) => panic!("running tidepool {args:?}: {err}"),
    }
}

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
    Some((status, response.split_once("\r\n\r\n")?.1.to_string()))
}

struct Running(std::process::Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn init_serve_feed_and_the_dashboard_has_numbers() {
    let started = Instant::now();
    let dir = match tempfile::tempdir() {
        Ok(dir) => dir,
        Err(err) => panic!("{err}"),
    };
    let project = dir.path().join("shop");
    let project_arg = project.to_string_lossy().to_string();
    let output = tidepool(&["init", &project_arg]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("tidepool feed"));
    let output = tidepool(&["validate", "--config", &project_arg]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // init doesn't write over a project
    assert!(!tidepool(&["init", &project_arg]).status.success());

    let address = match TcpListener::bind("127.0.0.1:0").and_then(|l| l.local_addr()) {
        Ok(address) => address,
        Err(err) => panic!("{err}"),
    };
    let server = match Command::new(env!("CARGO_BIN_EXE_tidepool"))
        .args([
            "serve",
            "--config",
            &project_arg,
            "--listen",
            &address.to_string(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => Running(child),
        Err(err) => panic!("{err}"),
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    while http(address, "GET", "/readyz", "").is_none_or(|(s, _)| s != 200) {
        assert!(Instant::now() < deadline, "the server didn't become ready");
        std::thread::sleep(Duration::from_millis(50));
    }

    let output = tidepool(&[
        "feed",
        "--server",
        &format!("http://{address}"),
        "--seconds",
        "1",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    std::thread::sleep(Duration::from_millis(500));
    let Some((200, body)) = http(
        address,
        "POST",
        "/v1/query",
        r#"{"metrics": ["revenue", "order_count"], "dimensions": [{"field": "customers.region"}]}"#,
    ) else {
        panic!("querying the fed data");
    };
    let answer: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
    let rows = answer["rows"].as_array().map_or(0, Vec::len);
    assert_eq!(rows, 6, "orders in every region: {answer}");
    // the data sits next to the config
    assert!(Path::new(&project).join("data").is_dir());
    drop(server);
    assert!(
        started.elapsed() < Duration::from_mins(2),
        "the quick-start took {:?}",
        started.elapsed()
    );
}
