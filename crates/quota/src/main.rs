//! `quota` — thin CLI client for `quotad`.
//!
//! No HTTP, no credential reads. The daemon is the only source of truth.

#![forbid(unsafe_code)]

#[cfg(not(unix))]
compile_error!("quota requires a Unix domain socket (macOS or Linux)");

mod client;
mod render;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use quota_core::protocol::{
    CanStartParams, CanStartResult, PaceParams, PaceResult, ProviderFilter, StatusParams,
    StatusResult, VersionInfo, WatchParams, METHOD_CAN_START, METHOD_PACE, METHOD_PING,
    METHOD_STATUS, METHOD_VERSION, METHOD_WATCH,
};
use quota_core::Config;

use crate::client::{decode_result, err_msg, rpc, rpc_watch, ClientError};
use crate::render::{print_can_start, print_pace, print_status};

#[derive(Parser, Debug)]
#[command(name = "quota", about = "Read inference quota from quotad", version)]
struct Cli {
    #[arg(long, global = true)]
    socket: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    Status {
        #[arg(long)]
        json: bool,
        #[arg(long, value_enum, default_value_t = ProviderArg::All)]
        provider: ProviderArg,
    },
    Pace {
        #[arg(long)]
        json: bool,
        #[arg(long, value_enum, default_value_t = ProviderArg::All)]
        provider: ProviderArg,
    },
    CanStart {
        #[arg(long)]
        tokens: u64,
        /// UTC unix seconds. Default: the window's published reset.
        #[arg(long)]
        deadline: Option<i64>,
        #[arg(long)]
        json: bool,
        #[arg(long, value_enum, default_value_t = ProviderArg::All)]
        provider: ProviderArg,
    },
    Watch {
        #[arg(long)]
        json: bool,
        #[arg(long, value_enum, default_value_t = ProviderArg::All)]
        provider: ProviderArg,
    },
    Ping,
    Version {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ProviderArg {
    All,
    Codex,
    Claude,
}

impl From<ProviderArg> for ProviderFilter {
    fn from(p: ProviderArg) -> Self {
        match p {
            ProviderArg::All => ProviderFilter::All,
            ProviderArg::Codex => ProviderFilter::Codex,
            ProviderArg::Claude => ProviderFilter::Claude,
        }
    }
}

fn socket_path(cli: &Cli) -> PathBuf {
    if let Some(p) = &cli.socket {
        return p.clone();
    }
    Config::load_default().socket_path()
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let sock = socket_path(&cli);
    match run(&cli, &sock) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("quota: {e}");
            if matches!(e, ClientError::Connect(_)) {
                eprintln!(
                    "is quotad running? start it with: quotad run --socket {}",
                    sock.display()
                );
            }
            ExitCode::FAILURE
        }
    }
}

fn run(cli: &Cli, sock: &std::path::Path) -> Result<ExitCode, ClientError> {
    match &cli.command {
        Command::Ping => {
            let resp = rpc(sock, 1, METHOD_PING, serde_json::Value::Null)?;
            if resp.ok {
                println!("pong");
                Ok(ExitCode::SUCCESS)
            } else {
                eprintln!("quota: {}", err_msg(&resp));
                Ok(ExitCode::FAILURE)
            }
        }
        Command::Version { json } => {
            let resp = rpc(sock, 1, METHOD_VERSION, serde_json::Value::Null)?;
            if *json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&resp.result).unwrap_or_default()
                );
            } else if let Some(v) = resp
                .result
                .as_ref()
                .and_then(|v| serde_json::from_value::<VersionInfo>(v.clone()).ok())
            {
                println!("{} {} protocol {}", v.name, v.version, v.protocol);
            } else {
                println!(
                    "{}",
                    serde_json::to_string(&resp.result).unwrap_or_default()
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Status { json, provider } => {
            let params = StatusParams {
                provider: (*provider).into(),
            };
            let resp = rpc(sock, 1, METHOD_STATUS, params)?;
            let result: StatusResult = decode_result(&resp)?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&result.snapshot)?);
            } else {
                print_status(&result.snapshot);
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Pace { json, provider } => {
            let params = PaceParams {
                provider: (*provider).into(),
            };
            let resp = rpc(sock, 1, METHOD_PACE, params)?;
            let result: PaceResult = decode_result(&resp)?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                print_pace(&result.reports);
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::CanStart {
            tokens,
            deadline,
            json,
            provider,
        } => {
            let params = CanStartParams {
                tokens: *tokens,
                deadline: *deadline,
                provider: (*provider).into(),
            };
            let resp = rpc(sock, 1, METHOD_CAN_START, params)?;
            let result: CanStartResult = decode_result(&resp)?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                print_can_start(&result);
            }
            if result.ok {
                Ok(ExitCode::SUCCESS)
            } else {
                Ok(ExitCode::from(2))
            }
        }
        Command::Watch { json, provider } => {
            let params = WatchParams {
                provider: (*provider).into(),
            };
            rpc_watch(sock, 1, METHOD_WATCH, params, |resp| {
                if let Ok(result) = decode_result::<StatusResult>(resp) {
                    if *json {
                        println!(
                            "{}",
                            serde_json::to_string(&result.snapshot).unwrap_or_default()
                        );
                    } else {
                        print_status(&result.snapshot);
                        println!("---");
                    }
                }
            })?;
            Ok(ExitCode::SUCCESS)
        }
    }
}
