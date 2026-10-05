use ratatui::{
    Terminal,
    backend::TestBackend,
    style::{Color, Modifier},
};
use terhub::{
    app::{App, Mode, SortKey},
    config::AppConfig,
    tmux::{Pane, Session},
    ui,
};

fn render_with_areas(app: &mut App, width: u16, height: u16) -> (Vec<String>, ui::ClickAreas) {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut out = None;
    terminal
        .draw(|frame| {
            out = Some(ui::render(frame, app));
        })
        .unwrap();
    let buf = terminal.backend().buffer();
    let area = buf.area;
    let lines = (0..area.height)
        .map(|y| {
            let line: String = (0..area.width)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect();
            line.trim_end().to_string()
        })
        .collect();
    (lines, out.unwrap())
}

fn render_lines(app: &mut App, width: u16, height: u16) -> Vec<String> {
    render_with_areas(app, width, height).0
}

/// Full cell grid: symbol plus (fg, modifier) per cell, for style wiring.
fn render_grid(app: &mut App, width: u16, height: u16) -> Vec<Vec<(String, Color, Modifier)>> {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            ui::render(frame, app);
        })
        .unwrap();
    let buf = terminal.backend().buffer();
    let area = buf.area;
    (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| {
                    let c = &buf[(x, y)];
                    (c.symbol().to_string(), c.fg, c.modifier)
                })
                .collect()
        })
        .collect()
}

fn app_with(names: &[&str]) -> App {
    let mut app = App::new(AppConfig::default());
    app.sessions = names
        .iter()
        .map(|n| Session {
            name: n.to_string(),
            windows: 2,
            attached: false,
            created: "today".to_string(),
            activity: "now".to_string(),
        })
        .collect();
    app
}

fn pane(id: &str, session: &str, window: &str, command: &str) -> Pane {
    Pane {
        id: id.to_string(),
        session: session.to_string(),
        window_id: "@0".to_string(),
        window_name: window.to_string(),
        window_active: true,
        index: 0,
        command: command.to_string(),
        path: "/tmp".to_string(),
        active: true,
        width: 80,
        height: 24,
        activity: 0,
        pid: 0,
    }
}

fn app_with_panes() -> App {
    let mut app = app_with(&["alpha", "beta"]);
    app.panes = vec![
        pane("%1", "alpha", "editor", "nvim"),
        pane("%2", "beta", "shell", "bash"),
    ];
    app.refresh_filtered();
    app
}

#[test]
fn selector_bar_lists_all_plus_sessions() {
    let mut app = app_with_panes();
    let lines = render_lines(&mut app, 120, 20);
    let bar = &lines[0];
    assert!(bar.contains("All"), "missing All segment:\n{bar}");
    assert!(bar.contains("alpha"), "missing alpha:\n{bar}");
    assert!(bar.contains("beta"), "missing beta:\n{bar}");
    assert!(bar.contains("│"), "missing separators:\n{bar}");
}

#[test]
fn table_lists_panes_with_columns() {
    let mut app = app_with_panes();
    let lines = render_lines(&mut app, 140, 20);
    let text = lines.join("\n");
    for h in [
        "Pane", "Session", "Window", "Cmd", "Dir", "Agent", "State", "Src", "For", "Turn", "Idle",
        "Summary",
    ] {
        assert!(text.contains(h), "missing header {h}:\n{text}");
    }
    assert!(text.contains("%1"), "missing pane id:\n{text}");
    assert!(text.contains("editor"), "missing window:\n{text}");
    assert!(text.contains("nvim"), "missing command:\n{text}");
    assert!(text.contains("2 pane(s)"), "missing count:\n{text}");
}

#[test]
fn table_sorts_by_recency_by_default() {
    let mut app = app_with_panes();
    app.panes[0].activity = 100; // older
    app.panes[1].activity = 200; // newer
    app.refresh_filtered();
    let lines = render_lines(&mut app, 140, 20);
    let text = lines.join("\n");
    assert!(text.contains("Idle▼"), "missing recency sort mark:\n{text}");
    let bash = text.find("bash").expect("missing bash row");
    let nvim = text.find("nvim").expect("missing nvim row");
    assert!(bash < nvim, "newest pane not first:\n{text}");
}

#[test]
fn sort_arrow_follows_clicks() {
    let mut app = app_with_panes();
    app.cycle_sort(SortKey::Cmd);
    app.refresh_filtered();
    let text = render_lines(&mut app, 140, 20).join("\n");
    assert!(text.contains("Cmd▲"), "missing asc arrow:\n{text}");
    assert!(!text.contains("Idle▼"), "stale arrow:\n{text}");
    app.cycle_sort(SortKey::Cmd);
    app.refresh_filtered();
    let text = render_lines(&mut app, 140, 20).join("\n");
    assert!(text.contains("Cmd▼"), "missing desc arrow:\n{text}");
}

#[test]
fn title_needy_counts_visible_rows_only() {
    use terhub::agent::{AgentRecord, AgentState, Source};
    let mut app = app_with_panes();
    let rec = |pane: &str| AgentRecord {
        pane: pane.to_string(),
        agent: "claude".to_string(),
        state: AgentState::Waiting,
        source: Source::Hook,
        summary: String::new(),
        session: "alpha".to_string(),
        window: "editor".to_string(),
        command: "claude".to_string(),
        updated_at: 1,
        state_since: 1,
        cpu_ticks: 0,
        cpu_still_since: 0,
        last_turn_secs: 0,
        screen_hash: 0,
        quiet_since: 0,
    };
    app.agents.insert("%1".to_string(), rec("%1"));
    app.agents.insert("%2".to_string(), rec("%2"));
    // Both needy, but the filter hides one: title must say 1, not 2.
    app.filter = "bash".to_string();
    app.refresh_filtered();
    let text = render_lines(&mut app, 140, 20).join("\n");
    assert!(text.contains("1 need you"), "wrong title count:\n{text}");
    assert!(!text.contains("2 need you"), "global count leaked:\n{text}");
}

#[test]
fn proc_source_and_stuck_state_render() {
    use terhub::agent::{AgentRecord, AgentState, Source};
    let mut app = app_with_panes();
    app.agents.insert(
        "%1".to_string(),
        AgentRecord {
            pane: "%1".to_string(),
            agent: "pi".to_string(),
            state: AgentState::Stuck,
            source: Source::Proc,
            summary: "no cpu for 12m".to_string(),
            session: "alpha".to_string(),
            window: "editor".to_string(),
            command: "pi".to_string(),
            updated_at: 1,
            state_since: 1,
            cpu_ticks: 500,
            cpu_still_since: 1,
            last_turn_secs: 0,
            screen_hash: 0,
            quiet_since: 0,
        },
    );
    let lines = render_lines(&mut app, 140, 20);
    let text = lines.join("\n");
    assert!(text.contains("stuck"), "missing state:\n{text}");
    assert!(text.contains("proc"), "missing source:\n{text}");
    assert!(text.contains("no cpu for 12m"), "missing summary:\n{text}");
    assert!(text.contains('!'), "stuck needs a mark:\n{text}");
}

#[test]
fn unclaimed_row_has_no_agent_state() {
    // No record, no agent-state: a shell was never expected to work on
    // its own, so it reads blank — never "idle" like a done agent.
    let mut app = app_with_panes();
    let text = render_lines(&mut app, 140, 20).join("\n");
    let row = text.lines().find(|l| l.contains("%2")).unwrap();
    let cells: Vec<&str> = row.split_whitespace().collect();
    assert!(cells.contains(&"—"), "unclaimed shell has a state:\n{text}");
    assert!(
        !cells.contains(&"idle"),
        "shell reads as agent-idle:\n{text}"
    );
}

#[test]
fn idle_column_prefers_visible_quiet_over_tmux_activity() {
    use terhub::agent::{AgentRecord, AgentState, Source, now_unix};
    let mut app = app_with_panes();
    let now = now_unix();
    // Fresh tmux activity (a repaint 5s ago) but a screen unchanged for
    // an hour: the Idle cell must show the quiet clock, not the repaint.
    app.panes[0].activity = now - 5;
    app.refresh_filtered();
    app.agents.insert(
        "%1".to_string(),
        AgentRecord {
            pane: "%1".to_string(),
            agent: "pi".to_string(),
            state: AgentState::Working,
            source: Source::Proc,
            summary: String::new(),
            session: "alpha".to_string(),
            window: "editor".to_string(),
            command: "pi".to_string(),
            updated_at: now,
            state_since: now - 120,
            cpu_ticks: 0,
            cpu_still_since: 0,
            last_turn_secs: 0,
            screen_hash: 7,
            quiet_since: now - 3600,
        },
    );
    let text = render_lines(&mut app, 140, 20).join("\n");
    let row = text.lines().find(|l| l.contains("%1")).unwrap();
    let tokens: Vec<&str> = row.split_whitespace().collect();
    assert!(tokens.contains(&"1h0m"), "quiet clock not shown:\n{text}");
    assert!(
        !tokens.contains(&"5s") && !tokens.contains(&"6s"),
        "repaint age leaked:\n{text}"
    );
}

#[test]
fn agent_color_and_idle_warn_reach_cells() {
    use terhub::agent::{AgentRecord, AgentState, Source, now_unix};
    let mut app = app_with_panes();
    app.config
        .agent_styles
        .insert("pi".to_string(), "red".to_string());
    app.config.idle_warn_style = "bold red".to_string();
    let now = now_unix();
    app.panes[0].activity = now - 3600;
    app.refresh_filtered();
    app.agents.insert(
        "%1".to_string(),
        AgentRecord {
            pane: "%1".to_string(),
            agent: "pi".to_string(),
            state: AgentState::Working,
            source: Source::Proc,
            summary: String::new(),
            session: "alpha".to_string(),
            window: "editor".to_string(),
            command: "pi".to_string(),
            updated_at: now,
            state_since: now - 100,
            cpu_ticks: 0,
            cpu_still_since: 0,
            last_turn_secs: 0,
            screen_hash: 7,
            quiet_since: now - 3600,
        },
    );
    let grid = render_grid(&mut app, 140, 20);
    let text: Vec<String> = grid
        .iter()
        .map(|row| row.iter().map(|(s, _, _)| s.as_str()).collect::<String>())
        .collect();
    let hdr = text.iter().find(|l| l.contains("Agent")).unwrap();
    // Byte offsets lie before multibyte borders: count cells instead.
    let ax = hdr[..hdr.find("Agent").unwrap()].chars().count();
    let ix = hdr[..hdr.find("Idle").unwrap()].chars().count();
    let row = text.iter().position(|l| l.contains("%1")).unwrap();
    assert_eq!(grid[row][ax].1, Color::Red, "agent override color missing");
    assert_eq!(grid[row][ix].1, Color::Red, "idle warn color missing");
    assert!(
        grid[row][ix].2.contains(Modifier::BOLD),
        "idle warn bold missing"
    );
}

#[test]
fn resort_restarts_view_at_top() {
    let mut app = app_with_panes();
    app.select_pane(0);
    app.pane_scroll = 1;
    app.resort(SortKey::Cmd);
    assert_eq!(app.pane_selected, 0);
    assert_eq!(app.pane_scroll, 0);
    let lines = render_lines(&mut app, 140, 20);
    assert!(
        lines[3].contains("bash"),
        "top row not sort winner:\n{}",
        lines[3]
    );
}

#[test]
fn header_hits_align_with_labels() {
    // Comfortable + All: every column incl. Session and Summary.
    let mut app = app_with_panes();
    let (lines, areas) = render_with_areas(&mut app, 140, 20);
    let expected: &[(Option<SortKey>, &str)] = &[
        (None, ""),
        (Some(SortKey::Pane), "Pane"),
        (Some(SortKey::Session), "Session"),
        (Some(SortKey::Window), "Window"),
        (Some(SortKey::Cmd), "Cmd"),
        (Some(SortKey::Dir), "Dir"),
        (Some(SortKey::Agent), "Agent"),
        (Some(SortKey::State), "State"),
        (Some(SortKey::Src), "Src"),
        (Some(SortKey::For), "For"),
        (Some(SortKey::Turn), "Turn"),
        (Some(SortKey::Idle), "Idle"),
        (Some(SortKey::Summary), "Summary"),
    ];
    assert_eq!(areas.headers.len(), expected.len());
    for (hit, (key, label)) in areas.headers.iter().zip(expected.iter()) {
        assert_eq!(hit.key, *key);
        assert_eq!(hit.area.y, 2, "header row moved");
        let row: Vec<char> = lines[hit.area.y as usize].chars().collect();
        let cell: String = row
            .iter()
            .skip(hit.area.x as usize)
            .take(label.len())
            .collect();
        assert_eq!(cell, *label, "hit/label misaligned at x={}", hit.area.x);
    }

    // Compact + filtered: Session and Summary columns are gone.
    let mut app = app_with_panes();
    app.config.density = "compact".to_string();
    app.selected = 1;
    app.refresh_filtered();
    let (lines, areas) = render_with_areas(&mut app, 140, 20);
    let keys: Vec<Option<SortKey>> = areas.headers.iter().map(|h| h.key).collect();
    assert_eq!(
        keys,
        vec![
            None,
            Some(SortKey::Pane),
            Some(SortKey::Window),
            Some(SortKey::Cmd),
            Some(SortKey::Dir),
            Some(SortKey::Agent),
            Some(SortKey::State),
            Some(SortKey::Src),
            Some(SortKey::For),
            Some(SortKey::Turn),
            Some(SortKey::Idle),
        ]
    );
    for hit in &areas.headers {
        assert_eq!(hit.area.y, 2, "header row moved");
        assert!(hit.area.width > 0, "empty hit cell: {hit:?}");
        assert!(
            (hit.area.x as usize) < lines[2].len(),
            "hit off-row: {hit:?}"
        );
    }
}

#[test]
fn text_filter_narrows_rows_and_titles_query() {
    let mut app = app_with_panes();
    app.filter = "bash".to_string();
    app.refresh_filtered();
    let lines = render_lines(&mut app, 140, 20);
    let text = lines.join("\n");
    assert!(text.contains("bash"), "match missing:\n{text}");
    assert!(!text.contains("nvim"), "non-match leaked:\n{text}");
    assert!(text.contains("filter: bash"), "title hides query:\n{text}");
}

#[test]
fn empty_state_hints_create_key() {
    let mut app = app_with(&[]);
    let lines = render_lines(&mut app, 100, 12);
    let text = lines.join("\n");
    assert!(text.contains("no sessions"), "missing empty hint:\n{text}");
}

#[test]
fn title_names_a_dead_daemon() {
    let mut app = app_with_panes();
    app.agents_live = false;
    let lines = render_lines(&mut app, 160, 12);
    let text = lines.join("\n");
    assert!(text.contains("daemon down"), "missing marker:\n{text}");
    app.agents_live = true;
    let lines = render_lines(&mut app, 160, 12);
    assert!(!lines.join("\n").contains("daemon down"));
}

#[test]
fn footer_hints_match_navigation() {
    // Tab toggles focus (not "sessions") and arrows only steer the bar.
    let mut app = app_with_panes();
    let lines = render_lines(&mut app, 160, 12);
    let footer = lines.last().cloned().unwrap_or_default();
    assert!(footer.contains("tab = switch list"), "footer: {footer}");
    assert!(footer.contains("←/→ sessions"), "footer: {footer}");
    assert!(!footer.contains("tab = sessions"), "stale hint: {footer}");
}

#[test]
fn filter_prompt_renders_text() {
    let mut app = app_with(&["alpha"]);
    app.mode = Mode::PromptFilter;
    app.input = "pi".to_string();
    app.cursor = 2;
    let lines = render_lines(&mut app, 100, 12);
    let text = lines.join("\n");
    assert!(text.contains("Filter"), "missing heading:\n{text}");
    assert!(text.contains("Text: pi"), "missing query:\n{text}");
}

#[test]
fn prompt_modal_renders_input() {
    let mut app = app_with(&["alpha"]);
    app.mode = Mode::PromptNew;
    app.input = "gamma".to_string();
    let lines = render_lines(&mut app, 100, 12);
    let text = lines.join("\n");
    assert!(text.contains("New session"), "missing modal:\n{text}");
    assert!(text.contains("gamma"), "missing input:\n{text}");
}

#[test]
fn confirm_modal_renders() {
    let mut app = app_with(&["alpha"]);
    app.mode = Mode::ConfirmKill("alpha".to_string());
    let lines = render_lines(&mut app, 100, 12);
    let text = lines.join("\n");
    assert!(text.contains("Kill 'alpha'"), "missing confirm:\n{text}");
}

#[test]
fn kill_pane_modal_renders() {
    let mut app = app_with(&["alpha"]);
    app.mode = Mode::ConfirmKillPane("%3".to_string());
    let lines = render_lines(&mut app, 100, 12);
    let text = lines.join("\n");
    assert!(text.contains("Kill pane '%3'"), "missing confirm:\n{text}");
}

#[test]
fn agent_state_renders_in_table() {
    use terhub::agent::{AgentRecord, AgentState, Source};
    let mut app = app_with_panes();
    app.agents.insert(
        "%1".to_string(),
        AgentRecord {
            pane: "%1".to_string(),
            agent: "claude".to_string(),
            state: AgentState::Waiting,
            source: Source::Hook,
            summary: "approve this".to_string(),
            session: "alpha".to_string(),
            window: "editor".to_string(),
            command: "claude".to_string(),
            updated_at: 1,
            state_since: 1,
            cpu_ticks: 0,
            cpu_still_since: 0,
            last_turn_secs: 0,
            screen_hash: 0,
            quiet_since: 0,
        },
    );
    let lines = render_lines(&mut app, 140, 20);
    let text = lines.join("\n");
    assert!(text.contains("claude"), "missing agent:\n{text}");
    assert!(text.contains("waiting"), "missing state:\n{text}");
    assert!(text.contains("approve this"), "missing summary:\n{text}");
    assert!(text.contains("1 need you"), "missing title count:\n{text}");
}

#[test]
fn crowded_bar_windows_to_selection() {
    let names: Vec<String> = (0..10).map(|i| format!("session-{i:02}")).collect();
    let refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
    let mut app = app_with(&refs);
    app.select(10);
    let lines = render_lines(&mut app, 60, 12);
    let bar = &lines[0];
    assert!(app.session_scroll > 0, "scroll never moved");
    assert!(bar.contains("session-09"), "selection not visible:\n{bar}");
    assert!(!bar.contains("session-00"), "window shows too much:\n{bar}");
    assert!(bar.contains("‹"), "missing overflow marker:\n{bar}");
}

#[test]
fn long_table_windows_to_selection() {
    let mut app = app_with(&["alpha"]);
    app.panes = (0..15)
        .map(|i| pane(&format!("%{i}"), "alpha", &format!("win-{i:02}"), "bash"))
        .collect();
    app.refresh_filtered();
    app.select_pane(14);
    let lines = render_lines(&mut app, 140, 14);
    let text = lines.join("\n");
    assert!(app.pane_scroll > 0, "scroll never moved");
    assert!(text.contains("win-14"), "selection not visible:\n{text}");
    assert!(!text.contains("win-00"), "window shows too much:\n{text}");
}

#[test]
fn compact_density_drops_summary() {
    let mut app = app_with_panes();
    app.config.density = "compact".to_string();
    let lines = render_lines(&mut app, 140, 20);
    let text = lines.join("\n");
    assert!(!text.contains("Summary"), "summary column leaked:\n{text}");
    assert!(text.contains("compact"), "missing density tag:\n{text}");
}

#[test]
fn preview_takes_full_screen() {
    let mut app = app_with_panes();
    app.preview = "hello from pane".to_string();
    let lines = render_lines(&mut app, 140, 24);
    assert!(
        !lines.join("\n").contains("hello from pane"),
        "preview leaked while closed"
    );
    app.show_preview = true;
    let lines = render_lines(&mut app, 140, 24);
    let text = lines.join("\n");
    assert!(text.contains("hello from pane"), "missing preview:\n{text}");
    // Full takeover: the table underneath is fully covered, edge to edge.
    assert!(
        !text.contains("nvim"),
        "table leaked through preview:\n{text}"
    );
    assert!(
        lines[0].starts_with('┌'),
        "preview not full width:\n{}",
        lines[0]
    );
    assert!(
        lines[23].starts_with('└'),
        "preview not full height:\n{}",
        lines[23]
    );
    assert!(text.contains("right-click"), "missing close hint:\n{text}");
    assert!(text.contains("esc close"), "missing popup hint:\n{text}");
}

#[test]
fn src_and_dir_columns_render() {
    use terhub::agent::{AgentRecord, AgentState, Source};
    let mut app = app_with_panes();
    app.panes[0].path = "/repo/alpha-proj".to_string();
    app.refresh_filtered();
    app.agents.insert(
        "%1".to_string(),
        AgentRecord {
            pane: "%1".to_string(),
            agent: "claude".to_string(),
            state: AgentState::Working,
            source: Source::Hook,
            summary: String::new(),
            session: "alpha".to_string(),
            window: "editor".to_string(),
            command: "claude".to_string(),
            updated_at: 1,
            state_since: 1,
            cpu_ticks: 0,
            cpu_still_since: 0,
            last_turn_secs: 0,
            screen_hash: 0,
            quiet_since: 0,
        },
    );
    app.agents.insert(
        "%2".to_string(),
        AgentRecord {
            pane: "%2".to_string(),
            agent: "codex".to_string(),
            state: AgentState::Error,
            source: Source::Screen,
            summary: String::new(),
            session: "beta".to_string(),
            window: "shell".to_string(),
            command: "codex".to_string(),
            updated_at: 1,
            state_since: 1,
            cpu_ticks: 0,
            cpu_still_since: 0,
            last_turn_secs: 0,
            screen_hash: 0,
            quiet_since: 0,
        },
    );
    let lines = render_lines(&mut app, 140, 20);
    let text = lines.join("\n");
    assert!(text.contains("alpha-proj"), "missing dir:\n{text}");
    assert!(text.contains("hook"), "missing hook src:\n{text}");
    assert!(text.contains("scr"), "missing screen src:\n{text}");
}

#[test]
fn filtered_table_hides_session_column() {
    let mut app = app_with_panes();
    app.select(1); // alpha
    app.refresh_filtered();
    let lines = render_lines(&mut app, 140, 20);
    let text = lines.join("\n");
    assert!(!text.contains("Session"), "session column leaked:\n{text}");
    assert!(text.contains("alpha only"), "missing filter tag:\n{text}");
}

#[test]
fn settings_popup_lists_styles() {
    let mut app = app_with_panes();
    app.open_settings();
    let lines = render_lines(&mut app, 120, 30);
    let text = lines.join("\n");
    assert!(text.contains("settings"), "missing popup:\n{text}");
    for t in ["dark", "dracula", "nord", "gruvbox", "solarized"] {
        assert!(text.contains(t), "missing theme {t}:\n{text}");
    }
    assert!(text.contains("Density"), "missing section:\n{text}");
    assert!(text.contains("notifications:"), "missing toggle:\n{text}");
}

#[test]
fn settings_popup_scrolls_when_short() {
    let mut app = app_with_panes();
    app.open_settings();
    app.settings_selected = 9; // last toggle
    let lines = render_lines(&mut app, 120, 12);
    let text = lines.join("\n");
    assert!(app.settings_scroll > 0, "scroll never moved");
    assert!(text.contains("▲"), "missing scroll marker:\n{text}");
    assert!(
        text.contains("highlight attached:"),
        "selection not visible:\n{text}"
    );
}

#[test]
fn title_names_theme() {
    let mut app = app_with_panes();
    app.config.theme = "dracula".to_string();
    let lines = render_lines(&mut app, 140, 20);
    let text = lines.join("\n");
    assert!(text.contains("dracula"), "missing theme tag:\n{text}");
}

#[test]
fn all_themes_render() {
    for theme in terhub::config::THEMES {
        let mut app = app_with_panes();
        app.config.theme = theme.to_string();
        let lines = render_lines(&mut app, 140, 20);
        let text = lines.join("\n");
        assert!(text.contains(theme), "theme {theme} missing:\n{text}");
        assert!(text.contains("%1"), "theme {theme} broke table:\n{text}");
    }
}
