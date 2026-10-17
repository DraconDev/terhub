//! terhubd: Unix-socket daemon holding the agent registry.
//!
//! The daemon merges three signals per pane: authoritative hook events pushed
//! by agents (`terhub hook ...`), process-tree agent identification, and
//! screen-pattern classification of hook-less panes from periodic
//! `capture-pane` polls. Clients talk newline-delimited JSON over the
//! socket; see [`Request`].
use crate::agent::{
    AgentRecord, AgentState, CompiledManifest, HookEvent, ScreenHit, Source, apply_event_to,
    builtin_manifests, classify_screen, compile_all_lossy, is_harness_command, is_shell_command,
    load_manifests, now_unix,
};
use crate::attend::{detect_all_transitions, notify_transition};
use crate::config;
use crate::history;
use crate::proc::is_editor_name as is_editor_command;
use crate::tmux::{Pane, Tmux};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader as TokioBufReader};
use tokio::net::{UnixListener, UnixStream as TokioUnixStream};

pub const POLL_SECS: u64 = 2;

/// Hook records lapse back to screen classification after 30min of silence.
pub const HOOK_STALE_SECS: u64 = 1800;

/// A working agent whose process tree burns no CPU for this long is stuck,
/// not busy.
pub const STUCK_SECS: u64 = 600;

/// A live agent whose screen has not changed for this long is finished,
/// not busy. This is the honest signal: a completed turn prints whatever
/// it prints, and most print nothing that matches an idle pattern, so
/// without it finished agents read `Working` forever (their REPL keeps
/// burning CPU, so Stuck never trips either).
pub const QUIET_IDLE_SECS: u64 = 300;

/// A `waiting` prompt untouched this long stops counting as urgent.
/// Longer than [`QUIET_IDLE_SECS`] on purpose: an agent that just asked
/// something must keep flagging, even if you stepped away for a minute.
pub const WAITING_IDLE_SECS: u64 = 1800;

/// Daemon timing knobs from the `[daemon]` config section (restart to
/// apply). `stuck_secs = 0` disables stuck detection entirely.
#[derive(Debug, Clone, Copy)]
pub struct Tunables {
    pub poll_secs: u64,
    pub stuck_secs: u64,
    pub stale_secs: u64,
    /// Frozen-screen → Idle threshold (`0` disables).
    pub quiet_idle_secs: u64,
    /// Untouched-waiting → Idle threshold (`0` disables).
    pub waiting_idle_secs: u64,
}

impl Default for Tunables {
    fn default() -> Self {
        Tunables {
            poll_secs: POLL_SECS,
            stuck_secs: STUCK_SECS,
            stale_secs: HOOK_STALE_SECS,
            quiet_idle_secs: QUIET_IDLE_SECS,
            waiting_idle_secs: WAITING_IDLE_SECS,
        }
    }
}

impl Tunables {
    pub fn from_config(cfg: &config::AppConfig) -> Self {
        Tunables {
            poll_secs: cfg.poll_secs.max(1),
            stuck_secs: cfg.stuck_secs,
            stale_secs: cfg.stale_secs.max(1),
            quiet_idle_secs: cfg.quiet_idle_secs,
            waiting_idle_secs: cfg.waiting_idle_secs,
        }
    }
}

/// Runtime dir: explicit override (tests) → XDG runtime → per-user /tmp.
/// Empty variables are ignored so a blank `export` can't redirect us to `/`.
pub fn runtime_dir() -> PathBuf {
    if let Ok(d) = std::env::var("TERHUB_RUNTIME_DIR")
        && !d.is_empty()
    {
        return PathBuf::from(d);
    }
    if let Ok(d) = std::env::var("XDG_RUNTIME_DIR")
        && !d.is_empty()
    {
        return PathBuf::from(d).join("terhub");
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "shared".to_string());
    std::env::temp_dir().join(format!("terhub-{user}"))
}

pub fn socket_path() -> PathBuf {
    runtime_dir().join("terhubd.sock")
}

pub fn pid_path() -> PathBuf {
    runtime_dir().join("terhubd.pid")
}

/// Daemon stderr goes here (manifest warnings, bind errors, panics).
pub fn log_path() -> PathBuf {
    runtime_dir().join("terhubd.log")
}

/// A candidate daemon binary is safe to exec only when it is a regular
/// file owned by us (or root) with no group/world write bit. A bare
/// PATH name would let anyone with a writable dir early in PATH plant
/// the binary we then spawn detached and re-spawn on every self-heal
/// tick, so every PATH-resolved candidate must pass this check.
fn safe_to_exec(path: &Path) -> Result<()> {
    let meta = std::fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?;
    if !meta.file_type().is_file() {
        anyhow::bail!("{} is not a regular file", path.display());
    }
    let euid = unsafe { libc::geteuid() };
    let owner = meta.uid();
    if owner != euid && owner != 0 {
        anyhow::bail!(
            "{} owned by uid {owner} (expected euid {euid} or root)",
            path.display()
        );
    }
    if meta.mode() & 0o022 != 0 {
        anyhow::bail!(
            "{} is group/world-writable (mode {:o})",
            path.display(),
            meta.mode() & 0o777
        );
    }
    Ok(())
}

/// Pure PATH search (takes the PATH value as an argument so tests do
/// not mutate process-global env): first `name` under a PATH entry
/// that passes [`safe_to_exec`]. Unsafe candidates are skipped with a
/// stderr note rather than aborting the search, so one bad dir early
/// in PATH does not brick a good install later in it.
fn find_in_path(name: &str, path_var: &std::ffi::OsStr) -> Result<PathBuf> {
    let mut rejected = 0;
    for dir in std::env::split_paths(path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(name);
        match safe_to_exec(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(e) => {
                // Missing file is the common case — only note real
                // candidates that fail the safety check.
                if candidate.exists() {
                    rejected += 1;
                    eprintln!("terhubd: skipping unsafe daemon binary: {e:#}");
                }
            }
        }
    }
    if rejected > 0 {
        anyhow::bail!("no safe `{name}` found in PATH ({rejected} unsafe candidate(s) rejected)");
    }
    anyhow::bail!("no `{name}` found in PATH")
}

/// The daemon binary: beside this executable when it is there (the
/// normal installed/cargo layout), else a verified PATH lookup.
/// Fail-closed: when no sibling exists and no PATH candidate passes
/// [`safe_to_exec`], spawning is refused rather than exec'ing an
/// attacker-planted binary.
pub fn daemon_bin() -> Result<PathBuf> {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let sibling = dir.join("terhubd");
        if sibling.is_file() {
            return Ok(sibling);
        }
    }
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    find_in_path("terhubd", &path_var)
}

/// How long the spawn waits for the new daemon to answer Ping.
const STARTUP_WAIT: Duration = Duration::from_millis(500);

/// Minimum gap between self-heal spawn attempts. A daemon that cannot
/// come up (no terhubd binary, unwritable runtime dir) would otherwise
/// be re-forked on every TUI tick, turning one dead daemon into a fork
/// storm.
const RESPAWN_BACKOFF: Duration = Duration::from_secs(2);

/// Epoch millis of the last self-heal attempt, or 0 when there was none.
static LAST_RESPAWN_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Pure gate for [`ensure_running`]: an attempt is due only once the
/// backoff since the last one has elapsed.
fn respawn_due(now_ms: u64, last_ms: u64) -> bool {
    if last_ms == 0 || now_ms < last_ms {
        return true; // never tried, or the clock jumped backwards
    }
    now_ms - last_ms >= RESPAWN_BACKOFF.as_millis() as u64
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Fork a detached daemon and wait for it to answer Ping. Detaching is
/// what keeps it alive past its launcher: a child in the caller's
/// process group dies with the terminal's SIGHUP, which is how the tray
/// and every TUI end up showing "daemon down" minutes later.
pub fn spawn_daemon() -> Result<()> {
    // Owner-only log file (0600): daemon stderr may contain prompt
    // text. File::create would inherit a permissive umask, so set
    // the mode explicitly at creation.
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(log_path());
    let bin = daemon_bin().context("no safe daemon binary to spawn")?;
    let mut cmd = std::process::Command::new(&bin);
    // Own process group: never share the launcher's SIGHUP fate.
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null());
    match log_file {
        Ok(f) => {
            cmd.stderr(f);
        }
        Err(_) => {
            cmd.stderr(std::process::Stdio::null());
        }
    }
    cmd.spawn()
        .with_context(|| format!("spawn {}", bin.display()))?;
    // The pid file is written by the daemon after a successful bind, so
    // Ping (not pid liveness) is the readiness signal.
    let deadline = STARTUP_WAIT * 10;
    let mut waited = Duration::ZERO;
    while waited < deadline {
        if daemon_alive() {
            return Ok(());
        }
        std::thread::sleep(STARTUP_WAIT);
        waited += STARTUP_WAIT;
    }
    anyhow::bail!("daemon did not come up (see {})", log_path().display())
}

/// Self-heal: start the daemon if it is not running. Cheap when the
/// daemon is healthy (one Ping), and rate-limited when it is not so a
/// permanently broken daemon is retried at most once per
/// [`RESPAWN_BACKOFF`] instead of once per tick.
pub fn ensure_running() -> bool {
    if daemon_alive() {
        return true;
    }
    let now = now_ms();
    let last = LAST_RESPAWN_MS.load(std::sync::atomic::Ordering::Relaxed);
    if !respawn_due(now, last) {
        return false;
    }
    // Record the attempt before spawning so a slow spawn cannot be
    // re-entered by a concurrent caller.
    LAST_RESPAWN_MS.store(now, std::sync::atomic::Ordering::Relaxed);
    spawn_daemon().is_ok()
}

/// Lock the registry, recovering the data when a panicked holder poisoned it.
fn lock_registry(
    registry: &Mutex<HashMap<String, AgentRecord>>,
) -> std::sync::MutexGuard<'_, HashMap<String, AgentRecord>> {
    registry.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "lowercase")]
pub enum Request {
    Ping,
    Snapshot,
    Event {
        event: HookEvent,
    },
    Register {
        pane: String,
        agent: String,
        pid: u32,
        state: AgentState,
    },
    Unregister {
        pane: String,
    },
    Shutdown,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<Snapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    fn ok() -> Self {
        Response {
            ok: true,
            snapshot: None,
            error: None,
        }
    }

    fn err(msg: String) -> Self {
        Response {
            ok: false,
            snapshot: None,
            error: Some(msg),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Snapshot {
    #[serde(default)]
    pub agents: Vec<AgentRecord>,
    #[serde(default)]
    pub updated_at: u64,
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

pub struct Server {
    runtime: PathBuf,
    runtime_guard: RuntimeDirGuard,
    registry: Arc<Mutex<HashMap<String, AgentRecord>>>,
    manifests: Arc<Vec<CompiledManifest>>,
    shutdown: Arc<AtomicBool>,
    tunables: Tunables,
}

/// Create the runtime dir owner-only (0700): it holds the control socket,
/// pid file, and registry. umask-permissive creation would expose them.
///
/// Refuses a symlink at the runtime dir itself: a symlinked dir would
/// have its *target* chmodded (and could redirect the socket/registry
/// to an attacker-controlled path). Pre-existing directories are
/// tightened to 0700 when we own them; non-directory entries and
/// symlinks are an error.
/// Proof that the runtime dir is still the directory we checked.
/// The baseline (dev, ino) is captured from an `O_NOFOLLOW |
/// O_DIRECTORY` open — which atomically refuses a symlink final
/// component — so a later dir→symlink (or dir→dir) swap by a
/// parent-dir writer is detected by [`RuntimeDirGuard::verify`]
/// before the next bind/save instead of being used blindly.
#[derive(Debug, Clone)]
struct RuntimeDirGuard {
    path: PathBuf,
    dev: u64,
    ino: u64,
}

impl RuntimeDirGuard {
    /// Re-verify the dir at a use point (bind, save): the path must
    /// still resolve to the same directory captured at check time.
    /// Fail-closed: any symlink, non-dir, or dev/ino mismatch is an
    /// error and the caller must not touch the dir's contents.
    fn verify(&self) -> Result<()> {
        let link_meta = std::fs::symlink_metadata(&self.path)
            .with_context(|| format!("stat runtime dir {}", self.path.display()))?;
        if link_meta.file_type().is_symlink() {
            anyhow::bail!(
                "runtime dir {} is a symlink; refusing to use it",
                self.path.display()
            );
        }
        let meta = std::fs::metadata(&self.path)
            .with_context(|| format!("stat runtime dir {}", self.path.display()))?;
        if !meta.is_dir() {
            anyhow::bail!(
                "runtime dir {} is no longer a directory; refusing to use it",
                self.path.display()
            );
        }
        if meta.dev() != self.dev || meta.ino() != self.ino {
            anyhow::bail!(
                "runtime dir {} was replaced after the startup check; refusing to use it",
                self.path.display()
            );
        }
        Ok(())
    }
}

fn ensure_runtime_dir(path: &Path) -> Result<RuntimeDirGuard> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => {
            anyhow::bail!(
                "runtime dir {} is a symlink; refusing to use it",
                path.display()
            );
        }
        Ok(m) if m.is_dir() => {
            let mut perms = std::fs::metadata(path)?.permissions();
            perms.set_mode(0o700);
            std::fs::set_permissions(path, perms)?;
        }
        Ok(_) => {
            anyhow::bail!(
                "runtime dir {} exists but is not a directory",
                path.display()
            )
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(path)
                .with_context(|| format!("create runtime dir {}", path.display()))?;
        }
        Err(e) => return Err(e.into()),
    }
    // Atomic baseline: this open refuses a symlink final component
    // (O_NOFOLLOW) and non-dirs (O_DIRECTORY) in one step, so there is
    // no check-then-use gap for the final component; fstat on the fd
    // pins the (dev, ino) that verify() compares against later.
    let fd = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(path)
        .with_context(|| format!("open runtime dir {}", path.display()))?;
    let meta = fd.metadata()?;
    Ok(RuntimeDirGuard {
        path: path.to_path_buf(),
        dev: meta.dev(),
        ino: meta.ino(),
    })
}

/// Tighten an already-created runtime file (socket, pid, log) to
/// owner-only (0600). Creation inherits the umask, so this is the
/// explicit chmod that covers a permissive umask or a runtime dir
/// shared with other users.
fn set_owner_only(path: &Path) -> Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod 0600 {}", path.display()))
}

/// Write the pid file atomically (tmp + rename) with owner-only
/// (0600) creation mode. Rename preserves the tmp mode, so the final
/// pid file is 0600 regardless of umask.
fn write_pid_file(pid_file: &Path, pid: &str) -> Result<()> {
    let proc = std::process::id();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = pid_file.with_extension(format!("tmp.{proc}.{nonce}"));
    std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut f| {
            use std::io::Write;
            f.write_all(pid.as_bytes())
        })
        .with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, pid_file)?;
    Ok(())
}

impl Server {
    pub fn load(runtime: PathBuf) -> Result<Self> {
        let runtime_guard = ensure_runtime_dir(&runtime)?;
        let mut manifests = builtin_manifests();
        let (user_manifests, mut warnings) = load_manifests(&config::manifests_dir());
        manifests.extend(user_manifests);
        let (compiled, compile_warnings) = compile_all_lossy(&manifests);
        warnings.extend(compile_warnings);
        for w in &warnings {
            eprintln!("terhubd: broken manifest skipped ({w})");
        }
        let registry = load_registry(&runtime.join("registry.json"));
        history::prune(&history::ledger_path(), now_unix(), 90);
        let tunables = Tunables::from_config(&config::load_config(None));
        Ok(Server {
            runtime,
            runtime_guard,
            registry: Arc::new(Mutex::new(registry)),
            manifests: Arc::new(compiled),
            shutdown: Arc::new(AtomicBool::new(false)),
            tunables,
        })
    }

    pub async fn run(&self) -> Result<()> {
        // Re-verify before touching anything: the dir may have been
        // swapped (dir→symlink) between load() and run().
        self.runtime_guard
            .verify()
            .context("runtime dir changed since startup check")?;
        let sock = self.runtime.join("terhubd.sock");
        let pid_file = self.runtime.join("terhubd.pid");
        // Single-instance lock, held for the daemon's whole life. The
        // connect-probe/remove/bind sequence below is a TOCTOU: two
        // daemons started together can both pass the probe, and the
        // loser's remove_file would unlink the *winner's* live socket
        // (both would then poll, notify, and fire automation twice).
        // The lock file is never renamed or deleted while held, so the
        // flock outlives the pid file's atomic replace.
        let _lock_file = acquire_daemon_lock(&self.runtime)?;
        if std::os::unix::net::UnixStream::connect(&sock).is_ok() {
            anyhow::bail!("a daemon is already listening on {}", sock.display());
        }
        let _ = std::fs::remove_file(&sock); // stale socket from a crash
        let listener =
            UnixListener::bind(&sock).with_context(|| format!("bind {}", sock.display()))?;
        // Owner-only socket (0600): the bind inherits the umask, so
        // tighten it explicitly. The runtime dir is already 0700; this
        // covers TERHUB_RUNTIME_DIR pointed at a shared dir.
        set_owner_only(&sock)?;
        // Pid file only after a successful bind: a concurrent-start loser
        // fails above without clobbering the winner's pid with dead data.
        // Atomic (tmp + rename): a torn write would leave a partial pid
        // for the next start to read.
        {
            let pid = std::process::id().to_string();
            write_pid_file(&pid_file, &pid)?;
        }

        let reg = self.registry.clone();
        let manifests = self.manifests.clone();
        let runtime = self.runtime.clone();
        let runtime_guard = self.runtime_guard.clone();
        let tunables = self.tunables;
        let shutdown_flag = self.shutdown.clone();
        let poll = tokio::spawn(async move {
            poll_loop(
                reg,
                manifests,
                runtime,
                runtime_guard,
                tunables,
                shutdown_flag,
            )
            .await
        });

        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                break;
            }
            let accepted =
                tokio::time::timeout(Duration::from_millis(500), listener.accept()).await;
            let stream = match accepted {
                Ok(Ok((s, _))) => s,
                Ok(Err(e)) => {
                    // A real accept error (EMFILE/ENFILE under fd
                    // exhaustion, etc.) is not the same as a quiet
                    // timeout. Log it and back off so we don't busy-spin
                    // a whole core, then loop so the shutdown flag still
                    // gets a chance to fire.
                    eprintln!("terhubd: accept error: {e}");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    continue;
                }
                Err(_) => continue, // timeout: loop to check shutdown
            };
            let reg = self.registry.clone();
            let runtime = self.runtime.clone();
            let runtime_guard = self.runtime_guard.clone();
            let shutdown = self.shutdown.clone();
            tokio::spawn(async move {
                handle_conn(stream, reg, runtime, runtime_guard, shutdown).await
            });
        }

        // Graceful drain: the flag is set, so the poll loop breaks within
        // 100ms (or finishes its in-flight poll first). `daemon stop`
        // escalates to TERM after 3s if a wedged poll hangs this.
        let _ = poll.await;
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&pid_file);
        Ok(())
    }
}

/// Max request line the daemon accepts: hook events are small JSON, so
/// 1MB is vast headroom — anything bigger truncates into "bad request".
const MAX_REQUEST_BYTES: u64 = 1024 * 1024;

async fn handle_conn(
    stream: TokioUnixStream,
    registry: Arc<Mutex<HashMap<String, AgentRecord>>>,
    runtime: PathBuf,
    runtime_guard: RuntimeDirGuard,
    shutdown: Arc<AtomicBool>,
) {
    let (reader, mut writer) = stream.into_split();
    // Bounded read: a peer that never sends a newline (or sends megabytes)
    // hits EOF at the cap instead of growing toward OOM.
    let mut lines = TokioBufReader::new(reader.take(MAX_REQUEST_BYTES)).lines();
    let line = match tokio::time::timeout(Duration::from_secs(10), lines.next_line()).await {
        Ok(Ok(Some(l))) => l,
        _ => return,
    };
    let req: Request = match serde_json::from_str(&line) {
        Ok(r) => r,
        Err(e) => {
            let _ = write_response(&mut writer, &Response::err(format!("bad request: {e}"))).await;
            return;
        }
    };
    let resp = match req {
        Request::Ping => Response::ok(),
        Request::Shutdown => {
            shutdown.store(true, Ordering::SeqCst);
            Response::ok()
        }
        Request::Snapshot => {
            let agents = {
                let reg = lock_registry(&registry);
                let mut v: Vec<AgentRecord> = reg.values().cloned().collect();
                v.sort_by(|a, b| a.pane.cmp(&b.pane));
                v
            };
            Response {
                ok: true,
                snapshot: Some(Snapshot {
                    agents,
                    updated_at: now_unix(),
                }),
                error: None,
            }
        }
        Request::Event { event } => {
            // Mutation + save without holding the lock across disk IO.
            let (transitions, now) = mutate_and_save(
                &registry,
                &runtime_guard,
                &runtime.join("registry.json"),
                |reg| {
                    let now = now_unix();
                    let t = apply_hook_event(reg, &event, now);
                    (t, now)
                },
            );
            emit_transitions(&transitions, now);
            // Prompts log even when the state didn't move: asking again is
            // itself the event the recap reads back.
            if !event.prompt.trim().is_empty() {
                log_entry(&history::Entry::prompt(
                    now,
                    &event.pane,
                    &crate::proc::canonical_agent(&event.agent),
                    &event.prompt,
                ));
            }
            Response::ok()
        }
        Request::Register {
            pane,
            agent,
            pid,
            state,
        } => {
            mutate_and_save(
                &registry,
                &runtime_guard,
                &runtime.join("registry.json"),
                |reg| {
                    let now = now_unix();
                    apply_register(reg, &pane, &agent, pid, state, now);
                    crate::agent::ensure_handles(reg);
                },
            );
            Response::ok()
        }
        Request::Unregister { pane } => {
            let dropped = mutate_and_save(
                &registry,
                &runtime_guard,
                &runtime.join("registry.json"),
                |reg| apply_unregister(reg, &pane),
            );
            if dropped {
                Response::ok()
            } else {
                Response::err(format!("no manual pin on {pane}"))
            }
        }
    };
    let _ = write_response(&mut writer, &resp).await;
}

async fn write_response(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    resp: &Response,
) -> Result<()> {
    let mut line = serde_json::to_string(resp)?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    Ok(())
}

/// Resolve when the shutdown flag sets, polling at 10Hz. The atomic flag
/// (not a Notify) keeps handle_conn's fire-and-forget Shutdown send simple;
/// 100ms is prompt next to a multi-second poll interval.
async fn shutdown_wait(shutdown: &AtomicBool) {
    while !shutdown.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn poll_loop(
    registry: Arc<Mutex<HashMap<String, AgentRecord>>>,
    manifests: Arc<Vec<CompiledManifest>>,
    runtime: PathBuf,
    runtime_guard: RuntimeDirGuard,
    tunables: Tunables,
    shutdown: Arc<AtomicBool>,
) {
    // A zero interval would panic; config validation keeps this >= 1, and
    // the clamp makes direct construction safe too.
    let mut tick = tokio::time::interval(Duration::from_secs(tunables.poll_secs.max(1)));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Prune runs hourly (independent of the poll tick) so a daemon
    // that's been up for weeks doesn't grow the ledger without bound.
    // The prune call is bounded and atomic, so it's safe to run on
    // the poll loop's task.
    let mut prune_tick = tokio::time::interval(Duration::from_secs(3600));
    prune_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut prev_sessions: Option<std::collections::HashSet<String>> = None;
    let mut prev_foreground: Option<Option<String>> = None;
    let automation_busy = Arc::new(AtomicBool::new(false));
    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        tokio::select! {
            _ = tick.tick() => {},
            _ = prune_tick.tick() => {
                let _ = tokio::task::spawn_blocking(|| {
                    history::prune(&history::ledger_path(), now_unix(), 90);
                }).await;
                continue;
            },
            _ = shutdown_wait(&shutdown) => break,
        }
        let poll_start = now_unix();
        // Fail-closed: a swapped runtime dir must stop saves, and a
        // daemon operating on an attacker-chosen dir must not keep
        // running — trip shutdown so the process exits and a fresh
        // start re-checks from scratch.
        if let Err(e) = runtime_guard.verify() {
            eprintln!("terhubd: runtime dir check failed: {e:#}; shutting down");
            shutdown.store(true, Ordering::SeqCst);
            break;
        }
        let prev = lock_registry(&registry).clone();
        let manifests = manifests.clone();
        let tune = tunables;
        let next = tokio::task::spawn_blocking(move || poll_sync(prev, &manifests, tune)).await;
        match next {
            Ok((mut next, sessions, foreground, outage)) => {
                // Fold the poll result back under one lock hold: hooks that
                // landed during the poll are in the live registry but were
                // never in the pre-poll clone poll_sync saw, so merging them
                // here (and diffing against live, not the clone) is what
                // keeps the write-back from silently discarding them.
                // Save runs outside the lock so a slow disk doesn't stall
                // every concurrent snapshot/automation reader.
                let transitions = {
                    let mut reg = lock_registry(&registry);
                    merge_mid_poll_hooks(&mut next, &reg, poll_start);
                    let t = detect_all_transitions(&reg, &next);
                    *reg = next;
                    crate::agent::ensure_handles(&mut reg);
                    let snap = (*reg).clone();
                    drop(reg);
                    // Fail-closed like mutate_and_save: the tick check above
                    // ran before the long poll_sync, so a dir→symlink swap
                    // mid-poll must not land this write-back in an
                    // attacker-chosen dir. Skip (and log) on failure; the
                    // next tick check trips shutdown right after.
                    match runtime_guard.verify() {
                        Ok(()) => save_registry(&runtime.join("registry.json"), &snap),
                        Err(e) => eprintln!("terhubd: refusing registry save: {e:#}"),
                    }
                    t
                };
                let now = now_unix();
                emit_transitions(&transitions, now);
                // Outage: sessions/foreground unknown — skip diffing AND the
                // prev update, so recovery diffs against last-known-good and
                // neither direction logs phantoms.
                if !outage {
                    let sessions: std::collections::HashSet<String> =
                        sessions.into_iter().collect();
                    if let Some(prev) = prev_sessions.replace(sessions.clone()) {
                        for opened in sessions.difference(&prev) {
                            log_entry(&history::Entry::session_event(now, opened, true));
                        }
                        for closed in prev.difference(&sessions) {
                            log_entry(&history::Entry::session_event(now, closed, false));
                        }
                    }
                    if let Some(prev) = prev_foreground.replace(foreground.clone())
                        && prev != foreground
                    {
                        log_entry(&history::Entry::human_focus(now, foreground.as_deref()));
                    }
                }
                // Automation runs detached so a slow shell action can't stall
                // polls; the in-flight guard means passes never overlap, so
                // guardrails still can't double-fire (a tick that finds a
                // pass running just skips — the running pass covers it).
                //
                // The guard resets on Drop, so a task that gets cancelled
                // (runtime shutdown, panic) clears the flag and the next
                // tick can run automation again instead of silently never
                // firing for the rest of the daemon's life.
                if !automation_busy.swap(true, Ordering::SeqCst) {
                    let reg = Arc::clone(&registry);
                    let busy = Arc::clone(&automation_busy);
                    tokio::spawn(async move {
                        let _guard = AutomationGuard(busy);
                        run_automation(&reg).await;
                    });
                }
            }
            Err(_) => break, // runtime shutting down
        }
    }
}

fn log_entry(entry: &history::Entry) {
    let _ = history::append_entry(&history::ledger_path(), entry);
}

/// Pin a manual agent record (`terhub register`). Same agent + state
/// refreshes the pin in place (new pid wins, clock kept); anything else
/// is a fresh pin with a fresh clock. Pure registry work for the lock.
fn apply_register(
    reg: &mut HashMap<String, AgentRecord>,
    pane: &str,
    agent: &str,
    pid: u32,
    state: AgentState,
    now: u64,
) {
    let agent = crate::proc::canonical_agent(agent);
    match reg.get(pane) {
        // Same pin refreshed: the new pid wins, the clock is kept so a
        // re-register after an agent restart doesn't renotify.
        Some(old) if old.source == Source::Manual && old.agent == agent && old.state == state => {
            let mut rec = old.clone();
            rec.reg_pid = pid;
            rec.updated_at = now;
            rec.cpu_ticks = 0;
            rec.cpu_still_since = 0;
            rec.agent_start = 0;
            reg.insert(pane.to_string(), rec);
        }
        _ => {
            // Fresh pin (or a pin over real evidence): fresh clock, but
            // keep pane metadata and model/ctx so the row isn't blank.
            let old = reg.get(pane);
            // A changed agent drops the name so ensure_handles mints a
            // fresh one (same rule as hook events).
            let handle = match old {
                Some(o) if o.agent == agent => o.handle.clone(),
                _ => String::new(),
            };
            // Entering Working (or a new agent taking the pane) is
            // activity in itself, pixels or not: restart the
            // visible-quiet clock (same rule as hook events).
            let fresh_work = state == AgentState::Working
                || old.map(|r| r.agent != agent).unwrap_or(true);
            reg.insert(
                pane.to_string(),
                AgentRecord {
                    pane: pane.to_string(),
                    agent,
                    state,
                    source: Source::Manual,
                    summary: String::new(),
                    session: old.map(|r| r.session.clone()).unwrap_or_default(),
                    window: old.map(|r| r.window.clone()).unwrap_or_default(),
                    command: old.map(|r| r.command.clone()).unwrap_or_default(),
                    updated_at: now,
                    state_since: now,
                    cpu_ticks: 0,
                    cpu_still_since: 0,
                    agent_start: 0,
                    last_turn_secs: old.map(|r| r.last_turn_secs).unwrap_or(0),
                    screen_hash: old.map(|r| r.screen_hash).unwrap_or(0),
                    quiet_since: if fresh_work {
                        now
                    } else {
                        old.map(|r| r.quiet_since).unwrap_or(0)
                    },
                    last_prompt_at: old.map(|r| r.last_prompt_at).unwrap_or(0),
                    last_prompt: old.map(|r| r.last_prompt.clone()).unwrap_or_default(),
                    handle,
                    model: old.map(|r| r.model.clone()).unwrap_or_default(),
                    ctx_pct: old.and_then(|r| r.ctx_pct),
                    reg_pid: pid,
                    bell: false,
                    bell_reason: String::new(),
                    stuck_base: None,
                },
            );
        }
    }
}

/// Drop a manual pin (`terhub unregister`). Only Manual records go:
/// a hook/proc/screen record under the pane is real evidence, never
/// removed by unregister. Returns true when a pin was dropped.
fn apply_unregister(reg: &mut HashMap<String, AgentRecord>, pane: &str) -> bool {
    match reg.get(pane) {
        Some(old) if old.source == Source::Manual => {
            reg.remove(pane);
            true
        }
        _ => false,
    }
}

/// Apply one hook event to the registry and report its transitions.
/// Pure registry work — no disk, no subprocess — so callers run it under
/// the lock and emit side effects after unlocking.
fn apply_hook_event(
    reg: &mut HashMap<String, AgentRecord>,
    event: &HookEvent,
    now: u64,
) -> Vec<crate::attend::Transition> {
    let before = reg.clone();
    apply_event_to(reg, event, now);
    crate::agent::ensure_handles(reg);
    detect_all_transitions(&before, reg)
}

/// Emit transition side effects: ledger lines always, a desktop
/// notification for attention states, a one-shot note for completions.
/// Runs WITHOUT the registry lock — a slow notify-send must never stall
/// polls or snapshots.
fn emit_transitions(transitions: &[crate::attend::Transition], now: u64) {
    for t in transitions {
        log_entry(&history::Entry::transition(now, t));
        if t.to.needs_attention() {
            notify_transition(t);
        } else if crate::attend::is_completion(t.from, t.to) {
            crate::attend::notify_done(t);
        }
    }
}

/// Fold hook events that landed in the live registry *during* a poll back
/// into the poll result. `poll_sync` only saw the pre-poll clone, so a blind
/// write-back would silently discard them (lost update); the hook handler
/// already logged their transitions, so these merge silently. Only hook
/// records at least as fresh as `poll_start` (the second the clone was
/// taken) merge — older hooks were already carried or lapsed by the poll
/// itself and must not be resurrected.
fn merge_mid_poll_hooks(
    next: &mut HashMap<String, AgentRecord>,
    live: &HashMap<String, AgentRecord>,
    poll_start: u64,
) {
    for (pane, rec) in live {
        if rec.source == Source::Hook && rec.updated_at >= poll_start {
            next.insert(pane.clone(), rec.clone());
        }
        // A register that landed mid-poll is fresher than the poll's
        // clone and wins the same way hook events do.
        if rec.source == Source::Manual && rec.updated_at >= poll_start {
            next.insert(pane.clone(), rec.clone());
        }
    }
    // ...while an unregister that landed mid-poll must not be
    // resurrected from the pre-poll clone.
    let dropped: Vec<String> = next
        .iter()
        .filter(|(pane, rec)| rec.source == Source::Manual && !live.contains_key(*pane))
        .map(|(pane, _)| pane.clone())
        .collect();
    for pane in dropped {
        next.remove(&pane);
    }
}

/// Log a recurring-condition message once per distinct text (poll-frequency
/// callers must warn, not spam). `None` re-arms the key.
fn note_sticky(key: &'static str, msg: Option<String>) {
    use std::sync::{Mutex, OnceLock};
    static LAST: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    let mut last = LAST
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    match msg {
        Some(m) => {
            if last.get(key) != Some(&m) {
                eprintln!("terhubd: {m}");
                last.insert(key.to_string(), m);
            }
        }
        None => {
            last.remove(key);
        }
    }
}

/// Report an automation load failure through the sticky logger: a corrupt
/// file warns once, a clean load re-arms.
fn note_rules_error(err: Option<&anyhow::Error>) {
    note_sticky("automation", err.map(|e| format!("automation.toml: {e:#}")));
}

/// RAII handle that clears the automation in-flight flag on drop, so a
/// cancelled or panicking task doesn't permanently disable rule firing.
struct AutomationGuard(Arc<AtomicBool>);

impl Drop for AutomationGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Evaluate automation rules against the current registry and carry out
/// firings. The poll loop detaches this with an in-flight guard instead of
/// awaiting it, so a slow shell action delays automation, never polls —
/// while passes still never overlap and guardrails can't double-fire.
async fn run_automation(registry: &Arc<Mutex<HashMap<String, AgentRecord>>>) {
    let rules = match crate::automation::load_rules() {
        Ok(r) => {
            note_rules_error(None);
            r
        }
        Err(e) => {
            note_rules_error(Some(&e));
            return;
        }
    };
    if !rules.iter().any(|r| r.enabled) {
        return;
    }
    let reg = lock_registry(registry).clone();
    let _ = tokio::task::spawn_blocking(move || {
        let ledger = history::ledger_path();
        let fires = history::read_rule_fires(&ledger);
        let now = now_unix();
        let tmux = Tmux::new().ok();
        for (fire, rec) in crate::automation::evaluate(&rules, &reg, &fires, now) {
            let desc = match crate::automation::execute(tmux.as_ref(), &fire, &rec) {
                Ok(d) => d,
                Err(e) => format!("FAILED: {e:?}"),
            };
            let _ = history::append_entry(
                &ledger,
                &history::Entry::rule_fire(&fire.rule, &rec, &desc, now),
            );
        }
    })
    .await;
}

/// Split the previous record into (authoritative, expired). A hook record
/// nobody refreshed recently has lapsed: screen classification takes over,
/// but the expired record still seeds state continuity so a pane that kept
/// its state doesn't renotify.
fn split_prev(
    rec: Option<&AgentRecord>,
    now: u64,
    stale_secs: u64,
) -> (Option<&AgentRecord>, Option<&AgentRecord>) {
    match rec {
        Some(r) if r.source == Source::Hook && now.saturating_sub(r.updated_at) > stale_secs => {
            (None, Some(r))
        }
        other => (other, None),
    }
}

/// One poll pass (blocking tmux calls; runs on a blocking thread).
/// Carry last-known state only while it has recent evidence behind it.
/// Screen records on harness panes are ghosts by construction (screen
/// classification never runs there) and drop immediately instead of
/// lingering out their 30 minutes.
fn carry_record(old: &AgentRecord, pane: &Pane, now: u64, stale_secs: u64) -> bool {
    if old.source == Source::Screen && is_harness_command(&pane.command) {
        return false;
    }
    now.saturating_sub(old.updated_at) <= stale_secs
}

/// Advance the stuck clock for `ticks` observed under `agent`: an
/// unchanged sum from the same process keeps the old still-time, anything
/// else (moved sum, restarted agent, new agent, recycled pid) restarts
/// it. Never trips on first sight. A zero `start` on either side means
/// unknown (legacy caller, unparsable stat, pre-upgrade registry) and
/// skips the identity check rather than forcing a restart.
fn track_cpu(o: &AgentRecord, agent: &str, ticks: u64, start: u64, now: u64) -> (u64, u64) {
    let same_proc = o.agent_start == 0 || start == 0 || o.agent_start == start;
    if o.agent.eq_ignore_ascii_case(agent)
        && o.cpu_ticks == ticks
        && o.cpu_still_since > 0
        && same_proc
    {
        (ticks, o.cpu_still_since)
    } else {
        (ticks, now)
    }
}

/// Strip animation noise from a capture before hashing: footer-carousel
/// lines (`Tip: …`, full-text rotation), braille spinner frames (⠙⠋),
/// block-element meters (█▆▄), and digit runs (elapsed counters, token
/// rates, relative times). Without this a spinner redraw restarts the
/// quiet clock every poll and Idle pegs near zero forever. Digit-only
/// semantic steps ("1 failed" → "2 failed") hide from the hash too — the
/// summary comparison in [`track_quiet_merged`] catches the ones the
/// classifier surfaces.
fn denoise_screen(s: &str) -> String {
    let s = crate::agent::strip_tip_lines(s);
    let mut out = String::with_capacity(s.len());
    let mut in_digits = false;
    for c in s.chars() {
        if ('\u{2800}'..='\u{28ff}').contains(&c) || ('\u{2580}'..='\u{259f}').contains(&c) {
            in_digits = false;
        } else if c.is_ascii_digit() {
            if !in_digits {
                out.push('#');
                in_digits = true;
            }
        } else {
            out.push(c);
            in_digits = false;
        }
    }
    out
}

/// Deterministic screen-change hash, stable across restarts so quiet
/// clocks survive daemon upgrades. Hashes the denoised capture, so
/// spinner frames and counter ticks don't count as screen changes.
/// (One break: hashes from before the denoise upgrade never match, so
/// every quiet clock restarts once on the first post-upgrade poll.)
pub(crate) fn screen_hash(s: &str) -> u64 {
    crate::fnv1a(&denoise_screen(s))
}

/// Advance the visible-quiet clock: same screen hash keeps the old
/// still-time, a new hash restarts it, and no capture (`None`) preserves
/// the clock rather than resetting it on a transient failure.
pub(crate) fn track_quiet(prev: (u64, u64), hash: Option<u64>, now: u64) -> (u64, u64) {
    match hash {
        None => prev,
        Some(h) if h == prev.0 => prev,
        Some(h) => (h, now),
    }
}

/// Merge-post quiet update: a screen change restarts the clock, and so
/// does a changed non-empty summary (it catches digit-only steps like
/// "1 failed" → "2 failed" that denoising hides from the hash). A wiped
/// summary is decay, not activity, and stuck rows are exempt outright —
/// their "no cpu for {dur}" ticker would otherwise read fresh forever.
fn track_quiet_merged(
    prev: (u64, u64),
    hash: Option<u64>,
    carried_summary: &str,
    new_summary: &str,
    state: AgentState,
    now: u64,
) -> (u64, u64) {
    let (h, q) = track_quiet(prev, hash, now);
    if state != AgentState::Stuck && !new_summary.is_empty() && new_summary != carried_summary {
        (h, now)
    } else {
        (h, q)
    }
}

/// Post-merge quiet update: the poller's verbatim step. A screen change
/// restarts the clock (via [`track_quiet_merged`]), and so does entering
/// Working — the state flip itself is activity, and spinner frames are
/// denoised out of the hash, so without this a pane that just started
/// working keeps showing the previous stillness in the Idle column until
/// its pixels visibly move. Demotions into Idle keep the old clock —
/// that stillness is the point.
fn refresh_quiet(
    carried: Option<&AgentRecord>,
    hash: Option<u64>,
    rec: &AgentRecord,
    now: u64,
) -> (u64, u64) {
    let prev_quiet = carried
        .map(|r| (r.screen_hash, r.quiet_since))
        .unwrap_or((0, 0));
    // Pre-fallback summaries: the fill below derives from the same
    // screen the hash already judged, so only merge verdicts count.
    let carried_summary = carried.map(|r| r.summary.as_str()).unwrap_or("");
    let (h, mut q) =
        track_quiet_merged(prev_quiet, hash, carried_summary, &rec.summary, rec.state, now);
    let fresh_work = carried
        .map(|r| r.state != AgentState::Working || r.agent != rec.agent)
        .unwrap_or(true);
    if rec.state == AgentState::Working && fresh_work {
        q = now;
    }
    (h, q)
}

/// Inputs to the shared stuck gate: a live process (`has_proc`) burning
/// no CPU (`still`) for `stuck_secs` on a Working|Idle row is hung —
/// unless the screen has been quiet past `quiet_idle_secs` (finished,
/// not hung) or the gate is disabled (`stuck_secs = 0`).
/// `prev_stuck_since` carries the clock across polls while the trip
/// persists (no renotify); None starts it at now.
struct StuckGate {
    has_proc: bool,
    still: u64,
    quiet_for: u64,
    now: u64,
    stuck_secs: u64,
    quiet_idle_secs: u64,
    prev_stuck_since: Option<u64>,
}

/// The stuck gate, shared by [`merge_record_owned`] and the Hook|Manual
/// carry branch. Returns the Stuck since-clock and summary on trip.
fn stuck_gate_trip(state: AgentState, g: &StuckGate) -> Option<(u64, String)> {
    if !matches!(state, AgentState::Working | AgentState::Idle) {
        return None;
    }
    if g.stuck_secs == 0 || !g.has_proc || g.still == 0 {
        return None;
    }
    if g.now.saturating_sub(g.still) < g.stuck_secs {
        return None;
    }
    // A long-quiet screen is finished, not hung (the `samai` fix).
    if g.quiet_idle_secs > 0 && g.quiet_for >= g.quiet_idle_secs {
        return None;
    }
    Some((
        g.prev_stuck_since.unwrap_or(g.now),
        format!(
            "no cpu for {}",
            crate::history::fmt_dur(g.now.saturating_sub(g.still))
        ),
    ))
}

/// Stuck evaluation for a Hook|Manual-carried record (post CPU/quiet
/// refresh). Hook authority means state carries forward instead of being
/// re-derived, so the trip saves the hook's verdict in `stuck_base`: a
/// still-still CPU keeps Stuck without renotifying, and recovered CPU
/// restores the verdict (clocking the flip, blanking the trip summary so
/// the screen fallback refills it). A hook-claimed Stuck (no trip base)
/// is never touched.
fn apply_hook_stuck_gate(
    r: &mut AgentRecord,
    has_proc: bool,
    now: u64,
    stuck_secs: u64,
    quiet_idle_secs: u64,
) {
    let underlying = r.stuck_base.unwrap_or(r.state);
    let quiet_for = if r.quiet_since > 0 {
        now.saturating_sub(r.quiet_since)
    } else {
        0
    };
    let prev_since = r.stuck_base.map(|_| r.state_since);
    if let Some((since, sum)) = stuck_gate_trip(
        underlying,
        &StuckGate {
            has_proc,
            still: r.cpu_still_since,
            quiet_for,
            now,
            stuck_secs,
            quiet_idle_secs,
            prev_stuck_since: prev_since,
        },
    ) {
        if r.stuck_base.is_none() {
            r.stuck_base = Some(underlying);
        }
        r.state = AgentState::Stuck;
        r.state_since = since;
        r.summary = sum;
    } else if r.stuck_base.is_some() {
        r.state = underlying;
        r.stuck_base = None;
        r.state_since = now;
        r.summary = String::new();
    }
}

/// Reset-each-pass bell evidence: mirror this poll's tmux bell flag onto
/// the record, never carry it. The flag is actionable only when the
/// classifier's bell_override fired — a manifest hit upgraded to Waiting
/// with the `🔔 ` summary prefix. A bare bell with no hit is shell noise
/// (dropped by bell_override), so the record stays clear. The reason is
/// the matched waiting pattern with the prefix stripped, so the chip can
/// render `🔔 y/n` instead of just `🔔`.
fn bell_evidence(hit: Option<&ScreenHit>, pane_bell: bool) -> (bool, String) {
    if !pane_bell {
        return (false, String::new());
    }
    match hit {
        Some(h) => match h.summary.strip_prefix("🔔 ") {
            Some(reason) if !reason.is_empty() => (true, reason.to_string()),
            // Override fired on an empty match line: still a real bell.
            Some(_) => (true, "bell".to_string()),
            // No prefix: bell_override declined (e.g. an Error/Stuck row
            // keeps its verdict), so there is no bell evidence to record.
            None => (false, String::new()),
        },
        None => (false, String::new()),
    }
}

/// Merge one poll's evidence into a record, legacy entry: no snapshot in
/// scope here, so editor ownership falls back to the old rule (editor
/// foreground plus any process hit) and the starttime is unknown (0).
/// Test-only: the poller calls [`merge_record_owned`] with the real
/// subtree check and starttime instead.
#[cfg(test)]
fn merge_record(
    carried: Option<&AgentRecord>,
    lapsed: bool,
    screen: Option<ScreenHit>,
    proc_hit: Option<(&str, u64)>,
    pane: &Pane,
    now: u64,
    stuck_secs: u64,
) -> Option<AgentRecord> {
    merge_record_owned(
        carried,
        lapsed,
        screen,
        proc_hit.map(|(pa, t)| (pa, t, 0)),
        pane,
        is_editor_command(&pane.command),
        now,
        stuck_secs,
        QUIET_IDLE_SECS,
        WAITING_IDLE_SECS,
    )
}

/// Merge one poll's evidence into a record. Identity priority: process tree
/// beats screen guess (a `pi` binary is a fact; screen text is a hint).
/// State priority: screen waiting/error/limited/done beats everything here (fresh hooks
/// never reach this function); a live agent with an unreadable screen is
/// working; a working agent with a long-still CPU sum is stuck. Anonymous
/// screen states (patterns, no name) only survive paired with a process
/// hit — otherwise a micro `>` bar would impersonate an agent. Returns
/// None when nothing claims the pane.
///
/// `has_editor` is the poller's verdict that the foreground editor sits
/// inside this agent's own subtree — a background agent never claims a
/// sibling editor. The proc triple is (agent, subtree ticks, starttime).
#[allow(clippy::too_many_arguments)] // one verdict per evidence source; a struct would churn 25 test sites
fn merge_record_owned(
    carried: Option<&AgentRecord>,
    lapsed: bool,
    screen: Option<ScreenHit>,
    proc_hit: Option<(&str, u64, u64)>,
    pane: &Pane,
    has_editor: bool,
    now: u64,
    stuck_secs: u64,
    quiet_idle_secs: u64,
    waiting_idle_secs: u64,
) -> Option<AgentRecord> {
    use AgentState::{Done, Error, Idle, Limited, Waiting, Working};
    let (agent, state, source) = match (proc_hit, screen.as_ref()) {
        // A done screen beats a live process: finished agents keep their
        // REPL open. (Stale banners can't pin this: freshest-line-wins
        // means live footer chrome below them classifies first.)
        (Some((pa, _, _)), Some(hit))
            if hit.state == Waiting
                || hit.state == Error
                || hit.state == Limited
                || hit.state == Idle
                || hit.state == Done =>
        {
            (pa.to_string(), hit.state, Source::Proc)
        }
        // Agent summoned $EDITOR: the foreground program awaits keystrokes —
        // but only its own editor, never a sibling's.
        (Some((pa, _, _)), _) if is_editor_command(&pane.command) && has_editor => {
            (pa.to_string(), Waiting, Source::Proc)
        }
        (Some((pa, _, _)), _) => (pa.to_string(), Working, Source::Proc),
        (None, Some(hit)) => {
            let sa = hit.agent.as_ref()?;
            (sa.clone(), hit.state, Source::Screen)
        }
        (None, None) => return None,
    };
    // A lapsed hook record still seeds continuity: same state keeps its
    // clock (no renotify) but drops its summary — hook text from 30+
    // minutes ago is rot, not information.
    let (mut since, mut summary) = match carried {
        Some(o) if o.state == state && o.agent.eq_ignore_ascii_case(&agent) => (
            o.state_since,
            if lapsed {
                String::new()
            } else {
                o.summary.clone()
            },
        ),
        _ => (now, String::new()),
    };
    // Waiting/error/limited rows show the prompt (or error) line that
    // tripped the classifier — the text you must read to act. Idle and
    // Done rows show the completion line, for the same reason. Refreshed
    // every poll; the clock above still decides notifications. Working
    // rows stay summary-free: spinner text is noise, not information.
    if matches!(state, Waiting | Error | Limited | Idle | Done)
        && let Some(hit) = screen.as_ref()
        && !hit.summary.is_empty()
    {
        summary = hit.summary.clone();
    }
    // Working rows stay summary-free even across carries: a prompt line
    // borrowed while waiting is rot once the agent works again (and
    // pre-gate summaries must not ride the carry forever).
    if state == Working {
        summary = String::new();
    }
    let (ticks, still, astart) = match proc_hit {
        Some((pa, t, s)) => carried
            .map(|o| {
                let (ti, st) = track_cpu(o, pa, t, s, now);
                (ti, st, s)
            })
            .unwrap_or((t, now, s)),
        None => (0, 0, 0),
    };
    let mut final_state = state;
    // How long the screen has been frozen — the one signal that
    // separates a *hung* agent from a *finished* one. A hung agent's
    // UI loop dies with its spinner: the screen freezes *recently* and
    // the process stops burning CPU. A finished agent's screen has
    // been quiet for a long time (its REPL sits idle). Both read
    // "no CPU" — only the quiescence clock tells them apart. 0 means
    // "not tracked yet" (first poll), which the gate treats as
    // recently-active so a real hang is caught on the first tick.
    let quiet_for = carried
        .map(|o| o.quiet_since)
        .filter(|q| *q > 0)
        .map(|q| now.saturating_sub(q))
        .unwrap_or(0);
    // Stuck is decided by CPU stillness on a live process: a leaf that
    // stopped making forward progress. It fires on Working and Idle
    // rows (a finished-looking idle row whose spinner just died is a
    // hang that finished), and `stuck_secs = 0` disables the gate
    // entirely. The quiescence guard is the discriminator: a screen
    // that has been frozen past `quiet_idle_secs` is finished, not
    // hung, so it must not be re-flagged stuck — this is the `samai`
    // fix. A screen that froze *recently* (spinner just stopped) with
    // no CPU is the genuine hang, and the gate still catches it. The
    // same gate (shared helper) runs on Hook|Manual carries, which
    // otherwise never evaluate Stuck.
    let prev_stuck_since = match carried {
        Some(o) if o.state == AgentState::Stuck && o.agent.eq_ignore_ascii_case(&agent) => {
            Some(o.state_since)
        }
        _ => None,
    };
    if let Some((stuck_since, stuck_summary)) = stuck_gate_trip(
        final_state,
        &StuckGate {
            has_proc: proc_hit.is_some(),
            still,
            quiet_for,
            now,
            stuck_secs,
            quiet_idle_secs,
            prev_stuck_since,
        },
    ) {
        final_state = AgentState::Stuck;
        since = stuck_since;
        summary = stuck_summary;
    }
    // Quiescence is evidence in its own right: a live process whose
    // screen has not moved is finished. The Stuck gate above already
    // declined to fire on a long-quiet screen, so this promotion is
    // what lands a finished pane on `Idle`. The order is deliberate:
    // gate first (so a recently-frozen hang escalates before the
    // demotion can mask it), then promotion.
    if final_state == Working && quiet_idle_secs > 0 && quiet_for >= quiet_idle_secs {
        final_state = Idle;
        since = match carried {
            Some(o) if o.state == Idle => o.state_since,
            _ => now,
        };
        // No summary here on purpose. The poller re-evaluates quiet with
        // track_quiet_merged right after this, and a summary *derived
        // from the quiet clock* reads as fresh activity and resets the
        // clock this branch just measured — the row flickers Idle for
        // exactly one poll and falls back to Working forever. The Idle
        // column already shows the duration; `fallback_summary_for`
        // fills the cell with the last screen line instead.
        summary = String::new();
    }
    // A prompt nobody has touched for a long time is no longer urgent.
    // Agents whose CLI ends every turn with a bare `>` (gemini, and
    // anything with the same shape) classify as `waiting` forever once
    // the work is done, so without this they sit in "need you" long
    // after you walked away. Deliberately a much longer threshold than
    // the working flip: stepping away for a few minutes must not
    // silence a question. The summary is kept — it is the question you
    // would have come back to read — and being stable it does not feed
    // the quiet clock.
    if final_state == Waiting && waiting_idle_secs > 0 && quiet_for >= waiting_idle_secs {
        final_state = Idle;
        since = match carried {
            Some(o) if o.state == Idle => o.state_since,
            _ => now,
        };
    }
    let last_turn_secs = match carried {
        Some(o) if crate::agent::turn_ended(o.state, final_state) => {
            since.saturating_sub(o.state_since)
        }
        Some(o) => o.last_turn_secs,
        None => 0,
    };
    // Handles follow the agent, not the pane: a changed agent drops the
    // old name so ensure_handles mints a fresh one.
    let handle = match carried {
        Some(o) if o.agent == agent => o.handle.clone(),
        _ => String::new(),
    };
    // Model/ctx ride a same-agent carry (polls never report them); on
    // an agent change they belong to the *old* agent and are dropped,
    // like the handle and the pinned pid (a new agent owns new ones).
    let (model, ctx_pct, reg_pid) = match carried {
        Some(o) if o.agent == agent => (o.model.clone(), o.ctx_pct, o.reg_pid),
        _ => (String::new(), None, 0),
    };
    // Same rule for the last prompt (computed before `agent` moves).
    let (last_prompt_at, last_prompt) = match carried {
        Some(o) if o.agent == agent => (o.last_prompt_at, o.last_prompt.clone()),
        _ => (0, String::new()),
    };
    Some(AgentRecord {
        pane: pane.id.clone(),
        agent,
        state: final_state,
        source,
        summary,
        handle,
        model,
        ctx_pct,
        reg_pid,
        session: pane.session.clone(),
        window: pane.window_name.clone(),
        command: pane.command.clone(),
        updated_at: now,
        state_since: since,
        cpu_ticks: ticks,
        cpu_still_since: still,
        agent_start: astart,
        last_turn_secs,
        // Quiet is set by the poller post-merge (it owns the capture).
        screen_hash: 0,
        quiet_since: 0,
        // Polls never wipe hook prompts: carry the old cut forward —
        // but only for the same agent (see `last_prompt` above).
        last_prompt_at,
        last_prompt,
        bell: carried.map(|o| o.bell).unwrap_or(false),
        bell_reason: carried.map(|o| o.bell_reason.clone()).unwrap_or_default(),
        // Merge re-derives state every poll: no trip base survives it.
        stuck_base: None,
    })
}

/// Summary fallback: the freshest non-blank screen line, so panes whose
/// agent was claimed by process (or whose hook lapsed) still say something
/// instead of showing an empty Summary cell. Hooks with real summaries and
/// screen-pattern hits keep theirs — this only fills blanks.
fn screen_fallback_summary(screen: Option<&str>) -> String {
    const MAX: usize = 80;
    let line = screen
        .unwrap_or("")
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .unwrap_or("");
    let mut out: String = line
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX)
        .collect();
    if line.chars().count() > MAX {
        out.push('…');
    }
    out
}

/// Screen-line fallback gated to states whose screen names actionable text
/// (a prompt, error, or completion line). Working rows stay summary-free —
/// spinner text is noise, not information — and Stuck carries its own
/// explanation from [`merge_record`], so neither borrows a screen line here.
fn fallback_summary_for(state: AgentState, screen: Option<&str>) -> String {
    if matches!(
        state,
        AgentState::Waiting
            | AgentState::Error
            | AgentState::Limited
            | AgentState::Idle
            | AgentState::Done
    ) {
        screen_fallback_summary(screen)
    } else {
        String::new()
    }
}

/// Outage result: the registry carries over untouched, and `outage` tells
/// the poll loop to skip session/foreground diffing — unknown is not
/// empty, and diffing it would log phantom open/close/focus events.
fn outage_result(
    prev: HashMap<String, AgentRecord>,
) -> (
    HashMap<String, AgentRecord>,
    Vec<String>,
    Option<String>,
    bool,
) {
    (prev, Vec::new(), None, true)
}

/// Take the single-instance flock for `runtime` (non-blocking). The
/// returned file must be kept alive for the daemon's whole run; the
/// lock releases when it drops (exit, crash, or shutdown alike).
pub fn acquire_daemon_lock(runtime: &std::path::Path) -> Result<std::fs::File> {
    use std::os::unix::io::AsRawFd;
    let lock_path = runtime.join("terhubd.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        // Never truncate: the lock file only ever holds an flock, and
        // its contents are irrelevant (a previous holder may still
        // have written a pid).
        .truncate(false)
        // Close-on-exec: without it every process the daemon spawns
        // (notify-send, an automation rule, a tmux call) inherits the
        // lock fd and keeps the single-instance lock alive in its own
        // right — a child that outlives the daemon would block the next
        // start for as long as it lives. A forked child duplicates the
        // open-file-description, so drop() in the parent does NOT release
        // the lock while such a child lives; O_CLOEXEC ends that at
        // execve instead of at child exit.
        .custom_flags(libc::O_CLOEXEC)
        .open(&lock_path)
        .with_context(|| format!("open lock {}", lock_path.display()))?;
    // SAFETY: flock on a live fd; LOCK_NB so a second daemon fails
    // fast instead of queueing behind the first one's lifetime.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        anyhow::bail!(
            "another terhub daemon already holds {}",
            lock_path.display()
        );
    }
    Ok(file)
}

/// Returns the new registry, the live session list for open/close
/// detection, the foreground pane, and whether tmux was unreachable
/// (sessions/foreground unknown, not empty). Transitions are NOT computed
/// here: the poll loop merges mid-poll hooks first and diffs after, so any
/// diff here would be discarded and recomputed.
fn poll_sync(
    prev: HashMap<String, AgentRecord>,
    manifests: &[CompiledManifest],
    tune: Tunables,
) -> (
    HashMap<String, AgentRecord>,
    Vec<String>,
    Option<String>,
    bool,
) {
    match Tmux::new() {
        // tmux gone: keep stale rather than wipe
        Ok(t) => poll_sync_with(&t, prev, manifests, tune),
        Err(_) => outage_result(prev),
    }
}

/// Serialises every test that forks a child, and the flock test below.
///
/// `flock` is per open-file-description, and `fork` duplicates the
/// description rather than the lock: a child forked while a test holds the
/// daemon lock keeps that lock alive until the child exits, even after the
/// parent has dropped its own fd. Taking the lock test's whole body and
/// every fork site under one guard keeps the two from interleaving, so the
/// "released on drop" arm measures drop semantics instead of whatever
/// sibling happened to fork next. (Rust opens files O_CLOEXEC, so the fd
/// is closed at execve — the window is fork→exec, but that window is
/// exactly where a fast script child still holds it.)
static FORK_GUARD: Mutex<()> = Mutex::new(());

fn fork_guard() -> std::sync::MutexGuard<'static, ()> {
    // Poison-tolerant on purpose: a panicking test must not wedge every
    // other test in the binary behind a lock nobody can take.
    FORK_GUARD.lock().unwrap_or_else(|e| e.into_inner())
}

fn poll_sync_with(
    tmux: &Tmux,
    prev: HashMap<String, AgentRecord>,
    manifests: &[CompiledManifest],
    tune: Tunables,
) -> (
    HashMap<String, AgentRecord>,
    Vec<String>,
    Option<String>,
    bool,
) {
    // Guarded: the fake tmux is a script this calls exec, and every fork
    // must not land inside a concurrent flock assertion.
    let _guard = fork_guard();
    let now = now_unix();
    // One process view shared by every pane: /proc is walked once per
    // poll, and the snapshot is reused for label resolution so we don't
    // walk it a second time inside list_all_panes.
    let snap = crate::proc::Snapshot::capture();
    let panes = match tmux.list_all_panes_with_snap(&snap) {
        Ok(p) => p,
        Err(_) => return outage_result(prev),
    };
    let mut next = HashMap::new();
    for pane in &panes {
        let (old, stale) = split_prev(prev.get(&pane.id), now, tune.stale_secs);
        let proc_hit = snap.detect_agent(pane.pid);
        // Skip idle shells nobody has claimed yet — unless a process hit
        // claims them (an agent behind a shell prompt is still an agent).
        if old.is_none() && proc_hit.is_none() && is_shell_command(&pane.command) {
            continue;
        }
        // One capture feeds classification and the visible-quiet clock.
        // Harness dashboards get hash-only: their screens name agents
        // without running them, but their pixels still go quiet.
        let screen = tmux.capture_pane(&pane.id, 30).ok();
        let hash = screen.as_ref().map(|s| screen_hash(s));
        // Hook and manual records stay authoritative; poll only refreshes
        // their metadata. The one exception is a hook whose agent is no
        // longer the pane's live agent: a process tree that names a
        // *different* agent is not a hint, it is the replacement (the
        // exit-pi/start-muse pane reuse). Letting the ended agent's last
        // hook claim the pane anyway would mask the live agent until the
        // hook stale window ran out. Manual pins are exempt: a human
        // named that agent precisely because detection missed it.
        // NOTE: updated_at is deliberately NOT bumped here: it measures hook
        // freshness (time since the last event), and refreshing it every poll
        // would make stale hooks live forever instead of lapsing. Manual
        // pins never lapse (split_prev keeps them), so the same hold applies.
        // Screen classification runs before the hook branch: a screen
        // that names a *different* agent is replacement evidence too
        // (custom/interpreter-launched agents the proc matcher cannot
        // see), not just a live process tree.
        let screen_hit = if is_harness_command(&pane.command) {
            None
        } else {
            screen.as_deref().and_then(|s| {
                classify_screen(
                    manifests,
                    &pane.command,
                    s,
                    pane.bell,
                    crate::proc::parked_in_tty_read(pane.pid),
                )
            })
        };
        let replaced_by_live_agent = old.is_some_and(|rec| {
            rec.source == Source::Hook
                && (proc_hit
                    .as_ref()
                    .is_some_and(|hit| !hit.agent.eq_ignore_ascii_case(&rec.agent))
                    || screen_hit
                        .as_ref()
                        .and_then(|hit| hit.agent.as_ref())
                        .is_some_and(|agent| !agent.eq_ignore_ascii_case(&rec.agent)))
        });
        if let Some(rec) = old
            && matches!(rec.source, Source::Hook | Source::Manual)
            && !replaced_by_live_agent
        {
            let mut r = rec.clone();
            r.session.clone_from(&pane.session);
            r.window.clone_from(&pane.window_name);
            r.command.clone_from(&pane.command);
            // CPU tracking runs through the hook's life (state untouched) so
            // stuck evaluation is seamless when the hook lapses — and the
            // shared stuck gate runs here too, so a hung hook-claimed pane
            // reads Stuck instead of Working-until-lapse.
            let has_proc = match &proc_hit {
                Some(h) => {
                    let t = snap.subtree_ticks(h.pid);
                    (r.cpu_ticks, r.cpu_still_since) = track_cpu(&r, &h.agent, t, h.start, now);
                    r.agent_start = h.start;
                    true
                }
                // No detection hit, but the pin names a process: track CPU
                // under it so stuck detection works for pinned agents.
                None if r.source == Source::Manual && r.reg_pid != 0 => {
                    let Some(start) = snap.proc_start(r.reg_pid) else {
                        // Pinned process gone: the pin drops even though
                        // the pane is alive.
                        continue;
                    };
                    let agent = r.agent.clone();
                    let t = snap.subtree_ticks(r.reg_pid);
                    (r.cpu_ticks, r.cpu_still_since) = track_cpu(&r, &agent, t, start, now);
                    r.agent_start = start;
                    true
                }
                None => {
                    r.cpu_ticks = 0;
                    r.cpu_still_since = 0;
                    r.agent_start = 0;
                    false
                }
            };
            (r.screen_hash, r.quiet_since) = track_quiet((r.screen_hash, r.quiet_since), hash, now);
            apply_hook_stuck_gate(&mut r, has_proc, now, tune.stuck_secs, tune.quiet_idle_secs);
            // Bell is reset-each-pass: mirror this poll's flag, never carry.
            (r.bell, r.bell_reason) = bell_evidence(screen_hit.as_ref(), pane.bell);
            if r.summary.is_empty() {
                r.summary = fallback_summary_for(r.state, screen.as_deref());
            }
            next.insert(pane.id.clone(), r);
            continue;
        }
        let proc_pair = proc_hit
            .as_ref()
            .map(|h| (h.agent.as_str(), snap.subtree_ticks(h.pid), h.start));
        // The editor gate only fires when the editor sits inside this
        // agent's own subtree — a background agent never claims a sibling.
        let has_editor = is_editor_command(&pane.command)
            && proc_hit
                .as_ref()
                .is_some_and(|h| snap.subtree_has_editor(h.pid));
        let carried = old.or(stale);
        let lapsed = old.is_none() && stale.is_some();
        if let Some(mut rec) = merge_record_owned(
            carried,
            lapsed,
            screen_hit,
            proc_pair,
            pane,
            has_editor,
            now,
            tune.stuck_secs,
            tune.quiet_idle_secs,
            tune.waiting_idle_secs,
        ) {
            (rec.screen_hash, rec.quiet_since) = refresh_quiet(carried, hash, &rec, now);
            // Bell is reset-each-pass: mirror this poll's flag, never carry.
            (rec.bell, rec.bell_reason) = bell_evidence(screen_hit.as_ref(), pane.bell);
            if rec.summary.is_empty() {
                rec.summary = fallback_summary_for(rec.state, screen.as_deref());
            }
            next.insert(pane.id.clone(), rec);
        } else if let Some(o) = old {
            // Pane alive but nothing matches: keep last known state only
            // while it has recent evidence behind it, else drop it rather
            // than show a state nobody can confirm anymore. (updated_at is
            // evidence time, deliberately not refreshed by the poll. The CPU
            // trackers reset: this poll found no agent process.)
            if carry_record(o, pane, now, tune.stale_secs) {
                let mut r = o.clone();
                r.session.clone_from(&pane.session);
                r.window.clone_from(&pane.window_name);
                r.command.clone_from(&pane.command);
                r.cpu_ticks = 0;
                r.cpu_still_since = 0;
                // Quiet is screen evidence, not agent evidence: keep the
                // clock running through the carry.
                (r.screen_hash, r.quiet_since) =
                    track_quiet((r.screen_hash, r.quiet_since), hash, now);
                // Bell is reset-each-pass: mirror this poll's flag, never carry.
                (r.bell, r.bell_reason) = bell_evidence(screen_hit.as_ref(), pane.bell);
                if r.summary.is_empty() {
                    r.summary = fallback_summary_for(r.state, screen.as_deref());
                }
                next.insert(pane.id.clone(), r);
            }
        }
    }
    let mut sessions: Vec<String> = panes.iter().map(|p| p.session.clone()).collect();
    sessions.sort();
    sessions.dedup();
    let foreground = tmux.foreground_pane();
    (next, sessions, foreground, false)
}

fn load_registry(path: &Path) -> HashMap<String, AgentRecord> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return HashMap::new(),
    };
    // Per-record parse: one corrupt record must not sink the registry.
    let values: Vec<serde_json::Value> = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            eprintln!(
                "terhubd: registry {} unreadable ({e:#}); starting fresh",
                path.display()
            );
            return HashMap::new();
        }
    };
    let mut out = HashMap::new();
    let mut dropped = 0;
    for v in values {
        match serde_json::from_value::<AgentRecord>(v) {
            Ok(a) => {
                out.insert(a.pane.clone(), a);
            }
            Err(_) => dropped += 1,
        }
    }
    if dropped > 0 {
        eprintln!(
            "terhubd: registry {}: dropped {dropped} corrupt record(s)",
            path.display()
        );
    }
    out
}

/// Lock the registry, run the mutation, snapshot, drop the lock, then
/// save. Keeping the disk write outside the lock means a hook burst
/// serialises behind the in-memory mutation but never behind the
/// filesystem — every other reader (snapshot, automation) keeps making
/// progress while a save is in flight.
fn mutate_and_save<F, R>(
    registry: &Arc<Mutex<HashMap<String, AgentRecord>>>,
    runtime_guard: &RuntimeDirGuard,
    path: &Path,
    f: F,
) -> R
where
    F: FnOnce(&mut HashMap<String, AgentRecord>) -> R,
{
    let snapshot = {
        let mut reg = lock_registry(registry);
        let result = f(&mut reg);
        let snap = (*reg).clone();
        drop(reg);
        (result, snap)
    };
    let (result, snap) = snapshot;
    // Fail-closed: a swapped runtime dir means the save would land in
    // an attacker-chosen location, so skip it (and log) instead of
    // writing. The poll-loop tick check aborts the daemon right after.
    match runtime_guard.verify() {
        Ok(()) => save_registry(path, &snap),
        Err(e) => eprintln!("terhubd: refusing registry save: {e:#}"),
    }
    result
}

fn save_registry(path: &Path, registry: &HashMap<String, AgentRecord>) {
    let agents: Vec<&AgentRecord> = registry.values().collect();
    let text = match serde_json::to_string_pretty(&agents) {
        Ok(t) => t,
        Err(e) => {
            note_sticky("registry-save", Some(format!("registry serialize: {e:#}")));
            return;
        }
    };
    // Tmp + rename: a crash mid-write never leaves a half file behind.
    // The tmp name includes pid + nanos so two daemons pointed at the
    // same runtime dir (or a leftover tmp from a prior run) interleave
    // instead of clobbering each other. `create_new(true)` is the
    // same-pid same-nanos guard: if another save on the same pid
    // collides (sub-nanosecond race), retry with a fresh nonce rather
    // than truncating a tmp the first save still needs.
    let pid = std::process::id();
    let mut attempt = 0;
    let result: std::io::Result<()> = (|| loop {
        attempt += 1;
        if attempt > 8 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "save_registry: tmp path collided 8 times",
            ));
        }
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let tmp = path.with_extension(format!("tmp.{pid}.{nonce}"));
        match std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&tmp)
        {
            Ok(mut f) => {
                use std::io::Write;
                if let Err(e) = f.write_all(text.as_bytes()) {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(e);
                }
                if let Err(e) = f.sync_all() {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(e);
                }
                drop(f);
                return std::fs::rename(&tmp, path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    })();
    match result {
        Ok(()) => note_sticky("registry-save", None),
        Err(e) => note_sticky(
            "registry-save",
            Some(format!("registry save {}: {e:#}", path.display())),
        ),
    }
}

// ---------------------------------------------------------------------------
// Sync client (CLI + TUI)
// ---------------------------------------------------------------------------

/// Max response line a client accepts: snapshots are KBs, so 16MB is
/// vast headroom — anything bigger is a flood, refused, never OOM.
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// Read one line, refusing payloads over `cap` bytes instead of growing
/// toward OOM on a peer that never sends a newline.
fn read_capped_line(reader: &mut impl BufRead, cap: usize) -> Result<String> {
    let mut line = Vec::new();
    std::io::Read::take(&mut *reader, (cap + 1) as u64).read_until(b'\n', &mut line)?;
    if line.len() > cap {
        anyhow::bail!("line exceeds {cap} bytes");
    }
    if !line.ends_with(b"\n") {
        // EOF without a newline within the cap: treat as a truncated/
        // oversize request rather than returning a partial line that
        // could happen to parse as valid JSON and get applied.
        anyhow::bail!("truncated line (no newline within {cap} bytes)");
    }
    String::from_utf8(line).map_err(|e| anyhow::anyhow!("non-utf8 line: {e}"))
}

pub fn send_request(req: &Request, timeout: Duration) -> Result<Response> {
    let sock = socket_path();
    let mut stream =
        UnixStream::connect(&sock).with_context(|| format!("connect {}", sock.display()))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    stream.flush()?;
    let mut reader = BufReader::new(stream);
    let resp = read_capped_line(&mut reader, MAX_RESPONSE_BYTES)?;
    serde_json::from_str(&resp).context("bad daemon response")
}

pub fn fetch_snapshot(timeout: Duration) -> Result<Snapshot> {
    let resp = send_request(&Request::Snapshot, timeout)?;
    resp.snapshot.context("daemon returned no snapshot")
}

pub fn send_event(ev: &HookEvent, timeout: Duration) -> Result<()> {
    let resp = send_request(&Request::Event { event: ev.clone() }, timeout)?;
    if resp.ok {
        Ok(())
    } else {
        anyhow::bail!(
            resp.error
                .unwrap_or_else(|| "daemon rejected event".to_string())
        )
    }
}

pub fn daemon_alive() -> bool {
    send_request(&Request::Ping, Duration::from_millis(300))
        .map(|r| r.ok)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentState;

    /// Process-wide counter for fake-tmux temp dirs. `std::process::id()`
    /// alone made concurrent sibling tests share a dir, so a `screen.txt`
    /// rewrite in one test could leak into another. The counter is the
    /// established pattern (`NEXT_CFG` in `src/app.rs`); kept local to
    /// this module so a cross-crate helper isn't introduced for a
    /// test-only concern.
    static NEXT_TEST_DIR: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    #[test]
    fn respawn_backoff_gates_repeated_attempts() {
        // A daemon that cannot come up must not be re-forked every tick:
        // only the first attempt after a quiet period is due.
        let backoff = RESPAWN_BACKOFF.as_millis() as u64;
        assert!(respawn_due(1_000, 0), "never tried: first attempt is due");
        assert!(
            !respawn_due(1_000 + backoff - 1, 1_000),
            "inside the backoff: no re-fork"
        );
        assert!(
            respawn_due(1_000 + backoff, 1_000),
            "backoff elapsed: retry is due"
        );
        // A clock that jumped backwards (NTP, suspend) must not wedge
        // the self-heal off forever.
        assert!(respawn_due(500, 1_000), "backwards clock: retry is due");
    }

    #[test]
    fn hook_event_applies_without_side_effects() {
        // The lock-held half of hook handling: mutate the registry and
        // report transitions. Ledger + notify emission is the caller's job
        // after unlocking, so a slow notify-send never stalls polls.
        let mut reg = std::collections::HashMap::new();
        let ev = crate::agent::HookEvent {
            pane: "%1".to_string(),
            agent: "pi".to_string(),
            state: AgentState::Waiting,
            summary: "Approve?".to_string(),
            prompt: String::new(),
            model: String::new(),
            ctx: None,
        };
        let transitions = apply_hook_event(&mut reg, &ev, 100_000);
        assert_eq!(reg["%1"].state, AgentState::Waiting);
        assert_eq!(reg["%1"].summary, "Approve?");
        assert_eq!(transitions.len(), 1);
        assert!(transitions[0].to.needs_attention());
    }

    #[test]
    fn register_upserts_manual_record() {
        let mut reg = HashMap::new();
        apply_register(&mut reg, "%7", "myagent", 4242, AgentState::Working, 1000);
        let rec = &reg["%7"];
        assert_eq!(rec.source, Source::Manual);
        assert_eq!(rec.agent, "myagent");
        assert_eq!(rec.state, AgentState::Working);
        assert_eq!(rec.reg_pid, 4242);
        assert_eq!(rec.updated_at, 1000);
        assert_eq!(rec.state_since, 1000);
        // Same agent + state refreshes the pin in place: new pid wins,
        // the clock is kept (no renotify downstream).
        apply_register(&mut reg, "%7", "myagent", 5000, AgentState::Working, 2000);
        let rec = &reg["%7"];
        assert_eq!(rec.reg_pid, 5000);
        assert_eq!(rec.updated_at, 2000);
        assert_eq!(rec.state_since, 1000);
        // A new state is a fresh pin with a fresh clock.
        apply_register(&mut reg, "%7", "myagent", 5000, AgentState::Waiting, 3000);
        let rec = &reg["%7"];
        assert_eq!(rec.state, AgentState::Waiting);
        assert_eq!(rec.state_since, 3000);
    }

    #[test]
    fn unregister_drops_only_manual() {
        let mut reg = HashMap::new();
        apply_register(&mut reg, "%7", "myagent", 0, AgentState::Working, 1000);
        reg.insert("%1".to_string(), hook_record(1000));
        assert!(apply_unregister(&mut reg, "%7"));
        assert!(!reg.contains_key("%7"));
        // A hook record is real evidence: unregister leaves it alone.
        assert!(!apply_unregister(&mut reg, "%1"));
        assert!(reg.contains_key("%1"));
        // Nothing pinned, nothing dropped.
        assert!(!apply_unregister(&mut reg, "%9"));
    }

    #[test]
    fn manual_pin_never_lapses() {
        // split_prev must keep Manual pins on the authoritative side no
        // matter how old: only unregister (or pane/pid death) drops them.
        let mut rec = hook_record(100);
        rec.source = Source::Manual;
        rec.pane = "%7".to_string();
        let (old, stale) = split_prev(Some(&rec), 100_000, 1800);
        assert!(old.is_some());
        assert!(stale.is_none());
    }

    #[test]
    fn merge_carries_model_and_ctx() {
        // Poll evidence never reports model/ctx: the merge carries the
        // old cut forward like the prompt, never blanks it.
        let mut old = carried("pi", AgentState::Working, 10, 90, 50);
        old.model = "opus-4.5".to_string();
        old.ctx_pct = Some(72);
        let rec = merge_record(
            Some(&old),
            false,
            None,
            Some(("pi", 10)),
            &test_pane(),
            100,
            300,
        )
        .expect("proc hit claims the pane");
        assert_eq!(rec.model, "opus-4.5");
        assert_eq!(rec.ctx_pct, Some(72));
        // Manual pins carry their pid through merges too.
        let mut old = carried("myagent", AgentState::Working, 10, 90, 50);
        old.source = Source::Manual;
        old.reg_pid = 4242;
        let rec = merge_record(
            Some(&old),
            false,
            None,
            Some(("myagent", 10)),
            &test_pane(),
            100,
            300,
        )
        .expect("proc hit claims the pane");
        assert_eq!(rec.reg_pid, 4242);
    }

    #[tokio::test]
    async fn poll_loop_exits_on_shutdown() {
        // A preset shutdown flag stops the loop before the first tick: no
        // tmux, no registry writes, prompt return.
        let registry = Arc::new(Mutex::new(HashMap::new()));
        let shutdown = Arc::new(AtomicBool::new(true));
        let dir = std::env::temp_dir().join(format!("terhub-poll-shutdown-{}", std::process::id()));
        let guard = ensure_runtime_dir(&dir).unwrap();
        tokio::time::timeout(
            Duration::from_secs(5),
            poll_loop(
                registry,
                Arc::new(vec![]),
                dir,
                guard,
                Tunables::default(),
                shutdown,
            ),
        )
        .await
        .expect("poll_loop ignored shutdown");
    }

    #[tokio::test]
    async fn failed_bind_leaves_no_pid_file() {
        // The pid file must be written only after a successful bind: a
        // concurrent-start loser that fails to bind must not clobber the
        // winner's pid with its own dead one. A >108-char socket path
        // fails the bind deterministically.
        let runtime = std::env::temp_dir().join(format!(
            "terhub-pid-order-{}-{}",
            std::process::id(),
            "p".repeat(100)
        ));
        std::fs::create_dir_all(&runtime).unwrap();
        let server = Server {
            runtime: runtime.clone(),
            runtime_guard: ensure_runtime_dir(&runtime).unwrap(),
            registry: Arc::new(Mutex::new(HashMap::new())),
            manifests: Arc::new(vec![]),
            shutdown: Arc::new(AtomicBool::new(false)),
            tunables: Tunables::default(),
        };
        server.run().await.expect_err("overlong socket bound?");
        assert!(
            !runtime.join("terhubd.pid").exists(),
            "loser wrote a pid file"
        );
        let _ = std::fs::remove_dir_all(&runtime);
    }

    #[test]
    fn poll_sync_outage_flags_unknown() {
        // tmux unreachable: the registry carries over untouched, and the
        // outage flag tells the poll loop to skip session/foreground
        // diffing (no phantom open/close/focus events either direction).
        let mut prev = HashMap::new();
        prev.insert("%1".to_string(), hook_record(99_000));
        let tmux = crate::tmux::Tmux::test("/nonexistent-tmux-binary".into());
        let (next, sessions, foreground, outage) =
            poll_sync_with(&tmux, prev, &[], Tunables::default());
        assert!(outage);
        assert_eq!(next.len(), 1);
        assert_eq!(next["%1"].state, AgentState::Waiting);
        assert!(sessions.is_empty());
        assert_eq!(foreground, None);
    }

    /// Fake tmux listing a single `%50` pane running `sleep` (pid
    /// substituted in): capture prints a bare prompt, everything else is
    /// empty. Fields are `\x1f`-separated like the real PANE_FORMAT.
    fn fake_tmux_one_sleep_pane(dir: &std::path::Path, pid: u32) -> crate::tmux::Tmux {
        use std::os::unix::fs::PermissionsExt;
        let sep = "\x1f";
        let line = [
            "%50", "@1", "w", "1", "0", "sleep", "/tmp", "1", "80", "24", "sess", "0",
        ]
        .join(sep);
        // The payload travels as a printf *argument*: the pane id's `%`
        // would otherwise be eaten as a format specifier.
        let payload = format!("{line}{sep}{pid}");
        let script = format!(
            "#!/bin/sh\ncase \"$1\" in\n  list-panes) printf '%s\\n' '{payload}' ;;\n  capture-pane) printf '$ \\n' ;;\n  *) printf '' ;;\nesac\n"
        );
        let path = dir.join("tmux");
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        crate::tmux::Tmux::test(path)
    }

    fn manual_record(pane: &str, pid: u32, updated_at: u64) -> AgentRecord {
        let mut rec = hook_record(updated_at);
        rec.pane = pane.to_string();
        rec.agent = "myagent".to_string();
        rec.state = AgentState::Working;
        rec.source = Source::Manual;
        rec.reg_pid = pid;
        rec
    }

    #[test]
    fn manual_pin_without_pid_survives_stale_window() {
        // A pid-less pin on a live pane is carried forever: staleness is
        // a hook concept (event freshness), pins persist by design.
        let dir = std::env::temp_dir().join(format!("terhub-poll-manual-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let tmux = fake_tmux_one_sleep_pane(&dir, 999_999_999);
        let now = now_unix();
        let mut prev = HashMap::new();
        prev.insert(
            "%50".to_string(),
            manual_record("%50", 0, now.saturating_sub(99_999)),
        );
        let (next, _, _, outage) = poll_sync_with(&tmux, prev, &[], Tunables::default());
        assert!(!outage);
        let rec = next.get("%50").expect("manual pin carried");
        assert_eq!(rec.source, Source::Manual);
        assert_eq!(rec.state, AgentState::Working);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn manual_pin_tracks_reg_pid_cpu() {
        // A live reg_pid feeds the CPU/stuck clocks like a proc hit; the
        // nonzero agent_start proves the pin path ran (the generic carry
        // leaves it zero).
        let mut child = {
            // Guarded: a forked child inherits every fd this process holds,
            // including a sibling test's daemon-lock fd (threads share one
            // fd table), and would hold that lock until its execve.
            let _guard = fork_guard();
            std::process::Command::new("sleep")
                .arg("30")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .spawn()
                .expect("spawn sleep")
        };
        let pid = child.id();
        let dir =
            std::env::temp_dir().join(format!("terhub-poll-manualpid-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let tmux = fake_tmux_one_sleep_pane(&dir, pid);
        let now = now_unix();
        let mut prev = HashMap::new();
        prev.insert("%50".to_string(), manual_record("%50", pid, now));
        let (next, _, _, _) = poll_sync_with(&tmux, prev, &[], Tunables::default());
        let rec = next.get("%50").expect("manual pin carried");
        assert!(rec.agent_start > 0, "reg_pid ticks not tracked");
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn live_muse_replaces_fresh_pi_hook_after_pane_reuse() {
        // Pane reuse, as seen live: Pi emitted its final `session ended`
        // hook, exited, and Muse started in the same pane seconds later.
        // The Pi hook is still inside its freshness window, but the live
        // process tree names Muse — the replacement must win immediately
        // instead of the pane reading `pi · idle · session ended` while
        // Muse is visibly thinking on screen.
        //
        // Per-test unique dir: a process-wide counter (see `NEXT_TEST_DIR`)
        // plus the pid — `std::process::id()` alone made concurrent
        // sibling tests share a dir, so a `screen.txt` rewrite in one
        // could leak into this test's `fake_tmux_file_screen` / `screen.txt`
        // reads. The audit's "1/25" flake was this class.
        let n = NEXT_TEST_DIR.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("terhub-poll-replaced-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A *live* fake muse: a script named like the real binary,
        // alive for the poll. (A symlink to coreutils `sleep` exits
        // instantly — argv0 is foreign — leaving a zombie whose comm
        // used to match as a "live" agent; detection now skips
        // zombies, so the fake must actually run.)
        let muse_bin = dir.join("muse-bin-1.4.6-R6000.1");
        std::fs::write(&muse_bin, "#!/bin/sh\nsleep 30\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&muse_bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut muse = {
            // Guarded: this child inherits every fd the process currently
            // holds open, including any sibling test's daemon-lock fd.
            // Threads share one fd table, so an unguarded fork here would
            // duplicate a lock the sibling is mid-drop on and hold it until
            // this child execve's.
            let _guard = fork_guard();
            std::process::Command::new(&muse_bin)
                .arg("30")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .spawn()
                .expect("spawn fake muse")
        };
        fake_tmux_file_screen(&dir, muse.id(), "muse-bin-1.4.6-R6000.1");
        std::fs::write(
            dir.join("screen.txt"),
            "◇ Thinking (3m 48s · esc to interrupt)\n\n❯ \n  muse-wonder-1.0 · xhigh\n",
        )
        .unwrap();
        let tmux = crate::tmux::Tmux::test(dir.join("tmux"));
        let now = now_unix();
        let mut pi = hook_record(now);
        pi.pane = "%50".to_string();
        pi.agent = "pi".to_string();
        pi.state = AgentState::Idle;
        pi.summary = "session ended".to_string();
        pi.command = "pi".to_string();
        let mut prev = HashMap::new();
        prev.insert("%50".to_string(), pi);

        let (next, _, _, outage) =
            poll_sync_with(&tmux, prev, &builtin_compiled(), Tunables::default());

        assert!(!outage);
        let rec = next.get("%50").expect("live muse record");
        assert_eq!(rec.agent, "muse");
        assert_eq!(rec.source, Source::Proc);
        assert_eq!(rec.state, AgentState::Working);
        let _ = muse.kill();
        let _ = muse.wait();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn screen_only_replacement_supersedes_fresh_hook() {
        // Same pane-reuse shape, but the replacement is invisible to
        // the process matcher (here the pane pid is a plain `sleep`):
        // the screen names muse, so the dead pi hook must still lose
        // instead of masking the pane for the 30-minute stale window.
        let dir =
            std::env::temp_dir().join(format!("terhub-poll-screenreplaced-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut sleeper = {
            // Guarded: see `manual_pin_tracks_reg_pid_cpu` — a forked child
            // inherits the process's fds, so it must not land while a
            // sibling holds the daemon lock.
            let _guard = fork_guard();
            std::process::Command::new("sleep")
                .arg("30")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .spawn()
                .expect("spawn sleep")
        };
        fake_tmux_file_screen(&dir, sleeper.id(), "muse");
        std::fs::write(
            dir.join("screen.txt"),
            "◇ Thinking (3m 48s · esc to interrupt)\n\n❯ \n  muse-wonder-1.0 · xhigh\n",
        )
        .unwrap();
        let tmux = crate::tmux::Tmux::test(dir.join("tmux"));
        let now = now_unix();
        let mut pi = hook_record(now);
        pi.pane = "%50".to_string();
        pi.agent = "pi".to_string();
        pi.state = AgentState::Idle;
        pi.summary = "session ended".to_string();
        pi.command = "pi".to_string();
        let mut prev = HashMap::new();
        prev.insert("%50".to_string(), pi);

        let (next, _, _, outage) =
            poll_sync_with(&tmux, prev, &builtin_compiled(), Tunables::default());

        assert!(!outage);
        let rec = next.get("%50").expect("live muse record");
        assert_eq!(rec.agent, "muse");
        let _ = sleeper.kill();
        let _ = sleeper.wait();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn replacement_does_not_inherit_predecessor_metadata() {
        // On an agent change the new record must not display the old
        // agent's model, context percentage, or last prompt.
        let now = 100_000;
        let mut old = carried("pi", AgentState::Idle, 0, 0, now - 60);
        old.model = "pi-model-1".to_string();
        old.ctx_pct = Some(42);
        old.last_prompt = "the pi prompt".to_string();
        old.last_prompt_at = now - 30;
        old.handle = "pi-handle".to_string();
        old.reg_pid = 4242;
        let rec = merge_record_owned(
            Some(&old),
            false,
            None,
            Some(("muse", 10, 100)),
            &test_pane(),
            false,
            now,
            STUCK_SECS,
            QUIET_IDLE_SECS,
            WAITING_IDLE_SECS,
        )
        .expect("no record");
        assert_eq!(rec.agent, "muse");
        assert!(rec.model.is_empty(), "model leaked: {}", rec.model);
        assert_eq!(rec.ctx_pct, None);
        assert!(rec.last_prompt.is_empty());
        assert_eq!(rec.last_prompt_at, 0);
        assert_eq!(rec.reg_pid, 0);
    }

    #[test]
    fn manual_pin_drops_when_reg_pid_dead() {
        // The pinned process is gone: the pin drops even though the pane
        // is alive (pid 999999999 cannot exist: over the kernel maximum).
        let dir =
            std::env::temp_dir().join(format!("terhub-poll-manualdead-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let tmux = fake_tmux_one_sleep_pane(&dir, 999_999_999);
        let now = now_unix();
        let mut prev = HashMap::new();
        prev.insert("%50".to_string(), manual_record("%50", 999_999_999, now));
        let (next, _, _, _) = poll_sync_with(&tmux, prev, &[], Tunables::default());
        assert!(!next.contains_key("%50"), "dead-pin record should drop");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Fake tmux with a file-backed screen: rewrite `screen.txt` between
    /// polls to change what the pane shows. Command + pid substituted in.
    fn fake_tmux_file_screen(dir: &std::path::Path, pid: u32, command: &str) {
        use std::os::unix::fs::PermissionsExt;
        let sep = "\x1f";
        let line = [
            "%50", "@1", "w", "1", "0", command, "/tmp", "1", "80", "24", "sess", "0",
        ]
        .join(sep);
        let payload = format!("{line}{sep}{pid}");
        let screen = dir.join("screen.txt");
        let script = format!(
            "#!/bin/sh\ncase \"$1\" in\n  list-panes) printf '%s\\n' '{payload}' ;;\n  capture-pane) cat '{}' ;;\n  *) printf '' ;;\nesac\n",
            screen.display()
        );
        let path = dir.join("tmux");
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(&screen, "$ \n").unwrap();
    }

    fn builtin_compiled() -> Vec<crate::agent::CompiledManifest> {
        let (compiled, _) = crate::agent::compile_all_lossy(&crate::agent::builtin_manifests());
        compiled
    }

    #[test]
    fn digit_only_error_step_keeps_hash_but_resets_quiet() {
        // "Error: 1 failed" → "Error: 2 failed": digit-fuzzing hides the
        // step from the screen hash (same hash), but the changed summary
        // still restarts the quiet clock (a new failure is activity).
        let dir = std::env::temp_dir().join(format!("terhub-poll-digits-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        fake_tmux_file_screen(&dir, 999_999_999, "claude");
        let tmux = crate::tmux::Tmux::test(dir.join("tmux"));
        let manifests = builtin_compiled();
        let screen = dir.join("screen.txt");
        std::fs::write(&screen, "Error: 1 failed\n$ \n").unwrap();
        let (next, _, _, _) =
            poll_sync_with(&tmux, HashMap::new(), &manifests, Tunables::default());
        let first = next.get("%50").expect("error record").clone();
        assert_eq!(first.summary, "Error: 1 failed");
        std::thread::sleep(std::time::Duration::from_secs(2));
        std::fs::write(&screen, "Error: 2 failed\n$ \n").unwrap();
        let (next, _, _, _) = poll_sync_with(&tmux, next, &manifests, Tunables::default());
        let second = next.get("%50").expect("error record carried").clone();
        assert_eq!(second.summary, "Error: 2 failed");
        assert_eq!(
            second.screen_hash, first.screen_hash,
            "digit-only step should not change the hash"
        );
        assert!(
            second.quiet_since > first.quiet_since,
            "changed summary should restart quiet"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn spinner_frames_do_not_restart_quiet() {
        // Spinner frame + elapsed counter advance, nothing else: the
        // quiet clock keeps growing instead of pegging near zero.
        let dir = std::env::temp_dir().join(format!("terhub-poll-spinner-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        fake_tmux_file_screen(&dir, 999_999_999, "claude");
        let tmux = crate::tmux::Tmux::test(dir.join("tmux"));
        let manifests = builtin_compiled();
        let screen = dir.join("screen.txt");
        std::fs::write(&screen, "⠙ Running 13min40s\n$ \n").unwrap();
        let (next, _, _, _) =
            poll_sync_with(&tmux, HashMap::new(), &manifests, Tunables::default());
        let first = next.get("%50").expect("working record").clone();
        assert_eq!(first.state, AgentState::Working);
        std::thread::sleep(std::time::Duration::from_secs(2));
        std::fs::write(&screen, "⠋ Running 13min44s\n$ \n").unwrap();
        let (next, _, _, _) = poll_sync_with(&tmux, next, &manifests, Tunables::default());
        let second = next.get("%50").expect("working record carried").clone();
        assert_eq!(
            second.quiet_since, first.quiet_since,
            "spinner-only change should not restart quiet"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tip_rotation_does_not_restart_quiet() {
        // Footer tip carousel swaps full text every few seconds: the
        // quiet clock must ignore it like any other animation, while the
        // verdict beside it still stands.
        let dir = std::env::temp_dir().join(format!("terhub-poll-tips-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        fake_tmux_file_screen(&dir, 999_999_999, "claude");
        let tmux = crate::tmux::Tmux::test(dir.join("tmux"));
        let manifests = builtin_compiled();
        let screen = dir.join("screen.txt");
        std::fs::write(&screen, "Error: static\nTip: /context shows budget\n$ \n").unwrap();
        let (next, _, _, _) =
            poll_sync_with(&tmux, HashMap::new(), &manifests, Tunables::default());
        let first = next.get("%50").expect("error record").clone();
        assert_eq!(first.state, AgentState::Error);
        std::thread::sleep(std::time::Duration::from_secs(2));
        std::fs::write(&screen, "Error: static\nTip: /steer guides response\n$ \n").unwrap();
        let (next, _, _, _) = poll_sync_with(&tmux, next, &manifests, Tunables::default());
        let second = next.get("%50").expect("error record carried").clone();
        assert_eq!(second.summary, "Error: static");
        assert_eq!(
            second.quiet_since, first.quiet_since,
            "tip-only change should not restart quiet"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn continuity_ignores_agent_case() {
        // track_cpu matches agents case-insensitively; state continuity
        // must agree, or a "PI"→"pi" wobble resets the clock every poll.
        let now = 100_000;
        // Fresh still-clock (no stuck trip): only the clock continuity matters.
        let mut old = carried("PI", AgentState::Working, 50, now - 60, now - 600);
        old.agent_start = 100;
        let rec = merge_record_owned(
            Some(&old),
            false,
            None,
            Some(("pi", 50, 100)),
            &test_pane(),
            false,
            now,
            STUCK_SECS,
            QUIET_IDLE_SECS,
            WAITING_IDLE_SECS,
        )
        .expect("no record");
        assert_eq!(rec.state_since, now - 600);
        assert_eq!(rec.cpu_still_since, now - 60);
    }

    #[test]
    fn frozen_screen_makes_a_live_agent_idle() {
        let now = 100_000;
        // The real failure case: the process is alive and its REPL keeps
        // burning CPU, so Stuck never trips, but the screen has not moved
        // for 10 minutes — it finished. No completion text, no pattern.
        let mut old = carried("pi", AgentState::Working, 500, now - 4, now - 900);
        old.quiet_since = now - 600;
        let rec = merge_record_owned(
            Some(&old),
            false,
            None,
            Some(("pi", 500, 100)),
            &test_pane(),
            false,
            now,
            STUCK_SECS,
            QUIET_IDLE_SECS,
            WAITING_IDLE_SECS,
        )
        .expect("no record");
        assert_eq!(rec.state, AgentState::Idle, "frozen screen read as busy");
        assert!(rec.summary.is_empty(), "{:?}", rec.summary);
        assert_eq!(rec.state_since, now);
        // The whole working stretch is the completed turn.
        assert_eq!(rec.last_turn_secs, 900);
    }

    #[test]
    fn quiet_idle_survives_the_poll_loop() {
        // The merge alone is not the feature: the poller re-evaluates quiet
        // with refresh_quiet right after, and a summary the flip itself
        // invented reads as fresh activity and resets the clock the flip
        // just measured. Drive three polls to prove Idle survives.
        let now = 100_000;
        let mut old = carried("pi", AgentState::Working, 500, now - 4, now - 900);
        old.quiet_since = now - 600;
        let h = 0xABCD_1234_u64; // screen has not moved across all polls
        old.screen_hash = h;

        let rec = merge_record_owned(
            Some(&old),
            false,
            None,
            Some(("pi", 500, 100)),
            &test_pane(),
            false,
            now,
            STUCK_SECS,
            QUIET_IDLE_SECS,
            WAITING_IDLE_SECS,
        )
        .expect("no record");
        assert_eq!(rec.state, AgentState::Idle);

        // What the poller does next, verbatim.
        let (nh, nq) = refresh_quiet(Some(&old), Some(h), &rec, now);
        assert_eq!(
            nq,
            now - 600,
            "the flip's own summary reset the clock it reads"
        );

        // Poll 2: same screen, so it must still be Idle — not one-poll flicker.
        let mut carried2 = rec.clone();
        carried2.screen_hash = nh;
        carried2.quiet_since = nq;
        let rec2 = merge_record_owned(
            Some(&carried2),
            false,
            None,
            Some(("pi", 500, 100)),
            &test_pane(),
            false,
            now + 2,
            STUCK_SECS,
            QUIET_IDLE_SECS,
            WAITING_IDLE_SECS,
        )
        .expect("no record");
        assert_eq!(
            rec2.state,
            AgentState::Idle,
            "flickered back to working after one poll"
        );
        assert_eq!(rec2.state_since, now, "idle clock must not restart");

        // And it stays put on a third poll.
        let mut carried3 = rec2.clone();
        carried3.screen_hash = nh;
        carried3.quiet_since = nq;
        let rec3 = merge_record_owned(
            Some(&carried3),
            false,
            None,
            Some(("pi", 500, 100)),
            &test_pane(),
            false,
            now + 4,
            STUCK_SECS,
            QUIET_IDLE_SECS,
            WAITING_IDLE_SECS,
        )
        .expect("no record");
        assert_eq!(rec3.state, AgentState::Idle);
    }

    #[test]
    fn resumed_work_restarts_the_idle_clock() {
        // The reported bug: a pane that just started working again kept
        // showing the previous stillness in the Idle column until its
        // pixels visibly moved (spinner frames are denoised out of the
        // hash, so a busy screen can hash unchanged for minutes). The
        // state flip itself is activity: entering Working restarts the
        // visible-quiet clock the same poll.
        let now = 100_000;
        let h = 0xABCD_1234_u64; // unchanged screen, still hash
        let mut old = carried("pi", AgentState::Idle, 500, now - 900, now - 900);
        old.quiet_since = now - 900;
        old.screen_hash = h;
        // Merge alone flips on a recently-quiet screen (fresh CPU ticks
        // keep Stuck out of it) ...
        let mut fresh = old.clone();
        fresh.quiet_since = now - 2;
        let rec = merge_record_owned(
            Some(&fresh),
            false,
            None,
            Some(("pi", 900, 100)),
            &test_pane(),
            false,
            now,
            STUCK_SECS,
            QUIET_IDLE_SECS,
            WAITING_IDLE_SECS,
        )
        .expect("no record");
        assert_eq!(rec.state, AgentState::Working);
        // ... and the poller's verbatim step restarts the clock even
        // when the screen did not move this poll. (`old` still carries
        // the stale clock here on purpose: the restart must not depend
        // on the screen having moved first.)
        let (nh, nq) = refresh_quiet(Some(&old), Some(h), &rec, now);
        assert_eq!(nh, h);
        assert_eq!(nq, now, "resumed work kept showing the old stillness");
        // Steady working keeps the screen clock (no whitewash): the
        // stuck smell still accumulates on a frozen screen.
        let mut steady = rec.clone();
        steady.screen_hash = h;
        steady.quiet_since = now - 60;
        let (sh, sq) = refresh_quiet(Some(&steady), Some(h), &steady, now + 60);
        assert_eq!((sh, sq), (h, now - 60));
        // A demotion into Idle keeps the old clock — that stillness is
        // the whole reason it demoted.
        let mut working = carried("pi", AgentState::Working, 500, now - 4, now - 900);
        working.quiet_since = now - 600;
        working.screen_hash = h;
        let demoted = merge_record_owned(
            Some(&working),
            false,
            None,
            Some(("pi", 500, 100)),
            &test_pane(),
            false,
            now,
            STUCK_SECS,
            QUIET_IDLE_SECS,
            WAITING_IDLE_SECS,
        )
        .expect("no record");
        assert_eq!(demoted.state, AgentState::Idle);
        let (_, dq) = refresh_quiet(Some(&working), Some(h), &demoted, now);
        assert_eq!(dq, now - 600, "demotion must not whitewash stillness");
    }

    #[test]
    fn idle_flips_back_to_working_when_the_screen_moves() {
        let now = 100_000;
        let mut old = carried("pi", AgentState::Idle, 500, now - 900, now - 900);
        old.quiet_since = now - 900;
        // quiet_since is one poll stale, so a resumed screen shows up here
        // as "recently quiet". Fresh CPU ticks keep Stuck out of it.
        let mut fresh = old.clone();
        fresh.quiet_since = now - 2;
        let rec = merge_record_owned(
            Some(&fresh),
            false,
            None,
            Some(("pi", 900, 100)),
            &test_pane(),
            false,
            now,
            STUCK_SECS,
            QUIET_IDLE_SECS,
            WAITING_IDLE_SECS,
        )
        .expect("no record");
        assert_eq!(rec.state, AgentState::Working);
        assert_eq!(rec.state_since, now);
    }

    #[test]
    fn quiet_idle_below_threshold_stays_working() {
        let now = 100_000;
        let mut old = carried("pi", AgentState::Working, 500, now - 4, now - 900);
        old.quiet_since = now - 60;
        let rec = merge_record_owned(
            Some(&old),
            false,
            None,
            Some(("pi", 500, 100)),
            &test_pane(),
            false,
            now,
            STUCK_SECS,
            QUIET_IDLE_SECS,
            WAITING_IDLE_SECS,
        )
        .expect("no record");
        assert_eq!(
            rec.state,
            AgentState::Working,
            "silent tool run read as idle"
        );
    }

    #[test]
    fn quiet_idle_zero_disables_the_flip() {
        let now = 100_000;
        let mut old = carried("pi", AgentState::Working, 500, now - 4, now - 900);
        old.quiet_since = now - 100_000;
        let rec = merge_record_owned(
            Some(&old),
            false,
            None,
            Some(("pi", 500, 100)),
            &test_pane(),
            false,
            now,
            STUCK_SECS,
            0,
            0,
        )
        .expect("no record");
        assert_eq!(rec.state, AgentState::Working, "0 must disable the flip");
    }

    #[test]
    fn long_untouched_waiting_prompt_becomes_idle() {
        let now = 100_000;
        // gemini's finished turn: the screen ends on a bare `>` that the
        // classifier reads as waiting, and nobody has touched it for 40
        // minutes. It must stop demanding attention.
        let mut old = carried("gemini", AgentState::Waiting, 500, now - 4, now - 5_000);
        old.quiet_since = now - 2_400;
        let rec = merge_record_owned(
            Some(&old),
            false,
            screen_hit(Some("gemini"), AgentState::Waiting, "1 artifact to review"),
            Some(("gemini", 500, 100)),
            &test_pane(),
            false,
            now,
            STUCK_SECS,
            QUIET_IDLE_SECS,
            WAITING_IDLE_SECS,
        )
        .expect("no record");
        assert_eq!(rec.state, AgentState::Idle, "stale prompt still nagging");
        // The question survives: that is what you came back to read.
        assert!(rec.summary.contains("artifact"), "{:?}", rec.summary);
    }

    #[test]
    fn fresh_waiting_prompt_keeps_flagging() {
        let now = 100_000;
        // Stepping away for a few minutes must not silence a question.
        let mut old = carried("gemini", AgentState::Waiting, 500, now - 4, now - 200);
        old.quiet_since = now - 300;
        let rec = merge_record_owned(
            Some(&old),
            false,
            screen_hit(Some("gemini"), AgentState::Waiting, "approve?"),
            Some(("gemini", 500, 100)),
            &test_pane(),
            false,
            now,
            STUCK_SECS,
            QUIET_IDLE_SECS,
            WAITING_IDLE_SECS,
        )
        .expect("no record");
        assert_eq!(rec.state, AgentState::Waiting);
        assert!(rec.state.needs_attention());
    }

    #[test]
    fn waiting_idle_zero_keeps_every_prompt_flagged() {
        let now = 100_000;
        let mut old = carried("gemini", AgentState::Waiting, 500, now - 4, now - 5_000);
        old.quiet_since = now - 100_000;
        let rec = merge_record_owned(
            Some(&old),
            false,
            screen_hit(Some("gemini"), AgentState::Waiting, "approve?"),
            Some(("gemini", 500, 100)),
            &test_pane(),
            false,
            now,
            STUCK_SECS,
            QUIET_IDLE_SECS,
            0,
        )
        .expect("no record");
        assert_eq!(
            rec.state,
            AgentState::Waiting,
            "0 must disable the demotion"
        );
    }

    #[test]
    fn finished_quiet_pane_reads_idle_not_stuck() {
        // The `samai` report: a pane whose screen has been quiet for
        // longer than `quiet_idle_secs` (300s) and whose process has
        // not burned CPU for longer than `stuck_secs` (600s). The
        // agent's REPL is still alive, so `still` keeps counting —
        // but the long-quiet screen is the stronger signal: the pane
        // is *finished*, not hung. The Stuck gate's quiescence guard
        // declines to fire, and the promotion below lands it on
        // `Idle`. Without that guard, a finished pane reads `stuck`
        // forever — the misclassification the user saw on `samai`.
        let now = 100_000;
        let mut old = carried("pi", AgentState::Working, 500, now - 900, now - 900);
        old.quiet_since = now - 900;
        let rec = merge_record_owned(
            Some(&old),
            false,
            None,
            Some(("pi", 500, 100)),
            &test_pane(),
            false,
            now,
            STUCK_SECS,
            QUIET_IDLE_SECS,
            WAITING_IDLE_SECS,
        )
        .expect("no record");
        assert_eq!(
            rec.state,
            AgentState::Idle,
            "a finished-and-quiet pane must read idle, not stuck: {rec:?}"
        );
    }

    #[test]
    fn recently_frozen_pane_reads_stuck() {
        // The genuine hang the Stuck gate exists for: the spinner
        // stopped *recently* (screen froze 30s ago — well inside
        // `quiet_idle_secs`), and the process has not burned CPU for
        // past `stuck_secs`. No quiescence promotion applies (the
        // screen was not long-quiet), so Stuck wins. This is the
        // case the old `stuck_outranks_quiescent_idle` described, but
        // with the screen freeze recent: a long-frozen screen is
        // finished, not hung.
        let now = 100_000;
        let mut old = carried("pi", AgentState::Working, 500, now - 900, now - 900);
        old.quiet_since = now - 30;
        let rec = merge_record_owned(
            Some(&old),
            false,
            None,
            Some(("pi", 500, 100)),
            &test_pane(),
            false,
            now,
            STUCK_SECS,
            QUIET_IDLE_SECS,
            WAITING_IDLE_SECS,
        )
        .expect("no record");
        assert_eq!(
            rec.state,
            AgentState::Stuck,
            "a recently-frozen pane is a real hang: {rec:?}"
        );
        // A pane already on Idle that froze *recently* still escalates:
        // a finished-looking row whose spinner just died is a hang
        // that finished, not a clean finish.
        let mut idle = carried("pi", AgentState::Idle, 500, now - 900, now - 900);
        idle.quiet_since = now - 30;
        let rec = merge_record_owned(
            Some(&idle),
            false,
            None,
            Some(("pi", 500, 100)),
            &test_pane(),
            false,
            now,
            STUCK_SECS,
            QUIET_IDLE_SECS,
            WAITING_IDLE_SECS,
        )
        .expect("no record");
        assert_eq!(
            rec.state,
            AgentState::Stuck,
            "idle masked a recent hang: {rec:?}"
        );
        // … but one that has been frozen for a long time stays Idle:
        // that is a finished pane, not a hang (the samai case).
        let mut idle_long = carried("pi", AgentState::Idle, 500, now - 900, now - 900);
        idle_long.quiet_since = now - 900;
        let rec = merge_record_owned(
            Some(&idle_long),
            false,
            None,
            Some(("pi", 500, 100)),
            &test_pane(),
            false,
            now,
            STUCK_SECS,
            QUIET_IDLE_SECS,
            WAITING_IDLE_SECS,
        )
        .expect("no record");
        assert_eq!(
            rec.state,
            AgentState::Idle,
            "long-quiet idle must stay idle: {rec:?}"
        );
    }

    #[test]
    fn runtime_dir_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("terhub-dirperms-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Simulate a pre-existing umask-permissive dir.
        let mut p = std::fs::metadata(&dir).unwrap().permissions();
        p.set_mode(0o755);
        std::fs::set_permissions(&dir, p).unwrap();
        ensure_runtime_dir(&dir).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn runtime_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("terhub-fileperms-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Socket: simulate a umask-permissive bind, then tighten.
        let sock = dir.join("terhubd.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let mut p = std::fs::metadata(&sock).unwrap().permissions();
        p.set_mode(0o777);
        std::fs::set_permissions(&sock, p).unwrap();
        set_owner_only(&sock).unwrap();
        let mode = std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "socket must be owner-only");
        drop(listener);
        // Pid file: atomic write must land 0600 straight away.
        let pid_file = dir.join("terhubd.pid");
        write_pid_file(&pid_file, "12345").unwrap();
        let mode = std::fs::metadata(&pid_file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "pid file must be owner-only");
        assert_eq!(std::fs::read_to_string(&pid_file).unwrap(), "12345");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn runtime_symlink_swap_detected() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("terhub-symlink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let real = base.join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = base.join("rt");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        // Symlink at check time is refused outright.
        assert!(ensure_runtime_dir(&link).is_err());
        // A checked dir verifies clean ...
        let guard = ensure_runtime_dir(&real).unwrap();
        guard.verify().unwrap();
        // ... but swapping the guarded path for a symlink afterwards
        // is detected: rename dir away, link the name at an attacker dir.
        let moved = base.join("real-moved");
        std::fs::rename(&real, &moved).unwrap();
        let evil = base.join("evil");
        std::fs::create_dir_all(&evil).unwrap();
        std::os::unix::fs::symlink(&evil, &real).unwrap();
        assert!(guard.verify().is_err(), "dir->symlink swap went undetected");
        // Same for a plain dir replacement (rename swap).
        std::fs::remove_file(&real).unwrap();
        std::fs::create_dir_all(&real).unwrap();
        let mut p = std::fs::metadata(&real).unwrap().permissions();
        p.set_mode(0o700);
        std::fs::set_permissions(&real, p).unwrap();
        assert!(guard.verify().is_err(), "dir replacement went undetected");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn path_daemon_bin_must_be_safe_to_exec() {
        use std::os::unix::fs::PermissionsExt;
        let base =
            std::env::temp_dir().join(format!("terhub-pathbin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let good_dir = base.join("good");
        let evil_dir = base.join("evil");
        std::fs::create_dir_all(&good_dir).unwrap();
        std::fs::create_dir_all(&evil_dir).unwrap();
        // Evil dir first in PATH: a world-writable `terhubd` there must
        // be skipped, and the good install later in PATH must win.
        let evil_bin = evil_dir.join("terhubd");
        std::fs::write(&evil_bin, "#!/bin/sh\nexit 1\n").unwrap();
        let mut p = std::fs::metadata(&evil_bin).unwrap().permissions();
        p.set_mode(0o777);
        std::fs::set_permissions(&evil_bin, p).unwrap();
        let good_bin = good_dir.join("terhubd");
        std::fs::write(&good_bin, "#!/bin/sh\nexit 0\n").unwrap();
        let mut p = std::fs::metadata(&good_bin).unwrap().permissions();
        p.set_mode(0o755);
        std::fs::set_permissions(&good_bin, p).unwrap();
        let path_var = std::ffi::OsString::from(format!(
            "{}:{}",
            evil_dir.display(),
            good_dir.display()
        ));
        let found = find_in_path("terhubd", &path_var).unwrap();
        assert_eq!(found, good_bin);
        // A non-regular file (directory) is not executable-as-daemon.
        let dir_only = base.join("dironly");
        std::fs::create_dir_all(dir_only.join("terhubd")).unwrap();
        let path_var = std::ffi::OsString::from(dir_only.join("terhubd").parent().unwrap());
        assert!(find_in_path("terhubd", &path_var).is_err());
        // Nothing at all: fail closed, no bare-name fallback.
        let empty = base.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let path_var = std::ffi::OsString::from(&empty);
        let err = find_in_path("terhubd", &path_var).unwrap_err();
        assert!(err.to_string().contains("no `terhubd` found in PATH"));
        // Only an unsafe candidate: rejected, not returned.
        let path_var = std::ffi::OsString::from(&evil_dir);
        let err = find_in_path("terhubd", &path_var).unwrap_err();
        assert!(err.to_string().contains("unsafe candidate(s) rejected"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn daemon_lock_excludes_second_holder() {
        // Two daemons started together must not both pass: the second
        // flock attempt fails while the first holder lives.
        //
        // Held under FORK_GUARD so no sibling forks a child while this
        // test holds the lock. flock is per open-file-description: a
        // forked child *duplicates* the OFD, so a child forked while a
        // lock is held keeps that lock alive past the parent's drop() —
        // O_CLOEXEC closes the duplicate only at execve, which leaves a
        // fork→execve window a fast script child still occupies.
        //
        // "Released on drop" is deliberately NOT asserted here: no
        // in-process assertion can distinguish "the parent dropped the
        // lock" from "some sibling's child is between fork and execve".
        // That arm lives in tests/daemon_lock.rs, its own test binary
        // where no sibling forks anything.
        let _guard = fork_guard();
        let dir = std::env::temp_dir().join(format!("terhub-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let first = acquire_daemon_lock(&dir).expect("first lock");
        assert!(
            acquire_daemon_lock(&dir).is_err(),
            "second daemon locked too"
        );
        drop(first);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn oversize_request_rejected_without_oom() {
        // A 2MB (valid!) hook event exceeds the 1MB request cap: truncated,
        // rejected, never applied — instead of read whole into memory.
        let registry = Arc::new(Mutex::new(HashMap::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let dir = std::env::temp_dir().join(format!("terhub-bigreq-{}", std::process::id()));
        let (server, client) = tokio::net::UnixStream::pair().unwrap();
        let reg = registry.clone();
        let rt = dir.clone();
        let guard = ensure_runtime_dir(&dir).unwrap();
        let handle =
            tokio::spawn(async move { handle_conn(server, reg, rt, guard, shutdown).await });
        let big = serde_json::to_string(&Request::Event {
            event: crate::agent::HookEvent {
                pane: "%1".to_string(),
                agent: "pi".to_string(),
                state: AgentState::Waiting,
                summary: "s".repeat(2 * 1024 * 1024),
                prompt: String::new(),
                model: String::new(),
                ctx: None,
            },
        })
        .unwrap();
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let (rd, mut wr) = client.into_split();
        // The write may stall once the server stops draining (that's the
        // cap working); abort it after the verdict instead of joining.
        let write_task = tokio::spawn(async move {
            let _ = wr.write_all(big.as_bytes()).await;
            let _ = wr.write_all(b"\n").await;
        });
        let mut lines = tokio::io::BufReader::new(rd).lines();
        let resp: Response = serde_json::from_str(
            &tokio::time::timeout(Duration::from_secs(5), lines.next_line())
                .await
                .expect("no response")
                .expect("read failed")
                .expect("empty response"),
        )
        .unwrap();
        write_task.abort();
        assert!(!resp.ok, "oversize request accepted");
        assert!(
            lock_registry(&registry).is_empty(),
            "truncated event applied"
        );
        handle.await.unwrap();
    }

    #[test]
    fn capped_line_reader_refuses_flood() {
        use std::io::Cursor;
        let mut big = vec![b'x'; 100];
        big.push(b'\n');
        assert!(read_capped_line(&mut Cursor::new(big), 16).is_err());
        let mut ok = Cursor::new(b"{\"ok\":true}\n".to_vec());
        assert_eq!(read_capped_line(&mut ok, 16).unwrap(), "{\"ok\":true}\n");
        // Truncated (no newline before EOF within the cap) is rejected
        // rather than returned as a parseable partial line.
        let mut truncated = Cursor::new(b"{\"ok\":true".to_vec());
        assert!(read_capped_line(&mut truncated, 16).is_err());
        // A line whose content is exactly `cap` bytes is rejected
        // (the trailing newline would push it over the cap).
        let mut exact = vec![b'x'; 16];
        exact.push(b'\n');
        assert!(read_capped_line(&mut Cursor::new(exact), 16).is_err());
    }

    fn hook_record(updated_at: u64) -> AgentRecord {
        AgentRecord {
            pane: "%1".to_string(),
            agent: "claude".to_string(),
            state: AgentState::Waiting,
            source: Source::Hook,
            summary: String::new(),
            session: "s".to_string(),
            window: "w".to_string(),
            command: "claude".to_string(),
            updated_at,
            state_since: updated_at,
            cpu_ticks: 0,
            cpu_still_since: 0,
            agent_start: 0,
            last_turn_secs: 0,
            screen_hash: 0,
            quiet_since: 0,
            last_prompt_at: 0,
            last_prompt: String::new(),
            handle: String::new(),
            model: String::new(),
            ctx_pct: None,
            reg_pid: 0,
            bell: false,
            bell_reason: String::new(),
            stuck_base: None,
        }
    }

    #[test]
    fn denoise_screen_ignores_spinners_and_counters() {
        // minimax spinner frame + elapsed counter: identical when clean.
        let a = "⠙ Running 13min40s · ⚡ 15092 tok/s\n$ ";
        let b = "⠋ Running 13min44s · ⚡ 15092 tok/s\n$ ";
        assert_eq!(denoise_screen(a), denoise_screen(b));
        assert_eq!(screen_hash(a), screen_hash(b));
        // pi block-meter animation + cost/relative-time creep: identical.
        let c = "[█▆▄▂▁▂ LIVE] · $15.443 · 5s ago\n$ ";
        let d = "[▆█▆▄▂▁ LIVE] · $15.444 · 0s ago\n$ ";
        assert_eq!(denoise_screen(c), denoise_screen(d));
        // Digit-only semantic steps hide too (the summary comparison in
        // track_quiet_merged catches the classifier-visible ones).
        assert_eq!(denoise_screen("1 failed"), denoise_screen("2 failed"));
        // Real changes still differ: new text, new lines, error lines.
        assert_ne!(denoise_screen("ok"), denoise_screen("Error: boom"));
        assert_ne!(denoise_screen("$ "), denoise_screen("done\n$ "));
        assert_ne!(denoise_screen("pass"), denoise_screen("pass\nfail"));
    }

    #[test]
    fn track_quiet_merged_matrix() {
        // Same screen + same summary: the clock grows.
        assert_eq!(
            track_quiet_merged((7, 100), Some(7), "e1", "e1", AgentState::Error, 200),
            (7, 100)
        );
        // Same screen + changed summary: a new failure is activity.
        assert_eq!(
            track_quiet_merged(
                (7, 100),
                Some(7),
                "1 failed",
                "2 failed",
                AgentState::Error,
                200
            ),
            (7, 200)
        );
        // Changed screen: resets regardless of summaries.
        assert_eq!(
            track_quiet_merged((7, 100), Some(9), "e1", "e1", AgentState::Error, 200),
            (9, 200)
        );
        // A wiped summary is decay, not activity.
        assert_eq!(
            track_quiet_merged((7, 100), Some(7), "e1", "", AgentState::Error, 200),
            (7, 100)
        );
        // Stuck rows are exempt: their "no cpu for {dur}" ticker must not
        // read fresh forever.
        assert_eq!(
            track_quiet_merged(
                (7, 100),
                Some(7),
                "no cpu for 10m",
                "no cpu for 11m",
                AgentState::Stuck,
                200
            ),
            (7, 100)
        );
    }

    #[test]
    fn quiet_clock_keeps_still_restarts_change() {
        let h = screen_hash("same screen");
        // Still screen keeps the old still-time.
        assert_eq!(track_quiet((h, 100), Some(h), 200), (h, 100));
        // Changed screen restarts the clock.
        let h2 = screen_hash("new output");
        assert_ne!(h, h2);
        assert_eq!(track_quiet((h, 100), Some(h2), 200), (h2, 200));
        // Failed capture preserves the clock, never resets it.
        assert_eq!(track_quiet((h, 100), None, 200), (h, 100));
        // First sighting starts the clock now.
        assert_eq!(track_quiet((0, 0), Some(h), 200), (h, 200));
    }

    #[test]
    fn fresh_hook_stays_authoritative() {
        let now = 10_000;
        let rec = hook_record(now - 60);
        let (old, stale) = split_prev(Some(&rec), now, HOOK_STALE_SECS);
        assert!(old.is_some());
        assert!(stale.is_none());
    }

    #[test]
    fn lapsed_hook_expires_but_seeds_continuity() {
        let now = 10_000;
        let rec = hook_record(now - HOOK_STALE_SECS - 1);
        let (old, stale) = split_prev(Some(&rec), now, HOOK_STALE_SECS);
        assert!(old.is_none());
        assert!(stale.is_some());
    }

    #[test]
    fn screen_records_pass_through() {
        let now = 10_000;
        let mut rec = hook_record(0);
        rec.source = Source::Screen;
        let (old, stale) = split_prev(Some(&rec), now, HOOK_STALE_SECS);
        assert!(old.is_some());
        assert!(stale.is_none());
    }

    #[test]
    fn mid_poll_hooks_survive_the_write_back() {
        let poll_start = 50_000;
        // The poll classified %1 from its stale pre-poll clone...
        let mut next = HashMap::new();
        next.insert(
            "%1".to_string(),
            carried("claude", AgentState::Working, 0, 0, poll_start - 100),
        );
        // ...but a hook event landed mid-poll (live registry only).
        let mut live = HashMap::new();
        let mut fresh = hook_record(poll_start + 1);
        fresh.summary = "Approve plan?".to_string();
        live.insert("%1".to_string(), fresh);
        // An older hook the poll already saw must not be resurrected.
        let mut old_hook = hook_record(poll_start - 10);
        old_hook.pane = "%2".to_string();
        live.insert("%2".to_string(), old_hook);

        merge_mid_poll_hooks(&mut next, &live, poll_start);

        assert_eq!(next["%1"].state, AgentState::Waiting);
        assert_eq!(next["%1"].summary, "Approve plan?");
        assert!(!next.contains_key("%2"));
    }

    #[test]
    fn fallback_summary_takes_freshest_line() {
        assert_eq!(screen_fallback_summary(Some("old\nfresh  \n  \n")), "fresh");
        assert_eq!(screen_fallback_summary(None), "");
        assert_eq!(screen_fallback_summary(Some("  \n\t\n")), "");
        // Long lines truncate with an ellipsis, control chars wash out.
        let long = "x".repeat(100);
        let out = screen_fallback_summary(Some(&format!("a\tb\n{long}")));
        assert_eq!(out.chars().count(), 81);
        assert!(out.ends_with('…'), "{out}");
        assert_eq!(screen_fallback_summary(Some("a\tb")), "a b");
    }

    #[test]
    fn fallback_summary_skips_working_rows() {
        use crate::agent::AgentState::{Done, Error, Limited, Stuck, Waiting, Working};
        let screen = Some("● Chrome Bridge (indefinite)");
        // Done rows show what finished, like Idle rows.
        assert_eq!(fallback_summary_for(Done, screen), "● Chrome Bridge (indefinite)");
        // Working rows stay summary-free: spinner text is noise.
        assert_eq!(fallback_summary_for(Working, screen), "");
        // Stuck carries its own explanation from merge_record.
        assert_eq!(fallback_summary_for(Stuck, screen), "");
        // Actionable states borrow the freshest screen line.
        assert_eq!(
            fallback_summary_for(Waiting, screen),
            "● Chrome Bridge (indefinite)"
        );
        assert!(!fallback_summary_for(Error, screen).is_empty());
        assert!(!fallback_summary_for(Limited, screen).is_empty());
        assert_eq!(fallback_summary_for(Waiting, None), "");
    }

    fn carried(agent: &str, state: AgentState, ticks: u64, still: u64, since: u64) -> AgentRecord {
        AgentRecord {
            pane: "%1".to_string(),
            agent: agent.to_string(),
            state,
            source: Source::Proc,
            summary: String::new(),
            session: "a".to_string(),
            window: "w".to_string(),
            command: "pi".to_string(),
            updated_at: since,
            state_since: since,
            cpu_ticks: ticks,
            cpu_still_since: still,
            agent_start: 0,
            last_turn_secs: 0,
            screen_hash: 0,
            quiet_since: 0,
            last_prompt_at: 0,
            last_prompt: String::new(),
            handle: String::new(),
            model: String::new(),
            ctx_pct: None,
            reg_pid: 0,
            bell: false,
            bell_reason: String::new(),
            stuck_base: None,
        }
    }

    fn test_pane() -> crate::tmux::Pane {
        crate::tmux::Pane::test("%1", "w", "pi")
    }

    fn screen_hit(agent: Option<&str>, state: AgentState, summary: &str) -> Option<ScreenHit> {
        Some(ScreenHit {
            agent: agent.map(str::to_string),
            state,
            summary: summary.to_string(),
        })
    }

    #[test]
    fn idle_verdict_keeps_its_completion_line() {
        // Done rows show what finished ("✓ Todo list…"), like waiting
        // rows show the plea — not a blank. And a done screen beats a
        // live process: finished agents keep their REPL open.
        for proc in [None, Some(("minimax-code", 10))] {
            let rec = merge_record(
                None,
                false,
                screen_hit(Some("minimax-code"), AgentState::Idle, "✓ Todo list 9/9"),
                proc,
                &test_pane(),
                100,
                300,
            )
            .expect("screen verdict claims the pane");
            assert_eq!(rec.state, AgentState::Idle, "proc: {proc:?}");
            assert_eq!(rec.summary, "✓ Todo list 9/9");
        }
    }

    #[test]
    fn done_verdict_beats_live_process() {
        // A finished agent with a live REPL must read Done, not Working:
        // the Done screen verdict promotes over the process hit, and the
        // completion line rides along like Idle's.
        for proc in [None, Some(("pi", 10))] {
            let rec = merge_record(
                None,
                false,
                screen_hit(Some("pi"), AgentState::Done, "✓ Todo list 9/9"),
                proc,
                &test_pane(),
                100,
                300,
            )
            .expect("screen verdict claims the pane");
            assert_eq!(rec.state, AgentState::Done, "proc: {proc:?}");
            assert_eq!(rec.summary, "✓ Todo list 9/9");
        }
    }

    #[test]
    fn working_to_done_records_turn_time() {
        // turn_ended covers Done: a finished turn banks its duration
        // instead of recording last_turn_secs = 0.
        assert!(crate::agent::turn_ended(
            AgentState::Working,
            AgentState::Done
        ));
        let now = 100_000;
        let old = carried("pi", AgentState::Working, 50, now - 10, now - 120);
        let rec = merge_record(
            Some(&old),
            false,
            screen_hit(Some("pi"), AgentState::Done, "✓ done"),
            Some(("pi", 60)),
            &test_pane(),
            now,
            300,
        )
        .expect("done merge");
        assert_eq!(rec.state, AgentState::Done);
        assert_eq!(rec.last_turn_secs, 120);
    }

    #[test]
    fn harness_screen_records_drop_as_ghosts() {
        let now = 100_000;
        let mut old = carried("claude", AgentState::Working, 0, 0, now - 60);
        old.source = Source::Screen;
        let mut harness = test_pane();
        harness.command = "muxa".to_string();
        assert!(!carry_record(&old, &harness, now, HOOK_STALE_SECS));
        // Same record on a normal pane carries while fresh …
        assert!(carry_record(&old, &test_pane(), now, HOOK_STALE_SECS));
        // … and drops once stale.
        assert!(!carry_record(
            &old,
            &test_pane(),
            now + HOOK_STALE_SECS + 1,
            HOOK_STALE_SECS
        ));
        // Proc records on harnesses still carry (the agent may return).
        let mut proc_old = carried("claude", AgentState::Working, 500, now - 60, now - 60);
        proc_old.source = Source::Proc;
        assert!(carry_record(&proc_old, &harness, now, HOOK_STALE_SECS));
    }

    #[test]
    fn proc_identity_beats_screen_guess() {
        // Screen says claude, process tree says pi: trust the tree.
        let now = 100_000;
        let rec = merge_record(
            None,
            false,
            screen_hit(Some("claude"), AgentState::Working, "⠋ Working…"),
            Some(("pi", 100)),
            &test_pane(),
            now,
            STUCK_SECS,
        )
        .expect("no record");
        assert_eq!(rec.agent, "pi");
        assert_eq!(rec.state, AgentState::Working);
        assert_eq!(rec.source, Source::Proc);
        assert_eq!((rec.cpu_ticks, rec.cpu_still_since), (100, now));
        // Working rows stay summary-free: spinner text is noise.
        assert!(rec.summary.is_empty());
    }

    #[test]
    fn screen_waiting_wins_state_proc_keeps_identity() {
        let now = 100_000;
        let rec = merge_record(
            None,
            false,
            screen_hit(Some("claude"), AgentState::Waiting, "Approve? [y/n]"),
            Some(("pi", 50)),
            &test_pane(),
            now,
            STUCK_SECS,
        )
        .expect("no record");
        assert_eq!(rec.agent, "pi");
        assert_eq!(rec.state, AgentState::Waiting);
        assert_eq!(rec.source, Source::Proc);
        assert_eq!(rec.summary, "Approve? [y/n]");
    }

    #[test]
    fn screen_limited_wins_state_proc_keeps_identity() {
        let now = 100_000;
        let rec = merge_record(
            None,
            false,
            screen_hit(
                Some("codex"),
                AgentState::Limited,
                "Error: rate limit reached",
            ),
            Some(("pi", 50)),
            &test_pane(),
            now,
            STUCK_SECS,
        )
        .expect("no record");
        assert_eq!(rec.agent, "pi");
        assert_eq!(rec.state, AgentState::Limited);
        assert_eq!(rec.source, Source::Proc);
        assert_eq!(rec.summary, "Error: rate limit reached");
        assert!(rec.state.needs_attention());
    }

    #[test]
    fn proc_without_screen_is_working() {
        // The %192 case: agent binary alive, screen unreadable.
        let now = 100_000;
        let rec = merge_record(
            None,
            false,
            None,
            Some(("pi", 50)),
            &test_pane(),
            now,
            STUCK_SECS,
        )
        .expect("no record");
        assert_eq!(rec.agent, "pi");
        assert_eq!(rec.state, AgentState::Working);
        assert_eq!(rec.source, Source::Proc);
    }

    #[test]
    fn screen_only_record_keeps_screen_source() {
        let now = 100_000;
        let rec = merge_record(
            None,
            false,
            screen_hit(
                Some("codex"),
                AgentState::Error,
                "Error: rate limit reached",
            ),
            None,
            &test_pane(),
            now,
            STUCK_SECS,
        )
        .expect("no record");
        assert_eq!(rec.agent, "codex");
        assert_eq!(rec.state, AgentState::Error);
        assert_eq!(rec.source, Source::Screen);
        assert_eq!((rec.cpu_ticks, rec.cpu_still_since), (0, 0));
        assert_eq!(rec.summary, "Error: rate limit reached");
    }

    #[test]
    fn nothing_claiming_returns_none() {
        assert!(merge_record(None, false, None, None, &test_pane(), 100_000, STUCK_SECS).is_none());
    }

    #[test]
    fn anonymous_waiting_pairs_with_proc_or_drops() {
        let now = 100_000;
        // Approve prompt, no name, live pi: that agent waiting.
        let rec = merge_record(
            None,
            false,
            screen_hit(None, AgentState::Waiting, "Save changes? confirm"),
            Some(("pi", 50)),
            &test_pane(),
            now,
            STUCK_SECS,
        )
        .expect("no record");
        assert_eq!(rec.agent, "pi");
        assert_eq!(rec.state, AgentState::Waiting);
        assert_eq!(rec.source, Source::Proc);
        // Same screen, no process: a micro bar impersonating nobody.
        assert!(
            merge_record(
                None,
                false,
                screen_hit(None, AgentState::Waiting, "Save changes? confirm"),
                None,
                &test_pane(),
                now,
                STUCK_SECS,
            )
            .is_none()
        );
    }

    #[test]
    fn editor_foreground_with_agent_means_waiting() {
        let now = 100_000;
        // pi summoned $EDITOR (micro foreground): awaiting input, not working.
        let mut ed = test_pane();
        ed.command = "micro".to_string();
        let rec = merge_record(None, false, None, Some(("pi", 50)), &ed, now, STUCK_SECS)
            .expect("no record");
        assert_eq!(rec.agent, "pi");
        assert_eq!(rec.state, AgentState::Waiting);
        assert_eq!(rec.source, Source::Proc);
        // Same editor, no agent anywhere: not our business.
        assert!(merge_record(None, false, None, None, &ed, now, STUCK_SECS).is_none());
        // Same agent, non-editor foreground: still working.
        let rec = merge_record(
            None,
            false,
            None,
            Some(("pi", 50)),
            &test_pane(),
            now,
            STUCK_SECS,
        )
        .expect("no record");
        assert_eq!(rec.state, AgentState::Working);
        // An error on screen still wins over the editor foreground.
        let rec = merge_record(
            None,
            false,
            screen_hit(None, AgentState::Error, "boom"),
            Some(("pi", 50)),
            &ed,
            now,
            STUCK_SECS,
        )
        .expect("no record");
        assert_eq!(rec.state, AgentState::Error);
    }

    #[test]
    fn sibling_editor_does_not_wait() {
        // Background pi plus a sibling micro: the editor is not pi's
        // $EDITOR, so pi stays on its own verdict path (Working here).
        let now = 100_000;
        let mut ed = test_pane();
        ed.command = "micro".to_string();
        let rec = merge_record_owned(
            None,
            false,
            None,
            Some(("pi", 50, 100)),
            &ed,
            false,
            now,
            STUCK_SECS,
            QUIET_IDLE_SECS,
            WAITING_IDLE_SECS,
        )
        .expect("no record");
        assert_eq!(rec.agent, "pi");
        assert_eq!(rec.state, AgentState::Working);
        assert_eq!(rec.source, Source::Proc);
    }

    #[test]
    fn owned_editor_still_waits() {
        // micro inside pi's own subtree: pi summoned $EDITOR, awaiting input.
        let now = 100_000;
        let mut ed = test_pane();
        ed.command = "micro".to_string();
        let rec = merge_record_owned(
            None,
            false,
            None,
            Some(("pi", 50, 100)),
            &ed,
            true,
            now,
            STUCK_SECS,
            QUIET_IDLE_SECS,
            WAITING_IDLE_SECS,
        )
        .expect("no record");
        assert_eq!(rec.agent, "pi");
        assert_eq!(rec.state, AgentState::Waiting);
        assert_eq!(rec.source, Source::Proc);
    }

    #[test]
    fn pid_reuse_restarts_still_clock() {
        // Same agent name, same tick sum — but a different starttime, so the
        // pid was recycled: the new process must not inherit the old clock.
        let now = 100_000;
        let mut old = carried("pi", AgentState::Working, 50, now - 600, now - 600);
        old.agent_start = 100;
        let rec = merge_record_owned(
            Some(&old),
            false,
            None,
            Some(("pi", 50, 200)),
            &test_pane(),
            false,
            now,
            STUCK_SECS,
            QUIET_IDLE_SECS,
            WAITING_IDLE_SECS,
        )
        .expect("no record");
        assert_eq!(rec.cpu_ticks, 50);
        assert_eq!(rec.cpu_still_since, now);
        assert_eq!(rec.agent_start, 200);
    }

    #[test]
    fn same_process_keeps_still_clock() {
        // Same agent, same ticks, same starttime: one continuing process.
        let now = 100_000;
        let mut old = carried("pi", AgentState::Working, 50, now - 600, now - 600);
        old.agent_start = 100;
        let rec = merge_record_owned(
            Some(&old),
            false,
            None,
            Some(("pi", 50, 100)),
            &test_pane(),
            false,
            now,
            STUCK_SECS,
            QUIET_IDLE_SECS,
            WAITING_IDLE_SECS,
        )
        .expect("no record");
        assert_eq!(rec.cpu_ticks, 50);
        assert_eq!(rec.cpu_still_since, now - 600);
        assert_eq!(rec.agent_start, 100);
    }

    #[test]
    fn registry_round_trip_is_atomic_and_tolerant() {
        let dir = std::env::temp_dir().join(format!("terhub-reg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("registry.json");
        // One corrupt record must not sink the good one.
        let good = carried("pi", AgentState::Working, 1, 2, 100);
        let text = format!(
            "[{}, {{\"pane\":42}}]",
            serde_json::to_string(&good).unwrap()
        );
        std::fs::write(&path, &text).unwrap();
        let loaded = load_registry(&path);
        assert_eq!(loaded.len(), 1);
        assert!(loaded.contains_key(&good.pane));
        // Atomic save round-trips and leaves no tmp litter.
        let mut reg = HashMap::new();
        reg.insert(good.pane.clone(), good);
        save_registry(&path, &reg);
        assert_eq!(load_registry(&path).len(), 1);
        assert!(!path.with_extension("json.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn working_carry_drops_stale_summary() {
        let now = 100_000;
        // A prompt line borrowed while waiting is rot once the agent
        // works again — and pre-gate summaries must not ride the carry.
        let mut old = carried("pi", AgentState::Working, 50, now - 10, now - 100);
        old.summary = "● Chrome Bridge (indefinite)".to_string();
        let rec = merge_record(
            Some(&old),
            false,
            None,
            Some(("pi", 60)),
            &test_pane(),
            now,
            STUCK_SECS,
        )
        .expect("no record");
        assert_eq!(rec.state, AgentState::Working);
        assert!(rec.summary.is_empty(), "carried rot: {}", rec.summary);
    }

    #[test]
    fn waiting_summary_tracks_prompt_text() {
        // Same waiting state, new prompt line: the clock stays (no
        // renotify) but the summary follows the fresh line.
        let now = 100_000;
        let mut old = carried("pi", AgentState::Waiting, 500, now - 60, now - 300);
        old.summary = "Approve? [y/n]".to_string();
        let rec = merge_record(
            Some(&old),
            false,
            screen_hit(Some("pi"), AgentState::Waiting, "❯ Try asking me instead"),
            Some(("pi", 500)),
            &test_pane(),
            now,
            STUCK_SECS,
        )
        .expect("no record");
        assert_eq!(rec.state, AgentState::Waiting);
        assert_eq!(rec.state_since, now - 300);
        assert_eq!(rec.summary, "❯ Try asking me instead");
    }

    #[test]
    fn completed_turn_records_duration() {
        let now = 100_000;
        let old = carried("pi", AgentState::Working, 500, now - 60, now - 300);
        let rec = merge_record(
            Some(&old),
            false,
            screen_hit(Some("pi"), AgentState::Waiting, "Approve? [y/n]"),
            Some(("pi", 500)),
            &test_pane(),
            now,
            STUCK_SECS,
        )
        .expect("no record");
        assert_eq!(rec.state, AgentState::Waiting);
        assert_eq!(rec.last_turn_secs, 300);
    }

    #[test]
    fn stuck_carries_turn_without_ending_it() {
        let now = 100_000;
        let mut old = carried("pi", AgentState::Working, 500, now - 900, now - 900);
        old.last_turn_secs = 120;
        let stuck = merge_record(
            Some(&old),
            false,
            None,
            Some(("pi", 500)),
            &test_pane(),
            now,
            STUCK_SECS,
        )
        .expect("no record");
        assert_eq!(stuck.state, AgentState::Stuck);
        assert_eq!(stuck.last_turn_secs, 120);
        // Stuck → waiting: the grinding episode never completed, so the
        // previous completed turn still stands.
        let waiting = merge_record(
            Some(&stuck),
            false,
            screen_hit(Some("pi"), AgentState::Waiting, "Approve? [y/n]"),
            Some(("pi", 500)),
            &test_pane(),
            now + 10,
            STUCK_SECS,
        )
        .expect("no record");
        assert_eq!(waiting.last_turn_secs, 120);
    }

    #[test]
    fn stuck_after_still_cpu() {
        let now = 100_000;
        let old = carried("pi", AgentState::Working, 500, now - 601, now - 601);
        let rec = merge_record(
            Some(&old),
            false,
            None,
            Some(("pi", 500)),
            &test_pane(),
            now,
            STUCK_SECS,
        )
        .expect("no record");
        assert_eq!(rec.state, AgentState::Stuck);
        assert_eq!(rec.state_since, now);
        assert_eq!(rec.summary, "no cpu for 10m");
        assert!(rec.state.needs_attention());
    }

    #[test]
    fn stuck_keeps_clock_while_still() {
        let now = 100_000;
        let old = carried("pi", AgentState::Stuck, 500, now - 660, now - 660);
        let rec = merge_record(
            Some(&old),
            false,
            None,
            Some(("pi", 500)),
            &test_pane(),
            now,
            STUCK_SECS,
        )
        .expect("no record");
        assert_eq!(rec.state, AgentState::Stuck);
        assert_eq!(rec.state_since, now - 660);
        assert_eq!(rec.summary, "no cpu for 11m");
    }

    #[test]
    fn stuck_disabled_when_zero() {
        let now = 100_000;
        let old = carried("pi", AgentState::Working, 500, now - 3600, now - 3600);
        let rec = merge_record(
            Some(&old),
            false,
            None,
            Some(("pi", 500)),
            &test_pane(),
            now,
            0,
        )
        .expect("no record");
        assert_eq!(rec.state, AgentState::Working);
    }

    #[test]
    fn hook_carry_trips_stuck_and_recovers() {
        // The Hook|Manual carry branch runs the shared stuck gate: a hung
        // hook-claimed pane trips Stuck (saving the hook's verdict), keeps
        // its clock while still, and restores the verdict when CPU
        // recovers — instead of reading Working until the 30-minute lapse
        // or latching Stuck forever.
        let now = 100_000;
        let mut r = hook_record(now - 1800);
        r.state = AgentState::Working;
        r.cpu_ticks = 500;
        r.cpu_still_since = now - 601;
        r.quiet_since = now; // screen fresh: a hang, not finished
        apply_hook_stuck_gate(&mut r, true, now, STUCK_SECS, 3600);
        assert_eq!(r.state, AgentState::Stuck);
        assert_eq!(r.stuck_base, Some(AgentState::Working));
        assert_eq!(r.state_since, now);
        assert_eq!(r.summary, "no cpu for 10m");
        // Still still: clock kept, no renotify.
        let mut r2 = r.clone();
        apply_hook_stuck_gate(&mut r2, true, now + 60, STUCK_SECS, 3600);
        assert_eq!(r2.state, AgentState::Stuck);
        assert_eq!(r2.state_since, now);
        // CPU recovered: hook's verdict restored, flip clocked, trip
        // summary blanked for the screen fallback.
        r2.cpu_still_since = now + 60;
        apply_hook_stuck_gate(&mut r2, true, now + 60, STUCK_SECS, 3600);
        assert_eq!(r2.state, AgentState::Working);
        assert_eq!(r2.stuck_base, None);
        assert_eq!(r2.state_since, now + 60);
        assert!(r2.summary.is_empty());
    }

    #[test]
    fn hook_carry_stuck_gate_respects_exemptions() {
        let now = 100_000;
        let mut hung = hook_record(now - 1800);
        hung.state = AgentState::Working;
        hung.cpu_ticks = 500;
        hung.cpu_still_since = now - 601;
        // Long-quiet screen: finished, not hung.
        hung.quiet_since = now - 7200;
        apply_hook_stuck_gate(&mut hung, true, now, STUCK_SECS, 3600);
        assert_eq!(hung.state, AgentState::Working);
        // Disabled gate: never trips.
        hung.quiet_since = now;
        apply_hook_stuck_gate(&mut hung, true, now, 0, 3600);
        assert_eq!(hung.state, AgentState::Working);
        // No live process: never trips.
        apply_hook_stuck_gate(&mut hung, false, now, STUCK_SECS, 3600);
        assert_eq!(hung.state, AgentState::Working);
        // A hook-claimed Stuck (no trip base) is never touched.
        let mut claimed = hook_record(now - 100);
        claimed.state = AgentState::Stuck;
        claimed.cpu_still_since = now - 601;
        apply_hook_stuck_gate(&mut claimed, true, now, STUCK_SECS, 3600);
        assert_eq!(claimed.state, AgentState::Stuck);
        assert_eq!(claimed.stuck_base, None);
    }

    #[test]
    fn custom_stuck_threshold_honored() {
        // 601s of stillness trips at 600 but not at 3600.
        let now = 100_000;
        let old = carried("pi", AgentState::Working, 500, now - 601, now - 601);
        let rec = merge_record(
            Some(&old),
            false,
            None,
            Some(("pi", 500)),
            &test_pane(),
            now,
            3600,
        )
        .expect("no record");
        assert_eq!(rec.state, AgentState::Working);
    }

    #[test]
    fn custom_stale_lapses_early() {
        let now = 10_000;
        let rec = hook_record(now - 61);
        let (old, stale) = split_prev(Some(&rec), now, 60);
        assert!(old.is_none());
        assert!(stale.is_some());
        // … but stays fresh under the default window.
        let (old, stale) = split_prev(Some(&rec), now, HOOK_STALE_SECS);
        assert!(old.is_some());
        assert!(stale.is_none());
    }

    #[test]
    fn tunables_clamp_zero_poll() {
        let cfg = config::AppConfig {
            poll_secs: 0,
            stale_secs: 0,
            ..config::AppConfig::default()
        };
        let tune = Tunables::from_config(&cfg);
        assert_eq!(tune.poll_secs, 1);
        assert_eq!(tune.stale_secs, 1);
        assert_eq!(tune.stuck_secs, STUCK_SECS);
    }

    #[test]
    fn cpu_movement_clears_stuck() {
        let now = 100_000;
        let old = carried("pi", AgentState::Stuck, 500, now - 900, now - 900);
        let rec = merge_record(
            Some(&old),
            false,
            None,
            Some(("pi", 750)),
            &test_pane(),
            now,
            STUCK_SECS,
        )
        .expect("no record");
        assert_eq!(rec.state, AgentState::Working);
        assert_eq!(rec.state_since, now);
        assert_eq!(rec.cpu_still_since, now);
    }

    #[test]
    fn restarted_agent_resets_clock() {
        // Shrunk sum = new process, not stillness: never stuck on sight.
        let now = 100_000;
        let old = carried("pi", AgentState::Working, 500, now - 900, now - 900);
        let rec = merge_record(
            Some(&old),
            false,
            None,
            Some(("pi", 100)),
            &test_pane(),
            now,
            STUCK_SECS,
        )
        .expect("no record");
        assert_eq!(rec.state, AgentState::Working);
        assert_eq!(rec.cpu_still_since, now);
    }

    #[test]
    fn waiting_clears_stuck_with_fresh_clock() {
        let now = 100_000;
        let old = carried("pi", AgentState::Stuck, 500, now - 900, now - 900);
        let rec = merge_record(
            Some(&old),
            false,
            screen_hit(Some("pi"), AgentState::Waiting, "Approve? [y/n]"),
            Some(("pi", 500)),
            &test_pane(),
            now,
            STUCK_SECS,
        )
        .expect("no record");
        assert_eq!(rec.state, AgentState::Waiting);
        assert_eq!(rec.state_since, now);
    }
}
