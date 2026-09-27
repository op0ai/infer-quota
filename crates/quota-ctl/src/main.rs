//! `quota-ctl` — mutating control surface for `quotad`.
//!
//! Does not collect usage. Tokens go through `quota-secrets`, never the socket.

#![forbid(unsafe_code)]

#[cfg(not(unix))]
compile_error!("quota-ctl requires a Unix domain socket (macOS or Linux)");

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use quota_core::protocol::AccountsAddParams;
use quota_core::types::ProviderId;
use quota_core::{Config, ProviderFilter};
use quota_secrets::{from_env, SecretsBackend};

#[derive(Parser, Debug)]
#[command(
    name = "quota-ctl",
    about = "Accounts, refresh, and secret pointers for quotad",
    version
)]
struct Cli {
    #[arg(long, global = true)]
    socket: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// List account metadata stored by the daemon.
    Accounts {
        #[command(subcommand)]
        action: AccountsCmd,
    },
    /// Ask quotad to probe providers now.
    Refresh {
        #[arg(long, value_enum, default_value_t = ProviderArg::All)]
        provider: ProviderArg,
        #[arg(long)]
        json: bool,
    },
    Ping,
    /// Local secrets chain (OpenBao → keychain stub → CLI files). Not RPC.
    Secret {
        #[command(subcommand)]
        action: SecretCmd,
    },
}

#[derive(Subcommand, Debug)]
enum AccountsCmd {
    List {
        #[arg(long)]
        json: bool,
    },
    Add {
        #[arg(long)]
        id: Option<String>,
        #[arg(long, value_enum)]
        provider: ProviderIdArg,
        #[arg(long)]
        email: Option<String>,
        #[arg(long)]
        workspace_label: Option<String>,
        #[arg(long)]
        login_method: Option<String>,
        /// Secrets backend name only (`openbao`, `keychain`, `file`). No material.
        #[arg(long)]
        secret_backend: Option<String>,
        #[arg(long)]
        secret_path: Option<String>,
        #[arg(long)]
        home_path: Option<String>,
        #[arg(long)]
        select: bool,
        #[arg(long)]
        json: bool,
    },
    Remove {
        #[arg(long)]
        id: String,
        #[arg(long)]
        json: bool,
    },
    Select {
        /// Omit `--id` (or pass empty) to clear the active account.
        #[arg(long)]
        id: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
enum SecretCmd {
    /// Show which backends the chain will try (never prints secret values).
    Backends,
    /// Look up a path. Prints `backend=` and `path=` only — never the bytes.
    Get { path: String },
    /// Write to the first writable backend (typically OpenBao when configured).
    Put {
        path: String,
        /// Read material from this environment variable (never argv).
        #[arg(long)]
        from_env: String,
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

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ProviderIdArg {
    Codex,
    Claude,
}

impl From<ProviderIdArg> for ProviderId {
    fn from(p: ProviderIdArg) -> Self {
        match p {
            ProviderIdArg::Codex => ProviderId::Codex,
            ProviderIdArg::Claude => ProviderId::Claude,
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
            eprintln!("quota-ctl: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: &Cli, sock: &std::path::Path) -> Result<ExitCode, Box<dyn std::error::Error>> {
    match &cli.command {
        Command::Ping => {
            quota_ctl::ping(sock)?;
            println!("pong");
            Ok(ExitCode::SUCCESS)
        }
        Command::Refresh { provider, json } => {
            let result = quota_ctl::refresh(sock, (*provider).into())?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&result.snapshot)?);
            } else {
                println!(
                    "refreshed {} provider(s) at {}",
                    result.snapshot.providers.len(),
                    result.snapshot.fetched_at_rfc3339
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Accounts { action } => match action {
            AccountsCmd::List { json } => {
                let book = quota_ctl::list_accounts(sock)?;
                if *json {
                    println!("{}", serde_json::to_string_pretty(&book)?);
                } else {
                    println!(
                        "accounts {}  active {}",
                        book.accounts.len(),
                        book.active_id.as_deref().unwrap_or("-")
                    );
                    for a in &book.accounts {
                        println!(
                            "  {:<24} {:<8} {} {}",
                            a.id,
                            a.provider.as_str(),
                            a.email.as_deref().unwrap_or("-"),
                            a.workspace_label.as_deref().unwrap_or("")
                        );
                    }
                }
                Ok(ExitCode::SUCCESS)
            }
            AccountsCmd::Add {
                id,
                provider,
                email,
                workspace_label,
                login_method,
                secret_backend,
                secret_path,
                home_path,
                select,
                json,
            } => {
                let params = AccountsAddParams {
                    id: id.clone(),
                    provider: (*provider).into(),
                    email: email.clone(),
                    workspace_label: workspace_label.clone(),
                    login_method: login_method.clone(),
                    workspace_account_id: None,
                    secret_ref: quota_ctl::secret_ref(secret_backend.clone(), secret_path.clone()),
                    home_path: home_path.clone(),
                    select: *select,
                };
                let result = quota_ctl::add_account(sock, params)?;
                if *json {
                    println!("{}", serde_json::to_string_pretty(&result)?);
                } else {
                    println!(
                        "added {} (active {})",
                        result.account.id,
                        result.active_id.as_deref().unwrap_or("-")
                    );
                }
                Ok(ExitCode::SUCCESS)
            }
            AccountsCmd::Remove { id, json } => {
                let book = quota_ctl::remove_account(sock, id)?;
                if *json {
                    println!("{}", serde_json::to_string_pretty(&book)?);
                } else {
                    println!("removed {id}");
                }
                Ok(ExitCode::SUCCESS)
            }
            AccountsCmd::Select { id, json } => {
                let want = id.as_deref().filter(|s| !s.is_empty());
                let book = quota_ctl::select_account(sock, want)?;
                if *json {
                    println!("{}", serde_json::to_string_pretty(&book)?);
                } else {
                    println!("active {}", book.active_id.as_deref().unwrap_or("-"));
                }
                Ok(ExitCode::SUCCESS)
            }
        },
        Command::Secret { action } => match action {
            SecretCmd::Backends => {
                let chain = from_env()?;
                println!("{}", chain.backend_names().join(" "));
                Ok(ExitCode::SUCCESS)
            }
            SecretCmd::Get { path } => {
                let chain = from_env()?;
                match chain.get(path)? {
                    Some(rec) => {
                        println!("backend={} path={} present=true", rec.backend, rec.path);
                        Ok(ExitCode::SUCCESS)
                    }
                    None => {
                        eprintln!("quota-ctl: secret not found");
                        Ok(ExitCode::from(2))
                    }
                }
            }
            SecretCmd::Put {
                path,
                from_env: var,
            } => {
                let value = std::env::var(var).map_err(|_| {
                    format!("environment variable {var} is unset (refusing empty secret)")
                })?;
                if value.is_empty() {
                    return Err("refusing to store an empty secret".into());
                }
                let chain = from_env()?;
                chain.put(path, &value)?;
                println!("stored path={path} (value not printed)");
                Ok(ExitCode::SUCCESS)
            }
        },
    }
}
