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

fn run_with_env(tag: &str, args: &[&str], env: &[(&str, &str)]) -> Output {
    let rt = runtime_dir(tag);
    let _ = std::fs::remove_dir_all(&rt);
    std::fs::create_dir_all(&rt).unwrap();
    let mut cmd = Command::new(bin());
    cmd.args(args)
        .env("TERHUB_RUNTIME_DIR", &rt)
        .env("PATH", "/nonexistent-terhub-test");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("spawn terhub");
    let _ = std::fs::remove_dir_all(&rt);
    out
}

#[test]
fn hooks_install_status_uninstall_roundtrip() {
    let base = std::env::temp_dir().join(format!("terhub-cli-hooks-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let claude = base.join("settings.json").display().to_string();
    let pi = base.join("terhub.ts").display().to_string();
    let env = [
        ("TERHUB_CLAUDE_SETTINGS", claude.as_str()),
        ("TERHUB_PI_EXTENSION", pi.as_str()),
    ];
    let out = run_with_env("hstat0", &["hooks", "status"], &env);
    assert_eq!(out.status.code(), Some(0));
    assert!(
        stdout(&out).contains("claude: not installed"),
        "{}",
        stdout(&out)
    );
    assert!(
        stdout(&out).contains("pi: not installed"),
        "{}",
        stdout(&out)
    );
    let out = run_with_env("hinst", &["hooks", "install"], &env);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("claude: installed"),
        "{}",
        stdout(&out)
    );
    assert!(stdout(&out).contains("pi: installed"), "{}", stdout(&out));
    let out = run_with_env("hstat1", &["hooks", "status"], &env);
    assert!(
        stdout(&out).contains("claude: installed"),
        "{}",
        stdout(&out)
    );
    assert!(stdout(&out).contains("pi: installed"), "{}", stdout(&out));
    let out = run_with_env("huninst", &["hooks", "uninstall", "claude"], &env);
    assert!(stdout(&out).contains("claude: removed"), "{}", stdout(&out));
    assert!(!stdout(&out).contains("pi: removed"), "{}", stdout(&out));
    let out = run_with_env("huninst2", &["hooks", "uninstall"], &env);
    assert!(stdout(&out).contains("pi: removed"), "{}", stdout(&out));
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn hook_summary_stdin_reads_message() {
    use std::io::Write;
    let rt = runtime_dir("hookstdin");
    let _ = std::fs::remove_dir_all(&rt);
    std::fs::create_dir_all(&rt).unwrap();
    let mut child = Command::new(bin())
        .args([
            "hook",
            "--agent",
            "pi",
            "--pane",
            "%1",
            "--state",
            "waiting",
            "--summary-stdin",
        ])
        .env("TERHUB_RUNTIME_DIR", &rt)
        .env("PATH", "/nonexistent-terhub-test")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn terhub");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(br#"{"message": "Approve plan?"}"#)
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    // --summary and --summary-stdin conflict.
    let out = run_isolated(
        "hookconflict",
        &[
            "hook",
            "--agent",
            "pi",
            "--pane",
            "%1",
            "--state",
            "waiting",
            "--summary",
            "x",
            "--summary-stdin",
        ],
    );
    assert_eq!(out.status.code(), Some(2));
    let _ = std::fs::remove_dir_all(&rt);
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
