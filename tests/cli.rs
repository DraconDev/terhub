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
fn send_template_unknown_name_exits_1_without_tmux() {
    // Template resolution runs before any tmux call, so the failure names
    // the template even with tmux missing.
    let out = run_isolated("sendtpl", &["send", "%1", "--template", "nope"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("nope"), "{}", stderr(&out));
}

#[test]
fn send_text_and_template_together_exits_1() {
    let out = run_isolated("sendboth", &["send", "%1", "hi", "--template", "continue"]);
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn send_needs_text_or_template() {
    let out = run_isolated("sendnone", &["send", "%1"]);
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

fn run_with_ledger(tag: &str, args: &[&str], ledger: &str) -> Output {
    let rt = runtime_dir(tag);
    let _ = std::fs::remove_dir_all(&rt);
    let data = rt.join("data");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("activity.jsonl"), ledger).unwrap();
    let out = Command::new(bin())
        .args(args)
        .env("TERHUB_RUNTIME_DIR", &rt)
        .env("TERHUB_DATA_DIR", &data)
        .env("PATH", "/nonexistent-terhub-test")
        .output()
        .expect("spawn terhub");
    let _ = std::fs::remove_dir_all(&rt);
    out
}

const SEED_LEDGER: &str = "{\"t\":100,\"kind\":\"agent\",\"pane\":\"%1\",\"agent\":\"pi\",\"from\":\"idle\",\"to\":\"working\",\"session\":\"s\",\"window\":\"w\"}\n{\"t\":200,\"kind\":\"agent\",\"pane\":\"%1\",\"agent\":\"pi\",\"from\":\"working\",\"to\":\"waiting\",\"session\":\"s\",\"window\":\"w\"}\n{\"t\":150,\"kind\":\"human\",\"pane\":\"%1\",\"event\":\"focus\"}\n{\"t\":250,\"kind\":\"human\",\"event\":\"detach\"}\n";

#[test]
fn report_json_flag_emits_structured_report() {
    let out = run_with_ledger(
        "repjson",
        &["report", "--since", "all", "--json"],
        SEED_LEDGER,
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid json");
    assert_eq!(v["by_agent"][0]["key"], "pi");
    assert_eq!(v["by_agent"][0]["work"], 100);
    assert_eq!(v["foreground"][0]["pane"], "%1");
    assert_eq!(v["foreground"][0]["secs"], 100);
}

#[test]
fn report_markdown_flag_emits_tables() {
    let out = run_with_ledger(
        "repmd",
        &["report", "--since", "all", "--markdown"],
        SEED_LEDGER,
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("## by agent"), "missing section:\n{text}");
    assert!(text.contains("| pi |"), "missing row:\n{text}");
    assert!(
        text.contains("## foreground"),
        "missing foreground:\n{text}"
    );
}

#[test]
fn report_text_includes_foreground_section() {
    let out = run_with_ledger("repfg", &["report", "--since", "all"], SEED_LEDGER);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("foreground"), "missing section:\n{text}");
    assert!(text.contains("%1"), "missing pane:\n{text}");
}

#[test]
fn timeline_rejects_bad_view() {
    let out = run_isolated("tlview", &["timeline", "--view", "bogus"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
}

#[test]
fn native_list_without_terminals_exits_0() {
    let out = run_isolated("natlist", &["native", "list"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stdout(&out).contains("no native tabs"), "{}", stdout(&out));
}

#[test]
fn native_list_json_without_terminals_is_empty_array() {
    let out = run_isolated("natjson", &["native", "list", "--json"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid json");
    assert_eq!(v, serde_json::Value::Array(vec![]));
}

#[test]
fn native_spawn_unknown_backend_exits_1() {
    let out = run_isolated("natspawn", &["native", "spawn", "gnome"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
}

#[test]
fn hook_prompt_flag_is_accepted() {
    // Fire-and-forget: exit 0 even with no daemon; the flag must parse.
    let out = run_isolated(
        "hookprompt",
        &[
            "hook", "--agent", "pi", "--pane", "%1", "--state", "working", "--prompt", "hi",
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
}

const PROMPT_LEDGER: &str = "{\"t\":100,\"kind\":\"prompt\",\"pane\":\"%1\",\"agent\":\"pi\",\"summary\":\"first\"}\n{\"t\":200,\"kind\":\"prompt\",\"pane\":\"%2\",\"agent\":\"pi\",\"summary\":\"second\"}\n{\"t\":300,\"kind\":\"prompt\",\"pane\":\"%1\",\"agent\":\"pi\",\"summary\":\"third\"}\n";

#[test]
fn recap_lists_prompts_newest_first() {
    let out = run_with_ledger("recap", &["recap", "--since", "all"], PROMPT_LEDGER);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let text = stdout(&out);
    let i3 = text.find("third").expect("third");
    let i2 = text.find("second").expect("second");
    let i1 = text.find("first").expect("first");
    assert!(i3 < i2 && i2 < i1, "wrong order:\n{text}");
}

#[test]
fn recap_filters_by_pane_and_limit() {
    let out = run_with_ledger(
        "recapf",
        &["recap", "--since", "all", "--pane", "%1", "--limit", "1"],
        PROMPT_LEDGER,
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("third"), "missing third:\n{text}");
    assert!(!text.contains("first"), "limit ignored:\n{text}");
    assert!(!text.contains("second"), "pane filter ignored:\n{text}");
}

#[test]
fn activity_kind_prompt_lists_prompt_entries() {
    let out = run_with_ledger(
        "actprompt",
        &["activity", "--since", "all", "--kind", "prompt"],
        PROMPT_LEDGER,
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stdout(&out).contains("third"), "{}", stdout(&out));
}

#[test]
fn send_capture_focus_route_native_ids_to_backends() {
    // No tmux and no qdbus on PATH: the failure must name the native
    // backend, proving the id never went down the tmux path.
    let out = run_isolated("natsend", &["send", "konsole:1:2", "hi"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("qdbus"), "{}", stderr(&out));
    let out = run_isolated("natcap", &["capture", "konsole:1:2"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("qdbus"), "{}", stderr(&out));
    let out = run_isolated("natfocus", &["focus", "konsole:1:2"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("qdbus"), "{}", stderr(&out));
}

#[test]
fn init_dry_run_lists_steps_without_acting() {
    let out = run_isolated("initdry", &["init", "--dry-run"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let text = stdout(&out);
    for step in ["config", "hooks", "daemon"] {
        assert!(text.contains(step), "missing {step}:\n{text}");
    }
}
