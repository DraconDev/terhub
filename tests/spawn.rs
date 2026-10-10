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

/// STOP/CONT land asynchronously: poll /proc briefly instead of
/// asserting a single (racy under load) read.
fn wait_stopped(pid: u32, want: bool) -> bool {
    for _ in 0..200 {
        if terhub::proc::is_stopped(pid) == want {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    false
}

fn argv_lines(log: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn spawn_restart_pause_against_fake_tmux() {
    // Single test: phases share FAKE_ARGV, so they run sequentially.
    phase_new_window();
    phase_respawn();
    phase_pause_resume();
}

fn phase_new_window() {
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

fn phase_pause_resume() {
    // Real child, real signals: pause must show T in /proc, resume must
    // clear it. The child is reaped either way (Drop kills on unwind).
    struct Guard(std::process::Child);
    impl Drop for Guard {
        fn drop(&mut self) {
            // Bounded reap: `Child::wait()` blocks indefinitely on a
            // child stuck in uninterruptible D-state I/O (pass-2 F4).
            // A try_wait loop with kill+final try_wait is the same
            // pattern as tests/daemon.rs:41-48 and matches what
            // test teardown everywhere else in the suite does.
            for _ in 0..20 {
                if matches!(self.0.try_wait(), Ok(Some(_))) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            let _ = terhub::proc::resume_group(self.0.id());
            let _ = self.0.kill();
            for _ in 0..20 {
                if matches!(self.0.try_wait(), Ok(Some(_))) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }
    let child = std::process::Command::new("sleep")
        .arg("30")
        .stdin(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let guard = Guard(child);
    let pid = guard.0.id();
    assert!(!terhub::proc::is_stopped(pid));
    terhub::proc::pause_group(pid).unwrap();
    assert!(wait_stopped(pid, true), "sleep must show T after STOP");
    terhub::proc::resume_group(pid).unwrap();
    assert!(wait_stopped(pid, false), "sleep must run after CONT");
    assert!(terhub::proc::pause_group(0).is_err());
    assert!(terhub::proc::pause_group(1).is_err());
}

fn phase_respawn() {
    let (tmux, log) = harness("respawn");
    tmux.respawn_pane("%7", "pi --model fast").unwrap();
    let lines = argv_lines(&log);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].starts_with("respawn-pane -k -t %7"),
        "{}",
        lines[0]
    );
    assert!(lines[0].contains("pi --model fast"), "{}", lines[0]);
    assert!(tmux.respawn_pane("%7", "  ").is_err());
}
