//! The Tidepool HTTP server: the API built from the endpoint table in
//! `tidepool-protocol`, the embedded UI, and the process lifecycle.
//!
//! A library rather than code in the binary so integration tests can drive
//! [`router`] in process through `tower::ServiceExt::oneshot`, with no socket.

mod apps;
pub mod auth;
pub mod config;
mod endpoints;
mod filters;
mod handlers;
mod idempotency;
mod limits;
mod live;
mod mcp;
pub mod metrics;
mod model;
pub mod openapi;
mod query;
mod tables;
pub mod ui;
mod views;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Context;
use axum::Router;

pub use auth::{Credential, Listener, Tokens};
pub use endpoints::{api_router, api_router_on};
pub use mcp::tools as mcp_tools;
pub use ui::{Assets, DirAssets};

use tidepool_config::server::Surface;
use tidepool_model::Model;
use tidepool_protocol::api_docs::Access;
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
    /// Counts and timings for `/metrics` and the status page.
    pub(crate) counters: metrics::Counters,
    /// Browser sessions.
    pub(crate) sessions: auth::Sessions,
    /// Tokens for views embedded in other apps.
    pub(crate) embeds: auth::Embeds,
    /// What each ingest credential has used of its limits.
    pub(crate) limiter: limits::Limiter,
    /// What time it is, for selections relative to now.
    pub(crate) clock: Clock,
    /// Dashboards that live only in memory (`POST /v1/views`).
    pub(crate) views: views::Views,
}

/// The time relative windows are resolved against: the wall clock, or a
/// fixed time a test sets.
#[derive(Default)]
pub(crate) struct Clock(std::sync::Mutex<Option<i64>>);

impl Clock {
    /// Microseconds since the Unix epoch, UTC.
    pub(crate) fn now(&self) -> i64 {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .unwrap_or_else(|| chrono::Utc::now().timestamp_micros())
    }
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
        let keys = idempotency::Keys::default();
        keys.restore(
            writer.take_recovered_keys(),
            std::time::Instant::now(),
            tidepool_store::keys::now_ms(),
        );
        Self {
            ready: AtomicBool::new(false),
            ui,
            writer,
            tokens,
            keys,
            engine: Arc::new(tidepool_engine::Engine::default()),
            hub: live::Hub::default(),
            started: std::time::Instant::now(),
            counters: metrics::Counters::default(),
            sessions: auth::Sessions::default(),
            embeds: auth::Embeds::default(),
            limiter: limits::Limiter::default(),
            clock: Clock::default(),
            views: views::Views::default(),
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

    /// Stop the clock at `micros` (since the Unix epoch, UTC), or let it
    /// run again with `None`: for tests of selections relative to now.
    pub fn set_clock(&self, micros: Option<i64>) {
        *self
            .clock
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = micros;
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
    router_on(state, Listener::ALL)
}

/// The HTTP surface as `listener` serves it, the UI included. A
/// documented path this listener doesn't serve (`/metrics` on the public
/// one) is a JSON 404, never the UI.
pub fn router_on(state: Arc<AppState>, listener: Listener) -> Router {
    api_router_on(Arc::clone(&state), listener).fallback(move |method, uri: axum::http::Uri| {
        let response = if endpoints::documented(uri.path()) {
            handlers::api_error(
                axum::http::StatusCode::NOT_FOUND,
                format!("`{}` isn't served on this listener", uri.path()),
            )
        } else {
            ui::serve(&state, &method, &uri)
        };
        async move { response }
    })
}

/// Levels `serve` may leave open away from loopback because the operator
/// said so (`--insecure-open`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InsecureOpen {
    pub ingest: bool,
    pub admin: bool,
}

/// Whether these listeners may start with these tokens (plan §9.2, fail
/// closed): an error for ingest or admin open on an address away from
/// loopback unless `insecure` allows it, and for tokens shared between
/// credentials. Otherwise the warnings to log.
///
/// # Errors
/// Why it may not start.
pub fn exposure(
    listeners: &[(SocketAddr, Vec<Surface>)],
    tokens: &Tokens,
    insecure: InsecureOpen,
) -> anyhow::Result<Vec<String>> {
    let (errors, mut warnings) = tokens.problems();
    if let Some(error) = errors.first() {
        anyhow::bail!("{error}");
    }
    for (address, surfaces) in listeners {
        if address.ip().is_loopback() {
            continue;
        }
        let listener = Listener::of(surfaces);
        let ingest = !listener.public_only() && tokens.open(Access::Ingest);
        if ingest && !insecure.ingest {
            anyhow::bail!(
                "refusing to listen on {address} with ingest open: anyone who can reach it could                  write, replace and delete rows. Set TIDEPOOL_INGEST_TOKEN (or name credentials                  under `auth.ingest`), listen on loopback, or start with --insecure-open ingest"
            );
        }
        if listener.admin && tokens.open(Access::Admin) && !insecure.admin {
            anyhow::bail!(
                "refusing to listen on {address} with admin open: anyone who can reach it could                  change the config. Set TIDEPOOL_ADMIN_TOKEN, give `listen.admin` a loopback                  address, or start with --insecure-open admin"
            );
        }
        let open: Vec<&str> = [
            (Access::Read, true),
            (Access::Ingest, ingest),
            (Access::Admin, listener.admin),
        ]
        .into_iter()
        .filter(|(access, served)| *served && tokens.open(*access))
        .map(|(access, _)| access.label())
        .collect();
        if !open.is_empty() {
            warnings.push(format!(
                "listening on {address} with no token for: {}; anyone who can reach it can use those endpoints",
                open.join(", ")
            ));
        }
    }
    Ok(warnings)
}

/// Bind every listener, serve until SIGINT or SIGTERM, then drain
/// in-flight requests. Refuses to start what [`exposure`] refuses.
///
/// # Errors
/// An address can't be bound, or the exposure isn't allowed.
pub async fn serve(
    listeners: Vec<(SocketAddr, Vec<Surface>)>,
    insecure: InsecureOpen,
    state: Arc<AppState>,
) -> anyhow::Result<()> {
    for warning in exposure(&listeners, &state.tokens, insecure)? {
        tracing::warn!("{warning}");
    }
    let mut bound = Vec::new();
    for (address, surfaces) in listeners {
        let listener = tokio::net::TcpListener::bind(address)
            .await
            .with_context(|| format!("binding {address}"))?;
        let local = listener.local_addr().context("reading the bound address")?;
        bound.push((listener, local, surfaces));
    }
    config::watch(&state);
    state.set_ready();
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let mut servers = tokio::task::JoinSet::new();
    for (listener, local, surfaces) in bound {
        let names: Vec<&str> = surfaces.iter().map(|s| s.label()).collect();
        tracing::info!(
            "tidepool listening on http://{local} ({})",
            names.join(", ")
        );
        let app = router_on(Arc::clone(&state), Listener::of(&surfaces));
        let mut stopped = stopped.clone();
        servers.spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = stopped.wait_for(|stop| *stop).await;
                })
                .await
        });
    }
    tokio::spawn(async move {
        shutdown_signal().await;
        let _ = stop.send(true);
    });
    while let Some(done) = servers.join_next().await {
        done.context("a listener stopped")?
            .context("serving HTTP")?;
    }
    Ok(())
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
