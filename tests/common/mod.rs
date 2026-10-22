//! Shared isolation for test binaries that spawn a real `terhubd`.
//!
//! # Why this exists
//!
//! A suite that spawns the daemon without isolating it does not fail
//! loudly — it fails *unpredictably*, and it writes into the
//! developer's real data:
//!
//! * The daemon polls tmux. On the ambient server it classifies the
//!   operator's live panes, so a test that assumes "no agent is in
//!   state `error`" passes or fails depending on what the operator is
//!   running that minute. CI has no live agents and is permanently
//!   green; a busy developer gets a random red suite.
//! * The daemon appends to the activity ledger. With the ambient
//!   `TERHUB_DATA_DIR` the suite's probe events land in the operator's
//!   real history, where they show up as agents that never existed.
//!
//! [`Sandbox`] points the process at a private runtime dir, a private
//! data dir, a private config file and a private tmux socket, so both
//! problems disappear. Call [`Sandbox::new`] once per test binary,
//! before spawning anything.
//!
//! [`TestDaemon`] additionally guarantees the daemon is stopped when
//! the test ends — *including* when it ends in a panic. A daemon that
//! outlives a failed test is not a test artifact; it is a process that
//! runs forever, burning a slice of a core, with no TTL to stop it.

// Each test binary includes this module and uses part of it; the rest
// would be dead code in that binary.
#![allow(dead_code)]

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use terhub::daemon::{self, Request};

/// A private environment for one test binary.
pub struct Sandbox {
    /// `TERHUB_RUNTIME_DIR` — socket, lock and pid file live here.
    pub runtime: PathBuf,
    /// `TERHUB_DATA_DIR` — registry and activity ledger live here.
    pub data: PathBuf,
    /// `TERHUB_CONFIG` — a path that does not exist, so the daemon runs
    /// on defaults however the developer has tuned their own config.
    pub config: PathBuf,
    /// The private tmux server name (also `TERHUB_TMUX_SOCKET`).
    pub socket: String,
}

impl Sandbox {
    /// Create the sandbox and point this process at it.
    ///
    /// `tag` distinguishes the suites from each other; the pid keeps
    /// concurrently running test binaries apart.
    ///
    /// This also reaps the scratch space left by *previous* runs — see
    /// [`sweep_stale_tempdirs`]. Doing it here means every hermetic
    /// suite cleans up after the last one, so the backlog stays at one
    /// run's worth instead of growing without bound.
    pub fn new(tag: &str) -> Sandbox {
        sweep_stale_tempdirs();
        let pid = std::process::id();
        let runtime = std::env::temp_dir().join(format!("terhub-{tag}-{pid}-rt"));
        let data = std::env::temp_dir().join(format!("terhub-{tag}-{pid}-data"));
        let config = std::env::temp_dir().join(format!("terhub-{tag}-{pid}.toml"));
        let socket = format!("terhub-{tag}-{pid}");

        let _ = std::fs::remove_dir_all(&runtime);
        let _ = std::fs::remove_dir_all(&data);
        let _ = std::fs::remove_file(&config);
        std::fs::create_dir_all(&runtime).expect("create runtime dir");
        std::fs::create_dir_all(&data).expect("create data dir");

        // SAFETY: one test binary per process, and this runs before any
        // child is spawned. Nothing else in the process reads these
        // variables, and the values are per-binary, so the usual
        // set_var/setenv race cannot be observed here.
        unsafe {
            std::env::set_var("TERHUB_RUNTIME_DIR", &runtime);
            std::env::set_var("TERHUB_DATA_DIR", &data);
            std::env::set_var("TERHUB_CONFIG", &config);
            std::env::set_var(terhub::tmux::SOCKET_ENV, &socket);
            // Backstop for the case the Drop guard cannot cover: if the
            // test process is killed outright, the daemon it spawned
            // would otherwise run forever. Generous enough that it never
            // fires mid-suite — the longest quiet stretch is the 20s
            // `terhub_wait` in the MCP suite.
            std::env::set_var("TERHUBD_IDLE_TTL_SECS", "120");
        }

        Sandbox {
            runtime,
            data,
            config,
            socket,
        }
    }

    /// Kill the private tmux server and remove the temp dirs.
    ///
    /// Best-effort: a sandbox that is already gone is not an error.
    pub fn cleanup(&self) {
        kill_private_server(&self.socket);
        let _ = std::fs::remove_dir_all(&self.runtime);
        let _ = std::fs::remove_dir_all(&self.data);
        let _ = std::fs::remove_file(&self.config);
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        self.cleanup();
    }
}

/// Tear down the private tmux server this sandbox created.
///
/// Uses the raw binary rather than the library client: the library
/// has no `kill-server`, and a server with no sessions is exactly the
/// state that must not be left behind.
fn kill_private_server(socket: &str) {
    let _ = Command::new("tmux")
        .args(["-L", socket, "kill-server"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// A `terhubd` that is guaranteed to be stopped when it goes out of
/// scope — on the happy path *and* on a panic.
pub struct TestDaemon {
    child: Child,
}

impl TestDaemon {
    /// Spawn the daemon and wait for its socket to answer.
    pub fn start() -> TestDaemon {
        let child = Command::new(env!("CARGO_BIN_EXE_terhubd"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn terhubd");
        assert!(
            wait_for(daemon::daemon_alive, 100),
            "daemon did not come up"
        );
        TestDaemon { child }
    }
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        // Ask politely first, then reap, then insist. The polite path
        // is what a clean run takes; the others exist because a failed
        // assertion must not leave a daemon running forever.
        let _ = daemon::send_request(&Request::Shutdown, Duration::from_secs(1));
        for _ in 0..50 {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                _ => std::thread::sleep(Duration::from_millis(100)),
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Poll `cond` until it is true, up to `tries` times.
pub fn wait_for(cond: impl Fn() -> bool, tries: u32) -> bool {
    for _ in 0..tries {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    cond()
}

/// Remove scratch directories left behind by test runs that are over.
///
/// The suites create scratch paths under the system temp dir, named
/// `terhub-<what>-<pid>…`. A binary that ends — or is killed — leaves
/// them behind, and they accumulate: a few hundred directories after a
/// handful of runs, all of them dead weight.
///
/// The pid in the name is the creator's, so "the owner is gone" is
/// decidable: only scratch space whose owning process no longer exists
/// is removed. A run in flight keeps its own directories, and a
/// directory with no pid in its name (`terhub-kwin`, a fixed global
/// path) is never touched.
pub fn sweep_stale_tempdirs() -> usize {
    let tmp = std::env::temp_dir();
    let Ok(entries) = std::fs::read_dir(&tmp) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !(name.starts_with("terhub-") || name.starts_with("terhubd-")) {
            continue;
        }
        let Some(pid) = owner_pid(name) else {
            continue;
        };
        // The owner is alive: this scratch space is in use.
        if PathBuf::from(format!("/proc/{pid}")).exists() {
            continue;
        }
        let path = entry.path();
        let ok = if path.is_dir() {
            std::fs::remove_dir_all(&path).is_ok()
        } else {
            std::fs::remove_file(&path).is_ok()
        };
        if ok {
            removed += 1;
        }
    }
    removed
}

/// The pid embedded in a scratch name, if it has one.
///
/// Names look like `terhub-hooks-1621106-install` or
/// `terhubd-env-2188986`: the creator's pid is the first all-digit
/// component. A component of one or two digits is far more likely to be
/// a counter than a pid, so those are skipped.
fn owner_pid(name: &str) -> Option<u32> {
    name.split(['-', '_', '.'])
        .find(|part| part.len() >= 3 && part.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|part| part.parse().ok())
}
