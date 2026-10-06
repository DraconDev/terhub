//! Native terminal backends against fake `qdbus`/`kitty`/`wezterm` on PATH.
//!
//! Single test: owns PATH for this binary.
use std::os::unix::fs::PermissionsExt;

const FAKE_QDBUS: &str = r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo "fake qdbus"; exit 0; fi
if [ $# -eq 0 ]; then printf ':1.84\n org.kde.konsole-999\n'; exit 0; fi
SVC="$1"; PATH_="$2"; METHOD="$3"; shift 3
base="${METHOD##*.}"
case "$PATH_::$base" in
  "/Windows::Introspect") printf '<node><node name="1"/></node>\n';;
  "/Windows/1::sessionList") printf '6\n7\n';;
  "/Windows/1::currentSession") printf '6\n';;
  "/Sessions/6::title") printf 'tab six\n';;
  "/Sessions/7::title") printf 'tab seven\n';;
  "/Sessions/6::processId") printf '1001\n';;
  "/Sessions/6::foregroundProcessId") printf '1002\n';;
  "/Sessions/7::processId") printf '1003\n';;
  "/Sessions/7::foregroundProcessId") printf '1003\n';;
  "/Sessions/6::getAllDisplayedText") printf 'hello  \nworld\n';;
  "/Sessions/6::sendText"|"/Sessions/7::sendText"|"/Windows/1::setCurrentSession"|"/Windows/1::newSession")
    echo "$PATH_ $base $*" >> "$FAKE_LOG";;
  *) echo "unexpected: $SVC $PATH_ $METHOD $*" >&2; exit 1;;
esac
"#;

const FAKE_KITTY: &str = r#"#!/bin/sh
if [ "$1" != "@" ]; then echo "want @" >&2; exit 1; fi
shift
SUB="$1"; shift
case "$SUB" in
  ls) cat "$FAKE_KITTY_LS";;
  get-text) printf 'k1\nk2\nk3\n';;
  send-text|focus-window|launch) echo "kitty $SUB $*" >> "$FAKE_LOG";;
  *) echo "unexpected: $SUB" >&2; exit 1;;
esac
"#;

const FAKE_WEZTERM: &str = r#"#!/bin/sh
if [ "$1" = "cli" ]; then shift; fi
if [ "$1" = "--no-auto-start" ]; then shift; else echo "missing --no-auto-start" >&2; exit 1; fi
SUB="$1"; shift
case "$SUB" in
  list) cat "$FAKE_WEZTERM_LIST";;
  get-text) printf '\033[31mred\033[0m\nplain\n';;
  send-text|activate-pane|spawn) echo "wezterm $SUB $*" >> "$FAKE_LOG";;
  *) echo "unexpected: $SUB" >&2; exit 1;;
esac
"#;

const KITTY_LS: &str = r#"[{"id":1,"tabs":[{"id":2,"title":"t","layout":"tall","windows":[{"id":3,"is_focused":true,"title":"w","pid":2001,"cwd":"/tmp"}]}]}]"#;
const WEZTERM_LIST: &str = r#"[{"window_id":0,"tab_id":1,"pane_id":7,"title":"wtab"}]"#;

fn write_exe(path: &std::path::Path, text: &str) {
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn native_backends_against_fakes() {
    use terhub::native as n;
    let base = std::env::temp_dir().join(format!("terhub-native-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let bin = base.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_exe(&bin.join("qdbus"), FAKE_QDBUS);
    write_exe(&bin.join("kitty"), FAKE_KITTY);
    write_exe(&bin.join("wezterm"), FAKE_WEZTERM);
    let log = base.join("calls.log");
    std::fs::write(&log, "").unwrap();
    std::fs::write(base.join("kitty.json"), KITTY_LS).unwrap();
    std::fs::write(base.join("wezterm.json"), WEZTERM_LIST).unwrap();
    // SAFETY: single test in this binary.
    unsafe {
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
        std::env::set_var("PATH", path);
        std::env::set_var("FAKE_LOG", &log);
        std::env::set_var("FAKE_KITTY_LS", base.join("kitty.json"));
        std::env::set_var("FAKE_WEZTERM_LIST", base.join("wezterm.json"));
        std::env::remove_var("TERHUB_KITTY_TO");
    }

    // Enumeration across all three backends.
    let reports = n::list_all();
    assert_eq!(reports.len(), 3);
    let konsole = reports.iter().find(|r| r.backend == "konsole").unwrap();
    assert!(konsole.error.is_none(), "{:?}", konsole.error);
    assert_eq!(konsole.tabs.len(), 2);
    assert_eq!(konsole.tabs[0].id, "konsole:999:6");
    assert_eq!(konsole.tabs[0].title, "tab six");
    assert_eq!(konsole.tabs[0].pid, 1001);
    assert_eq!(konsole.tabs[0].fg_pid, 1002);
    assert!(konsole.tabs[0].active);
    assert!(!konsole.tabs[1].active);
    let kitty = reports.iter().find(|r| r.backend == "kitty").unwrap();
    assert_eq!(kitty.tabs.len(), 1);
    assert_eq!(kitty.tabs[0].id, "kitty:3");
    assert_eq!(kitty.tabs[0].pid, 2001);
    let wezterm = reports.iter().find(|r| r.backend == "wezterm").unwrap();
    assert_eq!(wezterm.tabs.len(), 1);
    assert_eq!(wezterm.tabs[0].id, "wezterm:7");
    assert_eq!(wezterm.tabs[0].title, "wtab");

    // Capture trims Konsole padding and honors the tail count.
    assert_eq!(n::capture("konsole:999:6", 10).unwrap(), "hello\nworld\n");
    assert_eq!(n::capture("kitty:3", 1).unwrap(), "k3\n");
    assert!(n::capture("wezterm:7", 5).unwrap().contains("red"));
    assert!(n::capture("konsole:999:99", 5).is_err());

    // Mutating calls reach the fakes with the right argv.
    n::send_text("konsole:999:6", "hi", true).unwrap();
    n::focus("konsole:999:7").unwrap();
    n::spawn("konsole").unwrap();
    n::send_text("kitty:3", "hi", false).unwrap();
    n::focus("kitty:3").unwrap();
    n::send_text("wezterm:7", "hi", true).unwrap();
    n::focus("wezterm:7").unwrap();
    let log_text = std::fs::read_to_string(&log).unwrap();
    assert!(
        log_text.contains("sendText hi"),
        "missing konsole send:\n{log_text}"
    );
    assert!(
        log_text.contains("setCurrentSession 7"),
        "missing focus:\n{log_text}"
    );
    assert!(
        log_text.contains("newSession"),
        "missing spawn:\n{log_text}"
    );
    assert!(
        log_text.contains("kitty send-text --match id:3 hi"),
        "missing kitty send:\n{log_text}"
    );
    assert!(
        log_text.contains("kitty focus-window --match id:3"),
        "missing kitty focus:\n{log_text}"
    );
    assert!(
        log_text.contains("wezterm send-text --pane-id 7 --no-paste hi"),
        "missing wezterm send:\n{log_text}"
    );
    assert!(
        log_text.contains("wezterm activate-pane --pane-id 7"),
        "missing wezterm focus:\n{log_text}"
    );

    // /proc helpers read the live process table.
    let me = std::process::id();
    assert!(n::proc_comm(me).is_some_and(|c| !c.is_empty()));
    assert!(n::proc_cwd(me).is_some_and(|c| !c.is_empty()));
    assert!(n::proc_comm(0).is_none());

    let _ = std::fs::remove_dir_all(&base);
}
