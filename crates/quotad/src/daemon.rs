//! Accept loop, adaptive refresh, and request dispatch.

use std::collections::HashMap;
use std::fs::{self, DirBuilder};
use std::os::fd::AsFd;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, PermissionsExt};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use quota_adapters::http::TlsTransport;
use quota_adapters::provider::{ProbeCtx, Provider};
use quota_adapters::{ClaudeAdapter, CodexAdapter};
use quota_core::framing::{decode_len, encode_frame, FrameError};
use quota_core::math::{can_start, can_start_percent, pace_for, DEFAULT_RESERVE_PERCENT};
use quota_core::protocol::{
    AccountMutationResult, AccountsAddParams, AccountsRemoveParams, AccountsSelectParams,
    CanStartParams, CanStartResult, ErrorBody, ObserveParams, ObserveResult, PaceParams,
    PaceResult, ProviderFilter, RefreshParams, Request, Response, StatusParams, StatusResult,
    VersionInfo, WatchParams, METHOD_ACCOUNTS_ADD, METHOD_ACCOUNTS_LIST, METHOD_ACCOUNTS_REMOVE,
    METHOD_ACCOUNTS_SELECT, METHOD_CAN_START, METHOD_OBSERVE, METHOD_PACE, METHOD_PING,
    METHOD_REFRESH, METHOD_STATUS, METHOD_VERSION, METHOD_WATCH,
};
use quota_core::timeutil::now_unix;
use quota_core::types::{
    freshness_for, reading_answers_for_active_account, ActiveIdentity, AdapterError, Availability,
    Freshness, ProviderId, ProviderSnapshot, Snapshot, DEFAULT_READING_MAX_AGE_SECS,
    MAX_RETRY_AFTER_SECS,
};
use quota_core::Config;
use quota_source_cursor::CursorAdapter;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::unix::OwnedReadHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, Mutex, OwnedMutexGuard, RwLock, Semaphore};

/// Same-UID local DoS cap. Watchers cannot consume the RPC pool.
const MAX_RPC_CLIENTS: usize = 16;
const MAX_WATCH_CLIENTS: usize = 48;
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(15);
/// Drop a watch writer that is not draining snapshots (slowloris).
const WATCH_WRITE_TIMEOUT: Duration = Duration::from_secs(15);
/// Floor so a default `refresh_max` (300s) still drops a wedged silent slot.
const WATCH_IDLE_FLOOR_SECS: u64 = 600;
/// Slack past `refresh_max_secs` so a live configured refresh cannot race
/// the idle timer. Bundled `quota watch` has no heartbeat.
const WATCH_IDLE_SLACK_SECS: u64 = 30;

use crate::accounts::AccountStore;
use crate::observe::{snapshot_from_push, PUSH_RING_MIN_GAP_SECS};
use crate::store::Store;

pub fn run(cfg: Config) -> Result<(), DaemonError> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| DaemonError::Io(e.to_string()))?;
    rt.block_on(run_async(cfg))
}

/// Run a synchronous provider probe outside Tokio's blocking pool.
///
/// Dropping the receiver cancels observation, not the synchronous I/O. The
/// detached worker can finish independently, while runtime shutdown never
/// waits for a stalled provider request.
async fn probe_on_thread<T, F>(probe: F) -> Result<T, tokio::sync::oneshot::error::RecvError>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (send, receive) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let _ = send.send(probe());
    });
    receive.await
}

fn watch_idle_override_secs() -> Option<u64> {
    std::env::var("QUOTA_WATCH_IDLE_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&n| n >= 1)
}

/// Idle after subscribe: `max(600, refresh_max + 30)` unless
/// `QUOTA_WATCH_IDLE_SECS` is set. A live refresh interval cannot outlast
/// this timer. Override is for tests / operators.
fn watch_idle_secs(refresh_max_secs: u64, override_secs: Option<u64>) -> Duration {
    if let Some(n) = override_secs {
        return Duration::from_secs(n);
    }
    Duration::from_secs(
        refresh_max_secs
            .saturating_add(WATCH_IDLE_SLACK_SECS)
            .max(WATCH_IDLE_FLOOR_SECS),
    )
}

fn watch_idle_timeout(refresh_max_secs: u64) -> Duration {
    watch_idle_secs(refresh_max_secs, watch_idle_override_secs())
}

fn pid_alive(pid: u32) -> bool {
    let Ok(raw) = i32::try_from(pid) else {
        return false;
    };
    match rustix::process::Pid::from_raw(raw) {
        Some(p) => rustix::process::test_kill_process(p).is_ok(),
        None => false,
    }
}

/// Linux `SO_PEERCRED`: refuse a peer whose uid is not ours. Fail closed if
/// the sockopt fails. Other Unixes stay on the `0600` inode check only.
fn peer_is_same_uid<Fd: AsFd>(fd: Fd) -> bool {
    #[cfg(target_os = "linux")]
    {
        match rustix::net::sockopt::socket_peercred(fd) {
            Ok(cred) => cred.uid == rustix::process::geteuid(),
            Err(_) => false,
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = fd;
        true
    }
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
    /// Retry deadlines are keyed by provider so one refusal cannot pause the
    /// other provider's collector. Each names the account it was issued to
    /// and applies to no other.
    retry_after_until: HashMap<ProviderId, RetryAfter>,
    /// Each provider has its own single-flight gate and generation.
    refresh_gates: HashMap<ProviderId, Arc<RefreshGate>>,
    watch_tx: broadcast::Sender<Snapshot>,
    /// Receipt time of the newest push per provider. A current push outranks
    /// polling that provider.
    pushed_at: HashMap<ProviderId, i64>,
    /// When the ring last took a pushed snapshot, per provider.
    last_ring_push: HashMap<ProviderId, i64>,
    /// When a rate-limited-by-us provider (Cursor) was last probed.
    last_polled: HashMap<ProviderId, tokio::time::Instant>,
    /// Codex home when the active quota account names none. `None` resolves
    /// `$CODEX_HOME` or `~/.codex` at each read.
    codex_home: Option<PathBuf>,
    /// Claude config dir when the active quota account names none. `None`
    /// resolves `$CLAUDE_CONFIG_DIR` or `~/.claude` at each read.
    claude_home: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RetryAfterDeadline(tokio::time::Instant);

impl RetryAfterDeadline {
    /// Never more than [`MAX_RETRY_AFTER_SECS`] away, so every backoff ends.
    fn from_secs(seconds: u64) -> Self {
        Self(tokio::time::Instant::now() + Duration::from_secs(seconds.min(MAX_RETRY_AFTER_SECS)))
    }

    fn is_active(self, now: tokio::time::Instant) -> bool {
        now < self.0
    }
}

/// A provider's retry deadline and the account it was issued to: the quota
/// account scope and the provider account digest of the refused reading. It
/// is honoured, shown and kept only for that same account.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RetryAfter {
    scope: AccountScope,
    account_digest: Option<String>,
    until: RetryAfterDeadline,
    until_unix: i64,
}

impl RetryAfter {
    fn new(scope: AccountScope, account_digest: Option<String>, seconds: u64, now: i64) -> Self {
        let seconds = seconds.min(MAX_RETRY_AFTER_SECS);
        Self {
            scope,
            account_digest,
            until: RetryAfterDeadline::from_secs(seconds),
            until_unix: now.saturating_add(seconds as i64),
        }
    }

    fn belongs_to(&self, scope: &AccountScope, account_digest: Option<&str>) -> bool {
        self.scope == *scope && self.account_digest.as_deref() == account_digest
    }
}

/// The account a provider is probed as. A result, deadline or reading is only
/// valid for the scope it was collected under.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct AccountScope {
    id: Option<String>,
    home: Option<PathBuf>,
}

fn account_scope(g: &App, provider: ProviderId) -> AccountScope {
    match g.accounts.book().active() {
        Some(account) if account.provider == provider => AccountScope {
            id: Some(account.id.clone()),
            home: account.home_path.clone().map(PathBuf::from),
        },
        _ => AccountScope::default(),
    }
}

fn account_scopes(g: &App) -> HashMap<ProviderId, AccountScope> {
    [ProviderId::Codex, ProviderId::Claude, ProviderId::Cursor]
        .into_iter()
        .map(|provider| (provider, account_scope(g, provider)))
        .collect()
}

impl App {
    fn snapshot_or_empty(&self) -> Snapshot {
        self.store
            .latest()
            .cloned()
            .map(|snapshot| snapshot.refreshed_at(now_unix()))
            .unwrap_or_else(|| Snapshot::new(now_unix(), Vec::new()))
    }
}

/// Coalesces simultaneous refresh requests for one provider. The observation
/// generation and account scope are captured before waiting; a waiter reuses
/// a result only when it completed for that same account.
#[derive(Default)]
struct RefreshGate {
    /// Scope of the last completed refresh. Held for the whole probe.
    completed: Arc<Mutex<Option<AccountScope>>>,
    generation: AtomicU64,
}

impl RefreshGate {
    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Wait for any in-flight refresh. `None` when one completed for `scope`
    /// after `observed_generation`, so the caller can reuse its result.
    async fn begin(
        &self,
        observed_generation: u64,
        scope: &AccountScope,
    ) -> Option<OwnedMutexGuard<Option<AccountScope>>> {
        let guard = self.completed.clone().lock_owned().await;
        let covered = self.generation() > observed_generation && guard.as_ref() == Some(scope);
        (!covered).then_some(guard)
    }

    fn complete(&self, completed: &mut Option<AccountScope>, scope: AccountScope) {
        *completed = Some(scope);
        self.generation.fetch_add(1, Ordering::Release);
    }

    #[cfg(test)]
    async fn mark_complete(&self, scope: AccountScope) {
        let mut completed = self.completed.lock().await;
        self.complete(&mut completed, scope);
    }
}

async fn run_async(cfg: Config) -> Result<(), DaemonError> {
    let socket = cfg.socket_path();
    let _lock = acquire_instance_lock(&socket)?;
    prepare_socket(&socket)?;

    let listener = bind_private_socket(&socket)?;
    let rpc_slots = Arc::new(Semaphore::new(MAX_RPC_CLIENTS));
    let watch_slots = Arc::new(Semaphore::new(MAX_WATCH_CLIENTS));
    #[cfg(unix)]
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .map_err(|e| DaemonError::Io(format!("register SIGINT handler: {e}")))?;
    #[cfg(unix)]
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|e| DaemonError::Io(format!("register SIGTERM handler: {e}")))?;

    eprintln!(
        "quotad {} listening on {} (protocol {})",
        quota_core::PACKAGE_VERSION,
        socket.display(),
        quota_core::PROTOCOL_VERSION
    );

    let (watch_tx, _) = broadcast::channel(16);
    let accounts = AccountStore::load(cfg.accounts_file());
    let mut store = Store::new(&cfg);
    seed_codexbar_history(&mut store, &cfg);
    let app = Arc::new(RwLock::new(App {
        interval_secs: cfg.refresh_min_secs(),
        retry_after_until: HashMap::new(),
        refresh_gates: provider_refresh_gates(),
        store,
        accounts,
        cfg,
        watch_tx,
        pushed_at: HashMap::new(),
        last_ring_push: HashMap::new(),
        last_polled: HashMap::new(),
        codex_home: None,
        claude_home: None,
    }));

    // Do not await provider probes before entering the accept loop. Claude's
    // Keychain can show an approval prompt, so each provider refresh runs in
    // its own task and cannot hold the listener or another provider's cadence.
    let mut scheduled_refreshes = HashMap::new();
    for provider in [ProviderId::Codex, ProviderId::Claude, ProviderId::Cursor] {
        start_scheduled_refresh(&mut scheduled_refreshes, provider, || {
            tokio::spawn(refresh_one_provider(app.clone(), provider))
        })
        .await;
    }
    let first_refresh = {
        let g = app.read().await;
        Duration::from_secs(g.interval_secs)
    };
    let shutdown = async {
        tokio::select! {
            _ = interrupt.recv() => eprintln!("quotad: shutting down"),
            _ = terminate.recv() => eprintln!("quotad: received SIGTERM, shutting down"),
        }
    };
    let refresh_app = app.clone();
    let scheduled_refreshes = Arc::new(Mutex::new(scheduled_refreshes));
    let refresh_slots = scheduled_refreshes.clone();
    accept_loop(
        listener,
        app,
        rpc_slots,
        watch_slots,
        first_refresh,
        shutdown,
        move || {
            let refresh_app = refresh_app.clone();
            let refresh_slots = refresh_slots.clone();
            async move {
                let mut scheduled_refreshes = refresh_slots.lock().await;
                // Keep refresh cadence independent of socket traffic. Provider
                // probes run in their own tasks, so a stalled Keychain prompt or
                // network request cannot hold this accept loop.
                for provider in [ProviderId::Codex, ProviderId::Claude, ProviderId::Cursor] {
                    start_scheduled_refresh(&mut scheduled_refreshes, provider, || {
                        tokio::spawn(refresh_one_provider(refresh_app.clone(), provider))
                    })
                    .await;
                }
            }
        },
    )
    .await;
    let mut scheduled_refreshes = scheduled_refreshes.lock().await;
    for task in scheduled_refreshes.drain().map(|(_, task)| task) {
        task.abort();
    }
    let _ = fs::remove_file(&socket);
    Ok(())
}

/// Accept requests and run scheduled refreshes against one persistent timer.
/// Re-creating a sleep after every accepted connection lets continuous traffic
/// postpone the next refresh forever.
async fn accept_loop<S, F, Fut>(
    listener: UnixListener,
    app: Arc<RwLock<App>>,
    rpc_slots: Arc<Semaphore>,
    watch_slots: Arc<Semaphore>,
    first_refresh: Duration,
    shutdown: S,
    mut refresh: F,
) where
    S: std::future::Future<Output = ()>,
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    tokio::pin!(shutdown);
    let mut refresh_timer = Box::pin(tokio::time::sleep(first_refresh));
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        let app = app.clone();
                        let rpc_slots = rpc_slots.clone();
                        let watch_slots = watch_slots.clone();
                        tokio::spawn(async move {
                            if let Err(e) =
                                handle_client(app, stream, rpc_slots, watch_slots).await
                            {
                                if !matches!(e, ClientError::Eof) {
                                    eprintln!("quotad: client: {e}");
                                }
                            }
                        });
                    }
                    Err(e) => eprintln!("quotad: accept: {e}"),
                }
            }
            _ = &mut refresh_timer => {
                refresh().await;
                let wait = {
                    let g = app.read().await;
                    Duration::from_secs(g.interval_secs)
                };
                refresh_timer
                    .as_mut()
                    .reset(tokio::time::Instant::now() + wait);
            }
        }
    }
}

/// Start a scheduled refresh unless the previous one is still running. A task
/// that finished at any point since the last tick is reaped here, so it can
/// never suppress the next probe.
async fn start_scheduled_refresh<S>(
    slots: &mut HashMap<ProviderId, tokio::task::JoinHandle<()>>,
    provider: ProviderId,
    start: S,
) where
    S: FnOnce() -> tokio::task::JoinHandle<()>,
{
    if slots.get(&provider).is_some_and(|task| !task.is_finished()) {
        return;
    }
    if let Some(task) = slots.remove(&provider) {
        if let Err(error) = task.await {
            eprintln!("quotad: scheduled refresh task failed: {error}");
        }
    }
    slots.insert(provider, start());
}

fn seed_codexbar_history(store: &mut Store, cfg: &Config) {
    if !cfg.enable_codexbar_files {
        return;
    }
    let snaps = quota_adapters::codexbar::load_history_snapshots_from_dir(
        &cfg.codexbar_dir(),
        cfg.ring_capacity(),
    );
    for snap in snaps {
        store.push_memory(snap);
    }
}

/// Directory lock so two startups cannot both pass the stale-socket check
/// and unlink each other’s live bind. `create_dir` is atomic; a leftover
/// dir from a crash is removed when its pid is no longer alive.
#[derive(Debug)]
struct InstanceLock {
    dir: PathBuf,
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn claim_lock_dir(dir: &Path) -> bool {
    let pid_path = dir.join("pid");
    let me = std::process::id().to_string();
    if fs::write(&pid_path, format!("{me}\n")).is_err() {
        return false;
    }
    fs::read_to_string(&pid_path)
        .ok()
        .is_some_and(|got| got.trim() == me)
}

fn lock_held_by_live_pid(dir: &Path) -> bool {
    match fs::read_to_string(dir.join("pid")) {
        Ok(raw) => raw.trim().parse::<u32>().ok().is_some_and(pid_alive),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let fresh = fs::metadata(dir)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_none_or(|age| age < Duration::from_secs(2));
            if fresh {
                std::thread::sleep(Duration::from_millis(50));
                if let Ok(raw) = fs::read_to_string(dir.join("pid")) {
                    return raw.trim().parse::<u32>().ok().is_some_and(pid_alive);
                }
            }
            false
        }
        Err(_) => true,
    }
}

fn acquire_instance_lock(socket: &Path) -> Result<InstanceLock, DaemonError> {
    if let Some(dir) = socket.parent() {
        quota_core::ensure_private_dir(dir).map_err(|e| DaemonError::Io(e.to_string()))?;
    }
    let dir = socket.with_extension("lock");
    for _ in 0..8 {
        match DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => {
                if claim_lock_dir(&dir) {
                    return Ok(InstanceLock { dir });
                }
                let _ = fs::remove_dir_all(&dir);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if lock_held_by_live_pid(&dir) {
                    return Err(DaemonError::Io(format!(
                        "already running (lock held at {})",
                        dir.display()
                    )));
                }
                let _ = fs::remove_dir_all(&dir);
            }
            Err(e) => return Err(DaemonError::Io(e.to_string())),
        }
    }
    Err(DaemonError::Io(format!(
        "could not acquire instance lock at {}",
        dir.display()
    )))
}

/// Bind with umask `0177` so the inode is created mode `0600`, then chmod
/// again and refuse to listen if group/other bits remain.
fn bind_private_socket(path: &Path) -> Result<UnixListener, DaemonError> {
    let listener = {
        let previous = rustix::process::umask(
            rustix::fs::Mode::XUSR | rustix::fs::Mode::RWXG | rustix::fs::Mode::RWXO,
        );
        let _restore = UmaskGuard(previous);
        UnixListener::bind(path).map_err(|e| DaemonError::Io(e.to_string()))?
    };
    let mut perms = fs::metadata(path)
        .map_err(|e| DaemonError::Io(e.to_string()))?
        .permissions();
    perms.set_mode(0o600);
    fs::set_permissions(path, perms)
        .map_err(|e| DaemonError::Io(format!("chmod 0600 {}: {e}", path.display())))?;
    let mode = fs::metadata(path)
        .map_err(|e| DaemonError::Io(e.to_string()))?
        .permissions()
        .mode()
        & 0o777;
    if mode & 0o077 != 0 {
        return Err(DaemonError::Io(format!(
            "socket {} mode is {mode:o}; refusing to listen",
            path.display()
        )));
    }
    Ok(listener)
}

struct UmaskGuard(rustix::fs::Mode);

impl Drop for UmaskGuard {
    fn drop(&mut self) {
        rustix::process::umask(self.0);
    }
}

fn prepare_socket(path: &Path) -> Result<(), DaemonError> {
    if let Some(dir) = path.parent() {
        quota_core::ensure_private_dir(dir).map_err(|e| DaemonError::Io(e.to_string()))?;
    }
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(DaemonError::Io(format!(
                    "socket path {} is a symlink; refuse to bind",
                    path.display()
                )));
            }
            if meta.file_type().is_socket() {
                if StdUnixStream::connect(path).is_ok() {
                    return Err(DaemonError::Io(format!(
                        "already running (live socket at {})",
                        path.display()
                    )));
                }
                fs::remove_file(path).map_err(|e| DaemonError::Io(e.to_string()))?;
                return Ok(());
            }
            return Err(DaemonError::Io(format!(
                "socket path {} exists and is not a unix socket",
                path.display()
            )));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(DaemonError::Io(e.to_string())),
    }
    Ok(())
}

#[derive(Clone)]
struct ProbePlan {
    timeout: u64,
    cursor_secret_path: String,
    codex_home: Option<PathBuf>,
    claude_home: Option<PathBuf>,
    codexbar_dir: PathBuf,
    enable_codexbar_files: bool,
}

fn probe_plan(g: &App) -> ProbePlan {
    ProbePlan {
        timeout: g.cfg.http_timeout_secs,
        cursor_secret_path: g.cfg.cursor_secret_path.clone(),
        codex_home: account_scope(g, ProviderId::Codex)
            .home
            .or_else(|| g.codex_home.clone()),
        claude_home: account_scope(g, ProviderId::Claude)
            .home
            .or_else(|| g.claude_home.clone()),
        codexbar_dir: g.cfg.codexbar_dir(),
        enable_codexbar_files: g.cfg.enable_codexbar_files,
    }
}

fn provider_refresh_gates() -> HashMap<ProviderId, Arc<RefreshGate>> {
    [ProviderId::Codex, ProviderId::Claude, ProviderId::Cursor]
        .into_iter()
        .map(|provider| (provider, Arc::new(RefreshGate::default())))
        .collect()
}

fn refresh_interval_secs(any_ok: bool, changed: bool, current: u64, min: u64, max: u64) -> u64 {
    if !any_ok {
        current.saturating_mul(2).clamp(min, max)
    } else if changed {
        min
    } else {
        (current.saturating_mul(3) / 2).clamp(min, max)
    }
}

fn collect_provider(plan: ProbePlan, provider: ProviderId) -> ProviderSnapshot {
    let transport = TlsTransport::new(Duration::from_secs(plan.timeout));
    let ctx = ProbeCtx {
        transport: &transport,
        now: now_unix(),
    };
    match provider {
        ProviderId::Codex => codex_adapter(plan).probe(&ctx),
        ProviderId::Claude => ClaudeAdapter::for_account(plan.claude_home).probe(&ctx),
        ProviderId::Cursor => {
            match CursorAdapter::from_keychain_first_chain(plan.cursor_secret_path) {
                Ok(adapter) => adapter.probe(&ctx),
                Err(error) => ProviderSnapshot::unavailable(
                    ProviderId::Cursor,
                    AdapterError::new("secrets_config", error.to_string()),
                ),
            }
        }
    }
}

fn codex_adapter(plan: ProbePlan) -> CodexAdapter {
    CodexAdapter {
        home: plan.codex_home,
        codexbar_dir: Some(plan.codexbar_dir),
        enable_codexbar_files: plan.enable_codexbar_files,
    }
}

/// What a Codex probe found while an HTTP retry deadline stands.
enum BackoffProbe {
    /// The local credentials still read as the deadline's account, so only
    /// the CodexBar file was read. `None` when no file matches.
    Owner(Option<Box<ProviderSnapshot>>),
    /// The local credentials now read as another account. The deadline does
    /// not apply to it; that account is probed in full.
    OtherAccount,
}

/// During Codex HTTP backoff, read only the CodexBar file for the account the
/// local credentials name, if that is `owner`, the deadline's account. The
/// account is the file reading's, or the credential's when no file matches.
/// `stored` is the current reading; its refusal carries over only for the
/// same account.
fn collect_codex_in_backoff(
    plan: ProbePlan,
    stored: Option<ProviderSnapshot>,
    owner: Option<&str>,
) -> BackoffProbe {
    let adapter = codex_adapter(plan);
    let digest = adapter.active_identity().digest().map(str::to_owned);
    let mut skipped = ProviderSnapshot::unavailable(
        ProviderId::Codex,
        AdapterError::new("rate_limited", "HTTP probe skipped during provider backoff"),
    );
    skipped.permission = stored
        .filter(|stored| digest.is_some() && stored.account_digest == digest)
        .map(|stored| stored.permission)
        .unwrap_or_default();
    skipped.account_digest = digest.clone();
    let file = adapter.with_file_fallback(skipped);
    let file = (file.source == Some(quota_core::types::Source::File)).then_some(file);
    let account = file
        .as_ref()
        .map_or(digest.as_deref(), |file| file.account_digest.as_deref());
    if account != owner {
        return BackoffProbe::OtherAccount;
    }
    BackoffProbe::Owner(file.map(Box::new))
}

fn apply_snapshot_locked(g: &mut App, snap: Snapshot) {
    let changed = match g.store.latest() {
        Some(prev) => !same_usage(prev, &snap),
        None => true,
    };
    let any_ok = snap.providers.iter().any(|p| p.status == Availability::Ok);
    let min_s = g.cfg.refresh_min_secs();
    let max_s = g.cfg.refresh_max_secs();
    // Adaptive refresh: stay faster while numbers move; idle longer when stable
    // or unavailable so we do not hammer undocumented endpoints.
    g.interval_secs = refresh_interval_secs(any_ok, changed, g.interval_secs, min_s, max_s);
    publish_locked(g, snap);
}

fn publish_locked(g: &mut App, snap: Snapshot) {
    let _ = g.watch_tx.send(snap.clone());
    g.store.push(snap);
}

/// A 429 that names no `Retry-After` still backs that provider off, for the
/// longest interval the scheduler would ever wait.
fn ensure_rate_limit_backoff(g: &App, provider: &mut ProviderSnapshot) {
    let rate_limited = provider
        .error
        .as_ref()
        .is_some_and(|error| error.code == "rate_limited");
    // A stated `Retry-After: 0` (or a past date) means probe now.
    if rate_limited && provider.retry_after_secs.is_none() {
        provider.retry_after_secs = Some(g.cfg.refresh_max_secs());
    }
}

/// Record the deadline `provider` states for the account it was read as.
fn record_retry_after(g: &mut App, scope: AccountScope, provider: &ProviderSnapshot, now: i64) {
    // A provider's Retry-After affects only that provider's next probe. It
    // never lengthens the shared scheduler interval or another source's gate.
    match provider.retry_after_secs.filter(|seconds| *seconds > 0) {
        Some(seconds) => {
            let deadline = RetryAfter::new(scope, provider.account_digest.clone(), seconds, now);
            g.retry_after_until.insert(provider.provider, deadline);
        }
        None => {
            g.retry_after_until.remove(&provider.provider);
        }
    }
}

/// Whether a deadline issued under the active account scope still stands.
/// Whether the credentials still name its provider account is decided by the
/// probe, which reads them ([`collect_codex_in_backoff`]).
async fn provider_in_backoff(app: &Arc<RwLock<App>>, provider: ProviderId) -> bool {
    let g = app.read().await;
    let scope = account_scope(&g, provider);
    g.retry_after_until.get(&provider).is_some_and(|deadline| {
        deadline.scope == scope && deadline.until.is_active(tokio::time::Instant::now())
    })
}

/// How a provider is probed this cycle. During HTTP backoff a provider with a
/// local file source is still read from that file; the network is skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeMode {
    Full,
    FileOnly,
}

/// Codex is still probed during its HTTP backoff: the probe reads its local
/// credentials, reads only the CodexBar file while they name the deadline's
/// account, and probes in full once they name another.
fn probed_during_backoff(provider: ProviderId) -> bool {
    provider == ProviderId::Codex
}

/// A provider's gate, held from before the probe until its result is
/// published or discarded.
struct Claim {
    provider: ProviderId,
    gate: Arc<RefreshGate>,
    guard: OwnedMutexGuard<Option<AccountScope>>,
}

struct Probed {
    claim: Claim,
    scope: AccountScope,
    mode: ProbeMode,
    snapshot: ProviderSnapshot,
}

/// A provider's gate as a refresh request found it, before waiting on it.
struct Observed {
    provider: ProviderId,
    gate: Arc<RefreshGate>,
    scope: AccountScope,
    generation: u64,
}

fn observe(g: &App, provider: ProviderId) -> Observed {
    let gate = g
        .refresh_gates
        .get(&provider)
        .expect("gate for each provider")
        .clone();
    Observed {
        provider,
        generation: gate.generation(),
        scope: account_scope(g, provider),
        gate,
    }
}

impl Observed {
    /// `None` when a refresh for the same account completed after this
    /// observation, so its result already answers the request.
    async fn claim(self) -> Option<Claim> {
        let guard = self.gate.begin(self.generation, &self.scope).await?;
        Some(Claim {
            provider: self.provider,
            gate: self.gate,
            guard,
        })
    }
}

async fn probe_claimed<F, Fut>(app: &Arc<RwLock<App>>, claim: Claim, probe: F) -> Option<Probed>
where
    F: FnOnce(Arc<RwLock<App>>, ProbeMode) -> Fut,
    Fut: std::future::Future<Output = Option<ProviderSnapshot>>,
{
    let provider = claim.provider;
    let in_backoff = provider_in_backoff(app, provider).await;
    let (scope, mode) = {
        let g = app.read().await;
        let mode = match (in_backoff, probed_during_backoff(provider)) {
            (false, _) => ProbeMode::Full,
            (true, true) => ProbeMode::FileOnly,
            (true, false) => return None,
        };
        (account_scope(&g, provider), mode)
    };
    let snapshot = probe(app.clone(), mode).await?;
    if snapshot.provider != provider {
        eprintln!("quotad: {provider} refresh returned {}", snapshot.provider);
        return None;
    }
    Some(Probed {
        claim,
        scope,
        mode,
        snapshot,
    })
}

/// Publish one refresh cycle as a single history entry and watch update.
/// A result whose account is no longer active is discarded, and its gate is
/// released without completing so a waiter probes the new account.
async fn publish(app: &Arc<RwLock<App>>, probed: Vec<Probed>) {
    let now = now_unix();
    let mut g = app.write().await;
    let mut providers = g
        .store
        .latest()
        .map(|snapshot| snapshot.refreshed_at(now).providers)
        .unwrap_or_default();
    let mut applied = false;
    for Probed {
        mut claim,
        scope,
        mode,
        mut snapshot,
    } in probed
    {
        let provider = claim.provider;
        if account_scope(&g, provider) != scope {
            eprintln!(
                "quotad: discarding {provider} result for an account that is no longer active"
            );
            continue;
        }
        let held = g
            .retry_after_until
            .get(&provider)
            .filter(|deadline| deadline.belongs_to(&scope, snapshot.account_digest.as_deref()))
            .map(|deadline| deadline.until_unix);
        match (mode, held) {
            (ProbeMode::FileOnly, Some(until)) => {
                snapshot.retry_after_secs = None;
                snapshot.retry_after_until = Some(until);
            }
            _ => {
                ensure_rate_limit_backoff(&g, &mut snapshot);
                record_retry_after(&mut g, scope.clone(), &snapshot, now);
            }
        }
        if provider == ProviderId::Cursor {
            g.last_polled.insert(provider, tokio::time::Instant::now());
        }
        if provider == ProviderId::Claude && push_is_current(&g, provider, now) {
            // Keep the pushed measurements, but retain compatible OAuth-only
            // windows from this completed probe. Dropping the whole result
            // here loses Opus/Sonnet limits when the push arrived mid-probe.
            if let Some(pushed) = providers
                .iter_mut()
                .find(|existing| existing.provider == provider)
            {
                if merge_oauth_windows_during_push(pushed, &snapshot, now) {
                    applied = true;
                }
            }
            claim.gate.complete(&mut claim.guard, scope);
            continue;
        }
        match providers
            .iter_mut()
            .find(|existing| existing.provider == provider)
        {
            Some(existing) => *existing = snapshot,
            None => providers.push(snapshot),
        }
        claim.gate.complete(&mut claim.guard, scope);
        applied = true;
    }
    if applied {
        apply_snapshot_locked(&mut g, Snapshot::new(now, providers).refreshed_at(now));
    }
}

async fn probe_provider(
    app: Arc<RwLock<App>>,
    provider: ProviderId,
    mode: ProbeMode,
) -> Option<ProviderSnapshot> {
    probe_provider_with(app, provider, mode, collect_provider).await
}

/// [`probe_provider`] with the full (network) probe supplied by the caller.
async fn probe_provider_with<P>(
    app: Arc<RwLock<App>>,
    provider: ProviderId,
    mode: ProbeMode,
    full: P,
) -> Option<ProviderSnapshot>
where
    P: FnOnce(ProbePlan, ProviderId) -> ProviderSnapshot + Send + 'static,
{
    let (plan, stored, owner) = {
        let g = app.read().await;
        let enabled = match provider {
            ProviderId::Codex => g.cfg.enable_codex,
            ProviderId::Claude => g.cfg.enable_claude,
            ProviderId::Cursor => g.cfg.enable_cursor,
        };
        if !enabled {
            return None;
        }
        let stored = g
            .store
            .latest()
            .and_then(|snapshot| snapshot.by_id(provider))
            .cloned();
        let owner = g
            .retry_after_until
            .get(&provider)
            .map(|deadline| deadline.account_digest.clone());
        (probe_plan(&g), stored, owner)
    };
    let result = match (mode, owner) {
        (ProbeMode::FileOnly, Some(owner)) => {
            probe_on_thread(move || {
                match collect_codex_in_backoff(plan.clone(), stored, owner.as_deref()) {
                    BackoffProbe::Owner(file) => file.map(|file| *file),
                    BackoffProbe::OtherAccount => Some(full(plan, provider)),
                }
            })
            .await
        }
        _ => probe_on_thread(move || Some(full(plan, provider))).await,
    };
    result.unwrap_or_else(|error| {
        eprintln!("quotad: {provider} refresh task failed: {error}");
        None
    })
}

/// How long a finished provider waits for the rest of its cycle before it is
/// published alone. Providers that finish close together still share one
/// history entry and one watch update; a stalled one cannot withhold the rest.
const PUBLISH_GRACE: Duration = Duration::from_secs(2);

/// Probe every enabled provider concurrently and publish them together.
/// Each provider waits on its own gate in its own future, so one provider's
/// in-flight probe cannot delay the others from starting.
async fn refresh_providers<F, Fut>(app: Arc<RwLock<App>>, providers: &[ProviderId], probe: F)
where
    F: Fn(Arc<RwLock<App>>, ProviderId, ProbeMode) -> Fut,
    Fut: std::future::Future<Output = Option<ProviderSnapshot>>,
{
    refresh_providers_within(app, providers, PUBLISH_GRACE, probe).await;
}

/// [`refresh_providers`] with the grace a finished provider waits for the
/// others. A provider still probing after it publishes on its own when done.
async fn refresh_providers_within<F, Fut>(
    app: Arc<RwLock<App>>,
    providers: &[ProviderId],
    grace: Duration,
    probe: F,
) where
    F: Fn(Arc<RwLock<App>>, ProviderId, ProbeMode) -> Fut,
    Fut: std::future::Future<Output = Option<ProviderSnapshot>>,
{
    let observed: Vec<Observed> = {
        let g = app.read().await;
        providers
            .iter()
            .map(|provider| observe(&g, *provider))
            .collect()
    };
    debug_assert!(observed.len() <= 3, "there are only three quota providers");
    let (finished, mut arrivals) = tokio::sync::mpsc::unbounded_channel();
    let probing = async {
        let probe_one = |observed: Option<Observed>| {
            let app = &app;
            let probe = &probe;
            let finished = &finished;
            async move {
                let Some(observed) = observed else {
                    return;
                };
                let Some(claim) = observed.claim().await else {
                    return;
                };
                let provider = claim.provider;
                let probed =
                    probe_claimed(app, claim, |app, mode| probe(app, provider, mode)).await;
                if let Some(probed) = probed {
                    let _ = finished.send(probed);
                }
            }
        };
        let mut observed = observed.into_iter();
        tokio::join!(
            probe_one(observed.next()),
            probe_one(observed.next()),
            probe_one(observed.next())
        );
        drop(finished);
    };
    let publishing = async {
        while let Some(first) = arrivals.recv().await {
            let mut batch = vec![first];
            let deadline = tokio::time::Instant::now() + grace;
            while let Ok(Some(next)) = tokio::time::timeout_at(deadline, arrivals.recv()).await {
                batch.push(next);
            }
            publish(&app, batch).await;
        }
    };
    tokio::join!(probing, publishing);
}

/// Providers that are disabled, in backoff, or already covered by fresher
/// evidence return without probing.
async fn refresh(app: Arc<RwLock<App>>) {
    let providers: Vec<ProviderId> = {
        let g = app.read().await;
        [
            (ProviderId::Codex, g.cfg.enable_codex),
            (ProviderId::Claude, g.cfg.enable_claude),
            (ProviderId::Cursor, g.cfg.enable_cursor),
        ]
        .into_iter()
        .filter_map(|(provider, enabled)| (enabled && poll_due(&g, provider)).then_some(provider))
        .collect()
    };
    refresh_providers(app, &providers, probe_provider).await;
}

/// The scheduler owns one task per provider. A stalled probe keeps only its
/// own slot occupied; healthy providers continue on each scheduled cycle.
async fn refresh_one_provider(app: Arc<RwLock<App>>, provider: ProviderId) {
    let due = {
        let g = app.read().await;
        let enabled = match provider {
            ProviderId::Codex => g.cfg.enable_codex,
            ProviderId::Claude => g.cfg.enable_claude,
            ProviderId::Cursor => g.cfg.enable_cursor,
        };
        enabled && poll_due(&g, provider)
    };
    if due {
        refresh_providers(app, &[provider], probe_provider).await;
    }
}

/// A current statusline push already answers for Claude. Cursor's dashboard
/// route is polled no faster than its own floor.
fn poll_due(g: &App, provider: ProviderId) -> bool {
    match provider {
        ProviderId::Claude => !push_is_current(g, provider, now_unix()),
        ProviderId::Cursor => g.last_polled.get(&provider).is_none_or(|at| {
            at.elapsed() >= Duration::from_secs(quota_source_cursor::MIN_POLL_SECS)
        }),
        ProviderId::Codex => true,
    }
}

/// After an account mutation, drop every reading, pace sample and deadline
/// that belonged to a provider's previous account so none is shown for, or
/// sampled as, the new one. History is dropped whether or not its readings
/// name a provider account: an unknown account is still the previous one.
fn invalidate_switched_accounts(g: &mut App, before: &HashMap<ProviderId, AccountScope>) {
    let switched: Vec<ProviderId> = account_scopes(g)
        .into_iter()
        .filter(|(provider, scope)| before.get(provider) != Some(scope))
        .map(|(provider, _)| provider)
        .collect();
    if switched.is_empty() {
        return;
    }
    let now = now_unix();
    let latest = g.store.latest().map(|latest| latest.refreshed_at(now));
    for provider in &switched {
        g.retry_after_until.remove(provider);
        g.store.forget(*provider);
        g.pushed_at.remove(provider);
        g.last_ring_push.remove(provider);
        g.last_polled.remove(provider);
    }
    let Some(mut snapshot) = latest else {
        return;
    };
    let mut replaced = false;
    for existing in &mut snapshot.providers {
        if switched.contains(&existing.provider) {
            *existing = ProviderSnapshot::unavailable(
                existing.provider,
                AdapterError::new(
                    "account_switched",
                    "active account changed; awaiting a refresh for the selected account",
                ),
            );
            replaced = true;
        }
    }
    if replaced {
        publish_locked(g, Snapshot::new(now, snapshot.providers));
    }
}

fn push_is_current(g: &App, provider: ProviderId, now: i64) -> bool {
    g.pushed_at.get(&provider).is_some_and(|at| {
        freshness_for(Some(*at), DEFAULT_READING_MAX_AGE_SECS, now) == Freshness::Current
    })
}

fn same_usage(a: &Snapshot, b: &Snapshot) -> bool {
    if a.providers.len() != b.providers.len() {
        return false;
    }
    a.providers
        .iter()
        .zip(b.providers.iter())
        .all(|(x, y)| same_provider_usage(x, y))
}

fn same_provider_usage(x: &ProviderSnapshot, y: &ProviderSnapshot) -> bool {
    x.provider == y.provider
        && x.status == y.status
        && x.permission == y.permission
        && x.source == y.source
        && x.windows.len() == y.windows.len()
        && x.windows.iter().zip(y.windows.iter()).all(|(a, b)| {
            a.kind == b.kind
                && a.state == b.state
                && a.reading == b.reading
                && a.reset_at == b.reset_at
        })
}

/// A statusline payload has no account identifier. Accept it only for the
/// default Claude scope, without an environment-selected isolated config dir.
fn push_matches_active_account(
    g: &App,
    provider: ProviderId,
    env_config_dir: Option<&str>,
) -> bool {
    provider == ProviderId::Claude
        && account_scope(g, provider) == AccountScope::default()
        && !ClaudeAdapter::has_isolated_config(None, env_config_dir)
}

/// Merge the completed OAuth probe's windows into a newer statusline reading.
/// The push owns measurements observed at or after its provider timestamp;
/// older OAuth-only measurements keep their own original timestamps and are
/// replaced only by a newer OAuth measurement for the same window.
fn merge_oauth_windows_during_push(
    pushed: &mut ProviderSnapshot,
    oauth: &ProviderSnapshot,
    now: i64,
) -> bool {
    if pushed.provider != ProviderId::Claude
        || oauth.provider != ProviderId::Claude
        || pushed.source != Some(quota_core::types::Source::Statusline)
        || oauth.source != Some(quota_core::types::Source::Oauth)
        || pushed.account_digest != oauth.account_digest
    {
        return false;
    }

    let pushed_observed_at = pushed.observed_at;
    let mut oauth = oauth.clone();
    oauth.refresh_freshness(now);
    let mut changed = false;
    for incoming in oauth.windows {
        match pushed
            .windows
            .iter_mut()
            .find(|current| current.kind == incoming.kind && current.label == incoming.label)
        {
            Some(current) => {
                let current_is_pushed = pushed_observed_at.is_some_and(|pushed_at| {
                    current
                        .observed_at
                        .is_some_and(|observed_at| observed_at >= pushed_at)
                });
                let incoming_is_newer = incoming
                    .observed_at
                    .zip(current.observed_at)
                    .is_some_and(|(incoming_at, current_at)| incoming_at > current_at);
                if !current_is_pushed && incoming_is_newer {
                    *current = incoming;
                    changed = true;
                }
            }
            None => {
                pushed.windows.push(incoming);
                changed = true;
            }
        }
    }
    if pushed.permission != oauth.permission
        && oauth.permission != quota_core::types::ProviderPermission::Unknown
    {
        pushed.permission = oauth.permission;
        changed = true;
    }
    if changed {
        pushed.refresh_freshness(now);
    }
    changed
}

/// Preserve current OAuth windows omitted by a statusline payload. Claude's
/// OAuth response can include distinct Opus and Sonnet weekly limits, while
/// the statusline reports only aggregate windows.
fn preserve_current_oauth_windows(g: &App, pushed: &mut ProviderSnapshot, now: i64) {
    let Some(mut oauth) = g
        .store
        .latest()
        .and_then(|snapshot| snapshot.by_id(ProviderId::Claude))
        .filter(|reading| {
            matches!(
                reading.source,
                Some(quota_core::types::Source::Oauth | quota_core::types::Source::Statusline)
            )
        })
        .cloned()
    else {
        return;
    };
    oauth.refresh_freshness(now);
    if oauth.freshness != Freshness::Current {
        return;
    }
    if oauth.permission != quota_core::types::ProviderPermission::Unknown {
        pushed.permission = oauth.permission;
    }
    for window in oauth.windows {
        if !pushed
            .windows
            .iter()
            .any(|current| current.kind == window.kind && current.label == window.label)
        {
            pushed.windows.push(window);
        }
    }
    pushed.refresh_freshness(now);
}

/// Fold one pushed provider snapshot into the latest snapshot. Unlike a poll
/// it leaves the adaptive refresh interval alone, and while the numbers repeat
/// it refreshes the newest ring entry in place so a chatty statusline cannot
/// push other providers' history out of the ring.
async fn apply_pushed_snapshot(
    app: &Arc<RwLock<App>>,
    provider: ProviderSnapshot,
    now: i64,
) -> bool {
    let env_config_dir = std::env::var("CLAUDE_CONFIG_DIR").ok();
    apply_pushed_snapshot_for_config(app, provider, now, env_config_dir.as_deref()).await
}

async fn apply_pushed_snapshot_for_config(
    app: &Arc<RwLock<App>>,
    mut provider: ProviderSnapshot,
    now: i64,
    env_config_dir: Option<&str>,
) -> bool {
    let id = provider.provider;
    let mut g = app.write().await;
    if !push_matches_active_account(&g, id, env_config_dir) {
        return false;
    }
    preserve_current_oauth_windows(&g, &mut provider, now);
    g.pushed_at.insert(id, now);
    let mut providers = g
        .store
        .latest()
        .map(|snapshot| snapshot.refreshed_at(now).providers)
        .unwrap_or_default();
    let unchanged = providers
        .iter()
        .find(|existing| existing.provider == id)
        .is_some_and(|existing| same_provider_usage(existing, &provider));
    match providers
        .iter_mut()
        .find(|existing| existing.provider == id)
    {
        Some(existing) => *existing = provider,
        None => providers.push(provider),
    }
    let snapshot = Snapshot::new(now, providers).refreshed_at(now);
    let ring_due = g
        .last_ring_push
        .get(&id)
        .is_none_or(|at| now.saturating_sub(*at) >= PUSH_RING_MIN_GAP_SECS);
    let _ = g.watch_tx.send(snapshot.clone());
    if unchanged && !ring_due {
        g.store.replace_latest(snapshot);
    } else {
        g.store.push(snapshot);
        g.last_ring_push.insert(id, now);
    }
    true
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

async fn read_frame_timed(stream: &mut UnixStream) -> Result<Vec<u8>, ClientError> {
    match tokio::time::timeout(FIRST_FRAME_TIMEOUT, read_frame_async(stream)).await {
        Ok(Ok(p)) => Ok(p),
        Ok(Err(FrameError::UnexpectedEof)) => Err(ClientError::Eof),
        Ok(Err(e)) => Err(e.into()),
        Err(_) => Err(ClientError::Other("idle handshake timeout".into())),
    }
}

async fn handle_client(
    app: Arc<RwLock<App>>,
    mut stream: UnixStream,
    rpc_slots: Arc<Semaphore>,
    watch_slots: Arc<Semaphore>,
) -> Result<(), ClientError> {
    if !peer_is_same_uid(&stream) {
        return Err(ClientError::Other("peer uid mismatch".into()));
    }
    let mut rpc_permit = None;
    loop {
        let payload = read_frame_timed(&mut stream).await?;
        let req: Request = serde_json::from_slice(&payload)
            .map_err(|e| ClientError::Other(format!("bad request: {e}")))?;

        if req.method == METHOD_WATCH {
            return serve_watch(app, stream, req, watch_slots).await;
        }

        if rpc_permit.is_none() {
            let p = match rpc_slots.clone().try_acquire_owned() {
                Ok(p) => p,
                Err(_) => {
                    write_frame_async(
                        &mut stream,
                        &Response::err(
                            req.id,
                            "too_many_clients",
                            format!("rpc client limit ({MAX_RPC_CLIENTS}) reached"),
                        ),
                    )
                    .await?;
                    return Ok(());
                }
            };
            rpc_permit = Some(p);
        }
        let Some(ref _rpc_held) = rpc_permit else {
            unreachable!("rpc permit set above");
        };

        let resp = dispatch(&app, req).await;
        write_frame_async(&mut stream, &resp).await?;
    }
}

async fn serve_watch(
    app: Arc<RwLock<App>>,
    mut stream: UnixStream,
    req: Request,
    watch_slots: Arc<Semaphore>,
) -> Result<(), ClientError> {
    let _watch_permit = match watch_slots.try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            write_frame_async(
                &mut stream,
                &Response::err(
                    req.id,
                    "too_many_watchers",
                    format!("watch client limit ({MAX_WATCH_CLIENTS}) reached"),
                ),
            )
            .await?;
            return Ok(());
        }
    };
    let params: WatchParams = serde_json::from_value(req.params.clone()).unwrap_or_default();
    let (mut rx, idle_dur, mut latest) = {
        let g = app.read().await;
        let snap = status_now(&g, g.store.latest(), params.provider);
        write_frame_timed(
            &mut stream,
            &Response::result(req.id, StatusResult { snapshot: snap }),
        )
        .await?;
        let idle_dur = watch_idle_timeout(g.cfg.refresh_max_secs());
        (g.watch_tx.subscribe(), idle_dur, g.store.latest().cloned())
    };
    let (reader, mut writer) = stream.into_split();
    let idle = tokio::time::sleep(idle_dur);
    tokio::pin!(idle);
    let expiry = tokio::time::sleep_until(next_expiry_deadline(latest.as_ref()));
    tokio::pin!(expiry);
    // One frame read lives across loop turns. A snapshot or expiry write in
    // between must not drop bytes already read from a partial client frame.
    let next_frame = read_frame_owned(reader);
    tokio::pin!(next_frame);
    loop {
        tokio::select! {
            next = rx.recv() => {
                match next {
                    Ok(snap) => {
                        let filtered = status_now(&*app.read().await, Some(&snap), params.provider);
                        latest = Some(snap);
                        write_frame_timed(
                            &mut writer,
                            &Response::result(req.id, StatusResult { snapshot: filtered }),
                        )
                        .await?;
                        idle.as_mut().reset(tokio::time::Instant::now() + idle_dur);
                        expiry.as_mut().reset(next_expiry_deadline(latest.as_ref()));
                    }
                    Err(_) => return Ok(()),
                }
            }
            _ = &mut expiry => {
                // Readings age out between refreshes; tell the watcher
                // the moment `current` stops being true.
                let aged = status_now(&*app.read().await, latest.as_ref(), params.provider);
                write_frame_timed(
                    &mut writer,
                    &Response::result(req.id, StatusResult { snapshot: aged }),
                )
                .await?;
                expiry.as_mut().reset(next_expiry_deadline(latest.as_ref()));
            }
            (reader, incoming) = &mut next_frame => {
                next_frame.set(read_frame_owned(reader));
                match incoming {
                    Ok(bytes) => {
                        // Only a documented `ping` keepalive resets idle.
                        // Junk / other methods must not hold the slot.
                        if let Ok(r) = serde_json::from_slice::<Request>(&bytes) {
                            if r.method == METHOD_PING {
                                write_frame_timed(
                                    &mut writer,
                                    &Response::result(r.id, quota_core::protocol::Pong { pong: true }),
                                )
                                .await?;
                                idle.as_mut().reset(tokio::time::Instant::now() + idle_dur);
                            }
                        }
                    }
                    Err(FrameError::UnexpectedEof) => return Err(ClientError::Eof),
                    Err(e) => return Err(e.into()),
                }
            }
            _ = &mut idle => {
                return Err(ClientError::Other("watch idle timeout".into()));
            }
        }
    }
}

async fn dispatch(app: &Arc<RwLock<App>>, req: Request) -> Response {
    match req.method.as_str() {
        METHOD_PING => Response::result(req.id, quota_core::protocol::Pong { pong: true }),
        METHOD_VERSION => Response::result(req.id, VersionInfo::current()),
        METHOD_STATUS => {
            let params: StatusParams = serde_json::from_value(req.params).unwrap_or_default();
            let g = app.read().await;
            let snap = status_now(&g, g.store.latest(), params.provider);
            Response::result(req.id, StatusResult { snapshot: snap })
        }
        METHOD_PACE => {
            let params: PaceParams = serde_json::from_value(req.params).unwrap_or_default();
            let g = app.read().await;
            let latest = answering_now(&g, g.snapshot_or_empty());
            let mut reports = Vec::new();
            for p in latest
                .providers
                .iter()
                .filter(|p| params.provider.matches(p.provider))
            {
                reports.push(pace_for(g.store.history_ref(), p));
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
            let request = match admission_request(&params) {
                Ok(request) => request,
                Err(message) => return Response::err(req.id, "bad_params", message),
            };
            let g = app.read().await;
            let latest = answering_now(&g, g.snapshot_or_empty());
            let now = now_unix();
            let mut answers = Vec::new();
            for p in latest
                .providers
                .iter()
                .filter(|p| params.provider.matches(p.provider))
            {
                let history = g.store.history_ref();
                answers.push(match request {
                    Admission::Tokens(tokens) => {
                        can_start(p, history, tokens, params.deadline, now)
                    }
                    Admission::Percent { percent, reserve } => {
                        can_start_percent(p, history, percent, reserve, params.deadline, now)
                    }
                });
            }
            let available: Vec<_> = answers
                .iter()
                .filter(|a| a.basis != quota_core::types::CanStartBasis::Unavailable)
                .collect();
            let ok = !available.is_empty() && available.iter().all(|a| a.ok);
            Response::result(req.id, CanStartResult { ok, answers })
        }
        METHOD_OBSERVE => {
            let params: ObserveParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => return Response::err(req.id, "bad_params", format!("observe: {e}")),
            };
            let now = now_unix();
            match snapshot_from_push(&params, now) {
                Ok(snapshot) => {
                    if apply_pushed_snapshot(app, snapshot, now).await {
                        Response::result(
                            req.id,
                            ObserveResult {
                                accepted: params.windows.len(),
                                observed_at: now,
                            },
                        )
                    } else {
                        Response::err(
                            req.id,
                            "account_scope",
                            "Claude statusline has no account identity; push refused while a quota account is selected",
                        )
                    }
                }
                Err(rejection) => Response::err(req.id, rejection.code, rejection.message),
            }
        }
        METHOD_REFRESH => {
            let params: RefreshParams = serde_json::from_value(req.params).unwrap_or_default();
            refresh(app.clone()).await;
            let g = app.read().await;
            let snap = status_now(&g, g.store.latest(), params.provider);
            Response::result(req.id, StatusResult { snapshot: snap })
        }
        METHOD_ACCOUNTS_LIST => {
            let g = app.read().await;
            Response::result(req.id, g.accounts.list())
        }
        METHOD_ACCOUNTS_ADD => {
            let params: AccountsAddParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => return Response::err(req.id, "bad_params", format!("accounts.add: {e}")),
            };
            let mut g = app.write().await;
            let before = account_scopes(&g);
            match g.accounts.add(params) {
                Ok(account) => {
                    invalidate_switched_accounts(&mut g, &before);
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
            let mut g = app.write().await;
            let before = account_scopes(&g);
            match g.accounts.remove(&params.id) {
                Ok(true) => {
                    invalidate_switched_accounts(&mut g, &before);
                    Response::result(req.id, g.accounts.list())
                }
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
            let mut g = app.write().await;
            let before = account_scopes(&g);
            match g.accounts.select(params.id) {
                Ok(_) => {
                    invalidate_switched_accounts(&mut g, &before);
                    Response::result(req.id, g.accounts.list())
                }
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

#[derive(Clone, Copy)]
enum Admission {
    Tokens(u64),
    Percent { percent: f64, reserve: f64 },
}

/// `tokens` and `percent` are separate questions; ask exactly one.
fn admission_request(params: &CanStartParams) -> Result<Admission, String> {
    let Some(percent) = params.percent else {
        if params.reserve.is_some() {
            return Err("can_start: reserve only applies to a percent request".into());
        }
        return Ok(Admission::Tokens(params.tokens));
    };
    if params.tokens > 0 {
        return Err("can_start: give tokens or percent, not both".into());
    }
    let reserve = params.reserve.unwrap_or(DEFAULT_RESERVE_PERCENT);
    if !(percent.is_finite() && percent > 0.0 && percent <= 100.0) {
        return Err("can_start: percent must be in (0, 100]".into());
    }
    if !(reserve.is_finite() && (0.0..100.0).contains(&reserve)) {
        return Err("can_start: reserve must be in [0, 100)".into());
    }
    Ok(Admission::Percent { percent, reserve })
}

/// When the newest reading stops being current; a year out when none will.
fn next_expiry_deadline(latest: Option<&Snapshot>) -> tokio::time::Instant {
    const NEVER: Duration = Duration::from_secs(365 * 24 * 3600);
    let wait = latest
        .and_then(|snapshot| snapshot.secs_until_next_expiry(now_unix()))
        .map(Duration::from_secs)
        .unwrap_or(NEVER);
    tokio::time::Instant::now() + wait
}

fn filter_snapshot(snap: Option<&Snapshot>, filter: ProviderFilter) -> Snapshot {
    let now = now_unix();
    match snap {
        Some(s) if filter == ProviderFilter::All => s.refreshed_at(now),
        Some(s) => Snapshot::new(
            s.fetched_at,
            s.providers
                .iter()
                .filter(|p| filter.matches(p.provider))
                .cloned()
                .map(|mut p| {
                    p.refresh_freshness(now);
                    p
                })
                .collect(),
        ),
        None => Snapshot::new(now_unix(), Vec::new()),
    }
}

/// `snapshot` as it may answer at this call. Each reading is compared with the
/// account its provider's credentials name now; one taken for another account
/// counts as unknown. Publication never decides this, so a credential change
/// at any moment before this call is seen here.
fn answering_now(g: &App, mut snapshot: Snapshot) -> Snapshot {
    for reading in &mut snapshot.providers {
        if !reading_answers_now(g, reading) {
            *reading = reading.for_another_account();
        }
    }
    snapshot
}

/// A placeholder tied to no account answers nothing, so it is shown as is.
/// Every other reading, an error one included, answers only for the account
/// it was taken for. Claude credentials name no provider account, so a Claude reading is bound
/// to its account only by the quota account scope, whose switch drops it, and
/// answers only while Claude credentials load.
fn reading_answers_now(g: &App, reading: &ProviderSnapshot) -> bool {
    if reading.is_unattributed_placeholder() {
        return true;
    }
    let taken_for = reading.account_digest.as_deref();
    let plan = probe_plan(g);
    match reading.provider {
        ProviderId::Codex => codex_adapter(plan).answers_for_active_account(taken_for),
        ProviderId::Claude => {
            if reading.source == Some(quota_core::types::Source::Statusline) {
                // A statusline push has no provider identity. It is bound to
                // the default Claude scope at acceptance; an explicit Claude
                // account or an isolated CLAUDE_CONFIG_DIR rejects pushes.
                let env_config_dir = std::env::var("CLAUDE_CONFIG_DIR").ok();
                return account_scope(g, ProviderId::Claude) == AccountScope::default()
                    && !ClaudeAdapter::has_isolated_config(None, env_config_dir.as_deref());
            }
            let active = ClaudeAdapter::for_account(plan.claude_home)
                .active_identity_for(reading.credential_path.as_deref());
            reading_answers_for_active_account(taken_for, &active, Vec::new)
        }
        // Cursor does not expose a stable account identity. A selected quota
        // account is attached to its refresh scope and is invalidated on a
        // switch; the current session therefore behaves as an unnamed source.
        ProviderId::Cursor => {
            reading_answers_for_active_account(taken_for, &ActiveIdentity::Unnamed, Vec::new)
        }
    }
}

fn status_now(g: &App, snap: Option<&Snapshot>, filter: ProviderFilter) -> Snapshot {
    answering_now(g, filter_snapshot(snap, filter))
}

async fn read_frame_owned(
    mut reader: OwnedReadHalf,
) -> (OwnedReadHalf, Result<Vec<u8>, FrameError>) {
    let frame = read_frame_async(&mut reader).await;
    (reader, frame)
}

async fn read_frame_async<R: AsyncRead + Unpin>(stream: &mut R) -> Result<Vec<u8>, FrameError> {
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

async fn write_frame_async<W: AsyncWrite + Unpin>(
    stream: &mut W,
    resp: &Response,
) -> Result<(), ClientError> {
    let json = serde_json::to_vec(resp).map_err(|e| ClientError::Other(e.to_string()))?;
    let frame = encode_frame(&json)?;
    stream
        .write_all(&frame)
        .await
        .map_err(|e| ClientError::Other(e.to_string()))?;
    Ok(())
}

async fn write_frame_timed<W: AsyncWrite + Unpin>(
    stream: &mut W,
    resp: &Response,
) -> Result<(), ClientError> {
    match tokio::time::timeout(WATCH_WRITE_TIMEOUT, write_frame_async(stream, resp)).await {
        Ok(r) => r,
        Err(_) => Err(ClientError::Other("watch write timeout".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quota_core::protocol::{METHOD_PING, METHOD_VERSION};

    /// A push outranks polling while its reading is current: the core max age.
    const PUSH_PRECEDENCE_SECS: i64 = DEFAULT_READING_MAX_AGE_SECS as i64;
    use quota_core::types::{AdapterError, ProviderId, ProviderObservation, ProviderSnapshot};

    fn unique_test_dir(prefix: &str) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        for _ in 0..1_000 {
            let serial = NEXT.fetch_add(1, Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("{prefix}-{}-{serial}", std::process::id()));
            match fs::create_dir(&dir) {
                Ok(()) => {
                    owner_only_dir(&dir);
                    return dir;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create test directory {}: {error}", dir.display()),
            }
        }
        panic!("could not allocate unique test directory for {prefix}");
    }

    /// `bind_private_socket` narrows the process umask while another test may
    /// be creating a directory, so set a test directory's mode explicitly.
    fn owner_only_dir(dir: &Path) {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn test_subdir(dir: &Path, name: &str) -> PathBuf {
        let sub = dir.join(name);
        fs::create_dir_all(&sub).unwrap();
        owner_only_dir(&sub);
        sub
    }

    async fn claim(app: &Arc<RwLock<App>>, provider: ProviderId) -> Option<Claim> {
        let observed = observe(&*app.read().await, provider);
        observed.claim().await
    }

    /// One provider's refresh through the same gate, backoff and publish path
    /// as a scheduled cycle.
    async fn refresh_provider_with<F, Fut>(app: Arc<RwLock<App>>, provider: ProviderId, probe: F)
    where
        F: FnOnce(Arc<RwLock<App>>, ProbeMode) -> Fut,
        Fut: std::future::Future<Output = Option<ProviderSnapshot>>,
    {
        let Some(claim) = claim(&app, provider).await else {
            return;
        };
        let probed = probe_claimed(&app, claim, probe).await;
        publish(&app, probed.into_iter().collect()).await;
    }

    /// Publish a full probe result for the active account, ignoring backoff.
    async fn apply_provider_snapshot(app: &Arc<RwLock<App>>, snapshot: ProviderSnapshot) {
        let provider = snapshot.provider;
        let claim = claim(app, provider).await.expect("no refresh in flight");
        let scope = account_scope(&*app.read().await, provider);
        publish(
            app,
            vec![Probed {
                claim,
                scope,
                mode: ProbeMode::Full,
                snapshot,
            }],
        )
        .await;
    }

    impl RefreshGate {
        /// Account-agnostic single flight, for exercising the gate alone.
        async fn run_if_current<F, Fut>(&self, observed_generation: u64, refresh: F) -> bool
        where
            F: FnOnce() -> Fut,
            Fut: std::future::Future<Output = bool>,
        {
            let scope = AccountScope::default();
            let Some(mut guard) = self.begin(observed_generation, &scope).await else {
                return false;
            };
            if !refresh().await {
                return false;
            }
            self.complete(&mut guard, scope);
            true
        }
    }

    impl BackoffProbe {
        /// The file-only reading, asserting the credentials still name the
        /// deadline's account.
        fn owner_reading(self) -> Option<ProviderSnapshot> {
            match self {
                BackoffProbe::Owner(file) => file.map(|file| *file),
                BackoffProbe::OtherAccount => panic!("credentials name another account"),
            }
        }
    }

    /// Every credential and CodexBar read stays inside a fresh temp dir.
    fn test_app() -> App {
        test_app_in(&unique_test_dir("quota-test-acct"))
    }

    fn test_app_in(dir: &Path) -> App {
        let accounts_path = dir.join("accounts.json");
        let cfg = Config {
            history: false,
            ring_capacity: 16,
            accounts_path: Some(accounts_path.clone()),
            codexbar_dir: Some(dir.join("codexbar")),
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
            retry_after_until: HashMap::new(),
            refresh_gates: provider_refresh_gates(),
            store,
            accounts: AccountStore::load(accounts_path),
            cfg,
            watch_tx,
            pushed_at: HashMap::new(),
            last_ring_push: HashMap::new(),
            last_polled: HashMap::new(),
            codex_home: Some(dir.join("codex")),
            claude_home: Some(dir.join("claude")),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn scheduled_refresh_timer_ticks_during_continuous_socket_traffic() {
        let dir = unique_test_dir("quota-accept-loop");
        let socket = dir.join("quotad.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let app = Arc::new(RwLock::new(test_app_in(&dir)));
        app.write().await.interval_secs = 1;
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let ticks = Arc::new(AtomicU64::new(0));
        let observed_ticks = ticks.clone();
        let server = tokio::spawn(accept_loop(
            listener,
            app,
            Arc::new(Semaphore::new(MAX_RPC_CLIENTS)),
            Arc::new(Semaphore::new(MAX_WATCH_CLIENTS)),
            Duration::from_millis(25),
            async move {
                let _ = shutdown_rx.await;
            },
            move || {
                observed_ticks.fetch_add(1, Ordering::Relaxed);
                async {}
            },
        ));

        let traffic_socket = socket.clone();
        let traffic = tokio::spawn(async move {
            let stop = std::time::Instant::now() + Duration::from_millis(1_150);
            let mut requests = 0;
            while std::time::Instant::now() < stop {
                let mut stream = UnixStream::connect(&traffic_socket).await.unwrap();
                let request = serde_json::to_vec(&Request::new(requests, METHOD_PING)).unwrap();
                stream
                    .write_all(&encode_frame(&request).unwrap())
                    .await
                    .unwrap();
                let payload = read_frame_async(&mut stream).await.unwrap();
                let response: Response = serde_json::from_slice(&payload).unwrap();
                assert!(response.ok);
                requests += 1;
            }
            requests
        });
        let requests = tokio::time::timeout(Duration::from_secs(3), traffic)
            .await
            .expect("socket traffic completed")
            .expect("traffic task completed");
        let _ = shutdown_tx.send(());
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("accept loop shut down")
            .expect("accept loop task completed");

        assert!(
            requests >= 10,
            "test must sustain socket traffic: {requests}"
        );
        assert!(
            ticks.load(Ordering::Relaxed) >= 2,
            "scheduled refreshes were starved during traffic"
        );
        let _ = fs::remove_dir_all(dir);
    }

    /// Claude credentials in `test_app_in(dir)`'s Claude dir. Like every
    /// Claude credential, they name no account.
    fn write_claude_credentials(dir: &Path) {
        let claude = test_subdir(dir, "claude");
        fs::write(
            claude.join(".credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"test-token"}}"#,
        )
        .unwrap();
    }

    /// A Codex auth file in `test_app_in(dir)`'s Codex home that names no
    /// account.
    fn write_unnamed_codex_credentials(dir: &Path) {
        let codex = test_subdir(dir, "codex");
        fs::write(codex.join("auth.json"), r#"{"access_token":"test-token"}"#).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ping_and_version() {
        let app = Arc::new(RwLock::new(test_app()));
        let ping = dispatch(&app, Request::new(1, METHOD_PING)).await;
        assert!(ping.ok);
        let ver = dispatch(&app, Request::new(2, METHOD_VERSION)).await;
        assert!(ver.ok);
        let info: VersionInfo = serde_json::from_value(ver.result.unwrap()).unwrap();
        assert_eq!(info.protocol, quota_core::PROTOCOL_VERSION);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn concurrent_refresh_requests_share_one_probe() {
        let gate = Arc::new(RefreshGate::default());
        let observed_generation = gate.generation();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let gate = gate.clone();
            let calls = calls.clone();
            tasks.push(tokio::spawn(async move {
                gate.run_if_current(observed_generation, || async move {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    calls.fetch_add(1, Ordering::Relaxed);
                    true
                })
                .await
            }));
        }
        let mut completed = 0;
        for task in tasks {
            completed += usize::from(task.await.unwrap());
        }
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(completed, 1);
        assert_eq!(gate.generation(), 1);
    }

    #[test]
    fn retry_after_does_not_lengthen_the_shared_refresh_interval() {
        assert_eq!(refresh_interval_secs(true, false, 30, 5, 300), 45);
        assert_eq!(refresh_interval_secs(false, false, 30, 5, 300), 60);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn headerless_rate_limit_backs_off_for_the_longest_refresh_interval() {
        let app = Arc::new(RwLock::new(test_app()));
        let max = app.read().await.cfg.refresh_max_secs();
        let limited = ProviderSnapshot::unavailable(
            ProviderId::Claude,
            AdapterError::new("rate_limited", "HTTP 429"),
        );
        assert_eq!(limited.retry_after_secs, None);

        apply_provider_snapshot(&app, limited).await;

        assert!(provider_in_backoff(&app, ProviderId::Claude).await);
        let g = app.read().await;
        let stored = g.store.latest().unwrap().by_id(ProviderId::Claude).unwrap();
        assert_eq!(stored.retry_after_secs, Some(max));
    }

    #[test]
    fn watch_deadline_tracks_the_first_reading_to_expire() {
        let now = now_unix();
        let provider = ProviderSnapshot::observed(ProviderObservation {
            provider: ProviderId::Codex,
            source: None,
            windows: vec![quota_core::types::UsageWindow::from_percent_at(
                quota_core::types::WindowKind::Weekly,
                "weekly",
                10.0,
                None,
                None,
                Some(now),
                2,
            )],
            credits: None,
            plan: None,
            credential_path: None,
            observed_at: Some(now),
            max_age_secs: 2,
            permission: quota_core::types::ProviderPermission::Unknown,
        });
        let snapshot = Snapshot::new(now, vec![provider]);
        let soon = next_expiry_deadline(Some(&snapshot)) - tokio::time::Instant::now();
        assert!(soon <= Duration::from_secs(4), "{soon:?}");
        let never = next_expiry_deadline(None) - tokio::time::Instant::now();
        assert!(never > Duration::from_secs(24 * 3600));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn codex_fallback_retry_after_does_not_block_claude_refresh() {
        use quota_adapters::http::{HttpResponse, MockTransport};
        use quota_adapters::provider::ProbeCtx;
        use quota_adapters::CodexAdapter;
        use quota_core::types::{ProviderPermission, Source, UsageWindow, WindowKind};

        let dir = unique_test_dir("quota-rate-limit-adapter");
        let home = dir.join("codex");
        fs::create_dir(&home).unwrap();
        fs::write(home.join("auth.json"), br#"{"access_token":"test-token"}"#).unwrap();
        let transport = MockTransport {
            next: Some(Ok(HttpResponse {
                status: 429,
                body: b"{}".to_vec(),
                retry_after_secs: Some(120),
            })),
            last_url: std::sync::Mutex::new(None),
        };
        let adapter = CodexAdapter {
            home: Some(home),
            codexbar_dir: Some(quota_adapters::codexbar::workspace_fixtures_dir()),
            enable_codexbar_files: true,
        };
        let ctx = ProbeCtx {
            transport: &transport,
            now: now_unix(),
        };
        let codex = adapter.probe(&ctx);
        assert_eq!(codex.source, Some(Source::File));
        assert_eq!(codex.retry_after_secs, Some(120));

        let app = Arc::new(RwLock::new(test_app()));
        apply_provider_snapshot(&app, codex).await;
        assert!(provider_in_backoff(&app, ProviderId::Codex).await);
        assert!(!provider_in_backoff(&app, ProviderId::Claude).await);

        let codex_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let codex_calls_probe = codex_calls.clone();
        refresh_provider_with(app.clone(), ProviderId::Codex, move |_, mode| async move {
            if mode == ProbeMode::FileOnly {
                return None;
            }
            codex_calls_probe.fetch_add(1, Ordering::Relaxed);
            Some(ProviderSnapshot::unavailable(
                ProviderId::Codex,
                AdapterError::new("unexpected", "must remain in backoff"),
            ))
        })
        .await;
        assert_eq!(codex_calls.load(Ordering::Relaxed), 0);

        let codex_deadline = app.read().await.retry_after_until[&ProviderId::Codex].clone();
        let claude_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let claude_calls_probe = claude_calls.clone();
        refresh_provider_with(app.clone(), ProviderId::Claude, move |_, _| async move {
            claude_calls_probe.fetch_add(1, Ordering::Relaxed);
            let now = now_unix();
            Some(ProviderSnapshot::observed(ProviderObservation {
                provider: ProviderId::Claude,
                source: Some(Source::Oauth),
                windows: vec![UsageWindow::from_percent_at(
                    WindowKind::Weekly,
                    "weekly",
                    25.0,
                    None,
                    None,
                    Some(now),
                    300,
                )],
                credits: None,
                plan: None,
                credential_path: None,
                observed_at: Some(now),
                max_age_secs: 300,
                permission: ProviderPermission::Allowed,
            }))
        })
        .await;

        assert_eq!(claude_calls.load(Ordering::Relaxed), 1);
        assert!(provider_in_backoff(&app, ProviderId::Codex).await);
        assert_eq!(
            app.read().await.retry_after_until[&ProviderId::Codex],
            codex_deadline
        );
        let latest = app.read().await.store.latest().cloned().unwrap();
        assert!(latest.by_id(ProviderId::Codex).is_some());
        assert!(latest.by_id(ProviderId::Claude).is_some());
        assert_eq!(
            latest
                .providers
                .iter()
                .map(|provider| provider.provider)
                .collect::<Vec<_>>(),
            [ProviderId::Codex, ProviderId::Claude]
        );

        let now = now_unix();
        apply_provider_snapshot(
            &app,
            ProviderSnapshot::observed(ProviderObservation {
                provider: ProviderId::Codex,
                source: Some(Source::Oauth),
                windows: vec![UsageWindow::from_percent_at(
                    WindowKind::Weekly,
                    "weekly",
                    30.0,
                    None,
                    None,
                    Some(now),
                    300,
                )],
                credits: None,
                plan: None,
                credential_path: None,
                observed_at: Some(now),
                max_age_secs: 300,
                permission: ProviderPermission::Allowed,
            }),
        )
        .await;
        let mut claude_limited = ProviderSnapshot::unavailable(
            ProviderId::Claude,
            AdapterError::new("rate_limited", "HTTP 429"),
        );
        claude_limited.retry_after_secs = Some(120);
        apply_provider_snapshot(&app, claude_limited).await;
        assert!(provider_in_backoff(&app, ProviderId::Claude).await);
        assert!(!provider_in_backoff(&app, ProviderId::Codex).await);

        let codex_after_claude_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let codex_after_claude_probe = codex_after_claude_calls.clone();
        refresh_provider_with(app.clone(), ProviderId::Codex, move |_, _| async move {
            codex_after_claude_probe.fetch_add(1, Ordering::Relaxed);
            let now = now_unix();
            Some(ProviderSnapshot::observed(ProviderObservation {
                provider: ProviderId::Codex,
                source: Some(Source::Oauth),
                windows: vec![UsageWindow::from_percent_at(
                    WindowKind::Weekly,
                    "weekly",
                    35.0,
                    None,
                    None,
                    Some(now),
                    300,
                )],
                credits: None,
                plan: None,
                credential_path: None,
                observed_at: Some(now),
                max_age_secs: 300,
                permission: ProviderPermission::Allowed,
            }))
        })
        .await;
        assert_eq!(codex_after_claude_calls.load(Ordering::Relaxed), 1);
        assert!(provider_in_backoff(&app, ProviderId::Claude).await);
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn provider_gates_are_single_flight_without_cross_provider_locking() {
        let app = test_app();
        let codex = app.refresh_gates[&ProviderId::Codex].clone();
        let claude = app.refresh_gates[&ProviderId::Claude].clone();
        let codex_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let codex_done_task = codex_done.clone();
        let codex_task = tokio::spawn(async move {
            let generation = codex.generation();
            codex
                .run_if_current(generation, || async move {
                    tokio::time::sleep(Duration::from_millis(40)).await;
                    codex_done_task.store(true, Ordering::Release);
                    true
                })
                .await
        });
        tokio::task::yield_now().await;
        let generation = claude.generation();
        assert!(claude.run_if_current(generation, || async { true }).await);
        assert!(!codex_done.load(Ordering::Acquire));
        assert!(codex_task.await.unwrap());
        assert!(codex_done.load(Ordering::Acquire));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn interleaved_provider_refreshes_keep_pace_samples_source_scoped() {
        use quota_core::types::{ProviderPermission, Source, UsageWindow, WindowKind};

        let app = Arc::new(RwLock::new(test_app()));
        let now = now_unix();
        let observed = |provider, used, observed_at| {
            ProviderSnapshot::observed(ProviderObservation {
                provider,
                source: Some(Source::Oauth),
                windows: vec![UsageWindow::from_percent_at(
                    WindowKind::Weekly,
                    "weekly",
                    used,
                    None,
                    None,
                    Some(observed_at),
                    300,
                )],
                credits: None,
                plan: None,
                credential_path: None,
                observed_at: Some(observed_at),
                max_age_secs: 300,
                permission: ProviderPermission::Allowed,
            })
        };

        apply_provider_snapshot(&app, observed(ProviderId::Codex, 10.0, now - 100)).await;
        apply_provider_snapshot(&app, observed(ProviderId::Claude, 20.0, now - 100)).await;
        apply_provider_snapshot(&app, observed(ProviderId::Codex, 15.0, now - 50)).await;
        apply_provider_snapshot(&app, observed(ProviderId::Claude, 30.0, now)).await;

        let g = app.read().await;
        let latest = g.store.latest().unwrap();
        let codex = latest.by_id(ProviderId::Codex).unwrap();
        let report = pace_for(g.store.history_ref(), codex);

        assert_eq!(report.samples, 2);
        assert!((report.burn_percent_per_hour.unwrap() - 360.0).abs() < 1e-6);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn status_filters_provider() {
        let app = Arc::new(RwLock::new(test_app()));
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
        let app = Arc::new(RwLock::new(test_app()));
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

    #[test]
    fn permission_changes_count_as_usage_changes() {
        let now = now_unix();
        let make = |permission| {
            Snapshot::new(
                now,
                vec![ProviderSnapshot::observed(ProviderObservation {
                    provider: ProviderId::Codex,
                    source: Some(quota_core::types::Source::Oauth),
                    windows: Vec::new(),
                    credits: None,
                    plan: None,
                    credential_path: None,
                    observed_at: Some(now),
                    max_age_secs: 300,
                    permission,
                })],
            )
        };
        let allowed = make(quota_core::types::ProviderPermission::Allowed);
        let refused = make(quota_core::types::ProviderPermission::LimitReached);
        assert!(!same_usage(&allowed, &refused));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn accounts_add_list_remove() {
        let app = Arc::new(RwLock::new(test_app()));
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

    #[test]
    fn instance_lock_is_exclusive() {
        let dir = unique_test_dir("quota-lock");
        let sock = dir.join("quota.sock");
        let first = acquire_instance_lock(&sock).unwrap();
        let err = acquire_instance_lock(&sock).unwrap_err();
        assert!(err.to_string().contains("already running"));
        drop(first);
        let second = acquire_instance_lock(&sock).unwrap();
        drop(second);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prepare_socket_refuses_symlink() {
        let dir = unique_test_dir("quota-sock-sym");
        let target = dir.join("target.sock");
        let link = dir.join("quota.sock");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let err = prepare_socket(&link).unwrap_err();
        assert!(err.to_string().contains("symlink"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prepare_socket_refuses_live_instance() {
        let dir = unique_test_dir("quota-sock-live");
        let sock = dir.join("quota.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let err = prepare_socket(&sock).unwrap_err();
        assert!(err.to_string().contains("already running"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prepare_socket_unlinks_stale() {
        let dir = unique_test_dir("quota-sock-stale");
        let sock = dir.join("quota.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        drop(listener);
        prepare_socket(&sock).unwrap();
        assert!(!sock.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn peercred_same_uid_on_socketpair() {
        let (a, _b) = StdUnixStream::pair().unwrap();
        assert!(peer_is_same_uid(&a));
    }

    #[test]
    fn rustix_pid_alive_self() {
        assert!(pid_alive(std::process::id()));
        assert!(!pid_alive(0));
    }

    #[test]
    fn watch_idle_outlasts_configured_refresh() {
        assert_eq!(
            watch_idle_secs(300, None),
            Duration::from_secs(WATCH_IDLE_FLOOR_SECS)
        );
        assert_eq!(
            watch_idle_secs(600, None),
            Duration::from_secs(600 + WATCH_IDLE_SLACK_SECS)
        );
        assert_eq!(
            watch_idle_secs(3600, None),
            Duration::from_secs(3600 + WATCH_IDLE_SLACK_SECS)
        );
        assert_eq!(watch_idle_secs(86400, Some(1)), Duration::from_secs(1));
        assert!(watch_idle_secs(86400, None) > Duration::from_secs(86400));
    }

    #[test]
    fn socket_is_owner_only() {
        let dir = unique_test_dir("quota-sock-mode");
        let sock = dir.join("quota.sock");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let listener = rt.block_on(async { bind_private_socket(&sock).unwrap() });
        let mode = fs::metadata(&sock).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        drop(listener);
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn can_start_unavailable_is_honest() {
        let app = Arc::new(RwLock::new(test_app()));
        let req = Request::with_params(
            20,
            METHOD_CAN_START,
            quota_core::CanStartParams {
                tokens: 50_000,
                percent: None,
                reserve: None,
                deadline: None,
                provider: ProviderFilter::All,
            },
        );
        let resp = dispatch(&app, req).await;
        let result: CanStartResult = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert!(!result.ok);
        assert_eq!(result.answers.len(), 1);
        assert_eq!(
            result.answers[0].basis,
            quota_core::types::CanStartBasis::Unavailable
        );
        let raw = serde_json::to_string(&result).unwrap();
        assert!(!raw.contains("eyJ"));
        assert!(!raw.contains("access_token"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn accounts_add_does_not_echo_stuffed_secrets() {
        let app = Arc::new(RwLock::new(test_app()));
        let raw = serde_json::json!({
            "id": 30,
            "method": METHOD_ACCOUNTS_ADD,
            "params": {
                "id": "acct_leak",
                "provider": "codex",
                "email": "openai@ctx.op0.dev",
                "access_token": "sk-ant-secret-must-not-echo",
                "refresh_token": "rt-secret",
                "password": "hunter2",
                "secret_ref": { "backend": "openbao", "path": "quota/codex/work" },
                "select": true
            }
        });
        let req: Request = serde_json::from_value(raw).unwrap();
        let resp = dispatch(&app, req).await;
        assert!(resp.ok);
        let listed = dispatch(&app, Request::new(31, METHOD_ACCOUNTS_LIST)).await;
        let blob = serde_json::to_string(&listed).unwrap();
        assert!(blob.contains("quota/codex/work"));
        assert!(!blob.contains("sk-ant-secret-must-not-echo"));
        assert!(!blob.contains("rt-secret"));
        assert!(!blob.contains("hunter2"));
        assert!(!blob.contains("access_token"));
    }

    fn observe_request(id: u64, used: f64) -> Request {
        Request::with_params(
            id,
            METHOD_OBSERVE,
            serde_json::json!({
                "schema": 1,
                "provider": "claude",
                "source": "statusline",
                "windows": [{
                    "kind": "five_hour", "label": "5h", "used_percent": used,
                    "reset_at": now_unix() + 7_200, "limit_window_seconds": 18_000
                }]
            }),
        )
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_push_becomes_current_claude_evidence_and_outranks_polling() {
        let app = Arc::new(RwLock::new(test_app()));
        let before = app.read().await.interval_secs;
        assert!(poll_due(&*app.read().await, ProviderId::Claude));

        let resp = dispatch(&app, observe_request(1, 34.0)).await;
        assert!(resp.ok, "{resp:?}");
        let result: ObserveResult = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert_eq!(result.accepted, 1);

        let g = app.read().await;
        let claude = g
            .snapshot_or_empty()
            .by_id(ProviderId::Claude)
            .cloned()
            .unwrap();
        assert_eq!(claude.status, Availability::Ok);
        assert_eq!(claude.source, Some(quota_core::types::Source::Statusline));
        assert_eq!(claude.windows[0].used_percent, Some(34.0));
        assert!(
            !poll_due(&g, ProviderId::Claude),
            "fresh push must beat OAuth polling"
        );
        assert_eq!(
            g.interval_secs, before,
            "a push never retunes the poll cadence"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_statusline_push_is_refused_for_an_explicit_claude_account() {
        let app = Arc::new(RwLock::new(test_app()));
        let add = Request::with_params(
            1,
            METHOD_ACCOUNTS_ADD,
            quota_core::AccountsAddParams {
                id: Some("isolated-claude".into()),
                provider: ProviderId::Claude,
                email: None,
                workspace_label: Some("isolated".into()),
                login_method: None,
                workspace_account_id: None,
                secret_ref: None,
                home_path: None,
                select: true,
            },
        );
        assert!(dispatch(&app, add).await.ok);

        let response = dispatch(&app, observe_request(2, 34.0)).await;
        assert!(!response.ok);
        assert_eq!(response.error.unwrap().code, "account_scope");
        let g = app.read().await;
        assert!(g.snapshot_or_empty().by_id(ProviderId::Claude).is_none());
        assert!(!g.pushed_at.contains_key(&ProviderId::Claude));
        assert!(poll_due(&g, ProviderId::Claude));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_statusline_push_is_refused_for_an_environment_isolated_claude_config() {
        use quota_core::protocol::ObserveParams;

        let app = Arc::new(RwLock::new(test_app()));
        apply_provider_snapshot(&app, observed_reading(ProviderId::Claude, 12.0)).await;
        let before = app.read().await.store.latest().cloned().unwrap();
        let params: ObserveParams =
            serde_json::from_value(observe_request(2, 34.0).params).unwrap();
        let pushed = snapshot_from_push(&params, now_unix()).unwrap();

        assert!(
            !apply_pushed_snapshot_for_config(
                &app,
                pushed,
                now_unix(),
                Some("/tmp/claude-isolated-fixture"),
            )
            .await
        );

        let g = app.read().await;
        assert_eq!(g.store.latest(), Some(&before));
        assert!(!g.pushed_at.contains_key(&ProviderId::Claude));
        assert!(poll_due(&g, ProviderId::Claude));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_statusline_push_preserves_current_opus_and_sonnet_oauth_windows() {
        use quota_core::types::{ProviderPermission, Source, UsageWindow, WindowKind};

        let app = Arc::new(RwLock::new(test_app()));
        let now = now_unix();
        let oauth = ProviderSnapshot::observed(ProviderObservation {
            provider: ProviderId::Claude,
            source: Some(Source::Oauth),
            windows: vec![
                UsageWindow::from_percent_at(
                    WindowKind::Extra,
                    "opus weekly",
                    35.0,
                    Some(now + 7 * 24 * 60 * 60),
                    Some(7 * 24 * 60 * 60),
                    Some(now),
                    DEFAULT_READING_MAX_AGE_SECS,
                ),
                UsageWindow::from_percent_at(
                    WindowKind::Extra,
                    "sonnet weekly",
                    100.0,
                    Some(now + 7 * 24 * 60 * 60),
                    Some(7 * 24 * 60 * 60),
                    Some(now),
                    DEFAULT_READING_MAX_AGE_SECS,
                ),
            ],
            credits: None,
            plan: None,
            credential_path: None,
            observed_at: Some(now),
            max_age_secs: DEFAULT_READING_MAX_AGE_SECS,
            permission: ProviderPermission::Allowed,
        });
        apply_provider_snapshot(&app, oauth).await;

        let response = dispatch(&app, observe_request(3, 34.0)).await;
        assert!(response.ok, "{response:?}");
        let original_observed_at: HashMap<_, _> = {
            let g = app.read().await;
            let snapshot = g.snapshot_or_empty();
            let claude = snapshot.by_id(ProviderId::Claude).unwrap();
            assert_eq!(claude.source, Some(Source::Statusline));
            ["opus weekly", "sonnet weekly"]
                .into_iter()
                .map(|label| {
                    let window = claude
                        .windows
                        .iter()
                        .find(|window| window.label == label)
                        .unwrap();
                    (label, window.observed_at)
                })
                .collect()
        };
        assert_eq!(original_observed_at["opus weekly"], Some(now));
        assert_eq!(original_observed_at["sonnet weekly"], Some(now));

        let second = dispatch(&app, observe_request(4, 35.0)).await;
        assert!(second.ok, "{second:?}");
        {
            let g = app.read().await;
            let claude = g
                .snapshot_or_empty()
                .by_id(ProviderId::Claude)
                .cloned()
                .unwrap();
            assert_eq!(claude.source, Some(Source::Statusline));
            for label in ["opus weekly", "sonnet weekly"] {
                let preserved = claude
                    .windows
                    .iter()
                    .find(|window| window.label == label)
                    .unwrap();
                assert_eq!(preserved.observed_at, original_observed_at[label]);
            }
        }

        let response = dispatch(
            &app,
            can_start_request(serde_json::json!({"percent": 1.0, "provider": "claude"})),
        )
        .await;
        let result: CanStartResult = serde_json::from_value(response.result.unwrap()).unwrap();
        assert!(
            !result.ok,
            "the exhausted Sonnet window must veto admission"
        );
        assert_eq!(result.answers[0].window_kind, Some(WindowKind::Extra));
        assert!(result.answers[0].explanation.contains("sonnet weekly"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_push_during_oauth_probe_keeps_new_push_and_merges_exhausted_sonnet_limit() {
        use quota_core::types::{ProviderPermission, Source, UsageWindow, WindowKind};

        let app = Arc::new(RwLock::new(test_app()));
        let probe_observed_at = now_unix();
        let oauth = ProviderSnapshot::observed(ProviderObservation {
            provider: ProviderId::Claude,
            source: Some(Source::Oauth),
            windows: vec![
                UsageWindow::from_percent_at(
                    WindowKind::FiveHour,
                    "5h",
                    15.0,
                    None,
                    Some(18_000),
                    Some(probe_observed_at),
                    DEFAULT_READING_MAX_AGE_SECS,
                ),
                UsageWindow::from_percent_at(
                    WindowKind::Extra,
                    "opus weekly",
                    35.0,
                    None,
                    Some(604_800),
                    Some(probe_observed_at),
                    DEFAULT_READING_MAX_AGE_SECS,
                ),
                UsageWindow::from_percent_at(
                    WindowKind::Extra,
                    "sonnet weekly",
                    100.0,
                    None,
                    Some(604_800),
                    Some(probe_observed_at),
                    DEFAULT_READING_MAX_AGE_SECS,
                ),
            ],
            credits: None,
            plan: None,
            credential_path: None,
            observed_at: Some(probe_observed_at),
            max_age_secs: DEFAULT_READING_MAX_AGE_SECS,
            permission: ProviderPermission::Unknown,
        });
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (complete_tx, complete_rx) = tokio::sync::oneshot::channel();
        let probe_app = app.clone();
        let poll = tokio::spawn(async move {
            refresh_provider_with(probe_app, ProviderId::Claude, move |_, _mode| async move {
                let _ = started_tx.send(());
                complete_rx.await.ok()
            })
            .await;
        });

        started_rx.await.expect("OAuth probe started");
        assert!(dispatch(&app, observe_request(5, 34.0)).await.ok);
        let pushed_at = {
            let g = app.read().await;
            g.snapshot_or_empty()
                .by_id(ProviderId::Claude)
                .unwrap()
                .windows[0]
                .observed_at
                .unwrap()
        };
        complete_tx.send(oauth).expect("complete OAuth probe");
        poll.await.expect("poll task completed");

        let shown = claude_status(&app).await;
        assert_eq!(shown.source, Some(Source::Statusline));
        let pushed_five_hour = shown
            .windows
            .iter()
            .find(|window| window.label == "5h")
            .unwrap();
        assert_eq!(pushed_five_hour.used_percent, Some(34.0));
        assert_eq!(pushed_five_hour.observed_at, Some(pushed_at));
        for label in ["opus weekly", "sonnet weekly"] {
            assert_eq!(
                shown
                    .windows
                    .iter()
                    .find(|window| window.label == label)
                    .unwrap()
                    .observed_at,
                Some(probe_observed_at),
                "OAuth timestamps must survive the merge"
            );
        }

        let response = dispatch(
            &app,
            can_start_request(serde_json::json!({"percent": 1.0, "provider": "claude"})),
        )
        .await;
        let result: CanStartResult = serde_json::from_value(response.result.unwrap()).unwrap();
        assert!(!result.ok, "exhausted Sonnet must veto admission");
        assert_eq!(result.answers[0].window_kind, Some(WindowKind::Extra));
        assert!(result.answers[0].explanation.contains("sonnet weekly"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn an_in_flight_poll_never_overwrites_a_push_that_landed_during_it() {
        let app = Arc::new(RwLock::new(test_app()));
        assert!(poll_due(&*app.read().await, ProviderId::Claude));
        refresh_provider_with(app.clone(), ProviderId::Claude, |app, _mode| async move {
            assert!(dispatch(&app, observe_request(1, 34.0)).await.ok);
            let mut polled = ProviderSnapshot::unavailable(
                ProviderId::Claude,
                AdapterError::new("http_401", "token expired"),
            );
            polled.source = Some(quota_core::types::Source::Oauth);
            Some(polled)
        })
        .await;
        let g = app.read().await;
        let claude = g
            .snapshot_or_empty()
            .by_id(ProviderId::Claude)
            .cloned()
            .unwrap();
        assert_eq!(claude.source, Some(quota_core::types::Source::Statusline));
        assert_eq!(claude.status, Availability::Ok);
        assert_eq!(claude.windows[0].used_percent, Some(34.0));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_full_poll_snapshot_keeps_a_current_push() {
        let app = Arc::new(RwLock::new(test_app()));
        assert!(dispatch(&app, observe_request(1, 34.0)).await.ok);
        let polled = ProviderSnapshot::unavailable(
            ProviderId::Claude,
            AdapterError::new("http_401", "token expired"),
        );
        refresh_provider_with(
            app.clone(),
            ProviderId::Claude,
            move |_, _mode| async move { Some(polled) },
        )
        .await;
        let g = app.read().await;
        let claude = g
            .snapshot_or_empty()
            .by_id(ProviderId::Claude)
            .cloned()
            .unwrap();
        assert_eq!(claude.source, Some(quota_core::types::Source::Statusline));
        assert_eq!(claude.windows[0].used_percent, Some(34.0));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_poll_replaces_a_push_once_the_push_is_no_longer_current() {
        let app = Arc::new(RwLock::new(test_app()));
        assert!(dispatch(&app, observe_request(1, 34.0)).await.ok);
        let stale = now_unix() - PUSH_PRECEDENCE_SECS - 1;
        app.write()
            .await
            .pushed_at
            .insert(ProviderId::Claude, stale);
        let polled = ProviderSnapshot::unavailable(
            ProviderId::Claude,
            AdapterError::new("http_401", "token expired"),
        );
        apply_provider_snapshot(&app, polled).await;
        let g = app.read().await;
        let claude = g
            .snapshot_or_empty()
            .by_id(ProviderId::Claude)
            .cloned()
            .unwrap();
        assert_eq!(claude.status, Availability::Unavailable);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn polling_resumes_once_the_push_is_no_longer_current() {
        let app = Arc::new(RwLock::new(test_app()));
        dispatch(&app, observe_request(1, 34.0)).await;
        let mut g = app.write().await;
        let stale = now_unix() - PUSH_PRECEDENCE_SECS - 1;
        g.pushed_at.insert(ProviderId::Claude, stale);
        assert!(poll_due(&g, ProviderId::Claude));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn push_precedence_and_reading_freshness_agree_at_the_max_age_boundary() {
        let app = Arc::new(RwLock::new(test_app()));
        dispatch(&app, observe_request(1, 34.0)).await;
        let mut g = app.write().await;
        let now = now_unix();
        for (age, current) in [
            (PUSH_PRECEDENCE_SECS - 1, true),
            (PUSH_PRECEDENCE_SECS, true),
            (PUSH_PRECEDENCE_SECS + 1, false),
            (-1, false),
        ] {
            g.pushed_at.insert(ProviderId::Claude, now - age);
            assert_eq!(
                push_is_current(&g, ProviderId::Claude, now),
                current,
                "push at age {age}"
            );
            let reading = freshness_for(Some(now - age), DEFAULT_READING_MAX_AGE_SECS, now);
            assert_eq!(
                reading == Freshness::Current,
                current,
                "reading at age {age}"
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn repeated_identical_pushes_do_not_evict_ring_history() {
        let app = Arc::new(RwLock::new(test_app()));
        for id in 0..10 {
            assert!(dispatch(&app, observe_request(id, 34.0)).await.ok);
        }
        let g = app.read().await;
        assert_eq!(
            g.store.history_ref().len(),
            2,
            "seed entry plus one pushed entry, not one per push"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_changed_push_takes_a_new_ring_entry_for_pace_history() {
        let app = Arc::new(RwLock::new(test_app()));
        dispatch(&app, observe_request(1, 34.0)).await;
        dispatch(&app, observe_request(2, 35.0)).await;
        assert_eq!(app.read().await.store.history_ref().len(), 3);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn bad_pushes_are_refused_with_codes_and_store_nothing() {
        let app = Arc::new(RwLock::new(test_app()));
        let mut req = observe_request(1, 34.0);
        req.params["schema"] = serde_json::json!(9);
        let resp = dispatch(&app, req).await;
        assert_eq!(resp.error.unwrap().code, "unsupported_schema");
        let resp = dispatch(
            &app,
            Request::with_params(2, METHOD_OBSERVE, serde_json::json!({"x": 1})),
        )
        .await;
        assert_eq!(resp.error.unwrap().code, "bad_params");
        let g = app.read().await;
        assert!(g.snapshot_or_empty().by_id(ProviderId::Claude).is_none());
    }

    fn can_start_request(params: serde_json::Value) -> Request {
        Request::with_params(9, METHOD_CAN_START, params)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_statusline_push_with_a_malformed_weekly_bucket_refuses_percent_admission() {
        let app = Arc::new(RwLock::new(test_app()));
        let parsed = quota_source_claude_statusline::parse(
            br#"{"rate_limits":{"five_hour":{"used_percentage":10,"resets_at":4102444800},"seven_day":"soon"}}"#,
        )
        .unwrap();
        let push = Request::with_params(1, METHOD_OBSERVE, parsed.observe_params().unwrap());
        assert!(dispatch(&app, push).await.ok);
        let resp = dispatch(
            &app,
            can_start_request(serde_json::json!({"percent": 1.0, "provider": "claude"})),
        )
        .await;
        let result: CanStartResult = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert!(!result.ok, "{result:?}");
        assert_eq!(
            result.answers[0].window_kind,
            Some(quota_core::types::WindowKind::Weekly)
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_pushed_spend_limit_reads_back_from_status_as_a_coherent_usd_window() {
        let app = Arc::new(RwLock::new(test_app()));
        let fixture = std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../fixtures/claude-statusline/spend-2.1.284.json"),
        )
        .unwrap();
        let parsed = quota_source_claude_statusline::parse(&fixture).unwrap();
        let push = Request::with_params(1, METHOD_OBSERVE, parsed.observe_params().unwrap());
        assert!(dispatch(&app, push).await.ok);
        let resp = dispatch(
            &app,
            Request::with_params(
                2,
                METHOD_STATUS,
                StatusParams {
                    provider: ProviderFilter::Claude,
                },
            ),
        )
        .await;
        let status = resp.result.unwrap();
        let spend = status["snapshot"]["providers"][0]["windows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|w| w["kind"] == "spend")
            .unwrap()
            .clone();
        for money in [&spend, &spend["reading"]] {
            assert_eq!(money["unit"], "usd", "{spend}");
            assert_eq!(money["used_usd"], 271.4, "{spend}");
            assert_eq!(money["limit_usd"], 500.0, "{spend}");
            assert_eq!(money["limit"], 500.0, "{spend}");
            assert_eq!(money["remaining"], 500.0 - 271.4, "{spend}");
        }
        assert_eq!(spend["used_percent"], 271.4 / 500.0 * 100.0, "{spend}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn percent_admission_runs_through_dispatch_on_pushed_evidence() {
        let app = Arc::new(RwLock::new(test_app()));
        dispatch(&app, observe_request(1, 34.0)).await;
        let resp = dispatch(
            &app,
            can_start_request(serde_json::json!({"percent": 10.0, "provider": "claude"})),
        )
        .await;
        let result: CanStartResult = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert!(result.ok, "{result:?}");
        assert_eq!(
            result.answers[0].basis,
            quota_core::types::CanStartBasis::PercentBudget
        );

        let resp = dispatch(
            &app,
            can_start_request(serde_json::json!({"percent": 65.0, "provider": "claude"})),
        )
        .await;
        let result: CanStartResult = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert!(!result.ok);
        let admission = result.answers[0].admission.clone().unwrap();
        assert!(!admission.headroom_ok);
        assert_eq!(admission.reserve_percent, 2.0);

        let resp = dispatch(
            &app,
            can_start_request(
                serde_json::json!({"percent": 65.0, "reserve": 0.5, "provider": "claude"}),
            ),
        )
        .await;
        let result: CanStartResult = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert!(result.ok, "reserve is the caller's to lower: {result:?}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn percent_aggregate_keeps_an_all_unreadable_provider_veto() {
        use quota_core::types::{
            ProviderObservation, ProviderPermission, Source, UsageWindow, WindowKind,
        };

        let dir = unique_test_dir("quota-percent-unreadable-all");
        write_claude_credentials(&dir);
        let app = Arc::new(RwLock::new(test_app_in(&dir)));
        let now = now_unix();
        let claude = ProviderSnapshot::observed(ProviderObservation {
            provider: ProviderId::Claude,
            source: Some(Source::Oauth),
            windows: vec![UsageWindow::unreadable(
                WindowKind::Weekly,
                "sonnet weekly",
                None,
                Some(604_800),
                Some(now),
                300,
            )],
            credits: None,
            plan: None,
            credential_path: None,
            observed_at: Some(now),
            max_age_secs: 300,
            permission: ProviderPermission::Unknown,
        });
        assert_eq!(claude.status, Availability::Unavailable);
        apply_provider_snapshot(&app, claude).await;
        apply_provider_snapshot(&app, observed_reading(ProviderId::Cursor, 10.0)).await;

        let response = dispatch(
            &app,
            can_start_request(serde_json::json!({"percent": 1.0, "provider": "all"})),
        )
        .await;
        let result: CanStartResult = serde_json::from_value(response.result.unwrap()).unwrap();
        assert!(
            !result.ok,
            "the approving Cursor reading cannot mask Claude"
        );
        let claude = result
            .answers
            .iter()
            .find(|answer| answer.provider == ProviderId::Claude)
            .unwrap();
        assert_eq!(
            claude.basis,
            quota_core::types::CanStartBasis::UnknownWindow
        );
        assert_eq!(claude.window_kind, Some(WindowKind::Weekly));
        let cursor = result
            .answers
            .iter()
            .find(|answer| answer.provider == ProviderId::Cursor)
            .unwrap();
        assert!(cursor.ok);
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn percent_aggregate_keeps_a_mixed_readable_unreadable_provider_veto() {
        use quota_core::types::{
            ProviderObservation, ProviderPermission, Source, UsageWindow, WindowKind,
        };

        let dir = unique_test_dir("quota-percent-unreadable-mixed");
        write_claude_credentials(&dir);
        let app = Arc::new(RwLock::new(test_app_in(&dir)));
        let now = now_unix();
        let claude = ProviderSnapshot::observed(ProviderObservation {
            provider: ProviderId::Claude,
            source: Some(Source::Oauth),
            windows: vec![
                UsageWindow::from_percent_at(
                    WindowKind::FiveHour,
                    "5h",
                    10.0,
                    Some(now + 18_000),
                    Some(18_000),
                    Some(now),
                    300,
                ),
                UsageWindow::unreadable(
                    WindowKind::Weekly,
                    "sonnet weekly",
                    None,
                    Some(604_800),
                    Some(now),
                    300,
                ),
            ],
            credits: None,
            plan: None,
            credential_path: None,
            observed_at: Some(now),
            max_age_secs: 300,
            permission: ProviderPermission::Unknown,
        });
        assert_eq!(claude.status, Availability::Ok);
        apply_provider_snapshot(&app, claude).await;
        apply_provider_snapshot(&app, observed_reading(ProviderId::Cursor, 10.0)).await;

        let response = dispatch(
            &app,
            can_start_request(serde_json::json!({"percent": 1.0, "provider": "all"})),
        )
        .await;
        let result: CanStartResult = serde_json::from_value(response.result.unwrap()).unwrap();
        assert!(
            !result.ok,
            "a readable Claude window cannot vouch for the weekly limit"
        );
        let claude = result
            .answers
            .iter()
            .find(|answer| answer.provider == ProviderId::Claude)
            .unwrap();
        assert_eq!(
            claude.basis,
            quota_core::types::CanStartBasis::UnknownWindow
        );
        assert_eq!(claude.window_kind, Some(WindowKind::Weekly));
        let cursor = result
            .answers
            .iter()
            .find(|answer| answer.provider == ProviderId::Cursor)
            .unwrap();
        assert!(cursor.ok);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn admission_request_rejects_mixed_and_out_of_range_questions() {
        let ask = |v: serde_json::Value| admission_request(&serde_json::from_value(v).unwrap());
        assert!(matches!(
            ask(serde_json::json!({"tokens": 5})),
            Ok(Admission::Tokens(5))
        ));
        assert!(ask(serde_json::json!({"tokens": 5, "percent": 1.0})).is_err());
        assert!(ask(serde_json::json!({"percent": 0.0})).is_err());
        assert!(ask(serde_json::json!({"percent": 100.5})).is_err());
        assert!(ask(serde_json::json!({"percent": 5.0, "reserve": 100.0})).is_err());
        assert!(ask(serde_json::json!({"percent": 5.0, "reserve": -1.0})).is_err());
        assert!(ask(serde_json::json!({"reserve": 1.0})).is_err());
        assert!(ask(serde_json::json!({"percent": 5.0})).is_ok());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cursor_is_polled_no_faster_than_its_floor() {
        let app = Arc::new(RwLock::new(test_app()));
        let mut g = app.write().await;
        assert!(poll_due(&g, ProviderId::Cursor), "never polled yet");
        g.last_polled
            .insert(ProviderId::Cursor, tokio::time::Instant::now());
        assert!(!poll_due(&g, ProviderId::Cursor));
    }

    fn observed_reading(provider: ProviderId, used: f64) -> ProviderSnapshot {
        use quota_core::types::{ProviderPermission, Source, UsageWindow, WindowKind};
        let now = now_unix();
        ProviderSnapshot::observed(ProviderObservation {
            provider,
            source: Some(Source::Oauth),
            windows: vec![UsageWindow::from_percent_at(
                WindowKind::Weekly,
                "weekly",
                used,
                None,
                None,
                Some(now),
                300,
            )],
            credits: None,
            plan: None,
            credential_path: None,
            observed_at: Some(now),
            max_age_secs: 300,
            permission: ProviderPermission::Allowed,
        })
    }

    fn rate_limited(provider: ProviderId, retry_after_secs: Option<u64>) -> ProviderSnapshot {
        let mut snapshot =
            ProviderSnapshot::unavailable(provider, AdapterError::new("rate_limited", "HTTP 429"));
        snapshot.retry_after_secs = retry_after_secs;
        snapshot
    }

    async fn add_codex_account(app: &Arc<RwLock<App>>, id: &str, select: bool) {
        add_account(app, ProviderId::Codex, id, select).await;
    }

    async fn add_account(app: &Arc<RwLock<App>>, provider: ProviderId, id: &str, select: bool) {
        let add = Request::with_params(
            40,
            METHOD_ACCOUNTS_ADD,
            quota_core::AccountsAddParams {
                id: Some(id.into()),
                provider,
                email: None,
                workspace_label: None,
                login_method: None,
                workspace_account_id: None,
                secret_ref: None,
                home_path: None,
                select,
            },
        );
        assert!(dispatch(app, add).await.ok);
    }

    fn latest_codex_used(app: &App) -> Option<f64> {
        app.store
            .latest()?
            .by_id(ProviderId::Codex)?
            .windows
            .first()?
            .used_percent
    }

    #[tokio::test(flavor = "current_thread")]
    async fn greptile_3_refresh_after_account_switch_publishes_only_the_new_account() {
        let app = Arc::new(RwLock::new(test_app()));
        add_codex_account(&app, "acct_a", true).await;
        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let old_account_probe =
            refresh_provider_with(app.clone(), ProviderId::Codex, move |_, _| async move {
                let _ = started_tx.send(());
                let _ = release_rx.await;
                Some(observed_reading(ProviderId::Codex, 99.0))
            });
        let new_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let new_calls_probe = new_calls.clone();
        let switch_then_refresh = async {
            started_rx.await.unwrap();
            add_codex_account(&app, "acct_b", true).await;
            let new_account_probe =
                refresh_provider_with(app.clone(), ProviderId::Codex, move |_, _| async move {
                    new_calls_probe.fetch_add(1, Ordering::Relaxed);
                    Some(observed_reading(ProviderId::Codex, 42.0))
                });
            let release = async {
                tokio::task::yield_now().await;
                let _ = release_tx.send(());
            };
            tokio::join!(new_account_probe, release);
        };

        tokio::join!(old_account_probe, switch_then_refresh);

        assert_eq!(new_calls.load(Ordering::Relaxed), 1);
        let g = app.read().await;
        assert_eq!(latest_codex_used(&g), Some(42.0));
        assert!(g.store.history_ref().iter().all(|snapshot| snapshot
            .by_id(ProviderId::Codex)
            .is_none_or(|codex| codex.windows.iter().all(|w| w.used_percent != Some(99.0)))));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn greptile_3_gate_does_not_reuse_a_result_completed_for_another_account() {
        let gate = RefreshGate::default();
        let old = AccountScope {
            id: Some("acct_a".into()),
            home: None,
        };
        let new = AccountScope {
            id: Some("acct_b".into()),
            home: None,
        };
        let observed = gate.generation();
        gate.mark_complete(old.clone()).await;
        assert!(gate.begin(observed, &old).await.is_none());
        assert!(gate.begin(observed, &new).await.is_some());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn greptile_4_provider_schedule_restarts_healthy_slots_while_another_stalls() {
        let mut slots = HashMap::new();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        start_scheduled_refresh(&mut slots, ProviderId::Codex, || {
            tokio::spawn(async move {
                let _ = release_rx.await;
            })
        })
        .await;
        let mut healthy_starts = 0;
        for _ in 0..2 {
            let codex = slots.get(&ProviderId::Codex).unwrap();
            assert!(!codex.is_finished());
            start_scheduled_refresh(&mut slots, ProviderId::Claude, || {
                healthy_starts += 1;
                tokio::spawn(async {})
            })
            .await;
            while !slots.get(&ProviderId::Claude).unwrap().is_finished() {
                tokio::task::yield_now().await;
            }
        }
        assert_eq!(healthy_starts, 2);
        assert!(!slots.get(&ProviderId::Codex).unwrap().is_finished());
        let _ = release_tx.send(());
        for task in slots.into_values() {
            let _ = task.await;
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn greptile_4_refresh_claims_and_probes_are_independent_per_provider() {
        let app = Arc::new(RwLock::new(test_app()));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let held = tokio::spawn(refresh_provider_with(
            app.clone(),
            ProviderId::Codex,
            move |_, _| async move {
                let _ = started_tx.send(());
                let _ = release_rx.await;
                Some(observed_reading(ProviderId::Codex, 25.0))
            },
        ));
        started_rx.await.unwrap();

        let healthy_probes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let cycle_probes = healthy_probes.clone();
        let mixed_cycle = tokio::spawn(refresh_providers_within(
            app.clone(),
            &[ProviderId::Codex, ProviderId::Claude],
            Duration::ZERO,
            move |_, provider, _| {
                let cycle_probes = cycle_probes.clone();
                async move {
                    if provider == ProviderId::Claude {
                        cycle_probes.fetch_add(1, Ordering::Relaxed);
                        Some(observed_reading(ProviderId::Claude, 25.0))
                    } else {
                        None
                    }
                }
            },
        ));
        tokio::time::timeout(Duration::from_secs(2), async {
            while healthy_probes.load(Ordering::Relaxed) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("Claude starts while the Codex claim is held");
        assert!(!mixed_cycle.is_finished());
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let g = app.read().await;
                if g.refresh_gates[&ProviderId::Claude]
                    .completed
                    .try_lock()
                    .is_ok()
                {
                    break;
                }
                drop(g);
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the finished Claude probe releases its own gate");

        for _ in 0..2 {
            let healthy_probes = healthy_probes.clone();
            refresh_providers_within(
                app.clone(),
                &[ProviderId::Claude],
                Duration::ZERO,
                move |_, _, _| {
                    healthy_probes.fetch_add(1, Ordering::Relaxed);
                    async { Some(observed_reading(ProviderId::Claude, 25.0)) }
                },
            )
            .await;
        }
        assert_eq!(healthy_probes.load(Ordering::Relaxed), 3);
        assert!(!held.is_finished());

        let _ = release_tx.send(());
        held.await.unwrap();
        mixed_cycle.await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn greptile_4_finished_scheduled_refresh_does_not_block_the_next() {
        let mut slots = HashMap::new();
        start_scheduled_refresh(&mut slots, ProviderId::Claude, || tokio::spawn(async {})).await;
        while !slots.get(&ProviderId::Claude).unwrap().is_finished() {
            tokio::task::yield_now().await;
        }
        let mut started = 0;
        start_scheduled_refresh(&mut slots, ProviderId::Claude, || {
            started += 1;
            tokio::spawn(async {})
        })
        .await;
        assert_eq!(started, 1);
        assert!(slots.contains_key(&ProviderId::Claude));

        let (_hold, wait) = tokio::sync::oneshot::channel::<()>();
        slots.insert(
            ProviderId::Claude,
            tokio::spawn(async move {
                let _ = wait.await;
            }),
        );
        start_scheduled_refresh(&mut slots, ProviderId::Claude, || {
            started += 1;
            tokio::spawn(async {})
        })
        .await;
        assert_eq!(started, 1, "a running refresh is not duplicated");
        slots.remove(&ProviderId::Claude).unwrap().abort();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn greptile_6_retry_after_zero_means_probe_now() {
        let app = Arc::new(RwLock::new(test_app()));
        apply_provider_snapshot(&app, rate_limited(ProviderId::Claude, Some(0))).await;

        assert!(!provider_in_backoff(&app, ProviderId::Claude).await);
        let max = app.read().await.cfg.refresh_max_secs();
        let g = app.read().await;
        let stored = g.store.latest().unwrap().by_id(ProviderId::Claude).unwrap();
        assert_ne!(stored.retry_after_secs, Some(max));
        drop(g);

        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls_probe = calls.clone();
        refresh_provider_with(app.clone(), ProviderId::Claude, move |_, mode| async move {
            assert_eq!(mode, ProbeMode::Full);
            calls_probe.fetch_add(1, Ordering::Relaxed);
            Some(observed_reading(ProviderId::Claude, 5.0))
        })
        .await;
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn coderabbit_every_retry_deadline_has_a_finite_ceiling() {
        let ceiling = tokio::time::Instant::now() + Duration::from_secs(MAX_RETRY_AFTER_SECS + 1);
        let RetryAfterDeadline(deadline) = RetryAfterDeadline::from_secs(u64::MAX);
        assert!(deadline < ceiling);
        assert!(!RetryAfterDeadline::from_secs(u64::MAX).is_active(ceiling));

        let app = Arc::new(RwLock::new(test_app()));
        apply_provider_snapshot(&app, rate_limited(ProviderId::Codex, Some(u64::MAX))).await;
        let RetryAfterDeadline(stored) =
            app.read().await.retry_after_until[&ProviderId::Codex].until;
        assert!(stored < ceiling);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn coderabbit_file_fallback_is_collected_during_http_backoff() {
        let app = Arc::new(RwLock::new(test_app()));
        apply_provider_snapshot(&app, rate_limited(ProviderId::Codex, Some(120))).await;
        apply_provider_snapshot(&app, rate_limited(ProviderId::Claude, Some(120))).await;
        let deadline = app.read().await.retry_after_until[&ProviderId::Codex].clone();

        let modes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let modes_probe = modes.clone();
        refresh_provider_with(app.clone(), ProviderId::Codex, move |_, mode| async move {
            modes_probe.lock().unwrap().push(mode);
            let mut file = observed_reading(ProviderId::Codex, 61.0);
            file.source = Some(quota_core::types::Source::File);
            Some(file)
        })
        .await;
        let claude_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let claude_calls_probe = claude_calls.clone();
        refresh_provider_with(app.clone(), ProviderId::Claude, move |_, _| async move {
            claude_calls_probe.fetch_add(1, Ordering::Relaxed);
            None
        })
        .await;

        assert_eq!(*modes.lock().unwrap(), [ProbeMode::FileOnly]);
        assert_eq!(claude_calls.load(Ordering::Relaxed), 0);
        let g = app.read().await;
        assert_eq!(g.retry_after_until[&ProviderId::Codex], deadline);
        let codex = g.store.latest().unwrap().by_id(ProviderId::Codex).unwrap();
        assert_eq!(codex.source, Some(quota_core::types::Source::File));
        assert_eq!(codex.windows[0].used_percent, Some(61.0));
        assert!(codex.retry_after_secs.is_some_and(|secs| secs > 0));
    }

    const FIXTURE_ACCOUNT: &str = "072a8214-59be-4a01-a982-e42f80531441";

    fn codex_home_for(account_id: &str) -> PathBuf {
        let home = unique_test_dir("quota-file-only");
        fs::write(
            home.join("auth.json"),
            format!(r#"{{"tokens":{{"access_token":"test-token","account_id":"{account_id}"}}}}"#),
        )
        .unwrap();
        home
    }

    fn file_only_plan(home: &Path) -> ProbePlan {
        ProbePlan {
            timeout: 1,
            cursor_secret_path: "cursor/session".into(),
            codex_home: Some(home.to_path_buf()),
            claude_home: None,
            codexbar_dir: quota_adapters::codexbar::workspace_fixtures_dir(),
            enable_codexbar_files: true,
        }
    }

    #[test]
    fn coderabbit_file_only_probe_keeps_the_same_accounts_refusal() {
        use quota_adapters::creds::account_digest;
        use quota_core::types::ProviderPermission;

        let home = codex_home_for(FIXTURE_ACCOUNT);
        let owner = account_digest(FIXTURE_ACCOUNT);
        let mut refused = rate_limited(ProviderId::Codex, Some(120));
        refused.permission = ProviderPermission::LimitReached;
        refused.account_digest = Some(account_digest(FIXTURE_ACCOUNT));

        let same =
            collect_codex_in_backoff(file_only_plan(&home), Some(refused.clone()), Some(&owner))
                .owner_reading()
                .unwrap();
        assert_eq!(same.source, Some(quota_core::types::Source::File));
        assert_eq!(same.permission, ProviderPermission::LimitReached);

        refused.account_digest = Some(account_digest("someone-else"));
        let other = collect_codex_in_backoff(file_only_plan(&home), Some(refused), Some(&owner))
            .owner_reading()
            .unwrap();
        assert_eq!(other.permission, ProviderPermission::Unknown);

        let unmatched_id = "11111111-2222-3333-4444-555555555555";
        let unmatched = codex_home_for(unmatched_id);
        let unmatched_owner = account_digest(unmatched_id);
        assert!(
            collect_codex_in_backoff(file_only_plan(&unmatched), None, Some(&unmatched_owner))
                .owner_reading()
                .is_none()
        );
        let _ = fs::remove_dir_all(home);
        let _ = fs::remove_dir_all(unmatched);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn headerless_codex_429_with_file_fallback_keeps_backoff() {
        use quota_adapters::http::{HttpResponse, MockTransport};
        use quota_adapters::provider::ProbeCtx;

        let home = codex_home_for(FIXTURE_ACCOUNT);
        let transport = MockTransport {
            next: Some(Ok(HttpResponse {
                status: 429,
                body: b"{}".to_vec(),
                retry_after_secs: None,
            })),
            last_url: std::sync::Mutex::new(None),
        };
        let codex = codex_adapter(file_only_plan(&home)).probe(&ProbeCtx {
            transport: &transport,
            now: now_unix(),
        });
        assert_eq!(codex.source, Some(quota_core::types::Source::File));

        let app = Arc::new(RwLock::new(test_app()));
        let max = app.read().await.cfg.refresh_max_secs();
        apply_provider_snapshot(&app, codex).await;

        assert!(provider_in_backoff(&app, ProviderId::Codex).await);
        let g = app.read().await;
        let stored = g.store.latest().unwrap().by_id(ProviderId::Codex).unwrap();
        assert_eq!(stored.source, Some(quota_core::types::Source::File));
        assert_eq!(stored.retry_after_secs, Some(max));
        let _ = fs::remove_dir_all(home);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn coderabbit_account_switch_drops_the_previous_accounts_deadline_and_reading() {
        let app = Arc::new(RwLock::new(test_app()));
        add_codex_account(&app, "acct_a", true).await;
        apply_provider_snapshot(&app, observed_reading(ProviderId::Codex, 80.0)).await;
        apply_provider_snapshot(&app, rate_limited(ProviderId::Codex, Some(120))).await;
        assert!(provider_in_backoff(&app, ProviderId::Codex).await);

        add_codex_account(&app, "acct_b", true).await;

        assert!(!provider_in_backoff(&app, ProviderId::Codex).await);
        let status = dispatch(&app, Request::new(41, METHOD_STATUS)).await;
        let status: StatusResult = serde_json::from_value(status.result.unwrap()).unwrap();
        let codex = status.snapshot.by_id(ProviderId::Codex).unwrap();
        assert_eq!(codex.error.as_ref().unwrap().code, "account_switched");
        assert_eq!(codex.retry_after_secs, None);

        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls_probe = calls.clone();
        refresh_provider_with(app.clone(), ProviderId::Codex, move |_, mode| async move {
            assert_eq!(mode, ProbeMode::Full);
            calls_probe.fetch_add(1, Ordering::Relaxed);
            Some(observed_reading(ProviderId::Codex, 7.0))
        })
        .await;
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(latest_codex_used(&*app.read().await), Some(7.0));
    }

    fn unreadable_codex_reading(
        observed_at: i64,
        permission: quota_core::types::ProviderPermission,
    ) -> ProviderSnapshot {
        ProviderSnapshot::observed(ProviderObservation {
            provider: ProviderId::Codex,
            source: Some(quota_core::types::Source::Oauth),
            windows: vec![quota_core::types::UsageWindow::unreadable(
                quota_core::types::WindowKind::Session,
                "5h",
                None,
                None,
                Some(observed_at),
                300,
            )],
            credits: None,
            plan: None,
            credential_path: None,
            observed_at: Some(observed_at),
            max_age_secs: 300,
            permission,
        })
    }

    async fn all_can_start(app: &Arc<RwLock<App>>) -> CanStartResult {
        let req = Request::with_params(
            54,
            METHOD_CAN_START,
            CanStartParams {
                tokens: 0,
                percent: None,
                reserve: None,
                deadline: None,
                provider: ProviderFilter::All,
            },
        );
        serde_json::from_value(dispatch(app, req).await.result.unwrap()).unwrap()
    }

    async fn app_with_current_claude_reading() -> (Arc<RwLock<App>>, PathBuf) {
        let dir = unique_test_dir("quota-test-acct");
        write_claude_credentials(&dir);
        write_unnamed_codex_credentials(&dir);
        let app = Arc::new(RwLock::new(test_app_in(&dir)));
        apply_provider_snapshot(&app, observed_reading(ProviderId::Claude, 10.0)).await;
        (app, dir)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn greptile_7_a_stale_unreadable_provider_does_not_veto_a_current_approval() {
        use quota_core::types::CanStartBasis;

        let (app, dir) = app_with_current_claude_reading().await;
        let long_ago = now_unix() - 10_000;
        apply_provider_snapshot(
            &app,
            unreadable_codex_reading(long_ago, quota_core::types::ProviderPermission::Unknown),
        )
        .await;

        let all = all_can_start(&app).await;

        let codex = all.answers.iter().find(|a| a.provider == ProviderId::Codex);
        assert_eq!(codex.unwrap().basis, CanStartBasis::Unavailable);
        assert!(!codex.unwrap().ok);
        assert!(all
            .answers
            .iter()
            .any(|a| a.provider == ProviderId::Claude && a.ok));
        assert!(all.ok, "{all:?}");
        assert!(!codex_can_start(&app, 0).await.ok);
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn greptile_7_a_current_unreadable_provider_still_vetoes_the_all_provider_answer() {
        use quota_core::types::CanStartBasis;

        let (app, dir) = app_with_current_claude_reading().await;
        apply_provider_snapshot(
            &app,
            unreadable_codex_reading(now_unix(), quota_core::types::ProviderPermission::Unknown),
        )
        .await;

        let all = all_can_start(&app).await;

        let codex = all.answers.iter().find(|a| a.provider == ProviderId::Codex);
        assert_eq!(codex.unwrap().basis, CanStartBasis::UnknownWindow);
        assert!(!all.ok, "{all:?}");
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn greptile_7_an_explicit_refusal_reads_as_a_refusal_beside_an_unreadable_window() {
        use quota_core::types::CanStartBasis;

        let (app, dir) = app_with_current_claude_reading().await;
        apply_provider_snapshot(
            &app,
            unreadable_codex_reading(
                now_unix(),
                quota_core::types::ProviderPermission::LimitReached,
            ),
        )
        .await;

        let codex = codex_can_start(&app, 0).await.answers.remove(0);

        assert!(!codex.ok);
        assert_eq!(codex.basis, CanStartBasis::Unavailable);
        assert!(codex.explanation.contains("limit_reached"), "{codex:?}");
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn greptile_7_a_stalled_provider_does_not_withhold_the_finished_one() {
        let app = Arc::new(RwLock::new(test_app()));
        let mut watch = app.read().await.watch_tx.subscribe();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let release = Arc::new(std::sync::Mutex::new(Some(release_rx)));

        let cycle = refresh_providers_within(
            app.clone(),
            &[ProviderId::Codex, ProviderId::Claude],
            Duration::from_millis(20),
            move |_, provider, _| {
                let release = release.clone();
                async move {
                    if provider == ProviderId::Codex {
                        let held = release.lock().unwrap().take();
                        if let Some(held) = held {
                            let _ = held.await;
                        }
                        return Some(observed_reading(provider, 20.0));
                    }
                    Some(observed_reading(provider, 10.0))
                }
            },
        );
        let observer = async {
            let early = tokio::time::timeout(Duration::from_secs(5), watch.recv())
                .await
                .expect("the finished provider is published while the other stalls")
                .unwrap();
            let claude = early.by_id(ProviderId::Claude).unwrap();
            assert_eq!(claude.windows[0].used_percent, Some(10.0));
            assert!(early.by_id(ProviderId::Codex).unwrap().windows.is_empty());
            assert_eq!(
                latest_codex_used(&*app.read().await),
                None,
                "the stalled provider has published nothing yet"
            );
            release_tx.send(()).unwrap();
            let late = tokio::time::timeout(Duration::from_secs(5), watch.recv())
                .await
                .expect("the stalled provider publishes when it finishes")
                .unwrap();
            assert_eq!(
                late.by_id(ProviderId::Codex).unwrap().windows[0].used_percent,
                Some(20.0)
            );
            assert_eq!(
                late.by_id(ProviderId::Claude).unwrap().windows[0].used_percent,
                Some(10.0)
            );
        };

        tokio::join!(cycle, observer);
    }

    async fn switch_accounts_during_a_probe(observe_before_switch: bool) {
        let app = Arc::new(RwLock::new(test_app()));
        add_codex_account(&app, "acct_a", true).await;
        let mut watch = app.read().await.watch_tx.subscribe();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let old_account_probe =
            refresh_provider_with(app.clone(), ProviderId::Codex, move |_, _| async move {
                let _ = started_tx.send(());
                let _ = release_rx.await;
                Some(observed_reading(ProviderId::Codex, 99.0))
            });
        let switch_then_refresh = async {
            started_rx.await.unwrap();
            let requested_before = observe(&*app.read().await, ProviderId::Codex);
            add_codex_account(&app, "acct_b", true).await;
            let request = if observe_before_switch {
                requested_before
            } else {
                observe(&*app.read().await, ProviderId::Codex)
            };
            let new_account_probe = async {
                let claim = request
                    .claim()
                    .await
                    .expect("not answered by the old probe");
                let probed = probe_claimed(&app, claim, |_, _| async {
                    Some(observed_reading(ProviderId::Codex, 42.0))
                })
                .await;
                publish(&app, probed.into_iter().collect()).await;
            };
            let release = async {
                tokio::task::yield_now().await;
                let _ = release_tx.send(());
            };
            tokio::join!(new_account_probe, release);
        };

        tokio::join!(old_account_probe, switch_then_refresh);

        assert_eq!(latest_codex_used(&*app.read().await), Some(42.0));
        let mut last = None;
        while let Ok(update) = watch.try_recv() {
            let codex = update.by_id(ProviderId::Codex).unwrap();
            assert!(
                codex.windows.iter().all(|w| w.used_percent != Some(99.0)),
                "the old account's reading was broadcast"
            );
            last = codex.windows.first().and_then(|w| w.used_percent);
        }
        assert_eq!(last, Some(42.0));
        let g = app.read().await;
        assert!(g.store.history_ref().iter().all(|snapshot| snapshot
            .by_id(ProviderId::Codex)
            .is_none_or(|codex| codex.windows.iter().all(|w| w.used_percent != Some(99.0)))));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn greptile_7_a_refresh_requested_after_an_account_switch_is_not_lost_to_the_old_probe() {
        switch_accounts_during_a_probe(false).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn greptile_7_a_refresh_requested_before_an_account_switch_still_probes_the_new_account()
    {
        switch_accounts_during_a_probe(true).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn greptile_7_a_stated_zero_retry_after_is_not_replaced_by_the_maximum_delay() {
        for provider in [ProviderId::Codex, ProviderId::Claude] {
            let app = Arc::new(RwLock::new(test_app()));
            let max = app.read().await.cfg.refresh_max_secs();
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));

            for _ in 0..2 {
                let calls = calls.clone();
                refresh_providers(app.clone(), &[provider], move |_, _, mode| {
                    calls.fetch_add(1, Ordering::Relaxed);
                    async move {
                        assert_eq!(mode, ProbeMode::Full);
                        Some(rate_limited(provider, Some(0)))
                    }
                })
                .await;
            }

            assert_eq!(calls.load(Ordering::Relaxed), 2, "{provider}");
            assert!(!provider_in_backoff(&app, provider).await);
            let g = app.read().await;
            let stored = g.store.latest().unwrap().by_id(provider).unwrap();
            assert_eq!(stored.retry_after_secs, Some(0));
            assert_ne!(stored.retry_after_secs, Some(max));
            assert_eq!(stored.retry_after_until, None);
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn greptile_7_a_headerless_rate_limit_still_backs_off_for_the_maximum_delay() {
        let app = Arc::new(RwLock::new(test_app()));
        let max = app.read().await.cfg.refresh_max_secs();

        apply_provider_snapshot(&app, rate_limited(ProviderId::Claude, None)).await;

        assert!(provider_in_backoff(&app, ProviderId::Claude).await);
        let g = app.read().await;
        let stored = g.store.latest().unwrap().by_id(ProviderId::Claude).unwrap();
        assert_eq!(stored.retry_after_secs, Some(max));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn coderabbit_one_history_entry_and_one_watch_update_per_refresh_cycle() {
        let app = Arc::new(RwLock::new(test_app()));
        let mut rx = app.read().await.watch_tx.subscribe();
        let before = app.read().await.store.history_ref().len();

        refresh_providers(
            app.clone(),
            &[ProviderId::Codex, ProviderId::Claude],
            |_, provider, _| async move { Some(observed_reading(provider, 10.0)) },
        )
        .await;

        assert_eq!(app.read().await.store.history_ref().len(), before + 1);
        let update = rx.try_recv().unwrap();
        assert!(update.by_id(ProviderId::Codex).is_some());
        assert!(update.by_id(ProviderId::Claude).is_some());
        assert!(matches!(
            rx.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
    }

    async fn send_request(stream: &mut UnixStream, req: &Request) {
        let frame = encode_frame(&serde_json::to_vec(req).unwrap()).unwrap();
        stream.write_all(&frame).await.unwrap();
    }

    async fn recv_json(stream: &mut UnixStream) -> serde_json::Value {
        let bytes = tokio::time::timeout(Duration::from_secs(10), read_frame_async(stream))
            .await
            .expect("frame before timeout")
            .expect("frame");
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn watch_client_observes_current_then_stale_without_a_refresh() {
        use quota_core::types::{ProviderPermission, UsageWindow, WindowKind};

        let dir = unique_test_dir("quota-test-acct");
        write_unnamed_codex_credentials(&dir);
        let mut app = test_app_in(&dir);
        let now = now_unix();
        app.store.push(Snapshot::new(
            now,
            vec![ProviderSnapshot::observed(ProviderObservation {
                provider: ProviderId::Codex,
                source: None,
                windows: vec![UsageWindow::from_percent_at(
                    WindowKind::Weekly,
                    "weekly",
                    10.0,
                    None,
                    None,
                    Some(now),
                    2,
                )],
                credits: None,
                plan: None,
                credential_path: None,
                observed_at: Some(now),
                max_age_secs: 2,
                permission: ProviderPermission::Allowed,
            })],
        ));
        let app = Arc::new(RwLock::new(app));
        let history_before = app.read().await.store.history_ref().len();
        let (mut client, server) = UnixStream::pair().unwrap();
        let slots = || Arc::new(Semaphore::new(1));
        let server = handle_client(app.clone(), server, slots(), slots());
        let client = async move {
            send_request(&mut client, &Request::new(1, METHOD_WATCH)).await;
            let first = recv_json(&mut client).await;
            let second = recv_json(&mut client).await;
            (first, second)
        };

        let (served, (first, second)) = tokio::join!(server, client);

        let freshness = |frame: &serde_json::Value| {
            frame["result"]["snapshot"]["providers"][0]["freshness"].clone()
        };
        assert_eq!(freshness(&first), "current");
        assert_eq!(freshness(&second), "stale");
        for frame in [&first, &second] {
            assert_eq!(
                frame["result"]["snapshot"]["providers"][0]["windows"][0]["used_percent"],
                10.0
            );
        }
        assert!(matches!(served, Err(ClientError::Eof)));
        assert_eq!(app.read().await.store.history_ref().len(), history_before);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn coderabbit_watch_partial_frame_survives_a_snapshot_write() {
        let app = Arc::new(RwLock::new(test_app()));
        let (mut client, server) = UnixStream::pair().unwrap();
        let slots = || Arc::new(Semaphore::new(1));
        let server = handle_client(app.clone(), server, slots(), slots());
        let watch_tx = app.read().await.watch_tx.clone();
        let client = async move {
            send_request(&mut client, &Request::new(1, METHOD_WATCH)).await;
            let _initial = recv_json(&mut client).await;
            let ping =
                encode_frame(&serde_json::to_vec(&Request::new(7, METHOD_PING)).unwrap()).unwrap();
            client.write_all(&ping[..6]).await.unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
            watch_tx
                .send(Snapshot::new(now_unix(), Vec::new()))
                .unwrap();
            let pushed = recv_json(&mut client).await;
            client.write_all(&ping[6..]).await.unwrap();
            let pong = recv_json(&mut client).await;
            (pushed, pong)
        };

        let (served, (pushed, pong)) = tokio::join!(server, client);

        assert!(pushed["result"]["snapshot"].is_object());
        assert_eq!(pong["id"], 7);
        assert_eq!(pong["result"]["pong"], true);
        assert!(matches!(served, Err(ClientError::Eof)));
    }

    const ROTATED_ACCOUNT: &str = "11111111-2222-3333-4444-555555555555";

    fn write_codex_auth(home: &Path, account_id: &str) {
        fs::write(
            home.join("auth.json"),
            format!(r#"{{"tokens":{{"access_token":"test-token","account_id":"{account_id}"}}}}"#),
        )
        .unwrap();
    }

    #[test]
    fn review_r2_same_home_credential_rotation_ends_the_previous_accounts_backoff() {
        use quota_adapters::creds::account_digest;

        let home = codex_home_for(FIXTURE_ACCOUNT);
        let owner = account_digest(FIXTURE_ACCOUNT);
        let file = collect_codex_in_backoff(file_only_plan(&home), None, Some(&owner))
            .owner_reading()
            .unwrap();
        assert_eq!(file.account_digest.as_deref(), Some(owner.as_str()));

        write_codex_auth(&home, ROTATED_ACCOUNT);
        assert!(matches!(
            collect_codex_in_backoff(file_only_plan(&home), None, Some(&owner)),
            BackoffProbe::OtherAccount
        ));
        let _ = fs::remove_dir_all(home);
    }

    static ROTATION_FULL_PROBES: AtomicU64 = AtomicU64::new(0);

    /// Stands in for the network probe: a reading for whichever account the
    /// plan's credentials name.
    fn rotation_full_probe(plan: ProbePlan, provider: ProviderId) -> ProviderSnapshot {
        ROTATION_FULL_PROBES.fetch_add(1, Ordering::Relaxed);
        let mut reading = observed_reading(provider, 7.0);
        reading.account_digest = codex_adapter(plan)
            .active_identity()
            .digest()
            .map(str::to_owned);
        reading
    }

    #[tokio::test(flavor = "current_thread")]
    async fn review_r2_same_home_credential_rotation_during_backoff_probes_the_new_account() {
        use quota_adapters::creds::account_digest;

        let home = codex_home_for(FIXTURE_ACCOUNT);
        let mut app = test_app();
        app.cfg.codexbar_dir = Some(quota_adapters::codexbar::workspace_fixtures_dir());
        let app = Arc::new(RwLock::new(app));
        let add = Request::with_params(
            42,
            METHOD_ACCOUNTS_ADD,
            quota_core::AccountsAddParams {
                id: Some("same_home".into()),
                provider: ProviderId::Codex,
                email: None,
                workspace_label: None,
                login_method: None,
                workspace_account_id: None,
                secret_ref: None,
                home_path: Some(home.display().to_string()),
                select: true,
            },
        );
        assert!(dispatch(&app, add).await.ok);
        let mut refused = rate_limited(ProviderId::Codex, Some(120));
        refused.account_digest = Some(account_digest(FIXTURE_ACCOUNT));
        apply_provider_snapshot(&app, refused).await;
        let deadline = app.read().await.retry_after_until[&ProviderId::Codex].clone();
        let probe =
            |app, mode| probe_provider_with(app, ProviderId::Codex, mode, rotation_full_probe);

        refresh_provider_with(app.clone(), ProviderId::Codex, probe).await;
        assert_eq!(ROTATION_FULL_PROBES.load(Ordering::Relaxed), 0);
        {
            let g = app.read().await;
            let codex = g.store.latest().unwrap().by_id(ProviderId::Codex).unwrap();
            assert_eq!(codex.source, Some(quota_core::types::Source::File));
            assert_eq!(codex.retry_after_until, Some(deadline.until_unix));
            assert_eq!(g.retry_after_until[&ProviderId::Codex], deadline);
        }

        write_codex_auth(&home, ROTATED_ACCOUNT);
        refresh_provider_with(app.clone(), ProviderId::Codex, probe).await;

        assert_eq!(ROTATION_FULL_PROBES.load(Ordering::Relaxed), 1);
        assert!(!provider_in_backoff(&app, ProviderId::Codex).await);
        let g = app.read().await;
        assert!(!g.retry_after_until.contains_key(&ProviderId::Codex));
        let codex = g.store.latest().unwrap().by_id(ProviderId::Codex).unwrap();
        assert_eq!(codex.account_digest, Some(account_digest(ROTATED_ACCOUNT)));
        assert_eq!(codex.retry_after_secs, None);
        assert_eq!(codex.retry_after_until, None);
        drop(g);
        let _ = fs::remove_dir_all(home);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn review_r2_file_only_result_for_another_account_never_carries_the_stored_deadline() {
        let app = Arc::new(RwLock::new(test_app()));
        let mut refused = rate_limited(ProviderId::Codex, Some(120));
        refused.account_digest = Some("a".repeat(64));
        apply_provider_snapshot(&app, refused).await;
        let stored_until = {
            let g = app.read().await;
            g.store
                .latest()
                .unwrap()
                .by_id(ProviderId::Codex)
                .unwrap()
                .retry_after_until
        };
        assert!(stored_until.is_some());

        refresh_provider_with(app.clone(), ProviderId::Codex, |_, mode| async move {
            assert_eq!(mode, ProbeMode::FileOnly);
            let mut file = observed_reading(ProviderId::Codex, 30.0);
            file.source = Some(quota_core::types::Source::File);
            file.account_digest = Some("b".repeat(64));
            Some(file)
        })
        .await;

        assert!(!provider_in_backoff(&app, ProviderId::Codex).await);
        let g = app.read().await;
        assert!(!g.retry_after_until.contains_key(&ProviderId::Codex));
        let codex = g.store.latest().unwrap().by_id(ProviderId::Codex).unwrap();
        assert_eq!(codex.windows[0].used_percent, Some(30.0));
        assert_eq!(codex.retry_after_secs, None);
        assert_eq!(codex.retry_after_until, None);
    }

    fn reading_at(provider: ProviderId, used: f64, observed_at: i64) -> ProviderSnapshot {
        use quota_core::types::{ProviderPermission, Source, UsageWindow, WindowKind};
        ProviderSnapshot::observed(ProviderObservation {
            provider,
            source: Some(Source::Oauth),
            windows: vec![UsageWindow::from_percent_at(
                WindowKind::Weekly,
                "weekly",
                used,
                None,
                None,
                Some(observed_at),
                300,
            )],
            credits: None,
            plan: None,
            credential_path: None,
            observed_at: Some(observed_at),
            max_age_secs: 300,
            permission: ProviderPermission::Allowed,
        })
    }

    async fn claude_pace(app: &Arc<RwLock<App>>) -> quota_core::types::PaceReport {
        let req = Request::with_params(
            44,
            METHOD_PACE,
            PaceParams {
                provider: ProviderFilter::Claude,
            },
        );
        let pace: PaceResult =
            serde_json::from_value(dispatch(app, req).await.result.unwrap()).expect("pace result");
        pace.reports.into_iter().next().expect("claude pace report")
    }

    #[tokio::test(flavor = "current_thread")]
    async fn review_r3_unknown_account_history_does_not_cross_an_accounts_select() {
        let dir = unique_test_dir("quota-test-acct");
        write_claude_credentials(&dir);
        let app = Arc::new(RwLock::new(test_app_in(&dir)));
        add_account(&app, ProviderId::Claude, "claude_a", true).await;
        add_account(&app, ProviderId::Claude, "claude_b", false).await;
        let now = now_unix();
        apply_provider_snapshot(&app, reading_at(ProviderId::Claude, 10.0, now - 100)).await;
        apply_provider_snapshot(&app, reading_at(ProviderId::Claude, 60.0, now - 50)).await;
        let before_switch = claude_pace(&app).await;
        assert_eq!(before_switch.samples, 2);
        assert!(before_switch.burn_percent_per_hour.is_some());

        let select = Request::with_params(
            45,
            METHOD_ACCOUNTS_SELECT,
            AccountsSelectParams {
                id: Some("claude_b".into()),
            },
        );
        assert!(dispatch(&app, select).await.ok);
        apply_provider_snapshot(&app, reading_at(ProviderId::Claude, 65.0, now)).await;

        let after_switch = claude_pace(&app).await;
        assert_eq!(after_switch.samples, 1);
        assert_eq!(after_switch.burn_percent_per_hour, None);
        let req = Request::with_params(
            46,
            METHOD_CAN_START,
            CanStartParams {
                tokens: 0,
                percent: None,
                reserve: None,
                deadline: None,
                provider: ProviderFilter::Claude,
            },
        );
        let answer: CanStartResult =
            serde_json::from_value(dispatch(&app, req).await.result.unwrap()).unwrap();
        assert_eq!(answer.answers[0].burn_percent_per_hour, None);
        assert_eq!(answer.answers[0].eta_empty_secs, None);
        let g = app.read().await;
        assert!(g.store.history_ref().iter().all(|snapshot| snapshot
            .by_id(ProviderId::Claude)
            .is_none_or(|claude| claude.windows.iter().all(|w| w.used_percent == Some(65.0)))));
    }

    /// The first request blocks until released and then answers 429 with a
    /// deadline; any later request answers with a readable weekly window.
    struct BlockedTransport {
        entered: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
        calls: AtomicU64,
    }

    impl quota_adapters::http::Transport for BlockedTransport {
        fn get(
            &self,
            _url: &str,
            _headers: &[(&str, &str)],
        ) -> Result<quota_adapters::http::HttpResponse, quota_adapters::http::TransportError>
        {
            use quota_adapters::http::HttpResponse;
            if self.calls.fetch_add(1, Ordering::Relaxed) == 0 {
                if let Some(entered) = self.entered.lock().unwrap().take() {
                    let _ = entered.send(());
                }
                let _ = self.release.lock().unwrap().recv();
                return Ok(HttpResponse {
                    status: 429,
                    body: b"{}".to_vec(),
                    retry_after_secs: Some(120),
                });
            }
            Ok(HttpResponse {
                status: 200,
                body: br#"{"rate_limit":{"allowed":true,"limit_reached":false,
                    "secondary_window":{"used_percent":33,"limit_window_seconds":604800}}}"#
                    .to_vec(),
                retry_after_secs: None,
            })
        }
    }

    async fn codex_can_start(app: &Arc<RwLock<App>>, tokens: u64) -> CanStartResult {
        let req = Request::with_params(
            48,
            METHOD_CAN_START,
            CanStartParams {
                tokens,
                percent: None,
                reserve: None,
                deadline: None,
                provider: ProviderFilter::Codex,
            },
        );
        serde_json::from_value(dispatch(app, req).await.result.unwrap()).unwrap()
    }

    async fn codex_status(app: &Arc<RwLock<App>>) -> ProviderSnapshot {
        let req = Request::with_params(
            49,
            METHOD_STATUS,
            StatusParams {
                provider: ProviderFilter::Codex,
            },
        );
        let status: StatusResult =
            serde_json::from_value(dispatch(app, req).await.result.unwrap()).unwrap();
        status.snapshot.by_id(ProviderId::Codex).unwrap().clone()
    }

    async fn codex_pace(app: &Arc<RwLock<App>>) -> quota_core::types::PaceReport {
        let req = Request::with_params(
            50,
            METHOD_PACE,
            PaceParams {
                provider: ProviderFilter::Codex,
            },
        );
        let pace: PaceResult =
            serde_json::from_value(dispatch(app, req).await.result.unwrap()).unwrap();
        pace.reports.into_iter().next().expect("codex pace report")
    }

    /// Before round 4 the probe re-read the credentials and retried, so the
    /// old account's response was never published. Publication no longer
    /// decides the account: the old response is published under the account
    /// it was requested as, no consumer answers for the new account from it,
    /// and its deadline does not hold the new account back.
    #[tokio::test(flavor = "current_thread")]
    async fn review_r3_auth_rotation_while_a_full_probe_is_blocked_answers_only_for_the_new_account(
    ) {
        use quota_adapters::creds::account_digest;

        let home = codex_home_for(FIXTURE_ACCOUNT);
        let mut app = test_app();
        app.cfg.enable_codexbar_files = false;
        let app = Arc::new(RwLock::new(app));
        let add = Request::with_params(
            47,
            METHOD_ACCOUNTS_ADD,
            quota_core::AccountsAddParams {
                id: Some("same_home".into()),
                provider: ProviderId::Codex,
                email: None,
                workspace_label: None,
                login_method: None,
                workspace_account_id: None,
                secret_ref: None,
                home_path: Some(home.display().to_string()),
                select: true,
            },
        );
        assert!(dispatch(&app, add).await.ok);
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let transport = Arc::new(BlockedTransport {
            entered: std::sync::Mutex::new(Some(entered_tx)),
            release: std::sync::Mutex::new(release_rx),
            calls: AtomicU64::new(0),
        });
        let probe_through = |transport: Arc<BlockedTransport>| {
            move |app, mode| {
                probe_provider_with(app, ProviderId::Codex, mode, move |plan: ProbePlan, _| {
                    codex_adapter(plan).probe(&ProbeCtx {
                        transport: &*transport,
                        now: now_unix(),
                    })
                })
            }
        };
        let rotate_while_blocked = async {
            entered_rx.await.unwrap();
            write_codex_auth(&home, ROTATED_ACCOUNT);
            release_tx.send(()).unwrap();
        };

        tokio::join!(
            refresh_provider_with(
                app.clone(),
                ProviderId::Codex,
                probe_through(transport.clone())
            ),
            rotate_while_blocked
        );

        let old = Some(account_digest(FIXTURE_ACCOUNT));
        assert_eq!(transport.calls.load(Ordering::Relaxed), 1);
        {
            let g = app.read().await;
            let stored = g.store.latest().unwrap().by_id(ProviderId::Codex).unwrap();
            assert_eq!(stored.account_digest, old);
            assert_eq!(stored.error.as_ref().unwrap().code, "rate_limited");
        }
        let refused = codex_can_start(&app, 0).await;
        assert!(!refused.ok);
        assert_eq!(
            refused.answers[0].basis,
            quota_core::types::CanStartBasis::AccountChanged
        );
        let shown = codex_status(&app).await;
        assert!(shown.is_for_another_account());
        assert_eq!(shown.account_digest, old);
        assert_eq!(shown.retry_after_secs, None);

        refresh_provider_with(
            app.clone(),
            ProviderId::Codex,
            probe_through(transport.clone()),
        )
        .await;

        assert_eq!(transport.calls.load(Ordering::Relaxed), 2);
        assert!(!provider_in_backoff(&app, ProviderId::Codex).await);
        {
            let g = app.read().await;
            assert!(!g.retry_after_until.contains_key(&ProviderId::Codex));
            let codex = g.store.latest().unwrap().by_id(ProviderId::Codex).unwrap();
            assert_eq!(codex.account_digest, Some(account_digest(ROTATED_ACCOUNT)));
            assert_eq!(codex.windows[0].used_percent, Some(33.0));
            assert_eq!(codex.retry_after_secs, None);
            assert_eq!(codex.retry_after_until, None);
        }
        let admitted = codex_can_start(&app, 0).await;
        assert!(admitted.ok);
        assert_eq!(admitted.answers[0].remaining_percent, Some(67.0));
        let pace = codex_pace(&app).await;
        assert_eq!(pace.samples, 1);
        assert_eq!(pace.used_percent, Some(33.0));
        let _ = fs::remove_dir_all(home);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn review_r3_concurrent_two_provider_refreshes_share_one_probe_per_provider() {
        let app = Arc::new(RwLock::new(test_app()));
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let started = Arc::new(std::sync::Mutex::new(Some(started_tx)));
        let release = Arc::new(std::sync::Mutex::new(Some(release_rx)));
        let first_calls = calls.clone();
        let first = refresh_providers(
            app.clone(),
            &[ProviderId::Codex, ProviderId::Claude],
            move |_, provider, _| {
                first_calls.lock().unwrap().push(provider);
                let started = started.clone();
                let release = release.clone();
                async move {
                    if provider == ProviderId::Codex {
                        if let Some(started) = started.lock().unwrap().take() {
                            let _ = started.send(());
                        }
                        let release = release.lock().unwrap().take();
                        if let Some(release) = release {
                            let _ = release.await;
                        }
                    }
                    Some(observed_reading(provider, 10.0))
                }
            },
        );
        let second_calls = calls.clone();
        let second = async {
            started_rx.await.unwrap();
            let second_cycle = refresh_providers(
                app.clone(),
                &[ProviderId::Codex, ProviderId::Claude],
                move |_, provider, _| {
                    second_calls.lock().unwrap().push(provider);
                    async move { Some(observed_reading(provider, 20.0)) }
                },
            );
            let release_first = async {
                tokio::task::yield_now().await;
                let _ = release_tx.send(());
            };
            tokio::join!(second_cycle, release_first);
        };

        tokio::join!(first, second);

        assert_eq!(
            *calls.lock().unwrap(),
            [ProviderId::Codex, ProviderId::Claude]
        );
        let g = app.read().await;
        let latest = g.store.latest().unwrap();
        for provider in [ProviderId::Codex, ProviderId::Claude] {
            let used = latest.by_id(provider).unwrap().windows[0].used_percent;
            assert_eq!(used, Some(10.0));
        }
    }

    /// Answers every request with the usage of the account it was made as,
    /// named by its `ChatGPT-Account-Id` header.
    struct PerAccountTransport {
        responses: HashMap<&'static str, quota_adapters::http::HttpResponse>,
        requests: std::sync::Mutex<Vec<String>>,
    }

    fn weekly_usage(used: u32) -> quota_adapters::http::HttpResponse {
        quota_adapters::http::HttpResponse {
            status: 200,
            body: format!(
                r#"{{"rate_limit":{{"allowed":true,"limit_reached":false,
                    "secondary_window":{{"used_percent":{used},"limit_window_seconds":604800}}}}}}"#
            )
            .into_bytes(),
            retry_after_secs: None,
        }
    }

    impl PerAccountTransport {
        fn new(responses: [(&'static str, quota_adapters::http::HttpResponse); 2]) -> Self {
            Self {
                responses: responses.into_iter().collect(),
                requests: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn requests_as(&self, account: &str) -> usize {
            let requests = self.requests.lock().unwrap();
            requests
                .iter()
                .filter(|made_as| *made_as == account)
                .count()
        }
    }

    impl quota_adapters::http::Transport for PerAccountTransport {
        fn get(
            &self,
            _url: &str,
            headers: &[(&str, &str)],
        ) -> Result<quota_adapters::http::HttpResponse, quota_adapters::http::TransportError>
        {
            let account = headers
                .iter()
                .find(|(name, _)| *name == "ChatGPT-Account-Id")
                .map(|(_, value)| value.to_string())
                .expect("request names its account");
            let response = self.responses[account.as_str()].clone();
            self.requests.lock().unwrap().push(account);
            Ok(response)
        }
    }

    /// The real Codex adapter over `transport`, its readings observed at
    /// `observed_at`.
    fn codex_probe_through(
        transport: Arc<PerAccountTransport>,
        observed_at: i64,
    ) -> impl FnOnce(Arc<RwLock<App>>, ProbeMode) -> BoxedProbe {
        move |app, mode| {
            Box::pin(probe_provider_with(
                app,
                ProviderId::Codex,
                mode,
                move |plan: ProbePlan, _| {
                    codex_adapter(plan).probe(&ProbeCtx {
                        transport: &*transport,
                        now: observed_at,
                    })
                },
            ))
        }
    }

    type BoxedProbe =
        std::pin::Pin<Box<dyn std::future::Future<Output = Option<ProviderSnapshot>>>>;

    async fn watch_frame_codex(client: &mut UnixStream) -> ProviderSnapshot {
        let frame = recv_json(client).await;
        let snapshot: Snapshot =
            serde_json::from_value(frame["result"]["snapshot"].clone()).unwrap();
        snapshot.by_id(ProviderId::Codex).unwrap().clone()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn round4_reading_for_account_a_is_refused_after_credentials_rotate_to_b() {
        use quota_adapters::creds::account_digest;
        use quota_core::types::CanStartBasis;

        let home = codex_home_for(FIXTURE_ACCOUNT);
        let mut app = test_app();
        app.codex_home = Some(home.clone());
        let app = Arc::new(RwLock::new(app));
        let transport = Arc::new(PerAccountTransport::new([
            (FIXTURE_ACCOUNT, weekly_usage(10)),
            (ROTATED_ACCOUNT, weekly_usage(70)),
        ]));
        refresh_provider_with(
            app.clone(),
            ProviderId::Codex,
            codex_probe_through(transport.clone(), now_unix()),
        )
        .await;
        let a = Some(account_digest(FIXTURE_ACCOUNT));

        let admitted = codex_can_start(&app, 0).await;
        assert!(admitted.ok, "{admitted:?}");
        assert_eq!(admitted.answers[0].remaining_percent, Some(90.0));
        assert_eq!(codex_status(&app).await.account_digest, a);

        write_codex_auth(&home, ROTATED_ACCOUNT);

        for tokens in [0, 50_000] {
            let refused = codex_can_start(&app, tokens).await;
            assert!(!refused.ok);
            assert_eq!(refused.answers[0].basis, CanStartBasis::AccountChanged);
            assert!(refused.answers[0]
                .explanation
                .contains("account changed since reading"));
            assert_eq!(refused.answers[0].remaining_percent, None);
        }
        let everything = Request::with_params(
            51,
            METHOD_CAN_START,
            CanStartParams {
                tokens: 0,
                percent: None,
                reserve: None,
                deadline: None,
                provider: ProviderFilter::All,
            },
        );
        let everything: CanStartResult =
            serde_json::from_value(dispatch(&app, everything).await.result.unwrap()).unwrap();
        assert!(!everything.ok);

        let shown = codex_status(&app).await;
        assert!(shown.is_for_another_account());
        assert_eq!(shown.account_digest, a, "status names whose reading it is");
        assert_eq!(shown.status, Availability::Unavailable);
        assert!(shown.windows.is_empty());
        let pace = codex_pace(&app).await;
        assert_eq!(pace.used_percent, None);
        assert_eq!(pace.samples, 0);
        assert!(pace.explanation.contains("account changed since reading"));

        let (mut client, server) = UnixStream::pair().unwrap();
        let slots = || Arc::new(Semaphore::new(1));
        let server = handle_client(app.clone(), server, slots(), slots());
        let stored = app.read().await.store.latest().cloned().unwrap();
        let watch_tx = app.read().await.watch_tx.clone();
        let watcher = async move {
            send_request(&mut client, &Request::new(1, METHOD_WATCH)).await;
            let first = watch_frame_codex(&mut client).await;
            watch_tx.send(stored).unwrap();
            let pushed = watch_frame_codex(&mut client).await;
            (first, pushed)
        };
        let (served, (first, pushed)) = tokio::join!(server, watcher);
        assert!(matches!(served, Err(ClientError::Eof)));
        for frame in [first, pushed] {
            assert!(frame.is_for_another_account());
            assert_eq!(frame.account_digest, a);
            assert!(frame.windows.is_empty());
        }

        write_codex_auth(&home, FIXTURE_ACCOUNT);
        assert!(
            codex_can_start(&app, 0).await.ok,
            "A's reading answers for A"
        );
        let _ = fs::remove_dir_all(home);
    }

    #[derive(Debug, Clone, Copy)]
    enum Step {
        Probe,
        Rotate,
        Publish,
        Consume,
    }

    const STEPS: [Step; 4] = [Step::Probe, Step::Rotate, Step::Publish, Step::Consume];
    const INTERLEAVING_LENGTH: u32 = 6;

    #[derive(Default)]
    struct Exercised {
        refused_as_changed: usize,
        admitted: usize,
        deadlines_passed_over: usize,
    }

    /// One interleaving against the real adapter, gate, backoff, publish and
    /// dispatch paths. A's first answer is `a_response`; B answers 70 % used.
    async fn run_interleaving(
        dir: &Path,
        steps: &[Step],
        a_response: &quota_adapters::http::HttpResponse,
        exercised: &mut Exercised,
    ) {
        use quota_adapters::creds::account_digest;
        use quota_core::types::CanStartBasis;

        let home = dir.join("codex");
        write_codex_auth(&home, FIXTURE_ACCOUNT);
        let mut app = test_app_in(dir);
        app.cfg.enable_codexbar_files = false;
        let app = Arc::new(RwLock::new(app));
        let transport = Arc::new(PerAccountTransport::new([
            (FIXTURE_ACCOUNT, a_response.clone()),
            (ROTATED_ACCOUNT, weekly_usage(70)),
        ]));
        let used_by = |account: &str| {
            if account == FIXTURE_ACCOUNT {
                10.0
            } else {
                70.0
            }
        };
        let base = now_unix() - 250;
        let mut active = FIXTURE_ACCOUNT;
        let mut pending: Option<Probed> = None;
        let mut probes = 0;
        for (at, step) in steps.iter().enumerate() {
            let trace = || format!("{steps:?} at step {at}, credentials name {active}");
            match step {
                Step::Probe if pending.is_none() => {
                    let a_deadline = app
                        .read()
                        .await
                        .retry_after_until
                        .get(&ProviderId::Codex)
                        .is_some_and(|deadline| {
                            deadline.until.is_active(tokio::time::Instant::now())
                        });
                    let before = transport.requests_as(active);
                    probes += 1;
                    let claim = claim(&app, ProviderId::Codex).await.expect("gate free");
                    let probe = codex_probe_through(transport.clone(), base + probes);
                    pending = probe_claimed(&app, claim, probe).await;
                    if active == ROTATED_ACCOUNT {
                        assert_eq!(transport.requests_as(active), before + 1, "{}", trace());
                        exercised.deadlines_passed_over += usize::from(a_deadline);
                    }
                }
                Step::Probe => {}
                Step::Rotate => {
                    active = if active == FIXTURE_ACCOUNT {
                        ROTATED_ACCOUNT
                    } else {
                        FIXTURE_ACCOUNT
                    };
                    write_codex_auth(&home, active);
                }
                Step::Publish => {
                    if let Some(probed) = pending.take() {
                        publish(&app, vec![probed]).await;
                    }
                }
                Step::Consume => {
                    let digest = account_digest(active);
                    let stored = app
                        .read()
                        .await
                        .store
                        .latest()
                        .and_then(|latest| latest.by_id(ProviderId::Codex).cloned())
                        .unwrap();
                    let taken_for_another = stored
                        .account_digest
                        .as_ref()
                        .is_some_and(|taken_for| *taken_for != digest);

                    let shown = codex_status(&app).await;
                    if taken_for_another {
                        assert!(shown.is_for_another_account(), "{}", trace());
                    }
                    if !shown.windows.is_empty() || shown.retry_after_secs.is_some() {
                        assert_eq!(shown.account_digest.as_ref(), Some(&digest), "{}", trace());
                    }
                    for window in &shown.windows {
                        assert_eq!(window.used_percent, Some(used_by(active)), "{}", trace());
                    }

                    let answer = codex_can_start(&app, 0).await.answers.remove(0);
                    if taken_for_another {
                        assert_eq!(answer.basis, CanStartBasis::AccountChanged, "{}", trace());
                        exercised.refused_as_changed += 1;
                    }
                    if answer.ok {
                        let remaining = 100.0 - used_by(active);
                        assert_eq!(answer.remaining_percent, Some(remaining), "{}", trace());
                        exercised.admitted += 1;
                    }
                    assert!(
                        answer.burn_percent_per_hour.is_none_or(|burn| burn == 0.0),
                        "{}",
                        trace()
                    );

                    let pace = codex_pace(&app).await;
                    if let Some(used) = pace.used_percent {
                        assert_eq!(used, used_by(active), "{}", trace());
                    }
                    assert!(
                        pace.burn_percent_per_hour.is_none_or(|burn| burn == 0.0),
                        "{}",
                        trace()
                    );
                }
            }
        }
    }

    /// Every interleaving of probe, rotate, publish and consume up to
    /// [`INTERLEAVING_LENGTH`] steps, enumerated exhaustively. No consumer
    /// ever answers for the account the credentials name from a reading
    /// taken for the other one, and A's deadline never holds B's probe back.
    #[tokio::test(flavor = "current_thread")]
    async fn round4_no_consumer_answers_for_b_from_a_reading_under_any_interleaving() {
        let dir = unique_test_dir("quota-interleavings");
        fs::create_dir(dir.join("codex")).unwrap();
        let limited = quota_adapters::http::HttpResponse {
            status: 429,
            body: b"{}".to_vec(),
            retry_after_secs: Some(120),
        };
        let mut exercised = Exercised::default();
        let mut runs = 0;
        for a_response in [weekly_usage(10), limited] {
            for word in 0..STEPS.len().pow(INTERLEAVING_LENGTH) {
                let steps: Vec<Step> = (0..INTERLEAVING_LENGTH)
                    .map(|position| STEPS[word / STEPS.len().pow(position) % STEPS.len()])
                    .collect();
                run_interleaving(&dir, &steps, &a_response, &mut exercised).await;
                runs += 1;
            }
        }

        assert_eq!(runs, 2 * 4usize.pow(INTERLEAVING_LENGTH));
        assert!(exercised.refused_as_changed > 0);
        assert!(exercised.admitted > 0);
        assert!(exercised.deadlines_passed_over > 0);
        let _ = fs::remove_dir_all(dir);
    }

    async fn claude_can_start(app: &Arc<RwLock<App>>) -> CanStartResult {
        let req = Request::with_params(
            52,
            METHOD_CAN_START,
            CanStartParams {
                tokens: 0,
                percent: None,
                reserve: None,
                deadline: None,
                provider: ProviderFilter::Claude,
            },
        );
        serde_json::from_value(dispatch(app, req).await.result.unwrap()).unwrap()
    }

    async fn claude_status(app: &Arc<RwLock<App>>) -> ProviderSnapshot {
        let req = Request::with_params(
            53,
            METHOD_STATUS,
            StatusParams {
                provider: ProviderFilter::Claude,
            },
        );
        let status: StatusResult =
            serde_json::from_value(dispatch(app, req).await.result.unwrap()).unwrap();
        status.snapshot.by_id(ProviderId::Claude).unwrap().clone()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn round5_unnamed_claude_credentials_answer_and_absent_ones_refuse() {
        use quota_core::types::CanStartBasis;

        let dir = unique_test_dir("quota-test-acct");
        write_claude_credentials(&dir);
        let app = Arc::new(RwLock::new(test_app_in(&dir)));
        apply_provider_snapshot(&app, observed_reading(ProviderId::Claude, 10.0)).await;

        let admitted = claude_can_start(&app).await;
        assert!(admitted.ok, "{admitted:?}");
        assert_eq!(admitted.answers[0].remaining_percent, Some(90.0));
        assert_eq!(claude_pace(&app).await.samples, 1);

        fs::remove_file(dir.join("claude").join(".credentials.json")).unwrap();

        let refused = claude_can_start(&app).await;
        assert!(!refused.ok);
        assert_eq!(refused.answers[0].basis, CanStartBasis::AccountChanged);
        assert_eq!(claude_pace(&app).await.samples, 0);
        let shown = claude_status(&app).await;
        assert!(shown.is_for_another_account());
        assert!(shown.windows.is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn round5_absent_codex_credentials_with_no_other_source_refuse_a_named_reading() {
        use quota_adapters::creds::account_digest;
        use quota_core::types::CanStartBasis;

        let home = codex_home_for(FIXTURE_ACCOUNT);
        let mut app = test_app();
        app.codex_home = Some(home.clone());
        let app = Arc::new(RwLock::new(app));
        let mut reading = observed_reading(ProviderId::Codex, 10.0);
        reading.account_digest = Some(account_digest(FIXTURE_ACCOUNT));
        apply_provider_snapshot(&app, reading).await;
        assert!(codex_can_start(&app, 0).await.ok);

        fs::remove_file(home.join("auth.json")).unwrap();

        let refused = codex_can_start(&app, 0).await;
        assert!(!refused.ok);
        assert_eq!(refused.answers[0].basis, CanStartBasis::AccountChanged);
        let shown = codex_status(&app).await;
        assert!(shown.is_for_another_account());
        assert_eq!(
            shown.account_digest.as_deref(),
            Some(account_digest(FIXTURE_ACCOUNT).as_str())
        );
        let _ = fs::remove_dir_all(home);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn round5_unnamed_codex_credentials_with_no_other_source_answer_an_unnamed_reading() {
        let dir = unique_test_dir("quota-test-acct");
        write_unnamed_codex_credentials(&dir);
        let app = Arc::new(RwLock::new(test_app_in(&dir)));
        apply_provider_snapshot(&app, observed_reading(ProviderId::Codex, 10.0)).await;

        let admitted = codex_can_start(&app, 0).await;
        assert!(admitted.ok, "{admitted:?}");
        assert_eq!(admitted.answers[0].remaining_percent, Some(90.0));
        assert!(!codex_status(&app).await.is_for_another_account());
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn round5_unnamed_codex_credentials_with_two_codexbar_accounts_refuse() {
        use quota_core::types::CanStartBasis;

        let dir = unique_test_dir("quota-test-acct");
        write_unnamed_codex_credentials(&dir);
        let codexbar = test_subdir(&dir, "codexbar");
        let record = |id: &str| {
            format!(
                r#"{{"accountIdentity":{{"workspaceAccountID":"{id}"}},
                    "snapshot":{{"secondary":{{"usedPercent":5,"windowMinutes":10080}},"updatedAt":812150779.9}},
                    "sourceLabel":"oauth"}}"#
            )
        };
        fs::write(
            codexbar.join("codex-account-snapshots.json"),
            format!(
                r#"{{"version":1,"records":[{},{}]}}"#,
                record(FIXTURE_ACCOUNT),
                record(ROTATED_ACCOUNT)
            ),
        )
        .unwrap();
        let app = Arc::new(RwLock::new(test_app_in(&dir)));
        apply_provider_snapshot(&app, observed_reading(ProviderId::Codex, 10.0)).await;

        let refused = codex_can_start(&app, 0).await;
        assert!(!refused.ok);
        assert_eq!(refused.answers[0].basis, CanStartBasis::AccountChanged);
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn round5_a_reading_without_quota_evidence_keeps_its_own_error() {
        let app = Arc::new(RwLock::new(test_app()));
        let shown = codex_status(&app).await;
        assert_eq!(shown.error.as_ref().unwrap().code, "no_credentials");
        assert!(!shown.is_for_another_account());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn round6_an_error_reading_for_account_a_is_refused_after_credentials_rotate_to_b() {
        use quota_adapters::creds::account_digest;
        use quota_core::types::CanStartBasis;

        let home = codex_home_for(FIXTURE_ACCOUNT);
        let mut app = test_app();
        app.codex_home = Some(home.clone());
        let app = Arc::new(RwLock::new(app));
        let outage = quota_adapters::http::HttpResponse {
            status: 503,
            body: b"{}".to_vec(),
            retry_after_secs: None,
        };
        let transport = Arc::new(PerAccountTransport::new([
            (FIXTURE_ACCOUNT, outage),
            (ROTATED_ACCOUNT, weekly_usage(70)),
        ]));
        refresh_provider_with(
            app.clone(),
            ProviderId::Codex,
            codex_probe_through(transport, now_unix()),
        )
        .await;

        let shown = codex_status(&app).await;
        assert_eq!(shown.account_digest, Some(account_digest(FIXTURE_ACCOUNT)));
        assert!(
            !shown.holds_quota_evidence(),
            "premise: an error with no quota fields"
        );
        assert!(!shown.is_for_another_account());
        let before = codex_can_start(&app, 0).await;
        assert_eq!(before.answers[0].basis, CanStartBasis::Unavailable);

        write_codex_auth(&home, ROTATED_ACCOUNT);

        let shown = codex_status(&app).await;
        assert!(shown.is_for_another_account());
        assert_eq!(shown.account_digest, Some(account_digest(FIXTURE_ACCOUNT)));
        let refused = codex_can_start(&app, 0).await;
        assert!(!refused.ok);
        assert_eq!(refused.answers[0].basis, CanStartBasis::AccountChanged);
        let _ = fs::remove_dir_all(home);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn round6_pace_and_can_start_after_rotation_use_only_the_new_accounts_samples() {
        let home = codex_home_for(FIXTURE_ACCOUNT);
        let mut app = test_app();
        app.codex_home = Some(home.clone());
        let app = Arc::new(RwLock::new(app));
        let now = now_unix();
        let as_account = |used: u32, observed_at: i64| {
            let transport = Arc::new(PerAccountTransport::new([
                (FIXTURE_ACCOUNT, weekly_usage(used)),
                (ROTATED_ACCOUNT, weekly_usage(used)),
            ]));
            codex_probe_through(transport, observed_at)
        };

        refresh_provider_with(app.clone(), ProviderId::Codex, as_account(10, now - 400)).await;
        refresh_provider_with(app.clone(), ProviderId::Codex, as_account(60, now - 300)).await;
        write_codex_auth(&home, ROTATED_ACCOUNT);
        refresh_provider_with(app.clone(), ProviderId::Codex, as_account(20, now - 200)).await;
        refresh_provider_with(app.clone(), ProviderId::Codex, as_account(21, now - 100)).await;

        let pace = codex_pace(&app).await;
        assert_eq!(pace.samples, 2, "only the active account's readings count");
        assert!((pace.burn_percent_per_hour.unwrap() - 36.0).abs() < 1e-6);
        let answer = codex_can_start(&app, 0).await.answers.remove(0);
        assert_eq!(answer.burn_percent_per_hour, pace.burn_percent_per_hour);
        let _ = fs::remove_dir_all(home);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn round6_a_file_only_result_never_holds_a_deadline_issued_to_another_digest() {
        let named = Some("b".repeat(64));
        let file_for = |digest: Option<String>| {
            move |_, mode: ProbeMode| {
                let digest = digest.clone();
                async move {
                    assert_eq!(mode, ProbeMode::FileOnly);
                    let mut file = observed_reading(ProviderId::Codex, 30.0);
                    file.source = Some(quota_core::types::Source::File);
                    file.account_digest = digest;
                    Some(file)
                }
            }
        };
        for (issued_to, file_of) in [(None, named.clone()), (Some("a".repeat(64)), None)] {
            let app = Arc::new(RwLock::new(test_app()));
            let mut refused = rate_limited(ProviderId::Codex, Some(120));
            refused.account_digest = issued_to;
            apply_provider_snapshot(&app, refused).await;
            assert!(provider_in_backoff(&app, ProviderId::Codex).await);

            refresh_provider_with(app.clone(), ProviderId::Codex, file_for(file_of)).await;

            assert!(!provider_in_backoff(&app, ProviderId::Codex).await);
            let g = app.read().await;
            let codex = g.store.latest().unwrap().by_id(ProviderId::Codex).unwrap();
            assert_eq!(codex.retry_after_secs, None);
            assert_eq!(codex.retry_after_until, None);
        }
    }
}
