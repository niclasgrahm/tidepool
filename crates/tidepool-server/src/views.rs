//! Views (`POST /v1/views`): dashboards that live only in memory.
//!
//! An agent answering a question shows a view, not a new dashboard file:
//! a view is compiled exactly as a dashboard file would be (the config's
//! files plus this one, through the one compiler, so every rule and every
//! located diagnostic applies), opened live like a dashboard (its id in
//! `open_dashboard`), and never written, versioned or committed. Saving one
//! is an ordinary config change of its YAML.
//!
//! Views are recompiled when the config changes, the first time they're
//! looked at after. They're kept for a day without use, and at most
//! [`MAX`] of them; the least recently used goes first.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::Value;
use tidepool_config::ConfigSet;
use tidepool_config::dashboard::DashboardFile;
use tidepool_config::diag::render_all;
use tidepool_model::compile;
use tidepool_model::ir::Dashboard;
use tidepool_protocol::live::{ViewInfo, ViewProblems};
use tidepool_store::Snapshot;

use crate::AppState;

/// Views kept at most.
pub(crate) const MAX: usize = 1000;

/// How long a view nobody looks at is kept, in microseconds.
const IDLE: i64 = 24 * 60 * 60 * 1_000_000;

/// Whether `id` names a view rather than a dashboard file. A file name
/// can't start with `~` (config paths are letters, digits, `_` and `-`).
pub(crate) fn is_view(id: &str) -> bool {
    id.starts_with('~')
}

struct View {
    file: DashboardFile,
    /// The config version it was last compiled for, and what came of it.
    compiled: Option<(u64, Result<Arc<Dashboard>, String>)>,
    /// When it was last made or looked at (the server's clock).
    used: i64,
}

#[derive(Default)]
pub(crate) struct Views {
    views: Mutex<HashMap<String, View>>,
}

/// Why a view couldn't be made or shown.
pub(crate) enum Refused {
    /// The dashboard doesn't compile.
    Problems(ViewProblems),
    /// It can't be compiled now (no config files, a change being rebuilt).
    Unavailable(String),
}

impl IntoResponse for Refused {
    fn into_response(self) -> Response {
        match self {
            Self::Problems(problems) => {
                (StatusCode::UNPROCESSABLE_ENTITY, Json(problems)).into_response()
            }
            Self::Unavailable(message) => crate::handlers::api_error(StatusCode::CONFLICT, message),
        }
    }
}

/// A new view id: `~` and 128 random bits.
fn new_id() -> Option<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).ok()?;
    Some(bytes.iter().fold(String::from("~"), |mut id, b| {
        let _ = std::fmt::Write::write_fmt(&mut id, format_args!("{b:02x}"));
        id
    }))
}

/// Compile `file` as one more dashboard of the applied config, for the
/// snapshot's model: the compiled dashboard, slugged `id`.
fn compiled(
    state: &AppState,
    snapshot: &Snapshot,
    id: &str,
    file: &DashboardFile,
) -> Result<Dashboard, Refused> {
    let published = state.config.published();
    if published.files.is_empty() {
        return Err(Refused::Unavailable(
            "this server runs a model given in code: views need its config files \
             (start it with --config)"
                .into(),
        ));
    }
    if published.version != snapshot.version {
        return Err(Refused::Unavailable(format!(
            "version {} of the config is still being rebuilt: try again when it is live",
            published.version
        )));
    }
    // a name no dashboard file has, so the view is the only one under it
    let mut stem = String::from("view");
    while published
        .files
        .contains_key(&format!("dashboards/{stem}.yaml"))
    {
        stem.push('_');
    }
    let path = format!("dashboards/{stem}.yaml");
    let mut files = published.files.clone();
    files.insert(path.clone(), file.to_yaml());
    let (set, mut diagnostics) = ConfigSet::from_files(files);
    let (model, more) = compile(&set, &published.catalog);
    diagnostics.extend(more);
    let found = model.and_then(|c| c.model.dashboards.into_iter().find(|d| d.slug == stem));
    match found {
        Some(mut dashboard) if diagnostics.iter().all(|d| !d.is_error()) => {
            dashboard.slug = id.to_string();
            Ok(dashboard)
        }
        _ => Err(Refused::Problems(ViewProblems {
            error: "the view doesn't compile".into(),
            problems: crate::config::problems(&diagnostics, &set),
            problems_text: render_all(&diagnostics, &set.files),
        })),
    }
}

/// Give every visual without a place one: one visual takes the width,
/// more go two to a row, below whatever is placed already.
fn placed(mut file: Value) -> Value {
    let columns = file["layout"]["columns"].as_u64().unwrap_or(12).max(2);
    let Some(visuals) = file.get_mut("visuals").and_then(Value::as_array_mut) else {
        return file;
    };
    let mut below = visuals
        .iter()
        .filter_map(|v| Some(v["at"]["y"].as_u64()? + v["at"]["h"].as_u64()?))
        .max()
        .unwrap_or(0);
    let unplaced = visuals.iter().filter(|v| v.get("at").is_none()).count();
    let (w, h) = if unplaced == 1 {
        (columns, 5)
    } else {
        (columns / 2, 4)
    };
    let mut x = 0;
    for visual in visuals.iter_mut().filter(|v| v.get("at").is_none()) {
        if x + w > columns {
            x = 0;
            below += h;
        }
        visual["at"] = serde_json::json!({ "x": x, "y": below, "w": w, "h": h });
        x += w;
    }
    file
}

/// Forget views unused for [`IDLE`], then the least recently used until
/// there's room for one more under `max`.
fn make_room(views: &mut HashMap<String, View>, now: i64, max: usize) {
    views.retain(|_, v| now - v.used < IDLE);
    while views.len() >= max {
        let oldest = views
            .iter()
            .min_by_key(|(_, v)| v.used)
            .map(|(id, _)| id.clone());
        let Some(oldest) = oldest else { break };
        views.remove(&oldest);
    }
}

impl Views {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, View>> {
        self.views.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Make a view of `file`: its id and compiled dashboard.
    pub(crate) fn create(
        &self,
        state: &AppState,
        file: DashboardFile,
    ) -> Result<(String, Arc<Dashboard>), Refused> {
        let snapshot = state.writer.snapshot();
        let Some(id) = new_id() else {
            return Err(Refused::Unavailable("no random source for an id".into()));
        };
        let dashboard = Arc::new(compiled(state, &snapshot, &id, &file)?);
        let now = state.clock.now();
        let mut views = self.lock();
        make_room(&mut views, now, MAX);
        views.insert(
            id.clone(),
            View {
                file,
                compiled: Some((snapshot.version, Ok(Arc::clone(&dashboard)))),
                used: now,
            },
        );
        Ok((id, dashboard))
    }

    /// View `id` as of `snapshot`, compiled again if the config changed
    /// since: `None` if there's no such view, an error if it no longer
    /// compiles.
    pub(crate) fn get(
        &self,
        state: &AppState,
        snapshot: &Snapshot,
        id: &str,
    ) -> Option<Result<Arc<Dashboard>, String>> {
        let now = state.clock.now();
        let file = {
            let mut views = self.lock();
            let view = views.get_mut(id)?;
            if now - view.used >= IDLE {
                views.remove(id);
                return None;
            }
            view.used = now;
            match &view.compiled {
                Some((version, done)) if *version == snapshot.version => {
                    return Some(done.clone());
                }
                _ => view.file.clone(),
            }
        };
        // compiled outside the lock: views are looked at on every epoch
        let done = match compiled(state, snapshot, id, &file) {
            Ok(dashboard) => Ok(Arc::new(dashboard)),
            Err(Refused::Problems(p)) => Err(format!(
                "the view no longer fits the model:\n{}",
                p.problems_text
            )),
            // nothing to compile against yet: the last good one, if any
            Err(Refused::Unavailable(message)) => {
                let views = self.lock();
                return match views.get(id).and_then(|v| v.compiled.as_ref()) {
                    Some((_, Ok(dashboard))) => Some(Ok(Arc::clone(dashboard))),
                    _ => Some(Err(message)),
                };
            }
        };
        if let Some(view) = self.lock().get_mut(id) {
            view.compiled = Some((snapshot.version, done.clone()));
        }
        Some(done)
    }

    /// View `id`'s dashboard file.
    pub(crate) fn file(&self, id: &str) -> Option<DashboardFile> {
        self.lock().get(id).map(|v| v.file.clone())
    }

    /// Views kept.
    pub(crate) fn len(&self) -> usize {
        self.lock().len()
    }
}

fn info(state: &AppState, snapshot: &Snapshot, id: String, dashboard: &Dashboard) -> ViewInfo {
    let yaml = state
        .views
        .file(&id)
        .map(|f| f.to_yaml())
        .unwrap_or_default();
    ViewInfo {
        dashboard: crate::live::dashboard_info(&snapshot.model, dashboard),
        id,
        yaml,
    }
}

/// `POST /v1/views`. See `Operation::CreateView` in the endpoint table.
pub async fn create_view(State(state): State<Arc<AppState>>, Json(body): Json<Value>) -> Response {
    let file: DashboardFile = match serde_json::from_value(placed(body)) {
        Ok(file) => file,
        Err(err) => {
            return crate::handlers::api_error(
                StatusCode::BAD_REQUEST,
                format!("not a dashboard file: {err}"),
            );
        }
    };
    let worker = Arc::clone(&state);
    let made = tokio::task::spawn_blocking(move || worker.views.create(&worker, file)).await;
    match made {
        Ok(Ok((id, dashboard))) => {
            let snapshot = state.writer.snapshot();
            Json(info(&state, &snapshot, id, &dashboard)).into_response()
        }
        Ok(Err(refused)) => refused.into_response(),
        Err(_) => crate::handlers::api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the view couldn't be compiled",
        ),
    }
}

/// `GET /v1/views/{id}`. See `Operation::GetView` in the endpoint table.
pub async fn get_view(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let snapshot = state.writer.snapshot();
    match state.views.get(&state, &snapshot, &id) {
        Some(Ok(dashboard)) => Json(info(&state, &snapshot, id, &dashboard)).into_response(),
        Some(Err(message)) => crate::handlers::api_error(StatusCode::CONFLICT, message),
        None => crate::handlers::api_error(StatusCode::NOT_FOUND, format!("no view `{id}`")),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn view_ids_are_never_file_names() {
        let Some(id) = new_id() else {
            panic!("no random source")
        };
        assert!(is_view(&id));
        assert_eq!(id.len(), 33);
        assert!(!is_view("sales"));
    }

    #[test]
    fn the_idle_and_the_least_used_views_make_room() {
        let view = |used| View {
            file: DashboardFile {
                name: "v".into(),
                layout: tidepool_config::dashboard::Layout::default(),
                filters: Vec::new(),
                visuals: Vec::new(),
            },
            compiled: None,
            used,
        };
        let mut views: HashMap<String, View> = [
            ("~old".to_string(), view(0)),
            ("~a".to_string(), view(IDLE + 10)),
            ("~b".to_string(), view(IDLE + 20)),
            ("~c".to_string(), view(IDLE + 30)),
        ]
        .into_iter()
        .collect();
        // a day after `~old` was last used, it goes; then `~a`, used least
        make_room(&mut views, IDLE + 40, 3);
        let mut left: Vec<&str> = views.keys().map(String::as_str).collect();
        left.sort_unstable();
        assert_eq!(left, ["~b", "~c"]);
    }

    #[test]
    fn visuals_without_a_place_get_one() {
        let one = placed(json!({"name": "v", "visuals": [{"id": "a"}]}));
        assert_eq!(
            one["visuals"][0]["at"],
            json!({"x": 0, "y": 0, "w": 12, "h": 5})
        );
        let three = placed(json!({"name": "v", "visuals": [
            {"id": "kept", "at": {"x": 0, "y": 0, "w": 12, "h": 2}},
            {"id": "a"}, {"id": "b"}, {"id": "c"},
        ]}));
        let at = |i: usize| three["visuals"][i]["at"].clone();
        assert_eq!(at(0), json!({"x": 0, "y": 0, "w": 12, "h": 2}));
        assert_eq!(at(1), json!({"x": 0, "y": 2, "w": 6, "h": 4}));
        assert_eq!(at(2), json!({"x": 6, "y": 2, "w": 6, "h": 4}));
        assert_eq!(at(3), json!({"x": 0, "y": 6, "w": 6, "h": 4}));
    }
}
