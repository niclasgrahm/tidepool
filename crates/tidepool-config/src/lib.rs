//! Serde types for every Tidepool config file (`tidepool.yaml`,
//! `tables/*.yaml`, `model.ossie.yaml`, `dashboards/*.yaml`), their JSON
//! Schemas, and diagnostics that point at `file:line:col`.
//!
//! Parsing and per-file shape only. Whether the files make sense *together*
//! (a relationship's dataset exists, a metric's columns are reachable) is
//! `tidepool-model`'s job; it reports through the same [`diag`] types.
//!
//! Apache-2.0 so that anything generating or checking Tidepool config (Kayak,
//! editors, agents) can depend on it freely; see licensing.md.

pub mod dashboard;
pub mod diag;
pub mod ossie;
pub mod project;
pub mod schema;
pub mod server;
pub mod source;
pub mod table;
pub mod types;

pub use diag::{Diagnostic, Diagnostics, Severity};
pub use project::{ConfigSet, Parsed};
pub use source::{Location, SourceFile, YamlPath};
