//! Spawn/restart/pause coverage against a fake tmux that records argv.
use std::os::unix::fs::PermissionsExt;
use terhub::tmux::Tmux;

const FAKE_TMUX: &str = r#"#!/bin/sh
echo "$@" >> "$FAKE_ARGV"
case "$1" in
  new-window) echo "%99";;
  list-sessions) exit 0;;
  list-panes) exit 0;;
esac
exit 0
"#;

fn harness(tag: &str) -> (Tmux, std::path::PathBuf) {
    let base = std::env::temp_dir().join(format!("terhub-spawn-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let bin = base.join("tmux");
    std::fs::write(&bin, FAKE_TMUX).unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    // SAFETY: tests in this binary run sequentially on threads, but each
    // uses a unique tag dir; env is only read by the child script.
    unsafe {
        std::env::set_var("FAKE_ARGV", base.join("argv.log"));
    }
    (Tmux::test(bin), base.join("argv.log"))
}

fn argv_lines(log: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn new_window_targets_session_and_reports_pane() {
    let (tmux, log) = harness("newwin");
    let pane = tmux
        .new_window("dev", "agent-x", Some("/tmp"), Some("pi --model fast"))
        .unwrap();
    assert_eq!(pane, "%99");
    let lines = argv_lines(&log);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("new-window"), "{}", lines[0]);
    assert!(lines[0].contains("-t dev"), "{}", lines[0]);
    assert!(lines[0].contains("-n agent-x"), "{}", lines[0]);
    assert!(lines[0].contains("-c /tmp"), "{}", lines[0]);
    assert!(lines[0].contains("pi --model fast"), "{}", lines[0]);
    assert!(tmux.new_window("", "x", None, None).is_err());
    assert!(tmux.new_window("dev", "  ", None, None).is_err());
}

#[test]
fn respawn_pane_kills_and_reruns() {
    let (tmux, log) = harness("respawn");
    tmux.respawn_pane("%7", "pi --model fast").unwrap();
    let lines = argv_lines(&log);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].starts_with("respawn-pane -k -t %7"), "{}", lines[0]);
    assert!(lines[0].contains("pi --model fast"), "{}", lines[0]);
    assert!(tmux.respawn_pane("%7", "  ").is_err());
}
