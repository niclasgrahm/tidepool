//! The `tidepool` binary: the CLI over everything else in the workspace.
//!
//! Only argument parsing, logging setup and wiring live here. Subcommands that
//! do real work (`validate`, `plan`, `init`) arrive with the milestones that
//! give them something to do.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use tidepool_server::{AppState, Assets, DirAssets, serve, ui};
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
        /// Address to listen on. Loopback by default: nothing is authenticated
        /// yet, so exposing the port is a decision to make explicitly.
        #[arg(long, default_value = "127.0.0.1:7070")]
        listen: SocketAddr,
        /// Serve the UI from this directory instead of the one compiled into
        /// the binary. For development, against a frontend being rebuilt.
        #[arg(long)]
        ui_dir: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    match Cli::parse().command {
        Command::Serve { listen, ui_dir } => {
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
            serve(listen, Arc::new(AppState::new(ui))).await
        }
    }
}
