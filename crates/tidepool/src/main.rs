//! The `tidepool` binary: the CLI over everything else in the workspace.
//!
//! Only argument parsing, logging setup and wiring live here; `check` holds
//! the config subcommands.

mod check;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use tidepool_server::{AppState, Assets, DirAssets, Tokens, serve, ui};
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

#[derive(Subcommand)]
enum Command {
    /// Run the server.
    Serve {
        /// The config directory: tables, model and dashboards. Without it the
        /// server runs with no tables.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Address to listen on; overrides `listen` in tidepool.yaml. Loopback
        /// by default, so exposing the port is a decision made explicitly.
        #[arg(long)]
        listen: Option<SocketAddr>,
        /// Serve the UI from this directory instead of the one compiled into
        /// the binary. For development, against a frontend being rebuilt.
        #[arg(long)]
        ui_dir: Option<PathBuf>,
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
        Command::Plan { config, base } => check::plan(&config, base.as_deref()),
        Command::Serve {
            config,
            listen,
            ui_dir,
        } => match run_server(config.as_deref(), listen, ui_dir) {
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
    let listen = listen
        .or(model.server.listen)
        .unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], 7070)));
    let writer = WriterConfig {
        epoch_interval: model.server.epoch_interval.0,
        ..WriterConfig::default()
    };
    tracing::info!(
        "{} tables, {} metrics, {} dashboards",
        model.tables.len(),
        model.metrics.len(),
        model.dashboards.len()
    );
    let state = AppState::new(model, writer, ui, Tokens::from_env())?;
    serve(listen, Arc::new(state)).await
}
