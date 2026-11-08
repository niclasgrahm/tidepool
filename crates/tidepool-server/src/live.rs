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
//!
//! A selection can hold windows relative to now ("the last 7 days", plan
//! §5.5). A session keeps them as windows and turns them into ranges with
//! the server's clock whenever it runs a query, so the engine and its caches
//! only ever see ranges. Once a second the loop checks whether the clock has
//! moved a session's window into a new period, and refreshes it if so, data
//! or not.

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use axum::Extension;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::Value as Json;
use tidepool_config::dashboard::{FilterWidget, VisualType};
use tidepool_engine::{Condition, Query, Selection};
use tidepool_expr::Scalar;
use tidepool_model::ir::{Dashboard, FieldRef, Model, SortKey};
use tidepool_protocol::live::{
    ClientMessage, DashboardInfo, FilterFrame, FilterInfo, GridAt, RelativeUnit, RelativeWindow,
    SelectMode, SelectedField, SelectionChange, ServerMessage, SessionSelection, VisualFrame,
    VisualInfo,
};
use tidepool_protocol::{QueryDimension, QueryRequest, QuerySort};
use tidepool_store::Snapshot;
use tokio::sync::mpsc;

use crate::auth::{ByCookie, Scope};
use crate::filters::{self, Window, token};
use crate::query::{
    DEFAULT_ROWS, Stamp, field, json, resolve, result, rows_result, scalar, window_condition,
};
use crate::{AppState, views};

/// Messages a session hasn't sent yet, at most; past this a slow client is
/// dropped rather than buffered without end.
const BACKLOG: usize = 256;

/// A selection as a session's viewer made it: conditions, and windows
/// relative to now, which become conditions whenever it's used. A field has
/// one or the other.
#[derive(Clone, Debug, Default, PartialEq)]
struct Chosen {
    fixed: Selection,
    windows: BTreeMap<FieldRef, RelativeWindow>,
}

impl Chosen {
    fn set(&mut self, at: FieldRef, condition: Condition) {
        self.windows.remove(&at);
        self.fixed.0.insert(at, condition);
    }

    fn set_window(&mut self, at: FieldRef, window: RelativeWindow) {
        self.fixed.0.remove(&at);
        self.windows.insert(at, window);
    }

    fn remove(&mut self, at: &FieldRef) {
        self.fixed.0.remove(at);
        self.windows.remove(at);
    }

    fn has(&self, at: &FieldRef) -> bool {
        self.fixed.0.contains_key(at) || self.windows.contains_key(at)
    }

    fn retain(&mut self, keep: impl Fn(&FieldRef) -> bool) {
        self.fixed.0.retain(|at, _| keep(at));
        self.windows.retain(|at, _| keep(at));
    }

    /// The condition each window stands for at `now`; a window that can't
    /// be one (its field gone, or no longer a time) is left out.
    fn ranges(&self, model: &Model, now: i64) -> Vec<(FieldRef, RelativeWindow, Condition)> {
        self.windows
            .iter()
            .filter_map(|(at, window)| {
                let ty = field_type(model, at)?;
                let condition = window_condition(&at.field, *window, ty, now).ok()?;
                Some((at.clone(), *window, condition))
            })
            .collect()
    }

    /// The selection as of `now`: windows as ranges.
    fn resolve(&self, model: &Model, now: i64) -> Selection {
        let mut selection = self.fixed.clone();
        for (at, _, condition) in self.ranges(model, now) {
            selection.0.insert(at, condition);
        }
        selection
    }
}

#[derive(Default)]
struct SessionState {
    /// Every selection so far; `at` is the current one.
    history: Vec<Chosen>,
    at: usize,
    dashboard: Option<String>,
    /// Dashboards this session has opened: a filter's default window
    /// applies the first time only, so a viewer who cleared it keeps that.
    opened: HashSet<String>,
    /// The selection the last frame was computed under, windows resolved:
    /// when the clock moves a window on, it no longer matches.
    shown: Option<Selection>,
    paused: bool,
    windows: HashMap<String, Window>,
    /// Tree filters' open nodes, by field (keys as [`token`]s).
    open: HashMap<String, BTreeSet<String>>,
    /// Sort overrides by visual: (result column, descending).
    sorts: HashMap<String, (String, bool)>,
    /// What was last sent per visual or filter (a hash of it).
    sent: HashMap<String, u64>,
    out: Option<mpsc::Sender<ServerMessage>>,
    /// The views it may open, when it came with an embed token; `None`:
    /// anything.
    allowed: Option<Arc<HashSet<String>>>,
    /// When its connection went: it can be resumed for [`RESUMABLE`].
    gone: Option<std::time::Instant>,
}

/// How long a session without a connection can be resumed.
const RESUMABLE: Duration = Duration::from_hours(1);

/// A session id nobody can guess: `s` and 128 random bits (a counter, on
/// an operating system without a random source).
fn session_id(fallback: u64) -> String {
    let mut bytes = [0u8; 16];
    if getrandom::fill(&mut bytes).is_err() {
        return format!("s{fallback}");
    }
    bytes.iter().fold(String::from("s"), |mut id, b| {
        let _ = std::fmt::Write::write_fmt(&mut id, format_args!("{b:02x}"));
        id
    })
}

impl SessionState {
    fn selection(&self) -> Chosen {
        self.history.get(self.at).cloned().unwrap_or_default()
    }

    /// Make `next` the current selection, dropping anything after it.
    fn push(&mut self, next: Chosen) {
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
        scope: Option<&Scope>,
    ) -> Arc<Session> {
        let allowed = scope.map(|s| Arc::clone(&s.0));
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        sessions.retain(|_, s| s.lock().gone.is_none_or(|g| g.elapsed() < RESUMABLE));
        // resumed only by a connection allowed what it was
        let known = id
            .and_then(|id| sessions.get(&id).cloned())
            .filter(|s| s.lock().allowed == allowed);
        let session = known.unwrap_or_else(|| {
            let id = session_id(self.next.fetch_add(1, Ordering::Relaxed) + 1);
            let session = Arc::new(Session {
                id: id.clone(),
                state: Mutex::new(SessionState {
                    history: vec![Chosen::default()],
                    allowed,
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
            inner.gone = None;
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
        let selection = selection_message(&snapshot.model, &session.lock(), state.clock.now());
        session.send(selection);
        refresh(state, &session, &snapshot);
        session
    }

    /// A message from a session's client.
    #[allow(clippy::unused_self)] // a hub's operation, like the others
    pub(crate) fn handle(&self, state: &AppState, session: &Session, message: ClientMessage) {
        let snapshot = state.writer.snapshot();
        let model = &snapshot.model;
        let now = state.clock.now();
        // a view opened: what it shows, which `Model` doesn't say
        let view = match &message {
            ClientMessage::OpenDashboard { id } if views::is_view(id) => {
                match state.views.get(state, &snapshot, id) {
                    Some(Ok(dashboard)) => Some(dashboard),
                    Some(Err(message)) => {
                        session.send(ServerMessage::Error { message });
                        return;
                    }
                    None => {
                        session.send(ServerMessage::Error {
                            message: format!("no view `{id}`: it was never made, or forgotten"),
                        });
                        return;
                    }
                }
            }
            _ => None,
        };
        let lookup = |slug: &str| -> Option<Arc<Dashboard>> {
            if views::is_view(slug) {
                state.views.get(state, &snapshot, slug)?.ok()
            } else {
                model
                    .dashboards
                    .iter()
                    .find(|d| d.slug == slug)
                    .map(|d| Arc::new(d.clone()))
            }
        };
        let changed = {
            let mut inner = session.lock();
            match apply(model, &lookup, &mut inner, message, now) {
                Ok(changed) => changed,
                Err(message) => {
                    drop(inner);
                    session.send(ServerMessage::Error { message });
                    return;
                }
            }
        };
        if let Some(view) = view {
            session.send(ServerMessage::View {
                dashboard: dashboard_info(model, &view),
            });
        }
        if changed.selection {
            let selection = selection_message(model, &session.lock(), now);
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
            let now = state.clock.now();
            for session in self.sessions() {
                follow(state, &session, &previous, &snapshot, now);
            }
        }
        for session in self.sessions() {
            refresh(state, &session, &snapshot);
        }
    }

    /// Refresh the sessions whose windows the clock has moved into a new
    /// period since their last frame, and tell them where the windows are.
    fn roll(&self, state: &AppState) {
        let snapshot = state.writer.snapshot();
        let now = state.clock.now();
        for session in self.sessions() {
            let moved = {
                let inner = session.lock();
                let chosen = inner.selection();
                (!chosen.windows.is_empty()
                    && !inner.paused
                    && inner.out.is_some()
                    && inner
                        .shown
                        .as_ref()
                        .is_some_and(|shown| *shown != chosen.resolve(&snapshot.model, now)))
                .then(|| selection_message(&snapshot.model, &inner, now))
            };
            if let Some(selection) = moved {
                session.send(selection);
                refresh(state, &session, &snapshot);
            }
        }
    }

    fn session(&self, id: &str) -> Option<Arc<Session>> {
        self.sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .cloned()
    }

    /// Session `id`'s selection, as of now.
    pub(crate) fn selection_of(&self, state: &AppState, id: &str) -> Option<SessionSelection> {
        let session = self.session(id)?;
        let snapshot = state.writer.snapshot();
        let inner = session.lock();
        Some(session_selection(
            &session.id,
            &inner,
            selection_message(&snapshot.model, &inner, state.clock.now()),
        ))
    }

    /// Replace session `id`'s selection with `change`, as one step of its
    /// history, and refresh it. `Ok(None)`: no such session.
    pub(crate) fn set_selection(
        &self,
        state: &AppState,
        id: &str,
        change: &SelectionChange,
    ) -> Result<Option<SessionSelection>, String> {
        let Some(session) = self.session(id) else {
            return Ok(None);
        };
        let snapshot = state.writer.snapshot();
        let model = &snapshot.model;
        let now = state.clock.now();
        let mut next = Chosen::default();
        for filter in &change.fields {
            let (at, ty) = field(model, &filter.field)?;
            if next.has(&at) {
                return Err(format!("`{}` is in the selection twice", filter.field));
            }
            match filter.relative {
                Some(window)
                    if filter.values.is_none()
                        && filter.from.is_none()
                        && filter.to.is_none()
                        && filter.contains.is_none() =>
                {
                    // checked now, kept as a window that moves with the clock
                    window_condition(&filter.field, window, ty, now)?;
                    next.set_window(at, window);
                }
                _ => next.set(at, crate::query::condition(filter, ty, now)?),
            }
        }
        let message = {
            let mut inner = session.lock();
            inner.push(next);
            selection_message(model, &inner, now)
        };
        session.send(message);
        refresh(state, &session, &snapshot);
        Ok(self.selection_of(state, id))
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
        let mut inner = session.lock();
        inner.out = None;
        inner.gone = Some(std::time::Instant::now());
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
fn follow(state: &AppState, session: &Session, previous: &Model, snapshot: &Snapshot, now: i64) {
    let model = &snapshot.model;
    session.send(model_message(snapshot));
    let (selection, removed) = {
        let mut inner = session.lock();
        let before = inner.selection();
        for selection in &mut inner.history {
            selection.retain(|field| {
                let now = field_type(model, field);
                now.is_some() && now == field_type(previous, field)
            });
        }
        inner.sent.clear();
        inner.open.retain(|name, _| {
            field(model, name).is_ok_and(|(at, _)| field_type(previous, &at).is_some())
        });
        // a view stays open: it is compiled again for the new model
        let removed = inner
            .dashboard
            .take_if(|d| !views::is_view(d) && !model.dashboards.iter().any(|x| x.slug == *d));
        let view = inner.dashboard.clone().filter(|d| views::is_view(d));
        (
            (inner.selection() != before).then(|| selection_message(model, &inner, now)),
            (removed, view),
        )
    };
    let (removed, view) = removed;
    if let Some(selection) = selection {
        session.send(selection);
    }
    if let Some(id) = view
        && let Some(Ok(dashboard)) = state.views.get(state, snapshot, &id)
    {
        session.send(ServerMessage::View {
            dashboard: dashboard_info(model, &dashboard),
        });
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
    lookup: &dyn Fn(&str) -> Option<Arc<Dashboard>>,
    inner: &mut SessionState,
    message: ClientMessage,
    now: i64,
) -> Result<Changed, String> {
    let selected = |changed: bool| Changed {
        selection: changed,
        frame: changed,
    };
    Ok(match message {
        ClientMessage::Hello { .. } => Changed::default(),
        ClientMessage::OpenDashboard { id } => {
            if inner.allowed.as_ref().is_some_and(|a| !a.contains(&id)) {
                return Err(format!(
                    "this connection opens only the views it was given, not `{id}`"
                ));
            }
            let Some(dashboard) = lookup(&id) else {
                return Err(format!("no dashboard `{id}`"));
            };
            // the first time, its filters' default windows, unless the
            // viewer chose something for those fields already: part of the
            // selection it opens with, not a step to go back from
            let mut defaulted = false;
            if inner.opened.insert(id.clone()) {
                let mut current = inner.selection();
                for filter in &dashboard.filters {
                    if let Some(window) = filter.default
                        && !current.has(&filter.field)
                    {
                        current.set_window(filter.field.clone(), window_of(window));
                        defaulted = true;
                    }
                }
                if defaulted {
                    let at = inner.at;
                    inner.history[at] = current;
                }
            }
            inner.dashboard = Some(id);
            inner.sent.clear();
            Changed {
                selection: defaulted,
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
            let current = match next.fixed.0.get(&at) {
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
                next.remove(&at);
            } else {
                next.set(at, Condition::In(chosen));
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
                next.remove(&at);
            } else {
                next.set(at, Condition::Range { from, to });
            }
            let before = inner.selection();
            inner.push(next);
            selected(inner.selection() != before)
        }
        ClientMessage::SelectRelative {
            field: name,
            window,
        } => {
            let (at, ty) = field(model, &name)?;
            // refused now rather than left out of every query later
            window_condition(&name, window, ty, now)?;
            let mut next = inner.selection();
            next.set_window(at, window);
            let before = inner.selection();
            inner.push(next);
            selected(inner.selection() != before)
        }
        ClientMessage::Search { field: name, text } => {
            let (at, ty) = field(model, &name)?;
            let mut next = inner.selection();
            // a search being typed is one step back, not one per key
            let typing = matches!(next.fixed.0.get(&at), Some(Condition::Contains(_)));
            if text.is_empty() {
                next.remove(&at);
            } else {
                next.set(at, crate::query::contains(&name, &text, ty)?);
            }
            let before = inner.selection();
            if typing && !text.is_empty() {
                let at = inner.at;
                inner.history.truncate(at + 1);
                inner.history[at] = next;
            } else {
                inner.push(next);
            }
            selected(inner.selection() != before)
        }
        ClientMessage::Clear { field: name } => {
            let mut next = inner.selection();
            match name {
                Some(name) => {
                    next.remove(&field(model, &name)?.0);
                }
                None => next = Chosen::default(),
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
        ClientMessage::FilterOpen {
            field: name,
            keys,
            open,
        } => {
            let (at, ty) = field(model, &name)?;
            if tidepool_engine::hierarchy::columns(model, &at).is_none() {
                return Err(format!(
                    "`{name}` isn't a hierarchy, so it has no nodes to open"
                ));
            }
            let keys = keys
                .iter()
                .map(|v| scalar(v, ty).ok_or_else(|| format!("`{v}` isn't a key of `{name}`")))
                .collect::<Result<Vec<_>, _>>()?;
            let nodes = inner.open.entry(name).or_default();
            for key in &keys {
                if open {
                    nodes.insert(token(key));
                } else {
                    nodes.remove(&token(key));
                }
            }
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
            // a visual of the dashboard that is open, not of any other
            let open = inner.dashboard.as_deref().and_then(lookup);
            let found = open
                .as_ref()
                .and_then(|d| d.visuals.iter().find(|v| v.id == visual))
                .ok_or_else(|| format!("no visual `{visual}` on the open dashboard"))?;
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

fn selection_message(model: &Model, inner: &SessionState, now: i64) -> ServerMessage {
    let chosen = inner.selection();
    let mut fields: Vec<(FieldRef, SelectedField)> = chosen
        .fixed
        .0
        .iter()
        .map(|(at, condition)| {
            let field = label(model, at);
            let selected = match condition {
                Condition::In(values) => SelectedField {
                    field,
                    values: Some(values.iter().map(json).collect()),
                    from: None,
                    to: None,
                    relative: None,
                    contains: None,
                },
                Condition::Range { from, to } => SelectedField {
                    field,
                    values: None,
                    from: from.as_ref().map(json),
                    to: to.as_ref().map(json),
                    relative: None,
                    contains: None,
                },
                Condition::Set(set) => SelectedField {
                    field,
                    values: Some(set.values().iter().map(json).collect()),
                    from: None,
                    to: None,
                    relative: None,
                    contains: None,
                },
                Condition::Contains(needle) => SelectedField {
                    field,
                    values: None,
                    from: None,
                    to: None,
                    relative: None,
                    contains: Some(needle.text().to_string()),
                },
            };
            (at.clone(), selected)
        })
        .collect();
    // windows with where they are now
    fields.extend(
        chosen
            .ranges(model, now)
            .into_iter()
            .map(|(at, window, condition)| {
                let (from, to) = match condition {
                    Condition::Range { from, to } => (from, to),
                    _ => (None, None),
                };
                let selected = SelectedField {
                    field: label(model, &at),
                    values: None,
                    from: from.as_ref().map(json),
                    to: to.as_ref().map(json),
                    relative: Some(window),
                    contains: None,
                };
                (at, selected)
            }),
    );
    fields.sort_by(|a, b| a.0.cmp(&b.0));
    ServerMessage::Selection {
        fields: fields.into_iter().map(|(_, f)| f).collect(),
        can_back: inner.at > 0,
        can_forward: inner.at + 1 < inner.history.len(),
    }
}

/// A session's selection as the API shows it, from its selection message.
fn session_selection(id: &str, inner: &SessionState, message: ServerMessage) -> SessionSelection {
    let (fields, can_back, can_forward) = match message {
        ServerMessage::Selection {
            fields,
            can_back,
            can_forward,
        } => (fields, can_back, can_forward),
        _ => (Vec::new(), false, false),
    };
    SessionSelection {
        session: id.to_string(),
        dashboard: inner.dashboard.clone(),
        fields,
        can_back,
        can_forward,
    }
}

/// `GET /v1/sessions/{id}/selection`. See `Operation::GetSelection`.
pub async fn get_selection(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    match state.hub.selection_of(&state, &id) {
        Some(selection) => axum::Json(selection).into_response(),
        None => crate::handlers::api_error(StatusCode::NOT_FOUND, format!("no session `{id}`")),
    }
}

/// `PUT /v1/sessions/{id}/selection`. See `Operation::SetSelection`.
pub async fn set_selection(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::Json(change): axum::Json<SelectionChange>,
) -> Response {
    let worker = Arc::clone(&state);
    let lookup = id.clone();
    let done =
        tokio::task::spawn_blocking(move || worker.hub.set_selection(&worker, &lookup, &change))
            .await;
    match done {
        Ok(Ok(Some(selection))) => axum::Json(selection).into_response(),
        Ok(Ok(None)) => {
            crate::handlers::api_error(StatusCode::NOT_FOUND, format!("no session `{id}`"))
        }
        Ok(Err(message)) => crate::handlers::api_error(StatusCode::BAD_REQUEST, message),
        Err(_) => crate::handlers::api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the selection couldn't be set",
        ),
    }
}

/// A config file's window, as the protocol has it.
fn window_of(window: tidepool_config::dashboard::RelativeWindow) -> RelativeWindow {
    use tidepool_config::dashboard::WindowUnit;
    RelativeWindow {
        unit: match window.unit {
            WindowUnit::Minute => RelativeUnit::Minute,
            WindowUnit::Hour => RelativeUnit::Hour,
            WindowUnit::Day => RelativeUnit::Day,
            WindowUnit::Week => RelativeUnit::Week,
            WindowUnit::Month => RelativeUnit::Month,
            WindowUnit::Quarter => RelativeUnit::Quarter,
            WindowUnit::Year => RelativeUnit::Year,
        },
        last: window.last,
        ago: window.ago,
    }
}

pub(crate) fn dashboard_info(model: &Model, dashboard: &Dashboard) -> DashboardInfo {
    let name = |v: &serde_json::Value| v.as_str().unwrap_or_default().to_string();
    let lists = dashboard.lists(model);
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
                timed: matches!(
                    field_type(model, &f.field),
                    Some(tidepool_expr::Type::Timestamp | tidepool_expr::Type::TimestampTz)
                ),
                default: f.default.map(window_of),
                measure: f
                    .measure
                    .and_then(|id| model.metric(id))
                    .map(|m| m.name.clone()),
                measure_format: f
                    .measure
                    .and_then(|id| model.metric(id))
                    .and_then(|m| m.format.clone()),
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
                widget: (v.visual_type == VisualType::Filter)
                    .then(|| {
                        lists
                            .iter()
                            .find(|f| Some(&f.field) == v.dimensions.first().map(|d| &d.field))
                            .map(|f| name(&serde_json::to_value(f.widget).unwrap_or_default()))
                    })
                    .flatten(),
                formats: formats(model, v),
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
/// How each of a visual's metrics reads: its format there, else the
/// metric's own.
fn formats(
    model: &Model,
    visual: &tidepool_model::ir::Visual,
) -> std::collections::BTreeMap<String, String> {
    visual
        .metrics
        .iter()
        .chain(visual.compare.iter())
        .filter_map(|id| {
            let metric = model.metric(*id)?;
            let own = visual.formats.iter().find(|(m, _)| m == id).map(|(_, f)| f);
            let format = own.or(metric.format.as_ref())?;
            Some((metric.name.clone(), format.clone()))
        })
        .collect()
}

pub(crate) fn request(model: &Model, visual: &tidepool_model::ir::Visual) -> QueryRequest {
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
            level: None,
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
    let (dashboard, selection, windows, open, sorts) = {
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
            inner.open.clone(),
            inner.sorts.clone(),
        )
    };
    let model = &snapshot.model;
    let selection = selection.resolve(model, state.clock.now());
    let view;
    let dashboard: &Dashboard = if views::is_view(&dashboard) {
        match state.views.get(state, snapshot, &dashboard) {
            Some(Ok(found)) => {
                view = found;
                &view
            }
            Some(Err(message)) => {
                // once per problem, not once per epoch
                let hash = hash_of(&message);
                let first = session.lock().sent.insert("view".into(), hash) != Some(hash);
                if first {
                    session.send(ServerMessage::Error { message });
                }
                return;
            }
            None => return,
        }
    } else {
        let Some(found) = model.dashboards.iter().find(|d| d.slug == dashboard) else {
            return;
        };
        found
    };
    // a filter visual is shown from its list's frames
    let visuals: Vec<VisualFrame> = dashboard
        .visuals
        .iter()
        .filter(|v| v.visual_type != VisualType::Filter)
        .map(|v| visual_frame(state, snapshot, v, &selection, sorts.get(&v.id)))
        .collect();
    let filters: Vec<FilterFrame> = dashboard
        .lists(model)
        .iter()
        .filter_map(|f| {
            let name = label(model, &f.field);
            let window = windows.get(&name).cloned().unwrap_or_default();
            match f.widget {
                FilterWidget::List => {
                    filters::list(state, snapshot, f, name, &selection, &window).ok()
                }
                FilterWidget::Tree => {
                    let open = open.get(&name).cloned().unwrap_or_default();
                    filters::tree(state, snapshot, f, name, &selection, &window, &open).ok()
                }
                FilterWidget::DateRange | FilterWidget::Search => None,
            }
        })
        .collect();

    // only what changed since this session's last frame
    let mut inner = session.lock();
    inner.shown = Some(selection);
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

/// A `rows` visual's frame: the newest rows under the part of the
/// selection that can change them.
fn rows_frame(
    state: &AppState,
    snapshot: &Snapshot,
    visual: &tidepool_model::ir::Visual,
    selection: &Selection,
) -> VisualFrame {
    let fields: Vec<String> = visual
        .dimensions
        .iter()
        .map(|d| label(&snapshot.model, &d.field))
        .collect();
    let query = tidepool_engine::RowsQuery {
        fields: visual.dimensions.iter().map(|d| d.field.clone()).collect(),
        selection: selection.clone(),
        limit: visual.limit.map_or(DEFAULT_ROWS, |l| l as usize),
    };
    let answer = tidepool_engine::rows_relevant(snapshot, &query).and_then(|relevant| {
        state.engine.rows(
            snapshot,
            &tidepool_engine::RowsQuery {
                selection: relevant,
                ..query
            },
        )
    });
    match answer {
        Ok(answer) => VisualFrame {
            id: visual.id.clone(),
            result: Some(rows_result(&fields, &answer, Stamp::of(snapshot))),
            error: None,
            compare: None,
        },
        Err(error) => VisualFrame {
            id: visual.id.clone(),
            result: None,
            error: Some(error.0),
            compare: None,
        },
    }
}

fn visual_frame(
    state: &AppState,
    snapshot: &Snapshot,
    visual: &tidepool_model::ir::Visual,
    selection: &Selection,
    sort: Option<&(String, bool)>,
) -> VisualFrame {
    if visual.visual_type == VisualType::Rows {
        return rows_frame(state, snapshot, visual, selection);
    }
    let mut request = request(&snapshot.model, visual);
    if let Some((column, desc)) = sort {
        request.sort = vec![QuerySort {
            column: column.clone(),
            desc: *desc,
        }];
    }
    let run = |request: &QueryRequest| {
        resolve(&snapshot.model, request, state.clock.now())
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
            result: Some(result(&request, &answer, Stamp::of(snapshot))),
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
                    let _ = tokio::task::spawn_blocking(move || {
                        let started = std::time::Instant::now();
                        state.hub.refresh_all(&state);
                        state.counters.refresh.record(started.elapsed());
                    }).await;
                }
                _ = status.tick() => {
                    let Some(state) = Weak::upgrade(&weak) else { break };
                    let snapshot = state.writer.snapshot();
                    let rows_per_second = snapshot.tables.values().map(|t| t.rows_per_second).sum();
                    for session in state.hub.sessions() {
                        session.send(ServerMessage::Status { epoch: snapshot.epoch, rows_per_second });
                    }
                    // windows relative to now move with the clock, not epochs
                    let _ = tokio::task::spawn_blocking(move || state.hub.roll(&state)).await;
                }
            }
        }
    });
}

/// The live WebSocket. See `Operation::Live` in the endpoint table.
pub async fn socket(
    State(state): State<Arc<AppState>>,
    scope: Option<Extension<Scope>>,
    by_cookie: Option<Extension<ByCookie>>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    // a cookie rides along from any site's page: only this server's own
    // pages may use it for the socket (a token says who sent it)
    if by_cookie.is_some() && !same_origin(&headers) {
        return crate::handlers::api_error(
            StatusCode::FORBIDDEN,
            "a session cookie opens the live socket from this server's pages only",
        );
    }
    let scope = scope.map(|Extension(s)| s);
    upgrade.on_upgrade(move |socket| serve(state, socket, scope))
}

/// Whether a request's `Origin` (if it has one) is the host it was sent to.
fn same_origin(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return true;
    };
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    origin
        .split_once("://")
        .is_some_and(|(_, rest)| !host.is_empty() && rest == host)
}

async fn serve(state: Arc<AppState>, mut socket: WebSocket, scope: Option<Scope>) {
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
                        let _ = out.try_send(ServerMessage::Done);
                        continue;
                    }
                };
                let (state, queue) = (Arc::clone(&state), out.clone());
                let current = session.clone();
                let scope = scope.clone();
                // queries are CPU work: off the async workers
                let joined = tokio::task::spawn_blocking(move || match (current, message) {
                    (None, ClientMessage::Hello { session_id }) => state.hub.connect(&state, session_id, queue, scope.as_ref()),
                    (None, message) => {
                        let session = state.hub.connect(&state, None, queue, scope.as_ref());
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
                // behind what the message sent: `out` is the queue its
                // selection and frame went into (full: the session is gone)
                let _ = out.try_send(ServerMessage::Done);
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
            history: vec![Chosen::default()],
            ..SessionState::default()
        };
        let one = |n: i64| {
            let mut s = Chosen::default();
            s.set(
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

    #[test]
    fn a_field_holds_a_window_or_a_condition_not_both() {
        let at = FieldRef {
            table: tidepool_model::ir::TableId(1),
            field: "t".into(),
        };
        let week = RelativeWindow {
            unit: RelativeUnit::Day,
            last: 7,
            ago: 0,
        };
        let mut chosen = Chosen::default();
        chosen.set(at.clone(), Condition::In(vec![Scalar::Int(1)]));
        chosen.set_window(at.clone(), week);
        assert!(chosen.fixed.0.is_empty(), "the window replaced the values");
        chosen.set(
            at.clone(),
            Condition::Range {
                from: None,
                to: None,
            },
        );
        assert!(chosen.windows.is_empty(), "the range replaced the window");
        chosen.set_window(at.clone(), week);
        chosen.remove(&at);
        assert!(!chosen.has(&at));
    }
}
