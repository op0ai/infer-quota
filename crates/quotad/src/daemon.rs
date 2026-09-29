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
    freshness_for, AdapterError, Availability, Freshness, ProviderId, ProviderSnapshot, Snapshot,
    DEFAULT_READING_MAX_AGE_SECS,
};
use quota_core::Config;
use quota_source_cursor::CursorAdapter;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, Mutex, RwLock, Semaphore};

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
    /// other provider's collector.
    retry_after_until: HashMap<ProviderId, RetryAfterDeadline>,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryAfterDeadline {
    At(tokio::time::Instant),
    Unrepresentable,
}

impl RetryAfterDeadline {
    fn from_secs(seconds: u64) -> Self {
        tokio::time::Instant::now()
            .checked_add(Duration::from_secs(seconds))
            .map(Self::At)
            .unwrap_or(Self::Unrepresentable)
    }

    fn is_active(self, now: tokio::time::Instant) -> bool {
        match self {
            Self::At(deadline) => now < deadline,
            Self::Unrepresentable => true,
        }
    }
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
/// generation is captured before waiting; waiters reuse that provider's result.
#[derive(Default)]
struct RefreshGate {
    lock: Mutex<()>,
    generation: AtomicU64,
}

impl RefreshGate {
    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    fn mark_complete(&self) {
        self.generation.fetch_add(1, Ordering::Release);
    }

    async fn run_if_current<F, Fut>(&self, observed_generation: u64, refresh: F) -> bool
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let _guard = self.lock.lock().await;
        if self.generation() > observed_generation {
            return false;
        }
        if !refresh().await {
            return false;
        }
        self.mark_complete();
        true
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
    }));

    // Register both signals before starting the initial provider
    // probes. A startup SIGTERM then follows the same socket cleanup path as a
    // signal received after the accept loop starts.
    let plan = {
        let g = app.read().await;
        probe_plan(&g)
    };
    let startup_probe = probe_on_thread(move || collect_snapshot(plan));
    let startup = tokio::select! {
        _ = interrupt.recv() => {
            eprintln!("quotad: received SIGINT during startup, shutting down");
            None
        }
        _ = terminate.recv() => {
            eprintln!("quotad: received SIGTERM during startup, shutting down");
            None
        }
        result = startup_probe => Some(result),
    };
    let snapshot = match startup {
        Some(Ok(snapshot)) => snapshot,
        Some(Err(error)) => {
            let _ = fs::remove_file(&socket);
            return Err(DaemonError::Io(format!(
                "initial provider probe task failed: {error}"
            )));
        }
        None => {
            let _ = fs::remove_file(&socket);
            return Ok(());
        }
    };
    apply_snapshot(&app, snapshot).await;
    {
        let mut g = app.write().await;
        if g.cfg.enable_cursor {
            g.last_polled
                .insert(ProviderId::Cursor, tokio::time::Instant::now());
        }
        for gate in g.refresh_gates.values() {
            gate.mark_complete();
        }
    }

    let mut scheduled_refresh: Option<tokio::task::JoinHandle<()>> = None;
    loop {
        if scheduled_refresh
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
        {
            if let Some(task) = scheduled_refresh.take() {
                if let Err(error) = task.await {
                    eprintln!("quotad: scheduled refresh task failed: {error}");
                }
            }
        }
        let wait = {
            let g = app.read().await;
            Duration::from_secs(g.interval_secs)
        };
        tokio::select! {
            _ = interrupt.recv() => {
                eprintln!("quotad: shutting down");
                break;
            }
            _ = terminate.recv() => {
                eprintln!("quotad: received SIGTERM, shutting down");
                break;
            }
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
            _ = tokio::time::sleep(wait) => {
                // Keep this event loop polling signals and accepting socket
                // clients while providers refresh on independent threads.
                if scheduled_refresh.is_none() {
                    scheduled_refresh = Some(tokio::spawn(refresh(app.clone())));
                }
            }
        }
    }
    if let Some(task) = scheduled_refresh {
        task.abort();
    }
    let _ = fs::remove_file(&socket);
    Ok(())
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
    enable_codex: bool,
    enable_claude: bool,
    enable_cursor: bool,
    cursor_secret_path: String,
    codex_home: Option<PathBuf>,
    claude_home: Option<PathBuf>,
    codexbar_dir: PathBuf,
    enable_codexbar_files: bool,
}

fn probe_plan(g: &App) -> ProbePlan {
    let (codex_home, claude_home) = match g.accounts.book().active() {
        Some(a) if a.provider == ProviderId::Codex => {
            (a.home_path.clone().map(PathBuf::from), None)
        }
        Some(a) if a.provider == ProviderId::Claude => {
            (None, a.home_path.clone().map(PathBuf::from))
        }
        _ => (None, None),
    };
    ProbePlan {
        timeout: g.cfg.http_timeout_secs,
        enable_codex: g.cfg.enable_codex,
        enable_claude: g.cfg.enable_claude,
        enable_cursor: g.cfg.enable_cursor,
        cursor_secret_path: g.cfg.cursor_secret_path.clone(),
        codex_home,
        claude_home,
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
        ProviderId::Codex => CodexAdapter {
            home: plan.codex_home,
            codexbar_dir: Some(plan.codexbar_dir),
            enable_codexbar_files: plan.enable_codexbar_files,
        }
        .probe(&ctx),
        ProviderId::Claude => ClaudeAdapter::for_account(plan.claude_home).probe(&ctx),
        ProviderId::Cursor => {
            match CursorAdapter::from_keychain_first_chain(plan.cursor_secret_path) {
                Ok(adapter) => adapter.probe(&ctx),
                Err(e) => ProviderSnapshot::unavailable(
                    ProviderId::Cursor,
                    AdapterError::new("secrets_config", e.to_string()),
                ),
            }
        }
    }
}

fn collect_snapshot(plan: ProbePlan) -> Snapshot {
    let mut providers = Vec::new();
    if plan.enable_codex {
        providers.push(collect_provider(plan.clone(), ProviderId::Codex));
    }
    if plan.enable_claude {
        providers.push(collect_provider(plan.clone(), ProviderId::Claude));
    }
    if plan.enable_cursor {
        providers.push(collect_provider(plan, ProviderId::Cursor));
    }
    Snapshot::new(now_unix(), providers)
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
    let _ = g.watch_tx.send(snap.clone());
    g.store.push(snap);
}

async fn apply_snapshot(app: &Arc<RwLock<App>>, snap: Snapshot) {
    let mut snap = snap;
    let mut g = app.write().await;
    for provider in &mut snap.providers {
        ensure_rate_limit_backoff(&g, provider);
    }
    let now = now_unix();
    let mut snap = snap.refreshed_at(now);
    for provider in &snap.providers {
        record_retry_after(&mut g, provider);
    }
    keep_current_pushes(&g, &mut snap.providers, now);
    apply_snapshot_locked(&mut g, snap);
}

/// A poll result lands under the write lock. A push that arrived while the
/// poll was in flight is newer evidence, so the precedence check runs here, at
/// commit, and the pushed entry survives.
fn keep_current_pushes(g: &App, providers: &mut [ProviderSnapshot], now: i64) {
    let Some(latest) = g.store.latest() else {
        return;
    };
    for polled in providers.iter_mut() {
        if !push_is_current(g, polled.provider, now) {
            continue;
        }
        if let Some(pushed) = latest.by_id(polled.provider) {
            *polled = pushed.clone();
            polled.refresh_freshness(now);
        }
    }
}

/// A 429 that names no `Retry-After` still backs that provider off, for the
/// longest interval the scheduler would ever wait.
fn ensure_rate_limit_backoff(g: &App, provider: &mut ProviderSnapshot) {
    let rate_limited = provider
        .error
        .as_ref()
        .is_some_and(|error| error.code == "rate_limited");
    if rate_limited
        && provider
            .retry_after_secs
            .filter(|seconds| *seconds > 0)
            .is_none()
    {
        provider.retry_after_secs = Some(g.cfg.refresh_max_secs());
    }
}

fn record_retry_after(g: &mut App, provider: &ProviderSnapshot) {
    // A provider's Retry-After affects only that provider's next probe. It
    // never lengthens the shared scheduler interval or another source's gate.
    match provider.retry_after_secs.filter(|seconds| *seconds > 0) {
        Some(seconds) => {
            g.retry_after_until
                .insert(provider.provider, RetryAfterDeadline::from_secs(seconds));
        }
        None => {
            g.retry_after_until.remove(&provider.provider);
        }
    }
}

async fn apply_provider_snapshot(app: &Arc<RwLock<App>>, mut provider: ProviderSnapshot) {
    let now = now_unix();
    let mut g = app.write().await;
    ensure_rate_limit_backoff(&g, &mut provider);
    let mut providers = g
        .store
        .latest()
        .map(|snapshot| snapshot.refreshed_at(now).providers)
        .unwrap_or_default();
    record_retry_after(&mut g, &provider);
    if push_is_current(&g, provider.provider, now) {
        return;
    }
    if let Some(existing) = providers
        .iter_mut()
        .find(|existing| existing.provider == provider.provider)
    {
        *existing = provider;
    } else {
        providers.push(provider);
    }
    let snapshot = Snapshot::new(now, providers).refreshed_at(now);
    apply_snapshot_locked(&mut g, snapshot);
}

async fn provider_in_backoff(app: &Arc<RwLock<App>>, provider: ProviderId) -> bool {
    app.read()
        .await
        .retry_after_until
        .get(&provider)
        .is_some_and(|deadline| deadline.is_active(tokio::time::Instant::now()))
}

async fn refresh_provider_with<F, Fut>(app: Arc<RwLock<App>>, provider: ProviderId, probe: F)
where
    F: FnOnce(Arc<RwLock<App>>) -> Fut,
    Fut: std::future::Future<Output = Option<ProviderSnapshot>>,
{
    let gate = app
        .read()
        .await
        .refresh_gates
        .get(&provider)
        .expect("gate for each provider")
        .clone();
    let observed_generation = gate.generation();
    let app_for_probe = app.clone();
    let _ = gate
        .run_if_current(observed_generation, || async move {
            if provider_in_backoff(&app_for_probe, provider).await {
                return false;
            }
            let Some(snapshot) = probe(app_for_probe.clone()).await else {
                return false;
            };
            if snapshot.provider != provider {
                eprintln!("quotad: {provider} refresh returned {}", snapshot.provider);
                return false;
            }
            apply_provider_snapshot(&app_for_probe, snapshot).await;
            true
        })
        .await;
}

async fn refresh_provider(app: Arc<RwLock<App>>, provider: ProviderId) {
    refresh_provider_with(app, provider, move |app| async move {
        let plan = {
            let g = app.read().await;
            let enabled = match provider {
                ProviderId::Codex => g.cfg.enable_codex,
                ProviderId::Claude => g.cfg.enable_claude,
                ProviderId::Cursor => g.cfg.enable_cursor,
            };
            if !enabled || !poll_due(&g, provider) {
                return None;
            }
            probe_plan(&g)
        };
        match probe_on_thread(move || collect_provider(plan, provider)).await {
            Ok(snapshot) => {
                app.write()
                    .await
                    .last_polled
                    .insert(provider, tokio::time::Instant::now());
                Some(snapshot)
            }
            Err(error) => {
                eprintln!("quotad: {provider} refresh task failed: {error}");
                None
            }
        }
    })
    .await;
}

/// Providers that are disabled, in backoff, or already covered by fresher
/// evidence return without probing.
async fn refresh(app: Arc<RwLock<App>>) {
    tokio::join!(
        refresh_provider(app.clone(), ProviderId::Codex),
        refresh_provider(app.clone(), ProviderId::Claude),
        refresh_provider(app, ProviderId::Cursor)
    );
}

/// A current statusline push already answers for Claude, so its OAuth poll
/// waits; Cursor's dashboard route is polled no faster than its own floor.
fn poll_due(g: &App, provider: ProviderId) -> bool {
    match provider {
        ProviderId::Claude => !push_is_current(g, provider, now_unix()),
        ProviderId::Cursor => g.last_polled.get(&provider).is_none_or(|at| {
            at.elapsed() >= Duration::from_secs(quota_source_cursor::MIN_POLL_SECS)
        }),
        ProviderId::Codex => true,
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

/// Fold one pushed provider snapshot into the latest snapshot. Unlike a poll
/// it leaves the adaptive refresh interval alone, and while the numbers repeat
/// it refreshes the newest ring entry in place so a chatty statusline cannot
/// push other providers' history out of the ring.
async fn apply_pushed_snapshot(app: &Arc<RwLock<App>>, provider: ProviderSnapshot, now: i64) {
    let id = provider.provider;
    let mut g = app.write().await;
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
            let params: WatchParams =
                serde_json::from_value(req.params.clone()).unwrap_or_default();
            let (mut rx, idle_dur, mut latest) = {
                let g = app.read().await;
                let snap = filter_snapshot(g.store.latest(), params.provider);
                write_frame_timed(
                    &mut stream,
                    &Response::result(req.id, StatusResult { snapshot: snap }),
                )
                .await?;
                let idle_dur = watch_idle_timeout(g.cfg.refresh_max_secs());
                (g.watch_tx.subscribe(), idle_dur, g.store.latest().cloned())
            };
            let idle = tokio::time::sleep(idle_dur);
            tokio::pin!(idle);
            let expiry = tokio::time::sleep_until(next_expiry_deadline(latest.as_ref()));
            tokio::pin!(expiry);
            loop {
                tokio::select! {
                    next = rx.recv() => {
                        match next {
                            Ok(snap) => {
                                let filtered = filter_snapshot(Some(&snap), params.provider);
                                latest = Some(snap);
                                write_frame_timed(
                                    &mut stream,
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
                        let aged = filter_snapshot(latest.as_ref(), params.provider);
                        write_frame_timed(
                            &mut stream,
                            &Response::result(req.id, StatusResult { snapshot: aged }),
                        )
                        .await?;
                        expiry.as_mut().reset(next_expiry_deadline(latest.as_ref()));
                    }
                    incoming = read_frame_async(&mut stream) => {
                        match incoming {
                            Ok(bytes) => {
                                // Only a documented `ping` keepalive resets idle.
                                // Junk / other methods must not hold the slot.
                                if let Ok(r) = serde_json::from_slice::<Request>(&bytes) {
                                    if r.method == METHOD_PING {
                                        write_frame_timed(
                                            &mut stream,
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

async fn dispatch(app: &Arc<RwLock<App>>, req: Request) -> Response {
    match req.method.as_str() {
        METHOD_PING => Response::result(req.id, quota_core::protocol::Pong { pong: true }),
        METHOD_VERSION => Response::result(req.id, VersionInfo::current()),
        METHOD_STATUS => {
            let params: StatusParams = serde_json::from_value(req.params).unwrap_or_default();
            let g = app.read().await;
            let snap = filter_snapshot(g.store.latest(), params.provider);
            Response::result(req.id, StatusResult { snapshot: snap })
        }
        METHOD_PACE => {
            let params: PaceParams = serde_json::from_value(req.params).unwrap_or_default();
            let g = app.read().await;
            let latest = g.snapshot_or_empty();
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
            let latest = g.snapshot_or_empty();
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
                    apply_pushed_snapshot(app, snapshot, now).await;
                    Response::result(
                        req.id,
                        ObserveResult {
                            accepted: params.windows.len(),
                            observed_at: now,
                        },
                    )
                }
                Err(rejection) => Response::err(req.id, rejection.code, rejection.message),
            }
        }
        METHOD_REFRESH => {
            let params: RefreshParams = serde_json::from_value(req.params).unwrap_or_default();
            refresh(app.clone()).await;
            let g = app.read().await;
            let snap = filter_snapshot(g.store.latest(), params.provider);
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
            let mut g = app.write().await;
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
            let mut g = app.write().await;
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

async fn write_frame_timed(stream: &mut UnixStream, resp: &Response) -> Result<(), ClientError> {
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
                Ok(()) => return dir,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create test directory {}: {error}", dir.display()),
            }
        }
        panic!("could not allocate unique test directory for {prefix}");
    }

    fn test_app() -> App {
        let accounts_path = unique_test_dir("quota-test-acct").join("accounts.json");
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
            retry_after_until: HashMap::new(),
            refresh_gates: provider_refresh_gates(),
            store,
            accounts: AccountStore::load(accounts_path),
            cfg,
            watch_tx,
            pushed_at: HashMap::new(),
            last_ring_push: HashMap::new(),
            last_polled: HashMap::new(),
        }
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
        refresh_provider_with(app.clone(), ProviderId::Codex, move |_| async move {
            codex_calls_probe.fetch_add(1, Ordering::Relaxed);
            Some(ProviderSnapshot::unavailable(
                ProviderId::Codex,
                AdapterError::new("unexpected", "must remain in backoff"),
            ))
        })
        .await;
        assert_eq!(codex_calls.load(Ordering::Relaxed), 0);

        let codex_deadline = app.read().await.retry_after_until[&ProviderId::Codex];
        let claude_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let claude_calls_probe = claude_calls.clone();
        refresh_provider_with(app.clone(), ProviderId::Claude, move |_| async move {
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
        refresh_provider_with(app.clone(), ProviderId::Codex, move |_| async move {
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
    async fn an_in_flight_poll_never_overwrites_a_push_that_landed_during_it() {
        let app = Arc::new(RwLock::new(test_app()));
        assert!(poll_due(&*app.read().await, ProviderId::Claude));
        refresh_provider_with(app.clone(), ProviderId::Claude, |app| async move {
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
        apply_snapshot(&app, Snapshot::new(now_unix(), vec![polled])).await;
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
}
