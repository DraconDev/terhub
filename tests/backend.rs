use terhub::tmux::Tmux;

/// Backend tests need a real tmux: fail loudly without one instead of
/// green-but-vacuous early returns (CI installs tmux; see the workflow).
fn require_tmux() -> Tmux {
    assert!(
        Tmux::available(),
        "tmux is required for backend tests (install tmux to run them)"
    );
    Tmux::new().unwrap()
}

#[test]
fn list_sessions_returns_vec() {
    let tmux = require_tmux();
    let sessions = tmux.list_sessions().unwrap();
    // Live server has at least the user's own session; empty is also valid.
    assert!(sessions.iter().all(|s| !s.name.is_empty()));
}

#[test]
fn new_and_kill_roundtrip() {
    let tmux = require_tmux();
    let name = format!("terhub-test-{}", std::process::id());
    if tmux.session_exists(&name) {
        tmux.kill_session(&name).unwrap();
    }
    let created = tmux.new_session(&name, None, None).unwrap();
    assert_eq!(created.name, name);
    assert!(tmux.session_exists(&name));
    tmux.kill_session(&name).unwrap();
    assert!(!tmux.session_exists(&name));
}

#[test]
fn pane_discovery_capture_and_kill() {
    use std::process::Command;
    let tmux = require_tmux();
    let name = format!("terhub-pane-{}", std::process::id());
    if tmux.session_exists(&name) {
        tmux.kill_session(&name).unwrap();
    }
    tmux.new_session(&name, None, None).unwrap();
    // window 0: split into 2 panes; window 1: single pane.
    tmux.split_window(&name).unwrap();
    tmux.new_window(&name, "second", None, None).unwrap();
    let panes = tmux.list_panes(&name).unwrap();
    assert_eq!(panes.len(), 3, "panes: {panes:?}");
    let windows: std::collections::HashSet<&str> =
        panes.iter().map(|p| p.window_id.as_str()).collect();
    assert_eq!(windows.len(), 2);
    assert!(panes.iter().any(|p| p.active));
    assert_eq!(tmux.session_of_pane(&panes[0].id).unwrap(), name);
    assert!(tmux.capture_pane(&panes[0].id, 5).is_ok());
    tmux.kill_pane(&panes[0].id).unwrap();
    assert_eq!(tmux.list_panes(&name).unwrap().len(), 2);
    tmux.kill_session(&name).unwrap();
    assert!(!tmux.session_exists(&name));
}

#[test]
fn sidebar_filter_tracks_selection() {
    use terhub::app::App;
    use terhub::config::AppConfig;
    let tmux = require_tmux();
    let name = format!("terhub-detail-{}", std::process::id());
    if tmux.session_exists(&name) {
        tmux.kill_session(&name).unwrap();
    }
    tmux.new_session(&name, None, None).unwrap();
    let mut app = App::new(AppConfig::default());
    app.refresh(&tmux);
    let idx = app
        .sessions
        .iter()
        .position(|s| s.name == name)
        .expect("new session listed");
    // All shows everything; the session row narrows to its panes.
    assert!(!app.filtered.is_empty());
    app.select(idx + 1);
    app.refresh_filtered();
    assert!(!app.filtered.is_empty());
    assert!(app.filtered.iter().all(|p| p.session == name));
    app.select(0);
    app.refresh_filtered();
    assert!(app.filtered.len() >= app.panes.len().min(1));
    tmux.kill_session(&name).unwrap();
}

#[test]
fn swap_panes_flips_positions() {
    use std::process::Command;
    let tmux = require_tmux();
    let name = format!("terhub-swap-{}", std::process::id());
    if tmux.session_exists(&name) {
        tmux.kill_session(&name).unwrap();
    }
    tmux.new_session(&name, None, None).unwrap();
    tmux.split_window(&name).unwrap();
    let before = tmux.list_panes(&name).unwrap();
    assert_eq!(before.len(), 2);
    let (id0, id1) = (before[0].id.clone(), before[1].id.clone());
    tmux.swap_panes(&id0, &id1).unwrap();
    let after = tmux.list_panes(&name).unwrap();
    // Same panes, exchanged indices.
    assert_eq!(after[0].id, id1);
    assert_eq!(after[1].id, id0);
    // Foreground query answers when a client is attached (best-effort).
    let _ = tmux.foreground_pane();
    tmux.kill_session(&name).unwrap();
}

#[test]
fn window_of_pane_resolves() {
    use terhub::attend::window_of_pane;
    let tmux = require_tmux();
    let name = format!("terhub-win-{}", std::process::id());
    if tmux.session_exists(&name) {
        tmux.kill_session(&name).unwrap();
    }
    tmux.new_session(&name, None, None).unwrap();
    let panes = tmux.list_panes(&name).unwrap();
    assert_eq!(panes.len(), 1);
    let window = window_of_pane(&tmux, &panes[0].id).unwrap();
    assert_eq!(window, panes[0].window_id);
    assert!(window.starts_with('@'));
    tmux.kill_session(&name).unwrap();
}

#[test]
fn empty_name_rejected() {
    let tmux = require_tmux();
    assert!(tmux.new_session("  ", None, None).is_err());
    assert!(tmux.rename_session("whatever", " ").is_err());
}

#[test]
fn nav_failure_stays_in_tui_with_status() {
    use terhub::app::{App, AppEvent};
    use terhub::config::AppConfig;
    let tmux = require_tmux();
    let mut app = App::new(AppConfig::default());
    app.refresh(&tmux);
    // A jump to a dead pane stays in-process (no exit, no panic) and
    // reports the failure as a status line.
    app.do_nav(
        &tmux,
        AppEvent::FocusPane("%999999-no-such-pane".to_string()),
    );
    let status = app.status.clone().unwrap_or_default();
    assert!(status.contains("failed"), "status: {status}");
    // Same contract for a dead session attach.
    app.do_nav(
        &tmux,
        AppEvent::Attach("no-such-terhub-session".to_string()),
    );
    let status = app.status.clone().unwrap_or_default();
    assert!(status.contains("failed"), "status: {status}");
}
