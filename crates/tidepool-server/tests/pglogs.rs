//! The Postgres logs example (`examples/pglogs`) with the lines of a real
//! run (`sample.ndjson`, written by its shipper): searched the way its
//! dashboard searches them, and every answer worked out here from the file.

mod common;

use std::path::Path;
use std::sync::Arc;

use axum::http::{Method, StatusCode};
use common::{request, send, state};
use serde_json::{Value, json};
use tidepool_config::ConfigSet;
use tidepool_model::{Catalog, compile};
use tidepool_server::{AppState, Tokens};

fn example() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/pglogs")
}

async fn post(state: &Arc<AppState>, path: &str, kind: &str, body: String) -> Value {
    let reply = send(
        Arc::clone(state),
        request(Method::POST, path, &[("content-type", kind)], body),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{path}: {}", reply.text());
    reply.json()
}

/// The example's server with the sample loaded, and the sample's lines.
async fn loaded() -> (Arc<AppState>, Vec<Value>) {
    let (set, diagnostics) = match ConfigSet::load_dir(&example().join("tidepool")) {
        Ok(loaded) => loaded,
        Err(err) => panic!("reading the example: {err}"),
    };
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    let model = match compile(&set, &Catalog::default()) {
        (Some(compiled), _) => Arc::new(compiled.model),
        (None, d) => panic!("the example should compile: {d:?}"),
    };
    let state = state(model, Tokens::default(), None);
    // as the shipper writes them when it starts
    let severities = [
        ("DEBUG", "debug"),
        ("LOG", "info"),
        ("INFO", "info"),
        ("NOTICE", "info"),
        ("WARNING", "warning"),
        ("ERROR", "error"),
        ("FATAL", "fatal"),
        ("PANIC", "fatal"),
    ];
    let rows: Vec<Value> = severities
        .iter()
        .enumerate()
        .map(
            |(rank, (severity, level))| json!({"severity": severity, "rank": rank, "level": level}),
        )
        .collect();
    post(
        &state,
        "/v1/tables/severities/rows?wait=visible",
        "application/json",
        Value::Array(rows).to_string(),
    )
    .await;
    let sample = match std::fs::read_to_string(example().join("sample.ndjson")) {
        Ok(text) => text,
        Err(err) => panic!("reading the sample: {err}"),
    };
    let lines: Vec<Value> = sample
        .lines()
        .map(|line| match serde_json::from_str(line) {
            Ok(value) => value,
            Err(err) => panic!("a sample line: {err}"),
        })
        .collect();
    post(
        &state,
        "/v1/tables/logs/rows?wait=visible",
        "application/x-ndjson",
        sample,
    )
    .await;
    (state, lines)
}

/// The lines whose message contains `text`, ignoring the case of ASCII
/// letters, as the server must find them.
fn containing<'a>(lines: &'a [Value], text: &str) -> Vec<&'a Value> {
    let text = text.to_ascii_lowercase();
    lines
        .iter()
        .filter(|l| {
            l["message"]
                .as_str()
                .is_some_and(|m| m.to_ascii_lowercase().contains(&text))
        })
        .collect()
}

#[tokio::test]
async fn the_example_finds_what_its_lines_say() {
    let (state, lines) = loaded().await;
    assert!(lines.len() > 200, "the sample has {} lines", lines.len());
    for text in [
        "deadlock",
        "LOCK TIMEOUT",
        "password authentication",
        "does not exist",
    ] {
        let found = containing(&lines, text);
        assert!(!found.is_empty(), "the sample has no `{text}`");
        let problems = found
            .iter()
            .filter(|l| ["ERROR", "FATAL", "PANIC"].contains(&l["severity"].as_str().unwrap_or("")))
            .count();
        let answer = post(
            &state,
            "/v1/query",
            "application/json",
            json!({
                "metrics": ["lines", "problems"],
                "filters": [{"field": "logs.message", "contains": text}],
            })
            .to_string(),
        )
        .await;
        assert_eq!(
            answer["rows"],
            json!([[found.len(), problems]]),
            "lines and problems containing `{text}`"
        );

        // the tail: the newest of them first
        let tail = post(
            &state,
            "/v1/rows",
            "application/json",
            json!({
                "fields": ["logs.logged_at", "logs.severity", "logs.message"],
                "filters": [{"field": "logs.message", "contains": text}],
                "limit": 3,
            })
            .to_string(),
        )
        .await;
        let newest: Vec<Value> = found
            .iter()
            .rev()
            .take(3)
            .map(|l| json!([l["logged_at"], l["severity"], l["message"]]))
            .collect();
        assert_eq!(
            tail["rows"],
            Value::Array(newest),
            "the newest `{text}` lines"
        );
    }
}
