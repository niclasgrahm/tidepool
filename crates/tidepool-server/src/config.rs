//! The config pipeline (plan §8): every change to what the server runs,
//! from the API, the UI, the config directory or startup, goes the same way.
//!
//! ```text
//! change (files to set or remove, against a version)
//!   → compile against the catalog → plan (diff with the current model)
//!   → policy: destructive needs approval; proposals and unsafe edits in
//!     the directory wait in the queue
//!   → the writer swaps the model in (now, or after a background rebuild)
//!   → a new version in the history, written back to the config directory
//!     (and committed, in a git repository)
//!   → sessions are told by the epoch loop when the model they see changes
//! ```
//!
//! One lock serialises changes, so a change is always planned against the
//! version it is applied to. The watcher reads the directory under the same
//! lock, which is how it tells the server's own write-backs from edits.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::Duration;

use axum::Json;
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use tidepool_config::ConfigSet;
use tidepool_config::diag::{Diagnostics, Severity, render, render_all, resolve};
use tidepool_model::ir::Model;
use tidepool_model::plan::{ChangeClass, Plan, diff};
use tidepool_model::{Catalog, Compiled, compile};
use tidepool_protocol::config::{
    ApplyResult, ConfigChange, ConfigError, ConfigFile, ConfigProblem, ConfigState, ConfigVersion,
    ExternalEdits, PendingChange, PlanInfo, PlannedChange, RebuildInfo, VersionInfo,
};
use tidepool_store::{Change, ModelVersion};
use tokio::sync::Mutex;

use crate::AppState;

/// Config files by path.
pub type Files = BTreeMap<String, String>;

/// How often the config directory is looked at.
const WATCH_EVERY: Duration = Duration::from_secs(1);

struct Version {
    info: VersionInfo,
    files: Files,
}

/// What the config directory holds that isn't applied.
struct External {
    problems: Option<String>,
    change: Option<u64>,
}

struct Inner {
    version: u64,
    files: Files,
    catalog: Catalog,
    model: Arc<Model>,
    history: Vec<Version>,
    pending: Vec<PendingChange>,
    next_change: u64,
    /// The directory's files as last seen or written: the watcher acts when
    /// they change.
    on_disk: Option<Files>,
    external: Option<External>,
}

/// The config as the server runs it, and its history.
pub struct ConfigStore {
    /// The config directory, written back to and watched.
    dir: Option<PathBuf>,
    /// Where history and pending changes are kept (`<data>/config`), when
    /// there is a data directory.
    keep: Option<PathBuf>,
    inner: Mutex<Inner>,
}

/// Why a change wasn't made.
#[derive(Debug)]
pub enum Refused {
    /// Made against an older version.
    Stale { current: u64 },
    /// Names a file that can't be a config file.
    BadPath(String),
    /// Doesn't compile.
    Invalid(Box<PlanInfo>, u64),
    /// Destroys data and isn't approved.
    NeedsApproval(Box<PlanInfo>, u64),
    /// A rebuild is still running.
    Busy(String, u64),
    /// No such pending change.
    NoChange(u64),
    /// The history or the config directory couldn't be written.
    Failed(String),
}

impl IntoResponse for Refused {
    fn into_response(self) -> Response {
        let (status, error, current_version, plan) = match self {
            Self::Stale { current } => (
                StatusCode::CONFLICT,
                format!("the config is at version {current} now: plan the change again"),
                current,
                None,
            ),
            Self::BadPath(message) => {
                return crate::handlers::api_error(StatusCode::BAD_REQUEST, message);
            }
            Self::NoChange(id) => {
                return crate::handlers::api_error(
                    StatusCode::NOT_FOUND,
                    format!("no pending change {id}"),
                );
            }
            Self::Failed(message) => {
                return crate::handlers::api_error(StatusCode::INTERNAL_SERVER_ERROR, message);
            }
            Self::Invalid(plan, current) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "the config doesn't compile with this change".to_string(),
                current,
                Some(*plan),
            ),
            Self::NeedsApproval(plan, current) => (
                StatusCode::CONFLICT,
                "this change destroys data: apply it with `approve`, or propose it".to_string(),
                current,
                Some(*plan),
            ),
            Self::Busy(message, current) => (StatusCode::CONFLICT, message, current, None),
        };
        (
            status,
            Json(ConfigError {
                error,
                current_version,
                plan,
            }),
        )
            .into_response()
    }
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Whether `path` can be a config file: the server settings, the model, a
/// table or a dashboard (plan §3.1).
fn valid_path(path: &str) -> bool {
    let named = |rest: &str| {
        rest.strip_suffix(".yaml").is_some_and(|name| {
            !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        })
    };
    path == tidepool_config::project::SERVER_FILE
        || path == tidepool_config::project::MODEL_FILE
        || path.strip_prefix("tables/").is_some_and(named)
        || path.strip_prefix("dashboards/").is_some_and(named)
}

/// The config files in `dir`, as [`ConfigSet::load_dir`] reads them.
///
/// # Errors
/// The directory or a file can't be read.
pub fn read_dir(dir: &Path) -> std::io::Result<Files> {
    let mut files = Files::new();
    for sub in ["", "tables", "dashboards"] {
        let path = dir.join(sub);
        if !path.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(&path)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || !entry.file_type()?.is_file() {
                continue;
            }
            if !Path::new(&name)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("yaml") || e.eq_ignore_ascii_case("yml"))
            {
                continue;
            }
            let relative = if sub.is_empty() {
                name
            } else {
                format!("{sub}/{name}")
            };
            files.insert(relative, std::fs::read_to_string(entry.path())?);
        }
    }
    Ok(files)
}

/// `text` without its `renamed_from` hints: once a rename is applied the
/// catalog keeps the identity, and the hint has done its job (plan §8.4).
/// Hints on lines of their own and inside `{ … }` are removed.
#[must_use]
pub fn strip_renamed_from(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with("renamed_from:") {
            continue;
        }
        let mut line = line.to_string();
        while let Some(at) = line.find("renamed_from:") {
            // `, renamed_from: x` or `renamed_from: x, ` inside a flow map
            let end = line[at..].find([',', '}']).map_or(line.len(), |e| at + e);
            let before = line[..at].trim_end();
            if let Some(comma) = before.strip_suffix(',') {
                let rest = line[end..].trim_start();
                let gap = if rest.starts_with('}') { " " } else { "" };
                line = format!("{comma}{gap}{rest}");
            } else if line[end..].starts_with(',') {
                line = format!("{}{}", &line[..at], line[end + 1..].trim_start());
            } else {
                line = format!("{}{}", &line[..at], &line[end..]);
            }
        }
        out.push_str(&line);
    }
    out
}

fn class_label(class: ChangeClass) -> &'static str {
    match class {
        ChangeClass::Safe => "safe",
        ChangeClass::Rebuild => "rebuild",
        ChangeClass::Destructive => "destructive",
    }
}

/// A compiled change: the plan as the API shows it, and what applying needs.
struct Assessed {
    info: PlanInfo,
    compiled: Option<Compiled>,
    plan: Option<Plan>,
}

fn assess(inner: &Inner, files: &Files, base_version: u64) -> Assessed {
    let (set, mut diagnostics) = ConfigSet::from_files(files.clone());
    let (compiled, more) = compile(&set, &inner.catalog);
    diagnostics.extend(more);
    let problems = problems(&diagnostics, &set);
    let plan = compiled.as_ref().map(|c| diff(&inner.model, &c.model));
    let info = PlanInfo {
        base_version,
        valid: compiled.is_some(),
        problems,
        problems_text: render_all(&diagnostics, &set.files),
        class: plan
            .as_ref()
            .and_then(Plan::class)
            .map(|c| class_label(c).to_string()),
        changes: plan
            .as_ref()
            .map(|p| {
                p.changes
                    .iter()
                    .map(|c| PlannedChange {
                        op: format!("{:?}", c.op).to_lowercase(),
                        kind: c.kind.to_string(),
                        subject: c.subject.clone(),
                        detail: c.detail.clone(),
                        class: class_label(c.class).to_string(),
                        note: c.note.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        warnings: plan
            .as_ref()
            .map(|p| p.warnings.clone())
            .unwrap_or_default(),
        text: plan.as_ref().map(Plan::render).unwrap_or_default(),
    };
    Assessed {
        info,
        compiled,
        plan,
    }
}

fn problems(diagnostics: &Diagnostics, set: &ConfigSet) -> Vec<ConfigProblem> {
    diagnostics
        .iter()
        .map(|d| {
            let at = resolve(d, &set.files);
            let _ = render(d, &set.files);
            ConfigProblem {
                severity: match d.severity {
                    Severity::Error => "error",
                    Severity::Warning => "warning",
                }
                .to_string(),
                message: d.message.clone(),
                file: d.file.as_ref().map(|f| f.to_string_lossy().into_owned()),
                line: at.map(|l| l.line),
                column: at.map(|l| l.col),
                help: d.help.clone(),
            }
        })
        .collect()
}

/// A one-line account of a plan, for the history and commit messages.
fn summary(plan: Option<&Plan>) -> String {
    let Some(plan) = plan.filter(|p| !p.is_empty()) else {
        return "files edited, the model unchanged".to_string();
    };
    let parts: Vec<String> = plan
        .changes
        .iter()
        .take(3)
        .map(|c| {
            let op = match c.op {
                tidepool_model::plan::Op::Add => "add",
                tidepool_model::plan::Op::Remove => "remove",
                tidepool_model::plan::Op::Change => "change",
                tidepool_model::plan::Op::Rename => "rename",
            };
            format!("{op} {} {}", c.kind, c.subject)
        })
        .collect();
    let more = plan.changes.len().saturating_sub(3);
    if more > 0 {
        format!("{}, and {more} more", parts.join(", "))
    } else {
        parts.join(", ")
    }
}

/// `change` applied to `files`.
fn merged(files: &Files, change: &ConfigChange) -> Result<Files, Refused> {
    for path in change.files.keys() {
        if !valid_path(path) {
            return Err(Refused::BadPath(format!(
                "`{path}` can't be a config file: they are tidepool.yaml, model.ossie.yaml, \
                 tables/<name>.yaml and dashboards/<name>.yaml"
            )));
        }
    }
    let mut out = if change.replace {
        Files::new()
    } else {
        files.clone()
    };
    for (path, text) in &change.files {
        match text {
            Some(text) => {
                out.insert(path.clone(), text.clone());
            }
            None => {
                out.remove(path);
            }
        }
    }
    Ok(out)
}

/// Write `file` atomically: a temporary file next to it, then a rename.
fn write_file(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("yaml.tidepool-tmp");
    std::fs::write(&temporary, text)?;
    std::fs::rename(&temporary, path)
}

/// The startup state of the config: what [`ConfigStore::start`] works out.
pub struct Started {
    pub store: ConfigStore,
    /// The model change startup makes, if the directory moved on from the
    /// data's model.
    pub change: Option<ModelVersion>,
}

impl ConfigStore {
    /// The config of a server whose writer holds `live` (recovered from
    /// the data, or the directory's own when the data is new), with the
    /// directory's files `files`.
    ///
    /// If the directory holds what the data's version was made from, that
    /// is the config. If it moved on while the server was down, the
    /// difference is startup's change (plan §8.1, startup): made like any
    /// other, except that a destructive one needs `allow_destructive`.
    ///
    /// # Errors
    /// The files don't compile, or would destroy data without
    /// `allow_destructive`, or the history can't be read or written.
    #[allow(clippy::too_many_lines)] // the startup cases, in one place
    pub fn start(
        dir: Option<PathBuf>,
        data: Option<&Path>,
        files: Files,
        live: &ModelVersion,
        allow_destructive: bool,
    ) -> anyhow::Result<Started> {
        let keep = data.map(|d| d.join("config"));
        let (mut history, pending, next_change) = match &keep {
            Some(keep) => load(keep)?,
            None => (Vec::new(), Vec::new(), 1),
        };
        // versions the data never got to (a crash before the swap) are gone
        history.retain(|v| v.info.version <= live.version);
        let known = history
            .iter()
            .find(|v| v.info.version == live.version)
            .map(|v| v.files.clone());
        let mut inner = Inner {
            version: live.version,
            files: known.clone().unwrap_or_default(),
            catalog: (*live.catalog).clone(),
            model: Arc::clone(&live.model),
            history,
            pending,
            next_change,
            on_disk: dir.as_ref().map(|_| files.clone()),
            external: None,
        };
        let assessed = assess(&inner, &files, live.version);
        let Some(compiled) = assessed.compiled else {
            anyhow::bail!("the config has errors:\n{}", assessed.info.problems_text);
        };
        let unchanged = assessed.plan.as_ref().is_none_or(Plan::is_empty);
        let change = match known {
            Some(applied) if applied == files => None,
            // no record of the data's version (new data, or no history),
            // and the directory makes the same model: it is that version
            None if unchanged => {
                inner.files = files;
                inner.catalog = compiled.catalog;
                inner.history.push(Version {
                    info: VersionInfo {
                        version: live.version,
                        at: now(),
                        author: "tidepool".into(),
                        source: "startup".into(),
                        summary: "the config the server started with".into(),
                        class: None,
                    },
                    files: inner.files.clone(),
                });
                if let Some(keep) = &keep {
                    save_version(keep, &inner, live.version)?;
                }
                None
            }
            _ => {
                let class = assessed.plan.as_ref().and_then(Plan::class);
                if class == Some(ChangeClass::Destructive) && !allow_destructive {
                    anyhow::bail!(
                        "the config directory changed since the data was written, and applying \
                         it would destroy data:\n{}\nstart with --allow-destructive to apply it",
                        assessed.info.text
                    );
                }
                let version = live.version + 1;
                let stored: Files = files
                    .iter()
                    .map(|(p, t)| (p.clone(), strip_renamed_from(t)))
                    .collect();
                inner.history.push(Version {
                    info: VersionInfo {
                        version,
                        at: now(),
                        author: "tidepool".into(),
                        source: "startup".into(),
                        summary: summary(assessed.plan.as_ref()),
                        class: class.map(|c| class_label(c).to_string()),
                    },
                    files: stored.clone(),
                });
                if let Some(keep) = &keep {
                    save_version(keep, &inner, version)?;
                }
                if let Some(dir) = &dir
                    && stored != files
                    && compiled.model.server.config_writeback.enabled
                {
                    // the rename hints have done their job
                    write_back(dir, &files, &stored)?;
                    inner.on_disk = Some(stored.clone());
                }
                let target = ModelVersion {
                    version,
                    model: Arc::new(compiled.model),
                    catalog: Arc::new(compiled.catalog.clone()),
                };
                inner.version = version;
                inner.files = stored;
                inner.catalog = compiled.catalog;
                inner.model = Arc::clone(&target.model);
                Some(target)
            }
        };
        Ok(Started {
            store: Self {
                dir,
                keep,
                inner: Mutex::new(inner),
            },
            change,
        })
    }

    /// A config store for a model with no directory and no history: tests,
    /// and servers started without `--config`.
    #[must_use]
    pub fn detached(live: &ModelVersion) -> Self {
        Self {
            dir: None,
            keep: None,
            inner: Mutex::new(Inner {
                version: live.version,
                files: Files::new(),
                catalog: (*live.catalog).clone(),
                model: Arc::clone(&live.model),
                history: vec![Version {
                    info: VersionInfo {
                        version: live.version,
                        at: now(),
                        author: "tidepool".into(),
                        source: "startup".into(),
                        summary: "a model given in code, without config files".into(),
                        class: None,
                    },
                    files: Files::new(),
                }],
                pending: Vec::new(),
                next_change: 1,
                on_disk: None,
                external: None,
            }),
        }
    }
}

fn save_version(keep: &Path, inner: &Inner, version: u64) -> anyhow::Result<()> {
    let Some(v) = inner.history.iter().find(|v| v.info.version == version) else {
        return Ok(());
    };
    let body = serde_json::json!({ "info": v.info, "files": v.files });
    let path = keep.join("versions").join(format!("{version:08}.json"));
    write_file(&path, &body.to_string())?;
    Ok(())
}

fn save_pending(keep: &Path, inner: &Inner) -> anyhow::Result<()> {
    let body = serde_json::json!({ "next": inner.next_change, "pending": inner.pending });
    write_file(&keep.join("pending.json"), &body.to_string())?;
    Ok(())
}

type Loaded = (Vec<Version>, Vec<PendingChange>, u64);

fn load(keep: &Path) -> anyhow::Result<Loaded> {
    let mut history = Vec::new();
    let versions = keep.join("versions");
    if versions.is_dir() {
        let mut paths: Vec<PathBuf> = std::fs::read_dir(&versions)?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .collect();
        paths.sort();
        for path in paths {
            #[derive(serde::Deserialize)]
            struct Stored {
                info: VersionInfo,
                files: Files,
            }
            let stored: Stored = serde_json::from_slice(&std::fs::read(&path)?)
                .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
            history.push(Version {
                info: stored.info,
                files: stored.files,
            });
        }
    }
    let (pending, next) = match std::fs::read(keep.join("pending.json")) {
        Ok(bytes) => {
            #[derive(serde::Deserialize)]
            struct Stored {
                next: u64,
                pending: Vec<PendingChange>,
            }
            let stored: Stored =
                serde_json::from_slice(&bytes).map_err(|e| anyhow::anyhow!("pending.json: {e}"))?;
            (stored.pending, stored.next)
        }
        Err(_) => (Vec::new(), 1),
    };
    Ok((history, pending, next))
}

/// What a change does, without making it.
///
/// # Errors
/// The change is against an old version or names a bad path.
pub async fn plan(state: &AppState, change: &ConfigChange) -> Result<PlanInfo, Refused> {
    let inner = state.config.inner.lock().await;
    if change.base_version != inner.version {
        return Err(Refused::Stale {
            current: inner.version,
        });
    }
    let files = merged(&inner.files, change)?;
    Ok(assess(&inner, &files, inner.version).info)
}

/// Make a change (plan §8.1). `source` says where it came from.
///
/// # Errors
/// See [`Refused`].
#[allow(clippy::too_many_lines)]
pub async fn submit(
    state: &AppState,
    change: ConfigChange,
    source: &str,
) -> Result<ApplyResult, Refused> {
    let mut inner = state.config.inner.lock().await;
    let current = inner.version;
    if change.base_version != current {
        return Err(Refused::Stale { current });
    }
    let files = merged(&inner.files, &change)?;
    let assessed = assess(&inner, &files, current);
    let Some(compiled) = assessed.compiled else {
        return Err(Refused::Invalid(Box::new(assessed.info), current));
    };
    if files == inner.files {
        return Ok(ApplyResult {
            status: "unchanged".into(),
            version: current,
            change: None,
            plan: assessed.info,
        });
    }
    let author = change.author.clone().unwrap_or_else(|| source.to_string());
    if change.propose {
        let id = inner.next_change;
        inner.next_change += 1;
        inner.pending.push(PendingChange {
            id,
            source: source.to_string(),
            author,
            at: now(),
            base_version: current,
            files: change.files.clone(),
            replace: change.replace,
            plan: assessed.info.clone(),
        });
        if let Some(keep) = &state.config.keep {
            save_pending(keep, &inner).map_err(|e| Refused::Failed(e.to_string()))?;
        }
        return Ok(ApplyResult {
            status: "queued".into(),
            version: current,
            change: Some(id),
            plan: assessed.info,
        });
    }
    let class = assessed.plan.as_ref().and_then(Plan::class);
    if class == Some(ChangeClass::Destructive) && !change.approve {
        return Err(Refused::NeedsApproval(Box::new(assessed.info), current));
    }
    if let Some((version, _, _)) = state.writer.rebuild() {
        return Err(Refused::Busy(
            format!("version {version} is still being rebuilt: try again when it is live"),
            current,
        ));
    }

    let version = current + 1;
    let stored: Files = files
        .iter()
        .map(|(p, t)| (p.clone(), strip_renamed_from(t)))
        .collect();
    let target = ModelVersion {
        version,
        model: Arc::new(compiled.model),
        catalog: Arc::new(compiled.catalog.clone()),
    };
    let status = match state.writer.change_model(target.clone()).await {
        Ok(Change::Done { .. }) => "applied",
        Ok(Change::Rebuilding { .. }) => "rebuilding",
        Err(message) => return Err(Refused::Busy(message, current)),
    };
    let summary = summary(assessed.plan.as_ref());
    inner.history.push(Version {
        info: VersionInfo {
            version,
            at: now(),
            author: author.clone(),
            source: source.to_string(),
            summary: summary.clone(),
            class: class.map(|c| class_label(c).to_string()),
        },
        files: stored.clone(),
    });
    let old = std::mem::replace(&mut inner.files, stored);
    inner.version = version;
    inner.catalog = compiled.catalog;
    inner.model = target.model;
    if let Some(keep) = &state.config.keep {
        save_version(keep, &inner, version).map_err(|e| Refused::Failed(e.to_string()))?;
    }
    let writeback = inner.model.server.config_writeback;
    if let Some(dir) = state.config.dir.as_ref().filter(|_| writeback.enabled) {
        write_back(dir, &old, &inner.files).map_err(|e| {
            Refused::Failed(format!(
                "the change is live, but writing it back failed: {e}"
            ))
        })?;
        inner.on_disk = Some(inner.files.clone());
        inner.external = None;
        if writeback.git_commit {
            commit(dir, &format!("tidepool: {summary} [by {author}]")).await;
        }
    }
    Ok(ApplyResult {
        status: status.into(),
        version,
        change: None,
        plan: assessed.info,
    })
}

/// Bring the directory from `old` to `new`.
fn write_back(dir: &Path, old: &Files, new: &Files) -> std::io::Result<()> {
    for (path, text) in new {
        let on_disk = std::fs::read_to_string(dir.join(path)).ok();
        if old.get(path) != Some(text) || on_disk.as_deref() != Some(text) {
            write_file(&dir.join(path), text)?;
        }
    }
    for path in old.keys().filter(|p| !new.contains_key(*p)) {
        match std::fs::remove_file(dir.join(path)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    Ok(())
}

/// Commit the config directory, if it is in a git repository. Failing to
/// commit is logged, not an error: the change is applied and written.
async fn commit(dir: &Path, message: &str) {
    let git = |args: &[&str]| {
        let mut command = tokio::process::Command::new("git");
        command.arg("-C").arg(dir).args(args);
        command.stdout(std::process::Stdio::null());
        command.stderr(std::process::Stdio::piped());
        command
    };
    let inside = git(&["rev-parse", "--is-inside-work-tree"]).output().await;
    if !inside.is_ok_and(|o| o.status.success()) {
        return;
    }
    for args in [
        vec!["add", "-A", "--", "."],
        vec!["commit", "-q", "-m", message, "--", "."],
    ] {
        match git(&args).output().await {
            Ok(out) if out.status.success() => {}
            Ok(out) => {
                tracing::warn!(
                    "git {} in {}: {}",
                    args[0],
                    dir.display(),
                    String::from_utf8_lossy(&out.stderr).trim()
                );
                return;
            }
            Err(e) => {
                tracing::warn!("running git in {}: {e}", dir.display());
                return;
            }
        }
    }
}

/// Approve a pending change: apply it as proposed, destructive or not.
///
/// # Errors
/// See [`Refused`].
pub async fn approve(state: &AppState, id: u64) -> Result<ApplyResult, Refused> {
    let pending = {
        let inner = state.config.inner.lock().await;
        inner
            .pending
            .iter()
            .find(|p| p.id == id)
            .cloned()
            .ok_or(Refused::NoChange(id))?
    };
    let result = submit(
        state,
        ConfigChange {
            base_version: pending.base_version,
            files: pending.files.clone(),
            replace: pending.replace,
            approve: true,
            propose: false,
            author: Some(pending.author.clone()),
        },
        &pending.source,
    )
    .await?;
    drop_pending(state, id).await?;
    Ok(result)
}

/// Drop a pending change.
///
/// # Errors
/// There is no such change, or the queue can't be saved.
pub async fn drop_pending(state: &AppState, id: u64) -> Result<PendingChange, Refused> {
    let mut inner = state.config.inner.lock().await;
    let at = inner
        .pending
        .iter()
        .position(|p| p.id == id)
        .ok_or(Refused::NoChange(id))?;
    let dropped = inner.pending.remove(at);
    if inner
        .external
        .as_ref()
        .is_some_and(|e| e.change == Some(id))
    {
        inner.external = None;
    }
    if let Some(keep) = &state.config.keep {
        save_pending(keep, &inner).map_err(|e| Refused::Failed(e.to_string()))?;
    }
    Ok(dropped)
}

/// Look at the config directory once: edits that are safe are applied,
/// others wait for approval, a config that doesn't compile is reported
/// (plan §8.1, file watcher).
pub async fn look(state: &AppState) {
    let Some(dir) = &state.config.dir else { return };
    let change = {
        let mut inner = state.config.inner.lock().await;
        let Ok(found) = read_dir(dir) else { return };
        if inner.on_disk.as_ref() == Some(&found) {
            return;
        }
        inner.on_disk = Some(found.clone());
        // an earlier proposal from the directory is replaced by what it holds now
        let replaced = inner.pending.iter().any(|p| p.source == "files");
        inner.pending.retain(|p| p.source != "files");
        if replaced
            && let Some(keep) = &state.config.keep
            && let Err(e) = save_pending(keep, &inner)
        {
            tracing::warn!("saving pending changes: {e}");
        }
        if found == inner.files {
            inner.external = None;
            return;
        }
        // what differs from the applied files: set or removed
        let mut files: BTreeMap<String, Option<String>> = found
            .iter()
            .filter(|(p, t)| inner.files.get(*p) != Some(*t))
            .map(|(p, t)| (p.clone(), Some(t.clone())))
            .collect();
        for path in inner.files.keys().filter(|p| !found.contains_key(*p)) {
            files.insert(path.clone(), None);
        }
        let assessed = assess(&inner, &found, inner.version);
        if !assessed.info.valid {
            inner.external = Some(External {
                problems: Some(assessed.info.problems_text),
                change: None,
            });
            return;
        }
        let safe = assessed
            .plan
            .as_ref()
            .and_then(Plan::class)
            .is_none_or(|c| c == ChangeClass::Safe);
        ConfigChange {
            base_version: inner.version,
            files: files.into_iter().filter(|(p, _)| valid_path(p)).collect(),
            replace: false,
            approve: false,
            propose: !safe,
            author: Some("config directory".into()),
        }
    };
    match submit(state, change, "files").await {
        Ok(result) => {
            let mut inner = state.config.inner.lock().await;
            inner.external = result.change.map(|id| External {
                problems: None,
                change: Some(id),
            });
            if result.status != "queued" {
                tracing::info!(
                    "applied edits from the config directory: version {}",
                    result.version
                );
            }
        }
        Err(refused) => tracing::warn!("edits in the config directory: {refused:?}"),
    }
}

/// Look at the config directory every second, for as long as the server
/// runs.
pub fn watch(state: &Arc<AppState>) {
    if state.config.dir.is_none() {
        return;
    }
    let weak: Weak<AppState> = Arc::downgrade(state);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(WATCH_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let Some(state) = weak.upgrade() else { break };
            look(&state).await;
        }
    });
}

// handlers

pub async fn get_config(State(state): State<Arc<AppState>>) -> Json<ConfigState> {
    let inner = state.config.inner.lock().await;
    let snapshot = state.writer.snapshot();
    Json(ConfigState {
        version: inner.version,
        files: inner
            .files
            .iter()
            .map(|(path, text)| ConfigFile {
                path: path.clone(),
                text: text.clone(),
            })
            .collect(),
        live_version: snapshot.version,
        rebuild: rebuild_info(&state),
        external: inner.external.as_ref().map(|e| ExternalEdits {
            change: e.change,
            problems: e.problems.clone(),
        }),
        writeback: state.config.dir.is_some() && inner.model.server.config_writeback.enabled,
    })
}

/// The rebuild in progress, for the config and status pages.
#[must_use]
pub fn rebuild_info(state: &AppState) -> Option<RebuildInfo> {
    state
        .writer
        .rebuild()
        .map(|(version, rows_done, rows)| RebuildInfo {
            version,
            rows_done,
            rows,
        })
}

pub async fn plan_config(
    State(state): State<Arc<AppState>>,
    Json(change): Json<ConfigChange>,
) -> Result<Json<PlanInfo>, Refused> {
    plan(&state, &change).await.map(Json)
}

pub async fn apply_config(
    State(state): State<Arc<AppState>>,
    Json(change): Json<ConfigChange>,
) -> Result<Json<ApplyResult>, Refused> {
    let source = if change.replace { "revert" } else { "api" };
    submit(&state, change, source).await.map(Json)
}

pub async fn list_versions(State(state): State<Arc<AppState>>) -> Json<Vec<VersionInfo>> {
    let inner = state.config.inner.lock().await;
    Json(inner.history.iter().rev().map(|v| v.info.clone()).collect())
}

pub async fn get_version(
    State(state): State<Arc<AppState>>,
    UrlPath(version): UrlPath<u64>,
) -> Response {
    let inner = state.config.inner.lock().await;
    match inner.history.iter().find(|v| v.info.version == version) {
        Some(v) => Json(ConfigVersion {
            info: v.info.clone(),
            files: v
                .files
                .iter()
                .map(|(path, text)| ConfigFile {
                    path: path.clone(),
                    text: text.clone(),
                })
                .collect(),
        })
        .into_response(),
        None => crate::handlers::api_error(StatusCode::NOT_FOUND, format!("no version {version}")),
    }
}

pub async fn list_changes(State(state): State<Arc<AppState>>) -> Json<Vec<PendingChange>> {
    Json(state.config.inner.lock().await.pending.clone())
}

pub async fn approve_change(
    State(state): State<Arc<AppState>>,
    UrlPath(id): UrlPath<u64>,
) -> Result<Json<ApplyResult>, Refused> {
    approve(&state, id).await.map(Json)
}

pub async fn reject_change(
    State(state): State<Arc<AppState>>,
    UrlPath(id): UrlPath<u64>,
) -> Result<Json<PendingChange>, Refused> {
    drop_pending(&state, id).await.map(Json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename_hints_go_and_everything_else_stays() {
        let text = "name: sales\nrenamed_from: orders\ncolumns:\n  total: { type: int64, renamed_from: amount }\n  b: { renamed_from: a, type: string }\n  c: { type: string }\n";
        assert_eq!(
            strip_renamed_from(text),
            "name: sales\ncolumns:\n  total: { type: int64 }\n  b: { type: string }\n  c: { type: string }\n"
        );
    }

    #[test]
    fn only_config_files_can_be_written() {
        for good in [
            "tidepool.yaml",
            "model.ossie.yaml",
            "tables/returns.yaml",
            "dashboards/ops-2.yaml",
        ] {
            assert!(valid_path(good), "{good}");
        }
        for bad in [
            "../etc/passwd",
            "tables/../x.yaml",
            "tables/a/b.yaml",
            "tables/.yaml",
            "notes.txt",
            "/tables/x.yaml",
        ] {
            assert!(!valid_path(bad), "{bad}");
        }
    }
}
