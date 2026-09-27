//! Accept loop, adaptive refresh, and request dispatch.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use quota_adapters::http::TlsTransport;
use quota_adapters::provider::Provider;
use quota_adapters::{ClaudeAdapter, CodexAdapter};
use quota_core::framing::{decode_len, encode_frame, FrameError};
use quota_core::math::{can_start, pace_for};
use quota_core::protocol::{
    AccountMutationResult, AccountsAddParams, AccountsRemoveParams, AccountsSelectParams,
    CanStartParams, CanStartResult, ErrorBody, PaceParams, PaceResult, ProviderFilter,
    RefreshParams, Request, Response, StatusParams, StatusResult, VersionInfo, WatchParams,
    METHOD_ACCOUNTS_ADD, METHOD_ACCOUNTS_LIST, METHOD_ACCOUNTS_REMOVE, METHOD_ACCOUNTS_SELECT,
    METHOD_CAN_START, METHOD_PACE, METHOD_PING, METHOD_REFRESH, METHOD_STATUS, METHOD_VERSION,
    METHOD_WATCH,
};
use quota_core::timeutil::now_unix;
use quota_core::types::{Availability, Snapshot};
use quota_core::Config;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, Mutex};

use crate::accounts::AccountStore;
use crate::store::Store;

pub fn run(cfg: Config) -> Result<(), DaemonError> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| DaemonError::Io(e.to_string()))?;
    rt.block_on(run_async(cfg))
}

#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("{0}")]
    Io(String),
}

struct App {
    cfg: Config,
    store: Store,
    accounts: AccountStore,
    interval_secs: u64,
    watch_tx: broadcast::Sender<Snapshot>,
}

impl App {
    fn snapshot_or_empty(&self) -> Snapshot {
        self.store
            .latest()
            .cloned()
            .unwrap_or_else(|| Snapshot::new(now_unix(), Vec::new()))
    }
}

async fn run_async(cfg: Config) -> Result<(), DaemonError> {
    let socket = cfg.socket_path();
    prepare_socket(&socket)?;

    let listener = UnixListener::bind(&socket).map_err(|e| DaemonError::Io(e.to_string()))?;
    let _ = fs::set_permissions(&socket, fs::Permissions::from_mode(0o600));

    eprintln!(
        "quotad {} listening on {} (protocol {})",
        quota_core::PACKAGE_VERSION,
        socket.display(),
        quota_core::PROTOCOL_VERSION
    );

    let (watch_tx, _) = broadcast::channel(16);
    let accounts = AccountStore::load(cfg.accounts_file());
    let app = Arc::new(Mutex::new(App {
        interval_secs: cfg.refresh_min_secs(),
        store: Store::new(&cfg),
        accounts,
        cfg,
        watch_tx,
    }));

    // First probe before serving so `status` is not empty.
    refresh(app.clone()).await;

    loop {
        let wait = {
            let g = app.lock().await;
            Duration::from_secs(g.interval_secs)
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                eprintln!("quotad: shutting down");
                break;
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        let app = app.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_client(app, stream).await {
                                if !matches!(e, ClientError::Eof) {
                                    eprintln!("quotad: client: {e}");
                                }
                            }
                        });
                    }
                    Err(e) => eprintln!("quotad: accept: {e}"),
                }
            }
            _ = tokio::time::sleep(wait) => {
                refresh(app.clone()).await;
            }
        }
    }
    let _ = fs::remove_file(&socket);
    Ok(())
}

fn prepare_socket(path: &Path) -> Result<(), DaemonError> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| DaemonError::Io(e.to_string()))?;
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    }
    if path.exists() {
        fs::remove_file(path).map_err(|e| DaemonError::Io(e.to_string()))?;
    }
    Ok(())
}

async fn refresh(app: Arc<Mutex<App>>) {
    let (timeout, enable_codex, enable_claude, min_s, max_s, codex_home) = {
        let g = app.lock().await;
        let home = g
            .accounts
            .book()
            .active()
            .and_then(|a| a.home_path.clone())
            .map(std::path::PathBuf::from);
        (
            g.cfg.http_timeout_secs,
            g.cfg.enable_codex,
            g.cfg.enable_claude,
            g.cfg.refresh_min_secs(),
            g.cfg.refresh_max_secs(),
            home,
        )
    };

    let snap = tokio::task::spawn_blocking(move || {
        let transport = TlsTransport::new(Duration::from_secs(timeout));
        let codex = CodexAdapter { home: codex_home };
        let claude = ClaudeAdapter;
        let mut providers: Vec<&dyn Provider> = Vec::new();
        if enable_codex {
            providers.push(&codex);
        }
        if enable_claude {
            providers.push(&claude);
        }
        quota_adapters::probe_all(&providers, &transport)
    })
    .await;

    let Ok(snap) = snap else {
        eprintln!("quotad: refresh task failed");
        return;
    };

    let mut g = app.lock().await;
    let changed = match g.store.latest() {
        Some(prev) => !same_usage(prev, &snap),
        None => true,
    };
    let any_ok = snap.providers.iter().any(|p| p.status == Availability::Ok);
    let rate_limited = snap
        .providers
        .iter()
        .any(|p| p.error.as_ref().is_some_and(|e| e.code == "rate_limited"));

    // Adaptive refresh: stay faster while numbers move; idle longer when stable
    // or unavailable so we do not hammer undocumented endpoints.
    g.interval_secs = if rate_limited {
        max_s
    } else if !any_ok {
        (g.interval_secs.saturating_mul(2)).clamp(min_s, max_s)
    } else if changed {
        min_s
    } else {
        (g.interval_secs.saturating_mul(3) / 2).clamp(min_s, max_s)
    };

    let _ = g.watch_tx.send(snap.clone());
    g.store.push(snap);
}

fn same_usage(a: &Snapshot, b: &Snapshot) -> bool {
    if a.providers.len() != b.providers.len() {
        return false;
    }
    a.providers.iter().zip(b.providers.iter()).all(|(x, y)| {
        x.provider == y.provider
            && x.status == y.status
            && x.windows
                .iter()
                .map(|w| (w.kind.clone(), w.used_percent))
                .eq(y.windows.iter().map(|w| (w.kind.clone(), w.used_percent)))
    })
}

#[derive(Debug, thiserror::Error)]
enum ClientError {
    #[error("eof")]
    Eof,
    #[error("{0}")]
    Other(String),
}

impl From<FrameError> for ClientError {
    fn from(e: FrameError) -> Self {
        match e {
            FrameError::UnexpectedEof => Self::Eof,
            other => Self::Other(other.to_string()),
        }
    }
}

async fn handle_client(app: Arc<Mutex<App>>, mut stream: UnixStream) -> Result<(), ClientError> {
    loop {
        let payload = match read_frame_async(&mut stream).await {
            Ok(p) => p,
            Err(FrameError::UnexpectedEof) => return Err(ClientError::Eof),
            Err(e) => return Err(e.into()),
        };
        let req: Request = serde_json::from_slice(&payload)
            .map_err(|e| ClientError::Other(format!("bad request: {e}")))?;

        if req.method == METHOD_WATCH {
            let params: WatchParams =
                serde_json::from_value(req.params.clone()).unwrap_or_default();
            let mut rx = {
                let g = app.lock().await;
                let snap = filter_snapshot(g.snapshot_or_empty(), params.provider);
                write_frame_async(
                    &mut stream,
                    &Response::result(req.id, StatusResult { snapshot: snap }),
                )
                .await?;
                g.watch_tx.subscribe()
            };
            loop {
                tokio::select! {
                    next = rx.recv() => {
                        match next {
                            Ok(snap) => {
                                let snap = filter_snapshot(snap, params.provider);
                                write_frame_async(
                                    &mut stream,
                                    &Response::result(req.id, StatusResult { snapshot: snap }),
                                )
                                .await?;
                            }
                            Err(_) => return Ok(()),
                        }
                    }
                    incoming = read_frame_async(&mut stream) => {
                        match incoming {
                            Ok(bytes) => {
                                if let Ok(r) = serde_json::from_slice::<Request>(&bytes) {
                                    if r.method == METHOD_PING {
                                        write_frame_async(
                                            &mut stream,
                                            &Response::result(r.id, quota_core::protocol::Pong { pong: true }),
                                        )
                                        .await?;
                                    }
                                }
                            }
                            Err(FrameError::UnexpectedEof) => return Err(ClientError::Eof),
                            Err(e) => return Err(e.into()),
                        }
                    }
                }
            }
        }

        let resp = dispatch(&app, req).await;
        write_frame_async(&mut stream, &resp).await?;
    }
}

async fn dispatch(app: &Arc<Mutex<App>>, req: Request) -> Response {
    match req.method.as_str() {
        METHOD_PING => Response::result(req.id, quota_core::protocol::Pong { pong: true }),
        METHOD_VERSION => Response::result(req.id, VersionInfo::current()),
        METHOD_STATUS => {
            let params: StatusParams = serde_json::from_value(req.params).unwrap_or_default();
            let g = app.lock().await;
            let snap = filter_snapshot(g.snapshot_or_empty(), params.provider);
            Response::result(req.id, StatusResult { snapshot: snap })
        }
        METHOD_PACE => {
            let params: PaceParams = serde_json::from_value(req.params).unwrap_or_default();
            let g = app.lock().await;
            let history = g.store.history();
            let latest = g.snapshot_or_empty();
            let mut reports = Vec::new();
            for p in latest
                .providers
                .iter()
                .filter(|p| params.provider.matches(p.provider))
            {
                reports.push(pace_for(&history, p));
            }
            Response::result(req.id, PaceResult { reports })
        }
        METHOD_CAN_START => {
            let params: CanStartParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => {
                    return Response::err(req.id, "bad_params", format!("can_start: {e}"));
                }
            };
            let g = app.lock().await;
            let history = g.store.history();
            let latest = g.snapshot_or_empty();
            let now = now_unix();
            let mut answers = Vec::new();
            for p in latest
                .providers
                .iter()
                .filter(|p| params.provider.matches(p.provider))
            {
                answers.push(can_start(p, &history, params.tokens, params.deadline, now));
            }
            let available: Vec<_> = answers
                .iter()
                .filter(|a| a.basis != quota_core::types::CanStartBasis::Unavailable)
                .collect();
            let ok = !available.is_empty() && available.iter().all(|a| a.ok);
            Response::result(req.id, CanStartResult { ok, answers })
        }
        METHOD_REFRESH => {
            let params: RefreshParams = serde_json::from_value(req.params).unwrap_or_default();
            refresh(app.clone()).await;
            let g = app.lock().await;
            let snap = filter_snapshot(g.snapshot_or_empty(), params.provider);
            Response::result(req.id, StatusResult { snapshot: snap })
        }
        METHOD_ACCOUNTS_LIST => {
            let g = app.lock().await;
            Response::result(req.id, g.accounts.list())
        }
        METHOD_ACCOUNTS_ADD => {
            let params: AccountsAddParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => return Response::err(req.id, "bad_params", format!("accounts.add: {e}")),
            };
            let mut g = app.lock().await;
            match g.accounts.add(params) {
                Ok(account) => {
                    let active_id = g.accounts.list().active_id;
                    Response::result(req.id, AccountMutationResult { account, active_id })
                }
                Err(e) => Response::err(req.id, "accounts", e),
            }
        }
        METHOD_ACCOUNTS_REMOVE => {
            let params: AccountsRemoveParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => {
                    return Response::err(req.id, "bad_params", format!("accounts.remove: {e}"));
                }
            };
            let mut g = app.lock().await;
            match g.accounts.remove(&params.id) {
                Ok(true) => Response::result(req.id, g.accounts.list()),
                Ok(false) => {
                    Response::err(req.id, "not_found", format!("no account {}", params.id))
                }
                Err(e) => Response::err(req.id, "accounts", e),
            }
        }
        METHOD_ACCOUNTS_SELECT => {
            let params: AccountsSelectParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => {
                    return Response::err(req.id, "bad_params", format!("accounts.select: {e}"));
                }
            };
            let mut g = app.lock().await;
            match g.accounts.select(params.id) {
                Ok(_) => Response::result(req.id, g.accounts.list()),
                Err(e) => Response::err(req.id, "accounts", e),
            }
        }
        unknown => Response {
            id: req.id,
            ok: false,
            result: None,
            error: Some(ErrorBody {
                code: "unknown_method".into(),
                message: format!("unknown method {unknown}"),
            }),
        },
    }
}

fn filter_snapshot(mut snap: Snapshot, filter: ProviderFilter) -> Snapshot {
    if filter == ProviderFilter::All {
        return snap;
    }
    snap.providers.retain(|p| filter.matches(p.provider));
    snap
}

async fn read_frame_async(stream: &mut UnixStream) -> Result<Vec<u8>, FrameError> {
    let mut header = [0u8; 4];
    match stream.read_exact(&mut header).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            return Err(FrameError::UnexpectedEof);
        }
        Err(e) => return Err(FrameError::Io(e.to_string())),
    }
    let n = decode_len(header)?;
    let mut buf = vec![0u8; n];
    stream
        .read_exact(&mut buf)
        .await
        .map_err(|e| FrameError::Io(e.to_string()))?;
    Ok(buf)
}

async fn write_frame_async(stream: &mut UnixStream, resp: &Response) -> Result<(), ClientError> {
    let json = serde_json::to_vec(resp).map_err(|e| ClientError::Other(e.to_string()))?;
    let frame = encode_frame(&json)?;
    stream
        .write_all(&frame)
        .await
        .map_err(|e| ClientError::Other(e.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use quota_core::protocol::{METHOD_PING, METHOD_VERSION};
    use quota_core::types::{AdapterError, ProviderId, ProviderSnapshot};

    fn test_app() -> App {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let accounts_path = std::env::temp_dir().join(format!("quota-test-acct-{stamp}.json"));
        let cfg = Config {
            history: false,
            ring_capacity: 16,
            accounts_path: Some(accounts_path.clone()),
            ..Config::default()
        };
        let (watch_tx, _) = broadcast::channel(4);
        let mut store = Store::new(&cfg);
        store.push(Snapshot::new(
            1_700_000_000,
            vec![ProviderSnapshot::unavailable(
                ProviderId::Codex,
                AdapterError::new("no_credentials", "missing"),
            )],
        ));
        App {
            interval_secs: 30,
            store,
            accounts: AccountStore::load(accounts_path),
            cfg,
            watch_tx,
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ping_and_version() {
        let app = Arc::new(Mutex::new(test_app()));
        let ping = dispatch(&app, Request::new(1, METHOD_PING)).await;
        assert!(ping.ok);
        let ver = dispatch(&app, Request::new(2, METHOD_VERSION)).await;
        assert!(ver.ok);
        let info: VersionInfo = serde_json::from_value(ver.result.unwrap()).unwrap();
        assert_eq!(info.protocol, quota_core::PROTOCOL_VERSION);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn status_filters_provider() {
        let app = Arc::new(Mutex::new(test_app()));
        let req = Request::with_params(
            3,
            METHOD_STATUS,
            StatusParams {
                provider: ProviderFilter::Claude,
            },
        );
        let resp = dispatch(&app, req).await;
        let result: StatusResult = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert!(result.snapshot.providers.is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unknown_method() {
        let app = Arc::new(Mutex::new(test_app()));
        let resp = dispatch(&app, Request::new(9, "explode")).await;
        assert!(!resp.ok);
        assert_eq!(resp.error.unwrap().code, "unknown_method");
    }

    #[test]
    fn usage_equality() {
        let a = Snapshot::new(1, vec![]);
        let b = Snapshot::new(2, vec![]);
        assert!(same_usage(&a, &b));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn accounts_add_list_remove() {
        let app = Arc::new(Mutex::new(test_app()));
        let add = Request::with_params(
            10,
            METHOD_ACCOUNTS_ADD,
            quota_core::AccountsAddParams {
                id: Some("acct_x".into()),
                provider: ProviderId::Codex,
                email: Some("openai@ctx.op0.dev".into()),
                workspace_label: Some("Personal".into()),
                login_method: Some("pro".into()),
                workspace_account_id: None,
                secret_ref: None,
                home_path: None,
                select: true,
            },
        );
        let resp = dispatch(&app, add).await;
        assert!(resp.ok);
        let listed = dispatch(&app, Request::new(11, METHOD_ACCOUNTS_LIST)).await;
        let book: quota_core::AccountsListResult =
            serde_json::from_value(listed.result.unwrap()).unwrap();
        assert_eq!(book.accounts.len(), 1);
        assert_eq!(book.active_id.as_deref(), Some("acct_x"));
        let rm = Request::with_params(
            12,
            METHOD_ACCOUNTS_REMOVE,
            quota_core::AccountsRemoveParams {
                id: "acct_x".into(),
            },
        );
        let gone = dispatch(&app, rm).await;
        assert!(gone.ok);
        let listed = dispatch(&app, Request::new(13, METHOD_ACCOUNTS_LIST)).await;
        let book: quota_core::AccountsListResult =
            serde_json::from_value(listed.result.unwrap()).unwrap();
        assert!(book.accounts.is_empty());
    }
}
