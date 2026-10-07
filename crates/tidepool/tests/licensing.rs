//! The licence split (licensing.md), checked rather than remembered: every
//! workspace member declares a licence, the permissive ones are exactly the
//! ones meant to be, and each carries the matching licence text.

use std::path::{Path, PathBuf};

const PERMISSIVE: &str = "Apache-2.0";
const COPYLEFT: &str = "AGPL-3.0-or-later";

/// The crates any client of Tidepool needs. Everything else is the server.
const PERMISSIVE_CRATES: [&str; 2] = ["crates/tidepool-config", "crates/tidepool-protocol"];

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(path: &str) -> String {
    let full = root().join(path);
    match std::fs::read_to_string(&full) {
        Ok(text) => text,
        Err(err) => panic!("{} should be readable: {err}", full.display()),
    }
}

fn quoted(line: &str) -> Vec<&str> {
    line.split('"').skip(1).step_by(2).collect()
}

fn declared_license(manifest: &str) -> Option<&str> {
    let mut in_package = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
        } else if in_package
            && line.starts_with("license")
            && let [value] = quoted(line).as_slice()
        {
            return Some(value);
        }
    }
    None
}

/// The members list of the root manifest: every quoted string between
/// `members = [` and the closing `]`.
fn members() -> Vec<String> {
    let manifest = read("Cargo.toml");
    let Some((_, after)) = manifest.split_once("members = [") else {
        panic!("the root manifest should declare workspace members");
    };
    let Some((list, _)) = after.split_once(']') else {
        panic!("the members list should be closed");
    };
    quoted(list).into_iter().map(String::from).collect()
}

#[test]
fn every_member_is_on_the_right_side_of_the_split() {
    let members = members();
    assert!(
        members.len() > 2,
        "the members list should not have come back nearly empty"
    );
    for member in members {
        let manifest = read(&format!("{member}/Cargo.toml"));
        let expected = if PERMISSIVE_CRATES.contains(&member.as_str()) {
            PERMISSIVE
        } else {
            COPYLEFT
        };
        assert_eq!(
            declared_license(&manifest),
            Some(expected),
            "{member} is on the wrong side of the licence split (or declares none); see licensing.md"
        );
    }
}

#[test]
fn the_licence_texts_are_beside_the_crates_that_claim_them() {
    assert!(
        read("LICENSE").contains("GNU AFFERO GENERAL PUBLIC LICENSE"),
        "the root LICENSE should be the AGPL text"
    );
    for member in PERMISSIVE_CRATES {
        assert!(
            read(&format!("{member}/LICENSE")).contains("Apache License"),
            "{member} should carry its own Apache text rather than inheriting the root's"
        );
    }
}
