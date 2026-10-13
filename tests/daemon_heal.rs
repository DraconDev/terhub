//! Self-heal: a dead daemon comes back on its own, with no manual
//! `terhub daemon start`. The regression this guards: nothing supervises
//! terhubd, so a terminal SIGHUP or the OOM killer could leave the tray
//! and every TUI showing "daemon down" with all badges cleared for the
//! rest of the session.
//!
//! Single test function: it owns TERHUB_RUNTIME_DIR and PATH for this
//! test binary.
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

fn wait_for(cond: impl Fn() -> bool, tries: u32) -> bool {
    for _ in 0..tries {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    cond()
}

#[test]
fn a_dead_daemon_revives_itself() {
    // Fail loudly without tmux (the backend.rs house rule): a silent early
    // return reports "passed" with zero assertions executed. CI installs
    // tmux; see .github/workflows/ci.yml.
    assert!(
        terhub::tmux::Tmux::available(),
        "tmux is required for this suite (CI installs it)"
    );
    let stamp = std::process::id();
    let runtime = std::env::temp_dir().join(format!("terhub-heal-{stamp}"));
    let _ = std::fs::remove_dir_all(&runtime);
    std::fs::create_dir_all(&runtime).unwrap();
    let data = std::env::temp_dir().join(format!("terhub-heal-data-{stamp}"));
    let _ = std::fs::remove_dir_all(&data);
    let config = std::env::temp_dir().join(format!("terhub-heal-{stamp}.toml"));
    let _ = std::fs::remove_file(&config);
    // A missing config keeps the daemon on defaults no matter what the
    // developer tuned locally.

    // spawn_daemon resolves the daemon binary beside the *current* exe,
    // which in a test binary is target/debug/deps — so point PATH at the
    // build directory that really holds terhubd (it sits beside the
    // `terhub` binary).
    let terhub_bin = PathBuf::from(env!("CARGO_BIN_EXE_terhub"));
    let bin_dir = terhub_bin.parent().expect("terhub binary dir").to_path_buf();
    let path = std::env::var("PATH").unwrap_or_default();
    let joined = format!("{}:{path}", bin_dir.display());

    // SAFETY: this test binary runs this single test; nothing else reads
    // these vars.
    unsafe {
        std::env::set_var("TERHUB_RUNTIME_DIR", &runtime);
        std::env::set_var("TERHUB_DATA_DIR", &data);
        std::env::set_var("TERHUB_CONFIG", &config);
        std::env::set_var(
            terhub::tmux::SOCKET_ENV,
            format!("terhub-heal-{stamp}"),
        );
        std::env::set_var("PATH", &joined);
    }

    let client = |args: &[&str]| {
        Command::new(&terhub_bin)
            .args(args)
            .output()
            .expect("run terhub")
    };

    // 1. Clean start through the real CLI.
    let out = client(&["daemon", "start"]);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(out.status.success(), "start failed: {stdout}");
    assert!(stdout.contains("started"), "unexpected start output: {stdout}");
    assert!(
        wait_for(terhub::daemon::daemon_alive, 100),
        "daemon never came up"
    );

    // 2. Stop it, exactly as a user or a crash-recovery would.
    let out = client(&["daemon", "stop"]);
    assert!(out.status.success(), "stop failed");
    assert!(
        wait_for(|| !terhub::daemon::daemon_alive(), 100),
        "daemon still alive after stop"
    );

    // 3. The self-heal: one ensure_running() brings it back, and it is
    //    usable — not merely a process that exists.
    assert!(
        terhub::daemon::ensure_running(),
        "ensure_running did not revive the daemon"
    );
    let snap = terhub::daemon::fetch_snapshot(Duration::from_secs(2))
        .expect("revived daemon answers a snapshot");
    // A snapshot proves the poll loop is running, not just the socket.
    let _ = snap.agents.len();

    // 4. Idempotent: healthy daemon, no second spawn needed.
    assert!(
        terhub::daemon::ensure_running(),
        "ensure_running reported a healthy daemon as dead"
    );

    // 5. Leave the machine as we found it.
    let _ = client(&["daemon", "stop"]);
    let _ = std::fs::remove_dir_all(&runtime);
    let _ = std::fs::remove_dir_all(&data);
    let _ = std::fs::remove_file(&config);
}
