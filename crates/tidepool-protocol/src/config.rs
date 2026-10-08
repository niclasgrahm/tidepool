//! The config API (plan §8): reading the config, planning and applying
//! changes, version history, and the approval queue.
//!
//! A change names the version it was made against (`base_version`): a
//! change against an old version is refused rather than merged, and the
//! client plans again on the current one.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One config file, by its path in the config directory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ConfigFile {
    pub path: String,
    pub text: String,
}

/// `GET /v1/config`: the files of the current version.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ConfigState {
    pub version: u64,
    pub files: Vec<ConfigFile>,
    /// The version the server runs: behind `version` while a change is
    /// being rebuilt.
    pub live_version: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rebuild: Option<RebuildInfo>,
    /// Edits in the config directory that aren't applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external: Option<ExternalEdits>,
    /// Whether applied changes are written back to the config directory.
    pub writeback: bool,
}

/// A change being built in the background (plan §8.2, rebuild).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RebuildInfo {
    pub version: u64,
    pub rows_done: u64,
    pub rows: u64,
}

/// Edits found in the config directory that the server hasn't applied.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ExternalEdits {
    /// Waiting for approval as this pending change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<u64>,
    /// They don't compile: what's wrong, rendered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problems: Option<String>,
}

/// A proposed change: files to set or remove (`null`), against
/// `base_version`. Files not named stay as they are, unless `replace` is set:
/// then the named files are the whole config (a revert).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ConfigChange {
    pub base_version: u64,
    #[serde(default)]
    pub files: BTreeMap<String, Option<String>>,
    #[serde(default)]
    pub replace: bool,
    /// Apply even if it destroys data (plan §8.2, destructive).
    #[serde(default)]
    pub approve: bool,
    /// Put it in the approval queue instead of applying it.
    #[serde(default)]
    pub propose: bool,
    /// Who makes it, for the history and the commit message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
}

/// One problem with a config, located.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ConfigProblem {
    /// `error` or `warning`.
    pub severity: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// 1-based, when the problem points at a token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
}

/// One change a plan makes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PlannedChange {
    /// `add`, `remove`, `change` or `rename`.
    pub op: String,
    /// `table`, `column`, `relationship`, `dataset`, `field`, `metric`,
    /// `dashboard` or `setting`.
    pub kind: String,
    pub subject: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// `safe`, `rebuild` or `destructive`.
    pub class: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// What a change would do: `POST /v1/config/plan`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PlanInfo {
    pub base_version: u64,
    /// Whether the config compiles.
    pub valid: bool,
    pub problems: Vec<ConfigProblem>,
    /// The problems as `tidepool validate` prints them.
    pub problems_text: String,
    /// The worst class among the changes; `None` without changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<String>,
    pub changes: Vec<PlannedChange>,
    pub warnings: Vec<String>,
    /// The plan as `tidepool plan` prints it.
    pub text: String,
}

/// What became of an applied change: `POST /v1/config/apply`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ApplyResult {
    /// `applied` (live now), `rebuilding` (live when the rebuild is done),
    /// `queued` (waiting for approval) or `unchanged`.
    pub status: String,
    /// The config version: the new one when applied.
    pub version: u64,
    /// The pending change, when queued.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<u64>,
    pub plan: PlanInfo,
}

/// Why a change wasn't made (409 or 422).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ConfigError {
    pub error: String,
    /// The version changes are made against now.
    pub current_version: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<PlanInfo>,
}

/// One applied version: `GET /v1/config/versions`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct VersionInfo {
    pub version: u64,
    /// RFC 3339.
    pub at: String,
    pub author: String,
    /// `startup`, `api`, `files` or `revert`.
    pub source: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<String>,
}

/// One version's files: `GET /v1/config/versions/{version}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ConfigVersion {
    pub info: VersionInfo,
    pub files: Vec<ConfigFile>,
}

/// A change waiting for approval: `GET /v1/changes`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PendingChange {
    pub id: u64,
    pub source: String,
    pub author: String,
    /// RFC 3339.
    pub at: String,
    pub base_version: u64,
    /// The files it sets or removes.
    pub files: BTreeMap<String, Option<String>>,
    #[serde(default)]
    pub replace: bool,
    pub plan: PlanInfo,
}
