//! The idle reaper: a daemon that no client talks to retires on its own.
//!
//! `terhubd` has no parent-death signal, so a daemon whose test process
//! was killed outright would poll tmux forever. `TERHUBD_IDLE_TTL_SECS`
//! bounds that: after that many seconds without a single client
//! request, the daemon exits.
//!
//! Single test: owns the sandbox for this binary.
mod common;

use std::time::Duration;

/// Long enough that it cannot fire during the test itself, short enough
/// that the test does not take a minute.
const TTL: u64 = 3;

#[test]
fn daemon_exits_after_idle_ttl_with_no_client() {
    assert!(
        terhub::tmux::Tmux::available(),
        "tmux is required for this suite (CI installs it)"
    );
    let _sandbox = common::Sandbox::new("idle");

    // SAFETY: this test binary runs this single test; the value is
    // read by the daemon at startup, before it spawns anything.
    unsafe {
        std::env::set_var("TERHUBD_IDLE_TTL_SECS", TTL.to_string());
    }

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_terhubd"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn terhubd");

    assert!(
        common::wait_for(terhub::daemon::daemon_alive, 100),
        "daemon did not come up"
    );

    // Nobody talks to it, so it must go. The budget is generous
    // (TTL + 15s) because the daemon's first tick, the tmux probe and
    // the poll all have to line up before the clock is even read.
    let budget = Duration::from_secs(TTL + 15);
    let deadline = std::time::Instant::now() + budget;
    let mut exited = false;
    while std::time::Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => {
                exited = true;
                break;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(200)),
            Err(e) => panic!("try_wait failed: {e}"),
        }
    }
    let _ = child.kill();
    let _ = child.wait();

    assert!(
        exited,
        "daemon survived {TTL}s with no client request — the idle reaper did not fire"
    );
}
