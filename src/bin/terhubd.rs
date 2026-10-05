//! terhubd: agent-registry daemon. Foreground process; `terhub daemon start`
//! launches it detached.
use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use terhub::daemon::{self, Server};

#[derive(Parser)]
#[command(name = "terhubd", about = "terhub agent-registry daemon")]
struct Cli {
    /// Runtime dir for socket/registry/pid (default: resolved like clients).
    #[arg(long)]
    runtime_dir: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    // The flag passes straight through: no env write, so no `unsafe set_var`
    // under the already-spawned tokio worker threads (their existence is
    // exactly what makes set_var unsound here). The daemon never reads
    // TERHUB_RUNTIME_DIR again after this line — Server owns its paths.
    let runtime = cli.runtime_dir.unwrap_or_else(daemon::runtime_dir);
    let server = Server::load(runtime).context("load daemon state")?;
    server.run().await
}
