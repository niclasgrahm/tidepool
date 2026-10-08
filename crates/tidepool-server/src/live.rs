//! Live dashboards over a WebSocket (`/v1/ws`, plan §6.6 and §7).
//!
//! The [`Hub`] holds sessions and knows nothing of sockets: the socket
//! handler feeds it [`ClientMessage`]s and forwards what it sends, and
//! tests drive it directly. Per session there is a selection with
//! back/forward history, the dashboard on screen, filter-list windows, and
//! what was last sent, so a frame carries only what changed.
//!
//! Subscriptions aren't a separate structure: every visual of every session
//! is run through the one shared [`Engine`](tidepool_engine::Engine) with its
//! selection cut down to the fields that can change it
//! ([`tidepool_engine::relevant`]). Sessions showing the same thing then ask
//! the same question, which the engine answers once per epoch (its result
//! cache), and keeps up to date incrementally (its kept anchors).
//!
//! An epoch loop refreshes every live session when a new epoch is
//! published. It always takes the newest snapshot, so a slow refresh skips
//! epochs rather than falling behind (coalescing); a selection change is
//! answered right away, outside the loop (interactive first).

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeSet, HashMap};
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use serde_json::Value as Json;
use tidepool_engine::{Condition, Query, Selection};
use tidepool_expr::Scalar;
use tidepool_model::ir::{Dashboard, FieldRef, Model, SortKey};
use tidepool_protocol::live::{
    ClientMessage, DashboardInfo, FilterFrame, FilterInfo, FilterValue, GridAt, SelectMode,
    SelectedField, ServerMessage, ValueState, VisualFrame, VisualInfo,
};
use tidepool_protocol::{QueryDimension, QueryRequest, QuerySort};
use tidepool_store::Snapshot;
use tokio::sync::mpsc;

use crate::AppState;
use crate::query::{field, json, resolve, result, scalar};

/// Messages a session hasn't sent yet, at most; past this a slow client is
/// dropped rather than buffered without end.
const BACKLOG: usize = 256;

/// One filter list's window.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Window {
    offset: usize,
    limit: usize,
    search: Option<String>,
}

impl Default for Window {
    fn default() -> Self {
        Self {
            offset: 0,
            limit: 100,
            search: None,
        }
    }
}

#[derive(Default)]
struct SessionState {
    /// Every selection so far; `at` is the current one.
    history: Vec<Selection>,
    at: usize,
    dashboard: Option<String>,
    paused: bool,
    windows: HashMap<String, Window>,
    /// Sort overrides by visual: (result column, descending).
    sorts: HashMap<String, (String, bool)>,
    /// What was last sent per visual or filter (a hash of it).
    sent: HashMap<String, u64>,
    out: Option<mpsc::Sender<ServerMessage>>,
}

impl SessionState {
    fn selection(&self) -> Selection {
        self.history.get(self.at).cloned().unwrap_or_default()
    }

    /// Make `next` the current selection, dropping anything after it.
    fn push(&mut self, next: Selection) {
        if next == self.selection() {
            return;
        }
        self.history.truncate(self.at + 1);
        self.history.push(next);
        self.at = self.history.len() - 1;
    }
}

pub(crate) struct Session {
    pub(crate) id: String,
    state: Mutex<SessionState>,
}

impl Session {
    fn lock(&self) -> std::sync::MutexGuard<'_, SessionState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn send(&self, message: ServerMessage) {
        let out = self.lock().out.clone();
        if let Some(out) = out
            && out.try_send(message).is_err()
        {
            // full or gone: this connection is done
            self.lock().out = None;
        }
    }
}

/// Every session.
#[derive(Default)]
pub(crate) struct Hub {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    next: AtomicU64,
    /// Whether the epoch loop runs.
    running: AtomicBool,
    /// The model sessions were last told about.
    model: Mutex<Option<Arc<Model>>>,
}

impl Hub {
    fn sessions(&self) -> Vec<Arc<Session>> {
        self.sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .cloned()
            .collect()
    }

    /// Start or resume a session, sending to `out` from now on.
    pub(crate) fn connect(
        &self,
        state: &AppState,
        id: Option<String>,
        out: mpsc::Sender<ServerMessage>,
    ) -> Arc<Session> {
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        let known = id.and_then(|id| sessions.get(&id).cloned());
        let session = known.unwrap_or_else(|| {
            let id = format!("s{}", self.next.fetch_add(1, Ordering::Relaxed) + 1);
            let session = Arc::new(Session {
                id: id.clone(),
                state: Mutex::new(SessionState {
                    history: vec![Selection::default()],
                    ..SessionState::default()
                }),
            });
            sessions.insert(id, Arc::clone(&session));
            session
        });
        drop(sessions);
        {
            let mut inner = session.lock();
            inner.out = Some(out);
            // a new connection has seen nothing
            inner.sent.clear();
        }
        let snapshot = state.writer.snapshot();
        session.send(ServerMessage::Welcome {
            session_id: session.id.clone(),
            epoch: snapshot.epoch,
        });
        session.send(model_message(&snapshot));
        // built first: `send` takes the session lock itself
        let selection = selection_message(&snapshot.model, &session.lock());
        session.send(selection);
        refresh(state, &session, &snapshot);
        session
    }

    /// A message from a session's client.
    #[allow(clippy::unused_self)] // a hub's operation, like the others
    pub(crate) fn handle(&self, state: &AppState, session: &Session, message: ClientMessage) {
        let snapshot = state.writer.snapshot();
        let model = &snapshot.model;
        let changed = {
            let mut inner = session.lock();
            match apply(model, &mut inner, message) {
                Ok(changed) => changed,
                Err(message) => {
                    drop(inner);
                    session.send(ServerMessage::Error { message });
                    return;
                }
            }
        };
        if changed.selection {
            let selection = selection_message(model, &session.lock());
            session.send(selection);
        }
        if changed.frame {
            refresh(state, session, &snapshot);
        }
    }

    /// Bring every live session up to date with the newest epoch.
    fn refresh_all(&self, state: &AppState) {
        let snapshot = state.writer.snapshot();
        let previous = self
            .model
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .replace(Arc::clone(&snapshot.model));
        if let Some(previous) = previous
            && !Arc::ptr_eq(&previous, &snapshot.model)
        {
            for session in self.sessions() {
                follow(&session, &previous, &snapshot);
            }
        }
        for session in self.sessions() {
            refresh(state, &session, &snapshot);
        }
    }

    /// Sessions with a connection.
    pub(crate) fn live_sessions(&self) -> usize {
        self.sessions()
            .iter()
            .filter(|s| s.lock().out.is_some())
            .count()
    }

    /// Drop a session's connection (it can resume with its id).
    #[allow(clippy::unused_self)]
    pub(crate) fn disconnect(&self, session: &Session) {
        session.lock().out = None;
    }
}

/// The dashboards a client can open.
fn model_message(snapshot: &Snapshot) -> ServerMessage {
    ServerMessage::Model {
        dashboards: snapshot
            .model
            .dashboards
            .iter()
            .map(|d| dashboard_info(&snapshot.model, d))
            .collect(),
        version: snapshot.version,
    }
}

/// The type of a field, if the model has it.
fn field_type(model: &Model, field: &FieldRef) -> Option<tidepool_model::ir::Type> {
    model
        .dataset(field.table)?
        .field(&field.field)
        .map(|f| f.ty)
}

/// Tell a session the model changed (plan §8.1, notify): the dashboards
/// again, selections on fields that are gone or changed type dropped, and
/// its dashboard sent whole, or closed if it was removed.
fn follow(session: &Session, previous: &Model, snapshot: &Snapshot) {
    let model = &snapshot.model;
    session.send(model_message(snapshot));
    let (selection, removed) = {
        let mut inner = session.lock();
        let before = inner.selection();
        for selection in &mut inner.history {
            selection.0.retain(|field, _| {
                let now = field_type(model, field);
                now.is_some() && now == field_type(previous, field)
            });
        }
        inner.sent.clear();
        let removed = inner
            .dashboard
            .take_if(|d| !model.dashboards.iter().any(|x| x.slug == *d));
        (
            (inner.selection() != before).then(|| selection_message(model, &inner)),
            removed,
        )
    };
    if let Some(selection) = selection {
        session.send(selection);
    }
    if let Some(slug) = removed {
        session.send(ServerMessage::Error {
            message: format!("the dashboard `{slug}` was removed by a config change"),
        });
    }
}

/// What a client message changed.
#[derive(Default)]
struct Changed {
    selection: bool,
    frame: bool,
}

/// Apply a client message to a session's state.
#[allow(clippy::too_many_lines)]
fn apply(
    model: &Model,
    inner: &mut SessionState,
    message: ClientMessage,
) -> Result<Changed, String> {
    let selected = |changed: bool| Changed {
        selection: changed,
        frame: changed,
    };
    Ok(match message {
        ClientMessage::Hello { .. } => Changed::default(),
        ClientMessage::OpenDashboard { id } => {
            if !model.dashboards.iter().any(|d| d.slug == id) {
                return Err(format!("no dashboard `{id}`"));
            }
            inner.dashboard = Some(id);
            inner.sent.clear();
            Changed {
                selection: false,
                frame: true,
            }
        }
        ClientMessage::Select {
            field: name,
            values,
            mode,
        } => {
            let (at, ty) = field(model, &name)?;
            let values = values
                .iter()
                .map(|v| scalar(v, ty).ok_or_else(|| format!("`{v}` isn't a value of `{name}`")))
                .collect::<Result<Vec<_>, _>>()?;
            let mut next = inner.selection();
            let current = match next.0.get(&at) {
                Some(Condition::In(values)) => values.clone(),
                _ => Vec::new(),
            };
            let chosen = match mode {
                SelectMode::Replace => values,
                SelectMode::Add => union(current, values),
                SelectMode::Toggle => {
                    let (on, off): (Vec<Scalar>, Vec<Scalar>) =
                        values.into_iter().partition(|v| !contains(&current, v));
                    union(
                        current.into_iter().filter(|c| !contains(&off, c)).collect(),
                        on,
                    )
                }
            };
            if chosen.is_empty() {
                next.0.remove(&at);
            } else {
                next.0.insert(at, Condition::In(chosen));
            }
            let before = inner.selection();
            inner.push(next);
            selected(inner.selection() != before)
        }
        ClientMessage::SelectRange {
            field: name,
            from,
            to,
        } => {
            let (at, ty) = field(model, &name)?;
            let bound = |v: Option<Json>| -> Result<Option<Scalar>, String> {
                v.map(|v| scalar(&v, ty).ok_or_else(|| format!("`{v}` isn't a value of `{name}`")))
                    .transpose()
            };
            let (from, to) = (bound(from)?, bound(to)?);
            let mut next = inner.selection();
            if from.is_none() && to.is_none() {
                next.0.remove(&at);
            } else {
                next.0.insert(at, Condition::Range { from, to });
            }
            let before = inner.selection();
            inner.push(next);
            selected(inner.selection() != before)
        }
        ClientMessage::Clear { field: name } => {
            let mut next = inner.selection();
            match name {
                Some(name) => {
                    next.0.remove(&field(model, &name)?.0);
                }
                None => next.0.clear(),
            }
            let before = inner.selection();
            inner.push(next);
            selected(inner.selection() != before)
        }
        ClientMessage::Back => {
            let moved = inner.at > 0;
            inner.at = inner.at.saturating_sub(1);
            selected(moved)
        }
        ClientMessage::Forward => {
            let moved = inner.at + 1 < inner.history.len();
            if moved {
                inner.at += 1;
            }
            selected(moved)
        }
        ClientMessage::FilterWindow {
            field: name,
            offset,
            limit,
            search,
        } => {
            field(model, &name)?;
            inner.windows.insert(
                name,
                Window {
                    offset,
                    limit: limit.min(1000),
                    search: search.filter(|s| !s.is_empty()),
                },
            );
            Changed {
                selection: false,
                frame: true,
            }
        }
        ClientMessage::Sort {
            visual,
            column,
            desc,
        } => {
            let found = model
                .dashboards
                .iter()
                .flat_map(|d| &d.visuals)
                .find(|v| v.id == visual)
                .ok_or_else(|| format!("no visual `{visual}`"))?;
            let request = request(model, found);
            let columns: Vec<String> = request
                .dimensions
                .iter()
                .map(|d| match &d.grain {
                    Some(grain) => format!("{}:{grain}", d.field),
                    None => d.field.clone(),
                })
                .chain(request.metrics.iter().cloned())
                .collect();
            if !columns.contains(&column) {
                return Err(format!("visual `{visual}` has no column `{column}`"));
            }
            inner.sorts.insert(visual, (column, desc));
            Changed {
                selection: false,
                frame: true,
            }
        }
        ClientMessage::Pause => {
            inner.paused = true;
            Changed::default()
        }
        ClientMessage::Resume => {
            inner.paused = false;
            Changed {
                selection: false,
                frame: true,
            }
        }
    })
}

fn contains(values: &[Scalar], value: &Scalar) -> bool {
    values
        .iter()
        .any(|v| v.compare(value) == Some(std::cmp::Ordering::Equal))
}

fn union(mut values: Vec<Scalar>, more: Vec<Scalar>) -> Vec<Scalar> {
    for v in more {
        if !contains(&values, &v) {
            values.push(v);
        }
    }
    values
}

/// `dataset.field` for a field reference.
fn label(model: &Model, at: &FieldRef) -> String {
    let dataset = model.dataset(at.table).map_or("?", |d| d.name.as_str());
    format!("{dataset}.{}", at.field)
}

fn selection_message(model: &Model, inner: &SessionState) -> ServerMessage {
    ServerMessage::Selection {
        fields: inner
            .selection()
            .0
            .iter()
            .map(|(at, condition)| {
                let field = label(model, at);
                match condition {
                    Condition::In(values) => SelectedField {
                        field,
                        values: Some(values.iter().map(json).collect()),
                        from: None,
                        to: None,
                    },
                    Condition::Range { from, to } => SelectedField {
                        field,
                        values: None,
                        from: from.as_ref().map(json),
                        to: to.as_ref().map(json),
                    },
                }
            })
            .collect(),
        can_back: inner.at > 0,
        can_forward: inner.at + 1 < inner.history.len(),
    }
}

fn dashboard_info(model: &Model, dashboard: &Dashboard) -> DashboardInfo {
    let name = |v: &serde_json::Value| v.as_str().unwrap_or_default().to_string();
    DashboardInfo {
        id: dashboard.slug.clone(),
        name: dashboard.name.clone(),
        columns: dashboard.columns,
        row_height: dashboard.row_height,
        filters: dashboard
            .filters
            .iter()
            .map(|f| FilterInfo {
                field: label(model, &f.field),
                widget: name(&serde_json::to_value(f.widget).unwrap_or_default()),
            })
            .collect(),
        visuals: dashboard
            .visuals
            .iter()
            .map(|v| VisualInfo {
                id: v.id.clone(),
                visual_type: name(&serde_json::to_value(v.visual_type).unwrap_or_default()),
                dimensions: request(model, v).dimensions,
                metrics: request(model, v).metrics,
                compare: v
                    .compare
                    .and_then(|id| model.metric(id))
                    .map(|m| m.name.clone()),
                on_click: v.on_click,
                on_brush: v.on_brush,
                at: GridAt {
                    x: v.at.x,
                    y: v.at.y,
                    w: v.at.w,
                    h: v.at.h,
                },
            })
            .collect(),
    }
}

/// A visual as a one-shot query request.
fn request(model: &Model, visual: &tidepool_model::ir::Visual) -> QueryRequest {
    let metric_name = |id| {
        model
            .metric(id)
            .map_or_else(String::new, |m| m.name.clone())
    };
    let dimensions: Vec<QueryDimension> = visual
        .dimensions
        .iter()
        .map(|d| QueryDimension {
            field: label(model, &d.field),
            grain: d.grain.map(|g| format!("{g:?}").to_lowercase()),
        })
        .collect();
    let sort = visual
        .sort
        .iter()
        .map(|s| QuerySort {
            column: match &s.key {
                SortKey::Metric(id) => metric_name(*id),
                SortKey::Dimension(i) => dimensions.get(*i).map_or_else(String::new, |d| match &d
                    .grain
                {
                    Some(grain) => format!("{}:{grain}", d.field),
                    None => d.field.clone(),
                }),
            },
            desc: s.desc,
        })
        .collect();
    QueryRequest {
        metrics: visual.metrics.iter().map(|id| metric_name(*id)).collect(),
        dimensions,
        filters: Vec::new(),
        sort,
        limit: visual.limit.map(|l| l as usize),
    }
}

fn hash_of(value: &impl serde::Serialize) -> u64 {
    let mut hasher = DefaultHasher::new();
    serde_json::to_string(value)
        .unwrap_or_default()
        .hash(&mut hasher);
    hasher.finish()
}

/// Send a session what changed on its dashboard as of `snapshot`, as one
/// frame. Nothing when paused, without a dashboard, or when nothing changed.
fn refresh(state: &AppState, session: &Session, snapshot: &Arc<Snapshot>) {
    let (dashboard, selection, windows, sorts) = {
        let inner = session.lock();
        if inner.paused || inner.out.is_none() {
            return;
        }
        let Some(dashboard) = inner.dashboard.clone() else {
            return;
        };
        (
            dashboard,
            inner.selection(),
            inner.windows.clone(),
            inner.sorts.clone(),
        )
    };
    let model = &snapshot.model;
    let Some(dashboard) = model.dashboards.iter().find(|d| d.slug == dashboard) else {
        return;
    };
    let visuals: Vec<VisualFrame> = dashboard
        .visuals
        .iter()
        .map(|v| visual_frame(state, snapshot, v, &selection, sorts.get(&v.id)))
        .collect();
    let filters: Vec<FilterFrame> = dashboard
        .filters
        .iter()
        .filter(|f| f.widget == tidepool_config::dashboard::FilterWidget::List)
        .filter_map(|f| {
            let name = label(model, &f.field);
            let window = windows.get(&name).cloned().unwrap_or_default();
            filter_frame(state, snapshot, &f.field, name, &selection, &window).ok()
        })
        .collect();

    // only what changed since this session's last frame
    let mut inner = session.lock();
    let mut changed = |key: String, hash: u64| inner.sent.insert(key, hash) != Some(hash);
    let visuals: Vec<VisualFrame> = visuals
        .into_iter()
        .filter(|v| changed(format!("v:{}", v.id), hash_of(v)))
        .collect();
    let filters: Vec<FilterFrame> = filters
        .into_iter()
        .filter(|f| changed(format!("f:{}", f.field), hash_of(f)))
        .collect();
    drop(inner);
    if visuals.is_empty() && filters.is_empty() {
        return;
    }
    session.send(ServerMessage::Frame {
        epoch: snapshot.epoch,
        visuals,
        filters,
    });
}

fn visual_frame(
    state: &AppState,
    snapshot: &Snapshot,
    visual: &tidepool_model::ir::Visual,
    selection: &Selection,
    sort: Option<&(String, bool)>,
) -> VisualFrame {
    let mut request = request(&snapshot.model, visual);
    if let Some((column, desc)) = sort {
        request.sort = vec![QuerySort {
            column: column.clone(),
            desc: *desc,
        }];
    }
    let run = |request: &QueryRequest| {
        resolve(&snapshot.model, request)
            .map_err(tidepool_engine::QueryError)
            .and_then(|query| {
                // only what can change the answer: sessions that differ
                // elsewhere share it
                let query = Query {
                    selection: selection.clone(),
                    ..query
                };
                let relevant = tidepool_engine::relevant(snapshot, &query)?;
                state.engine.run(
                    snapshot,
                    &Query {
                        selection: relevant,
                        ..query
                    },
                )
            })
    };
    // a KPI's comparison: shown when its metric can be computed
    let compare = visual
        .compare
        .and_then(|id| snapshot.model.metric(id))
        .and_then(|metric| {
            let answer = run(&QueryRequest {
                metrics: vec![metric.name.clone()],
                ..QueryRequest::default()
            })
            .ok()?;
            answer.rows.first()?.first().map(json)
        });
    match run(&request) {
        Ok(answer) => VisualFrame {
            id: visual.id.clone(),
            result: Some(result(&request, &answer, snapshot.epoch)),
            error: None,
            compare,
        },
        Err(error) => VisualFrame {
            id: visual.id.clone(),
            result: None,
            error: Some(error.0),
            compare: None,
        },
    }
}

fn filter_frame(
    state: &AppState,
    snapshot: &Snapshot,
    at: &FieldRef,
    name: String,
    selection: &Selection,
    window: &Window,
) -> Result<FilterFrame, tidepool_engine::QueryError> {
    let all = state.engine.values(snapshot, at, &Selection::default())?;
    // Qlik's rule: a field's own selection doesn't narrow its alternatives
    let mut others = selection.clone();
    let own = others.0.remove(at);
    let possible = state.engine.values(snapshot, at, &others)?;
    let rows: HashMap<String, i64> = possible
        .iter()
        .map(|(v, n)| (format!("{v:?}"), *n))
        .collect();
    let selected: BTreeSet<String> = match &own {
        Some(Condition::In(values)) => values.iter().map(|v| format!("{v:?}")).collect(),
        _ => BTreeSet::new(),
    };
    let in_range = |value: &Scalar| match &own {
        Some(condition @ Condition::Range { .. }) => condition.matches(value),
        _ => false,
    };
    let search = window.search.as_ref().map(|s| s.to_lowercase());
    let values: Vec<FilterValue> = all
        .iter()
        .map(|(value, _)| {
            let key = format!("{value:?}");
            let state = if selected.contains(&key) || in_range(value) {
                ValueState::Selected
            } else if rows.contains_key(&key) {
                ValueState::Possible
            } else {
                ValueState::Excluded
            };
            FilterValue {
                value: json(value),
                state,
                rows: rows.get(&key).copied().unwrap_or(0),
            }
        })
        .filter(|v| {
            search.as_ref().is_none_or(|s| {
                let text = match &v.value {
                    Json::String(text) => text.to_lowercase(),
                    other => other.to_string(),
                };
                text.contains(s.as_str())
            })
        })
        .collect();
    Ok(FilterFrame {
        field: name,
        total: values.len(),
        offset: window.offset,
        values: values
            .into_iter()
            .skip(window.offset)
            .take(window.limit)
            .collect(),
    })
}

/// Start the epoch loop, once per server.
fn start(state: &Arc<AppState>) {
    if state.hub.running.swap(true, Ordering::SeqCst) {
        return;
    }
    let weak = Arc::downgrade(state);
    let mut epochs = state.writer.epochs();
    tokio::spawn(async move {
        let mut status = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                changed = epochs.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    epochs.borrow_and_update();
                    let Some(state) = Weak::upgrade(&weak) else { break };
                    // the newest snapshot is read inside: epochs published
                    // meanwhile are skipped, not queued
                    let _ = tokio::task::spawn_blocking(move || state.hub.refresh_all(&state)).await;
                }
                _ = status.tick() => {
                    let Some(state) = Weak::upgrade(&weak) else { break };
                    let snapshot = state.writer.snapshot();
                    let rows_per_second = snapshot.tables.values().map(|t| t.rows_per_second).sum();
                    for session in state.hub.sessions() {
                        session.send(ServerMessage::Status { epoch: snapshot.epoch, rows_per_second });
                    }
                }
            }
        }
    });
}

/// The live WebSocket. See `Operation::Live` in the endpoint table.
pub async fn socket(State(state): State<Arc<AppState>>, upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(move |socket| serve(state, socket))
}

async fn serve(state: Arc<AppState>, mut socket: WebSocket) {
    start(&state);
    let (out, mut outgoing) = mpsc::channel::<ServerMessage>(BACKLOG);
    let mut session: Option<Arc<Session>> = None;
    loop {
        tokio::select! {
            incoming = socket.recv() => {
                let Some(Ok(message)) = incoming else { break };
                let text = match message {
                    Message::Text(text) => text,
                    Message::Close(_) => break,
                    _ => continue,
                };
                let parsed: Result<ClientMessage, _> = serde_json::from_str(text.as_str());
                let message = match parsed {
                    Ok(message) => message,
                    Err(error) => {
                        let reply = ServerMessage::Error { message: format!("unreadable message: {error}") };
                        if send(&mut socket, &reply).await.is_err() {
                            break;
                        }
                        continue;
                    }
                };
                let (state, out) = (Arc::clone(&state), out.clone());
                let current = session.clone();
                // queries are CPU work: off the async workers
                let joined = tokio::task::spawn_blocking(move || match (current, message) {
                    (None, ClientMessage::Hello { session_id }) => state.hub.connect(&state, session_id, out),
                    (None, message) => {
                        let session = state.hub.connect(&state, None, out);
                        state.hub.handle(&state, &session, message);
                        session
                    }
                    (Some(session), message) => {
                        state.hub.handle(&state, &session, message);
                        session
                    }
                })
                .await;
                match joined {
                    Ok(current) => session = Some(current),
                    Err(_) => break,
                }
            }
            Some(message) = outgoing.recv() => {
                if send(&mut socket, &message).await.is_err() {
                    break;
                }
            }
        }
    }
    if let Some(session) = session {
        state.hub.disconnect(&session);
    }
}

async fn send(socket: &mut WebSocket, message: &ServerMessage) -> Result<(), axum::Error> {
    let text = serde_json::to_string(message).unwrap_or_default();
    socket.send(Message::Text(text.into())).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_selection_change_drops_what_came_after_it() {
        let mut state = SessionState {
            history: vec![Selection::default()],
            ..SessionState::default()
        };
        let one = |n: i64| {
            let mut s = Selection::default();
            s.0.insert(
                FieldRef {
                    table: tidepool_model::ir::TableId(1),
                    field: "x".into(),
                },
                Condition::In(vec![Scalar::Int(n)]),
            );
            s
        };
        state.push(one(1));
        state.push(one(2));
        state.at = 1;
        state.push(one(3));
        assert_eq!(state.history.len(), 3);
        assert_eq!(state.selection(), one(3));
        // the same selection again isn't a step
        state.push(one(3));
        assert_eq!(state.history.len(), 3);
    }
}
