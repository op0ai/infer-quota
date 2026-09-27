//! `quotad` — foreground inference-quota daemon.
//!
//! Runtime: Tokio `current_thread` (no multi-worker pool). HTTP probes run in
//! `spawn_blocking` so a slow provider cannot stall socket accept.

#![forbid(unsafe_code)]

#[cfg(not(unix))]
compile_error!("quotad requires a Unix domain socket (macOS or Linux)");

mod accounts;
mod daemon;
mod store;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use quota_core::Config;

use crate::daemon::run;

#[derive(Parser, Debug)]
#[command(
    name = "quotad",
    about = "Tiny inference-quota daemon (Codex + Claude session reuse)",
    version
)]
struct Cli {
    #[arg(long, global = true)]
    socket: Option<PathBuf>,
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run in the foreground (default).
    Run,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let mut cfg = match cli.config {
        Some(p) => Config::load_path(&p),
        None => Config::load_default(),
    };
    if let Some(sock) = cli.socket {
        cfg.socket = Some(sock);
    }
    match run(cfg) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("quotad: {e}");
            ExitCode::FAILURE
        }
    }
}
