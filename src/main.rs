//! CLI entry: `terhub` (TUI) + subcommands for scripts.
use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use crossterm::{
    event::{self, Event, KeyEventKind, MouseButton, MouseEventKind},
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{Terminal, backend::CrosstermBackend};
use std::io;
use std::time::{Duration, Instant};
use terhub::{
    agent::{AgentState, HookEvent},
    app::{App, AppEvent},
    config,
    daemon::{self, Request},
    history::fmt_dur,
    tmux::Tmux,
    ui::ClickAreas,
};

#[derive(Parser)]
#[command(name = "terhub", about = "Terminal-first session hub over tmux.")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// List sessions.
    List {
        /// Print only names.
        #[arg(long)]
        plain: bool,
    },
    /// Create a session.
    New {
        name: String,
        #[arg(long)]
        dir: Option<String>,
        #[arg(long)]
        window: Option<String>,
    },
    /// Attach to a session.
    Attach { name: String },
    /// Kill a session.
    Kill { name: String },
    /// List panes of a session.
    Panes { session: String },
    /// Print a pane's recent content.
    Capture {
        pane: String,
        /// Scrollback lines to capture.
        #[arg(long, default_value_t = 60)]
        lines: u32,
    },
    /// Jump the client to a pane.
    Focus { pane: String },
    /// Kill a pane.
    KillPane { pane: String },
    /// Swap two panes' positions.
    SwapPane { a: String, b: String },
    /// Show agent states from the daemon.
    Status {
        /// Print the raw snapshot as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Push a hook event to the daemon (fire-and-forget for agent hooks).
    Hook {
        #[arg(long)]
        agent: String,
        #[arg(long)]
        pane: String,
        /// working|waiting|error|idle|stuck|limited|unknown (aliases accepted).
        #[arg(long)]
        state: String,
        #[arg(long)]
        summary: Option<String>,
        /// Read the summary from stdin (JSON {"message": ...} or raw head).
        #[arg(long, conflicts_with = "summary")]
        summary_stdin: bool,
        /// The user's prompt text (goes to the prompt ledger).
        #[arg(long, conflicts_with = "prompt_stdin")]
        prompt: Option<String>,
        /// Read the prompt from stdin (JSON {"prompt": ...} or raw head).
        #[arg(long, conflicts_with = "prompt")]
        prompt_stdin: bool,
    },
    /// Manage the terhubd daemon.
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Install agent lifecycle hooks (claude settings, pi extension).
    Hooks {
        #[command(subcommand)]
        action: HooksAction,
    },
    /// Jump to the agent waiting longest.
    Attend {
        /// Cycle to the next needy agent instead of the longest-waiting.
        #[arg(long)]
        cycle: bool,
        /// List agents needing attention without jumping.
        #[arg(long)]
        list: bool,
    },
    /// Overlay of this window's panes with agent states.
    Peek {
        /// Print the report as text instead of opening a popup.
        #[arg(long)]
        plain: bool,
        /// Interactive digit-to-jump picker (runs inside the popup).
        #[arg(long, hide = true)]
        pick: bool,
    },
    /// One-line summary for tmux status-right. Always exits 0.
    StatusLine {
        /// Show one pane's agent instead of global counts.
        #[arg(long)]
        pane: Option<String>,
    },
    /// Time-in-state stats from the activity ledger.
    Stats {
        #[arg(long, default_value = "today")]
        since: String,
        #[arg(long, value_enum, default_value_t = GroupByArg::Agent)]
        group_by: GroupByArg,
    },
    /// All stats breakdowns at once.
    Report {
        #[arg(long, default_value = "today")]
        since: String,
        /// Emit the report as JSON.
        #[arg(long, conflicts_with = "markdown")]
        json: bool,
        /// Emit the report as Markdown tables.
        #[arg(long, conflicts_with = "json")]
        markdown: bool,
    },
    /// Raw activity ledger entries.
    Activity {
        #[arg(long, default_value = "today")]
        since: String,
        #[arg(long, default_value = "all", value_parser = ["agent", "session", "rule", "human", "prompt", "all"])]
        kind: String,
        #[arg(long)]
        json: bool,
    },
    /// Recent user prompts from the ledger, newest first.
    Recap {
        #[arg(long, default_value = "today")]
        since: String,
        /// Only prompts for one pane.
        #[arg(long)]
        pane: Option<String>,
        /// Most prompts to show.
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// Scrollable timeline of agent work.
    Timeline {
        #[arg(long, default_value = "today")]
        since: String,
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        agent: Option<String>,
        /// Entry order: chronological, or attention states first.
        #[arg(long, value_parser = ["time", "waiting"], default_value = "time")]
        sort: String,
        /// Render the entry log or a day × hour heatmap.
        #[arg(long, value_parser = ["time", "heatmap"], default_value = "time")]
        view: String,
    },
    /// Rules that act on agent state.
    Automation {
        #[command(subcommand)]
        action: AutomationAction,
    },
    /// MCP stdio server for agent orchestration.
    Mcp,
    /// Loopback web dashboard (binds 127.0.0.1 only).
    Serve {
        /// Port to listen on (0 = ephemeral).
        #[arg(long, default_value_t = 0)]
        port: u16,
    },
    /// Type text into a pane.
    Send {
        pane: String,
        text: String,
        /// Don't press Enter after typing.
        #[arg(long)]
        no_submit: bool,
    },
    /// Manage fleet hosts.
    Host {
        #[command(subcommand)]
        action: HostAction,
    },
    /// Observe and drive fleet hosts over SSH.
    Fleet {
        #[command(subcommand)]
        action: FleetAction,
    },
    /// Zellij baseline backend (list/attach).
    Zellij {
        #[command(subcommand)]
        action: ZellijAction,
    },
    /// Native terminal tabs (Konsole, kitty, WezTerm).
    Native {
        #[command(subcommand)]
        action: NativeAction,
    },
    /// Show or init config.
    Config {
        #[arg(long)]
        show: bool,
        #[arg(long)]
        path: bool,
        #[arg(long)]
        init: bool,
        /// Validate the config file and report problems.
        #[arg(long)]
        check: bool,
    },
    /// One-shot setup: config file, agent hooks, daemon.
    Init {
        /// Print the steps without running them.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(ValueEnum, Clone, Copy)]
enum GroupByArg {
    Day,
    Agent,
    Session,
}

#[derive(Subcommand)]
enum AutomationAction {
    /// List rules with validation and recent fire counts.
    List,
    /// Dry-run one rule against live state without acting.
    Test { name: String },
}

#[derive(Subcommand)]
enum HostAction {
    Add {
        name: String,
        target: String,
        #[arg(long, default_value = "observe")]
        mode: String,
    },
    List,
    Remove {
        name: String,
    },
    Doctor {
        name: String,
    },
}

#[derive(Subcommand)]
enum FleetAction {
    Status {
        #[arg(long)]
        json: bool,
    },
    Send {
        host: String,
        pane: String,
        text: String,
        #[arg(long)]
        no_submit: bool,
    },
    /// SSH into a host's tmux (session defaults to most-recent).
    Attach {
        host: String,
        session: Option<String>,
    },
}

#[derive(Subcommand)]
enum ZellijAction {
    List,
    Attach { name: String },
}

#[derive(Subcommand)]
enum NativeAction {
    /// List tabs across all native terminals.
    List {
        /// Print the tabs as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Open a fresh tab in a backend.
    Spawn {
        /// Which terminal (konsole|kitty|wezterm).
        backend: String,
    },
}

#[derive(Subcommand)]
enum DaemonAction {
    Start,
    Stop,
    Status,
}

#[derive(Subcommand)]
enum HooksAction {
    /// Add terhub hooks (idempotent; backs up the first write).
    Install {
        /// Which agent to wire (default: all).
        #[arg(value_parser = ["all", "claude", "pi"], default_value = "all")]
        agent: String,
    },
    /// Show which agents are wired.
    Status,
    /// Remove terhub hooks (user hooks are kept).
    Uninstall {
        /// Which agent to unwire (default: all).
        #[arg(value_parser = ["all", "claude", "pi"], default_value = "all")]
        agent: String,
    },
}

/// Print a line, silently tolerating closed pipes (`| head`).
fn print_out(line: &str) {
    print_raw(&format!("{line}\n"));
}

/// Write bytes verbatim (no added newline), tolerating closed pipes.
fn print_raw(bytes: &str) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    match out.write_all(bytes.as_bytes()) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => std::process::exit(0),
        Err(_) => {}
    }
}

fn cmd_list(plain: bool) -> i32 {
    let tmux = match Tmux::new() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("terhub: {e:?}");
            return 1;
        }
    };
    let sessions = match tmux.list_sessions() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("terhub: {e:?}");
            return 1;
        }
    };
    if plain {
        for s in &sessions {
            print_out(&s.name);
        }
        return 0;
    }
    if sessions.is_empty() {
        print_out("no sessions");
        return 0;
    }
    let width = sessions
        .iter()
        .map(|s| s.name.len())
        .max()
        .unwrap_or(4)
        .max(4);
    print_out(&format!(
        "{:<width$}  WIN  ATT  CREATED             ACTIVITY",
        "NAME"
    ));
    for s in &sessions {
        let att = if s.attached { "yes" } else { "no" };
        print_out(&format!(
            "{:<width$}  {:<4} {:<4} {:<19} {}",
            s.name, s.windows, att, s.created, s.activity
        ));
    }
    0
}

fn cmd_new(name: &str, dir: Option<&str>, window: Option<&str>) -> i32 {
    let tmux = match Tmux::new() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("terhub: {e:?}");
            return 1;
        }
    };
    match tmux.new_session(name, dir, window) {
        Ok(s) => {
            println!("{}", s.name);
            0
        }
        Err(e) => {
            eprintln!("terhub: {e:?}");
            1
        }
    }
}

fn cmd_attach(name: &str) -> i32 {
    let tmux = match Tmux::new() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("terhub: {e:?}");
            return 1;
        }
    };
    if !tmux.session_exists(name) {
        eprintln!("terhub: no such session: {name}");
        return 1;
    }
    match tmux.attach_here(name) {
        Ok(()) => 0, // unreachable: exec replaces the process
        Err(e) => {
            eprintln!("terhub: {e:?}");
            1
        }
    }
}

fn cmd_panes(session: &str) -> i32 {
    let tmux = match Tmux::new() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("terhub: {e:?}");
            return 1;
        }
    };
    match tmux.list_panes(session) {
        Ok(panes) => {
            if panes.is_empty() {
                print_out("no panes");
                return 0;
            }
            print_out("ID    WINDOW            CMD       PATH");
            for p in &panes {
                let mark = if p.active { "*" } else { " " };
                print_out(&format!(
                    "{:<5} {:<17} {:<9} {}",
                    format!("{}{}", mark, p.id),
                    format!("{}:[{}]", p.window_name, p.index),
                    p.command,
                    p.path,
                ));
            }
            0
        }
        Err(e) => {
            eprintln!("terhub: {e:?}");
            1
        }
    }
}

fn cmd_capture(pane: &str, lines: u32) -> i32 {
    if terhub::native::is_native_id(pane) {
        return match terhub::native::capture(pane, lines) {
            Ok(mut text) => {
                if !text.ends_with('\n') {
                    text.push('\n');
                }
                print_raw(&text);
                0
            }
            Err(e) => {
                eprintln!("terhub: {e:#}");
                1
            }
        };
    }
    let tmux = match Tmux::new() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("terhub: {e:?}");
            return 1;
        }
    };
    match tmux.capture_pane(pane, lines) {
        Ok(mut text) => {
            if !text.ends_with('\n') {
                text.push('\n');
            }
            print_raw(&text);
            0
        }
        Err(e) => {
            eprintln!("terhub: {e:?}");
            1
        }
    }
}

fn cmd_focus(pane: &str) -> i32 {
    if terhub::native::is_native_id(pane) {
        return match terhub::native::focus(pane) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("terhub: {e:#}");
                1
            }
        };
    }
    let tmux = match Tmux::new() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("terhub: {e:?}");
            return 1;
        }
    };
    match tmux.focus_pane(pane) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("terhub: {e:?}");
            1
        }
    }
}

fn cmd_swap_pane(a: &str, b: &str) -> i32 {
    let tmux = match Tmux::new() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("terhub: {e:?}");
            return 1;
        }
    };
    match tmux.swap_panes(a, b) {
        Ok(()) => {
            println!("swapped {a} {b}");
            0
        }
        Err(e) => {
            eprintln!("terhub: {e:?}");
            1
        }
    }
}

fn cmd_kill_pane(pane: &str) -> i32 {
    let tmux = match Tmux::new() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("terhub: {e:?}");
            return 1;
        }
    };
    match tmux.kill_pane(pane) {
        Ok(()) => {
            println!("killed pane {pane}");
            0
        }
        Err(e) => {
            eprintln!("terhub: {e:?}");
            1
        }
    }
}

fn cmd_status(json: bool) -> i32 {
    let snapshot = match daemon::fetch_snapshot(Duration::from_secs(2)) {
        Ok(s) => s,
        Err(_) => {
            eprintln!("terhub: daemon not running (terhub daemon start)");
            return 1;
        }
    };
    if json {
        match serde_json::to_string_pretty(&snapshot) {
            Ok(text) => {
                println!("{text}");
                return 0;
            }
            Err(e) => {
                eprintln!("terhub: {e:?}");
                return 1;
            }
        }
    }
    let now = terhub::agent::now_unix();
    let mut agents = snapshot.agents;
    agents.sort_by(|a, b| {
        b.state
            .needs_attention()
            .cmp(&a.state.needs_attention())
            .then(a.state_since.cmp(&b.state_since))
    });
    let needy = agents.iter().filter(|a| a.state.needs_attention()).count();
    print_out(&format!("{} agent(s), {} need you", agents.len(), needy));
    if agents.is_empty() {
        return 0;
    }
    print_out(terhub::attend::status_table(&agents, now).trim_end());
    0
}

fn cmd_hook(
    agent: &str,
    pane: &str,
    state: &str,
    summary: Option<&str>,
    summary_stdin: bool,
    prompt: Option<&str>,
    prompt_stdin: bool,
) -> i32 {
    let Some(state) = AgentState::parse(state) else {
        eprintln!("terhub: bad state '{state}' (working|waiting|error|idle|stuck|limited|unknown)");
        return 1;
    };
    // Both stdin readers drain the same pipe: --summary-stdin and
    // --prompt-stdin together would split it, so read once and share.
    let stdin_text = if summary_stdin || prompt_stdin {
        read_stdin_text()
    } else {
        String::new()
    };
    let summary = if summary_stdin {
        terhub::hooks::stdin_summary(&stdin_text)
    } else {
        summary.unwrap_or("").to_string()
    };
    let prompt = if prompt_stdin {
        terhub::hooks::stdin_prompt(&stdin_text)
    } else {
        prompt.unwrap_or("").to_string()
    };
    let ev = HookEvent {
        pane: pane.to_string(),
        agent: agent.to_string(),
        state,
        summary,
        prompt,
    };
    // Fire-and-forget: hooks must never break the agent they observe.
    if let Err(e) = daemon::send_event(&ev, Duration::from_secs(1)) {
        eprintln!("terhub: hook not delivered ({e:?})");
    }
    0
}

/// Read hook stdin once for `--summary-stdin` / `--prompt-stdin`;
/// unreadable stdin degrades to empty (fire-and-forget must not fail).
fn read_stdin_text() -> String {
    use std::io::Read;
    let mut raw = String::new();
    if std::io::stdin().read_to_string(&mut raw).is_ok() {
        raw
    } else {
        String::new()
    }
}

fn cmd_hooks(action: &HooksAction) -> i32 {
    use terhub::hooks;
    match action {
        HooksAction::Status => {
            let cpath = hooks::claude_settings_path();
            let wired = hooks::claude_status(&cpath);
            if wired.len() == hooks::claude_expected_events() {
                print_out(&format!("claude: installed ({})", cpath.display()));
            } else if wired.is_empty() {
                print_out("claude: not installed");
            } else {
                print_out(&format!("claude: partial ({})", wired.join(", ")));
            }
            let ppath = hooks::pi_extension_path();
            if hooks::pi_status(&ppath) {
                print_out(&format!("pi: installed ({})", ppath.display()));
            } else {
                print_out("pi: not installed");
            }
            0
        }
        HooksAction::Install { agent } => {
            let mut code = 0;
            if agent == "all" || agent == "claude" {
                match hooks::claude_install(&hooks::claude_settings_path()) {
                    Ok(report) => {
                        let added: Vec<_> =
                            report.iter().filter(|(_, a)| *a).map(|(e, _)| *e).collect();
                        if added.is_empty() {
                            print_out("claude: already installed");
                        } else {
                            print_out(&format!("claude: installed ({})", added.join(", ")));
                        }
                    }
                    Err(e) => {
                        eprintln!("terhub: claude install failed: {e:?}");
                        code = 1;
                    }
                }
            }
            if agent == "all" || agent == "pi" {
                match hooks::pi_install(&hooks::pi_extension_path()) {
                    Ok(true) => print_out(&format!(
                        "pi: installed ({})",
                        hooks::pi_extension_path().display()
                    )),
                    Ok(false) => print_out("pi: already installed"),
                    Err(e) => {
                        eprintln!("terhub: pi install failed: {e:?}");
                        code = 1;
                    }
                }
            }
            code
        }
        HooksAction::Uninstall { agent } => {
            let mut code = 0;
            if agent == "all" || agent == "claude" {
                match hooks::claude_uninstall(&hooks::claude_settings_path()) {
                    Ok(cleaned) if cleaned.is_empty() => print_out("claude: not installed"),
                    Ok(cleaned) => print_out(&format!("claude: removed ({})", cleaned.join(", "))),
                    Err(e) => {
                        eprintln!("terhub: claude uninstall failed: {e:?}");
                        code = 1;
                    }
                }
            }
            if agent == "all" || agent == "pi" {
                match hooks::pi_uninstall(&hooks::pi_extension_path()) {
                    Ok(true) => print_out("pi: removed"),
                    Ok(false) => print_out("pi: not installed"),
                    Err(e) => {
                        eprintln!("terhub: pi uninstall failed: {e:?}");
                        code = 1;
                    }
                }
            }
            code
        }
    }
}

fn daemon_bin() -> std::path::PathBuf {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let sibling = dir.join("terhubd");
        if sibling.is_file() {
            return sibling;
        }
    }
    std::path::PathBuf::from("terhubd")
}

fn cmd_daemon(action: &DaemonAction) -> i32 {
    match action {
        DaemonAction::Status => {
            if daemon::daemon_alive() {
                println!("running ({})", daemon::socket_path().display());
                0
            } else {
                println!("not running");
                1
            }
        }
        DaemonAction::Start => {
            if daemon::daemon_alive() {
                println!("already running ({})", daemon::socket_path().display());
                return 0;
            }
            let _ = std::fs::remove_file(daemon::socket_path()); // stale socket
            let log_file = std::fs::File::create(daemon::log_path());
            let mut cmd = std::process::Command::new(daemon_bin());
            cmd.stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null());
            match log_file {
                Ok(f) => {
                    cmd.stderr(f);
                }
                Err(_) => {
                    cmd.stderr(std::process::Stdio::null());
                }
            }
            match cmd.spawn() {
                Ok(_) => {}
                Err(e) => {
                    eprintln!("terhub: failed to start terhubd: {e}");
                    return 1;
                }
            }
            for _ in 0..50 {
                if daemon::daemon_alive() {
                    println!("started ({})", daemon::socket_path().display());
                    return 0;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            eprintln!("terhub: daemon did not come up");
            if let Ok(text) = std::fs::read_to_string(daemon::log_path()) {
                let tail: Vec<&str> = text.lines().collect();
                let start = tail.len().saturating_sub(5);
                for line in &tail[start..] {
                    eprintln!("terhubd: {line}");
                }
            }
            1
        }
        DaemonAction::Stop => {
            if !daemon::daemon_alive() {
                let _ = std::fs::remove_file(daemon::socket_path());
                println!("not running");
                return 0;
            }
            let _ = daemon::send_request(&Request::Shutdown, Duration::from_secs(2));
            for _ in 0..30 {
                if !daemon::daemon_alive() {
                    println!("stopped");
                    return 0;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            // Last resort: pid file + TERM, then verify before claiming it.
            if let Ok(pid) = std::fs::read_to_string(daemon::pid_path()) {
                let _ = std::process::Command::new("kill").arg(pid.trim()).status();
                for _ in 0..20 {
                    if !daemon::daemon_alive() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
            if daemon::daemon_alive() {
                eprintln!("terhub: daemon would not stop (kill it by hand)");
                return 1;
            }
            let _ = std::fs::remove_file(daemon::socket_path());
            println!("stopped");
            0
        }
    }
}

fn cmd_attend(cycle: bool, list: bool) -> i32 {
    use terhub::attend as at;
    let snapshot = match daemon::fetch_snapshot(Duration::from_secs(2)) {
        Ok(s) => s,
        Err(_) => {
            eprintln!("terhub: daemon not running (terhub daemon start)");
            return 1;
        }
    };
    if list {
        let needy: Vec<_> = at::needy_first(snapshot.agents.clone())
            .into_iter()
            .filter(|a| a.state.needs_attention())
            .collect();
        if needy.is_empty() {
            println!("all clear ({} agents)", snapshot.agents.len());
            return 0;
        }
        let now = terhub::agent::now_unix();
        for a in &needy {
            print_out(&format!(
                "{} {} {} {} {} {}",
                a.pane,
                a.agent,
                a.state.label(),
                fmt_dur(a.waiting_secs(now)),
                a.session,
                a.summary,
            ));
        }
        return 0;
    }
    let after = if cycle { at::read_last_attend() } else { None };
    match at::pick_attend(&snapshot.agents, after.as_deref()) {
        None => {
            println!("all clear ({} agents)", snapshot.agents.len());
            0
        }
        Some(a) => {
            println!("{} {} {} ({})", a.pane, a.agent, a.state.label(), a.session);
            at::write_last_attend(&a.pane);
            cmd_focus(&a.pane)
        }
    }
}

fn cmd_peek(plain: bool, pick: bool) -> i32 {
    use std::collections::HashMap;
    use terhub::attend as at;
    let tmux = match Tmux::new() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("terhub: {e:?}");
            return 1;
        }
    };
    let snapshot = daemon::fetch_snapshot(Duration::from_secs(2)).unwrap_or_default();
    let (session, window) = match at::current_window(&tmux) {
        Ok(w) => w,
        Err(_) => {
            eprintln!("terhub: peek needs tmux (run inside tmux, or use status)");
            return 1;
        }
    };
    let panes: Vec<terhub::tmux::Pane> = match tmux.list_panes(&session) {
        Ok(p) => p.into_iter().filter(|p| p.window_id == window).collect(),
        Err(e) => {
            eprintln!("terhub: {e:?}");
            return 1;
        }
    };
    let agents: HashMap<String, terhub::agent::AgentRecord> = snapshot
        .agents
        .into_iter()
        .map(|a| (a.pane.clone(), a))
        .collect();
    let total_panes = panes.len();
    let rows = at::peek_rows(panes, &agents);
    let window_name = tmux
        .display(&window, "#{window_name}")
        .map(|s| s.trim().to_string())
        .unwrap_or(window.clone());
    let report = at::peek_report(
        &session,
        &window_name,
        &rows,
        total_panes,
        terhub::agent::now_unix(),
    );
    if plain {
        print_out(&report);
        return 0;
    }
    if pick {
        println!("{report}");
        return peek_pick(&rows);
    }
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "terhub".to_string());
    match std::process::Command::new("tmux")
        .args([
            "display-popup",
            "-E",
            "-w",
            "80%",
            "-h",
            "80%",
            &format!("{exe} peek --pick"),
        ])
        .status()
    {
        Ok(status) if status.success() => 0,
        _ => {
            println!("{report}");
            0
        }
    }
}

fn peek_pick(rows: &[terhub::attend::PeekRow]) -> i32 {
    use crossterm::event::{self, Event, KeyCode};
    if terminal::enable_raw_mode().is_err() {
        return 1;
    }
    let result = loop {
        let key = match event::read() {
            Ok(Event::Key(k)) => k.code,
            Ok(_) => continue,
            Err(_) => break 1, // broken input: quit, don't hot-spin
        };
        match key {
            KeyCode::Char('q') | KeyCode::Esc => break 0,
            KeyCode::Char(d) => {
                if let Some(row) = rows.iter().find(|r| r.digit == d) {
                    break cmd_focus(&row.pane.id);
                }
            }
            _ => {}
        }
    };
    let _ = terminal::disable_raw_mode();
    println!();
    result
}

fn cmd_status_line(pane: Option<&str>) -> i32 {
    let text = daemon::fetch_snapshot(Duration::from_secs(1))
        .map(|s| terhub::attend::status_line(&s.agents, pane))
        .unwrap_or_else(|_| "◎ --".to_string());
    println!("{text}");
    0
}

fn resolve_since(since: &str, now: u64) -> Option<Option<u64>> {
    match terhub::history::parse_since(since, now) {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("terhub: {e:?}");
            None
        }
    }
}

/// Human label for a resolved cutoff (report headers).
fn since_label(since: Option<u64>) -> String {
    match since {
        None => "all".to_string(),
        Some(t) => terhub::history::ymd_label(terhub::history::day_of(t)),
    }
}

fn print_stats_table(rows: &[terhub::history::StatsRow]) {
    print_out("KEY             WORK    WAIT    ERR     IDLE    TOTAL");
    let mut tw = 0u64;
    let (mut ta, mut te, mut ti) = (0u64, 0u64, 0u64);
    for r in rows {
        tw += r.work;
        ta += r.wait;
        te += r.err;
        ti += r.idle;
        print_out(&format!(
            "{:<15} {:<7} {:<7} {:<7} {:<7} {}",
            r.key,
            terhub::history::fmt_dur(r.work),
            terhub::history::fmt_dur(r.wait),
            terhub::history::fmt_dur(r.err),
            terhub::history::fmt_dur(r.idle),
            terhub::history::fmt_dur(r.total()),
        ));
    }
    print_out(&format!(
        "{:<15} {:<7} {:<7} {:<7} {:<7} {}",
        "TOTAL",
        terhub::history::fmt_dur(tw),
        terhub::history::fmt_dur(ta),
        terhub::history::fmt_dur(te),
        terhub::history::fmt_dur(ti),
        terhub::history::fmt_dur(tw + ta + te + ti),
    ));
}

fn cmd_stats(since: &str, group_by: GroupByArg) -> i32 {
    use terhub::history as h;
    let now = terhub::agent::now_unix();
    let Some(since) = resolve_since(since, now) else {
        return 1;
    };
    let group = match group_by {
        GroupByArg::Day => h::GroupBy::Day,
        GroupByArg::Agent => h::GroupBy::Agent,
        GroupByArg::Session => h::GroupBy::Session,
    };
    let spans = h::build_spans(&h::read_entries(&h::ledger_path()), now);
    let rows = h::stats(&spans, since, group);
    if rows.iter().all(|r| r.total() == 0) {
        print_out("no activity in range");
        return 0;
    }
    print_stats_table(&rows);
    0
}

fn cmd_report(since: &str, json: bool, markdown: bool) -> i32 {
    use terhub::history as h;
    let now = terhub::agent::now_unix();
    let Some(since) = resolve_since(since, now) else {
        return 1;
    };
    let entries = h::read_entries(&h::ledger_path());
    let spans = h::build_spans(&entries, now);
    let data = h::ReportData {
        by_day: h::stats(&spans, since, h::GroupBy::Day),
        by_agent: h::stats(&spans, since, h::GroupBy::Agent),
        by_session: h::stats(&spans, since, h::GroupBy::Session),
        foreground: h::foreground_totals(&h::build_foreground(&entries, now), since),
    };
    if json {
        print_out(&h::report_json(&data));
        return 0;
    }
    if markdown {
        print_out(&h::report_markdown(&data, &since_label(since)));
        return 0;
    }
    for (name, rows) in [
        ("by day", &data.by_day),
        ("by agent", &data.by_agent),
        ("by session", &data.by_session),
    ] {
        print_out(&format!("== {name} =="));
        if rows.iter().all(|r| r.total() == 0) {
            print_out("no activity in range");
        } else {
            print_stats_table(rows);
        }
    }
    print_out("== foreground ==");
    if data.foreground.is_empty() {
        print_out("no activity in range");
    } else {
        for f in &data.foreground {
            print_out(&format!("{:<15} {}", f.pane, h::fmt_dur(f.secs)));
        }
    }
    0
}

fn cmd_activity(since: &str, kind: &str, json: bool) -> i32 {
    use terhub::history as h;
    let now = terhub::agent::now_unix();
    let Some(since) = resolve_since(since, now) else {
        return 1;
    };
    let entries: Vec<h::Entry> = h::read_entries(&h::ledger_path())
        .into_iter()
        .filter(|e| since.is_none_or(|c| e.t >= c))
        .filter(|e| kind == "all" || e.kind == kind)
        .collect();
    if entries.is_empty() {
        print_out("no activity in range");
        return 0;
    }
    for e in &entries {
        if json {
            if let Ok(line) = serde_json::to_string(e) {
                print_out(&line);
            }
            continue;
        }
        let when = format!("{} {}", h::ymd_label(h::day_of(e.t)), h::hm_label(e.t));
        if e.kind == "session" {
            print_out(&format!(
                "{when} session {} {}",
                e.session,
                e.event.as_deref().unwrap_or("?")
            ));
        } else if e.kind == "rule" {
            print_out(&format!(
                "{when} rule {} {} {}",
                e.event.as_deref().unwrap_or("?"),
                e.pane,
                e.summary,
            ));
        } else if e.kind == "human" {
            print_out(&format!(
                "{when} human {} {}",
                e.event.as_deref().unwrap_or("?"),
                e.pane,
            ));
        } else if e.kind == "prompt" {
            print_out(&format!(
                "{when} prompt {} {} {}",
                e.pane, e.agent, e.summary
            ));
        } else {
            let from = e.from.map(|s| s.label()).unwrap_or("?");
            let to = e.to.map(|s| s.label()).unwrap_or("?");
            print_out(&format!(
                "{when} {} {} {from}->{to} {}:{} {}",
                e.pane, e.agent, e.session, e.window, e.summary,
            ));
        }
    }
    0
}

fn cmd_recap(since: &str, pane: Option<&str>, limit: usize) -> i32 {
    use terhub::history as h;
    let now = terhub::agent::now_unix();
    let Some(since) = resolve_since(since, now) else {
        return 1;
    };
    let entries: Vec<h::Entry> = h::read_entries(&h::ledger_path())
        .into_iter()
        .filter(|e| since.is_none_or(|c| e.t >= c))
        .collect();
    let prompts = h::recent_prompts(&entries, pane, limit.max(1));
    if prompts.is_empty() {
        print_out("no prompts in range");
        return 0;
    }
    for e in &prompts {
        let when = format!("{} {}", h::ymd_label(h::day_of(e.t)), h::hm_label(e.t));
        let age = h::fmt_dur(now.saturating_sub(e.t));
        print_out(&format!(
            "{when} ({age} ago) {} {}: {}",
            e.pane, e.agent, e.summary
        ));
    }
    0
}

fn cmd_timeline(
    since: &str,
    session: Option<&str>,
    agent: Option<&str>,
    sort: &str,
    view: &str,
) -> i32 {
    use terhub::history as h;
    let now = terhub::agent::now_unix();
    let Some(since) = resolve_since(since, now) else {
        return 1;
    };
    let mut spans = h::build_spans(&h::read_entries(&h::ledger_path()), now);
    spans.retain(|s| {
        since.is_none_or(|c| s.end >= c)
            && session.is_none_or(|f| s.session.contains(f))
            && agent.is_none_or(|f| s.agent.contains(f))
    });
    h::sort_spans(
        &mut spans,
        if sort == "waiting" {
            h::TimelineSort::Waiting
        } else {
            h::TimelineSort::Time
        },
    );
    match run_timeline_tui(spans, view == "heatmap") {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("terhub: {e:?}");
            1
        }
    }
}

fn run_timeline_tui(spans: Vec<terhub::history::Span>, heatmap: bool) -> Result<()> {
    use terhub::history::TimelineView;
    let _guard = TerminalGuard::new()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut view = TimelineView { spans, scroll: 0 };
    let mut page_h: usize = 20;
    let pal = terhub::ui::palette(terhub::config::load_config(None).palette());
    let line_count = |view: &TimelineView| -> usize {
        if heatmap {
            terhub::history::heatmap_line_count(&view.spans)
        } else {
            view.line_count()
        }
    };
    // Open at the newest (bottom) on the first frame, once the height is known.
    let mut jumped = false;
    loop {
        terminal.draw(|frame| {
            let area = frame.area();
            page_h = area.height.saturating_sub(2).max(1) as usize;
            if !jumped {
                jumped = true;
                view.scroll = line_count(&view).saturating_sub(page_h);
            }
            if heatmap {
                terhub::history::render_heatmap(
                    frame,
                    &view.spans,
                    "timeline heatmap",
                    view.scroll,
                    area,
                    &pal,
                );
            } else {
                terhub::history::render_timeline(frame, &view, "timeline", area, &pal);
            }
        })?;
        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        let max_scroll = line_count(&view).saturating_sub(1);
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                crossterm::event::KeyCode::Char('q') | crossterm::event::KeyCode::Esc => {
                    return Ok(());
                }
                crossterm::event::KeyCode::Up => view.scroll = view.scroll.saturating_sub(1),
                crossterm::event::KeyCode::Down => {
                    view.scroll = (view.scroll + 1).min(max_scroll);
                }
                crossterm::event::KeyCode::PageUp => {
                    view.scroll = view.scroll.saturating_sub(page_h);
                }
                crossterm::event::KeyCode::PageDown => {
                    view.scroll = (view.scroll + page_h).min(max_scroll);
                }
                crossterm::event::KeyCode::Home => view.scroll = 0,
                crossterm::event::KeyCode::End => view.scroll = max_scroll,
                _ => {}
            },
            _ => {}
        }
    }
}

fn cmd_automation(action: &AutomationAction) -> i32 {
    use terhub::automation as a;
    if a::ensure_automation().is_err() {
        eprintln!("terhub: cannot write {}", a::automation_path().display());
        return 1;
    }
    let rules = match a::load_rules() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("terhub: {e:#}");
            return 1;
        }
    };
    match action {
        AutomationAction::List => {
            print_out(&format!("rules: {}", a::automation_path().display()));
            if rules.is_empty() {
                print_out("no rules (edit the file to add some)");
                return 0;
            }
            let fires = terhub::history::read_rule_fires(&terhub::history::ledger_path());
            let now = terhub::agent::now_unix();
            for r in &rules {
                let problems = a::validate(r);
                let status = if !r.enabled {
                    "disabled"
                } else if problems.is_empty() {
                    "armed"
                } else {
                    "invalid"
                };
                let day_fires = fires
                    .iter()
                    .filter(|(name, _, t)| name == &r.name && now.saturating_sub(*t) < 86400)
                    .count();
                print_out(&format!(
                    "{} [{}] on={} action={} delay={}s cooldown={}s cap={}/h fired24h={}",
                    r.name,
                    status,
                    r.on,
                    r.action,
                    r.after_secs,
                    r.cooldown_secs,
                    r.hourly_cap,
                    day_fires,
                ));
                for p in &problems {
                    print_out(&format!("  ! {p}"));
                }
            }
            0
        }
        AutomationAction::Test { name } => {
            let Some(rule) = rules.iter().find(|r| &r.name == name) else {
                eprintln!("terhub: no rule '{name}'");
                return 1;
            };
            let problems = a::validate(rule);
            if !problems.is_empty() {
                for p in &problems {
                    eprintln!("terhub: rule invalid: {p}");
                }
                return 1;
            }
            let snapshot = match daemon::fetch_snapshot(Duration::from_secs(2)) {
                Ok(s) => s,
                Err(_) => {
                    eprintln!("terhub: daemon not running (terhub daemon start)");
                    return 1;
                }
            };
            let reg: std::collections::HashMap<String, terhub::agent::AgentRecord> = snapshot
                .agents
                .into_iter()
                .map(|r| (r.pane.clone(), r))
                .collect();
            let fires = terhub::history::read_rule_fires(&terhub::history::ledger_path());
            let verdicts = a::dry_run(
                std::slice::from_ref(rule),
                &reg,
                &fires,
                terhub::agent::now_unix(),
            );
            if verdicts.is_empty() {
                print_out("no matches right now");
                return 0;
            }
            for (_, pane, verdict) in &verdicts {
                match verdict {
                    a::Verdict::Fire(f) => {
                        let what = match &f.action {
                            a::Action::SendKeys { text } => format!("send-keys '{text}'"),
                            a::Action::Shell { command } => format!("shell '{command}'"),
                        };
                        print_out(&format!("FIRE {pane}: {what}"));
                    }
                    a::Verdict::WaitDelay(s) => print_out(&format!("WAIT {pane}: delay {s}s left")),
                    a::Verdict::Cooldown(s) => {
                        print_out(&format!("COOLDOWN {pane}: {s}s left"));
                    }
                    a::Verdict::Capped => print_out(&format!("CAPPED {pane}: hourly cap reached")),
                }
            }
            0
        }
    }
}

fn cmd_send(pane: &str, text: &str, submit: bool) -> i32 {
    if terhub::native::is_native_id(pane) {
        return match terhub::native::send_text(pane, text, submit) {
            Ok(()) => {
                println!("sent {} chars to {pane}", text.chars().count());
                0
            }
            Err(e) => {
                eprintln!("terhub: {e:#}");
                1
            }
        };
    }
    let tmux = match Tmux::new() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("terhub: {e:?}");
            return 1;
        }
    };
    match tmux.send_keys(pane, text, submit) {
        Ok(()) => {
            println!("sent {} chars to {pane}", text.chars().count());
            0
        }
        Err(e) => {
            eprintln!("terhub: {e:?}");
            1
        }
    }
}

fn cmd_host(action: &HostAction) -> i32 {
    use terhub::fleet as f;
    match action {
        HostAction::Add { name, target, mode } => {
            let Some(mode) = f::Mode::parse(mode) else {
                eprintln!("terhub: bad mode (observe|control)");
                return 1;
            };
            match f::add_host(name, target, mode) {
                Ok(()) => {
                    println!("added {name} ({target}, {})", mode.label());
                    0
                }
                Err(e) => {
                    eprintln!("terhub: {e:?}");
                    1
                }
            }
        }
        HostAction::List => {
            let hosts = match f::load_hosts() {
                Ok(h) => h,
                Err(e) => {
                    eprintln!("terhub: {e:#}");
                    return 1;
                }
            };
            if hosts.is_empty() {
                print_out("no hosts (terhub host add <name> <target>)");
                return 0;
            }
            for h in &hosts {
                print_out(&format!("{} {} {}", h.name, h.target, h.mode.label()));
            }
            0
        }
        HostAction::Remove { name } => match f::remove_host(name) {
            Ok(true) => {
                println!("removed {name}");
                0
            }
            Ok(false) => {
                eprintln!("terhub: no host '{name}'");
                1
            }
            Err(e) => {
                eprintln!("terhub: {e:?}");
                1
            }
        },
        HostAction::Doctor { name } => {
            let hosts = match f::load_hosts() {
                Ok(h) => h,
                Err(e) => {
                    eprintln!("terhub: {e:#}");
                    return 1;
                }
            };
            let Some(host) = hosts.iter().find(|h| &h.name == name) else {
                eprintln!("terhub: no host '{name}'");
                return 1;
            };
            let mut ok_all = true;
            for check in f::doctor(&host.target) {
                ok_all &= check.ok;
                print_out(&format!(
                    "{} {} {}",
                    if check.ok { "ok  " } else { "FAIL" },
                    check.name,
                    check.detail,
                ));
            }
            i32::from(!ok_all)
        }
    }
}

fn cmd_fleet(action: &FleetAction) -> i32 {
    use terhub::fleet as f;
    let hosts = match f::load_hosts() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("terhub: {e:#}");
            return 1;
        }
    };
    match action {
        FleetAction::Status { json } => {
            if hosts.is_empty() {
                if *json {
                    print_out(r#"{"hosts":[]}"#);
                } else {
                    print_out("no hosts (terhub host add <name> <target>)");
                }
                return 0;
            }
            let statuses = f::fleet_status(&hosts);
            if *json {
                let arr: Vec<serde_json::Value> = statuses
                    .iter()
                    .map(|s| match &s.result {
                        Ok(snap) => serde_json::json!({
                            "name": s.host.name, "ok": true,
                            "agents": snap.agents,
                        }),
                        Err(e) => serde_json::json!({
                            "name": s.host.name, "ok": false, "error": e,
                        }),
                    })
                    .collect();
                let payload = serde_json::to_string(&serde_json::json!({"hosts": arr}))
                    .unwrap_or_else(|_| r#"{"hosts":[]}"#.to_string());
                print_out(&payload);
                return 0;
            }
            for s in &statuses {
                match &s.result {
                    Ok(snap) => {
                        let needy = snap
                            .agents
                            .iter()
                            .filter(|a| a.state.needs_attention())
                            .count();
                        print_out(&format!(
                            "== {} ({}): {} agents, {} need you",
                            s.host.name,
                            s.host.target,
                            snap.agents.len(),
                            needy,
                        ));
                        for a in terhub::attend::needy_first(snap.agents.clone()) {
                            print_out(&format!(
                                "  {} {} {} {} {}",
                                a.pane,
                                a.agent,
                                a.state.label(),
                                a.session,
                                a.summary,
                            ));
                        }
                    }
                    Err(e) => print_out(&format!("== {}: unreachable ({e})", s.host.name)),
                }
            }
            0
        }
        FleetAction::Send {
            host,
            pane,
            text,
            no_submit,
        } => {
            let Some(h) = hosts.iter().find(|h| &h.name == host) else {
                eprintln!("terhub: no host '{host}'");
                return 1;
            };
            match f::fleet_send(h, pane, text, !no_submit) {
                Ok(out) => {
                    print_out(out.trim());
                    0
                }
                Err(e) => {
                    eprintln!("terhub: {e:?}");
                    1
                }
            }
        }
        FleetAction::Attach { host, session } => cmd_fleet_attach(host, session.as_deref()),
    }
}

/// SSH into a fleet host's tmux (TUI remote jump + `fleet attach`).
/// Execs ssh, so a failure past the tty check can't clean up — the
/// process image is already gone.
fn cmd_fleet_attach(host: &str, session: Option<&str>) -> i32 {
    let target = match terhub::fleet::load_hosts() {
        Ok(hosts) => hosts.into_iter().find(|h| h.name == host).map(|h| h.target),
        Err(e) => {
            eprintln!("terhub: {e:#}");
            return 1;
        }
    };
    let Some(target) = target else {
        eprintln!("terhub: no host '{host}'");
        return 1;
    };
    match terhub::fleet::attach_remote(&target, session) {
        Ok(()) => 0, // unreachable: attach execs
        Err(e) => {
            eprintln!("terhub: {e:?}");
            1
        }
    }
}

fn cmd_native(action: &NativeAction) -> i32 {
    use terhub::native;
    match action {
        NativeAction::List { json } => {
            let mut tabs = Vec::new();
            for rep in native::list_all() {
                if let Some(e) = rep.error {
                    eprintln!("terhub: {}: {e}", rep.backend);
                }
                tabs.extend(rep.tabs);
            }
            if *json {
                match serde_json::to_string(&tabs) {
                    Ok(s) => print_out(&s),
                    Err(e) => {
                        eprintln!("terhub: {e:?}");
                        return 1;
                    }
                }
                return 0;
            }
            if tabs.is_empty() {
                print_out("no native tabs");
                return 0;
            }
            for t in &tabs {
                print_out(&format!(
                    "{:<22} {:<12} {}",
                    t.id,
                    t.window,
                    if t.title.is_empty() {
                        "(untitled)"
                    } else {
                        &t.title
                    }
                ));
            }
            0
        }
        NativeAction::Spawn { backend } => match native::spawn(backend) {
            Ok(()) => {
                print_out(&format!("opened a {backend} tab"));
                0
            }
            Err(e) => {
                eprintln!("terhub: {e:#}");
                1
            }
        },
    }
}

fn cmd_zellij(action: &ZellijAction) -> i32 {
    use terhub::zellij as z;
    match action {
        ZellijAction::List => match z::list_sessions() {
            Ok(sessions) => {
                if sessions.is_empty() {
                    print_out("no sessions");
                    return 0;
                }
                for s in &sessions {
                    print_out(&s.name);
                }
                0
            }
            Err(e) => {
                eprintln!("terhub: {e:?}");
                1
            }
        },
        ZellijAction::Attach { name } => match z::attach_here(name) {
            Ok(()) => 0, // unreachable: exec replaces the process
            Err(e) => {
                eprintln!("terhub: {e:?}");
                1
            }
        },
    }
}

fn cmd_kill(name: &str) -> i32 {
    let tmux = match Tmux::new() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("terhub: {e:?}");
            return 1;
        }
    };
    match tmux.kill_session(name) {
        Ok(()) => {
            println!("killed {name}");
            0
        }
        Err(e) => {
            eprintln!("terhub: {e:?}");
            1
        }
    }
}

/// One-shot setup: config file, hooks for all agents, daemon start.
/// Steps share the subcommand implementations; the summary names failures.
fn cmd_init(dry_run: bool) -> i32 {
    if dry_run {
        print_out("init would run, in order:");
        print_out("  1) write the default config file");
        print_out("  2) install agent hooks (all agents)");
        print_out("  3) start the terhubd daemon");
        return 0;
    }
    let mut failed: Vec<&str> = Vec::new();
    print_out("== config ==");
    match config::ensure_config() {
        Ok(p) => print_out(&format!("config ready ({})", p.display())),
        Err(e) => {
            eprintln!("terhub: config failed: {e:?}");
            failed.push("config");
        }
    }
    print_out("== hooks ==");
    if cmd_hooks(&HooksAction::Install {
        agent: "all".to_string(),
    }) != 0
    {
        failed.push("hooks");
    }
    print_out("== daemon ==");
    if cmd_daemon(&DaemonAction::Start) != 0 {
        failed.push("daemon");
    }
    if failed.is_empty() {
        print_out("init complete: config, hooks, daemon");
        0
    } else {
        eprintln!("terhub: init failed: {}", failed.join(", "));
        1
    }
}

fn cmd_config(show: bool, path: bool, init: bool, check: bool) -> i32 {
    if check {
        let (_, problems) = config::load_config_report(None);
        if problems.is_empty() {
            println!("config ok ({})", config::config_path().display());
            return 0;
        }
        for p in &problems {
            println!("{p}");
        }
        return 1;
    }
    if init {
        match config::ensure_config() {
            Ok(p) => {
                println!("{}", p.display());
                return 0;
            }
            Err(e) => {
                eprintln!("terhub: {e:?}");
                return 1;
            }
        }
    }
    if path {
        println!("{}", config::config_path().display());
        return 0;
    }
    if show {
        let p = config::config_path();
        match std::fs::read_to_string(&p) {
            Ok(t) => print_raw(&t),
            Err(_) => print_raw(&config::default_config_text()),
        }
        return 0;
    }
    match config::ensure_config() {
        Ok(p) => {
            println!("{}", p.display());
            0
        }
        Err(e) => {
            eprintln!("terhub: {e:?}");
            1
        }
    }
}

/// Restores the terminal on drop, including during unwind.
struct TerminalGuard;

impl TerminalGuard {
    fn new() -> Result<Self> {
        terminal::enable_raw_mode()?;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            event::EnableMouseCapture
        )?;
        Ok(TerminalGuard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(
            io::stdout(),
            event::DisableMouseCapture,
            LeaveAlternateScreen
        );
        let _ = terminal::disable_raw_mode();
    }
}

#[derive(Debug)]
enum TuiExit {
    Attach(String),
    FocusPane(String),
    RemoteAttach { host: String, session: String },
}

fn in_area(col: u16, row: u16, area: ratatui::layout::Rect) -> bool {
    col >= area.x && col < area.x + area.width && row >= area.y && row < area.y + area.height
}

fn run_tui() -> Result<Option<TuiExit>> {
    config::ensure_config()?;
    let (cfg, problems) = config::load_config_report(None);
    let tmux = Tmux::new()?;
    let mut app = App::new(cfg);
    app.own_pane = std::env::var("TMUX_PANE").ok();
    if let Some(first) = problems.first() {
        app.status = Some(format!("config: {first}"));
    }
    app.refresh(&tmux);
    app.fetch_native();

    let _guard = TerminalGuard::new()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut areas = ClickAreas::default();
    let mut last_tick = Instant::now();
    // Left-press position for click-vs-drag: a jump only fires when press
    // and release land on the same cell, so text drags never jump.
    let mut press: Option<(u16, u16, Instant)> = None;
    // Inside tmux a jump only moves the client's view, so the table stays
    // open behind it; outside tmux the attach must take over this terminal.
    let stay_open = std::env::var("TMUX").is_ok();

    loop {
        terminal.draw(|frame| {
            areas = terhub::ui::render(frame, &mut app);
        })?;
        if last_tick.elapsed() >= Duration::from_secs(3) {
            app.refresh(&tmux);
            last_tick = Instant::now();
        }
        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if let Some(ev) = app.on_key(key, &tmux) {
                    match ev {
                        AppEvent::Quit => return Ok(None),
                        // Remote jumps always exit: ssh owns the terminal.
                        AppEvent::RemoteAttach { host, session } => {
                            return Ok(Some(TuiExit::RemoteAttach { host, session }));
                        }
                        // Native jumps only switch a terminal tab: always
                        // stay open, in both modes.
                        AppEvent::FocusNative(id) => {
                            app.do_nav(&tmux, AppEvent::FocusNative(id));
                        }
                        ev => {
                            if stay_open {
                                app.do_nav(&tmux, ev);
                            } else {
                                return Ok(Some(match ev {
                                    AppEvent::Attach(n) => TuiExit::Attach(n),
                                    AppEvent::FocusPane(i) => TuiExit::FocusPane(i),
                                    AppEvent::RemoteAttach { .. } => unreachable!("matched above"),
                                    AppEvent::FocusNative(_) => unreachable!("matched above"),
                                    AppEvent::Quit => unreachable!("matched above"),
                                }));
                            }
                        }
                    }
                }
            }
            Event::Mouse(mouse) => {
                // Settings rows are clickable; other prompts eat all clicks.
                if app.mode == terhub::app::Mode::Settings {
                    if mouse.kind == MouseEventKind::Down(MouseButton::Left)
                        && in_area(mouse.column, mouse.row, areas.settings)
                    {
                        let row = app.settings_scroll + (mouse.row - areas.settings.y) as usize;
                        app.settings_apply_at_row(row);
                    }
                    continue;
                }
                // Prompts own the screen: clicks behind a modal do nothing.
                if app.mode != terhub::app::Mode::Normal {
                    continue;
                }
                match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        press = Some((mouse.column, mouse.row, Instant::now()));
                    }
                    MouseEventKind::Up(MouseButton::Left) => {
                        let click = matches!(press,
                            Some((c, r, t))
                            if c == mouse.column && r == mouse.row
                                && t.elapsed() < Duration::from_millis(750));
                        press = None;
                        if !click {
                            continue; // a drag, not a click
                        }
                        if let Some(id) = app.preview_jump_target() {
                            // Fullscreen preview: jump to the previewed pane.
                            // Preview owns the click: never fall through
                            // to header/session/pane handling below.
                            match app.jump_event(&id) {
                                Some(AppEvent::RemoteAttach { host, session }) => {
                                    return Ok(Some(TuiExit::RemoteAttach { host, session }));
                                }
                                Some(AppEvent::FocusPane(p)) => {
                                    if stay_open {
                                        app.do_nav(&tmux, AppEvent::FocusPane(p));
                                    } else {
                                        return Ok(Some(TuiExit::FocusPane(p)));
                                    }
                                }
                                Some(AppEvent::FocusNative(q)) => {
                                    app.do_nav(&tmux, AppEvent::FocusNative(q));
                                }
                                // Stale remote row (status set) or events
                                // jump_event never emits: swallow, stay open.
                                _ => {}
                            }
                            continue;
                        }
                        if let Some(hit) = areas
                            .headers
                            .iter()
                            .find(|h| in_area(mouse.column, mouse.row, h.area))
                        {
                            if let Some(key) = hit.key {
                                app.resort(key);
                                app.follow_preview(&tmux);
                            }
                            continue; // header clicks sort, never jump
                        }
                        if in_area(mouse.column, mouse.row, areas.sessions) {
                            let hit = areas
                                .segments
                                .iter()
                                .position(|r| in_area(mouse.column, mouse.row, *r));
                            if let Some(pos) = hit {
                                // Segments are windowed: offset by scroll.
                                let idx = app.session_scroll + pos;
                                let ctrl = mouse
                                    .modifiers
                                    .intersects(crossterm::event::KeyModifiers::CONTROL);
                                if let Some(AppEvent::Attach(n)) =
                                    app.click_session_segment(idx, ctrl, &tmux)
                                {
                                    if stay_open {
                                        app.do_nav(&tmux, AppEvent::Attach(n));
                                    } else {
                                        return Ok(Some(TuiExit::Attach(n)));
                                    }
                                }
                            }
                        } else if in_area(mouse.column, mouse.row, areas.panes) {
                            let idx = app.pane_scroll + (mouse.row - areas.panes.y) as usize;
                            if idx < app.filtered.len()
                                && let Some(p) = app.filtered.get(idx).map(|p| p.id.clone())
                            {
                                // Left-click jumps straight to the pane.
                                match app.jump_event(&p) {
                                    Some(AppEvent::RemoteAttach { host, session }) => {
                                        return Ok(Some(TuiExit::RemoteAttach { host, session }));
                                    }
                                    Some(AppEvent::FocusPane(q)) => {
                                        if stay_open {
                                            app.do_nav(&tmux, AppEvent::FocusPane(q));
                                        } else {
                                            return Ok(Some(TuiExit::FocusPane(q)));
                                        }
                                    }
                                    Some(AppEvent::FocusNative(q)) => {
                                        app.do_nav(&tmux, AppEvent::FocusNative(q));
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                    MouseEventKind::Down(MouseButton::Right) => {
                        press = None;
                        let idx = if in_area(mouse.column, mouse.row, areas.panes) {
                            let idx = app.pane_scroll + (mouse.row - areas.panes.y) as usize;
                            (idx < app.filtered.len()).then_some(idx)
                        } else {
                            None
                        };
                        app.right_click(&tmux, idx);
                    }
                    MouseEventKind::Moved => {
                        app.hover = if in_area(mouse.column, mouse.row, areas.panes) {
                            let idx = app.pane_scroll + (mouse.row - areas.panes.y) as usize;
                            (idx < app.filtered.len()).then_some(idx)
                        } else {
                            None
                        };
                    }
                    MouseEventKind::ScrollUp => {
                        app.on_wheel(true);
                    }
                    MouseEventKind::ScrollDown => {
                        app.on_wheel(false);
                    }
                    // Drags, releases, horizontal scrolls: no surface for them.
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

fn serve_blocking(port: u16) -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_io()
        .enable_time()
        .build()?
        .block_on(terhub::web::serve(port))
}

fn real_main() -> i32 {
    let cli = Cli::parse();
    match cli.cmd {
        None => match run_tui() {
            Ok(Some(TuiExit::Attach(name))) => cmd_attach(&name),
            Ok(Some(TuiExit::FocusPane(id))) => cmd_focus(&id),
            Ok(Some(TuiExit::RemoteAttach { host, session })) => {
                cmd_fleet_attach(&host, Some(&session))
            }
            Ok(None) => 0,
            Err(e) => {
                eprintln!("terhub: {e:?}");
                1
            }
        },
        Some(Cmd::List { plain }) => cmd_list(plain),
        Some(Cmd::New { name, dir, window }) => cmd_new(&name, dir.as_deref(), window.as_deref()),
        Some(Cmd::Attach { name }) => cmd_attach(&name),
        Some(Cmd::Kill { name }) => cmd_kill(&name),
        Some(Cmd::Panes { session }) => cmd_panes(&session),
        Some(Cmd::Capture { pane, lines }) => cmd_capture(&pane, lines),
        Some(Cmd::Focus { pane }) => cmd_focus(&pane),
        Some(Cmd::KillPane { pane }) => cmd_kill_pane(&pane),
        Some(Cmd::SwapPane { a, b }) => cmd_swap_pane(&a, &b),
        Some(Cmd::Status { json }) => cmd_status(json),
        Some(Cmd::Hook {
            agent,
            pane,
            state,
            summary,
            summary_stdin,
            prompt,
            prompt_stdin,
        }) => cmd_hook(
            &agent,
            &pane,
            &state,
            summary.as_deref(),
            summary_stdin,
            prompt.as_deref(),
            prompt_stdin,
        ),
        Some(Cmd::Daemon { action }) => cmd_daemon(&action),
        Some(Cmd::Hooks { action }) => cmd_hooks(&action),
        Some(Cmd::Attend { cycle, list }) => cmd_attend(cycle, list),
        Some(Cmd::Peek { plain, pick }) => cmd_peek(plain, pick),
        Some(Cmd::StatusLine { pane }) => cmd_status_line(pane.as_deref()),
        Some(Cmd::Stats { since, group_by }) => cmd_stats(&since, group_by),
        Some(Cmd::Report {
            since,
            json,
            markdown,
        }) => cmd_report(&since, json, markdown),
        Some(Cmd::Activity { since, kind, json }) => cmd_activity(&since, &kind, json),
        Some(Cmd::Recap { since, pane, limit }) => cmd_recap(&since, pane.as_deref(), limit),
        Some(Cmd::Timeline {
            since,
            session,
            agent,
            sort,
            view,
        }) => cmd_timeline(&since, session.as_deref(), agent.as_deref(), &sort, &view),
        Some(Cmd::Automation { action }) => cmd_automation(&action),
        Some(Cmd::Send {
            pane,
            text,
            no_submit,
        }) => cmd_send(&pane, &text, !no_submit),
        Some(Cmd::Host { action }) => cmd_host(&action),
        Some(Cmd::Fleet { action }) => cmd_fleet(&action),
        Some(Cmd::Zellij { action }) => cmd_zellij(&action),
        Some(Cmd::Native { action }) => cmd_native(&action),
        Some(Cmd::Mcp) => match terhub::mcp::run() {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("terhub: {e:?}");
                1
            }
        },
        Some(Cmd::Serve { port }) => match serve_blocking(port) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("terhub: {e:?}");
                1
            }
        },
        Some(Cmd::Config {
            show,
            path,
            init,
            check,
        }) => cmd_config(show, path, init, check),
        Some(Cmd::Init { dry_run }) => cmd_init(dry_run),
    }
}

fn main() {
    std::process::exit(real_main());
}
