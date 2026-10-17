//! The live connection: one WebSocket to `/v1/ws`, and the signals its
//! messages update. Shared by every page through Leptos context.

use std::collections::HashMap;

use leptos::prelude::*;
use serde_json::{Value, json};
use tidepool_protocol::live::{DashboardInfo, SelectedField, ServerMessage};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::Closure;

use crate::board::{Board, InFlight, socket_url};

/// Milliseconds on the page's clock.
pub fn now() -> f64 {
    web_sys::window()
        .and_then(|w| w.performance())
        .map_or(0.0, |p| p.now())
}

#[derive(Clone, Copy)]
pub struct Live {
    pub dashboards: RwSignal<Vec<DashboardInfo>>,
    /// The config version the server runs: pages showing the model reload
    /// when it changes.
    pub model_version: RwSignal<u64>,
    /// The dashboard the server sends frames for.
    pub open: RwSignal<Option<String>>,
    pub board: RwSignal<Board>,
    pub selection: RwSignal<Vec<SelectedField>>,
    /// Whether there's a selection to go back to, and forward to.
    pub history: RwSignal<(bool, bool)>,
    pub rate: RwSignal<f64>,
    /// The newest epoch the server has published (from status messages).
    pub server_epoch: RwSignal<u64>,
    pub connected: RwSignal<bool>,
    pub paused: RwSignal<bool>,
    /// When the last frame arrived, on [`now`]'s clock.
    pub updated: RwSignal<f64>,
    /// Ticks four times a second, for "updated … ago".
    pub clock: RwSignal<f64>,
    pub problem: RwSignal<Option<String>>,
    /// What's been sent and not answered: [`Live::busy`] while a selection
    /// is.
    pub in_flight: RwSignal<InFlight>,
    /// Each table's sort, by visual id: (column, descending). Kept here
    /// because a table is rebuilt with every frame.
    pub sorts: RwSignal<HashMap<String, (String, bool)>>,
    socket: StoredValue<Option<web_sys::WebSocket>, LocalStorage>,
}

impl Live {
    pub fn new() -> Self {
        Self {
            dashboards: RwSignal::new(Vec::new()),
            model_version: RwSignal::new(0),
            open: RwSignal::new(None),
            board: RwSignal::new(Board::default()),
            selection: RwSignal::new(Vec::new()),
            history: RwSignal::new((false, false)),
            rate: RwSignal::new(0.0),
            server_epoch: RwSignal::new(0),
            connected: RwSignal::new(false),
            paused: RwSignal::new(false),
            updated: RwSignal::new(0.0),
            clock: RwSignal::new(0.0),
            problem: RwSignal::new(None),
            in_flight: RwSignal::new(InFlight::default()),
            sorts: RwSignal::new(HashMap::new()),
            socket: StoredValue::new_local(None),
        }
    }

    pub fn send(self, message: &Value) {
        let sent = self.socket.with_value(|socket| {
            socket
                .as_ref()
                .is_some_and(|s| s.send_with_str(&message.to_string()).is_ok())
        });
        if sent {
            let kind = message["type"].as_str().unwrap_or_default();
            self.in_flight.update(|f| f.sent(kind));
        }
    }

    /// A selection is under way: the dashboard is about to change.
    pub fn busy(self) -> bool {
        self.in_flight.with(InFlight::busy)
    }

    /// Show `slug` (frames for it from now on).
    pub fn open_dashboard(self, slug: &str) {
        if self.open.get_untracked().as_deref() == Some(slug) {
            return;
        }
        self.open.set(Some(slug.to_string()));
        self.board.set(Board::default());
        self.sorts.set(HashMap::new());
        self.send(&json!({"type": "open_dashboard", "id": slug}));
    }

    /// Select `values` of `field`: replacing what's selected, or toggling
    /// them (Ctrl/Cmd-click).
    pub fn select(self, field: &str, values: &[Value], toggle: bool) {
        self.send(&json!({
            "type": "select", "field": field, "values": values,
            "mode": if toggle { "toggle" } else { "replace" },
        }));
    }

    pub fn toggle_pause(self) {
        let paused = !self.paused.get_untracked();
        self.paused.set(paused);
        self.send(&json!({"type": if paused { "pause" } else { "resume" }}));
    }

    fn receive(self, message: ServerMessage) {
        match message {
            ServerMessage::Welcome { epoch, .. } => {
                self.connected.set(true);
                self.server_epoch.set(epoch);
                // a reconnect: show again what was open
                if let Some(slug) = self.open.get_untracked() {
                    self.send(&json!({"type": "open_dashboard", "id": slug}));
                }
            }
            ServerMessage::Model {
                dashboards,
                version,
            } => {
                self.dashboards.set(dashboards);
                if self.model_version.get_untracked() != version {
                    self.model_version.set(version);
                }
            }
            ServerMessage::Selection {
                fields,
                can_back,
                can_forward,
            } => {
                self.selection.set(fields);
                self.history.set((can_back, can_forward));
            }
            message @ ServerMessage::Frame { .. } => {
                self.board.update(|board| {
                    board.apply(&message);
                });
                self.updated.set(now());
            }
            ServerMessage::Status {
                epoch,
                rows_per_second,
            } => {
                self.server_epoch.set(epoch);
                self.rate.set(rows_per_second);
            }
            ServerMessage::Error { message } => self.problem.set(Some(message)),
            ServerMessage::Done => self.in_flight.update(InFlight::done),
        }
    }

    /// Open the socket to the server this page came from, and start the
    /// clock.
    pub fn connect(self) {
        let Some(window) = web_sys::window() else {
            return;
        };
        let tick = Closure::<dyn Fn()>::new(move || self.clock.set(now()));
        let _ = window.set_interval_with_callback_and_timeout_and_arguments_0(
            tick.as_ref().unchecked_ref(),
            250,
        );
        tick.forget(); // lives as long as the page
        let location = window.location();
        let url = socket_url(
            &location.protocol().unwrap_or_default(),
            &location.host().unwrap_or_default(),
            &location.search().unwrap_or_default(),
        );
        let Ok(socket) = web_sys::WebSocket::new(&url) else {
            self.problem.set(Some(format!("Can't open {url}.")));
            return;
        };
        let on_open = Closure::<dyn Fn()>::new(move || self.send(&json!({"type": "hello"})));
        socket.set_onopen(Some(on_open.as_ref().unchecked_ref()));
        on_open.forget();
        let on_message =
            Closure::<dyn Fn(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
                let Some(text) = event.data().as_string() else {
                    return;
                };
                match serde_json::from_str::<ServerMessage>(&text) {
                    Ok(message) => self.receive(message),
                    Err(err) => self.problem.set(Some(format!("Unreadable message: {err}"))),
                }
            });
        socket.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        on_message.forget();
        let on_close = Closure::<dyn Fn()>::new(move || {
            self.connected.set(false);
            self.in_flight.set(InFlight::default());
            self.problem.set(Some(
                "Disconnected from the server. Reload to reconnect.".into(),
            ));
        });
        socket.set_onclose(Some(on_close.as_ref().unchecked_ref()));
        on_close.forget();
        self.socket.set_value(Some(socket));
    }
}
