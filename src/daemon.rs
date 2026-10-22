//! terhubd: Unix-socket daemon holding the agent registry.
//!
//! The daemon merges three signals per pane: authoritative hook events pushed
//! by agents (`terhub hook ...`), process-tree agent identification, and
//! screen-pattern classification of hook-less panes from periodic
//! `capture-pane` polls. Clients talk newline-delimited JSON over the
//! socket; see [`Request`].
mod poll;

use crate::agent::{
    AgentRecord, AgentState, CompiledManifest, HookEvent, builtin_manifests, compile_all_lossy,
    load_manifests, now_unix,
};
use crate::attend::detect_all_transitions;
use crate::config;
use crate::history;
use anyhow::{Context, Result};
pub(crate) use poll::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
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
///
/// 15 minutes, not 5: a live turn is often silent (a model thinking, a
/// tool call that prints nothing), and five minutes of a frozen screen
/// called plenty of working agents "finished". The flip only has to come
/// later than the silence it mistakes for an end — a quarter of an hour
/// of a frozen screen behind a live agent is a real end-of-turn signal.
pub const QUIET_IDLE_SECS: u64 = 900;

/// A `waiting` prompt untouched this long stops counting as urgent.
/// Longer than [`QUIET_IDLE_SECS`] on purpose: an agent that just asked
/// something must keep flagging, even if you stepped away for a minute.
pub const WAITING_IDLE_SECS: u64 = 1800;

/// A finished agent that has stopped producing output is waiting for its
/// next prompt — that is idle, not a permanent `done`.
///
/// `done` is a transient FYI: it floats above the idle mass when the table
/// is sorted by state, so a completion you missed still surfaces. It is
/// not a resting state. Without this decay the completion banner stays in
/// the scrollback, the classifier re-matches it on every poll, and the row
/// reads `done` forever — so it never counts towards the `N idle` summary
/// and the operator cannot tell a finished agent that is ready for input
/// from one that is mid-turn.
///
/// Shorter than [`QUIET_IDLE_SECS`] on purpose: a `done` banner is positive
/// evidence that the turn ended, while a frozen screen is a guess that has
/// to wait out a false positive. The summary is kept, so the completion
/// itself is not lost — only the state label decays. `0` disables it.
pub const DONE_IDLE_SECS: u64 = 300;

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
    /// Stale-completion → Idle threshold (`0` disables).
    pub done_idle_secs: u64,
}

impl Default for Tunables {
    fn default() -> Self {
        Tunables {
            poll_secs: POLL_SECS,
            stuck_secs: STUCK_SECS,
            stale_secs: HOOK_STALE_SECS,
            quiet_idle_secs: QUIET_IDLE_SECS,
            waiting_idle_secs: WAITING_IDLE_SECS,
            done_idle_secs: DONE_IDLE_SECS,
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
            done_idle_secs: cfg.done_idle_secs,
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
    let meta = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
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

/// Epoch millis of the last *failed* self-heal attempt, or 0 when none is
/// outstanding (never tried, or the last attempt succeeded). A success
/// clears it so the next death heals immediately instead of waiting out
/// the previous attempt's backoff.
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
    let log_file = open_daemon_log(&runtime_dir());
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
        Err(e) => {
            // Say why the log is not there instead of dropping the
            // daemon's stderr on the floor silently.
            eprintln!("terhub: {e}");
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
    // re-entered by a concurrent caller. A success clears the stamp:
    // the next death is a new incident, not a continuation, so it
    // heals immediately instead of waiting out this attempt's backoff.
    // A failure keeps it, so a permanently broken daemon is still
    // retried at most once per backoff.
    LAST_RESPAWN_MS.store(now, std::sync::atomic::Ordering::Relaxed);
    let healed = spawn_daemon().is_ok();
    if healed {
        LAST_RESPAWN_MS.store(0, std::sync::atomic::Ordering::Relaxed);
    }
    healed
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

/// The wire protocol version.
///
/// The version rides with every request and response. A request
/// *without* `proto` comes from a pre-versioned (legacy) client, and
/// the daemon answers such a client in a shape that client can still
/// decode — see [`legacy_state`].
pub const PROTO_VERSION: u32 = 1;

/// The command set, as it appears on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Cmd {
    Ping,
    Snapshot,
    Event,
    Register,
    Unregister,
    Shutdown,
}

/// The wire envelope.
///
/// A flat struct rather than the tagged [`Request`] enum, so the
/// protocol version can ride alongside the command without touching
/// every variant. The serialized shape is the same one the tagged enum
/// produced (`{"cmd":"snapshot"}`, `{"cmd":"register","pane":…}`), so
/// an old client and a new daemon still speak to each other.
#[derive(Debug, Serialize, Deserialize)]
pub struct WireRequest {
    pub cmd: Cmd,
    /// Absent => a legacy (pre-versioned) client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proto: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<HookEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<AgentState>,
}

impl From<&Request> for WireRequest {
    fn from(req: &Request) -> Self {
        let mut w = WireRequest {
            cmd: Cmd::Ping,
            proto: Some(PROTO_VERSION),
            event: None,
            pane: None,
            agent: None,
            pid: None,
            state: None,
        };
        match req {
            Request::Ping => {}
            Request::Snapshot => w.cmd = Cmd::Snapshot,
            Request::Shutdown => w.cmd = Cmd::Shutdown,
            Request::Event { event } => {
                w.cmd = Cmd::Event;
                w.event = Some(event.clone());
            }
            Request::Register {
                pane,
                agent,
                pid,
                state,
            } => {
                w.cmd = Cmd::Register;
                w.pane = Some(pane.clone());
                w.agent = Some(agent.clone());
                w.pid = Some(*pid);
                w.state = Some(*state);
            }
            Request::Unregister { pane } => {
                w.cmd = Cmd::Unregister;
                w.pane = Some(pane.clone());
            }
        }
        w
    }
}

impl TryFrom<WireRequest> for Request {
    type Error = String;

    fn try_from(w: WireRequest) -> Result<Self, String> {
        Ok(match w.cmd {
            Cmd::Ping => Request::Ping,
            Cmd::Snapshot => Request::Snapshot,
            Cmd::Shutdown => Request::Shutdown,
            Cmd::Event => Request::Event {
                event: w.event.ok_or("event command without an event")?,
            },
            Cmd::Register => Request::Register {
                pane: w.pane.ok_or("register without a pane")?,
                agent: w.agent.ok_or("register without an agent")?,
                pid: w.pid.ok_or("register without a pid")?,
                state: w.state.ok_or("register without a state")?,
            },
            Cmd::Unregister => Request::Unregister {
                pane: w.pane.ok_or("unregister without a pane")?,
            },
        })
    }
}

/// The agent states a legacy (pre-versioned) client can decode.
///
/// A legacy client has no `paused` variant: handing it one fails the
/// whole response, and the client then reports "daemon not running"
/// while the daemon is perfectly alive. The daemon therefore
/// down-projects the states a legacy client cannot read.
///
/// `paused` and `idle` are both quiet, so `idle` is the honest legacy
/// rendering of a paused agent: it needs nobody, exactly like idle.
/// Every state is listed explicitly — adding a new one forces a
/// decision here instead of silently breaking every old client.
pub fn legacy_state(state: AgentState) -> AgentState {
    match state {
        AgentState::Paused => AgentState::Idle,
        AgentState::Unknown => AgentState::Unknown,
        AgentState::Working => AgentState::Working,
        AgentState::Waiting => AgentState::Waiting,
        AgentState::Error => AgentState::Error,
        AgentState::Done => AgentState::Done,
        AgentState::Idle => AgentState::Idle,
        AgentState::Stuck => AgentState::Stuck,
        AgentState::Limited => AgentState::Limited,
    }
}

/// Down-project a whole snapshot for a legacy client.
pub fn project_for_legacy(mut snap: Snapshot) -> Snapshot {
    for agent in &mut snap.agents {
        agent.state = legacy_state(agent.state);
    }
    snap
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<Snapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The daemon's protocol version. Absent => a pre-versioned daemon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proto: Option<u32>,
    /// The daemon's build identity (see [`build_id`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
}

impl Response {
    fn ok() -> Self {
        Response {
            ok: true,
            snapshot: None,
            error: None,
            proto: Some(PROTO_VERSION),
            build: Some(build_label()),
        }
    }

    fn err(msg: String) -> Self {
        Response {
            ok: false,
            snapshot: None,
            error: Some(msg),
            proto: Some(PROTO_VERSION),
            build: Some(build_label()),
        }
    }

    fn snapshot(snapshot: Snapshot) -> Self {
        Response {
            ok: true,
            snapshot: Some(snapshot),
            error: None,
            proto: Some(PROTO_VERSION),
            build: Some(build_label()),
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

/// Open the daemon's stderr log inside `dir`, creating the runtime dir
/// when it does not exist yet.
///
/// The log lives inside the runtime dir, and on a first install that
/// dir does not exist yet: the daemon creates it in the child
/// (`Server::load` -> `ensure_runtime_dir`), which is *after* the
/// stderr redirect is set up by the parent. Opening the log first
/// failed with ENOENT, and the fallback sent the daemon's stderr to
/// /dev/null — losing the manifest warnings, bind errors and panics
/// the log exists to keep, and leaving the failure message pointing
/// at a file that was never created.
///
/// Owner-only (0600): daemon stderr may contain prompt text. Append,
/// not truncate: a respawn must not wipe the previous run's stderr —
/// that is where a crash or a failed bind is explained.
fn open_daemon_log(dir: &Path) -> std::result::Result<std::fs::File, String> {
    // Same 0700 mode the daemon uses for the dir itself. A failure is
    // not fatal here: the child creates the dir itself and reports the
    // real error.
    if let Err(e) = ensure_runtime_dir(dir) {
        eprintln!("terhub: runtime dir not ready ({e})");
    }
    let path = dir.join("terhubd.log");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| format!("cannot open daemon log {}: {e}", path.display()))?;
    // An existing log from an older build keeps its old mode; tighten
    // it like every other runtime file.
    let _ = set_owner_only(&path);
    Ok(file)
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
        // No ledger prune here: pruning is the poll loop's job (hourly,
        // and its first tick fires immediately), so it happens off the
        // bind path. Pruning in `load` meant a daemon start blocked on
        // a full read-and-rewrite of the ledger — seconds on a ledger
        // of tens of megabytes — and the socket did not appear until
        // it was done, so every `terhub daemon start` (and every TUI
        // self-heal) stalled on it.
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
        // The data dir holds prompts, summaries and project paths — the
        // same material the daemon's own log is kept out of reach for.
        // The ledger is created 0600, but a file that predates that
        // keeps its old mode until something corrects it.
        if let Some(parent) = history::ledger_path().parent() {
            let _ = std::fs::create_dir_all(parent);
            let _ = std::fs::set_permissions(
                parent,
                <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
            );
        }
        history::ensure_ledger_mode(&history::ledger_path());
        // Collect the crash leftovers of the ledger's atomic writes.
        // They are megabytes each on a busy ledger, and nothing else
        // would ever remove them.
        crate::config::sweep_orphan_tmps(&history::data_dir());
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

        // Bounded connection tasks: each accepted stream spawns a task
        // that can linger up to the 10s read timeout, so an uncapped
        // accept loop lets a peer holding many connections open pile up
        // registry-Arc-holding tasks without bound.
        let permits = Arc::new(tokio::sync::Semaphore::new(MAX_CONN_TASKS));
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
            // Peer-credential check, before a single byte is read.
            //
            // The socket is 0600 and the runtime dir 0700, so "only my
            // own user can open it" is already true — but that is a
            // property of the filesystem, not a statement about who is
            // on the other end of an accepted connection. SO_PEERCRED
            // reports the peer's uid as the kernel saw it at connect
            // time, so a socket reached through a loosened runtime dir
            // (a shared TERHUB_RUNTIME_DIR, a filesystem that does not
            // enforce POSIX modes, an operator who chmod'ed the dir to
            // share it) still admits nobody but our own uid. The check
            // happens before the stream is admitted, so a rejected peer
            // never reaches the registry, never pins a Manual record,
            // and never gets to send a Shutdown.
            match peer_uid(&stream) {
                Ok(peer) => {
                    if !peer_allowed(peer, our_uid()) {
                        eprintln!(
                            "terhubd: rejected connection from peer uid {peer:?} (expected {})",
                            our_uid()
                        );
                        continue;
                    }
                }
                Err(e) => {
                    // Fail closed: a credential check that passes when
                    // it cannot read the credentials is not a check.
                    eprintln!("terhubd: peer credential check failed: {e}");
                    continue;
                }
            }
            let reg = self.registry.clone();
            let runtime = self.runtime.clone();
            let runtime_guard = self.runtime_guard.clone();
            let shutdown = self.shutdown.clone();
            admit_conn(
                stream,
                reg,
                runtime,
                runtime_guard,
                shutdown,
                permits.clone(),
            );
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

/// The daemon's own effective uid: the uid a peer must match to be
/// served. The daemon is never setuid, so the effective uid is the
/// identity that owns the runtime dir and the socket.
fn our_uid() -> u32 {
    // SAFETY: geteuid() is always safe and never fails.
    unsafe { libc::geteuid() }
}

/// The peer-credential policy, split out so it can be tested without
/// privileges.
///
/// A peer whose uid the platform reports must be our own uid. A
/// platform that cannot report a Unix socket peer's uid (`None`)
/// leaves the socket's 0600 mode as the only gate — the check is
/// skipped there rather than invented.
fn peer_allowed(peer: Option<u32>, ours: u32) -> bool {
    peer.is_none_or(|uid| uid == ours)
}

/// The uid of the peer on the other end of a connected Unix stream.
///
/// Linux reports it through `SO_PEERCRED`, which the kernel fills in
/// from the connecting process's credentials at `connect()` time — it
/// cannot be forged by the peer. On platforms without `SO_PEERCRED`
/// there is no equivalent, and `Ok(None)` says so: the caller then
/// falls back to the socket's own 0600 mode, which is the whole gate
/// on those systems.
#[cfg(target_os = "linux")]
fn peer_uid(stream: &TokioUnixStream) -> std::result::Result<Option<u32>, std::io::Error> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` is a correctly sized, writable `ucred`, and `len`
    // is its real size; getsockopt writes at most that many bytes.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(Some(cred.uid))
}

/// Non-Linux: the kernel offers no way to read a Unix socket peer's
/// uid, so there is nothing to report. The socket's 0600 mode remains
/// the gate.
#[cfg(not(target_os = "linux"))]
fn peer_uid(_stream: &TokioUnixStream) -> std::result::Result<Option<u32>, std::io::Error> {
    Ok(None)
}

/// Max concurrent connection-handler tasks. Hook bursts and TUI polls
/// are single-digit concurrent; 64 is vast headroom while bounding the
/// pile-up to 64 task stacks instead of one per open fd.
const MAX_CONN_TASKS: usize = 64;

/// Admit one accepted stream under the connection-task cap: a free
/// permit spawns the handler (held for the task's whole life, including
/// the 10s read wait); at capacity the stream is dropped unhandled so
/// the peer sees EOF and can retry. Shedding beats queueing: a queue
/// would just move the pile-up behind a semaphore.
fn admit_conn(
    stream: TokioUnixStream,
    registry: Arc<Mutex<HashMap<String, AgentRecord>>>,
    runtime: PathBuf,
    runtime_guard: RuntimeDirGuard,
    shutdown: Arc<AtomicBool>,
    permits: Arc<tokio::sync::Semaphore>,
) {
    let Ok(permit) = permits.try_acquire_owned() else {
        return;
    };
    tokio::spawn(async move {
        let _permit = permit;
        handle_conn(stream, registry, runtime, runtime_guard, shutdown).await;
    });
}

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
    let wire: WireRequest = match serde_json::from_str(&line) {
        Ok(w) => w,
        Err(e) => {
            let _ = write_response(&mut writer, &Response::err(format!("bad request: {e}"))).await;
            return;
        }
    };
    // A request without a protocol version comes from a pre-versioned
    // client. It is served in a shape that client can still decode.
    let legacy = wire.proto.is_none();
    let req: Request = match Request::try_from(wire) {
        Ok(r) => r,
        Err(e) => {
            let _ = write_response(&mut writer, &Response::err(format!("bad request: {e}"))).await;
            return;
        }
    };
    // A parsed request is a live client: keep the idle reaper honest.
    stamp_request();
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
            let snapshot = Snapshot {
                agents,
                updated_at: now_unix(),
            };
            // A legacy client cannot decode states that were added
            // after it was built: hand it the down-projected view
            // instead of a response it will fail to parse. A versioned
            // client gets the true state.
            let snapshot = if legacy {
                project_for_legacy(snapshot)
            } else {
                snapshot
            };
            Response::snapshot(snapshot)
        }
        Request::Event { event } => {
            // Mutation + save without holding the lock across disk IO.
            let (transitions, now) = mutate_and_save(
                &registry,
                &runtime_guard,
                &runtime.join("registry.json"),
                |reg| {
                    let now = now_unix();
                    let t = poll::apply_hook_event(reg, &event, now);
                    (t, now)
                },
            );
            poll::emit_transitions(&transitions, now).await;
            // Prompts log even when the state didn't move: asking again is
            // itself the event the recap reads back.
            if !event.prompt.trim().is_empty() {
                log_entry(history::Entry::prompt(
                    now,
                    &event.pane,
                    &crate::proc::canonical_agent(&event.agent),
                    &event.prompt,
                ))
                .await;
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
                    poll::apply_register(reg, &pane, &agent, pid, state, now);
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
                |reg| poll::apply_unregister(reg, &pane),
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

/// Seconds of client silence after which the daemon retires on its own.
///
/// Opt-in via `TERHUBD_IDLE_TTL_SECS`; 0 (the default) means never.
/// A daemon nobody talks to is an orphan — usually a test that died
/// before its cleanup ran — and `terhubd` has no parent-death signal,
/// so without this it would poll tmux forever, burning a slice of a
/// core. A live daemon is never idle: the TUI polls it every 2s, the
/// status line and the tray every 3s, so a real installation never
/// comes near the TTL.
fn idle_ttl_secs() -> u64 {
    std::env::var("TERHUBD_IDLE_TTL_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

/// Unix second of the most recent client request; 0 means "never".
///
/// Any request counts: the TTL exists to reap orphans, not to retire a
/// daemon somebody is still using.
static LAST_REQUEST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Stamp the last-request clock. Monotonic within a run: a backwards
/// wall-clock step must not make an active daemon look idle.
fn stamp_request() {
    LAST_REQUEST.fetch_max(now_unix(), Ordering::SeqCst);
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
    let idle_ttl = idle_ttl_secs();
    let started = now_unix();
    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        tokio::select! {
            _ = tick.tick() => {},
            _ = prune_tick.tick() => {
                let _ = tokio::task::spawn_blocking(|| {
                    let ledger = history::ledger_path();
                    if let Err(e) = history::prune(&ledger, now_unix(), 90) {
                        eprintln!("terhubd: history prune failed ({e})");
                    }
                    // Crash leftovers from the ledger's atomic writes
                    // are megabytes each; collect them with the prune
                    // that already rewrites the file.
                    crate::config::sweep_orphan_tmps(&history::data_dir());
                }).await;
                continue;
            },
            _ = shutdown_wait(&shutdown) => break,
        }
        let poll_start = now_unix();
        // Orphan reaper: a daemon that no client has spoken to for the
        // TTL exits on its own. Off by default — a live daemon is
        // polled constantly — but it bounds the damage when a test
        // process is killed before its cleanup runs.
        if idle_ttl > 0 {
            let last = LAST_REQUEST.load(Ordering::SeqCst);
            // A daemon that has never been spoken to has no timestamp
            // yet: measure from the start, so a daemon that is born and
            // then ignored still expires.
            let idle_since = if last == 0 { started } else { last };
            if poll_start.saturating_sub(idle_since) > idle_ttl {
                eprintln!("terhubd: no client request for {idle_ttl}s; exiting (idle timeout)");
                shutdown.store(true, Ordering::SeqCst);
                break;
            }
        }
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
        let next =
            tokio::task::spawn_blocking(move || poll::poll_sync(prev, &manifests, tune)).await;
        match next {
            Ok((mut next, sessions, foreground, outage, live_panes)) => {
                // Fold the poll result back under one lock hold: hooks that
                // landed during the poll are in the live registry but were
                // never in the pre-poll clone poll_sync saw, so merging them
                // here (and diffing against live, not the clone) is what
                // keeps the write-back from silently discarding them.
                // Save runs outside the lock so a slow disk doesn't stall
                // every concurrent snapshot/automation reader.
                let transitions = {
                    let mut reg = lock_registry(&registry);
                    poll::merge_mid_poll_hooks(&mut next, &reg, poll_start, live_panes.as_ref());
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
                poll::emit_transitions(&transitions, now).await;
                // Outage: sessions/foreground unknown — skip diffing AND the
                // prev update, so recovery diffs against last-known-good and
                // neither direction logs phantoms.
                if !outage {
                    let sessions: std::collections::HashSet<String> =
                        sessions.into_iter().collect();
                    if let Some(prev) = prev_sessions.replace(sessions.clone()) {
                        for opened in sessions.difference(&prev) {
                            log_entry(history::Entry::session_event(now, opened, true)).await;
                        }
                        for closed in prev.difference(&sessions) {
                            log_entry(history::Entry::session_event(now, closed, false)).await;
                        }
                    }
                    if let Some(prev) = prev_foreground.replace(foreground.clone())
                        && prev != foreground
                    {
                        log_entry(history::Entry::human_focus(now, foreground.as_deref())).await;
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
                        let _guard = poll::AutomationGuard(busy);
                        run_automation(&reg).await;
                    });
                }
            }
            Err(_) => break, // runtime shutting down
        }
    }
}

/// Append one entry to the activity ledger.
///
/// The ledger write takes an exclusive `flock` and fsyncs, so it is
/// blocking I/O: on a tokio worker it parks that worker for the
/// duration — and for the whole of an hourly prune, which holds that
/// same lock while it rewrites the ledger. `spawn_blocking` keeps the
/// worker free; the callers await it, so the ledger order still
/// matches the event order.
async fn log_entry(entry: history::Entry) {
    let path = history::ledger_path();
    match tokio::task::spawn_blocking(move || history::append_entry(&path, &entry)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => eprintln!("terhubd: ledger append failed ({e})"),
        Err(e) => eprintln!("terhubd: ledger append task failed ({e})"),
    }
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

/// Attempt counter, folded into save-tmp names: two saves on one pid
/// can share a nanos tick, so pid + nanos alone does not uniquely name
/// an attempt. With the counter every attempt name is unique, which
/// means a retry never abandons a name a later attempt will reuse —
/// the AlreadyExists path below can only be somebody else's file.
static SAVE_ATTEMPT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Process start (epoch secs), pinned on first save: tmp leftovers
/// older than this predate us, so the stale sweep can never touch a
/// live peer's in-flight tmp.
static PROCESS_START_SECS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

/// Best-effort removal of our own stale save tmps: crash leftovers no
/// future save will ever collide with, so without a sweep they
/// accumulate one per crash. Only our pid, only predating our start —
/// a live peer (a concurrent save on this process, or a second daemon
/// on this runtime dir) always has a newer mtime or a different pid,
/// so the sweep can never eat an in-flight save.
fn sweep_stale_tmps(final_path: &Path, pid: u32, start_secs: u64) {
    let Some(dir) = final_path.parent() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let needle = format!("tmp.{pid}.");
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.contains(&needle) {
            continue;
        }
        // Unreadable mtime: conservative, leave it alone.
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs());
        if stale.is_some_and(|mtime| mtime < start_secs) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn save_registry(path: &Path, registry: &HashMap<String, AgentRecord>) {
    let agents: Vec<&AgentRecord> = registry.values().collect();
    let text = match serde_json::to_string_pretty(&agents) {
        Ok(t) => t,
        Err(e) => {
            poll::note_sticky("registry-save", Some(format!("registry serialize: {e:#}")));
            return;
        }
    };
    // Tmp + rename: a crash mid-write never leaves a half file behind.
    // The tmp name includes pid + nanos + a per-process attempt counter
    // so two daemons pointed at the same runtime dir (or two saves on
    // one pid sharing a nanos tick) interleave instead of clobbering
    // each other. `create_new(true)` is the last-resort guard: a hit
    // means somebody else's live file, so retry with a fresh name
    // rather than truncating a tmp another save still needs.
    let pid = std::process::id();
    let start = *PROCESS_START_SECS.get_or_init(now_unix);
    sweep_stale_tmps(path, pid, start);
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
        let tag = SAVE_ATTEMPT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = path.with_extension(format!("tmp.{pid}.{nonce}.{tag}"));
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
                // No fsync: the registry is a derived cache (the poll
                // rebuilds it every 2s) whose only non-derivable
                // content is a manual pin, and the write is atomic
                // anyway (tmp + rename), so a reader never sees a torn
                // file. The fsync cost a disk flush per poll -- over a
                // second on a loaded machine -- which made the daemon
                // answer its clients later than their deadlines. The
                // data still survives a process crash; only a machine
                // crash loses it, and the ledger (the real history)
                // keeps its own fsync.
                drop(f);
                return std::fs::rename(&tmp, path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    })();
    match result {
        Ok(()) => poll::note_sticky("registry-save", None),
        Err(e) => poll::note_sticky(
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

/// Why a daemon request failed.
///
/// The three cases are genuinely different, and the difference is the
/// whole point of this type: only the first means "there is no
/// daemon". The second means a daemon answered and this build could
/// not read the answer; the third means a daemon answered, named its
/// protocol version, and that version is not ours.
#[derive(Debug)]
pub enum DaemonError {
    /// No daemon answered: the connect, write or read failed.
    Unreachable { detail: String },
    /// A daemon answered, but the response is not readable by this build.
    Unreadable { detail: String },
    /// A daemon answered, but it speaks another protocol version.
    Incompatible {
        client_proto: u32,
        daemon_proto: Option<u32>,
        daemon_build: Option<String>,
        detail: String,
    },
    /// The daemon is there but did not answer within the timeout.
    ///
    /// Not the same as "not running": the daemon answered other
    /// requests (or was mid-write) and this one outran its deadline.
    /// Reporting that as "daemon not running" is a lie the user acts
    /// on -- they start a second daemon -- so it gets its own answer.
    Slow { detail: String },
}

impl DaemonError {
    /// The user-facing message.
    ///
    /// A daemon that answered must never be reported as "not running":
    /// that lie is what turned a protocol mismatch into a blank
    /// screen and a false "daemon down" header.
    pub fn message(&self) -> String {
        match self {
            DaemonError::Unreachable { detail } => {
                format!("daemon not running (terhub daemon start): {detail}")
            }
            DaemonError::Slow { detail } => {
                format!("daemon too slow to answer (terhub daemon start): {detail}")
            }
            DaemonError::Unreadable { detail } => {
                format!("daemon running but the response could not be read ({detail})")
            }
            DaemonError::Incompatible {
                client_proto,
                daemon_proto,
                daemon_build,
                detail,
            } => match daemon_proto {
                Some(p) if *p != *client_proto => {
                    let build = daemon_build
                        .as_ref()
                        .map(|b| format!(" (daemon build {b})"))
                        .unwrap_or_default();
                    format!(
                        "daemon build incompatible: client proto {client_proto} / daemon proto {p}{build} — run `make build`"
                    )
                }
                _ => format!("daemon running but the response could not be read ({detail})"),
            },
        }
    }

    /// True only when there is genuinely no daemon to talk to.
    ///
    /// The TUI's self-heal re-forks the daemon on this and only this: a
    /// daemon that answered but cannot be understood must not be
    /// restarted, or the log fills with "another terhub daemon already
    /// holds the lock" while the healthy daemon keeps serving.
    pub fn is_unreachable(&self) -> bool {
        matches!(self, DaemonError::Unreachable { .. })
    }
}

impl std::fmt::Display for DaemonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for DaemonError {}

/// This build's identity, as the daemon reports it.
pub fn build_id() -> &'static str {
    crate::BUILD_ID
}

/// The build label the client and the daemon compare: the package
/// version plus the source fingerprint. Two processes agree on "same
/// build" only when both parts agree.
pub fn build_label() -> String {
    format!("{}+{}", crate::VERSION, crate::BUILD_ID)
}

/// Send a request to the daemon and decode its answer.
///
/// The error type distinguishes "no daemon" from "a daemon I cannot
/// understand" — the distinction the whole protocol work exists for.
pub fn send_request(req: &Request, timeout: Duration) -> Result<Response, DaemonError> {
    send_request_at(&socket_path(), req, timeout)
}

/// [`send_request`] against an explicit socket path.
///
/// The path is a parameter so the timeout behaviour can be tested
/// against a peer that accepts the connection and then says nothing.
fn send_request_at(
    sock: &std::path::Path,
    req: &Request,
    timeout: Duration,
) -> Result<Response, DaemonError> {
    let mut stream = UnixStream::connect(sock).map_err(|e| DaemonError::Unreachable {
        detail: format!("connect {}: {e}", sock.display()),
    })?;
    stream
        .set_read_timeout(Some(timeout))
        .and_then(|()| stream.set_write_timeout(Some(timeout)))
        .map_err(|e| DaemonError::Unreachable {
            detail: format!("set socket timeout: {e}"),
        })?;
    let mut line =
        serde_json::to_string(&WireRequest::from(req)).map_err(|e| DaemonError::Unreadable {
            detail: format!("encode request: {e}"),
        })?;
    line.push('\n');
    stream
        .write_all(line.as_bytes())
        .and_then(|()| stream.flush())
        .map_err(|e| DaemonError::Unreachable {
            detail: format!("write request: {e}"),
        })?;
    let mut reader = BufReader::new(stream);
    let raw = read_capped_line(&mut reader, MAX_RESPONSE_BYTES).map_err(|e| {
        // A read that outran the socket's receive timeout surfaces as
        // EAGAIN on Linux (that is how SO_RCVTIMEO expiry is reported),
        // which is "the daemon is too slow right now" -- not "the
        // daemon is gone". The two deserve different answers: the
        // first is worth retrying, the second means start one.
        let timed_out = e
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::WouldBlock);
        if timed_out {
            DaemonError::Slow {
                detail: format!("read response timed out: {e}"),
            }
        } else {
            DaemonError::Unreachable {
                detail: format!("read response: {e}"),
            }
        }
    })?;
    decode_response(&raw)
}

/// Decode a daemon response in two steps.
///
/// The envelope (`ok`, `proto`, `build`, `error`) is read leniently
/// first, so the daemon's identity is known even when the payload is
/// not readable by this build. Only then is the snapshot decoded
/// strictly — that is where an unknown agent state used to fail, and
/// the failure used to take the daemon's identity with it, leaving
/// the client with nothing to say but "daemon not running".
fn decode_response(raw: &str) -> Result<Response, DaemonError> {
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| DaemonError::Unreadable {
            detail: format!("decode response: {e}"),
        })?;
    let proto = value
        .get("proto")
        .and_then(serde_json::Value::as_u64)
        .map(|v| v as u32);
    let build = value
        .get("build")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let ok = value
        .get("ok")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let error = value
        .get("error")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let snapshot = match value.get("snapshot") {
        Some(v) if !v.is_null() => {
            let snapshot: Snapshot =
                serde_json::from_value(v.clone()).map_err(|e| DaemonError::Incompatible {
                    client_proto: PROTO_VERSION,
                    daemon_proto: proto,
                    daemon_build: build.clone(),
                    detail: format!("decode snapshot: {e}"),
                })?;
            Some(snapshot)
        }
        _ => None,
    };
    Ok(Response {
        ok,
        snapshot,
        error,
        proto,
        build,
    })
}

/// A snapshot plus the identity of the daemon that served it.
#[derive(Debug, Clone)]
pub struct SnapshotResponse {
    pub snapshot: Snapshot,
    pub daemon_proto: Option<u32>,
    pub daemon_build: Option<String>,
}

/// A one-line note when the daemon is not this build, or `None` when
/// client and daemon agree.
///
/// A daemon that speaks another protocol version is not a failure by
/// itself: the data may decode fine. It is still worth saying out
/// loud, because the next state the daemon adds will not.
pub fn compat_note(resp: &SnapshotResponse) -> Option<String> {
    match resp.daemon_proto {
        Some(p) if p != PROTO_VERSION => Some(format!(
            "daemon build incompatible: client proto {PROTO_VERSION} / daemon proto {p} — run `make build`"
        )),
        None => Some(
            "daemon predates the versioned protocol (it sends no protocol version) — run `make build` to update it"
                .to_string(),
        ),
        Some(_) => None,
    }
}

/// A one-line note when the daemon is not the same build as this
/// client, or `None` when it is.
pub fn build_note(resp: &SnapshotResponse) -> Option<String> {
    let daemon = resp.daemon_build.as_deref()?;
    if daemon == build_label() {
        None
    } else {
        Some(format!(
            "daemon build {daemon} is not this build ({}) — run `make build`",
            build_label()
        ))
    }
}

pub fn fetch_snapshot_detailed(timeout: Duration) -> Result<SnapshotResponse, DaemonError> {
    let resp = send_request(&Request::Snapshot, timeout)?;
    let snapshot = resp
        .snapshot
        .clone()
        .ok_or_else(|| DaemonError::Unreadable {
            detail: "daemon returned no snapshot".to_string(),
        })?;
    Ok(SnapshotResponse {
        snapshot,
        daemon_proto: resp.proto,
        daemon_build: resp.build,
    })
}

pub fn fetch_snapshot(timeout: Duration) -> Result<Snapshot, DaemonError> {
    Ok(fetch_snapshot_detailed(timeout)?.snapshot)
}

pub fn send_event(ev: &HookEvent, timeout: Duration) -> Result<(), DaemonError> {
    let resp = send_request(&Request::Event { event: ev.clone() }, timeout)?;
    if resp.ok {
        Ok(())
    } else {
        Err(DaemonError::Unreadable {
            detail: resp
                .error
                .unwrap_or_else(|| "daemon rejected event".to_string()),
        })
    }
}

pub fn daemon_alive() -> bool {
    send_request(&Request::Ping, Duration::from_millis(300))
        .map(|r| r.ok)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests;
