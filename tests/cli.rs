//! Black-box CLI dispatch coverage: exit codes and error paths for hook,
//! status, daemon, and the destructive send/kill/zellij commands. Each test
//! isolates via per-child env (own runtime dir, PATH without tmux), so the
//! suite is hermetic and parallel-safe: no daemon, tmux, or zellij needed.
use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_terhub")
}

fn runtime_dir(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("terhub-cli-{}-{}", std::process::id(), tag))
}

fn run_isolated(tag: &str, args: &[&str]) -> Output {
    let rt = runtime_dir(tag);
    let _ = std::fs::remove_dir_all(&rt);
    std::fs::create_dir_all(&rt).unwrap();
    let out = Command::new(bin())
        .args(args)
        // No daemon here; no tmux/zellij on this PATH either.
        .env("TERHUB_RUNTIME_DIR", &rt)
        .env("PATH", "/nonexistent-terhub-test")
        .output()
        .expect("spawn terhub");
    let _ = std::fs::remove_dir_all(&rt);
    out
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

#[test]
fn hook_bad_state_exits_1() {
    let out = run_isolated(
        "hookbad",
        &["hook", "--agent", "pi", "--pane", "%1", "--state", "bogus"],
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("bad state"), "{}", stderr(&out));
}

#[test]
fn hook_without_daemon_still_exits_0() {
    // Fire-and-forget: hooks must never break the agent they observe.
    let out = run_isolated(
        "hookok",
        &[
            "hook", "--agent", "pi", "--pane", "%1", "--state", "waiting",
        ],
    );
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn status_without_daemon_exits_1() {
    let out = run_isolated("status", &["status"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("daemon not running"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn daemon_status_without_daemon_exits_1() {
    let out = run_isolated("dstatus", &["daemon", "status"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stdout(&out).contains("not running"), "{}", stdout(&out));
}

#[test]
fn send_without_tmux_exits_1() {
    let out = run_isolated("send", &["send", "%1", "hi"]);
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn kill_pane_without_tmux_exits_1() {
    let out = run_isolated("killpane", &["kill-pane", "%1"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stdout(&out).is_empty(), "{}", stdout(&out));
}

#[test]
fn zellij_list_without_binary_exits_1() {
    let out = run_isolated("zlist", &["zellij", "list"]);
    assert_eq!(out.status.code(), Some(1));
}
