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
    if let Some(dir) = cli.runtime_dir {
        // SAFETY: single-threaded at startup; no other threads exist yet.
        unsafe {
            std::env::set_var("TERHUB_RUNTIME_DIR", &dir);
        }
    }
    let runtime = daemon::runtime_dir();
    let server = Server::load(runtime).context("load daemon state")?;
    server.run().await
}
