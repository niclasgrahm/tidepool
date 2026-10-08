//! The Tidepool HTTP server: the API built from the endpoint table in
//! `tidepool-protocol`, the embedded UI, and the process lifecycle.
//!
//! A library rather than code in the binary so integration tests can drive
//! [`router`] in process through `tower::ServiceExt::oneshot`, with no socket.

pub mod auth;
pub mod config;
mod endpoints;
mod handlers;
mod idempotency;
mod live;
mod model;
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
use tidepool_store::{Change, ModelVersion, WriterConfig, WriterHandle};

/// Everything a handler can reach.
pub struct AppState {
    ready: AtomicBool,
    ui: Option<Arc<dyn Assets>>,
    pub(crate) writer: WriterHandle,
    pub(crate) tokens: Tokens,
    pub(crate) keys: idempotency::Keys,
    /// The query engine and its caches, shared by every query.
    pub(crate) engine: Arc<tidepool_engine::Engine>,
    /// Live sessions on `/v1/ws`.
    pub(crate) hub: live::Hub,
    pub(crate) started: std::time::Instant,
    /// The config files, their versions, and changes waiting for approval.
    pub(crate) config: config::ConfigStore,
}

/// A config to run: its directory (written back to and watched), if any,
/// and its files.
pub struct Project {
    pub dir: Option<std::path::PathBuf>,
    pub files: config::Files,
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
        let (writer, _thread) = tidepool_store::spawn(ModelVersion::initial(model), writer)?;
        Ok(Self::with_writer(writer, ui, tokens))
    }

    /// A server for a project: the writer started (and its data recovered),
    /// then the config reconciled with the data (plan §8.1, startup). A
    /// config directory that changed while the server was down is applied
    /// as a change; one that would destroy data only with
    /// `allow_destructive`. Returns once the model the config describes is
    /// live, rebuilds included.
    ///
    /// # Errors
    /// The config doesn't compile, the data can't be recovered, or the
    /// change would destroy data.
    pub async fn open(
        project: Project,
        writer: WriterConfig,
        ui: Option<Arc<dyn Assets>>,
        tokens: Tokens,
        allow_destructive: bool,
    ) -> anyhow::Result<Self> {
        let (set, mut diagnostics) = tidepool_config::ConfigSet::from_files(project.files.clone());
        let (compiled, more) = tidepool_model::compile(&set, &tidepool_model::Catalog::default());
        diagnostics.extend(more);
        let Some(compiled) = compiled else {
            anyhow::bail!(
                "the config has errors:\n{}",
                tidepool_config::diag::render_all(&diagnostics, &set.files)
            );
        };
        let data = writer.data.clone();
        let (writer, _thread) = tidepool_store::spawn(
            ModelVersion {
                version: 1,
                model: Arc::new(compiled.model),
                catalog: Arc::new(compiled.catalog),
            },
            writer,
        )?;
        let snapshot = writer.snapshot();
        let live = ModelVersion {
            version: snapshot.version,
            model: Arc::clone(&snapshot.model),
            catalog: Arc::clone(&snapshot.catalog),
        };
        let started = config::ConfigStore::start(
            project.dir,
            data.as_deref(),
            project.files,
            &live,
            allow_destructive,
        )?;
        if let Some(target) = started.change {
            let version = target.version;
            tracing::info!("applying the config directory's changes: version {version}");
            if let Change::Rebuilding { rows } = writer
                .change_model(target)
                .await
                .map_err(|e| anyhow::anyhow!(e))?
            {
                tracing::info!("rebuilding {rows} rows for version {version}");
                let mut epochs = writer.epochs();
                while writer.snapshot().version < version {
                    epochs
                        .changed()
                        .await
                        .context("the writer stopped during a rebuild")?;
                }
            }
        }
        let mut state = Self::with_writer(writer, ui, tokens);
        state.config = started.store;
        Ok(state)
    }

    /// A server around an existing writer handle (tests use one whose queue is
    /// never drained, to see what happens when the writer is busy).
    #[must_use]
    pub fn with_writer(writer: WriterHandle, ui: Option<Arc<dyn Assets>>, tokens: Tokens) -> Self {
        let snapshot = writer.snapshot();
        Self {
            ready: AtomicBool::new(false),
            ui,
            writer,
            tokens,
            keys: idempotency::Keys::default(),
            engine: Arc::new(tidepool_engine::Engine::default()),
            hub: live::Hub::default(),
            started: std::time::Instant::now(),
            config: config::ConfigStore::detached(&ModelVersion {
                version: snapshot.version,
                model: Arc::clone(&snapshot.model),
                catalog: Arc::clone(&snapshot.catalog),
            }),
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
    config::watch(&state);
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
