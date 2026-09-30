//! `quotad` — foreground inference-quota daemon.
//!
//! Runtime: Tokio `current_thread` (no multi-worker pool). Synchronous HTTP
//! probes run on detached threads so a slow provider cannot stall socket
//! accept or hold runtime shutdown open.

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
#[command(name = "quotad", about = "Rust-native inference quota daemon", version)]
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
    let cfg = match cli.config {
        Some(p) => Config::load_explicit(&p),
        None => Config::load_default(),
    };
    let mut cfg = match cfg {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("quotad: {e}");
            return ExitCode::FAILURE;
        }
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
