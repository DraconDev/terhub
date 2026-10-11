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
        bell: false,
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
fn title_counts_idle_rows_scoped_to_the_filter() {
    use terhub::agent::{AgentRecord, AgentState, Source};
    let mut app = app_with_panes();
    let rec = |pane: &str, session: &str, state: AgentState| AgentRecord {
        pane: pane.to_string(),
        agent: "pi".to_string(),
        state,
        source: Source::Proc,
        summary: String::new(),
        session: session.to_string(),
        window: "editor".to_string(),
        command: "pi".to_string(),
        updated_at: 1,
        state_since: 1,
        cpu_ticks: 0,
        cpu_still_since: 0,
        agent_start: 0,
        last_turn_secs: 0,
        screen_hash: 0,
        quiet_since: 1,
        last_prompt_at: 0,
        last_prompt: String::new(),
        handle: String::new(),
        model: String::new(),
        ctx_pct: None,
        reg_pid: 0,
        bell: false,
        bell_reason: String::new(),
    };
    // Both idle, but the filter (which matches the bash pane) hides one:
    // title must say 1, not 2.
    app.agents
        .insert("%1".to_string(), rec("%1", "alpha", AgentState::Idle));
    app.agents
        .insert("%2".to_string(), rec("%2", "beta", AgentState::Idle));
    app.filter = "bash".to_string();
    app.refresh_filtered();
    let text = render_lines(&mut app, 140, 20).join("\n");
    assert!(text.contains("1 idle"), "wrong idle count:\n{text}");
    assert!(!text.contains("2 idle"), "global count leaked:\n{text}");

    // Nothing idle: the segment disappears rather than reading "0 idle".
    let mut app = app_with_panes();
    app.agents
        .insert("%1".to_string(), rec("%1", "alpha", AgentState::Working));
    app.agents
        .insert("%2".to_string(), rec("%2", "beta", AgentState::Working));
    app.refresh_filtered();
    let text = render_lines(&mut app, 140, 20).join("\n");
    assert!(!text.contains("idle"), "phantom idle count:\n{text}");
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
            agent_start: 0,
            last_turn_secs: 0,
            screen_hash: 7,
            quiet_since: now - 3600,
            last_prompt_at: 0,
            last_prompt: String::new(),
            handle: String::new(),
            model: String::new(),
            ctx_pct: None,
            reg_pid: 0,
            bell: false,
            bell_reason: String::new(),
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

fn waiting_record(pane: &str, session: &str, summary: &str) -> terhub::agent::AgentRecord {
    use terhub::agent::{AgentRecord, AgentState, Source};
    let now = terhub::agent::now_unix();
    AgentRecord {
        pane: pane.to_string(),
        agent: "pi".to_string(),
        state: AgentState::Waiting,
        source: Source::Hook,
        summary: summary.to_string(),
        session: session.to_string(),
        window: "editor".to_string(),
        command: "pi".to_string(),
        updated_at: now,
        state_since: now - 60,
        cpu_ticks: 0,
        cpu_still_since: 0,
        agent_start: 0,
        last_turn_secs: 0,
        screen_hash: 7,
        quiet_since: now - 60,
        last_prompt_at: 0,
        last_prompt: String::new(),
        handle: String::new(),
        model: String::new(),
        ctx_pct: None,
        reg_pid: 0,
        bell: false,
        bell_reason: String::new(),
    }
}

fn idle_record(pane: &str, session: &str, quiet_age: u64, now: u64) -> terhub::agent::AgentRecord {
    use terhub::agent::{AgentRecord, AgentState, Source};
    AgentRecord {
        pane: pane.to_string(),
        agent: "pi".to_string(),
        state: AgentState::Idle,
        source: Source::Proc,
        summary: "done".to_string(),
        session: session.to_string(),
        window: "editor".to_string(),
        command: "pi".to_string(),
        updated_at: now,
        state_since: now - quiet_age,
        cpu_ticks: 0,
        cpu_still_since: 0,
        agent_start: 0,
        last_turn_secs: 0,
        screen_hash: 7,
        quiet_since: now - quiet_age,
        last_prompt_at: 0,
        last_prompt: String::new(),
        handle: String::new(),
        model: String::new(),
        ctx_pct: None,
        reg_pid: 0,
        bell: false,
        bell_reason: String::new(),
    }
}

fn cell_style(grid: &[Vec<(String, Color, Modifier)>], id: &str, header: &str) -> Color {
    let text: Vec<String> = grid
        .iter()
        .map(|row| row.iter().map(|(s, _, _)| s.as_str()).collect::<String>())
        .collect();
    let hdr = text.iter().find(|l| l.contains(header)).unwrap();
    let ix = hdr[..hdr.find(header).unwrap()].chars().count();
    let row = text.iter().position(|l| l.contains(id)).unwrap();
    grid[row][ix].1
}

/// Find a cell by row + literal header; `cell_style` returns just
/// the colour, this also returns the modifier so bold checks work.
fn cell_at(grid: &[Vec<(String, Color, Modifier)>], id: &str, header: &str) -> (Color, Modifier) {
    let text: Vec<String> = grid
        .iter()
        .map(|row| row.iter().map(|(s, _, _)| s.as_str()).collect::<String>())
        .collect();
    let hdr = text.iter().find(|l| l.contains(header)).unwrap();
    let ix = hdr[..hdr.find(header).unwrap()].chars().count();
    let row = text.iter().position(|l| l.contains(id)).unwrap();
    (grid[row][ix].1, grid[row][ix].2)
}

fn idle_cell_style(grid: &[Vec<(String, Color, Modifier)>], id: &str) -> (Color, Modifier) {
    let text: Vec<String> = grid
        .iter()
        .map(|row| row.iter().map(|(s, _, _)| s.as_str()).collect::<String>())
        .collect();
    let hdr = text.iter().find(|l| l.contains("Idle")).unwrap();
    let ix = hdr[..hdr.find("Idle").unwrap()].chars().count();
    let row = text.iter().position(|l| l.contains(id)).unwrap();
    (grid[row][ix].1, grid[row][ix].2)
}

#[test]
fn long_idle_tints_idle_cell() {
    use terhub::agent::now_unix;
    let mut app = app_with_panes();
    app.config.idle_warn_style = "bold red".to_string();
    let now = now_unix();
    app.agents
        .insert("%1".to_string(), idle_record("%1", "alpha", 7 * 3600, now));
    app.agents
        .insert("%2".to_string(), idle_record("%2", "beta", 3600, now));
    // A day-scale idle tints under the default threshold; an hour doesn't.
    let grid = render_grid(&mut app, 140, 20);
    let (fg, mods) = idle_cell_style(&grid, "%1");
    assert_eq!(fg, Color::Red, "long idle missing warn color");
    assert!(mods.contains(Modifier::BOLD), "long idle missing warn bold");
    let (fg, _) = idle_cell_style(&grid, "%2");
    assert_ne!(fg, Color::Red, "fresh idle wrongly warned");
    // idle_flag_secs = 0 is the off switch, like stuck_secs = 0.
    app.config.idle_flag_secs = 0;
    let grid = render_grid(&mut app, 140, 20);
    let (fg, _) = idle_cell_style(&grid, "%1");
    assert_ne!(fg, Color::Red, "idle_flag_secs = 0 still warns");
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
            agent_start: 0,
            last_turn_secs: 0,
            screen_hash: 7,
            quiet_since: now - 3600,
            last_prompt_at: 0,
            last_prompt: String::new(),
            handle: String::new(),
            model: String::new(),
            ctx_pct: None,
            reg_pid: 0,
            bell: false,
            bell_reason: String::new(),
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
fn columns_carry_semantic_styles_not_terminal_default() {
    use terhub::agent::now_unix;
    let mut app = app_with_panes();
    app.config.theme = "dark".to_string();
    app.agents
        .insert("%1".to_string(), idle_record("%1", "alpha", 60, now_unix()));
    // Handles group with their agent (same hash colour, not dim).
    app.agents.get_mut("%1").unwrap().handle = "@pi-a".to_string();
    let grid = render_grid(&mut app, 140, 20);
    // Session borrows the title hue (no bold): categories read as one family.
    assert_eq!(
        cell_style(&grid, "%1", "Session"),
        terhub::ui::palette("dark").title.fg.unwrap(),
        "session lost its title-family colour"
    );
    // Cmd is primary content ink, not the terminal default.
    assert_eq!(
        cell_style(&grid, "%1", "Cmd"),
        terhub::ui::palette("dark").status.fg.unwrap(),
        "cmd fell back to terminal default"
    );
    // Window recedes into dim.
    assert_eq!(
        cell_style(&grid, "%1", "Window"),
        terhub::ui::palette("dark").dim.fg.unwrap(),
        "window does not recede"
    );
    // Handle matches its agent cell colour exactly (group identity).
    assert_eq!(
        cell_style(&grid, "%1", "Handle"),
        cell_style(&grid, "%1", "Agent"),
        "handle does not group with its agent"
    );
}

#[test]
fn done_state_renders_in_dim_green_check() {
    use terhub::agent::{AgentRecord, AgentState, Source, now_unix};
    let mut app = app_with_panes();
    app.config.theme = "dark".to_string();
    // A `Done` pane shares a record with Idle, but pinned to Done.
    let now = now_unix();
    app.agents.insert(
        "%1".to_string(),
        AgentRecord {
            pane: "%1".to_string(),
            agent: "pi".to_string(),
            state: AgentState::Done,
            source: Source::Proc,
            summary: "all tasks passed".to_string(),
            session: "alpha".to_string(),
            window: "editor".to_string(),
            command: "pi".to_string(),
            updated_at: now,
            state_since: now - 60,
            cpu_ticks: 0,
            cpu_still_since: 0,
            agent_start: 0,
            last_turn_secs: 0,
            screen_hash: 7,
            quiet_since: now - 60,
            last_prompt_at: 0,
            last_prompt: String::new(),
            handle: String::new(),
            model: String::new(),
            ctx_pct: None,
            reg_pid: 0,
            bell: false,
            bell_reason: String::new(),
        },
    );
    let grid = render_grid(&mut app, 140, 20);
    // The State column header is "State" and the value is lowercase
    // (`done`, `waiting`, `working`); the cell under "Idle" might be
    // a different column entirely if the row contains the word
    // "idle" elsewhere. Use the literal State column instead.
    let (fg, mods) = cell_at(&grid, "%1", "State");
    // Done uses the palette's `done` role — the soft green distinct
    // from the alarm red used for `Waiting` / `Error`. The check
    // itself is the bold modifier (a glanceable marker, not a row
    // tint that bleeds across the cells).
    assert_eq!(fg, terhub::ui::palette("dark").done.fg.unwrap());
    assert!(
        mods.contains(Modifier::BOLD),
        "done state lost its check mark"
    );
    // `done` must never be the alarm red — that's reserved for
    // things actually needing the human.
    assert_ne!(fg, Color::Red, "done state misused the alarm colour");
}

#[test]
fn sparkline_renders_six_cells_monotonic_in_grid() {
    // Contract item 10: the sparkline column, as rendered into the
    // grid, shows 6 cells with non-decreasing height. A regression
    // here would mean the renderer truncates the column, swaps the
    // glyph order, or drops cells under wide terminals — the unit
    // tests on `sparkline_glyphs` would not catch any of those.
    let mut app = app_with_panes();
    app.config.activity_sparkline = true;
    // Drive a monotonic ramp: each tick adds more bytes to the
    // screen, so the ring's rightmost-10 bucket (which is what the
    // 6th cell renders) ends up larger than the 5th, which is
    // larger than the 4th, etc. The first call seeds prev=unknown
    // (recorded as 0); every call after that is a clean delta.
    let mut screen = String::new();
    for i in 0..60 {
        let n = (i + 1) * 4; // 4, 8, 12, ... 240 bytes added
        screen.push_str(&"x".repeat(n));
        app.sample_sparkline("%1", &screen);
    }
    let grid = render_grid(&mut app, 200, 20);
    // The Activity column header is "Activity" but ratui truncates
    // the visible cell to fit the 7-wide constraint (renders as
    // "Activit"). Find the column by its position: the last 7 cells
    // of every pane row (6 sparkline glyphs + 1 padding) is the
    // Activity column.
    let text: Vec<String> = grid
        .iter()
        .map(|row| row.iter().map(|(s, _, _)| s.as_str()).collect())
        .collect();
    // Pull the sparkline cell out of pane %1's row. The Activity
    // column is the last column in the row, 7 cells wide: 6 block
    // glyphs + 1 padding (or in this test, the right border │).
    // Take the last contiguous run of block characters (U+2581
    // through U+2588 — the sparkline glyph set) from the row.
    let row_line = text
        .iter()
        .find(|l| l.contains("%1"))
        .expect("pane %1 must render in the grid");
    let sparkline: String = {
        // Walk the row from the end, collecting block glyphs until
        // we hit a non-block character. Then take exactly 6 of the
        // freshest samples (the 6-cell column) and reverse to
        // left-to-right order.
        let mut cells: Vec<char> = Vec::with_capacity(8);
        for c in row_line.chars().rev() {
            let cp = c as u32;
            if (0x2581..=0x2588).contains(&cp) {
                cells.push(c);
            } else if !cells.is_empty() {
                break;
            }
        }
        cells.truncate(6);
        cells.reverse();
        cells.into_iter().collect()
    };
    assert_eq!(
        sparkline.chars().count(),
        6,
        "Activity column must render 6 cells, got {sparkline:?}"
    );
    // Heights must be monotonically non-decreasing across the six
    // cells (rightmost = freshest, leftmost = oldest). Compare on
    // unicode codepoints — every glyph in the set has a unique
    // height and the height order matches the codepoint order.
    let mut prev = '\u{0000}';
    for c in sparkline.chars() {
        assert!(
            (c as u32) >= (prev as u32),
            "non-monotonic sparkline: {prev:?} > {c:?} in {sparkline:?}"
        );
        prev = c;
    }
    // The freshest bucket must be the tallest — a busy tick should
    // light the rightmost cell, not get masked by log scaling.
    let chars: Vec<char> = sparkline.chars().collect();
    assert!(
        (chars[5] as u32) >= ('▄' as u32),
        "freshest bucket too short for a busy ramp: {sparkline:?}"
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
        (Some(SortKey::Handle), "Handle"),
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
            Some(SortKey::Handle),
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
    assert!(text.contains("search: bash"), "title hides query:\n{text}");
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
fn preview_title_shows_agent_context() {
    use terhub::agent::{AgentRecord, AgentState, Source, now_unix};
    let mut app = app_with_panes();
    let now = now_unix();
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
            agent_start: 0,
            last_turn_secs: 0,
            screen_hash: 1,
            quiet_since: now - 180,
            last_prompt_at: 0,
            last_prompt: String::new(),
            handle: String::new(),
            model: String::new(),
            ctx_pct: None,
            reg_pid: 0,
            bell: false,
            bell_reason: String::new(),
        },
    );
    app.show_preview = true;
    app.preview = "hello from pane".to_string();
    let text = render_lines(&mut app, 160, 20).join("\n");
    assert!(
        text.contains("pi working 3m"),
        "title lacks context:\n{text}"
    );
}

#[test]
fn preview_title_shows_model_and_ctx_strip() {
    use terhub::agent::{AgentRecord, AgentState, Source, now_unix};
    let mut app = app_with_panes();
    let now = now_unix();
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
            agent_start: 0,
            last_turn_secs: 0,
            screen_hash: 1,
            quiet_since: now - 180,
            last_prompt_at: 0,
            last_prompt: String::new(),
            handle: String::new(),
            model: "opus-4.5".to_string(),
            ctx_pct: Some(72),
            reg_pid: 0,
            bell: false,
            bell_reason: String::new(),
        },
    );
    app.show_preview = true;
    app.preview = "hello from pane".to_string();
    let text = render_lines(&mut app, 160, 20).join("\n");
    assert!(text.contains("opus-4.5"), "title lacks model:\n{text}");
    assert!(text.contains("ctx 72%"), "title lacks ctx:\n{text}");
}

#[test]
fn settings_selection_stays_visible() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use terhub::tmux::Tmux;
    let mut app = app_with_panes();
    app.open_settings();
    // Fake tmux: the settings path never touches it.
    let tmux = Tmux::test("/nonexistent-tmux-binary".into());
    let down = KeyEvent::new(KeyCode::Down, KeyModifiers::empty());
    for _ in 0..30 {
        // overshoot on purpose: 26 selectable items, land clamped at the standing row
        app.on_key(down, &tmux);
    }
    // Short terminal: driving to the bottom lands on the standing
    // row (Filters section) — it must be visible, not the toggles.
    let text = render_lines(&mut app, 160, 12).join("\n");
    assert!(
        text.contains("standing filter"),
        "last item scrolled out of view:\n{text}"
    );
    assert!(text.contains("›"), "selection marker missing:\n{text}");
    // Back up: the selection marker must stay on a visible row (scroll
    // tracks in the renderer, like the table's ensure_visible).
    let up = KeyEvent::new(KeyCode::Up, KeyModifiers::empty());
    for _ in 0..30 {
        // overshoot on purpose: clamped at the first theme
        app.on_key(up, &tmux);
    }
    let text = render_lines(&mut app, 160, 12).join("\n");
    assert!(text.contains("› dark"), "selection lost going up:\n{text}");
}

#[test]
fn kill_marked_confirm_keeps_hint_visible() {
    let mut app = app_with_panes();
    let ids: Vec<String> = (1..=20).map(|i| format!("%{i}")).collect();
    app.mode = Mode::ConfirmKillMarked(ids);
    let text = render_lines(&mut app, 160, 20).join("\n");
    assert!(
        text.contains("Enter=yes Esc=no"),
        "hint clipped off the modal:\n{text}"
    );
}

#[test]
fn hidden_sort_column_shows_in_title() {
    let mut app = app_with_panes();
    // Summary sorted but compact hides the column: the title says so.
    app.config.density = "compact".to_string();
    app.sort_key = SortKey::Summary;
    let text = render_lines(&mut app, 160, 20).join("\n");
    assert!(
        text.contains("sorted by Summary"),
        "sort indicator lost:\n{text}"
    );
    // Session sorted but filtered to one session: same fallback.
    let mut app = app_with_panes();
    app.select(1);
    app.sort_key = SortKey::Session;
    let text = render_lines(&mut app, 160, 20).join("\n");
    assert!(
        text.contains("sorted by Session"),
        "sort indicator lost:\n{text}"
    );
}

#[test]
fn preview_title_uses_attach_binding() {
    let mut app = app_with_panes();
    app.config
        .keys
        .insert("attach".to_string(), "ctrl+o".to_string());
    app.show_preview = true;
    app.preview = "hello from pane".to_string();
    let text = render_lines(&mut app, 160, 20).join("\n");
    assert!(
        text.contains("left-click/ctrl+o jump"),
        "title ignores binding:\n{text}"
    );
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
fn standing_prompt_renders_exclusion_chrome() {
    // The standing editor wears exclusion chrome so it never
    // reads as a search: `Hide:` plus a pre-filled `!`. The
    // editor accepts space-separated tokens (each becomes a
    // NOT-OR exclude); the prompt itself is one line.
    let mut app = app_with(&["alpha"]);
    app.open_standing_editor();
    let lines = render_lines(&mut app, 100, 12);
    let text = lines.join("\n");
    assert!(text.contains("Exclude filter"), "missing heading:\n{text}");
    assert!(text.contains("Hide: !"), "missing bang prefix:\n{text}");
    assert!(!text.contains("Text: "), "search chrome leaked:\n{text}");
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
fn saved_filter_name_appears_in_the_table_header() {
    let mut app = app_with_panes();
    // An active saved filter is visible in the title, not silent.
    app.saved_filters = vec![terhub::config::SavedFilter {
        name: "work".to_string(),
        query: "bash".to_string(),
    }];
    app.filter = "bash".to_string();
    app.active_saved_filter = Some("work".to_string());
    app.refresh_filtered();
    let text = render_lines(&mut app, 140, 20).join("\n");
    assert!(
        text.contains("filter: work"),
        "saved filter name missing from header:\n{text}"
    );
    assert!(
        !text.contains("search: bash"),
        "a named filter must not also read as a raw search:\n{text}"
    );

    // Typed by hand: the title names the query, not a saved filter.
    app.active_saved_filter = None;
    let text = render_lines(&mut app, 140, 20).join("\n");
    assert!(
        text.contains("search: bash"),
        "hand-typed query lost its header text:\n{text}"
    );
}

#[test]
fn saved_filter_prompt_is_single_line() {
    // Naming a filter is an add prompt…
    let mut app = app_with(&["alpha"]);
    app.mode = Mode::PromptSavedFilter { index: None };
    app.input = "work".to_string();
    let text = render_lines(&mut app, 100, 12).join("\n");
    assert!(text.contains("Filter name: work"), "missing label:\n{text}");
    assert!(!text.contains("Hide: "), "exclusion chrome leaked:\n{text}");

    // …and the rename prompt seeds the existing name on the same line.
    let mut app = app_with(&["alpha"]);
    app.mode = Mode::PromptSavedFilter { index: Some(0) };
    app.input = "old".to_string();
    let text = render_lines(&mut app, 100, 12).join("\n");
    assert!(
        text.contains("Rename: old"),
        "missing rename label:\n{text}"
    );
}

#[test]
fn settings_filters_section_lists_saved_names() {
    let mut app = app_with_panes();
    app.saved_filters = vec![terhub::config::SavedFilter {
        name: "work".to_string(),
        query: "bash nvim".to_string(),
    }];
    app.mode = Mode::Settings;
    // The popup opens on the first section; jump the sidebar cursor to
    // the Filters section so its rows are the ones rendered.
    let filters = app
        .settings_sections()
        .iter()
        .position(|(name, _)| *name == "Filters")
        .expect("a Filters section exists");
    app.settings_section = filters;
    let text = render_lines(&mut app, 120, 30).join("\n");
    assert!(text.contains("Filters"), "missing section:\n{text}");
    assert!(text.contains("saved: work"), "missing saved name:\n{text}");
    assert!(text.contains("bash nvim"), "missing query text:\n{text}");
}

/// The filter chip: a coloured block plus a label, so an active filter
/// is never silent. Four states — no filter, search only, exclusion
/// only, both at once — each with its own label, so the label always
/// names what is actually hiding rows.
#[test]
fn filter_active_indicator_covers_every_filter_state() {
    const BLOCK: char = '\u{25fc}';

    // No filter at all: no chip, not even an empty one.
    let mut app = app_with_panes();
    assert!(app.exclude.is_empty() && app.filter.is_empty());
    let text = render_lines(&mut app, 120, 12).join("\n");
    assert!(
        !text.contains(BLOCK),
        "a chip with no filter behind it:\n{text}"
    );

    // Search only.
    let mut app = app_with_panes();
    app.filter = "bash".to_string();
    app.refresh_filtered();
    let text = render_lines(&mut app, 120, 12).join("\n");
    assert!(
        text.contains(&format!("{BLOCK} search: bash")),
        "search-only chip missing:\n{text}"
    );
    assert!(
        !text.contains("filter:"),
        "search-only must not read as an exclusion:\n{text}"
    );

    // Standing exclusion only.
    let mut app = app_with_panes();
    app.exclude = "zsh".to_string();
    app.refresh_filtered();
    let text = render_lines(&mut app, 120, 12).join("\n");
    assert!(
        text.contains(&format!("{BLOCK} filter: zsh")),
        "exclusion-only chip missing:\n{text}"
    );

    // Both: the chip names both halves.
    let mut app = app_with_panes();
    app.exclude = "zsh".to_string();
    app.filter = "vim".to_string();
    app.refresh_filtered();
    let text = render_lines(&mut app, 120, 12).join("\n");
    assert!(
        text.contains(&format!("{BLOCK} filter: zsh · search: vim")),
        "combined chip missing or mislabelled:\n{text}"
    );
}

/// Wide terminals: the chip sits on the title line, right of the title,
/// inside the same border - one header line, nothing clipped, and the
/// table gives up no rows for it.
#[test]
fn filter_active_indicator_rides_the_title_line_on_wide_terminals() {
    const BLOCK: char = '\u{25fc}';
    let mut app = app_with_panes();
    app.exclude = "zsh".to_string();
    app.filter = "vim".to_string();
    app.refresh_filtered();
    let lines = render_lines(&mut app, 140, 14);
    let label = format!("{BLOCK} filter: zsh \u{b7} search: vim");
    let row = lines
        .iter()
        .position(|l| l.contains(&label))
        .unwrap_or_else(|| panic!("chip missing or clipped:\n{lines:?}"));
    assert!(
        lines[row].contains("terhub \u{2014}"),
        "chip did not share the title line:\n{lines:?}"
    );
    // Same border line as the title, so no row above the table is spent.
    assert!(
        lines[row].starts_with('\u{250c}'),
        "chip is not inside the table border:\n{lines:?}"
    );
    // And the unfiltered layout is untouched: one row fewer spent on
    // chrome would push the header down.
    let mut plain = app_with_panes();
    let plain_lines = render_lines(&mut plain, 140, 14);
    assert_eq!(
        lines[row + 1],
        plain_lines[2],
        "wide chip stole a row from the header"
    );
}

/// Narrow terminals: the title alone fills the border, so the chip moves
/// to a header line of its own directly above the table instead of being
/// clipped mid-label. The label stays whole at every width, and the chip
/// never lands anywhere but the header region.
#[test]
fn filter_active_indicator_wraps_to_a_second_header_line_when_narrow() {
    const BLOCK: char = '\u{25fc}';
    let mut rode_title_line = Vec::new();
    for width in [170u16, 150, 140, 120, 100, 80, 60, 44] {
        let mut app = app_with_panes();
        app.exclude = "zsh".to_string();
        app.filter = "vim".to_string();
        app.refresh_filtered();
        let lines = render_lines(&mut app, width, 14);
        let label = format!("{BLOCK} filter: zsh \u{b7} search: vim");
        // Never clipped: the whole label is drawn at every width.
        let row = lines
            .iter()
            .position(|l| l.contains(&label))
            .unwrap_or_else(|| panic!("w={width}: chip vanished or clipped:\n{lines:?}"));
        let inline = lines[row].contains("terhub \u{2014}");
        if inline {
            rode_title_line.push(width);
            // It is inside the table's own border, header row right after.
            assert!(
                lines[row].starts_with('\u{250c}') && lines[row + 1].starts_with('\u{2502}'),
                "w={width}: inline chip is not inside the table border:\n{lines:?}"
            );
        } else {
            // Its own line, immediately above the table's top border.
            assert!(
                lines[row + 1].starts_with('\u{250c}'),
                "w={width}: chip line is not adjacent to the table:\n{lines:?}"
            );
            assert!(
                !lines[row].contains('|'),
                "w={width}: chip drifted into the table body:\n{lines:?}"
            );
        }
    }
    // Both placements really occur, so neither branch above is vacuous:
    // wide enough to share the title line, narrow enough to need its own.
    assert!(
        rode_title_line.iter().any(|w| *w >= 140),
        "chip never rode the title line on a wide terminal: {rode_title_line:?}"
    );
    assert!(
        rode_title_line.iter().all(|w| *w >= 120),
        "chip rode the title line while narrow and should have wrapped: {rode_title_line:?}"
    );
}

/// The chip is drawn in the semantic palette - the same title ink as the
/// rest of the chrome, so it reads as part of the header rather than a
/// different widget that happened to appear.
#[test]
fn filter_active_indicator_uses_the_title_palette() {
    let mut app = app_with_panes();
    app.config.theme = "dark".to_string();
    app.filter = "vim".to_string();
    app.refresh_filtered();
    let grid = render_grid(&mut app, 120, 12);
    // Locate the chip's block glyph by its own text position.
    let text: Vec<String> = grid
        .iter()
        .map(|row| row.iter().map(|(s, _, _)| s.as_str()).collect())
        .collect();
    let row = text
        .iter()
        .position(|l| l.contains('\u{25fc}'))
        .expect("chip block glyph not rendered");
    // `find` returns a byte index; the grid is indexed by cell, and the
    // line holds multi-byte glyphs, so count characters.
    let col = text[row][..text[row].find('\u{25fc}').unwrap()]
        .chars()
        .count();
    let (symbol, fg, _) = grid[row][col].clone();
    assert_eq!(symbol, "\u{25fc}", "off-by-one: not the chip glyph");
    assert_eq!(
        fg,
        terhub::ui::palette("dark").title.fg.unwrap(),
        "chip block left the title palette"
    );
    // The label carries the same ink, so the whole chip reads as one.
    let (_, label_fg, _) = grid[row][col + 1].clone();
    assert_eq!(
        label_fg,
        terhub::ui::palette("dark").title.fg.unwrap(),
        "chip label left the title palette"
    );
}

/// The footer is the discoverability surface. With 40+ panes the filter
/// key is the most valuable one on screen, so it must be named — and it
/// must follow the user's rebind, not a hard-coded `f`.
#[test]
fn footer_names_the_filter_key_and_follows_a_rebind() {
    let mut app = app_with(&["alpha"]);
    let text = render_lines(&mut app, 200, 8).join("\n");
    assert!(
        text.contains(" = filter"),
        "footer no longer advertises the filter key:\n{text}"
    );
    // Default binding is `f`; the label is derived from config, so a
    // rebind is reflected instead of lying to the user.
    assert_eq!(app.config.key_label("filter"), "f");
    app.config
        .keys
        .insert("filter".to_string(), "ctrl+s".to_string());
    let rebound = render_lines(&mut app, 200, 8).join("\n");
    assert!(
        rebound.contains("ctrl+s = filter"),
        "footer ignored the rebound filter key:\n{rebound}"
    );
}

/// A record with no timestamp is "never recorded", not 56 years old: a
/// zero `state_since` must render as a dash, never as 20737d. (The Turn
/// column already guards its zero the same way.)
#[test]
fn zero_state_since_renders_dash_not_epoch() {
    let mut app = app_with_panes();
    let mut r = idle_record("%1", "alpha", 60, terhub::agent::now_unix());
    r.state_since = 0;
    app.agents.insert("%1".to_string(), r);
    let grid = render_grid(&mut app, 140, 12);
    let text: Vec<String> = grid
        .iter()
        .map(|row| row.iter().map(|(s, _, _)| s.as_str()).collect())
        .collect();
    let hdr = text.iter().find(|l| l.contains("For")).unwrap();
    let ix = hdr[..hdr.find("For").unwrap()].chars().count();
    let row = text.iter().position(|l| l.contains("%1")).unwrap();
    assert_eq!(
        grid[row][ix].0, "\u{2014}",
        "zero timestamp rendered as an epoch age"
    );
    // And a recorded timestamp still renders its age.
    let mut app = app_with_panes();
    let mut r = idle_record("%1", "alpha", 60, terhub::agent::now_unix());
    r.state_since = terhub::agent::now_unix() - 90;
    app.agents.insert("%1".to_string(), r);
    let grid = render_grid(&mut app, 140, 12);
    let text: Vec<String> = grid
        .iter()
        .map(|row| row.iter().map(|(s, _, _)| s.as_str()).collect())
        .collect();
    let hdr = text.iter().find(|l| l.contains("For")).unwrap();
    let ix = hdr[..hdr.find("For").unwrap()].chars().count();
    let row = text.iter().position(|l| l.contains("%1")).unwrap();
    // Cells hold single graphemes, so rejoin from the column start.
    let got: String = grid[row][ix..].iter().map(|(s, _, _)| s.as_str()).collect();
    assert!(got.starts_with("1m"), "real age lost its value: {got:?}");
}

/// The attended marker: a visited row wears a quiet check until its
/// state or summary says something new — so a sweep of the table shows
/// what is left, not just what is loud. It never outranks a live mark:
/// a pane that still needs you keeps its `!`, a manual mark keeps `+`.
#[test]
fn attended_marker_renders_and_yields_to_urgent_marks() {
    const SEEN: char = '\u{2713}';
    // Seen and unchanged: the check is drawn.
    let mut app = app_with_panes();
    app.agents.insert(
        "%1".to_string(),
        idle_record("%1", "alpha", 60, terhub::agent::now_unix()),
    );
    app.attended
        .insert("%1".to_string(), ("idle".to_string(), "done".to_string()));
    // The fixture panes are all `active` in one window, which would read
    // `*` (focus disambiguation outranks the check by design); stand the
    // rows down and re-filter so the check itself is what renders.
    app.panes.iter_mut().for_each(|p| p.active = false);
    app.refresh_filtered();
    let text = render_lines(&mut app, 120, 12).join("\n");
    let row = text
        .lines()
        .find(|l| l.contains("%1"))
        .expect("attended row missing");
    assert!(row.contains(SEEN), "visited row wears no check:\n{row}");

    // A pane that still needs you keeps its `!` — the tray counts it,
    // so the table must not pretend it is handled.
    let mut app = app_with_panes();
    app.agents.insert(
        "%1".to_string(),
        waiting_record("%1", "alpha", "approve deploy?"),
    );
    app.attended.insert(
        "%1".to_string(),
        ("waiting".to_string(), "approve deploy?".to_string()),
    );
    let text = render_lines(&mut app, 120, 12).join("\n");
    let row = text
        .lines()
        .find(|l| l.contains("%1"))
        .expect("needy row missing");
    assert!(row.contains('!'), "need lost to the check:\n{row}");
    assert!(!row.contains(SEEN), "check hid the need:\n{row}");

    // A manual mark outranks the automatic one.
    let mut app = app_with_panes();
    app.agents.insert(
        "%1".to_string(),
        idle_record("%1", "alpha", 60, terhub::agent::now_unix()),
    );
    app.attended
        .insert("%1".to_string(), ("idle".to_string(), "done".to_string()));
    app.marked.insert("%1".to_string());
    let text = render_lines(&mut app, 120, 12).join("\n");
    let row = text
        .lines()
        .find(|l| l.contains("%1"))
        .expect("marked row missing");
    assert!(row.contains('+'), "manual mark lost to the check:\n{row}");
    assert!(!row.contains(SEEN), "check hid the manual mark:\n{row}");
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
    // Other sections live behind the sidebar, not on this page.
    for s in ["Theme", "Density", "Toggles", "Filters", "Tray icons"] {
        assert!(text.contains(s), "missing sidebar section {s}:\n{text}");
    }
    // ...but each is one jump away.
    app.goto_settings_section(2); // Toggles
    let lines = render_lines(&mut app, 120, 30);
    let text = lines.join("\n");
    assert!(text.contains("notifications:"), "missing toggle:\n{text}");
}

#[test]
fn settings_theme_section_shows_live_preview_when_wide() {
    let mut app = app_with_panes();
    app.open_settings(); // Theme section, first item
    let lines = render_lines(&mut app, 160, 30);
    let text = lines.join("\n");
    // Sample rows in the hovered theme (dark = first item).
    assert!(text.contains("preview: dark"), "missing preview:\n{text}");
    assert!(text.contains("turn started"), "missing sample row:\n{text}");
    // Hovering dracula re-themes the preview without applying.
    app.settings_selected = 2;
    let lines = render_lines(&mut app, 160, 30);
    let text = lines.join("\n");
    assert!(
        text.contains("preview: dracula"),
        "preview did not follow hover:\n{text}"
    );
    assert_eq!(
        app.config.theme,
        AppConfig::default().theme,
        "hover applied the theme"
    );
}

#[test]
fn settings_preview_hides_when_narrow_or_elsewhere() {
    let mut app = app_with_panes();
    app.open_settings();
    // Narrow terminal: no room, options take the full content.
    let lines = render_lines(&mut app, 60, 30);
    let text = lines.join("\n");
    assert!(
        !text.contains("preview:"),
        "preview leaked into narrow layout:\n{text}"
    );
    // Toggles section at full width: no preview there either.
    app.goto_settings_section(2);
    let lines = render_lines(&mut app, 160, 30);
    let text = lines.join("\n");
    assert!(
        !text.contains("preview:"),
        "preview outside Theme/Density:\n{text}"
    );
}

#[test]
fn settings_popup_scrolls_when_short() {
    let mut app = app_with_panes();
    app.open_settings();
    // Deep in the Theme section (20 themes): a 12-high page cannot
    // show them all, so the content scrolls within the section.
    app.settings_selected = 15;
    let lines = render_lines(&mut app, 120, 12);
    let text = lines.join("\n");
    assert!(app.settings_scroll > 0, "scroll never moved");
    assert!(
        text.contains("▲") || text.contains("▼"),
        "missing scroll marker:\n{text}"
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

#[test]
fn wide_table_fits_long_commands_and_summaries() {
    use terhub::agent::{AgentRecord, AgentState, Source};
    let mut app = app_with(&["alpha"]);
    app.panes = vec![pane("%1", "alpha", "editor", "muse-bin-1.4.2-R4684.1")];
    app.refresh_filtered();
    let summary = format!("summary {}", "x".repeat(22)); // 30 chars
    assert_eq!(summary.chars().count(), 30);
    app.agents.insert(
        "%1".to_string(),
        AgentRecord {
            pane: "%1".to_string(),
            agent: "pi".to_string(),
            state: AgentState::Waiting,
            source: Source::Proc,
            summary: summary.clone(),
            session: "alpha".to_string(),
            window: "editor".to_string(),
            command: "pi".to_string(),
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
        },
    );
    let text = render_lines(&mut app, 195, 24).join("\n");
    assert!(text.contains("muse-bin-1.4.2-R4684.1"), "cmd cut:\n{text}");
    assert!(text.contains(&summary), "summary cut:\n{text}");
}

#[test]
fn title_nudges_hookless_setups() {
    use terhub::agent::{AgentRecord, AgentState, Source};
    let mut app = app_with_panes();
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
        },
    );
    let text = render_lines(&mut app, 140, 20).join("\n");
    assert!(text.contains("no hooks yet"), "missing nudge:\n{text}");
    // One hook reporter silences it.
    app.agents.get_mut("%1").unwrap().source = Source::Hook;
    let text = render_lines(&mut app, 140, 20).join("\n");
    assert!(!text.contains("no hooks yet"), "nudge stuck:\n{text}");
    // A dead daemon says that instead.
    app.agents.get_mut("%1").unwrap().source = Source::Proc;
    app.agents_live = false;
    let text = render_lines(&mut app, 140, 20).join("\n");
    assert!(
        !text.contains("no hooks yet"),
        "nudge over daemon-down:\n{text}"
    );
    assert!(text.contains("daemon down"), "missing daemon-down:\n{text}");
}

#[test]
fn self_and_deleted_rows_recede() {
    let mut app = app_with_panes();
    app.own_pane = Some("%1".to_string());
    app.panes[1].path = "/home/u/nav2 (deleted)".to_string();
    app.refresh_filtered();
    let grid = render_grid(&mut app, 140, 20);
    let text: Vec<String> = grid
        .iter()
        .map(|row| row.iter().map(|(s, _, _)| s.as_str()).collect::<String>())
        .collect();
    let hdr = text.iter().find(|l| l.contains("Pane")).unwrap();
    let px = hdr[..hdr.find("Pane").unwrap()].chars().count();
    let dx = hdr[..hdr.find("Dir").unwrap()].chars().count();
    let r1 = text.iter().position(|l| l.contains("%1")).unwrap();
    let r2 = text.iter().position(|l| l.contains("%2")).unwrap();
    // The TUI's own pane wears the attached accent, not the dim id color.
    assert_ne!(
        grid[r1][px].1,
        grid[r2][px].1,
        "self id not marked:\n{}",
        text.join("\n")
    );
    // A deleted cwd is struck through; a live one is not.
    assert!(
        grid[r2][dx].2.contains(Modifier::CROSSED_OUT),
        "deleted dir not struck"
    );
    assert!(
        !grid[r1][dx].2.contains(Modifier::CROSSED_OUT),
        "live dir struck"
    );
}

fn press(code: crossterm::event::KeyCode) -> crossterm::event::KeyEvent {
    crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::empty())
}

/// Fake tmux that logs argv next to itself (no env, parallel-safe) and
/// answers the verbs spawn/restart/refresh need.
fn fake_tmux(tag: &str) -> (terhub::tmux::Tmux, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let base = std::env::temp_dir().join(format!("terhub-ui-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let bin = base.join("tmux");
    std::fs::write(
        &bin,
        "#!/bin/sh\necho \"$@\" >> \"$(dirname \"$0\")/argv.log\"\n\
         case \"$1\" in new-window) echo \"%99\";; esac\nexit 0\n",
    )
    .unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    (terhub::tmux::Tmux::test(bin), base.join("argv.log"))
}

fn argv_log(log: &std::path::Path) -> String {
    std::fs::read_to_string(log).unwrap_or_default()
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

#[test]
fn bulk_modal_sends_to_marked_with_submit() {
    use crossterm::event::KeyCode;
    let mut app = app_with_panes();
    let (tmux, log) = fake_tmux("bulk");
    app.marked.insert("%1".to_string());
    app.marked.insert("%2".to_string());
    app.on_key(press(KeyCode::Char('b')), &tmux);
    assert!(matches!(app.mode, terhub::app::Mode::PromptBulk { .. }));
    let lines = render_lines(&mut app, 120, 30);
    let text = lines.join("\n");
    assert!(text.contains("Prompt 2 pane(s)"), "no modal:\n{text}");
    assert!(text.contains("%1 %2"), "no targets:\n{text}");
    for c in "go".chars() {
        app.on_key(press(KeyCode::Char(c)), &tmux);
    }
    app.on_key(press(KeyCode::Enter), &tmux);
    assert_eq!(app.mode, terhub::app::Mode::Normal);
    let argv = argv_log(&log);
    assert_eq!(
        argv.matches("send-keys").count(),
        4,
        "want text+Enter per target:\n{argv}"
    );
    assert!(argv.contains("go"), "text lost:\n{argv}");
    assert!(argv.contains("Enter"), "no submit:\n{argv}");
}

#[test]
fn spawn_modal_cycles_profile_and_spawns_with_prompt() {
    use crossterm::event::KeyCode;
    let mut app = app_with_panes();
    let (tmux, log) = fake_tmux("spawn");
    app.on_key(press(KeyCode::Char('w')), &tmux);
    assert!(matches!(
        app.mode,
        terhub::app::Mode::PromptSpawn { profile: 0 }
    ));
    let lines = render_lines(&mut app, 120, 30);
    assert!(
        lines.iter().any(|l| l.contains("Spawn")),
        "no modal:\n{}",
        lines.join("\n")
    );
    // Tab cycles to the second builtin profile (codex).
    app.on_key(press(KeyCode::Tab), &tmux);
    assert!(matches!(
        app.mode,
        terhub::app::Mode::PromptSpawn { profile: 1 }
    ));
    for c in "hello".chars() {
        app.on_key(press(KeyCode::Char(c)), &tmux);
    }
    app.on_key(press(KeyCode::Enter), &tmux);
    assert_eq!(app.mode, terhub::app::Mode::Normal);
    let argv = argv_log(&log);
    assert!(argv.contains("new-window"), "no spawn:\n{argv}");
    assert!(argv.contains("codex"), "wrong profile:\n{argv}");
    assert!(argv.contains("send-keys"), "prompt not sent:\n{argv}");
    assert!(argv.contains("hello"), "prompt text lost:\n{argv}");
}

#[test]
fn restart_confirm_respawns_pane_command() {
    use crossterm::event::KeyCode;
    let mut app = app_with_panes();
    let (tmux, log) = fake_tmux("restart");
    app.on_key(press(KeyCode::Char('e')), &tmux);
    assert!(matches!(
        app.mode,
        terhub::app::Mode::ConfirmRestartPane { .. }
    ));
    app.on_key(press(KeyCode::Enter), &tmux);
    assert_eq!(app.mode, terhub::app::Mode::Normal);
    let argv = argv_log(&log);
    assert!(
        argv.contains("respawn-pane -k -t %1"),
        "no respawn:\n{argv}"
    );
    assert!(argv.contains("nvim"), "command lost:\n{argv}");
}

#[test]
fn pause_key_toggles_live_process_tree() {
    use crossterm::event::KeyCode;
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
    let mut guard = Guard(child);
    let pid = guard.0.id();
    let mut app = app_with_panes();
    app.panes[0].pid = pid;
    app.refresh_filtered();
    let (tmux, _) = fake_tmux("pause");
    app.on_key(press(KeyCode::Char('z')), &tmux);
    assert!(wait_stopped(pid, true), "first z must stop");
    app.on_key(press(KeyCode::Char('z')), &tmux);
    assert!(wait_stopped(pid, false), "second z must resume");
    let _ = &mut guard;
}

fn rec_with_handle(pane: &str, handle: &str) -> terhub::agent::AgentRecord {
    let mut r = rec(pane, "alpha", terhub::agent::AgentState::Working);
    r.handle = handle.to_string();
    r
}

fn rec(pane: &str, session: &str, state: terhub::agent::AgentState) -> terhub::agent::AgentRecord {
    use terhub::agent::Source;
    terhub::agent::AgentRecord {
        pane: pane.to_string(),
        agent: "pi".to_string(),
        state,
        source: Source::Proc,
        summary: String::new(),
        session: session.to_string(),
        window: "w".to_string(),
        command: "pi".to_string(),
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
    }
}

#[test]
fn tabs_show_worst_state_dot() {
    use terhub::agent::AgentState;
    let mut app = app_with_panes();
    app.agents
        .insert("%1".to_string(), rec("%1", "alpha", AgentState::Stuck));
    app.agents
        .insert("%2".to_string(), rec("%2", "beta", AgentState::Working));
    let grid = render_grid(&mut app, 140, 20);
    // Row 0 is the tab bar; attached marks here are all ○, so every ●
    // is a worst-state dot: All=stuck(red), alpha=stuck(red), beta=working(green).
    let dots: Vec<Color> = grid[0]
        .iter()
        .filter(|(s, _, _)| s == "●")
        .map(|(_, fg, _)| *fg)
        .collect();
    assert_eq!(dots.len(), 3, "want All+alpha+beta dots");
    assert_eq!(dots.iter().filter(|c| **c == Color::Red).count(), 2);
    assert_eq!(dots.iter().filter(|c| **c == Color::Green).count(), 1);
    // Idle/unknown-only tabs stay dotless (noise discipline).
    let mut app = app_with_panes();
    app.agents
        .insert("%1".to_string(), rec("%1", "alpha", AgentState::Idle));
    let grid = render_grid(&mut app, 140, 20);
    assert!(
        !grid[0].iter().any(|(s, _, _)| s == "●"),
        "idle must be dotless"
    );
}

#[test]
fn hosts_modal_lists_registry_and_cache_age() {
    let mut app = app_with_panes();
    app.fleet_hosts = vec![terhub::fleet::Host {
        name: "dev".to_string(),
        target: "user@host".to_string(),
        mode: terhub::fleet::Mode::Control,
        label: String::new(),
    }];
    app.mode = terhub::app::Mode::Hosts;
    let lines = render_lines(&mut app, 120, 30);
    let text = lines.join("\n");
    assert!(text.contains("Fleet hosts"), "no modal:\n{text}");
    assert!(text.contains("never fetched"), "no age:\n{text}");
    assert!(text.contains("dev user@host [control]"), "no row:\n{text}");
    assert!(text.contains("a=add"), "no hints:\n{text}");
}

#[test]
fn remote_rows_render_in_table() {
    let mut app = app_with_panes();
    app.panes.push(Pane {
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
        activity: 1,
        pid: 0,
        bell: false,
    });
    app.refresh_filtered();
    let lines = render_lines(&mut app, 160, 24);
    let text = lines.join("\n");
    // The Pane column shows the backend-local id; the host identity
    // rides the Session column ("dev ▸ web" below) so the namespace is
    // never clipped away in a 6-wide column.
    assert!(text.contains("%1"), "remote id missing:\n{text}");
    assert!(
        text.contains("dev \u{25b8} web"),
        "remote session missing:\n{text}"
    );
}

#[test]
fn preview_title_shows_cpu_and_git_segments() {
    let mut app = app_with_panes();
    app.show_preview = true;
    app.preview_cpu_pct = Some(42.0);
    app.preview_git_line = Some("git main +45/-12 · 2 changed".to_string());
    let lines = render_lines(&mut app, 160, 24);
    let text = lines.join("\n");
    assert!(text.contains("cpu 42%"), "no cpu:\n{text}");
    assert!(text.contains("git main +45/-12"), "no git:\n{text}");
    // Absent data leaves no hole in the title.
    let mut app = app_with_panes();
    app.show_preview = true;
    let lines = render_lines(&mut app, 160, 24);
    let text = lines.join("\n");
    assert!(!text.contains("cpu"), "phantom cpu:\n{text}");
    assert!(text.contains("left-click"), "title broken:\n{text}");
}

#[test]
fn refresh_keeps_preview_when_filter_matches_only_foreign_rows() {
    let mut app = app_with_panes();
    let (tmux, _) = fake_tmux("keepprev");
    app.native_rows = vec![pane("konsole:999:6", "konsole", "tab", "pi")];
    app.merge_native();
    app.filter = "konsole".to_string();
    app.refresh_filtered();
    assert_eq!(app.filtered.len(), 1);
    app.show_preview = true;
    app.preview = "hello".to_string();
    // Fake tmux reports zero local panes: the tick must not mid-pass
    // through an empty list and close the preview.
    app.refresh(&tmux);
    assert!(app.show_preview, "tick closed foreign-only preview");
    assert_eq!(app.filtered.len(), 1);
}

#[test]
fn preview_scroll_renders_offset_content() {
    let mut app = app_with_panes();
    app.show_preview = true;
    app.preview = (0..60)
        .map(|i| format!("pline {i:03}"))
        .collect::<Vec<_>>()
        .join("\n");
    app.preview_scroll = 40;
    let lines = render_lines(&mut app, 120, 12);
    let text = lines.join("\n");
    assert!(
        !text.contains("pline 000"),
        "top leaked through scroll:\n{text}"
    );
    assert!(text.contains("pline 040"), "offset start missing:\n{text}");
    assert!(text.contains("40/60"), "scroll position missing:\n{text}");
}

#[test]
fn table_shows_handle_column() {
    let mut app = app_with_panes();
    app.agents
        .insert("%1".to_string(), rec_with_handle("%1", "@pi-alpha"));
    let lines = render_lines(&mut app, 160, 24);
    let text = lines.join("\n");
    assert!(text.contains("Handle"), "no header:\n{text}");
    assert!(text.contains("@pi-alpha"), "no handle:\n{text}");
}

#[test]
fn narrow_table_keeps_fixed_columns_and_clicks_in_bounds() {
    let mut app = app_with_panes();
    let (lines, areas) = render_with_areas(&mut app, 100, 24);
    let header = lines
        .iter()
        .find(|l| l.contains("Pane"))
        .expect("no header line");
    for h in ["Pane", "State", "Src", "For", "Turn", "Idle"] {
        assert!(header.contains(h), "lost {h}:\n{header}");
    }
    for hit in &areas.headers {
        assert!(
            hit.area.x + hit.area.width <= 100,
            "hit out of bounds: {:?}",
            hit.area
        );
    }
}

// File ends at line 1499 (this comment sits on its own line so the
// last test's closing brace is on 1497 and the file's line count is
// reproducible across editors).
