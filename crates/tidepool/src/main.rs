//! The `tidepool` binary: the CLI over everything else in the workspace.
//!
//! Only argument parsing, logging setup and wiring live here; `check` holds
//! the config subcommands.

mod check;
mod quickstart;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use tidepool_server::{AppState, Assets, DirAssets, InsecureOpen, Project, Tokens, serve, ui};
use tidepool_store::WriterConfig;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    name = "tidepool",
    version,
    about = "A live, associative analytics server"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// A level `--insecure-open` may leave open.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum OpenLevel {
    Ingest,
    Admin,
}

#[derive(Subcommand)]
enum Command {
    /// Run the server.
    Serve {
        /// The config directory: tables, model and dashboards. Without it the
        /// server runs with no tables.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Address to listen on, for everything; overrides `listen` in
        /// tidepool.yaml. Loopback by default, so exposing the port is a
        /// decision made explicitly.
        #[arg(long)]
        listen: Option<SocketAddr>,
        /// Start although these levels have no token on an address away from
        /// loopback: `ingest`, `admin` or both (comma-separated). Without it
        /// such a server refuses to start. For demos in containers.
        #[arg(long, value_delimiter = ',', env = "TIDEPOOL_INSECURE_OPEN")]
        insecure_open: Vec<OpenLevel>,
        /// Serve the UI from this directory instead of the one compiled into
        /// the binary. For development, against a frontend being rebuilt.
        #[arg(long)]
        ui_dir: Option<PathBuf>,
        /// Keep data here (write-ahead log, checkpoints); overrides
        /// `data_dir` in tidepool.yaml. Without either, data lives in memory
        /// only and is gone when the server stops.
        #[arg(long)]
        data: Option<PathBuf>,
        /// Apply a config directory that changed while the server was down
        /// even if that destroys data (a table or column removed, a type
        /// narrowed). Without it such a server refuses to start.
        #[arg(long)]
        allow_destructive: bool,
    },
    /// Start a project: the retail example (tables, a model with metrics,
    /// dashboards) in a new directory.
    Init {
        /// Where to write it.
        #[arg(default_value = "tidepool-project")]
        dir: PathBuf,
    },
    /// Send sample data to a server running an `init` project: customers,
    /// products, ninety days of orders, then new orders as they happen.
    Feed {
        /// The server.
        #[arg(long, default_value = "http://127.0.0.1:7070")]
        server: String,
        /// New orders a second.
        #[arg(long, default_value_t = 20)]
        rate: u64,
        /// Stop after this many seconds (default: run until stopped).
        #[arg(long)]
        seconds: Option<u64>,
    },
    /// Check a config directory: every file, and how they fit together.
    Validate {
        /// The config directory.
        #[arg(long, default_value = ".")]
        config: PathBuf,
    },
    /// Show what changes from one config to another, and what each change costs.
    Plan {
        /// The config directory with the changes.
        #[arg(long, default_value = ".")]
        config: PathBuf,
        /// The config directory to compare against. Without it, the plan is
        /// against an empty project.
        #[arg(long)]
        base: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Validate { config } => check::validate(&config),
        Command::Init { dir } => quickstart::init(&dir),
        Command::Feed {
            server,
            rate,
            seconds,
        } => {
            // the ingest token, if the server wants one (the admin one works too)
            let token = std::env::var("TIDEPOOL_INGEST_TOKEN")
                .ok()
                .or_else(|| std::env::var("TIDEPOOL_ADMIN_TOKEN").ok())
                .filter(|t| !t.is_empty());
            quickstart::feed(&server, rate, token.as_deref(), seconds)
        }
        Command::Plan { config, base } => check::plan(&config, base.as_deref()),
        Command::Serve {
            config,
            listen,
            ui_dir,
            data,
            allow_destructive,
            insecure_open,
        } => match run_server(
            config.as_deref(),
            listen,
            ui_dir,
            data,
            allow_destructive,
            InsecureOpen {
                ingest: insecure_open.contains(&OpenLevel::Ingest),
                admin: insecure_open.contains(&OpenLevel::Admin),
            },
        ) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("error: {err:#}");
                ExitCode::FAILURE
            }
        },
    }
}

#[tokio::main]
async fn run_server(
    config: Option<&std::path::Path>,
    listen: Option<SocketAddr>,
    ui_dir: Option<PathBuf>,
    data: Option<PathBuf>,
    allow_destructive: bool,
    insecure: InsecureOpen,
) -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let ui: Option<Arc<dyn Assets>> = match ui_dir {
        Some(dir) => {
            anyhow::ensure!(
                dir.join("index.html").is_file(),
                "--ui-dir {} has no index.html; build the frontend first",
                dir.display()
            );
            Some(Arc::new(DirAssets::new(dir)))
        }
        None => ui::embedded(),
    };
    let compiled = match config {
        Some(dir) => check::load(dir, &tidepool_model::Catalog::default())
            .ok_or_else(|| anyhow::anyhow!("the config in {} has errors", dir.display()))?,
        None => match tidepool_model::compile(
            &tidepool_config::ConfigSet::default(),
            &tidepool_model::Catalog::default(),
        ) {
            (Some(compiled), _) => compiled,
            (None, _) => anyhow::bail!("the empty project didn't compile"),
        },
    };
    let model = Arc::new(compiled.model);
    let listeners = match listen {
        Some(address) => tidepool_config::server::Listen::One(address),
        None => model.server.listen_or_default(),
    }
    .listeners()
    .ok_or_else(|| anyhow::anyhow!("`listen` gives no address to listen on"))?;
    // a relative data_dir is relative to the config directory
    let data = data.or_else(|| {
        let dir = PathBuf::from(model.server.data_dir.as_ref()?);
        Some(match config {
            Some(config) if dir.is_relative() => config.join(dir),
            _ => dir,
        })
    });
    if let Some(dir) = &data {
        tracing::info!("data in {}", dir.display());
    } else {
        tracing::warn!("no data directory (--data or data_dir): data lives in memory only");
    }
    let writer = WriterConfig {
        epoch_interval: model.server.epoch_interval.0,
        data,
        durability: model.server.durability,
        ..WriterConfig::default()
    };
    tracing::info!(
        "{} tables, {} metrics, {} dashboards",
        model.tables.len(),
        model.metrics.len(),
        model.dashboards.len()
    );
    let project = Project {
        files: match config {
            Some(dir) => tidepool_server::config::read_dir(dir)?,
            None => tidepool_server::config::Files::new(),
        },
        dir: config.map(std::path::Path::to_path_buf),
    };
    let tokens = Tokens::from_settings(&model.server.auth);
    let state = AppState::open(project, writer, ui, tokens, allow_destructive).await?;
    serve(listeners, insecure, Arc::new(state)).await
}
