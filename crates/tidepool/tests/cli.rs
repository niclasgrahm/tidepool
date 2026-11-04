//! The config subcommands, run as the real binary.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn tidepool(args: &[&str]) -> Output {
    match Command::new(env!("CARGO_BIN_EXE_tidepool"))
        .args(args)
        .output()
    {
        Ok(output) => output,
        Err(err) => panic!("running tidepool {args:?}: {err}"),
    }
}

fn retail() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/retail")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn validate_accepts_the_example_and_summarises_it() {
    let dir = retail();
    let output = tidepool(&["validate", "--config", &dir.to_string_lossy()]);
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert_eq!(
        text(&output.stdout),
        "ok: 4 tables, 3 relationships, 7 metrics, 3 dashboards\n"
    );
}

#[test]
fn the_oee_example_over_indus_namespace_validates() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/indud-oee");
    let output = tidepool(&["validate", "--config", &dir.to_string_lossy()]);
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert_eq!(
        text(&output.stdout),
        "ok: 3 tables, 2 relationships, 10 metrics, 1 dashboard\n"
    );
}

#[test]
fn the_postgres_logs_example_validates() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/pglogs/tidepool");
    let output = tidepool(&["validate", "--config", &dir.to_string_lossy()]);
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert_eq!(
        text(&output.stdout),
        "ok: 3 tables, 2 relationships, 4 metrics, 1 dashboard\n"
    );
}

#[test]
fn validate_fails_with_rendered_errors() {
    let dir = match tempfile::tempdir() {
        Ok(dir) => dir,
        Err(err) => panic!("temp dir: {err}"),
    };
    if let Err(err) = std::fs::create_dir(dir.path().join("tables")).and_then(|()| {
        std::fs::write(
            dir.path().join("tables/orders.yaml"),
            "name: order\ncolumns:\n  id: { type: string }\n",
        )
    }) {
        panic!("writing config: {err}");
    }
    let output = tidepool(&["validate", "--config", &dir.path().to_string_lossy()]);
    assert!(!output.status.success());
    let stderr = text(&output.stderr);
    assert!(stderr.contains("--> tables/orders.yaml:1:7"), "{stderr}");
    assert!(stderr.ends_with("1 error, 0 warnings\n"), "{stderr}");
}

#[test]
fn validate_fails_when_a_file_does_not_parse() {
    // the table is left out, and the rest compiles without it: still an error
    let dir = match tempfile::tempdir() {
        Ok(dir) => dir,
        Err(err) => panic!("temp dir: {err}"),
    };
    if let Err(err) = std::fs::create_dir(dir.path().join("tables")).and_then(|()| {
        std::fs::write(
            dir.path().join("tables/returns.yaml"),
            "name: returns\nmode: append\ncolumns:\n  id: { type: strng }\n",
        )
    }) {
        panic!("writing config: {err}");
    }
    let output = tidepool(&["validate", "--config", &dir.path().to_string_lossy()]);
    assert!(!output.status.success(), "{}", text(&output.stdout));
    assert_eq!(text(&output.stdout), "");
}

#[test]
fn validate_reports_a_missing_directory() {
    let output = tidepool(&["validate", "--config", "/nonexistent/tidepool-config"]);
    assert!(!output.status.success());
    assert!(text(&output.stderr).contains("can't read config directory"));
}

#[test]
fn plan_against_itself_is_empty_and_against_nothing_adds_everything() {
    let dir = retail();
    let dir = dir.to_string_lossy();
    let same = tidepool(&["plan", "--config", &dir, "--base", &dir]);
    assert!(same.status.success(), "{}", text(&same.stderr));
    assert_eq!(text(&same.stdout), "no changes\n");

    let fresh = tidepool(&["plan", "--config", &dir]);
    assert!(fresh.status.success(), "{}", text(&fresh.stderr));
    let stdout = text(&fresh.stdout);
    assert!(stdout.contains("+ table        orders"), "{stdout}");
    assert!(!stdout.contains("destructive  ("), "{stdout}");
}
