//! The eval set (`evals/questions.yaml`): questions about the retail
//! example with the tool call that answers each one. This writes the set's
//! data and makes every call over MCP, so the answers it gives agents to be
//! scored against are true.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::http::{Method, StatusCode};
use common::{request, send};
use serde_json::{Value, json};
use tidepool_server::config::read_dir;
use tidepool_server::{AppState, Project, Tokens};
use tidepool_store::WriterConfig;

fn repository() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
}

async fn server() -> Arc<AppState> {
    let files = match read_dir(&repository().join("examples/retail")) {
        Ok(files) => files,
        Err(err) => panic!("{err}"),
    };
    let writer = WriterConfig {
        epoch_interval: Duration::from_millis(10),
        ..WriterConfig::default()
    };
    match AppState::open(
        Project { dir: None, files },
        writer,
        None,
        Tokens::default(),
        false,
    )
    .await
    {
        Ok(state) => {
            state.set_ready();
            Arc::new(state)
        }
        Err(err) => panic!("{err:#}"),
    }
}

async fn tool(state: &Arc<AppState>, name: &str, arguments: &Value) -> Value {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                      "params": {"name": name, "arguments": arguments}});
    let reply = send(
        Arc::clone(state),
        request(
            Method::POST,
            "/mcp",
            &[("content-type", "application/json")],
            body.to_string(),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let reply = reply.json();
    assert_ne!(reply["result"]["isError"], true, "{name} failed: {reply}");
    reply["result"]["structuredContent"].clone()
}

#[tokio::test]
async fn every_question_in_the_eval_set_gets_its_answer() {
    let path = repository().join("evals/questions.yaml");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) => panic!("{}: {err}", path.display()),
    };
    let set: Value = match serde_norway::from_str(&text) {
        Ok(set) => set,
        Err(err) => panic!("{}: {err}", path.display()),
    };
    let state = server().await;
    // dimensions before facts, so every order finds its customer
    for table in ["categories", "products", "customers", "orders"] {
        let rows = &set["data"][table];
        assert!(rows.is_array(), "the set has no {table}");
        let _ = tool(
            &state,
            "ingest_rows",
            &json!({"table": table, "rows": rows}),
        )
        .await;
    }
    let Some(questions) = set["questions"].as_array() else {
        panic!("the set has no questions")
    };
    assert!(questions.len() >= 10, "a set this small scores nothing");
    for question in questions {
        let ask = question["ask"].as_str().unwrap_or("?");
        let Some(name) = question["tool"].as_str() else {
            panic!("`{ask}` names no tool")
        };
        let answer = tool(&state, name, &question["arguments"]).await;
        assert_eq!(answer["rows"], question["answer"], "`{ask}`: {answer}");
    }
}
