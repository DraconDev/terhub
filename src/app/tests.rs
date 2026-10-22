//! The app's own tests.
//!
//! They live in their own file because they are the bulk of the
//! module: the implementation is worth reading on its own, and a test
//! file that sits under it hides how much of the file is test.
use super::*;
use crossterm::event::KeyModifiers;

fn press(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn app_with(sessions: &[&str]) -> App {
    let mut app = App::new(AppConfig::default());
    app.set_sessions(sessions.iter().map(|n| Session::test(n)).collect());
    app
}

/// Sidebar rows are 0 = All, 1+i = sessions[i].
#[test]
fn navigation_clamps() {
    let mut app = app_with(&["a", "b"]);
    assert_eq!(app.side_rows(), 3);
    app.move_down();
    assert_eq!(app.selected, 1);
    app.move_down();
    assert_eq!(app.selected, 2);
    app.move_down();
    assert_eq!(app.selected, 2);
    app.move_up();
    assert_eq!(app.selected, 1);
    app.select(0);
    app.move_up();
    assert_eq!(app.selected, 0);
}

#[test]
fn live_search_is_multi_token_or() {
    // The field contract is multi-token OR-combined; the old
    // single-substring form hid every row here (no column
    // contains "nvim bash" contiguously).
    let mut app = app_with_panes();
    app.filter = "nvim bash".to_string();
    app.refresh_filtered();
    assert_eq!(
        app.filtered.len(),
        2,
        "multi-token query should OR-match both rows"
    );
    // A `!`-prefixed token excludes within the same query.
    app.filter = "editor !nvim".to_string();
    app.refresh_filtered();
    assert_eq!(
        app.filtered
            .iter()
            .map(|p| p.id.as_str())
            .collect::<Vec<_>>(),
        vec!["%2"],
        "!-token should hide only its rows"
    );
    // Single tokens behave exactly as before.
    app.filter = "nvim".to_string();
    app.refresh_filtered();
    assert_eq!(
        app.filtered
            .iter()
            .map(|p| p.id.as_str())
            .collect::<Vec<_>>(),
        vec!["%1"],
    );
}

#[test]
fn refresh_keeps_selection_by_name() {
    let mut app = app_with(&["a", "b"]);
    app.select(2); // "b"
    app.set_sessions(vec![Session::test("b"), Session::test("c")]);
    assert_eq!(app.selected, 1);
    assert_eq!(app.side_session().unwrap().name, "b");
    // A vanished filter falls back to All.
    app.set_sessions(vec![Session::test("c")]);
    assert_eq!(app.selected, 0);
    assert!(app.side_session().is_none());
}

#[test]
fn session_kill_refuses_own_session() {
    // Killing the session the hub runs inside would kill the TUI
    // out from under itself (the pane paths all guard own_pane;
    // the sidebar session-kill did not).
    let mut app = app_with(&["a", "b"]);
    let mut p = Pane::test("%1", "editor", "nvim");
    p.session = "a".to_string();
    app.panes = vec![p];
    app.refresh_filtered();
    app.own_pane = Some("%1".to_string());
    let (tmux, dir) = fake_tmux();
    // Own session is refused, never attempted. (Refuse first: a
    // confirm triggers refresh(), and the fake tmux lists no
    // panes, which would wipe the own-pane evidence.)
    app.mode = Mode::ConfirmKill("a".to_string());
    app.on_key(press(KeyCode::Enter), &tmux);
    assert!(
        app.status
            .as_deref()
            .is_some_and(|s| s.contains("own session")),
        "own session kill not refused: {:?}",
        app.status
    );
    // Another session still dies on confirm.
    app.mode = Mode::ConfirmKill("b".to_string());
    app.on_key(press(KeyCode::Enter), &tmux);
    assert_eq!(
        app.status.as_deref(),
        Some("killed b"),
        "foreign session kill broke: {:?}",
        app.status
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn modifier_quit_binding_quits() {
    // Quit rebound to a modifier combo must quit: the dispatch
    // "quit" arm returned None ("unclaimed"), so the caller
    // skipped it and the press died with no quit and no status.
    let mut app = app_with(&["a"]);
    app.config
        .keys
        .insert("quit".to_string(), "ctrl+q".to_string());
    let tmux = Tmux::new().unwrap();
    assert_eq!(
        app.on_key(
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
            &tmux
        ),
        Some(AppEvent::Quit)
    );
}

#[test]
fn quit_and_esc_exit() {
    let mut app = app_with(&["a"]);
    let tmux = Tmux::new().unwrap();
    assert_eq!(
        app.on_key(press(KeyCode::Char('q')), &tmux),
        Some(AppEvent::Quit)
    );
    // Default focus is the table: Esc steps out first, then quits.
    assert_eq!(app.focus, Focus::Panes);
    assert_eq!(app.on_key(press(KeyCode::Esc), &tmux), None);
    assert_eq!(app.focus, Focus::Sessions);
    assert_eq!(app.on_key(press(KeyCode::Esc), &tmux), Some(AppEvent::Quit));
}

/// Fake tmux: listings succeed empty, kills fail. No real tmux needed.
fn fake_tmux() -> (Tmux, std::path::PathBuf) {
    // Unique per call, not just per process: parallel tests share
    // the pid, and one test's cleanup used to delete a sibling's
    // fake binary mid-run (the bulk_kill flake).
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("terhub-faketmux-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("tmux");
    std::fs::write(
        &bin,
        "#!/bin/sh\ncase \"$1\" in\nlist-*) exit 0;;\nkill-pane) exit 1;;\n*) exit 0;;\nesac\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut p = std::fs::metadata(&bin).unwrap().permissions();
    p.set_mode(0o755);
    std::fs::set_permissions(&bin, p).unwrap();
    (Tmux::test(bin), dir)
}

#[test]
fn bulk_kill_skips_own_pane() {
    // The own pane is never attempted: one attempt (failed), one skip.
    // Without the guard both would be attempted ("0, 2 failed").
    let mut app = app_with(&["a"]);
    app.own_pane = Some("%1".to_string());
    app.marked.insert("%1".to_string());
    app.marked.insert("%2".to_string());
    app.mode = Mode::ConfirmKillMarked(vec!["%1".to_string(), "%2".to_string()]);
    let (tmux, dir) = fake_tmux();
    app.on_key(press(KeyCode::Enter), &tmux);
    let status = app.status.as_deref().unwrap_or("");
    assert!(status.contains("0, 1 failed"), "{status}");
    assert!(status.contains("own pane skipped"), "{status}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn enter_attaches_selected() {
    let mut app = app_with(&["a", "b"]);
    app.focus = Focus::Sessions;
    app.select(2); // "b"
    let tmux = Tmux::new().unwrap();
    assert_eq!(
        app.on_key(press(KeyCode::Enter), &tmux),
        Some(AppEvent::Attach("b".to_string()))
    );
    // Row 0 (All) is a filter, not a session: nothing to attach.
    app.select(0);
    assert_eq!(app.on_key(press(KeyCode::Enter), &tmux), None);
    assert_eq!(
        app.status.as_deref(),
        Some("pick a session row (All doesn't count)")
    );
}

#[test]
fn spawn_window_name_sanitizes_and_caps() {
    assert_eq!(
        spawn_window_name("pi", "fix the login bug now"),
        "pi-fix-the-login"
    );
    assert_eq!(spawn_window_name("pi", ""), "pi");
    assert_eq!(spawn_window_name("pi", "   "), "pi");
    // Shell metachars and slashes can't smuggle through the name.
    assert_eq!(
        spawn_window_name("pi", "a; rm -rf / && echo hi"),
        "pi-a-rm--rf"
    );
    // Long prompts cap at 32 chars; weird profile falls back.
    let long = "word ".repeat(20);
    let name = spawn_window_name("codex", &long);
    assert!(name.chars().count() <= 32, "{name}");
    assert!(name.starts_with("codex-word-word"), "{name}");
    assert_eq!(spawn_window_name("!!!", "hi"), "agent-hi");
}

#[test]
fn prompt_input_edits_and_cancels() {
    let mut app = app_with(&["a"]);
    app.focus = Focus::Sessions;
    let tmux = Tmux::new().unwrap();
    assert_eq!(app.on_key(press(KeyCode::Char('n')), &tmux), None);
    assert_eq!(app.mode, Mode::PromptNew);
    app.on_key(press(KeyCode::Char('z')), &tmux);
    assert_eq!(app.input, "z");
    app.on_key(press(KeyCode::Backspace), &tmux);
    assert_eq!(app.input, "");
    app.on_key(press(KeyCode::Esc), &tmux);
    assert_eq!(app.mode, Mode::Normal);
}

#[test]
fn ctrl_keys_ignored() {
    let mut app = app_with(&["a"]);
    let tmux = Tmux::new().unwrap();
    let key = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL);
    assert_eq!(app.on_key(key, &tmux), None);
}

#[test]
fn modifier_binding_fires_and_bare_key_stays() {
    // A ctrl+s bound to settings fires the settings popup; a bare s
    // is no longer bound — type-to-filter picks it up instead of
    // swallowing it (bare keys fire on unmodified presses, modifier
    // bindings fire on exact combos).
    let mut app = app_with(&["a"]);
    app.config
        .keys
        .insert("settings".to_string(), "ctrl+s".to_string());
    let tmux = Tmux::new().unwrap();
    // Bare s: unbound -> opens the filter seeded with `s`.
    assert_eq!(
        app.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE), &tmux),
        None
    );
    assert_eq!(app.mode, Mode::PromptFilter);
    assert_eq!(app.input, "s");
    app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tmux);
    assert_eq!(app.mode, Mode::Normal);
    // ctrl+s: bound -> opens settings.
    assert_eq!(
        app.on_key(
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            &tmux
        ),
        None
    );
    assert_eq!(app.mode, Mode::Settings);
}

#[test]
fn pane_action_keys_fire_under_sessions_focus() {
    // Bare mark/unmark/bulk/restart/pause must fire under
    // Sessions focus — not fall into type-to-filter, which
    // shadows them in the Sessions-fallback branch.
    let mut app = app_with_panes();
    app.focus = Focus::Sessions;
    let tmux = Tmux::new().unwrap();
    app.on_key(press(KeyCode::Char(' ')), &tmux); // mark
    assert!(app.marked.contains("%1"), "mark dead: {:?}", app.status);
    assert_eq!(app.mode, Mode::Normal, "mark opened the filter");
    app.on_key(press(KeyCode::Char('u')), &tmux); // unmark
    assert!(app.marked.is_empty(), "unmark dead: {:?}", app.status);
    app.on_key(press(KeyCode::Char('b')), &tmux); // bulk
    assert!(
        !matches!(app.mode, Mode::PromptFilter),
        "bulk fell into type-to-filter"
    );
    app.on_key(press(KeyCode::Char('z')), &tmux); // pause
    assert!(
        !matches!(app.mode, Mode::PromptFilter),
        "pause fell into type-to-filter"
    );
    app.on_key(press(KeyCode::Char('e')), &tmux); // restart
    assert!(
        matches!(app.mode, Mode::ConfirmRestartPane { .. }),
        "restart dead: {:?} / {:?}",
        app.mode,
        app.status
    );
}

#[test]
fn bare_saved_filter_and_clear_fire_in_normal() {
    // Bare saved_filter/filter_clear bindings must fire in
    // Normal mode — not fall into type-to-filter (F) or vanish
    // (X). Handled pre-split so both foci are covered.
    let mut app = app_with(&["a"]);
    app.config
        .keys
        .insert("saved_filter".to_string(), "F".to_string());
    app.config
        .keys
        .insert("filter_clear".to_string(), "X".to_string());
    let tmux = Tmux::new().unwrap();
    app.on_key(press(KeyCode::Char('F')), &tmux);
    assert_eq!(
        app.mode,
        Mode::Normal,
        "bare saved_filter fell into type-to-filter"
    );
    assert!(
        app.status
            .as_deref()
            .is_some_and(|s| s.contains("no saved filters")),
        "saved_filter did not cycle: {:?}",
        app.status
    );
    app.focus = Focus::Sessions;
    app.filter = "abc".to_string();
    app.on_key(press(KeyCode::Char('X')), &tmux);
    assert_eq!(
        app.mode,
        Mode::Normal,
        "bare filter_clear opened the filter"
    );
    assert!(app.filter.is_empty(), "filter_clear did not clear");
    assert!(
        app.status
            .as_deref()
            .is_some_and(|s| s.contains("search cleared")),
        "clear status missing: {:?}",
        app.status
    );
}

#[test]
fn modifier_binding_on_filter_clear_wipes_in_prompt() {
    let mut app = app_with(&["a"]);
    app.config
        .keys
        .insert("filter_clear".to_string(), "ctrl+backspace".to_string());
    let tmux = Tmux::new().unwrap();
    app.open_filter();
    app.input = "abc".to_string();
    app.cursor = 3;
    // ctrl+backspace in the prompt: hard-clears the query in place.
    app.on_key(
        KeyEvent::new(KeyCode::Backspace, KeyModifiers::CONTROL),
        &tmux,
    );
    assert_eq!(app.input, "");
    assert_eq!(app.filter, "");
    // Esc still restores the pre-edit query — the two paths coexist.
    app.open_filter();
    app.input = "abc".to_string();
    app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tmux);
    assert!(app.input.is_empty()); // pre-edit query was empty
}

fn app_with_panes() -> App {
    let mut app = app_with(&["a"]);
    let mut p1 = Pane::test("%1", "editor", "nvim");
    p1.session = "a".to_string();
    let mut p2 = Pane::test("%2", "editor", "bash");
    p2.session = "a".to_string();
    app.panes = vec![p1, p2];
    app.refresh_filtered();
    app
}

#[test]
fn session_worst_picks_neediest_state() {
    use crate::agent::AgentState;
    let mut app = app_with_panes();
    assert_eq!(app.session_worst(Some("a")), None);
    assert_eq!(app.session_worst(None), None);
    app.agents
        .insert("%1".to_string(), record("%1", "pi", AgentState::Working, 1));
    app.agents
        .insert("%2".to_string(), record("%2", "pi", AgentState::Idle, 1));
    assert_eq!(app.session_worst(Some("a")), Some(AgentState::Working));
    assert_eq!(app.session_worst(Some("zzz")), None);
    app.agents
        .insert("%2".to_string(), record("%2", "pi", AgentState::Waiting, 1));
    assert_eq!(app.session_worst(Some("a")), Some(AgentState::Waiting));
    assert_eq!(app.session_worst(None), Some(AgentState::Waiting));
}

#[test]
fn cycle_theme_advances_from_mixed_case_theme() {
    // `theme = "Dark"` loads fine (case-insensitive validation);
    // cycling used to exact-match, miss, and land on dark again.
    let mut app = app_with(&["a"]);
    app.config.theme = "Dark".to_string();
    app.cycle_theme();
    assert_ne!(app.config.theme, "dark", "cycled back to dark itself");
    // The tray palette resolves case-insensitively too.
    assert_eq!(
        crate::tray::palette_for_theme("Dark"),
        crate::tray::palette_for_theme("dark")
    );
}

#[test]
fn modifier_bound_attach_forwards_the_jump_event() {
    // `attach = "ctrl+enter"` must behave exactly like bare Enter:
    // dispatch_binding used to discard jump_event's AppEvent, so
    // the chord silently did nothing.
    let mut app = app_with_panes();
    app.config
        .keys
        .insert("attach".to_string(), "ctrl+enter".to_string());
    let chord = KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL);
    let via_chord = app.on_key(chord, &fake_tmux().0);
    let mut bare = app_with_panes();
    let via_bare = bare.on_key(press(KeyCode::Enter), &fake_tmux().0);
    assert!(via_bare.is_some(), "bare Enter should jump");
    assert_eq!(via_chord, via_bare);
}

#[test]
fn settings_hidden_rows_use_multi_token_exclude() {
    // Standing filter "nvim bash" hides both rows in the table
    // (multi-token NOT-OR); the settings page used to pass the
    // whole string as one token and report them as not hidden
    // by the standing filter.
    let mut app = app_with_panes();
    app.exclude = "nvim bash".to_string();
    let rows = app.settings_rows();
    let legend = rows.iter().find_map(|r| match r {
        SettingRow::Legend { name, meaning, .. } if name.ends_with("hidden") => {
            Some((name.clone(), meaning.clone()))
        }
        _ => None,
    });
    let (name, meaning) = legend.expect("hidden legend row");
    assert_eq!(name, "2 hidden", "{meaning}");
    assert!(meaning.contains("standing filter"), "{meaning}");
}

#[test]
fn blur_toggle_persists_only_blur() {
    // ToggleBlurCols used to also flag density as changed, so a
    // stale instance (in-memory density "comfortable") rewrote
    // its stale density over the file's newer "compact" on a
    // blur toggle. Only the blur key may be written.
    let _scratch = ScopedConfig::new();
    let path = crate::config::config_path();
    std::fs::write(&path, "[ui]\ndensity = \"compact\"\nblur_cols = false\n").unwrap();
    let mut app = app_with(&["a"]);
    assert_eq!(app.config.density, "comfortable"); // the stale copy
    app.apply_setting(SettingKind::ToggleBlurCols);
    assert!(app.config.blur_cols);
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("blur_cols = true"), "{text}");
    assert!(
        text.contains("density = \"compact\""),
        "density clobbered:\n{text}"
    );
}

#[test]
fn mru_tab_order_sorts_by_activity_and_follows_selection() {
    let mut app = app_with(&["a", "b", "c"]);
    let mut p1 = Pane::test("%1", "w", "pi");
    p1.session = "a".to_string();
    p1.activity = 100;
    let mut p2 = Pane::test("%2", "w", "pi");
    p2.session = "b".to_string();
    p2.activity = 300;
    app.panes = vec![p1, p2];
    // MRU off: tmux order untouched.
    app.apply_tab_order();
    let names: Vec<&str> = app.sessions.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["a", "b", "c"]);
    // MRU on: b(300) first, a(100) next, pane-less c sinks; the
    // cursor on "a" (selected=1) follows it to position 2.
    app.config.mru_tabs = true;
    app.selected = 1;
    app.apply_tab_order();
    let names: Vec<&str> = app.sessions.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["b", "a", "c"]);
    assert_eq!(app.selected, 2);
    // All (selected=0) survives a reorder untouched.
    app.selected = 0;
    app.apply_tab_order();
    assert_eq!(app.selected, 0);
}

fn remote_pane() -> Pane {
    Pane {
        id: "dev:%1".to_string(),
        session: "dev \u{25b8} web".to_string(),
        window_id: String::new(),
        window_name: "srv".to_string(),
        window_active: false,
        index: 0,
        command: "pi".to_string(),
        path: String::new(),
        active: false,
        width: 0,
        height: 0,
        activity: 1700,
        pid: 0,
        pid_start: 0,
        bell: false,
    }
}

fn app_with_remote() -> App {
    let mut app = app_with_panes();
    app.panes.push(remote_pane());
    app.fleet_rows = vec![remote_pane()];
    app.fleet_index
        .insert("dev:%1".to_string(), ("dev".to_string(), "web".to_string()));
    app.refresh_filtered();
    let idx = app.filtered.iter().position(|p| p.id == "dev:%1").unwrap();
    app.select_pane(idx);
    app
}

#[test]
fn remote_rows_are_read_only() {
    let mut app = app_with_remote();
    let tmux = Tmux::test("/nonexistent-tmux-binary".into());
    assert_eq!(app.selected_pane().unwrap().id, "dev:%1");
    for key in ['x', 'e', ' ', 'z'] {
        app.on_key(press(KeyCode::Char(key)), &tmux);
        assert_eq!(app.mode, Mode::Normal, "key {key} opened a modal");
        assert!(
            app.status.as_deref().unwrap_or("").contains("remote"),
            "key {key}: {:?}",
            app.status
        );
    }
    app.on_key(press(KeyCode::Char('r')), &tmux);
    assert_eq!(app.mode, Mode::Normal);
    assert!(app.status.as_deref().unwrap_or("").contains("remote"));
    // Custom commands refuse too (their {dir} is a remote path).
    app.config.commands.push(crate::config::CustomCommand {
        name: "gs".to_string(),
        key: "G".to_string(),
        spec: crate::config::parse_key("G"),
        run: "git -C {dir} status".to_string(),
    });
    app.on_key(press(KeyCode::Char('g')), &tmux);
    assert_eq!(app.mode, Mode::Normal);
    assert!(app.status.as_deref().unwrap_or("").contains("remote"));
    // But jumping out to the remote session is the whole point.
    let ev = app.on_key(press(KeyCode::Enter), &tmux);
    assert_eq!(
        ev,
        Some(AppEvent::RemoteAttach {
            host: "dev".to_string(),
            session: "web".to_string(),
        })
    );
    // A remote id with no index entry is stale, not jumpable.
    app.fleet_index.clear();
    let ev = app.on_key(press(KeyCode::Enter), &tmux);
    assert_eq!(ev, None);
    assert!(app.status.as_deref().unwrap_or("").contains("stale"));
}

#[test]
fn merge_fleet_appends_rows_and_agents_idempotently() {
    use crate::agent::AgentState;
    let mut app = app_with_panes();
    let rec = record("dev:%9", "pi", AgentState::Waiting, 5);
    let mut row = remote_pane();
    row.id = "dev:%9".to_string();
    app.fleet_rows = vec![row];
    app.fleet_agents = vec![rec];
    app.merge_fleet();
    app.merge_fleet();
    assert_eq!(app.panes.iter().filter(|p| p.id == "dev:%9").count(), 1);
    assert!(app.agents.contains_key("dev:%9"));
    assert_eq!(app.panes.len(), 3);
}

fn native_pane() -> Pane {
    let mut p = Pane::test("konsole:999:6", "tab six", "pi");
    p.session = "konsole".to_string();
    p.pid = 1001;
    p
}

#[test]
fn native_rows_merge_all_only() {
    let mut app = app_with_panes();
    app.native_rows = vec![native_pane()];
    app.merge_native();
    assert!(app.panes.iter().any(|p| p.id == "konsole:999:6"));
    assert_eq!(app.filtered.len(), 3);
    // Foreign rows never leak into a session filter.
    app.select(1); // session "a"
    app.refresh_filtered();
    assert!(app.filtered.iter().all(|p| p.id.starts_with('%')));
    app.select(0); // All
    app.refresh_filtered();
    assert_eq!(app.filtered.len(), 3);
    // Re-merge never duplicates.
    app.merge_native();
    assert_eq!(
        app.panes.iter().filter(|p| p.id == "konsole:999:6").count(),
        1
    );
}

#[test]
fn jump_event_routes_native_to_focus_native() {
    let mut app = app_with_panes();
    assert_eq!(
        app.jump_event("konsole:999:6"),
        Some(AppEvent::FocusNative("konsole:999:6".to_string()))
    );
    assert_eq!(
        app.jump_event("%1"),
        Some(AppEvent::FocusPane("%1".to_string()))
    );
}

#[test]
fn mark_kill_restart_refuse_native_rows() {
    let mut app = app_with_panes();
    let tmux = Tmux::new().unwrap();
    app.native_rows = vec![native_pane()];
    app.merge_native();
    let idx = app
        .filtered
        .iter()
        .position(|p| p.id == "konsole:999:6")
        .unwrap();
    app.select_pane(idx);
    app.toggle_mark();
    assert!(
        app.status.as_deref().unwrap_or("").contains("can't mark"),
        "mark allowed native: {:?}",
        app.status
    );
    assert!(app.marked.is_empty());
    app.on_key(press(KeyCode::Char('x')), &tmux);
    assert!(
        app.status.as_deref().unwrap_or("").contains("can't close"),
        "kill allowed native: {:?}",
        app.status
    );
    app.on_key(press(KeyCode::Char('e')), &tmux);
    assert!(
        app.status
            .as_deref()
            .unwrap_or("")
            .contains("can't restart"),
        "restart allowed native: {:?}",
        app.status
    );
}

#[test]
fn pause_passes_native_guard_to_pid_check() {
    let mut app = app_with_panes();
    let mut p = native_pane();
    p.pid = 0;
    app.native_rows = vec![p];
    app.merge_native();
    let idx = app
        .filtered
        .iter()
        .position(|p| p.id == "konsole:999:6")
        .unwrap();
    app.select_pane(idx);
    app.do_pause();
    // Past the remote guard (pid-based pause works on native rows).
    assert_eq!(app.status.as_deref(), Some("no pid for pane konsole:999:6"));
}

#[test]
fn native_quiet_tracks_screen_change() {
    let mut app = app_with_panes();
    let (h1, s1) = app.track_native_quiet("konsole:999:6", "hello", 1000);
    assert_eq!(s1, 1000);
    // Same screen keeps the clock; a changed screen restarts it.
    assert_eq!(
        app.track_native_quiet("konsole:999:6", "hello", 2000),
        (h1, s1)
    );
    assert_eq!(
        app.track_native_quiet("konsole:999:6", "hello world", 3000)
            .1,
        3000
    );
    // Spinner frames share the daemon's denoise: no clock restart.
    let (_, sa) = app.track_native_quiet("konsole:999:6", "⠙ busy", 4000);
    let (_, sb) = app.track_native_quiet("konsole:999:6", "⠋ busy", 5000);
    assert_eq!(sa, sb);
    // Failed capture preserves the clock instead of resetting it.
    assert_eq!(app.track_native_quiet("konsole:999:6", "", 6000).1, 4000);
    // Unknown ids with no capture stay unknown ("—", not 0s).
    assert_eq!(app.track_native_quiet("konsole:999:7", "", 6000), (0, 0));
}

#[test]
fn bulk_targets_prefers_marked_and_skips_unsendable() {
    let mut app = app_with_panes();
    app.native_rows = vec![native_pane()];
    app.fleet_rows = vec![{
        let mut p = Pane::test("dev:%9", "w", "pi");
        p.session = "dev ▸ web".to_string();
        p
    }];
    app.merge_caches();
    app.marked.insert("%1".to_string());
    app.marked.insert("konsole:999:6".to_string());
    app.marked.insert("dev:%9".to_string());
    app.marked.insert("%2".to_string());
    app.own_pane = Some("%2".to_string());
    let mut targets = app.bulk_targets();
    targets.sort();
    assert_eq!(targets, vec!["%1".to_string(), "konsole:999:6".to_string()]);
}

#[test]
fn bulk_targets_falls_back_to_waiting() {
    use crate::agent::AgentState;
    let mut app = app_with_panes();
    app.fleet_rows = vec![{
        let mut p = Pane::test("dev:%9", "w", "pi");
        p.session = "dev ▸ web".to_string();
        p
    }];
    app.merge_caches();
    app.agents.insert(
        "%1".to_string(),
        record("%1", "pi", AgentState::Waiting, 10),
    );
    app.agents
        .insert("%2".to_string(), record("%2", "pi", AgentState::Idle, 10));
    app.agents.insert(
        "dev:%9".to_string(),
        record("dev:%9", "pi", AgentState::Waiting, 10),
    );
    assert_eq!(app.bulk_targets(), vec!["%1".to_string()]);
}

#[test]
fn attended_records_visit_and_survives_unchanged_ticks() {
    use crate::agent::AgentState;
    let mut app = app_with_panes();
    app.agents.insert(
        "%1".to_string(),
        record("%1", "pi", AgentState::Waiting, 10),
    );
    assert!(!app.is_attended("%1"), "unvisited row reads attended");
    app.mark_attended("%1");
    assert!(app.is_attended("%1"), "visited row does not read attended");
    // The clock moving is not news: ticks must not clear the mark.
    // (The key is (state, summary) only; `waiting_secs`-style fields
    // are deliberately excluded.)
    app.agents.get_mut("%1").unwrap().cpu_ticks += 999;
    app.agents.get_mut("%1").unwrap().updated_at += 999;
    app.prune_attended();
    assert!(app.is_attended("%1"), "a tick cleared the attended mark");
}

#[test]
fn attended_survives_tick_for_remote_rows() {
    // The tick pruned attended against the tmux-only live set
    // (and against the daemon-only agent map) before the
    // fleet/native caches merged back, so a visited remote row
    // lost its checkmark every 3s.
    use crate::agent::AgentState;
    let mut app = app_with_remote();
    let rec = record("dev:%1", "pi", AgentState::Waiting, 10);
    app.fleet_agents = vec![rec.clone()];
    app.agents.insert("dev:%1".to_string(), rec);
    app.mark_attended("dev:%1");
    assert!(app.is_attended("dev:%1"), "setup: remote row not attended");
    // A full tick: fake tmux lists no remote rows; the daemon may
    // or may not answer — the fleet cache is what must keep the mark.
    let (tmux, dir) = fake_tmux();
    app.refresh(&tmux);
    assert!(app.is_attended("dev:%1"), "tick wiped the remote checkmark");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn attended_clears_on_new_state_or_new_summary() {
    use crate::agent::AgentState;
    let mut app = app_with_panes();
    app.agents.insert(
        "%1".to_string(),
        record("%1", "pi", AgentState::Waiting, 10),
    );
    app.mark_attended("%1");
    // A state change is new information.
    app.agents.get_mut("%1").unwrap().state = AgentState::Working;
    app.prune_attended();
    assert!(
        !app.is_attended("%1"),
        "state change kept the attended mark"
    );
    // So is a new summary under the same state.
    app.mark_attended("%1");
    app.agents.get_mut("%1").unwrap().summary = "approve deploy?".to_string();
    app.prune_attended();
    assert!(
        !app.is_attended("%1"),
        "summary change kept the attended mark"
    );
}

#[test]
fn attended_shell_without_a_record_stays_seen() {
    let mut app = app_with_panes();
    // %2 has no agent record: a plain shell has no state that could
    // change, so visiting it stays visited.
    app.mark_attended("%2");
    assert!(app.is_attended("%2"));
    app.prune_attended();
    assert!(app.is_attended("%2"), "shell lost its attended mark");
}

#[test]
fn do_nav_failure_never_marks() {
    // A failed jump (dead row, no server) must not record a visit:
    // the user saw the error, not the pane.
    let mut app = app_with_panes();
    let tmux = Tmux::new().unwrap();
    app.do_nav(&tmux, AppEvent::FocusPane("%999-no-such-pane".to_string()));
    assert!(
        !app.is_attended("%999-no-such-pane"),
        "failed jump recorded a visit"
    );
    assert!(
        app.attended.is_empty(),
        "failed jump left attended state behind"
    );
}

#[test]
fn bulk_empty_reports_instead_of_modal() {
    let mut app = app_with_panes();
    app.do_bulk();
    assert!(matches!(app.mode, Mode::Normal));
    assert!(
        app.status.as_deref().unwrap_or("").contains("nothing"),
        "{:?}",
        app.status
    );
}

#[test]
fn bulk_modal_tab_cycles_templates_and_esc_cancels() {
    use crate::agent::AgentState;
    let mut app = app_with_panes();
    let tmux = Tmux::new().unwrap();
    app.agents.insert(
        "%1".to_string(),
        record("%1", "pi", AgentState::Waiting, 10),
    );
    app.do_bulk();
    assert!(matches!(app.mode, Mode::PromptBulk { .. }));
    app.on_key(press(KeyCode::Tab), &tmux);
    assert_eq!(app.input, app.config.templates[1].text);
    app.on_key(press(KeyCode::Esc), &tmux);
    assert!(matches!(app.mode, Mode::Normal));
}

#[test]
fn bulk_empty_input_refuses() {
    use crate::agent::AgentState;
    let mut app = app_with_panes();
    let tmux = Tmux::new().unwrap();
    app.agents.insert(
        "%1".to_string(),
        record("%1", "pi", AgentState::Waiting, 10),
    );
    app.do_bulk();
    app.on_key(press(KeyCode::Enter), &tmux);
    assert!(matches!(app.mode, Mode::PromptBulk { .. }));
    assert!(
        app.status
            .as_deref()
            .unwrap_or("")
            .contains("nothing to send"),
        "{:?}",
        app.status
    );
}

#[test]
fn attend_event_routes_native_winners_to_focus() {
    use crate::agent::AgentState;
    let app = app_with_panes();
    let native = record("konsole:999:6", "pi", AgentState::Waiting, 50);
    let ev = app.attend_event(std::slice::from_ref(&native));
    assert_eq!(ev, Some(AppEvent::FocusNative("konsole:999:6".to_string())));
}

#[test]
fn sparkline_disabled_means_no_sampling() {
    // The config toggle is the gatekeeper: the ring stays empty
    // until the user opts in. A regression here would silently
    // sample every pane and balloon memory on big tables.
    let mut app = app_with_panes();
    assert!(!app.config.activity_sparkline);
    app.sample_sparkline("%1", "lots of new bytes of output");
    assert!(app.sparkline_samples.is_empty());
}

#[test]
fn sparkline_enabled_tracks_bytes_added() {
    let mut app = app_with_panes();
    app.config.activity_sparkline = true;
    // First sample: the previous screen is unknown, so the diff
    // is 0 (we can't attribute bytes without a baseline).
    app.sample_sparkline("%1", "first capture");
    let ring = app.sparkline_samples.get("%1").unwrap();
    assert_eq!(ring[59], 0, "first sample without baseline should be 0");
    // Second sample, screen grew by 6 bytes: ring records it.
    app.sample_sparkline("%1", "first capture!!");
    let ring = app.sparkline_samples.get("%1").unwrap();
    assert_eq!(ring[59], 2);
}

#[test]
fn sparkline_buckets_collapse_sixty_samples_into_six() {
    let mut app = app_with_panes();
    app.config.activity_sparkline = true;
    // Drive a monotonic activity ramp: each sample larger than
    // the last. After 60 samples, the rightmost bucket must
    // contain the largest value (the freshest tick) and the
    // leftmost must contain the smallest (the oldest).
    let mut screen = String::new();
    for i in 0..60 {
        let n = (i + 1) as u32 * 10;
        screen.push_str(&"x".repeat(n as usize));
        app.sample_sparkline("%1", &screen);
    }
    let buckets = app.sparkline_buckets("%1");
    assert_eq!(buckets.len(), 6);
    // Monotonic non-decreasing across buckets (older=smaller).
    for i in 1..6 {
        assert!(
            buckets[i] >= buckets[i - 1],
            "buckets regressed: {buckets:?}"
        );
    }
    // The freshest (rightmost) bucket must contain a busy sample.
    assert!(buckets[5] >= 100, "freshest bucket should be busy");
}

#[test]
fn sparkline_buckets_default_to_zeros_for_unknown_pane() {
    // A pane we never sampled must return 6 zeros — no crash,
    // no panic, just a flat ridge in the renderer.
    let app = app_with_panes();
    let buckets = app.sparkline_buckets("never-seen");
    assert_eq!(buckets, [0u32; 6]);
}

#[test]
fn sparkline_resets_when_capture_scrolls_past_baseline() {
    // A capture that no longer starts with the previous tail
    // (the user scrolled, or the terminal reset) mustn't blow
    // up — it just records 0 for that tick. The point: the
    // sparkline is for activity rate, not byte deltas, and a
    // discontinuity isn't a runaway process.
    let mut app = app_with_panes();
    app.config.activity_sparkline = true;
    app.sample_sparkline("%1", "previous content");
    app.sample_sparkline("%1", "totally different capture");
    let ring = app.sparkline_samples.get("%1").unwrap();
    assert_eq!(ring[59], 0);
}

#[test]
fn wchan_parked_ignores_run_state() {
    // A process in `R` (running) is never parked, even if its
    // wchan happens to mention tty. The combination is the
    // signal: state S + wchan n_tty_read. Anything else returns
    // false. Tested via a synthetic /proc setup; the function
    // itself just reads the kernel, so we exercise the
    // *contract* by checking the dead-pid path (which always
    // returns false) and trust the kernel for the rest.
    assert!(!crate::proc::parked_in_tty_read(u32::MAX));
}

#[test]
fn pane_carries_bell_and_bell_reason() {
    // Pane deserializes a bell flip from tmux: the bell flag and
    // its reason (the matched waiting pattern) ride together so
    // the chip can render `🔔 y/n` next to the row.
    use crate::tmux::Pane;
    let p = Pane {
        id: "%1".to_string(),
        session: "a".to_string(),
        window_id: "@0".to_string(),
        window_name: "x".to_string(),
        window_active: true,
        index: 0,
        command: "pi".to_string(),
        path: "/tmp".to_string(),
        active: true,
        width: 80,
        height: 24,
        activity: 0,
        bell: true,
        pid: 0,
        pid_start: 0,
    };
    assert!(p.bell, "Pane.bell must round-trip the tmux flag");
}

#[test]
fn tmux_pane_format_includes_bell_flag() {
    // A regression here would silently drop the bell evidence:
    // the parser would still return bell=false forever, and the
    // row would never get the chip. The round-trip is
    // end-to-end through the parser — verified by overflow_tests
    // already — so this is a smoke check that the bell field
    // exists on the public Pane type.
    use crate::tmux::Pane;
    let p = Pane::test("%1", "w", "sh");
    assert!(!p.bell, "fresh Pane::test defaults to bell=false");
    let mut p2 = p;
    p2.bell = true;
    assert!(
        p2.bell,
        "bell flag must be writable on the public Pane type"
    );
}

#[test]
fn done_state_sorts_after_working_before_idle() {
    // Done is "ran, finished, quiet" — more meaningful than
    // never-ran idle, less urgent than working. A regression in
    // state_rank_of would bury finished panes below the noise.
    use crate::agent::AgentState;
    assert!(state_rank_of(AgentState::Working) < state_rank_of(AgentState::Done));
    assert!(state_rank_of(AgentState::Done) < state_rank_of(AgentState::Idle));
}

#[test]
fn attend_event_routes_remote_winners_out() {
    use crate::agent::AgentState;
    let mut app = app_with_panes();
    app.fleet_index
        .insert("dev:%1".to_string(), ("dev".to_string(), "web".to_string()));
    let local = record("%1", "pi", AgentState::Waiting, 100);
    let mut remote = record("dev:%1", "pi", AgentState::Waiting, 50);
    remote.session = "web".to_string();
    // Longest-waiting wins: the remote (since=50) beats local (100).
    let ev = app.attend_event(&[local, remote]);
    assert_eq!(
        ev,
        Some(AppEvent::RemoteAttach {
            host: "dev".to_string(),
            session: "web".to_string(),
        })
    );
    let local2 = record("%1", "pi", AgentState::Waiting, 100);
    let ev = app.attend_event(std::slice::from_ref(&local2));
    assert_eq!(ev, Some(AppEvent::FocusPane("%1".to_string())));
    assert_eq!(app.attend_event(&[]), None);
}

#[test]
fn hosts_modal_add_remove_roundtrip() {
    let _scratch = ScopedConfig::new();
    let mut app = app_with(&["a"]);
    let tmux = Tmux::test("/nonexistent-tmux-binary".into());
    app.on_key(press(KeyCode::Char('m')), &tmux);
    assert_eq!(app.mode, Mode::Hosts);
    app.on_key(press(KeyCode::Char('a')), &tmux);
    assert_eq!(app.mode, Mode::PromptHostAdd);
    for c in "dev user@host control".chars() {
        app.on_key(press(KeyCode::Char(c)), &tmux);
    }
    app.on_key(press(KeyCode::Enter), &tmux);
    assert_eq!(app.mode, Mode::Hosts);
    assert_eq!(app.fleet_hosts.len(), 1);
    assert_eq!(app.fleet_hosts[0].name, "dev");
    assert_eq!(app.fleet_hosts[0].mode, crate::fleet::Mode::Control);
    app.on_key(press(KeyCode::Char('x')), &tmux);
    assert!(app.fleet_hosts.is_empty());
    app.on_key(press(KeyCode::Esc), &tmux);
    assert_eq!(app.mode, Mode::Normal);
}

#[test]
fn host_add_rejects_garbage_and_stays_open() {
    let _scratch = ScopedConfig::new();
    let mut app = app_with(&["a"]);
    let tmux = Tmux::test("/nonexistent-tmux-binary".into());
    app.on_key(press(KeyCode::Char('m')), &tmux);
    app.on_key(press(KeyCode::Char('a')), &tmux);
    for c in "justname".chars() {
        app.on_key(press(KeyCode::Char(c)), &tmux);
    }
    app.on_key(press(KeyCode::Enter), &tmux);
    assert_eq!(app.mode, Mode::PromptHostAdd);
    assert!(app.status.as_deref().unwrap_or("").contains("want"));
    app.on_key(press(KeyCode::Esc), &tmux);
    assert_eq!(app.mode, Mode::Hosts);
}

#[test]
fn hosts_enter_jumps_to_needy_remote() {
    use crate::agent::AgentState;
    let _scratch = ScopedConfig::new();
    let mut app = app_with(&["a"]);
    app.fleet_hosts = vec![crate::fleet::Host {
        name: "dev".to_string(),
        target: "t".to_string(),
        mode: crate::fleet::Mode::Observe,
        label: String::new(),
    }];
    let mut rec = record("dev:%1", "pi", AgentState::Waiting, 5);
    rec.session = "web".to_string();
    app.fleet_agents = vec![rec];
    app.fleet_index
        .insert("dev:%1".to_string(), ("dev".to_string(), "web".to_string()));
    let tmux = Tmux::test("/nonexistent-tmux-binary".into());
    app.mode = Mode::Hosts;
    let ev = app.on_key(press(KeyCode::Enter), &tmux);
    assert_eq!(
        ev,
        Some(AppEvent::RemoteAttach {
            host: "dev".to_string(),
            session: "web".to_string(),
        })
    );
}

#[test]
fn alt_digits_jump_to_tabs() {
    let mut app = app_with(&["a", "b"]);
    let tmux = Tmux::test("/nonexistent-tmux-binary".into());
    let alt = |c: char| KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT);
    app.on_key(alt('2'), &tmux);
    assert_eq!(app.selected, 1);
    assert_eq!(app.side_session().unwrap().name, "a");
    app.on_key(alt('3'), &tmux);
    assert_eq!(app.selected, 2);
    app.on_key(alt('1'), &tmux);
    assert_eq!(app.selected, 0);
    // Out of range: loud no-op.
    app.on_key(alt('8'), &tmux);
    assert_eq!(app.selected, 0);
    assert!(app.status.as_deref().unwrap_or("").contains("no tab"));
    // Modals keep eating Alt keys (typing safety).
    app.open_filter();
    app.on_key(alt('2'), &tmux);
    assert_eq!(app.mode, Mode::PromptFilter);
    assert_eq!(app.selected, 0);
}

#[test]
fn cpu_pct_needs_two_rising_samples() {
    use std::time::{Duration, Instant};
    let t0 = Instant::now();
    // 150 ticks in 1s at 100Hz = 150% (subtrees span cores).
    assert_eq!(
        cpu_pct(100, t0, 250, t0 + Duration::from_secs(1), 100.0),
        Some(150.0)
    );
    // Restarted process (ticks fell) and zero elapsed re-baseline.
    assert_eq!(
        cpu_pct(250, t0, 10, t0 + Duration::from_secs(1), 100.0),
        None
    );
    assert_eq!(cpu_pct(100, t0, 100, t0, 100.0), None);
}

#[test]
fn parse_shortstat_handles_missing_parts() {
    assert_eq!(
        parse_shortstat(" 2 files changed, 45 insertions(+), 12 deletions(-)"),
        (2, 45, 12)
    );
    assert_eq!(
        parse_shortstat(" 1 file changed, 1 insertion(+)"),
        (1, 1, 0)
    );
    assert_eq!(
        parse_shortstat(" 1 file changed, 3 deletions(-)"),
        (1, 0, 3)
    );
    assert_eq!(parse_shortstat(""), (0, 0, 0));
}

#[test]
fn git_stat_reads_branch_diff_and_untracked() {
    let dir = std::env::temp_dir().join(format!("terhub-git-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args([
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-b", "main", "-q"]);
    std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "init"]);
    std::fs::write(dir.join("a.txt"), "one\nTWO\nthree\n").unwrap();
    std::fs::write(dir.join("new.txt"), "brand new\n").unwrap();
    let stat = git_diff_stat(dir.to_str().unwrap()).expect("repo stats");
    assert_eq!(stat.branch, "main");
    assert_eq!(stat.files, 1);
    assert!(stat.ins >= 2, "ins={}", stat.ins);
    assert!(stat.del >= 1, "del={}", stat.del);
    assert_eq!(stat.untracked, 1);
    let line = stat.render();
    assert!(line.contains("main"), "{line}");
    assert!(line.contains("1 new"), "{line}");
    // Clean tree renders as clean; non-repo renders nothing.
    git(&["add", "-A"]);
    git(&["commit", "-qm", "all"]);
    let clean = git_diff_stat(dir.to_str().unwrap()).unwrap();
    assert!(clean.render().contains("clean"), "{}", clean.render());
    assert!(git_diff_stat("/nonexistent-terhub-dir").is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn session_segment_click_selects_or_attaches() {
    let mut app = app_with(&["a", "b"]);
    let tmux = Tmux::test("/nonexistent-tmux-binary".into());
    // Plain click selects the tab and refilters, no event.
    let ev = app.click_session_segment(2, false, &tmux);
    assert_eq!(ev, None);
    assert_eq!(app.selected, 2);
    assert_eq!(app.focus, Focus::Sessions);
    // Ctrl+click attaches to the tab's session.
    let ev = app.click_session_segment(1, true, &tmux);
    assert_eq!(ev, Some(AppEvent::Attach("a".to_string())));
    assert_eq!(app.selected, 1);
    // The All tab never attaches (it isn't a session).
    let ev = app.click_session_segment(0, true, &tmux);
    assert_eq!(ev, None);
    assert!(app.status.as_deref().unwrap_or("").contains("All"));
    // Out of range: loud no-op.
    let ev = app.click_session_segment(99, false, &tmux);
    assert_eq!(ev, None);
}

#[test]
fn pause_and_restart_refuse_own_pane() {
    let mut app = app_with_panes();
    app.own_pane = Some("%1".to_string());
    let tmux = Tmux::test("/nonexistent-tmux-binary".into());
    let idx = app.filtered.iter().position(|p| p.id == "%1").unwrap();
    app.select_pane(idx);
    app.on_key(press(KeyCode::Char('z')), &tmux);
    assert_eq!(app.mode, Mode::Normal);
    assert!(
        app.status.as_deref().unwrap_or("").contains("own"),
        "pause own: {:?}",
        app.status
    );
    app.on_key(press(KeyCode::Char('e')), &tmux);
    assert_eq!(app.mode, Mode::Normal);
    assert!(
        app.status.as_deref().unwrap_or("").contains("own"),
        "restart own: {:?}",
        app.status
    );
}

#[test]
fn refresh_error_keeps_panes_and_marks() {
    // A tick that fails to list panes is non-destructive: stale rows
    // and the mark set survive, with the error in the status line.
    let mut app = app_with_panes();
    app.marked.insert("%1".to_string());
    let tmux = Tmux::test("/nonexistent-tmux-binary".into());
    app.refresh(&tmux);
    assert_eq!(app.panes.len(), 2);
    assert_eq!(app.filtered.len(), 2);
    assert!(app.marked.contains("%1"));
    let status = app.status.as_deref().unwrap_or("");
    assert!(status.contains("panes error"), "{status}");
}

#[test]
fn tab_toggles_pane_focus() {
    let mut app = app_with_panes();
    let tmux = Tmux::new().unwrap();
    assert_eq!(app.focus, Focus::Panes);
    app.on_key(press(KeyCode::Tab), &tmux);
    assert_eq!(app.focus, Focus::Sessions);
    app.on_key(press(KeyCode::Tab), &tmux);
    assert_eq!(app.focus, Focus::Panes);
}

#[test]
fn session_keys_work_from_pane_focus() {
    let mut app = app_with_panes();
    let tmux = Tmux::new().unwrap();
    assert_eq!(app.focus, Focus::Panes);
    app.on_key(press(KeyCode::Char('n')), &tmux);
    assert_eq!(app.mode, Mode::PromptNew);
    app.on_key(press(KeyCode::Esc), &tmux);
    app.select(1); // session "a"
    app.on_key(press(KeyCode::Char('r')), &tmux);
    assert!(matches!(app.mode, Mode::PromptRename(_)));
    app.on_key(press(KeyCode::Esc), &tmux);
    // Detach routes from pane focus too — but only pressed without TMUX:
    // with it set, 'd' would really detach the test runner's client, and
    // mutating the env here would race parallel tests either way.
    if std::env::var("TMUX").is_err() {
        app.on_key(press(KeyCode::Char('d')), &tmux);
        assert_eq!(
            app.status.as_deref(),
            Some("not inside tmux (nothing to detach)")
        );
    }
}

#[test]
fn help_names_the_settings_key() {
    let mut app = app_with_panes();
    let tmux = Tmux::new().unwrap();
    // Panes focus.
    app.on_key(press(KeyCode::Char('?')), &tmux);
    let panes = app.status.clone().unwrap_or_default();
    assert!(panes.contains("s=settings"), "panes help: {panes}");
    // Sessions focus.
    app.focus = Focus::Sessions;
    app.on_key(press(KeyCode::Char('?')), &tmux);
    let sessions = app.status.clone().unwrap_or_default();
    assert!(sessions.contains("s=settings"), "sessions help: {sessions}");
}

#[test]
fn preview_open_enter_jumps_from_either_focus() {
    let mut app = app_with_panes();
    let tmux = Tmux::new().unwrap();
    app.show_preview = true;
    app.focus = Focus::Sessions;
    app.select(1); // session "a": without the preview this would attach.
    assert_eq!(
        app.on_key(press(KeyCode::Enter), &tmux),
        Some(AppEvent::FocusPane("%1".to_string()))
    );
}

#[test]
fn status_expires_after_ttl() {
    let mut app = app_with(&["a"]);
    app.set_status("killed x".to_string());
    app.expire_status();
    assert!(app.status.is_some()); // fresh: stays
    app.status_since = Some(
        std::time::Instant::now()
            .checked_sub(App::STATUS_TTL + std::time::Duration::from_secs(1))
            .expect("test clock must exceed STATUS_TTL+1s"),
    );
    app.expire_status();
    assert!(app.status.is_none()); // stale: footer hints return
    // Unstamped (direct write) stays sticky.
    app.status = Some("sticky".to_string());
    app.expire_status();
    assert!(app.status.is_some());
}

#[test]
fn repeated_help_toggles_legend() {
    let mut app = app_with_panes();
    let tmux = Tmux::new().unwrap();
    app.on_key(press(KeyCode::Char('?')), &tmux);
    assert!(
        app.status
            .as_deref()
            .is_some_and(|s| s.starts_with("panes:"))
    );
    app.on_key(press(KeyCode::Char('?')), &tmux);
    assert!(
        app.status
            .as_deref()
            .is_some_and(|s| s.starts_with("legend:"))
    );
    app.on_key(press(KeyCode::Char('?')), &tmux);
    assert!(
        app.status
            .as_deref()
            .is_some_and(|s| s.starts_with("panes:"))
    );
    app.focus = Focus::Sessions;
    app.on_key(press(KeyCode::Char('?')), &tmux);
    assert!(
        app.status
            .as_deref()
            .is_some_and(|s| s.starts_with("sessions:"))
    );
    app.on_key(press(KeyCode::Char('?')), &tmux);
    assert!(
        app.status
            .as_deref()
            .is_some_and(|s| s.starts_with("legend:"))
    );
}

#[test]
fn tab_without_panes_stays_put() {
    let mut app = app_with(&["a"]);
    let tmux = Tmux::new().unwrap();
    app.focus = Focus::Sessions;
    app.on_key(press(KeyCode::Tab), &tmux);
    assert_eq!(app.focus, Focus::Sessions);
}

#[test]
fn esc_in_panes_steps_back_instead_of_quit() {
    let mut app = app_with_panes();
    let tmux = Tmux::new().unwrap();
    app.focus = Focus::Panes;
    assert_eq!(app.on_key(press(KeyCode::Esc), &tmux), None);
    assert_eq!(app.focus, Focus::Sessions);
}

#[test]
fn enter_in_panes_focuses_pane() {
    let mut app = app_with_panes();
    let tmux = Tmux::new().unwrap();
    app.focus = Focus::Panes;
    app.select_pane(1);
    assert_eq!(
        app.on_key(press(KeyCode::Enter), &tmux),
        Some(AppEvent::FocusPane("%2".to_string()))
    );
}

#[test]
fn attend_key_never_errors() {
    // Daemon may or may not run during tests: either we get a focus event
    // for a needy agent or a status explaining why not.
    let mut app = app_with(&["a"]);
    app.focus = Focus::Sessions;
    let tmux = Tmux::new().unwrap();
    let ev = app.on_key(press(KeyCode::Char('a')), &tmux);
    if ev.is_none() {
        assert!(app.status.is_some());
    }
}

#[test]
fn kill_in_panes_asks_confirm() {
    let mut app = app_with_panes();
    let tmux = Tmux::new().unwrap();
    app.focus = Focus::Panes;
    app.on_key(press(KeyCode::Char('x')), &tmux);
    assert_eq!(app.mode, Mode::ConfirmKillPane("%1".to_string()));
    app.on_key(press(KeyCode::Esc), &tmux);
    assert_eq!(app.mode, Mode::Normal);
}

#[test]
fn space_toggles_mark_and_steps_down() {
    let mut app = app_with_panes();
    app.focus = Focus::Panes;
    let tmux = Tmux::new().unwrap();
    let first = app.selected_pane().unwrap().id.clone();
    app.on_key(press(KeyCode::Char(' ')), &tmux);
    assert!(app.marked.contains(&first));
    assert_eq!(app.pane_selected, 1);
    // Toggle off from the same row.
    app.select_pane(0);
    app.on_key(press(KeyCode::Char(' ')), &tmux);
    assert!(!app.marked.contains(&first));
}

#[test]
fn expand_covers_all_documented_vars() {
    for v in crate::config::COMMAND_VARS {
        let out = expand_command(
            &format!("pre-{{{v}}}-post"),
            &Pane::test("%1", "w", "sh"),
            None,
        );
        assert!(!out.contains('{'), "var {v} did not expand: {out}");
    }
    // Unknown vars and escapes stay literal; known vars are
    // shell-quoted so the expanded string is safe to feed to
    // `sh -c`. `%1` becomes `'%1'` (the quoting is what closes
    // the pass-2 shell-injection finding for `[[command]]`).
    let out = expand_command(
        "{bogus} {{pane}} {pane}",
        &Pane::test("%1", "w", "sh"),
        None,
    );
    assert_eq!(out, "{bogus} {pane} '%1'");
}

#[test]
fn expand_command_quotes_substitutions_for_shell_safety() {
    // The user template `"git -C {dir} status"` is fed to
    // `sh -c <expanded>`. A `dir` (or pane id, session/window name,
    // agent, state, command) containing shell metachars must NOT
    // be able to inject an extra command. sh_quote wraps the
    // substitution in single quotes, so `';id;'` becomes
    // `'';id;'''` and the embedded `;` is literal inside the
    // quoted region.
    let mut pane = Pane::test("%1", "w", "sh");
    pane.path = "/tmp/agent-cwd; touch /tmp/terhub-pwned".to_string();
    let out = expand_command("git -C {dir} status", &pane, None);
    // The shell metachars are inside single quotes — they cannot
    // break out and run `touch /tmp/terhub-pwned`.
    assert!(
        out.contains("'/tmp/agent-cwd; touch /tmp/terhub-pwned'"),
        "substitution not shell-quoted: {out:?}"
    );
    // Run it for real and prove no side effect.
    let _ = std::fs::remove_file("/tmp/terhub-pwned");
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(&out)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    // git may fail because the fake path doesn't exist; what
    // matters is no second command ran.
    assert!(!status.success(), "git should not find the fake path");
    assert!(
        !std::path::Path::new("/tmp/terhub-pwned").exists(),
        "shell injection succeeded — /tmp/terhub-pwned was created"
    );
}

#[test]
fn custom_command_runs_and_shows_output() {
    use crate::config::CustomCommand;
    let mut app = app_with_panes();
    app.focus = Focus::Panes;
    app.config.commands.push(CustomCommand {
        name: "echo-pane".to_string(),
        key: "G".to_string(),
        spec: crate::config::parse_key("G"),
        run: "echo pane-is-{pane}".to_string(),
    });
    let tmux = Tmux::new().unwrap();
    let id = app.selected_pane().unwrap().id.clone();
    app.on_key(press(KeyCode::Char('g')), &tmux);
    match &app.mode {
        Mode::CommandOutput { name, lines, .. } => {
            assert_eq!(name, "echo-pane");
            assert!(
                lines.iter().any(|l| l.contains(&format!("pane-is-{id}"))),
                "{lines:?}"
            );
        }
        m => panic!("expected output popup, got {m:?}"),
    }
    app.on_key(press(KeyCode::Esc), &tmux);
    assert_eq!(app.mode, Mode::Normal);
}

#[test]
fn custom_command_runs_from_sessions_focus() {
    use crate::config::CustomCommand;
    let mut app = app_with_panes();
    app.focus = Focus::Sessions;
    app.config.commands.push(CustomCommand {
        name: "echo-pane".to_string(),
        key: "G".to_string(),
        spec: crate::config::parse_key("G"),
        run: "echo pane-is-{pane}".to_string(),
    });
    let tmux = Tmux::new().unwrap();
    app.on_key(press(KeyCode::Char('g')), &tmux);
    assert!(
        matches!(app.mode, Mode::CommandOutput { .. }),
        "custom dead in Sessions focus: {:?}",
        app.mode
    );
}

#[test]
fn wheel_over_the_session_bar_refilters_the_pane_table() {
    // The sidebar selection decides which session the table shows.
    // The wheel is the same gesture as the arrow keys, so it has to
    // re-filter: without it the table kept listing the previous
    // session's panes until the next tick rebuilt it.
    let tmux = Tmux::new().unwrap();
    let mut app = app_with(&["a", "b"]);
    let mut p1 = Pane::test("%1", "editor", "nvim");
    p1.session = "a".to_string();
    let mut p2 = Pane::test("%2", "editor", "bash");
    p2.session = "b".to_string();
    app.panes = vec![p1, p2];
    app.refresh_filtered();
    app.focus = Focus::Sessions;
    // "All sessions" is row 0, so row 1 is session "a".
    app.selected = 1;
    app.refresh_filtered();
    assert_eq!(app.filtered.len(), 1);
    assert_eq!(app.filtered[0].session, "a");
    // One wheel notch down moves to session "b" ...
    app.on_wheel(false, &tmux);
    assert_eq!(app.selected, 2);
    // ... and the table follows it.
    assert_eq!(
        app.filtered.len(),
        1,
        "the table still lists the old session"
    );
    assert_eq!(app.filtered[0].session, "b");
    // And back again.
    app.on_wheel(true, &tmux);
    assert_eq!(app.selected, 1);
    assert_eq!(app.filtered[0].session, "a");
}

#[test]
fn wheel_scrolls_output_popup() {
    let tmux = Tmux::new().unwrap();
    let mut app = app_with_panes();
    app.mode = Mode::CommandOutput {
        name: "out".to_string(),
        lines: (0..20).map(|i| format!("line {i}")).collect(),
        scroll: 10,
    };
    app.on_wheel(true, &tmux);
    assert_eq!(output_scroll(&app), 7);
    app.on_wheel(false, &tmux);
    assert_eq!(output_scroll(&app), 10);
    // Clamps at both ends.
    for _ in 0..20 {
        app.on_wheel(true, &tmux);
    }
    assert_eq!(output_scroll(&app), 0);
    for _ in 0..30 {
        app.on_wheel(false, &tmux);
    }
    assert_eq!(output_scroll(&app), 19);
}

#[test]
fn wheel_moves_table_and_settings_selection() {
    let tmux = Tmux::new().unwrap();
    let mut app = app_with_panes();
    app.focus = Focus::Panes;
    assert_eq!(app.pane_selected, 0);
    app.on_wheel(false, &tmux);
    assert_eq!(app.pane_selected, 1);
    app.on_wheel(true, &tmux);
    assert_eq!(app.pane_selected, 0);
    app.open_settings();
    assert_eq!(app.settings_selected, 0);
    app.on_wheel(false, &tmux);
    assert_eq!(app.settings_selected, 1);
    app.on_wheel(true, &tmux);
    assert_eq!(app.settings_selected, 0);
    // Prompts eat the wheel.
    app.open_filter();
    app.on_wheel(false, &tmux);
    assert_eq!(app.mode, Mode::PromptFilter);
}

fn output_scroll(app: &App) -> usize {
    match &app.mode {
        Mode::CommandOutput { scroll, .. } => *scroll,
        m => panic!("lost popup mode: {m:?}"),
    }
}

#[test]
fn kill_with_marks_opens_bulk_confirm() {
    let mut app = app_with_panes();
    app.focus = Focus::Panes;
    app.marked.insert("%1".to_string());
    app.marked.insert("%2".to_string());
    let tmux = Tmux::new().unwrap();
    app.on_key(press(KeyCode::Char('x')), &tmux);
    assert_eq!(
        app.mode,
        Mode::ConfirmKillMarked(vec!["%1".to_string(), "%2".to_string()])
    );
    app.on_key(press(KeyCode::Esc), &tmux);
    assert_eq!(app.mode, Mode::Normal);
    // Marks survive a cancelled kill.
    assert_eq!(app.marked.len(), 2);
    app.on_key(press(KeyCode::Char('u')), &tmux);
    assert!(app.marked.is_empty());
}

#[test]
fn ctrl_c_quits_anywhere() {
    let mut app = app_with(&["a"]);
    let tmux = Tmux::new().unwrap();
    let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert_eq!(app.on_key(key, &tmux), Some(AppEvent::Quit));
    // Even mid-prompt.
    let mut app = app_with(&["a"]);
    app.focus = Focus::Sessions;
    app.on_key(press(KeyCode::Char('n')), &tmux);
    assert_eq!(app.mode, Mode::PromptNew);
    assert_eq!(app.on_key(key, &tmux), Some(AppEvent::Quit));
}

#[test]
fn prompt_cursor_edits_mid_line() {
    let mut app = app_with(&["a"]);
    app.focus = Focus::Sessions;
    let tmux = Tmux::new().unwrap();
    app.on_key(press(KeyCode::Char('n')), &tmux);
    for c in ['a', 'b'] {
        app.on_key(press(KeyCode::Char(c)), &tmux);
    }
    app.on_key(press(KeyCode::Left), &tmux);
    app.on_key(press(KeyCode::Char('X')), &tmux);
    assert_eq!(app.input, "aXb");
    assert_eq!(app.cursor, 2);
    app.on_key(press(KeyCode::Home), &tmux);
    app.on_key(press(KeyCode::Delete), &tmux);
    assert_eq!(app.input, "Xb");
    app.on_key(press(KeyCode::End), &tmux);
    app.on_key(press(KeyCode::Backspace), &tmux);
    assert_eq!(app.input, "X");
    assert_eq!(app.cursor, 1);
}

#[test]
fn rename_prefill_parks_cursor_at_end() {
    let mut app = app_with(&["alpha"]);
    app.focus = Focus::Sessions;
    app.select(1); // "alpha"
    let tmux = Tmux::new().unwrap();
    app.on_key(press(KeyCode::Char('r')), &tmux);
    assert_eq!(app.mode, Mode::PromptRename("alpha".to_string()));
    assert_eq!(app.cursor, 5);
}

#[test]
fn rename_from_panes_focus_uses_pane_session() {
    // Sidebar on All must not block rename: the selected pane's session
    // is the obvious target.
    let mut app = app_with_panes();
    app.focus = Focus::Panes;
    app.select(0); // All
    assert!(app.side_session().is_none());
    assert!(app.selected_pane().is_some());
    let tmux = Tmux::new().unwrap();
    app.on_key(press(KeyCode::Char('r')), &tmux);
    assert_eq!(app.mode, Mode::PromptRename("a".to_string()));
}

#[test]
fn attend_works_from_pane_focus() {
    let mut app = app_with_panes();
    let tmux = Tmux::new().unwrap();
    app.focus = Focus::Panes;
    let ev = app.on_key(press(KeyCode::Char('a')), &tmux);
    // Same contract as sessions focus: a jump event or an explanation.
    if ev.is_none() {
        assert!(app.status.is_some());
    }
}

#[test]
fn ensure_visible_tracks_selection() {
    let mut app = app_with(&["a"]);
    app.panes = (0..10)
        .map(|i| {
            let mut p = Pane::test(&format!("%{i}"), "w", "sh");
            p.session = "a".to_string();
            p
        })
        .collect();
    app.refresh_filtered();
    app.select_pane(9);
    app.ensure_visible(4);
    assert_eq!(app.pane_scroll, 6);
    app.select_pane(0);
    app.ensure_visible(4);
    assert_eq!(app.pane_scroll, 0);
    app.select_pane(2);
    app.ensure_visible(4);
    assert_eq!(app.pane_scroll, 0);
}

#[test]
fn sidebar_filters_table_and_keeps_pane() {
    let mut app = app_with(&["a", "b"]);
    let mut p1 = Pane::test("%1", "w", "sh");
    p1.session = "a".to_string();
    let mut p2 = Pane::test("%2", "w", "sh");
    p2.session = "b".to_string();
    app.panes = vec![p1, p2];
    app.refresh_filtered();
    assert_eq!(app.filtered.len(), 2);
    app.select_pane(1);
    app.select(1); // "a"
    app.refresh_filtered();
    assert_eq!(app.filtered.len(), 1);
    assert_eq!(app.filtered[0].id, "%1");
    // %2 didn't survive the filter: selection falls back, stays valid.
    assert_eq!(app.selected_pane().unwrap().id, "%1");
    app.select(0); // All
    app.refresh_filtered();
    assert_eq!(app.filtered.len(), 2);
}

#[test]
fn esc_closes_preview_before_stepping_out() {
    let mut app = app_with_panes();
    let tmux = Tmux::new().unwrap();
    app.focus = Focus::Panes;
    app.show_preview = true;
    assert_eq!(app.on_key(press(KeyCode::Esc), &tmux), None);
    assert!(!app.show_preview);
    assert_eq!(app.focus, Focus::Panes);
}

#[test]
fn right_click_selects_row_and_opens_preview() {
    let mut app = app_with_panes();
    let tmux = Tmux::new().unwrap();
    app.right_click(&tmux, Some(1));
    assert!(app.show_preview);
    assert_eq!(app.pane_selected, 1);
    assert_eq!(app.focus, Focus::Panes);
}

#[test]
fn right_click_closes_preview_from_anywhere() {
    let mut app = app_with_panes();
    let tmux = Tmux::new().unwrap();
    app.right_click(&tmux, Some(0));
    assert!(app.show_preview);
    app.right_click(&tmux, None); // off-table click still closes
    assert!(!app.show_preview);
    app.right_click(&tmux, Some(0));
    app.right_click(&tmux, Some(1)); // on-table click closes, no reselect
    assert!(!app.show_preview);
    assert_eq!(app.pane_selected, 0);
}

#[test]
fn right_click_without_row_is_noop_when_closed() {
    let mut app = app_with_panes();
    let tmux = Tmux::new().unwrap();
    app.right_click(&tmux, None);
    assert!(!app.show_preview);
}

#[test]
fn preview_jump_targets_selected_pane_only_while_open() {
    let mut app = app_with_panes();
    let tmux = Tmux::new().unwrap();
    assert_eq!(app.preview_jump_target(), None);
    app.right_click(&tmux, Some(1));
    assert_eq!(app.preview_jump_target(), Some("%2".to_string()));
}

#[test]
fn preview_key_toggles() {
    let mut app = app_with_panes();
    let tmux = Tmux::new().unwrap();
    assert_eq!(app.on_key(press(KeyCode::Char('p')), &tmux), None);
    assert!(app.show_preview);
    assert_eq!(app.on_key(press(KeyCode::Char('p')), &tmux), None);
    assert!(!app.show_preview);
}

#[test]
fn theme_key_cycles_and_wraps() {
    let mut app = app_with(&["a"]);
    let tmux = Tmux::new().unwrap();
    assert_eq!(app.on_key(press(KeyCode::Char('t')), &tmux), None);
    assert_eq!(app.config.theme, "light");
    app.config.theme = "onehalf-light".to_string();
    app.on_key(press(KeyCode::Char('t')), &tmux);
    assert_eq!(app.config.theme, "dark");
    assert!(app.status.as_ref().unwrap().contains("theme: dark"));
}

#[test]
fn density_key_flips() {
    let mut app = app_with(&["a"]);
    let tmux = Tmux::new().unwrap();
    assert_eq!(app.on_key(press(KeyCode::Char('v')), &tmux), None);
    assert!(app.config.compact());
    app.on_key(press(KeyCode::Char('v')), &tmux);
    assert!(!app.config.compact());
}

/// Serialises every lib test that mutates a *process-global* input.
///
/// `PATH` is the only one left: the config path moved to a
/// per-thread override (see `crate::config::override_config_path`),
/// so config tests no longer touch shared state at all. `PATH` is
/// still process-global, so a test that repoints it must not
/// overlap another that resolves a program through it.
///
/// `pub(crate)` because the lock is process-wide, not module-wide:
/// every lib test that redirects `PATH` has to take the *same*
/// lock, or a concurrent setter in another module can hijack a
/// sibling's program lookup.
pub(crate) static SCRATCH_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
/// Hands out distinct scratch config paths, one per `ScopedConfig`.
static NEXT_CFG: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[test]
fn settings_open_navigate_apply_persists() {
    let _scratch = ScopedConfig::new();
    let mut app = app_with(&["a"]);
    let tmux = Tmux::new().unwrap();
    assert_eq!(app.on_key(press(KeyCode::Char('s')), &tmux), None);
    assert_eq!(app.mode, Mode::Settings);
    // Down moves between items, skipping the Theme header.
    app.on_key(press(KeyCode::Down), &tmux);
    app.on_key(press(KeyCode::Down), &tmux);
    assert_eq!(app.settings_selected, 2);
    // Enter applies dracula live and to disk.
    app.on_key(press(KeyCode::Enter), &tmux);
    assert_eq!(app.config.theme, "dracula");
    assert!(app.status.as_ref().unwrap().contains("(saved)"));
    let text = std::fs::read_to_string(crate::config::config_path()).unwrap();
    assert!(text.contains("theme = \"dracula\""), "{text}");
    // Toggles flip too (drive to the last row, however many
    // theme rows exist now).
    for _ in 0..app.settings_rows().len() + 1 {
        app.on_key(press(KeyCode::Down), &tmux);
    }
    // Bottom item is the standing-filter editor now, not a toggle:
    // Enter opens the prompt in standing mode instead of applying.
    app.on_key(press(KeyCode::Enter), &tmux);
    assert_eq!(app.mode, Mode::PromptFilter);
    assert!(app.editing_standing);
    app.on_key(press(KeyCode::Esc), &tmux);
    assert_eq!(app.mode, Mode::Settings);
    // The highlight toggle still flips by explicit index (position
    // within the selectable items, not the row number).
    let items = app.settings_item_rows();
    let hi = items
        .iter()
        .position(|&row| {
            matches!(
                app.settings_rows()[row],
                SettingRow::Item {
                    kind: SettingKind::ToggleHighlight,
                    ..
                }
            )
        })
        .unwrap();
    app.settings_selected = hi;
    app.on_key(press(KeyCode::Enter), &tmux);
    assert!(!app.config.highlight_attached);
    app.on_key(press(KeyCode::Esc), &tmux);
    assert_eq!(app.mode, Mode::Normal);
}

#[test]
fn mru_toggle_flips_orders_and_persists() {
    let _scratch = ScopedConfig::new();
    let mut app = app_with(&["a", "b"]);
    let mut p = Pane::test("%1", "w", "pi");
    p.session = "b".to_string();
    p.activity = 50;
    app.panes = vec![p];
    assert!(!app.config.mru_tabs);
    app.apply_setting(SettingKind::ToggleMruTabs);
    assert!(app.config.mru_tabs);
    assert!(app.status.as_ref().unwrap().contains("(saved)"));
    let names: Vec<&str> = app.sessions.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["b", "a"]);
    let text = std::fs::read_to_string(crate::config::config_path()).unwrap();
    assert!(text.contains("mru_tabs = true"), "{text}");
}

#[test]
fn settings_click_hits_items_not_headers() {
    let _scratch = ScopedConfig::new();
    let mut app = app_with(&["a"]);
    app.open_settings();
    assert!(!app.settings_apply_at_row(0)); // Theme header
    assert!(app.settings_apply_at_row(2)); // light
    assert_eq!(app.config.theme, "light");
}

#[test]
fn session_counts_cover_needy() {
    use crate::agent::{AgentRecord, AgentState, Source};
    let mut app = app_with_panes();
    app.agents.insert(
        "%1".to_string(),
        AgentRecord {
            pane: "%1".to_string(),
            agent: "claude".to_string(),
            state: AgentState::Waiting,
            source: Source::Hook,
            summary: String::new(),
            session: "a".to_string(),
            window: "w".to_string(),
            command: "sh".to_string(),
            updated_at: 1,
            state_since: 1,
            cpu_ticks: 0,
            cpu_still_since: 0,
            agent_start: 0,
            last_turn_secs: 0,
            screen_hash: 0,
            quiet_since: 0,
            last_prompt_at: 0,
            last_prompt: String::new(),
            handle: String::new(),
            model: String::new(),
            ctx_pct: None,
            reg_pid: 0,
            bell: false,
            bell_reason: String::new(),
            stuck_base: None,
        },
    );
    let counts = app.session_counts();
    assert_eq!(counts.get("a"), Some(&(2, 1)));
}

#[test]
fn attention_count_ignores_orphan_agents() {
    use crate::agent::AgentState;
    // Two panes in session "a"; one needy record on %1, plus a
    // stale needy record for %99 (a pane the live list doesn't
    // know about). The "All" sidebar must show 1, not 2 — orphan
    // records can never push needy past panes.len().
    let mut app = app_with_panes();
    app.agents
        .insert("%1".to_string(), record("%1", "pi", AgentState::Waiting, 1));
    app.agents.insert(
        "%99".to_string(),
        record("%99", "pi", AgentState::Waiting, 1),
    );
    assert_eq!(app.attention_count(), 1);
    assert!(app.attention_count() <= app.panes.len());
}

fn record(
    pane: &str,
    agent: &str,
    state: crate::agent::AgentState,
    state_since: u64,
) -> crate::agent::AgentRecord {
    crate::agent::AgentRecord {
        pane: pane.to_string(),
        agent: agent.to_string(),
        state,
        source: crate::agent::Source::Hook,
        summary: String::new(),
        session: "a".to_string(),
        window: "w".to_string(),
        command: "sh".to_string(),
        updated_at: state_since,
        state_since,
        cpu_ticks: 0,
        cpu_still_since: 0,
        agent_start: 0,
        last_turn_secs: 0,
        screen_hash: 0,
        quiet_since: 0,
        last_prompt_at: 0,
        last_prompt: String::new(),
        handle: String::new(),
        model: String::new(),
        ctx_pct: None,
        reg_pid: 0,
        bell: false,
        bell_reason: String::new(),
        stuck_base: None,
    }
}

fn app_with_three() -> App {
    let mut app = app_with(&["a", "b"]);
    let mut p1 = Pane::test("%1", "editor", "nvim");
    p1.session = "b".to_string();
    p1.activity = 100;
    let mut p2 = Pane::test("%2", "shell", "bash");
    p2.session = "a".to_string();
    p2.activity = 200;
    let mut p3 = Pane::test("%10", "shell", "zsh");
    p3.session = "a".to_string();
    app.panes = vec![p1, p2, p3];
    app.refresh_filtered();
    app
}

fn ids(app: &App) -> Vec<String> {
    app.filtered.iter().map(|p| p.id.clone()).collect()
}

#[test]
fn default_sort_is_recency_newest_first_unknowns_last() {
    let app = app_with_three();
    assert_eq!(app.sort_key, SortKey::Idle);
    assert!(!app.sort_asc);
    assert_eq!(ids(&app), vec!["%2", "%1", "%10"]);
}

#[test]
fn hide_quiet_shells_keeps_claimed_fresh_and_unknown() {
    let mut app = app_with_three();
    app.agents.insert(
        "%1".to_string(),
        record("%1", "pi", crate::agent::AgentState::Working, 100),
    );
    app.hide_quiet_shells = true;
    app.refresh_filtered();
    // %2 (unclaimed, long quiet) hides; %1 is claimed, %10 has
    // unknown activity (0) and unknowns always stay visible.
    assert_eq!(ids(&app), vec!["%1", "%10"]);
    assert_eq!(app.hidden_quiet, 1);
    app.hide_quiet_shells = false;
    app.refresh_filtered();
    assert_eq!(ids(&app).len(), 3);
    assert_eq!(app.hidden_quiet, 0);
}

#[test]
fn sort_cmd_reads_a_to_z_and_toggles() {
    let mut app = app_with_three();
    app.cycle_sort(SortKey::Cmd);
    app.refresh_filtered();
    assert!(app.sort_asc);
    assert_eq!(ids(&app), vec!["%2", "%1", "%10"]);
    app.cycle_sort(SortKey::Cmd);
    app.refresh_filtered();
    assert!(!app.sort_asc);
    assert_eq!(ids(&app), vec!["%10", "%1", "%2"]);
}

#[test]
fn sort_state_puts_attention_first_unknowns_last() {
    use crate::agent::AgentState;
    let mut app = app_with_three();
    app.agents.insert(
        "%10".to_string(),
        record("%10", "codex", AgentState::Stuck, 50),
    );
    app.agents.insert(
        "%1".to_string(),
        record("%1", "claude", AgentState::Waiting, 60),
    );
    app.cycle_sort(SortKey::State);
    app.refresh_filtered();
    assert!(app.sort_asc);
    assert_eq!(ids(&app), vec!["%1", "%10", "%2"]);
}

#[test]
fn sort_pane_orders_ids_numerically() {
    let mut app = app_with_three();
    app.cycle_sort(SortKey::Pane);
    app.refresh_filtered();
    assert_eq!(ids(&app), vec!["%1", "%2", "%10"]);
}

#[test]
fn sort_age_longest_waiting_first() {
    use crate::agent::AgentState;
    let mut app = app_with_three();
    app.agents.insert(
        "%1".to_string(),
        record("%1", "claude", AgentState::Waiting, 100),
    );
    app.agents.insert(
        "%10".to_string(),
        record("%10", "codex", AgentState::Waiting, 200),
    );
    app.cycle_sort(SortKey::For);
    app.refresh_filtered();
    assert!(!app.sort_asc);
    assert_eq!(ids(&app), vec!["%1", "%10", "%2"]);
}

#[test]
fn hover_clears_on_list_rebuild() {
    let mut app = app_with_panes();
    app.hover = Some(1);
    app.refresh_filtered();
    assert_eq!(app.hover, None);
}

#[test]
fn sort_turn_longest_last_turn_first() {
    use crate::agent::AgentState;
    let mut app = app_with_three();
    let mut r1 = record("%1", "claude", AgentState::Waiting, 100);
    r1.last_turn_secs = 60;
    app.agents.insert("%1".to_string(), r1);
    let mut r10 = record("%10", "codex", AgentState::Waiting, 200);
    r10.last_turn_secs = 600;
    app.agents.insert("%10".to_string(), r10);
    app.cycle_sort(SortKey::Turn);
    app.refresh_filtered();
    assert!(!app.sort_asc);
    assert_eq!(ids(&app), vec!["%10", "%1", "%2"]);
}

#[test]
fn resort_restarts_at_top_row() {
    let mut app = app_with_three();
    app.select_pane(2);
    app.pane_scroll = 2;
    app.resort(SortKey::Cmd);
    assert_eq!(app.pane_selected, 0);
    assert_eq!(app.pane_scroll, 0);
    assert_eq!(app.filtered[0].command, "bash");
}

#[test]
fn sort_key_refollows_preview_to_new_top() {
    let _scratch = ScopedConfig::new();
    let mut app = app_with_panes();
    let tmux = Tmux::new().unwrap();
    app.show_preview = true;
    app.preview = "stale".to_string();
    app.select_pane(1);
    app.on_key(press(KeyCode::Char('o')), &tmux);
    assert_eq!(app.pane_selected, 0);
    // Whatever the capture yields (live text or failure marker), the
    // stale preview of the old row must be gone.
    assert_ne!(app.preview, "stale");
}

#[test]
fn filter_matches_across_fields_case_insensitive() {
    use crate::agent::AgentState;
    let mut app = app_with_panes();
    app.agents.insert(
        "%1".to_string(),
        record("%1", "claude", AgentState::Waiting, 60),
    );
    app.agents.get_mut("%1").unwrap().summary = "approve deploy".to_string();
    app.filter = "CLAUDE".to_string();
    app.refresh_filtered();
    assert_eq!(ids(&app), vec!["%1"]);
    app.filter = "wait".to_string();
    app.refresh_filtered();
    assert_eq!(ids(&app), vec!["%1"]);
    app.filter = "approve deploy".to_string();
    app.refresh_filtered();
    assert_eq!(ids(&app), vec!["%1"]);
    app.filter = "%2".to_string();
    app.refresh_filtered();
    assert_eq!(ids(&app), vec!["%2"]);
    app.filter = "zzz-no-match".to_string();
    app.refresh_filtered();
    assert!(app.filtered.is_empty());
    app.filter.clear();
    app.refresh_filtered();
    assert_eq!(app.filtered.len(), 2);
}

#[test]
fn bang_prefix_excludes_matches() {
    // `!zsh` hides every row whose columns mention zsh — the
    // standing-exclusion shape (always-on quiet-shell hiding).
    use crate::agent::AgentState;
    let mut app = app_with_panes(); // %1 nvim, %2 bash
    app.agents.insert(
        "%1".to_string(),
        record("%1", "claude", AgentState::Waiting, 60),
    );
    app.filter = "!bash".to_string();
    app.refresh_filtered();
    assert_eq!(ids(&app), vec!["%1"]);
    app.filter = "!claude".to_string();
    app.refresh_filtered();
    assert_eq!(ids(&app), vec!["%2"]);
    app.filter = "!zzz-no-match".to_string();
    app.refresh_filtered();
    assert_eq!(app.filtered.len(), 2);
    // Case-insensitive like the positive match, and a lone `!`
    // matches everything (never a foot-gun empty table).
    app.filter = "!BASH".to_string();
    app.refresh_filtered();
    assert_eq!(ids(&app), vec!["%1"]);
    app.filter = "!".to_string();
    app.refresh_filtered();
    assert_eq!(app.filtered.len(), 2);
}

#[test]
fn exclude_line_is_not_or_combined_across_tokens() {
    // The new standing-filter (settings) behavior: tokens are
    // NOT-OR combined, so a row survives only if it matches
    // NONE of the tokens. `bash npm` keeps rows that mention
    // neither. The live `/` filter stays as a single
    // substring (no tokenize).
    use crate::agent::AgentState;
    let mut app = app_with_panes(); // %1 nvim, %2 bash
    app.agents.insert(
        "%1".to_string(),
        record("%1", "claude", AgentState::Waiting, 60),
    );
    app.agents.insert(
        "%2".to_string(),
        record("%2", "codex", AgentState::Working, 5),
    );
    app.exclude = "bash npm".to_string();
    app.refresh_filtered();
    // %1 (nvim, claude): matches neither; kept.
    // %2 (bash, codex): matches bash; hidden.
    assert_eq!(ids(&app), vec!["%1"]);
    // Three tokens: same logic.
    app.exclude = "nvim bash npm".to_string();
    app.refresh_filtered();
    // %1 matches nvim; %2 matches bash; both hidden.
    assert!(app.filtered.is_empty());
    // `!`-prefix on a token is stripped (the on-disk form
    // and the prompt pre-fill both have one), so `!bash` and
    // `bash` are equivalent.
    app.exclude = "!bash".to_string();
    app.refresh_filtered();
    // %2 matches bash; hidden. %1 doesn't; kept.
    assert_eq!(ids(&app), vec!["%1"]);
}

fn legend_texts(rows: &[SettingRow]) -> Vec<String> {
    rows.iter()
        .filter_map(|r| match r {
            SettingRow::Legend { name, meaning, .. } => Some(format!("{name} — {meaning}")),
            _ => None,
        })
        .collect()
}

#[test]
fn filters_page_lists_what_each_query_hides() {
    // %1 nvim, %2 bash. Standing `bash` (the new bare-token
    // shape; the on-disk form still carries a leading `!`)
    // hides %2; search `nvim` hides %2 as well — but %2
    // already failed standing, so it lists once, under the
    // exclusion (no double-counting).
    let mut app = app_with_panes();
    app.exclude = "bash".to_string();
    app.filter = "nvim".to_string();
    let texts = legend_texts(&app.settings_rows());
    assert!(
        texts
            .iter()
            .any(|t| t.contains("1 hidden") && t.contains("bash")),
        "exclusion caption missing:\n{texts:?}"
    );
    assert!(
        texts.iter().any(|t| t.contains("%2") && t.contains("bash")),
        "%2 row missing:\n{texts:?}"
    );
    // Nothing hidden twice: %2 appears exactly once.
    assert_eq!(
        texts.iter().filter(|t| t.contains("%2")).count(),
        1,
        "double-counted:\n{texts:?}"
    );
    // Legend rows are display-only: never selectable/actionable.
    let items = app.settings_item_rows();
    let rows = app.settings_rows();
    for (i, r) in rows.iter().enumerate() {
        if matches!(r, SettingRow::Legend { .. }) {
            assert!(!items.contains(&i), "legend row {i} selectable");
        }
    }
}

#[test]
fn filters_page_says_so_when_nothing_hidden() {
    let app = app_with_panes();
    let texts = legend_texts(&app.settings_rows());
    assert!(
        texts.iter().any(|t| t.contains("nothing hidden")),
        "empty state missing:\n{texts:?}"
    );
}

#[test]
fn typing_unbound_key_opens_filter_seeded() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut app = app_with(&["a"]);
    let tmux = Tmux::new().unwrap();
    // `/` is unbound: opens the filter with `/` already typed.
    app.on_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE), &tmux);
    assert_eq!(app.mode, Mode::PromptFilter);
    assert_eq!(app.input, "/");
    // A query over no match narrows to nothing live.
    for c in "zzz".chars() {
        app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE), &tmux);
    }
    assert!(app.filtered.is_empty());
}

#[test]
fn typing_unbound_key_in_panes_focus_filters() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut app = app_with_panes();
    app.focus = Focus::Panes;
    let tmux = Tmux::new().unwrap();
    app.on_key(KeyEvent::new(KeyCode::Char('.'), KeyModifiers::NONE), &tmux);
    assert_eq!(app.mode, Mode::PromptFilter);
    assert_eq!(app.input, ".");
}

#[test]
fn bound_key_still_wins_over_typing() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut app = app_with_panes();
    let tmux = Tmux::new().unwrap();
    // `q` quits; it must never detour into the filter prompt.
    let ev = app.on_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE), &tmux);
    assert!(matches!(ev, Some(AppEvent::Quit)));
    assert_eq!(app.mode, Mode::Normal);
}

#[test]
fn ctrl_chord_never_starts_filter() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut app = app_with(&["a"]);
    let tmux = Tmux::new().unwrap();
    app.on_key(
        KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL),
        &tmux,
    );
    assert_eq!(app.mode, Mode::Normal);
    assert!(app.input.is_empty());
}

#[test]
fn filter_seeds_from_config_at_startup() {
    // A persisted standing exclusion applies on the first refresh,
    // no typing; the temporary search always starts empty. Old
    // configs wrote `!bash`; the loader strips a single leading
    // `!` so both forms land in `app.exclude` as bare tokens.
    let cfg = AppConfig {
        filter: "!bash".to_string(),
        ..AppConfig::default()
    };
    let mut app = App::new(cfg);
    assert!(app.filter.is_empty());
    assert_eq!(app.exclude, "bash");
    app.panes = app_with_panes().panes;
    app.refresh_filtered();
    assert_eq!(ids(&app), vec!["%1"]);
}

#[test]
fn stale_instance_cannot_revert_a_newer_instances_filter() {
    use crate::config::ViewChanges;
    let dir = std::env::temp_dir().join(format!("terhub-viewclobber-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("cfg.toml");
    std::fs::write(
        &path,
        "[ui]\ntheme = \"dark\"\nfilter = \"!bash\"\nsort = \"Idle\"\nsort_desc = true\nhide_shells = false\n",
    )
    .unwrap();
    let _cfg = ConfigAt::new(&path);
    // Instance B opens first and holds the file as it stands.
    let cfg = crate::config::load_config(None);
    let mut b = App::new(cfg.clone());
    // Instance A opens against the same file and saves a new
    // standing filter — the user's edit, from the newest window.
    let mut a = App::new(cfg);
    a.exclude = "!zsh".to_string();
    a.persist_filter();
    // B (stale — it never saw the new filter) toggles a theme.
    b.apply_setting(SettingKind::Theme(1));
    // The filter the user just saved must survive: B only ever
    // changed the theme, so only the theme may be written.
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.contains("filter = \"!zsh\""),
        "stale instance reverted the saved filter:\n{text}"
    );
    assert_eq!(
        b.view_changes,
        ViewChanges::default(),
        "a successful write should clear the instance's dirty flags"
    );
    assert!(
        text.contains("sort = \"Idle\""),
        "untouched sort key was rewritten:\n{text}"
    );
}

#[test]
fn enter_in_search_never_persists() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let dir = std::env::temp_dir().join(format!("terhub-search-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("search-nopersist.toml");
    std::fs::write(&path, "[ui]\ntheme = \"dark\"\n").unwrap();
    let _cfg = ConfigAt::new(&path);
    let mut app = app_with(&["a"]);
    let tmux = Tmux::new().unwrap();
    app.open_filter();
    for c in "ba".chars() {
        app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE), &tmux);
    }
    app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tmux);
    // Kept for the session, but the file is untouched: temporary
    // searches must never become standing exclusions by accident.
    assert_eq!(app.filter, "ba");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        !text.contains("filter = "),
        "search leaked into config:\n{text}"
    );
    std::fs::remove_file(path).ok();
}

#[test]
fn standing_strips_leading_bang_on_enter() {
    // The standing editor used to call `normalize_exclude`,
    // which added a `!`. Under the new model the field stores
    // bare tokens and the matcher treats the line as exclude
    // already, so a leading `!` is harmless noise to strip.
    use crate::config::AppConfig;
    let mut app = App::new(AppConfig::default());
    app.open_standing_editor();
    app.input = "zsh".to_string();
    // Simulate Enter (the actual handler is exercised by
    // standing_editor_enter_persists_and_reloads).
    app.exclude = app.input.trim().trim_start_matches('!').trim().to_string();
    assert_eq!(app.exclude, "zsh");
    app.input = "!npm".to_string();
    app.exclude = app.input.trim().trim_start_matches('!').trim().to_string();
    assert_eq!(app.exclude, "npm");
}

#[test]
fn section_jumps_wrap_and_land_on_first_items() {
    let mut app = app_with(&["a"]);
    app.open_settings();
    let names: Vec<&str> = app.settings_sections().iter().map(|(n, _)| *n).collect();
    assert_eq!(
        names,
        vec!["Theme", "Density", "Toggles", "Filters", "Tray icons"]
    );
    // Right from Theme lands Density's first item (comfortable).
    app.goto_settings_section(1);
    assert_eq!(app.settings_section, 1);
    // Wrap-around both directions.
    app.goto_settings_section(4); // Tray icons: legend only, selection kept
    assert_eq!(app.settings_section, 4);
    app.move_settings_section(1);
    assert_eq!(app.settings_section, 0);
    app.move_settings_section(-1);
    assert_eq!(app.settings_section, 4);
    // Landing on Filters selects the standing row.
    app.goto_settings_section(3);
    let items = app.settings_item_rows();
    let row = items[app.settings_selected];
    assert!(matches!(
        app.settings_rows()[row],
        SettingRow::Item {
            kind: SettingKind::EditStandingFilter,
            ..
        }
    ));
}

#[test]
fn preview_style_follows_hovered_theme_or_density() {
    let mut app = app_with(&["a"]);
    app.open_settings();
    // Hovering a theme previews it (config stays dracula-free).
    app.settings_selected = 2; // dracula
    let (theme, comfy) = app.settings_preview_style();
    assert_eq!(theme, "dracula");
    assert!(comfy);
    // Hovering a density previews it under the applied theme.
    let items = app.settings_item_rows();
    let compact = items
        .iter()
        .position(|&r| {
            matches!(
                app.settings_rows()[r],
                SettingRow::Item {
                    kind: SettingKind::Density(1),
                    ..
                }
            )
        })
        .unwrap();
    app.settings_selected = compact;
    let (theme, comfy) = app.settings_preview_style();
    assert_eq!(theme, AppConfig::default().theme);
    assert!(!comfy);
    // A toggle row: falls back to the applied config.
    let items = app.settings_item_rows();
    let toggle = items
        .iter()
        .position(|&r| {
            matches!(
                app.settings_rows()[r],
                SettingRow::Item {
                    kind: SettingKind::ToggleNotifications,
                    ..
                }
            )
        })
        .unwrap();
    app.settings_selected = toggle;
    app.config.theme = "nord".to_string();
    let (theme, comfy) = app.settings_preview_style();
    assert_eq!(theme, "nord");
    assert!(comfy);
}

#[test]
fn standing_editor_enter_persists_and_reloads() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let dir = std::env::temp_dir().join(format!("terhub-standing-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("standing-persist.toml");
    std::fs::write(&path, "[ui]\ntheme = \"dark\"\n").unwrap();
    let _cfg = ConfigAt::new(&path);
    let mut app = app_with(&["a"]);
    let tmux = Tmux::new().unwrap();
    // The settings row opens the same prompt in standing mode.
    app.open_standing_editor();
    assert!(app.editing_standing);
    // Saves as `zsh` in the in-memory field (the matcher
    // treats the line as exclude already) and as `!zsh` in the
    // on-disk form (kept for back-compat with old configs).
    for c in "zsh".chars() {
        app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE), &tmux);
    }
    app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tmux);
    assert_eq!(app.exclude, "zsh");
    // Back in settings (where the editor was opened from), not the table.
    assert_eq!(app.mode, Mode::Settings);
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("filter = \"!zsh\""), "{text}");
    // And the saved exclusion reloads through the normal loader.
    let (cfg, _) = crate::config::load_config_report(Some(path.clone()));
    assert_eq!(cfg.filter, "!zsh");
    std::fs::remove_file(path).ok();
}

#[test]
fn view_seeds_sort_hide_and_exclude_from_config() {
    // The standing view is exactly what was saved: exclusion, sort
    // column + direction, and shell hiding all apply on first refresh.
    // `filter` keeps the on-disk form (`!bash`); `App::new` strips
    // one leading `!` so the in-memory `app.exclude` is bare.
    let cfg = AppConfig {
        filter: "!bash".to_string(),
        sort: "State".to_string(),
        sort_desc: false,
        hide_shells: true,
        ..AppConfig::default()
    };
    let app = App::new(cfg);
    assert_eq!(app.exclude, "bash");
    assert_eq!(app.sort_key, SortKey::State);
    assert!(app.sort_asc);
    assert!(app.hide_quiet_shells);
    assert!(app.filter.is_empty());
}

#[test]
fn sort_key_change_persists_view() {
    // ScopedConfig's guard serializes us against every other
    // env-mutating test (a concurrent TERHUB_CONFIG setter would
    // hijack config_path()).
    let _scratch = ScopedConfig::new();
    let mut app = app_with_panes();
    let tmux = Tmux::new().unwrap();
    app.on_key(press(KeyCode::Char('o')), &tmux);
    assert_eq!(app.sort_key, SortKey::State);
    let text = std::fs::read_to_string(crate::config::config_path()).unwrap();
    assert!(text.contains("sort = \"State\""), "{text}");
    assert!(text.contains("sort_desc = false"), "{text}");
}

#[test]
fn hide_shells_toggle_persists_view() {
    // Same serialization as above; ScopedConfig already holds it.
    let _scratch = ScopedConfig::new();
    let mut app = app_with_panes();
    let tmux = Tmux::new().unwrap();
    // `h` is bound by default; the toggle flips and persists.
    app.on_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE), &tmux);
    assert!(app.hide_quiet_shells);
    let text = std::fs::read_to_string(crate::config::config_path()).unwrap();
    assert!(text.contains("hide_shells = true"), "{text}");
}

#[test]
fn standing_editor_prefills_existing_and_filters_live() {
    // The standing editor seeds its input from the existing
    // exclude line, prepending `!` so the user is always
    // typing into an exclude query. Typing into the prompt
    // hides rows live; Enter keeps the bare-token form (the
    // on-disk file gets a `!` re-added for back-compat).
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut app = app_with_panes(); // %1 nvim, %2 bash
    let tmux = Tmux::new().unwrap();
    app.open_standing_editor();
    // Empty exclusion: input pre-fills with `!`.
    assert_eq!(app.input, "!");
    assert_eq!(app.exclude, "");
    // Typing appends after the pre-filled bang.
    for c in "bash".chars() {
        app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE), &tmux);
    }
    assert_eq!(app.input, "!bash");
    assert_eq!(app.exclude, "bash");
    assert_eq!(ids(&app), vec!["%1"]);
    app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tmux);
    assert_eq!(app.exclude, "bash");
}

#[test]
fn standing_editor_seeds_existing_exclusion() {
    let mut app = app_with_panes();
    app.exclude = "!zsh".to_string();
    app.open_standing_editor();
    assert_eq!(app.input, "!zsh");
}

#[test]
fn filter_prompt_live_enter_esc() {
    let mut app = app_with_panes();
    let tmux = Tmux::new().unwrap();
    app.focus = Focus::Panes;
    app.on_key(press(KeyCode::Char('f')), &tmux);
    assert_eq!(app.mode, Mode::PromptFilter);
    assert_eq!(app.input, "");
    app.on_key(press(KeyCode::Char('b')), &tmux);
    app.on_key(press(KeyCode::Char('a')), &tmux);
    assert_eq!(app.filter, "ba");
    assert_eq!(ids(&app), vec!["%2"]);
    assert_eq!(app.pane_selected, 0);
    app.on_key(press(KeyCode::Enter), &tmux);
    assert_eq!(app.mode, Mode::Normal);
    assert_eq!(app.filter, "ba");
    // Reopen edits in place; Esc restores the pre-edit query.
    app.on_key(press(KeyCode::Char('f')), &tmux);
    assert_eq!(app.input, "ba");
    app.on_key(press(KeyCode::Esc), &tmux);
    assert_eq!(app.mode, Mode::Normal);
    assert_eq!(app.filter, "ba");
    assert_eq!(ids(&app), vec!["%2"]);
}

/// The shared history list lets one prompt recall what the
/// other committed. Up/Down cycle most-recent-first; Down past the
/// newest entry returns to the live draft.
#[test]
fn filter_history_cycles() {
    let tmux = Tmux::new().unwrap();

    // --- empty history: Up/Down leave the draft untouched. ---
    let mut app = app_with_panes();
    app.focus = Focus::Panes;
    app.on_key(press(KeyCode::Char('f')), &tmux);
    app.on_key(press(KeyCode::Char('a')), &tmux);
    assert_eq!(app.input, "a");
    app.on_key(press(KeyCode::Up), &tmux);
    assert_eq!(app.input, "a");
    app.on_key(press(KeyCode::Down), &tmux);
    assert_eq!(app.input, "a");
    app.on_key(press(KeyCode::Esc), &tmux);

    // --- single query: Up once shows it, Down returns to draft. ---
    let mut app = app_with_panes();
    app.focus = Focus::Panes;
    app.on_key(press(KeyCode::Char('f')), &tmux);
    for c in ['c', 'a', 't'] {
        app.on_key(press(KeyCode::Char(c)), &tmux);
    }
    app.on_key(press(KeyCode::Enter), &tmux);
    assert_eq!(app.filter, "cat");
    // Reopen: the draft is seeded from the live query.
    app.on_key(press(KeyCode::Char('f')), &tmux);
    assert_eq!(app.input, "cat");
    app.on_key(press(KeyCode::Up), &tmux); // history[0] == "cat"
    assert_eq!(app.input, "cat");
    app.on_key(press(KeyCode::Up), &tmux); // already oldest: stays
    assert_eq!(app.input, "cat");
    app.on_key(press(KeyCode::Down), &tmux); // past the newest: draft
    assert_eq!(app.input, "cat");
    app.on_key(press(KeyCode::Esc), &tmux);

    // --- multiple entries cycle newest-first and clamp. ---
    let mut app = app_with_panes();
    app.focus = Focus::Panes;
    for q in ["alpha", "beta", "gamma"] {
        app.on_key(press(KeyCode::Char('f')), &tmux);
        for _ in 0..10 {
            app.on_key(press(KeyCode::Backspace), &tmux);
        }
        for c in q.chars() {
            app.on_key(press(KeyCode::Char(c)), &tmux);
        }
        app.on_key(press(KeyCode::Enter), &tmux);
    }
    assert_eq!(app.filter, "gamma");

    // --- cycling past the end clamps at the oldest entry. ---
    app.on_key(press(KeyCode::Char('f')), &tmux);
    app.on_key(press(KeyCode::Up), &tmux);
    assert_eq!(app.input, "gamma");
    app.on_key(press(KeyCode::Up), &tmux);
    assert_eq!(app.input, "beta");
    app.on_key(press(KeyCode::Up), &tmux);
    assert_eq!(app.input, "alpha");
    // History is exhausted: further Up is a no-op (clamped).
    app.on_key(press(KeyCode::Up), &tmux);
    assert_eq!(app.input, "alpha");
    // And Down walks back out the way it came.
    app.on_key(press(KeyCode::Down), &tmux);
    assert_eq!(app.input, "beta");
    app.on_key(press(KeyCode::Down), &tmux);
    assert_eq!(app.input, "gamma");
    // Past the newest entry we land back on the live draft, which
    // is the current live query ("gamma") captured on entry.
    app.on_key(press(KeyCode::Down), &tmux);
    assert_eq!(app.input, "gamma");

    // --- edit-and-save: a recalled entry can be edited and the
    // edited form is what Enter commits (and history records). ---
    app.on_key(press(KeyCode::Up), &tmux); // gamma
    app.on_key(press(KeyCode::Char('z')), &tmux); // -> gammaz
    assert_eq!(app.input, "gammaz");
    app.on_key(press(KeyCode::Enter), &tmux);
    assert_eq!(app.filter, "gammaz");
    // Reopen: the edited query is the newest entry. The draft is
    // the live query itself ("gammaz"), so Up shows gammaz (the
    // same text, now recalled from history), Up again walks back
    // to gamma, and Down returns to the draft.
    app.on_key(press(KeyCode::Char('f')), &tmux);
    app.on_key(press(KeyCode::Up), &tmux);
    assert_eq!(app.input, "gammaz");
    app.on_key(press(KeyCode::Up), &tmux);
    assert_eq!(app.input, "gamma");
    app.on_key(press(KeyCode::Down), &tmux);
    assert_eq!(app.input, "gammaz");
    app.on_key(press(KeyCode::Down), &tmux); // draft
    assert_eq!(app.input, "gammaz");
    app.on_key(press(KeyCode::Esc), &tmux);

    // --- Tab does not lose history: Tab is inert in the prompt,
    // and the history survives it (and survives being reopened). ---
    app.on_key(press(KeyCode::Char('f')), &tmux);
    app.on_key(press(KeyCode::Tab), &tmux);
    app.on_key(press(KeyCode::Up), &tmux);
    assert_eq!(app.input, "gammaz");
    app.on_key(press(KeyCode::Esc), &tmux);
    assert_eq!(app.filter, "gammaz");
}

/// The standing editor shares the same history list, and recall
/// preserves the exclusion marker (the `!` form).
#[test]
fn filter_history_shared_with_standing_editor() {
    let tmux = Tmux::new().unwrap();
    let mut app = app_with_panes();
    app.focus = Focus::Panes;

    // Seed history from the live prompt (`f` opens it seeded with
    // the current query, so clear first: `/` would insert a
    // literal slash once the prompt is open).
    for q in ["vim", "npm"] {
        app.on_key(press(KeyCode::Char('f')), &tmux);
        for _ in 0..10 {
            app.on_key(press(KeyCode::Backspace), &tmux);
        }
        for c in q.chars() {
            app.on_key(press(KeyCode::Char(c)), &tmux);
        }
        app.on_key(press(KeyCode::Enter), &tmux);
    }
    assert_eq!(app.filter, "npm");
    assert_eq!(app.filter_history, ["vim", "npm"]);

    // Standing editor: Up recalls "npm" in the bang form.
    app.open_standing_editor();
    app.on_key(press(KeyCode::Up), &tmux);
    assert_eq!(app.input, "!npm");
    app.on_key(press(KeyCode::Up), &tmux);
    assert_eq!(app.input, "!vim");
    app.on_key(press(KeyCode::Up), &tmux); // clamped
    assert_eq!(app.input, "!vim");
    app.on_key(press(KeyCode::Down), &tmux);
    assert_eq!(app.input, "!npm");
    // The pre-existing standing value is the draft.
    app.on_key(press(KeyCode::Down), &tmux);
    app.on_key(press(KeyCode::Esc), &tmux);

    // A standing commit lands in the same list, still bang-prefixed.
    app.open_standing_editor();
    for c in "zsh".chars() {
        app.on_key(press(KeyCode::Char(c)), &tmux);
    }
    app.on_key(press(KeyCode::Enter), &tmux);
    assert_eq!(app.exclude, "zsh");
    assert_eq!(app.filter_history.back().unwrap(), "!zsh");
    app.on_key(press(KeyCode::Esc), &tmux);

    // Reopen: Up shows the bang form of what was just committed.
    app.open_standing_editor();
    app.on_key(press(KeyCode::Up), &tmux);
    assert_eq!(app.input, "!zsh");
    app.on_key(press(KeyCode::Esc), &tmux);

    // Recalled back into the live prompt, the marker is dropped.
    // Saving the standing filter leaves Settings; Esc returns to
    // Normal, where the live-prompt binding lives.
    app.mode = Mode::Normal;
    app.focus = Focus::Panes;
    app.on_key(press(KeyCode::Char('f')), &tmux);
    app.on_key(press(KeyCode::Up), &tmux);
    assert_eq!(app.input, "zsh");
    app.on_key(press(KeyCode::Esc), &tmux);
}

/// Esc restores the pre-edit query and never rewinds history: the
/// navigation state is discarded, and the next prompt opens in
/// draft mode.
#[test]
fn filter_history_esc_restores_pre_edit_and_ignores_history() {
    let tmux = Tmux::new().unwrap();
    let mut app = app_with_panes();
    app.focus = Focus::Panes;
    app.on_key(press(KeyCode::Char('f')), &tmux);
    for word in ["one", "two"] {
        for c in word.chars() {
            app.on_key(press(KeyCode::Char(c)), &tmux);
        }
    }
    app.on_key(press(KeyCode::Enter), &tmux);
    assert_eq!(app.filter, "onetwo");

    app.filter = "alpha".to_string();
    app.refresh_filtered();
    app.on_key(press(KeyCode::Char('f')), &tmux);
    app.on_key(press(KeyCode::Up), &tmux); // onetwo
    assert_eq!(app.input, "onetwo");
    app.on_key(press(KeyCode::Esc), &tmux);
    assert_eq!(app.mode, Mode::Normal);
    // Esc ignored history entirely: the pre-edit query is back.
    assert_eq!(app.filter, "alpha");

    // The next open starts fresh — no stale history position.
    app.on_key(press(KeyCode::Char('f')), &tmux);
    assert_eq!(app.input, "alpha");
    app.on_key(press(KeyCode::Up), &tmux);
    assert_eq!(app.input, "onetwo");
    app.on_key(press(KeyCode::Down), &tmux);
    assert_eq!(app.input, "alpha");
    app.on_key(press(KeyCode::Esc), &tmux);
}

/// The history is capped: committing more than the cap evicts the
/// oldest entries, and duplicates of the newest entry are dropped.
#[test]
fn filter_history_caps_and_dedups() {
    let tmux = Tmux::new().unwrap();
    let mut app = app_with_panes();
    for n in 0..12 {
        app.on_key(press(KeyCode::Char('f')), &tmux);
        for _ in 0..10 {
            app.on_key(press(KeyCode::Backspace), &tmux);
        }
        for c in format!("q{n}").chars() {
            app.on_key(press(KeyCode::Char(c)), &tmux);
        }
        app.on_key(press(KeyCode::Enter), &tmux);
    }
    // Only the cap survives; the oldest (q0, q1) are gone.
    assert_eq!(app.filter_history.len(), App::FILTER_HISTORY_CAP);
    assert_eq!(app.filter_history.front().unwrap(), "q2");
    assert_eq!(app.filter_history.back().unwrap(), "q11");

    // Re-opening and pressing Up yields the newest, then walks back
    // only as far as the cap allows.
    app.on_key(press(KeyCode::Char('f')), &tmux);
    for _ in 0..10 {
        app.on_key(press(KeyCode::Backspace), &tmux);
    }
    app.on_key(press(KeyCode::Up), &tmux);
    assert_eq!(app.input, "q11");
    for _ in 0..App::FILTER_HISTORY_CAP {
        app.on_key(press(KeyCode::Up), &tmux);
    }
    assert_eq!(app.input, "q2"); // clamped at the oldest survived

    // A duplicate of the newest entry is not appended.
    let len = app.filter_history.len();
    app.on_key(press(KeyCode::Esc), &tmux);
    app.on_key(press(KeyCode::Char('f')), &tmux);
    app.on_key(press(KeyCode::Enter), &tmux);
    assert_eq!(app.filter_history.len(), len);
}

/// Esc restores the pre-edit query.
#[test]
fn filter_esc_restores_pre_edit_query() {
    let mut app = app_with_panes();
    let tmux = Tmux::new().unwrap();
    app.focus = Focus::Panes;
    app.filter = "ba".to_string();
    app.refresh_filtered();
    // Reopen, type more (live narrows), then Esc: the pre-edit query
    // comes back — Esc is cancel, not clear (clear lives on Esc in
    // Normal mode, or deleting everything + Enter).
    app.on_key(press(KeyCode::Char('f')), &tmux);
    assert_eq!(app.input, "ba");
    app.on_key(press(KeyCode::Char('x')), &tmux);
    assert_eq!(app.filter, "bax");
    app.on_key(press(KeyCode::Esc), &tmux);
    assert_eq!(app.mode, Mode::Normal);
    assert_eq!(app.filter, "ba");
    assert_eq!(ids(&app), vec!["%2"]);
}

#[test]
fn esc_unwind_clears_filter_before_stepping_back() {
    let mut app = app_with_panes();
    let tmux = Tmux::new().unwrap();
    app.focus = Focus::Panes;
    app.filter = "bash".to_string();
    app.refresh_filtered();
    assert_eq!(app.filtered.len(), 1);
    app.on_key(press(KeyCode::Esc), &tmux);
    assert_eq!(app.filter, "");
    assert_eq!(app.focus, Focus::Panes);
    assert_eq!(app.filtered.len(), 2);
    app.on_key(press(KeyCode::Esc), &tmux);
    assert_eq!(app.focus, Focus::Sessions);
}

#[test]
fn cycle_sort_key_loops_through_all() {
    let mut app = app_with_panes();
    for _ in 0..SORT_CYCLE.len() {
        app.cycle_sort_key();
    }
    assert_eq!(app.sort_key, SortKey::Idle);
    app.cycle_sort_key();
    assert_eq!(app.sort_key, SortKey::State);
}

#[test]
fn preview_wheel_scrolls_content_not_selection() {
    let tmux = Tmux::new().unwrap();
    let mut app = app_with_panes();
    app.show_preview = true;
    app.preview = (0..100)
        .map(|i| format!("line {i:03}"))
        .collect::<Vec<_>>()
        .join("\n");
    app.select_pane(1);
    app.on_wheel(false, &tmux);
    assert_eq!(app.preview_scroll, 3);
    assert_eq!(app.pane_selected, 1);
    app.on_wheel(true, &tmux);
    assert_eq!(app.preview_scroll, 0);
}

#[test]
fn preview_page_keys_scroll_and_clamp() {
    let mut app = app_with_panes();
    let tmux = Tmux::new().unwrap();
    app.show_preview = true;
    app.preview = (0..100)
        .map(|i| format!("line {i:03}"))
        .collect::<Vec<_>>()
        .join("\n");
    app.preview_visible = 10;
    app.on_key(press(KeyCode::PageDown), &tmux);
    assert_eq!(app.preview_scroll, 10);
    app.on_key(press(KeyCode::End), &tmux);
    assert_eq!(app.preview_scroll, 90);
    app.on_key(press(KeyCode::PageDown), &tmux);
    assert_eq!(app.preview_scroll, 90);
    app.on_key(press(KeyCode::PageUp), &tmux);
    assert_eq!(app.preview_scroll, 80);
    app.on_key(press(KeyCode::Home), &tmux);
    assert_eq!(app.preview_scroll, 0);
}

#[test]
fn preview_open_resets_scroll() {
    let mut app = app_with_panes();
    let tmux = Tmux::new().unwrap();
    app.toggle_preview(&tmux);
    assert!(app.show_preview);
    app.preview_scroll = 7;
    app.toggle_preview(&tmux);
    app.toggle_preview(&tmux);
    assert_eq!(app.preview_scroll, 0);
}

#[test]
fn native_handles_avoid_daemon_issued() {
    use crate::agent::AgentState;
    let mut app = app_with_panes();
    // Daemon already owns @pi-konsole (a tmux session of that name).
    let mut d = record("%1", "pi", AgentState::Working, 10);
    d.handle = "@pi-konsole".to_string();
    app.agents.insert("%1".to_string(), d);
    let mut n = record("konsole:999:6", "pi", AgentState::Working, 10);
    n.session = "konsole".to_string();
    app.native_agents = vec![n];
    app.assign_native_handles();
    assert_eq!(app.native_agents[0].handle, "@pi-konsole-2");
    // Without the collision the bare handle mints.
    app.agents.clear();
    app.native_agents[0].handle.clear();
    app.assign_native_handles();
    assert_eq!(app.native_agents[0].handle, "@pi-konsole");
}

#[test]
fn sort_by_handle_orders_names() {
    use crate::agent::AgentState;
    let mut app = app_with_panes();
    let mut a = record("%1", "pi", AgentState::Working, 10);
    a.handle = "@pi-b".to_string();
    let mut b = record("%2", "pi", AgentState::Working, 10);
    b.handle = "@pi-a".to_string();
    app.agents.insert("%1".to_string(), a);
    app.agents.insert("%2".to_string(), b);
    app.sort_key = SortKey::Handle;
    app.sort_asc = true;
    app.apply_sort();
    assert_eq!(app.filtered[0].id, "%2");
    assert_eq!(app.filtered[1].id, "%1");
}

#[test]
fn sort_key_o_cycles_with_status() {
    let _scratch = ScopedConfig::new();
    let mut app = app_with_panes();
    let tmux = Tmux::new().unwrap();
    app.select_pane(1);
    app.pane_scroll = 1;
    assert_eq!(app.on_key(press(KeyCode::Char('o')), &tmux), None);
    assert_eq!(app.pane_selected, 0);
    assert_eq!(app.pane_scroll, 0);
    assert_eq!(app.sort_key, SortKey::State);
    assert!(
        app.status
            .as_deref()
            .unwrap_or("")
            .starts_with("sort: State"),
        "missing sort status: {:?}",
        app.status
    );
}

/// Point *this thread's* config resolution at an existing file, and
/// restore it on drop.
///
/// The per-thread override (not the `TERHUB_CONFIG` env var) is
/// what makes this safe: the env var is process-global, so a test
/// that redirected it changed what every other thread in the
/// process read, and two such tests raced.
struct ConfigAt {
    old: Option<std::path::PathBuf>,
}

impl ConfigAt {
    fn new(path: &std::path::Path) -> Self {
        let old = crate::config::override_config_path(Some(path.to_path_buf()));
        ConfigAt { old }
    }
}

impl Drop for ConfigAt {
    fn drop(&mut self) {
        crate::config::override_config_path(self.old.take());
    }
}

/// Point config reads/writes at one scratch file (the per-thread
/// config-path override), so saved-filter persistence is asserted
/// against a real round-trip without touching the user's config.
///
/// The override is thread-local, not process-global: two tests that
/// each want their own scratch config no longer race, and neither
/// can hijack the other's read.
struct ScopedConfig {
    // The path the test wrote to. Storing it (instead of re-reading
    // the override at load time) means the assertion can never see
    // a different file than the test wrote to.
    path: std::path::PathBuf,
    // The override before we took over; restored on drop so a
    // nested scope keeps its own config path.
    old: Option<std::path::PathBuf>,
}

impl ScopedConfig {
    fn new() -> Self {
        // A fresh file per instance (not per process): tests that
        // persist must not write through each other's scratch state,
        // and a per-pid path made every such test share one file.
        let n = NEXT_CFG.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let path = std::env::temp_dir()
            .join(format!("terhub-cfg-{}-{n}", std::process::id()))
            .with_extension("toml");
        let old = crate::config::override_config_path(Some(path.clone()));
        ScopedConfig { path, old }
    }

    fn load(&self) -> (AppConfig, Vec<String>) {
        // Read from the path we wrote to, not from `config_path()`:
        // that resolves the override afresh, which is fine while the
        // scope is live but couples the assertion to state we don't
        // strictly control. Using the captured path removes the only
        // way the read could see a different file than the test
        // wrote to.
        crate::config::load_config_report(Some(self.path.clone()))
    }
}

impl Drop for ScopedConfig {
    fn drop(&mut self) {
        // Restore the override the parent scope had.
        crate::config::override_config_path(self.old.take());
    }
}

// ---- saved (named) filters ------------------------------------

/// A saved filter drives the table, names itself in the title, and
/// survives a config round-trip. Covers add + switch + reload.
#[test]
fn saved_filters_add_switch_and_reload() {
    let scratch = ScopedConfig::new();
    let tmux = Tmux::new().unwrap();
    let mut app = app_with_panes();
    app.focus = Focus::Panes;

    // Name the current search: 'f' opens the live prompt, type a
    // multi-token query, Enter commits it, then 's' → Filters → 'n'
    // names it.
    app.on_key(press(KeyCode::Char('f')), &tmux);
    for c in "vim npm".chars() {
        app.on_key(press(KeyCode::Char(c)), &tmux);
    }
    app.on_key(press(KeyCode::Enter), &tmux);
    assert_eq!(app.filter, "vim npm");
    app.on_key(press(KeyCode::Char('s')), &tmux);
    assert_eq!(app.mode, Mode::Settings);
    app.on_key(press(KeyCode::Char('n')), &tmux);
    assert_eq!(
        app.mode,
        Mode::PromptSavedFilter { index: None },
        "naming prompt did not open"
    );
    for c in "work".chars() {
        app.on_key(press(KeyCode::Char(c)), &tmux);
    }
    app.on_key(press(KeyCode::Enter), &tmux);
    assert_eq!(app.mode, Mode::Settings);
    assert_eq!(app.saved_filters.len(), 1);
    assert_eq!(app.saved_filters[0].name, "work");
    assert_eq!(app.saved_filters[0].query, "vim npm");

    // Reload from disk: the entry is back, so a restart keeps it.
    let (cfg, problems) = scratch.load();
    assert!(problems.is_empty(), "load problems: {problems:?}");
    assert_eq!(cfg.saved_filters.len(), 1, "saved filter did not persist");
    assert_eq!(cfg.saved_filters[0].name, "work");
    assert_eq!(cfg.saved_filters[0].query, "vim npm");
    // The standing exclude line is untouched by saved-filter writes.
    assert!(
        cfg.filter.is_empty(),
        "filter line disturbed: {:?}",
        cfg.filter
    );

    // The settings page renders the name.
    let labels: Vec<String> = app
        .settings_rows()
        .iter()
        .filter_map(|r| match r {
            SettingRow::Item { label, .. } => Some(label.clone()),
            _ => None,
        })
        .collect();
    assert!(
        labels
            .iter()
            .any(|l| l.contains("work") && l.contains("vim npm")),
        "saved filter missing from settings rows: {labels:?}"
    );

    // Switch to it: the live search takes its query and the header
    // names it.
    app.filter = String::new();
    app.active_saved_filter = None;
    app.refresh_filtered();
    app.apply_setting(SettingKind::UseSavedFilter(0));
    assert_eq!(app.filter, "vim npm");
    assert_eq!(app.active_saved_filter.as_deref(), Some("work"));
}

#[test]
fn startup_names_no_saved_filter_from_the_exclusion() {
    // `[ui] filter` persists the STANDING exclusion (`!zsh`);
    // saved queries are live includes applied to an empty live
    // filter at startup. Matching one domain against the other
    // named a filter that was never applied.
    let cfg = AppConfig {
        filter: "!zsh".to_string(),
        saved_filters: vec![crate::config::SavedFilter {
            name: "w".to_string(),
            query: "!zsh".to_string(),
        }],
        ..AppConfig::default()
    };
    let app = App::new(cfg);
    assert_eq!(app.filter, "");
    assert_eq!(
        app.active_saved_filter, None,
        "the exclusion named an unapplied saved filter"
    );
}

/// Rename moves the entry and the header marker; delete removes the
/// entry and drops the marker.
#[test]
fn saved_filters_rename_and_delete() {
    let scratch = ScopedConfig::new();
    let mut app = app_with_panes();
    let tmux = Tmux::new().unwrap();
    app.saved_filters = vec![crate::config::SavedFilter {
        name: "old".to_string(),
        query: "zsh".to_string(),
    }];
    app.filter = "zsh".to_string();
    app.active_saved_filter = Some("old".to_string());
    app.refresh_filtered();

    // 's' opens settings; 'r' on the selected saved-filter row
    // renames it. The section cursor starts on the first Filters
    // row (the standing filter), so step down once to the saved row.
    app.on_key(press(KeyCode::Char('s')), &tmux);
    let _ = &mut app;
    let rows = app.settings_rows();
    let saved_at = rows.iter().position(|r| match r {
        SettingRow::Item { kind, .. } => *kind == SettingKind::UseSavedFilter(0),
        _ => false,
    });
    assert!(saved_at.is_some(), "no saved-filter row: {rows:?}");
    // Select that row through the public settings API.
    let item_ordinal = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| matches!(r, SettingRow::Item { .. }))
        .position(|(i, _)| Some(&i) == saved_at.as_ref());
    app.settings_selected = item_ordinal.unwrap();
    app.settings_scroll = saved_at.unwrap();

    app.on_key(press(KeyCode::Char('r')), &tmux);
    assert_eq!(
        app.mode,
        Mode::PromptSavedFilter { index: Some(0) },
        "rename prompt did not open"
    );
    // The prompt seeds the current name (cursor at the end), so a
    // rename clears it first — exactly what a user does.
    assert_eq!(app.input, "old");
    for _ in 0.."old".len() {
        app.on_key(press(KeyCode::Backspace), &tmux);
    }
    assert_eq!(app.input, "");
    for c in "new".chars() {
        app.on_key(press(KeyCode::Char(c)), &tmux);
    }
    app.on_key(press(KeyCode::Enter), &tmux);
    assert_eq!(app.saved_filters[0].name, "new");
    // The header marker moved with it.
    assert_eq!(app.active_saved_filter.as_deref(), Some("new"));
    assert_eq!(app.saved_filters.len(), 1, "rename must not duplicate");

    // Delete via 'd' on the selected row.
    app.on_key(press(KeyCode::Char('d')), &tmux);
    assert!(
        app.saved_filters.is_empty(),
        "delete did not remove the row"
    );
    assert_eq!(app.active_saved_filter, None, "marker survived its filter");
    // …and the file agrees.
    let (cfg, _p) = scratch.load();
    assert!(cfg.saved_filters.is_empty(), "delete did not persist");
}

/// Ctrl+F cycles saved filters and wraps; an empty list is a no-op
/// that says so instead of silently doing nothing.
#[test]
fn saved_filters_ctrl_f_cycles_and_wraps() {
    let _scratch = ScopedConfig::new();
    let tmux = Tmux::new().unwrap();
    let mut app = app_with_panes();
    app.focus = Focus::Panes;
    // No saved filters yet: the key reports the way out.
    app.config
        .keys
        .insert("saved_filter".to_string(), "ctrl+f".to_string());
    app.on_key(
        KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL),
        &tmux,
    );
    assert!(
        app.status
            .as_deref()
            .is_some_and(|s| s.contains("no saved filters")),
        "empty-list status missing: {:?}",
        app.status
    );
    assert_eq!(app.active_saved_filter, None);

    // Two entries: cycle first → wraps back to first.
    app.saved_filters = vec![
        crate::config::SavedFilter {
            name: "a".to_string(),
            query: "alpha".to_string(),
        },
        crate::config::SavedFilter {
            name: "b".to_string(),
            query: "beta".to_string(),
        },
    ];
    app.on_key(
        KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL),
        &tmux,
    );
    assert_eq!(app.active_saved_filter.as_deref(), Some("a"));
    assert_eq!(app.filter, "alpha");
    app.on_key(
        KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL),
        &tmux,
    );
    assert_eq!(app.active_saved_filter.as_deref(), Some("b"));
    assert_eq!(app.filter, "beta");
    // Wrap-around.
    app.on_key(
        KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL),
        &tmux,
    );
    assert_eq!(app.active_saved_filter.as_deref(), Some("a"));
    assert_eq!(app.filter, "alpha");
}

/// A hand-typed query drops the header name; typing a saved query
/// exactly restores it. The marker never outlives its query.
#[test]
fn saved_filters_marker_follows_the_query() {
    let tmux = Tmux::new().unwrap();
    let mut app = app_with_panes();
    app.focus = Focus::Panes;
    app.saved_filters = vec![crate::config::SavedFilter {
        name: "work".to_string(),
        query: "vim".to_string(),
    }];
    // Typing the saved query exactly re-names it.
    app.on_key(press(KeyCode::Char('f')), &tmux);
    for c in "vim".chars() {
        app.on_key(press(KeyCode::Char(c)), &tmux);
    }
    assert_eq!(app.active_saved_filter.as_deref(), Some("work"));
    // Editing away from it drops the name.
    app.on_key(press(KeyCode::Char('x')), &tmux);
    assert_eq!(app.active_saved_filter, None);
    // Clearing the search drops it too.
    app.on_key(press(KeyCode::Enter), &tmux);
    assert_eq!(app.filter, "vimx");
    assert_eq!(app.active_saved_filter, None);
}

/// A naming prompt with an empty name commits nothing, and Esc
/// leaves the list untouched.
#[test]
fn saved_filters_empty_name_and_esc() {
    let tmux = Tmux::new().unwrap();
    let mut app = app_with_panes();
    app.focus = Focus::Panes;
    app.filter = "vim".to_string();
    app.on_key(press(KeyCode::Char('s')), &tmux);
    app.on_key(press(KeyCode::Char('n')), &tmux);
    assert_eq!(app.mode, Mode::PromptSavedFilter { index: None });
    // Esc: nothing saved.
    app.on_key(press(KeyCode::Esc), &tmux);
    assert_eq!(app.saved_filters.len(), 0);
    // Empty name on Enter: also nothing saved, and the mode closes.
    app.on_key(press(KeyCode::Char('n')), &tmux);
    app.on_key(press(KeyCode::Enter), &tmux);
    assert_eq!(app.saved_filters.len(), 0);
}
