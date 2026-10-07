//! The Tidepool HTTP server: the API built from the endpoint table in
//! `tidepool-protocol`, the embedded UI, and the process lifecycle.
//!
//! A library rather than code in the binary so integration tests can drive
//! [`router`] in process through `tower::ServiceExt::oneshot`, with no socket.

pub mod auth;
mod endpoints;
mod handlers;
mod idempotency;
pub mod openapi;
mod query;
mod tables;
pub mod ui;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Context;
use axum::Router;

pub use auth::Tokens;
pub use endpoints::api_router;
pub use ui::{Assets, DirAssets};

use tidepool_model::Model;
use tidepool_store::{WriterConfig, WriterHandle};

/// Everything a handler can reach.
pub struct AppState {
    ready: AtomicBool,
    ui: Option<Arc<dyn Assets>>,
    pub(crate) writer: WriterHandle,
    pub(crate) tokens: Tokens,
    pub(crate) keys: idempotency::Keys,
    /// The query engine and its caches, shared by every query.
    pub(crate) engine: Arc<tidepool_engine::Engine>,
}

impl AppState {
    /// A server for `model`, not ready yet, serving the given UI if any. Starts
    /// the writer thread, which stops when the state is dropped.
    ///
    /// # Errors
    /// The writer thread can't be started.
    pub fn new(
        model: Arc<Model>,
        writer: WriterConfig,
        ui: Option<Arc<dyn Assets>>,
        tokens: Tokens,
    ) -> std::io::Result<Self> {
        let (writer, _thread) = tidepool_store::spawn(model, writer)?;
        Ok(Self::with_writer(writer, ui, tokens))
    }

    /// A server around an existing writer handle (tests use one whose queue is
    /// never drained, to see what happens when the writer is busy).
    #[must_use]
    pub fn with_writer(writer: WriterHandle, ui: Option<Arc<dyn Assets>>, tokens: Tokens) -> Self {
        Self {
            ready: AtomicBool::new(false),
            ui,
            writer,
            tokens,
            keys: idempotency::Keys::default(),
            engine: Arc::new(tidepool_engine::Engine::default()),
        }
    }

    /// The writer: for tests and tools that need to publish or read snapshots.
    #[must_use]
    pub fn writer(&self) -> &WriterHandle {
        &self.writer
    }

    /// Whether `GET /readyz` answers 200.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    /// Mark the server ready to take traffic. Called once startup work is done:
    /// today that is binding the listener; later it is config apply and WAL
    /// recovery as well.
    pub fn set_ready(&self) {
        self.ready.store(true, Ordering::Release);
    }

    #[must_use]
    pub fn ui(&self) -> Option<&Arc<dyn Assets>> {
        self.ui.as_ref()
    }
}

/// The whole HTTP surface: the documented API, plus the UI behind the fallback.
pub fn router(state: Arc<AppState>) -> Router {
    api_router(Arc::clone(&state)).fallback(move |method, uri| {
        let response = ui::serve(&state, &method, &uri);
        async move { response }
    })
}

/// Bind, serve until SIGINT or SIGTERM, then drain in-flight requests.
pub async fn serve(listen: SocketAddr, state: Arc<AppState>) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    let local = listener.local_addr().context("reading the bound address")?;
    let open = state.tokens.open_levels();
    if !local.ip().is_loopback() && !open.is_empty() {
        tracing::warn!(
            "listening on {local} with no token for: {} (set TIDEPOOL_<LEVEL>_TOKEN); anyone who can reach this port can use those endpoints",
            open.join(", ")
        );
    }
    state.set_ready();
    tracing::info!("tidepool listening on http://{local}");

    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("serving HTTP")
}

/// Resolves on the first SIGINT (Ctrl-C) or SIGTERM (what an orchestrator
/// sends). Failing to install a handler is logged and treated as "never",
/// rather than shutting the server down immediately.
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            tracing::error!("could not listen for Ctrl-C: {err}");
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(err) => {
                tracing::error!("could not listen for SIGTERM: {err}");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("shutting down");
}
