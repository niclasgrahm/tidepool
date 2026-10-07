//! The Tidepool HTTP server: the API built from the endpoint table in
//! `tidepool-protocol`, the embedded UI, and the process lifecycle.
//!
//! A library rather than code in the binary so integration tests can drive
//! [`router`] in process through `tower::ServiceExt::oneshot`, with no socket.

mod endpoints;
mod handlers;
pub mod openapi;
pub mod ui;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Context;
use axum::Router;

pub use endpoints::api_router;
pub use ui::{Assets, DirAssets};

/// Everything a handler can reach. Grows a model, a store and sessions in the
/// milestones that add them.
pub struct AppState {
    ready: AtomicBool,
    ui: Option<Arc<dyn Assets>>,
}

impl AppState {
    /// A server that is not ready yet and serves the given UI, if any.
    #[must_use]
    pub fn new(ui: Option<Arc<dyn Assets>>) -> Self {
        Self {
            ready: AtomicBool::new(false),
            ui,
        }
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
    if !local.ip().is_loopback() {
        // Nothing is authenticated until M8, so a server reachable from other
        // machines is open to anyone who can reach the port.
        tracing::warn!(
            "listening on {local} without authentication: anyone who can reach this port can use the API"
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
