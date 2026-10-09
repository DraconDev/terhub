//! TUI state machine. Backend-agnostic rendering input; knows nothing about
//! ratatui so the logic stays unit-testable.
use crate::agent::AgentRecord;
use crate::config::{AppConfig, key_matches};
use crate::daemon;
use crate::tmux::{Pane, Session, Tmux};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::HashMap;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Normal,
    PromptNew,
    PromptRename(String),
    PromptFilter,
    PromptSpawn {
        profile: usize,
    },
    PromptBulk {
        targets: Vec<String>,
        template: usize,
    },
    ConfirmKill(String),
    ConfirmKillPane(String),
    ConfirmRestartPane {
        id: String,
        command: String,
    },
    Hosts,
    PromptHostAdd,
    ConfirmKillMarked(Vec<String>),
    CommandOutput {
        name: String,
        lines: Vec<String>,
        scroll: usize,
    },
    Settings,
}

/// One settings-popup row: a section header or an actionable item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingRow {
    Header(&'static str),
    Item {
        label: String,
        /// Currently in effect (gets the ● marker).
        active: bool,
        kind: SettingKind,
    },
    /// Read-only legend row: the coloured tray-icon glyph, its name,
    /// and what that state means. Not actionable; shown for reference
    /// so the user can map the panel triangle to a real status.
    Legend {
        triangle: &'static str,
        color: &'static str,
        name: &'static str,
        meaning: &'static str,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingKind {
    Theme(usize),
    Density(usize),
    ToggleNotifications,
    ToggleHighlight,
    ToggleMruTabs,
    /// Opens the text prompt editing the standing exclusion; Enter
    /// saves it to `[ui] filter`, Esc cancels. Not a toggle.
    EditStandingFilter,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppEvent {
    Attach(String),
    FocusPane(String),
    /// Jump to a remote session: exits the TUI into `ssh -t` (ssh owns
    /// the terminal from there, so this never stays open).
    RemoteAttach {
        host: String,
        session: String,
    },
    /// Switch a native terminal to one of its tabs (stays open: only the
    /// terminal's view moves).
    FocusNative(String),
    Quit,
}

/// Remote rows carry `host:%N` ids; local tmux ids always start with `%`.
/// Native terminal rows (`konsole:…`) are remote-shaped but local — every
/// guard below checks [`is_native_pane`] first.
pub fn is_remote_pane(id: &str) -> bool {
    !id.starts_with('%')
}

/// True for native terminal rows (Konsole/kitty/WezTerm tabs).
pub fn is_native_pane(id: &str) -> bool {
    crate::native::is_native_id(id)
}

/// Backend fetch failures stay one status line: first line, capped.
fn short_native_error(e: &str) -> String {
    let first = e.lines().next().unwrap_or(e).trim();
    if first.chars().count() > 90 {
        format!("{}…", first.chars().take(87).collect::<String>())
    } else {
        first.to_string()
    }
}

/// CPU percent between two tick samples (None = re-baseline: first
/// sample, zero elapsed, or ticks fell because the process restarted).
/// Over 100% is real — agent subtrees span cores.
fn cpu_pct(prev_ticks: u64, prev_at: Instant, ticks: u64, at: Instant, hz: f64) -> Option<f64> {
    let dt = at.duration_since(prev_at).as_secs_f64();
    if dt <= 0.0 || ticks < prev_ticks || hz <= 0.0 {
        return None;
    }
    Some((ticks - prev_ticks) as f64 / dt / hz * 100.0)
}

/// Working-tree summary for the preview title.
struct GitStat {
    branch: String,
    files: u32,
    ins: u32,
    del: u32,
    untracked: u32,
}

impl GitStat {
    fn render(&self) -> String {
        let mut s = format!("git {}", self.branch);
        if self.files == 0 && self.untracked == 0 {
            return s + " · clean";
        }
        s.push_str(&format!(" +{}/-{}", self.ins, self.del));
        if self.files > 0 {
            s.push_str(&format!(" · {} changed", self.files));
        }
        if self.untracked > 0 {
            s.push_str(&format!(" · {} new", self.untracked));
        }
        s
    }
}

/// Branch + diff + untracked count for a dir (None outside a repo).
/// `--no-optional-locks` keeps read-only probes from ever blocking on
/// a lock a running agent holds.
fn git_diff_stat(dir: &str) -> Option<GitStat> {
    let branch = git_out(dir, &["branch", "--show-current"])?;
    let branch = branch.trim();
    let branch = if branch.is_empty() {
        "detached".to_string()
    } else {
        branch.to_string()
    };
    let short = git_out(dir, &["diff", "--shortstat"])?;
    let (files, ins, del) = parse_shortstat(&short);
    let status = git_out(
        dir,
        &["status", "--porcelain=v1", "--untracked-files=normal"],
    )?;
    let untracked = status.lines().filter(|l| l.starts_with("??")).count() as u32;
    Some(GitStat {
        branch,
        files,
        ins,
        del,
        untracked,
    })
}

fn git_out(dir: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .arg("--no-optional-locks")
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Parse `git diff --shortstat`: any part may be absent when zero, and
/// empty output means a clean tree.
fn parse_shortstat(out: &str) -> (u32, u32, u32) {
    let mut files = 0;
    let mut ins = 0;
    let mut del = 0;
    for part in out.split(',') {
        let num: u32 = part
            .split_whitespace()
            .next()
            .and_then(|n| n.parse().ok())
            .unwrap_or(0);
        if part.contains("file") {
            files = num;
        } else if part.contains("insertion") {
            ins = num;
        } else if part.contains("deletion") {
            del = num;
        }
    }
    (files, ins, del)
}

/// Clock ticks per second for CPU math (Linux almost always 100).
fn clock_hz() -> f64 {
    // SAFETY: sysconf with a valid name has no memory effects.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if hz > 0 { hz as f64 } else { 100.0 }
}

/// Which list keyboard input drives: the session sidebar or the pane table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    Sessions,
    #[default]
    Panes,
}

/// Lines of scrollback captured for the fullscreen preview. The reader
/// shows a window into this buffer and scrolls through it.
pub const PREVIEW_SCROLLBACK: u32 = 500;

/// Table column the pane rows sort by. Every data header is clickable;
/// the mark column is not sortable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Pane,
    Session,
    Window,
    Cmd,
    Dir,
    Agent,
    Handle,
    State,
    Src,
    For,
    Turn,
    Idle,
    Summary,
}

impl SortKey {
    /// Direction a fresh click (or the `o` key) starts with: attention and
    /// recency put the most urgent first, text reads A→Z.
    fn default_asc(self) -> bool {
        !matches!(self, SortKey::For | SortKey::Turn | SortKey::Idle)
    }

    /// Config-file name for this key (`Idle`, `Pane`, …).
    pub fn as_str(self) -> &'static str {
        match self {
            SortKey::Pane => "Pane",
            SortKey::Session => "Session",
            SortKey::Window => "Window",
            SortKey::Cmd => "Cmd",
            SortKey::Dir => "Dir",
            SortKey::Agent => "Agent",
            SortKey::Handle => "Handle",
            SortKey::State => "State",
            SortKey::Src => "Src",
            SortKey::For => "For",
            SortKey::Turn => "Turn",
            SortKey::Idle => "Idle",
            SortKey::Summary => "Summary",
        }
    }

    /// Parse a config-file name (case-insensitive); `None` means unknown.
    pub fn parse_name(s: &str) -> Option<SortKey> {
        Some(match s.to_lowercase().as_str() {
            "pane" => SortKey::Pane,
            "session" => SortKey::Session,
            "window" => SortKey::Window,
            "cmd" => SortKey::Cmd,
            "dir" => SortKey::Dir,
            "agent" => SortKey::Agent,
            "handle" => SortKey::Handle,
            "state" => SortKey::State,
            "src" => SortKey::Src,
            "for" => SortKey::For,
            "turn" => SortKey::Turn,
            "idle" => SortKey::Idle,
            "summary" => SortKey::Summary,
            _ => return None,
        })
    }
}

/// `o` key cycle order: most useful sorts first.
const SORT_CYCLE: &[SortKey] = &[
    SortKey::Idle,
    SortKey::State,
    SortKey::For,
    SortKey::Turn,
    SortKey::Agent,
    SortKey::Handle,
    SortKey::Session,
    SortKey::Window,
    SortKey::Cmd,
    SortKey::Dir,
    SortKey::Src,
    SortKey::Summary,
    SortKey::Pane,
];

/// Numeric pane id for ordering (%2 before %10); unparseable ids sink.
fn pane_num(id: &str) -> u32 {
    id.strip_prefix('%')
        .and_then(|n| n.parse().ok())
        .unwrap_or(u32::MAX)
}

/// Last path segment, mirroring the Dir cell (blank sinks last).
fn dir_name(path: &str) -> &str {
    let base = path.rsplit('/').next().unwrap_or("");
    if base.is_empty() { "—" } else { base }
}

/// Blank/unknown text sinks last in ascending order.
fn agent_name(rec: Option<&AgentRecord>) -> &str {
    rec.map(|r| r.agent.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("—")
}

fn handle_name(rec: Option<&AgentRecord>) -> &str {
    rec.map(|r| r.handle.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("—")
}

fn summary_text(rec: Option<&AgentRecord>) -> &str {
    rec.map(|r| r.summary.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("—")
}

/// Attention first, unknowns last.
fn state_rank(rec: Option<&AgentRecord>) -> u8 {
    match rec.map(|r| r.state) {
        Some(st) => state_rank_of(st),
        None => 7,
    }
}

/// Attention order shared by sort and tab dots: the lower, the needier.
fn state_rank_of(state: crate::agent::AgentState) -> u8 {
    use crate::agent::AgentState;
    match state {
        AgentState::Waiting => 0,
        AgentState::Stuck => 1,
        AgentState::Error => 2,
        AgentState::Limited => 3,
        AgentState::Working => 4,
        AgentState::Idle => 5,
        AgentState::Unknown => 6,
    }
}

fn src_name(rec: Option<&AgentRecord>) -> &'static str {
    match rec {
        Some(r) if r.source == crate::agent::Source::Hook => "hook",
        Some(r) if r.source == crate::agent::Source::Proc => "proc",
        Some(r) if r.source == crate::agent::Source::Manual => "man",
        Some(_) => "scr",
        None => "—",
    }
}

/// Idle timestamp the column displays: the visible-quiet clock when the
/// daemon knows it, else window activity (0 = unknown, sorts stalest).
fn idle_secs(rec: Option<&AgentRecord>, activity: u64) -> u64 {
    match rec {
        Some(r) if r.quiet_since > 0 => r.quiet_since,
        _ => activity,
    }
}

/// Quiet threshold for the `h` toggle and the unclaimed-quiet "idle"
/// label: unclaimed panes idle this long are considered dormant. The
/// `h` toggle hides them; the table otherwise renders their state as
/// `idle` so the user can see dormancy without the daemon.
pub(crate) const QUIET_SHELL_SECS: u64 = 600;

/// True for rows the `h` toggle hides: unclaimed panes quiet past
/// [`QUIET_SHELL_SECS`]. Unknown activity (0) stays visible — the toggle
/// hides judged-quiet rows, never unknowns.
pub(crate) fn is_quiet_shell(rec: Option<&AgentRecord>, activity: u64, now: u64) -> bool {
    rec.is_none() && activity > 0 && now.saturating_sub(activity) >= QUIET_SHELL_SECS
}

fn age_secs(rec: Option<&AgentRecord>, now: u64) -> u64 {
    rec.map(|r| r.waiting_secs(now)).unwrap_or(0)
}

/// Last completed turn; 0 (none yet) sinks in the default descending order.
fn turn_secs(rec: Option<&AgentRecord>) -> u64 {
    rec.map(|r| r.last_turn_secs).unwrap_or(0)
}

/// Expand a custom command template against the selected row. Unknown
/// `{var}`s stay literal (config validation rejects them at load; this is
/// the belt-and-braces path for programmatically built configs).
/// Window name for a spawn: `{profile}-{first words of prompt}`,
/// sanitized to tmux-safe chars and capped so tabs stay readable.
/// Empty prompt degrades to the bare profile name.
pub fn spawn_window_name(profile: &str, prompt: &str) -> String {
    let clean = |s: &str| {
        s.chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect::<String>()
    };
    let prof = clean(profile);
    let prof = if prof.is_empty() {
        "agent".to_string()
    } else {
        prof
    };
    let slug: String = prompt
        .split_whitespace()
        .take(3)
        .collect::<Vec<_>>()
        .join("-")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(20)
        .collect();
    if slug.is_empty() {
        prof.chars().take(32).collect()
    } else {
        format!("{prof}-{slug}").chars().take(32).collect()
    }
}

pub fn expand_command(
    template: &str,
    pane: &crate::tmux::Pane,
    rec: Option<&AgentRecord>,
) -> String {
    let agent = rec.map(|r| r.agent.as_str()).unwrap_or("");
    let state = rec.map(|r| r.state.label()).unwrap_or("");
    let value = |var: &str| -> Option<&str> {
        match var {
            "pane" => Some(pane.id.as_str()),
            "session" => Some(pane.session.as_str()),
            "window" => Some(pane.window_name.as_str()),
            "dir" => Some(pane.path.as_str()),
            "cmd" => Some(pane.command.as_str()),
            "agent" => Some(agent),
            "state" => Some(state),
            _ => None,
        }
    };
    let chars: Vec<char> = template.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '}' && chars.get(i + 1) == Some(&'}') {
            out.push('}');
            i += 2;
            continue;
        }
        if chars[i] == '{' {
            if chars.get(i + 1) == Some(&'{') {
                out.push('{');
                i += 2;
                continue;
            }
            if let Some(end) = chars[i..].iter().position(|c| *c == '}') {
                let var: String = chars[i + 1..i + end].iter().collect();
                match value(&var) {
                    Some(v) => out.push_str(v),
                    None => {
                        out.push('{');
                        out.push_str(&var);
                        out.push('}');
                    }
                }
                i += end + 1;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Normalize a standing-filter query to exclusion form: trim, drop any
/// leading `!`s the user typed, then re-add exactly one — unless empty.
/// The standing filter always excludes (`zsh` means `!zsh`); the
/// temporary search keeps whatever was typed, `!` or not.
fn normalize_exclude(q: &str) -> String {
    let rest = q.trim().trim_start_matches('!').trim();
    if rest.is_empty() {
        String::new()
    } else {
        format!("!{rest}")
    }
}

/// Case-insensitive substring match across every text column. `q` must
/// already be lowercased. A leading `!` negates: `!zsh` keeps every row
/// whose columns do NOT contain `zsh` (a lone `!` matches everything).
fn pane_matches(p: &Pane, rec: Option<&AgentRecord>, q: &str) -> bool {
    let (neg, q) = match q.strip_prefix('!') {
        Some(rest) => (true, rest),
        None => (false, q),
    };
    if neg && q.is_empty() {
        return true;
    }
    let hit = |s: &str| s.to_lowercase().contains(q);
    let m = hit(&p.id)
        || hit(&p.session)
        || hit(&p.window_name)
        || p.index.to_string().contains(q)
        || hit(&p.command)
        || hit(&p.path)
        || rec.is_some_and(|r| hit(&r.agent) || hit(r.state.label()) || hit(&r.summary));
    if neg { !m } else { m }
}

/// Table legend, shown when `?` is pressed twice: the marks and column
/// shorthands the table itself never explains.
const LEGEND: &str = "legend: !=needs you · +=marked · *=focused pane · ●/○=attached · Src hook/proc/scr · For state age · Turn last turn · Idle quiet time";

pub struct App {
    pub config: AppConfig,
    pub sessions: Vec<Session>,
    /// Sidebar row: 0 = "All sessions", 1+i = sessions[i].
    pub selected: usize,
    pub mode: Mode,
    pub input: String,
    pub status: Option<String>,
    /// When `status` was set via [`App::set_status`] (`None` = sticky direct
    /// write, e.g. startup errors — never expires).
    status_since: Option<std::time::Instant>,
    pub focus: Focus,
    /// Every pane on the server, sorted by session/window/index.
    pub panes: Vec<Pane>,
    /// Panes visible under the sidebar filter (the table rows).
    pub filtered: Vec<Pane>,
    pub pane_selected: usize,
    pub preview: String,
    /// Width of the previewed pane in columns (for EOL background fill).
    pub preview_width: Option<usize>,
    /// Fullscreen preview open (captured on demand, never by default).
    pub show_preview: bool,
    /// Reader offset into the preview buffer (wheel / PgUp / PgDn / Home / End).
    pub preview_scroll: usize,
    /// Preview inner height in lines, recorded by render for page steps.
    pub preview_visible: usize,
    /// Pane id the preview buffer belongs to (follow resets on change).
    preview_for: Option<String>,
    /// Latest daemon snapshot by pane id (empty when the daemon is down).
    pub agents: HashMap<String, AgentRecord>,
    /// Last snapshot fetch succeeded. False reads as "daemon down" in the
    /// title — an empty `agents` alone can't tell down from idle.
    pub agents_live: bool,
    /// First visible row of each list (the UI keeps these in view).
    pub session_scroll: usize,
    pub pane_scroll: usize,
    /// Prompt input cursor, in characters.
    pub cursor: usize,
    /// Settings popup: selected item row and first visible row.
    pub settings_selected: usize,
    pub settings_scroll: usize,
    /// Displayed settings section (sidebar cursor). Follows the
    /// selection on Up/Down; set directly by Left/Right and sidebar
    /// clicks. Sections with no selectable items (the tray-icon
    /// legend) are landable but keep the stored selection.
    pub settings_section: usize,
    /// Table sort: column plus direction (default: recency, newest first).
    pub sort_key: SortKey,
    pub sort_asc: bool,
    /// Temporary find query (case-insensitive substring, empty = off).
    /// Session-only: typing, `/`, or `f` sets it; Enter keeps it for
    /// the session; Esc clears it. Never persisted.
    pub filter: String,
    /// Standing row exclusion, from `[ui] filter` (empty = off). Applies
    /// before the temporary find and survives restarts; edited
    /// deliberately via the settings row, never by typing.
    pub exclude: String,
    /// Which query the filter prompt is editing: the temporary find
    /// (false) or the standing exclusion (true, from the settings row).
    editing_standing: bool,
    /// Query as it was when the filter prompt opened: live typing edits
    /// `filter` in place, and Esc restores this instead of clearing.
    filter_before_edit: String,
    /// Hide unclaimed panes quiet past [`QUIET_SHELL_SECS`] (`h` toggle).
    pub hide_quiet_shells: bool,
    /// Rows the toggle is currently hiding (0 when off), for the title.
    pub hidden_quiet: usize,
    /// Space-marked pane ids for bulk kill/attend. Survives sort/filter;
    /// pruned to live panes on every refresh.
    pub marked: std::collections::HashSet<String>,
    /// Filtered-list index under the mouse cursor, if any. Cleared on
    /// every list rebuild (an index can't survive a reorder).
    pub hover: Option<usize>,
    /// The TUI's own pane id (`$TMUX_PANE`, set once at startup), so its
    /// row reads as the observer instead of just another entry.
    pub own_pane: Option<String>,
    /// Cached remote agent rows (manual fetch; re-merged every refresh).
    pub fleet_rows: Vec<Pane>,
    /// Cached remote agent records, merged into `agents` on refresh.
    pub fleet_agents: Vec<AgentRecord>,
    /// Remote id → (host, remote session) for jumps and previews.
    pub fleet_index: HashMap<String, (String, String)>,
    /// Hosts shown in the Hosts modal (loaded when it opens).
    pub fleet_hosts: Vec<crate::fleet::Host>,
    /// Per-host errors from the last fetch (shown in the modal).
    pub fleet_errors: Vec<(String, String)>,
    /// When the fleet cache was last fetched (None = never).
    pub fleet_fetched_at: Option<std::time::Instant>,
    /// Cached native terminal rows (startup + F5 fetch; re-merged every
    /// refresh like the fleet cache).
    pub native_rows: Vec<Pane>,
    /// Cached native agent records, merged into `agents` on refresh.
    pub native_agents: Vec<AgentRecord>,
    /// Native quiet clocks by tab id: (screen hash, since). The TUI owns
    /// them (the daemon never fetches native tabs); first sighting starts
    /// fresh, like a daemon restart.
    native_quiet: HashMap<String, (u64, u64)>,
    /// Hosts modal cursor.
    pub hosts_selected: usize,
    /// Hosts modal first visible row.
    pub hosts_scroll: usize,
    /// Previous (pane, ticks, at) for preview CPU math.
    preview_cpu_prev: Option<(String, u64, Instant)>,
    /// Last computed preview CPU percent (None until two samples).
    pub preview_cpu_pct: Option<f64>,
    /// Git summary cache for the previewed dir: (dir, rendered, at).
    preview_git: Option<(String, Option<String>, Instant)>,
    /// Rendered git line for the preview title (None = no repo here).
    pub preview_git_line: Option<String>,
}

impl App {
    pub fn new(config: AppConfig) -> Self {
        let exclude = config.filter.clone();
        // Seeded from the config file: the standing view (exclusion,
        // sort, direction, shell hiding) is exactly what was saved.
        // Unknown sort names were already warned + defaulted at load.
        let sort_key = SortKey::parse_name(&config.sort).unwrap_or(SortKey::Idle);
        let sort_asc = !config.sort_desc;
        let hide_quiet_shells = config.hide_shells;
        App {
            config,
            sessions: Vec::new(),
            selected: 0,
            mode: Mode::Normal,
            input: String::new(),
            status: None,
            status_since: None,
            focus: Focus::Panes,
            panes: Vec::new(),
            filtered: Vec::new(),
            pane_selected: 0,
            preview: String::new(),
            preview_width: None,
            show_preview: false,
            preview_scroll: 0,
            preview_visible: 0,
            preview_for: None,
            agents: HashMap::new(),
            agents_live: true,
            session_scroll: 0,
            pane_scroll: 0,
            cursor: 0,
            settings_selected: 0,
            settings_scroll: 0,
            settings_section: 0,
            sort_key,
            sort_asc,
            filter: String::new(),
            exclude,
            editing_standing: false,
            filter_before_edit: String::new(),
            hide_quiet_shells,
            hidden_quiet: 0,
            marked: std::collections::HashSet::new(),
            hover: None,
            own_pane: None,
            fleet_rows: Vec::new(),
            fleet_agents: Vec::new(),
            fleet_index: HashMap::new(),
            fleet_hosts: Vec::new(),
            fleet_errors: Vec::new(),
            fleet_fetched_at: None,
            native_rows: Vec::new(),
            native_agents: Vec::new(),
            native_quiet: HashMap::new(),
            hosts_selected: 0,
            hosts_scroll: 0,
            preview_cpu_prev: None,
            preview_cpu_pct: None,
            preview_git: None,
            preview_git_line: None,
        }
    }

    /// Scroll `scroll` just enough to show `selected` in a `visible` window.
    fn keep_visible(selected: usize, scroll: &mut usize, visible: usize, len: usize) {
        if visible == 0 {
            return;
        }
        if selected < *scroll {
            *scroll = selected;
        } else if selected >= *scroll + visible {
            *scroll = selected + 1 - visible;
        }
        let max = len.saturating_sub(visible);
        if *scroll > max {
            *scroll = max;
        }
    }

    /// Called by the renderer with the real visible table rows. (The
    /// selector bar manages its own horizontal scroll during layout.)
    pub fn ensure_visible(&mut self, pane_rows: usize) {
        let panes = self.filtered.len();
        let pane_selected = self.pane_selected;
        Self::keep_visible(pane_selected, &mut self.pane_scroll, pane_rows, panes);
    }

    /// Sidebar rows: the "All sessions" row plus one per session.
    pub fn side_rows(&self) -> usize {
        self.sessions.len() + 1
    }

    /// Session under the sidebar cursor, if a real session row is selected.
    pub fn side_session(&self) -> Option<&Session> {
        self.selected
            .checked_sub(1)
            .and_then(|i| self.sessions.get(i))
    }

    /// MRU tab order: most recently active session first (max pane
    /// activity per session), ties by name, pane-less sessions sink.
    /// The cursor follows its session so a reorder never hijacks it.
    /// No-op unless `mru_tabs` is set (tmux order is the default).
    fn apply_tab_order(&mut self) {
        if !self.config.mru_tabs {
            return;
        }
        let keep = self.side_session().map(|s| s.name.clone());
        let mut max_act: HashMap<&str, u64> = HashMap::new();
        for p in &self.panes {
            let e = max_act.entry(p.session.as_str()).or_insert(0);
            *e = (*e).max(p.activity);
        }
        self.sessions.sort_by(|a, b| {
            let (aa, bb) = (
                max_act.get(a.name.as_str()).copied().unwrap_or(0),
                max_act.get(b.name.as_str()).copied().unwrap_or(0),
            );
            bb.cmp(&aa).then_with(|| a.name.cmp(&b.name))
        });
        self.selected = 0;
        if let Some(name) = keep
            && let Some(i) = self.sessions.iter().position(|s| s.name == name)
        {
            self.selected = i + 1;
        }
        self.clamp_selection();
    }

    /// Re-apply the fleet cache after a local refresh (idempotent: old
    /// remote rows are stripped first, so re-merges never duplicate).
    pub fn merge_fleet(&mut self) {
        self.merge_fleet_rows();
        self.refresh_filtered();
    }

    /// Fleet rows without the re-filter: refresh and fetch paths merge
    /// both caches before filtering once, so a foreign-only filter view
    /// never mid-passes through empty (which would close the preview).
    fn merge_fleet_rows(&mut self) {
        self.panes.retain(|p| !is_remote_pane(&p.id));
        self.panes.extend(self.fleet_rows.iter().cloned());
        for a in &self.fleet_agents {
            self.agents.insert(a.pane.clone(), a.clone());
        }
    }

    /// Re-apply the native cache after a local refresh. Runs after the
    /// fleet merge (which strips all non-% rows), so natives always come
    /// back; idempotent like the fleet merge.
    pub fn merge_native(&mut self) {
        self.merge_native_rows();
        self.refresh_filtered();
    }

    /// Native rows without the re-filter (see [`merge_fleet_rows`]).
    fn merge_native_rows(&mut self) {
        self.panes.retain(|p| !is_native_pane(&p.id));
        self.panes.extend(self.native_rows.iter().cloned());
        for a in &self.native_agents {
            self.agents.insert(a.pane.clone(), a.clone());
        }
    }

    /// Re-filter after both caches merged (single filter, no mid-pass).
    fn merge_caches(&mut self) {
        self.merge_fleet_rows();
        self.merge_native_rows();
        self.refresh_filtered();
    }

    /// Fetch remote agents from every host (manual: ssh latency has no
    /// place in the 3s tick). Replaces the cache; hosts that answer keep
    /// their rows while failures are reported, never silent.
    pub fn fetch_fleet(&mut self) {
        let hosts = match crate::fleet::load_hosts() {
            Ok(h) => h,
            Err(e) => {
                self.set_status(format!("fleet: {e:#}"));
                return;
            }
        };
        self.fleet_hosts = hosts.clone();
        if hosts.is_empty() {
            self.fleet_rows.clear();
            self.fleet_agents.clear();
            self.fleet_index.clear();
            self.fleet_errors.clear();
            self.fleet_fetched_at = None;
            self.set_status("no fleet hosts (terhub host add <name> <target>)".to_string());
            self.merge_caches();
            return;
        }
        let mut rows = Vec::new();
        let mut agents = Vec::new();
        let mut index: HashMap<String, (String, String)> = HashMap::new();
        let mut errors = Vec::new();
        for st in &crate::fleet::fleet_status(&hosts) {
            match &st.result {
                Ok(snap) => {
                    let (r, a) = crate::fleet::snapshot_rows(&st.host, snap);
                    for (row, rec) in r.iter().zip(a.iter()) {
                        index.insert(row.id.clone(), (st.host.name.clone(), rec.session.clone()));
                    }
                    rows.extend(r);
                    agents.extend(a);
                }
                Err(e) => errors.push((st.host.name.clone(), e.clone())),
            }
        }
        let hosts_ok = hosts.len() - errors.len();
        self.fleet_rows = rows;
        self.fleet_agents = agents;
        self.fleet_index = index;
        self.fleet_errors = errors.clone();
        self.fleet_fetched_at = Some(std::time::Instant::now());
        if errors.is_empty() {
            self.set_status(format!(
                "fleet: {} remote agents from {hosts_ok} host(s)",
                self.fleet_agents.len()
            ));
        } else {
            let names: Vec<&str> = errors.iter().map(|(h, _)| h.as_str()).collect();
            self.set_status(format!(
                "fleet: {} agents, {} host(s) failed ({})",
                self.fleet_agents.len(),
                errors.len(),
                names.join(", ")
            ));
        }
        self.merge_caches();
    }

    /// Advance one native tab's quiet clock over its capture. Same
    /// semantics as the daemon's poll clock: same denoised hash keeps the
    /// still-time, a new hash restarts it, an empty capture (fetch
    /// failure) preserves it — or stays unknown for never-seen tabs.
    fn track_native_quiet(&mut self, id: &str, screen: &str, now: u64) -> (u64, u64) {
        let hash = if screen.is_empty() {
            None
        } else {
            Some(crate::daemon::screen_hash(screen))
        };
        let prev = self.native_quiet.get(id).copied().unwrap_or((0, 0));
        let next = crate::daemon::track_quiet(prev, hash, now);
        self.native_quiet.insert(id.to_string(), next);
        next
    }

    /// Fetch native terminal tabs into the table (startup + F5; replaces
    /// the cache). Agent state is fetch-time: proc identity plus one
    /// screen classification, frozen until the next fetch.
    pub fn fetch_native(&mut self) {
        use crate::agent::{Source, classify_screen, compile_all_lossy, load_manifests};
        let now = crate::agent::now_unix();
        let snap = crate::proc::Snapshot::capture();
        let mut manifests = crate::agent::builtin_manifests();
        manifests.extend(load_manifests(&crate::config::manifests_dir()).0);
        let (compiled, _) = compile_all_lossy(&manifests);
        let mut rows = Vec::new();
        let mut agents = Vec::new();
        let mut notes: Vec<String> = Vec::new();
        let mut alerts: Vec<String> = Vec::new();
        for rep in crate::native::list_all() {
            match &rep.error {
                Some(e) => {
                    let short = short_native_error(e);
                    if rep.informational {
                        notes.push(format!("{}: {}", rep.backend, short));
                    } else {
                        alerts.push(format!("{}: {}", rep.backend, short));
                    }
                }
                None => {
                    notes.push(format!("{}: {} tab(s)", rep.backend, rep.tabs.len()));
                }
            }
            for tab in rep.tabs {
                let fg_comm = crate::native::proc_comm(tab.fg_pid)
                    .or_else(|| crate::native::proc_comm(tab.pid))
                    .unwrap_or_default();
                let reported = if fg_comm.is_empty() {
                    &tab.title
                } else {
                    &fg_comm
                };
                let command = snap.display_command(tab.pid, reported);
                rows.push(Pane {
                    id: tab.id.clone(),
                    session: tab.backend.clone(),
                    window_id: String::new(),
                    window_name: tab.title.clone(),
                    window_active: tab.active,
                    index: 0,
                    command: command.clone(),
                    path: crate::native::proc_cwd(tab.pid).unwrap_or_default(),
                    active: tab.active,
                    width: 0,
                    height: 0,
                    activity: 0,
                    pid: tab.pid,
                });
                let Some(hit) = snap.detect_agent(tab.pid) else {
                    continue;
                };
                // An agent with an editor in the tree summoned $EDITOR and
                // sits on keystrokes: waiting, same as the daemon reads it.
                let editor_wait = !fg_comm.is_empty() && crate::proc::is_editor_name(&fg_comm);
                let screen = crate::native::capture(&tab.id, 60).unwrap_or_default();
                let (screen_hash, quiet_since) = self.track_native_quiet(&tab.id, &screen, now);
                let classified = classify_screen(&compiled, &command, &screen);
                let (state, summary, source) = match classified {
                    Some(h) => (h.state, h.summary, Source::Screen),
                    None if editor_wait => (
                        crate::agent::AgentState::Waiting,
                        "editor in foreground".to_string(),
                        Source::Proc,
                    ),
                    None => (
                        crate::agent::AgentState::Unknown,
                        String::new(),
                        Source::Proc,
                    ),
                };
                agents.push(AgentRecord {
                    pane: tab.id.clone(),
                    agent: hit.agent.clone(),
                    state,
                    source,
                    summary,
                    session: tab.backend.clone(),
                    window: tab.title.clone(),
                    command: command.clone(),
                    updated_at: now,
                    state_since: now,
                    cpu_ticks: snap.subtree_ticks(hit.pid),
                    cpu_still_since: 0,
                    agent_start: hit.start,
                    last_turn_secs: 0,
                    screen_hash,
                    quiet_since,
                    // Native tabs have no hooks: prompts never land here.
                    last_prompt_at: 0,
                    last_prompt: String::new(),
                    // Minted below against the daemon's handles (no clash).
                    handle: String::new(),
                    model: String::new(),
                    ctx_pct: None,
                    reg_pid: 0,
                });
            }
        }
        self.native_rows = rows;
        self.native_agents = agents;
        // Closed tabs must not pin clocks forever.
        let live: Vec<String> = self.native_rows.iter().map(|r| r.id.clone()).collect();
        self.native_quiet.retain(|id, _| live.contains(id));
        self.assign_native_handles();
        // Split the footer into two surfaces:
        //  - alerts (real backend failures) -> the sticky status line,
        //    warning color, re-asserted only when the text changes.
        //  - notes ("N tab(s)" / "absent here") -> informational, dim,
        //    never the warning color. They don't pin the footer at all:
        //    the tab counts are already visible in the table rows, so the
        //    footer hint stays the standard dim help line.
        if !alerts.is_empty() {
            let alert = format!("native: {}", alerts.join("; "));
            if self.status.as_deref() != Some(alert.as_str()) {
                self.set_status(alert);
            }
        } else {
            // No real failures: let the TTL-faded hint line win instead
            // of re-asserting a note every 3s tick.
        }
        self.merge_caches();
    }

    /// Mint handles for native agent records, avoiding the daemon-issued
    /// ones (a tmux session named `konsole` must not shadow a tab).
    fn assign_native_handles(&mut self) {
        use crate::agent::mint_handle;
        let mut used: std::collections::HashSet<String> = self
            .agents
            .values()
            .filter_map(|r| {
                if r.handle.is_empty() {
                    None
                } else {
                    Some(r.handle.clone())
                }
            })
            .collect();
        let mut idx: Vec<usize> = (0..self.native_agents.len()).collect();
        idx.sort_by_key(|i| self.native_agents[*i].pane.clone());
        for i in idx {
            let rec = &self.native_agents[i];
            if !rec.handle.is_empty() || rec.agent.is_empty() {
                continue;
            }
            let handle = mint_handle(&used, &rec.agent, &rec.session);
            used.insert(handle.clone());
            self.native_agents[i].handle = handle;
        }
    }

    /// (host, remote session) for a remote row id, if the cache knows it.
    pub fn remote_target(&self, id: &str) -> Option<(String, String)> {
        self.fleet_index.get(id).cloned()
    }

    /// Jump event for a pane id: remote rows exit to ssh, native rows
    /// switch the terminal's tab, local rows focus in place. A remote id
    /// the cache forgot is stale, not fatal.
    pub fn jump_event(&mut self, id: &str) -> Option<AppEvent> {
        if is_native_pane(id) {
            return Some(AppEvent::FocusNative(id.to_string()));
        }
        if !is_remote_pane(id) {
            return Some(AppEvent::FocusPane(id.to_string()));
        }
        match self.remote_target(id) {
            Some((host, session)) => Some(AppEvent::RemoteAttach { host, session }),
            None => {
                self.set_status(format!("stale remote row {id} (fetch the fleet again)"));
                None
            }
        }
    }

    pub fn set_sessions(&mut self, sessions: Vec<Session>) {
        let keep = self.side_session().map(|s| s.name.clone());
        self.sessions = sessions;
        self.selected = 0;
        if let Some(name) = keep
            && let Some(i) = self.sessions.iter().position(|s| s.name == name)
        {
            self.selected = i + 1;
        }
        self.clamp_selection();
    }

    /// Refresh from tmux; never fails outwardly, stores errors as status.
    pub fn refresh(&mut self, tmux: &Tmux) {
        match tmux.list_sessions() {
            Ok(s) => self.set_sessions(s),
            Err(e) => self.set_status(format!("tmux error: {e}")),
        }
        match tmux.list_all_panes() {
            Ok(mut panes) => {
                panes.sort_by(|a, b| {
                    (&a.session, &a.window_name, a.index, &a.id).cmp(&(
                        &b.session,
                        &b.window_name,
                        b.index,
                        &b.id,
                    ))
                });
                self.panes = panes;
                // No refresh_filtered here: filtering the tmux-only midway
                // list would momentarily empty a foreign-only filter view
                // and close the preview. The merges below re-filter once
                // the row set is complete.
                // Drop marks for dead panes; survivors keep theirs.
                let live: std::collections::HashSet<&str> =
                    self.panes.iter().map(|p| p.id.as_str()).collect();
                self.marked.retain(|id| live.contains(id.as_str()));
            }
            Err(e) => {
                // Non-destructive: a failed tick keeps stale rows, the
                // filter view, and the mark set — the error shows in the
                // status line and the next good tick refreshes everything.
                self.set_status(format!("panes error: {e}"));
            }
        }
        self.apply_tab_order();
        self.refresh_agents();
        // Remote rows/badges ride the cache: local ticks must not drop
        // them (and a dead daemon must not take them with it).
        self.merge_caches();
        self.follow_preview(tmux);
        if self.filtered.is_empty() {
            // Only bounce to Sessions when the user wasn't deliberately
            // on Panes — a deliberate Panes focus survives a filter
            // that empties, so toggle_focus can still return the user
            // to a now-non-empty table without the refresh stealing it.
            if self.focus != Focus::Panes {
                self.focus = Focus::Sessions;
            }
        }
    }

    /// Pull the daemon snapshot; a dead daemon clears the badges and drops
    /// the liveness flag (the title says so — empty badges can't).
    pub fn refresh_agents(&mut self) {
        match daemon::fetch_snapshot(Duration::from_millis(200)) {
            Ok(snap) => {
                self.agents = snap
                    .agents
                    .into_iter()
                    .map(|a| (a.pane.clone(), a))
                    .collect();
                self.agents_live = true;
            }
            Err(_) => {
                self.agents.clear();
                self.agents_live = false;
            }
        }
    }

    /// Footer toast lifetime: confirmations fade back to the hints instead
    /// of pinning the footer for the rest of the session.
    pub const STATUS_TTL: std::time::Duration = std::time::Duration::from_secs(5);

    /// Show a footer message that expires after [`App::STATUS_TTL`].
    pub fn set_status(&mut self, text: String) {
        self.status = Some(text);
        self.status_since = Some(std::time::Instant::now());
    }

    /// Clear an expired footer message, if any. Called every render.
    pub fn expire_status(&mut self) {
        if self
            .status_since
            .is_some_and(|t| t.elapsed() >= Self::STATUS_TTL)
        {
            self.status = None;
            self.status_since = None;
        }
    }

    pub fn attention_count(&self) -> usize {
        self.agents
            .values()
            .filter(|a| a.state.needs_attention())
            .count()
    }

    /// Needy panes among the visible (session- and text-filtered) rows, for
    /// the table title — so the count never exceeds the rows shown.
    pub fn visible_needy(&self) -> usize {
        self.filtered
            .iter()
            .filter(|p| {
                self.agents
                    .get(&p.id)
                    .is_some_and(|a| a.state.needs_attention())
            })
            .count()
    }

    /// Visible agents that finished, plus unclaimed panes quiet past
    /// [`QUIET_SHELL_SECS`]. Disjoint from `visible_needy` (idle never
    /// needs attention), so the two counters together answer "how much is
    /// running vs. how much is done". Counting unclaimed-quiet panes too
    /// means the `X idle` summary stays meaningful when the daemon is
    /// down or an agent never registered — the user can still see how
    /// much of the fleet is dormant.
    pub fn visible_idle(&self) -> usize {
        let now = crate::agent::now_unix();
        self.filtered
            .iter()
            .filter(|p| {
                match self.agents.get(&p.id) {
                    Some(a) => a.state == crate::agent::AgentState::Idle,
                    None => is_quiet_shell(None, p.activity, now),
                }
            })
            .count()
    }

    /// Rebuild the visible table rows from the sidebar filter, keeping the
    /// selected pane when it survives the filter.
    pub fn refresh_filtered(&mut self) {
        self.hover = None;
        let keep = self.selected_pane().map(|p| p.id.clone());
        let session = self.side_session().map(|s| s.name.clone());
        self.filtered = match session {
            None => self.panes.clone(),
            // Foreign rows (fleet + native) live under All only, even if a
            // tmux session ever shares their display name.
            Some(name) => self
                .panes
                .iter()
                .filter(|p| p.session == name && !is_remote_pane(&p.id))
                .cloned()
                .collect(),
        };
        if !self.exclude.is_empty() {
            let q = self.exclude.to_lowercase();
            let agents = &self.agents;
            self.filtered
                .retain(|p| pane_matches(p, agents.get(&p.id), &q));
        }
        if !self.filter.is_empty() {
            let q = self.filter.to_lowercase();
            let agents = &self.agents;
            self.filtered
                .retain(|p| pane_matches(p, agents.get(&p.id), &q));
        }
        self.hidden_quiet = 0;
        if self.hide_quiet_shells {
            let now = crate::agent::now_unix();
            let agents = &self.agents;
            let before = self.filtered.len();
            self.filtered
                .retain(|p| !is_quiet_shell(agents.get(&p.id), p.activity, now));
            self.hidden_quiet = before - self.filtered.len();
        }
        self.apply_sort();
        self.pane_selected = 0;
        if let Some(id) = keep
            && let Some(i) = self.filtered.iter().position(|p| p.id == id)
        {
            self.pane_selected = i;
        }
        self.clamp_pane_selection();
        if self.filtered.is_empty() && self.show_preview {
            self.show_preview = false;
            self.preview.clear();
        }
    }

    /// Header click: re-click toggles direction, a new column starts with
    /// its default direction. Callers re-run `refresh_filtered` to apply.
    pub fn cycle_sort(&mut self, key: SortKey) {
        if self.sort_key == key {
            self.sort_asc = !self.sort_asc;
        } else {
            self.sort_key = key;
            self.sort_asc = key.default_asc();
        }
    }

    /// `o` key: step through the columns in cycle order.
    pub fn cycle_sort_key(&mut self) {
        let pos = SORT_CYCLE
            .iter()
            .position(|k| *k == self.sort_key)
            .unwrap_or(0);
        self.cycle_sort(SORT_CYCLE[(pos + 1) % SORT_CYCLE.len()]);
    }

    /// Explicit re-sorts restart at the top row: the point of a new order
    /// is seeing its first rows, not keeping a stale cursor mid-list.
    pub fn resort(&mut self, key: SortKey) {
        self.cycle_sort(key);
        self.refresh_filtered();
        self.pane_selected = 0;
        self.pane_scroll = 0;
    }

    /// `o` key: next column, same restart-at-top.
    pub fn resort_next(&mut self) {
        self.cycle_sort_key();
        self.refresh_filtered();
        self.pane_selected = 0;
        self.pane_scroll = 0;
        self.persist_view();
    }

    /// Order `filtered` by the current sort. Stable, so ties keep the base
    /// session/window/index order; unknowns sink in default directions.
    pub fn apply_sort(&mut self) {
        let now = crate::agent::now_unix();
        let key = self.sort_key;
        let asc = self.sort_asc;
        let agents = &self.agents;
        self.filtered.sort_by(|a, b| {
            let ord = match key {
                SortKey::Pane => pane_num(&a.id).cmp(&pane_num(&b.id)),
                SortKey::Session => a.session.cmp(&b.session),
                SortKey::Window => {
                    (a.window_name.as_str(), a.index).cmp(&(b.window_name.as_str(), b.index))
                }
                SortKey::Cmd => a.command.cmp(&b.command),
                SortKey::Dir => dir_name(&a.path).cmp(dir_name(&b.path)),
                SortKey::Agent => agent_name(agents.get(&a.id)).cmp(agent_name(agents.get(&b.id))),
                SortKey::Handle => {
                    handle_name(agents.get(&a.id)).cmp(handle_name(agents.get(&b.id)))
                }
                SortKey::State => state_rank(agents.get(&a.id)).cmp(&state_rank(agents.get(&b.id))),
                SortKey::Src => src_name(agents.get(&a.id)).cmp(src_name(agents.get(&b.id))),
                SortKey::For => {
                    age_secs(agents.get(&a.id), now).cmp(&age_secs(agents.get(&b.id), now))
                }
                SortKey::Turn => turn_secs(agents.get(&a.id)).cmp(&turn_secs(agents.get(&b.id))),
                SortKey::Idle => idle_secs(agents.get(&a.id), a.activity)
                    .cmp(&idle_secs(agents.get(&b.id), b.activity)),
                SortKey::Summary => {
                    summary_text(agents.get(&a.id)).cmp(summary_text(agents.get(&b.id)))
                }
            };
            if asc { ord } else { ord.reverse() }
        });
    }

    /// Worst agent state in a session (None = everywhere): the tab dot.
    /// Unclaimed panes don't vote; with no agent rows there is no dot.
    pub fn session_worst(&self, session: Option<&str>) -> Option<crate::agent::AgentState> {
        let mut best: Option<crate::agent::AgentState> = None;
        for p in &self.panes {
            if session.is_some_and(|s| p.session != s) {
                continue;
            }
            let Some(rec) = self.agents.get(&p.id) else {
                continue;
            };
            let take = match best {
                None => true,
                Some(b) => state_rank_of(rec.state) < state_rank_of(b),
            };
            if take {
                best = Some(rec.state);
            }
        }
        best
    }

    /// (panes, needy) per session for the sidebar counts.
    pub fn session_counts(&self) -> HashMap<String, (usize, usize)> {
        let mut out: HashMap<String, (usize, usize)> = HashMap::new();
        for p in &self.panes {
            let e = out.entry(p.session.clone()).or_insert((0, 0));
            e.0 += 1;
            if self
                .agents
                .get(&p.id)
                .is_some_and(|a| a.state.needs_attention())
            {
                e.1 += 1;
            }
        }
        out
    }

    /// Preview a remote row over ssh (target resolved from the host
    /// registry by the jump index's host name).
    fn capture_remote_preview(&mut self, id: &str) {
        let target = self.remote_target(id).and_then(|(host, _)| {
            crate::fleet::load_hosts()
                .ok()?
                .into_iter()
                .find(|h| h.name == host)
                .map(|h| h.target)
        });
        let pane = id.split_once(':').map(|(_, p)| p).unwrap_or("");
        match (target, pane) {
            (Some(t), p) if !p.is_empty() => {
                match crate::fleet::remote_capture(&t, p, PREVIEW_SCROLLBACK) {
                    Ok(text) => {
                        self.preview = text;
                        self.preview_width = None;
                    }
                    Err(_) => {
                        self.preview = "(remote capture failed)".to_string();
                        self.preview_width = None;
                    }
                }
            }
            _ => {
                self.preview = "(stale remote row)".to_string();
                self.preview_width = None;
            }
        }
    }

    /// Refresh the preview title's CPU segment from the selected pane's
    /// daemon tick counter (two samples make a rate; a pane switch or a
    /// restarted process re-baselines instead of spiking).
    fn update_preview_cpu(&mut self) {
        let now = Instant::now();
        let sample = self
            .selected_pane()
            .and_then(|p| self.agents.get(&p.id).map(|r| (p.id.clone(), r.cpu_ticks)));
        let pct = match (&sample, &self.preview_cpu_prev) {
            (Some((id, ticks)), Some((prev_id, prev_ticks, prev_at))) if id == prev_id => {
                cpu_pct(*prev_ticks, *prev_at, *ticks, now, clock_hz())
            }
            _ => None,
        };
        self.preview_cpu_pct = pct;
        self.preview_cpu_prev = sample.map(|(id, ticks)| (id, ticks, now));
    }

    /// Refresh the preview title's git segment for the selected pane's
    /// dir (cached 30s per dir; empty dirs and non-repos stay blank).
    fn update_preview_git(&mut self) {
        const TTL_SECS: u64 = 30;
        let now = Instant::now();
        let dir = self
            .selected_pane()
            .map(|p| p.path.clone())
            .unwrap_or_default();
        if dir.is_empty() {
            self.preview_git_line = None;
            return;
        }
        if let Some((cached_dir, line, at)) = &self.preview_git
            && *cached_dir == dir
            && now.duration_since(*at).as_secs() < TTL_SECS
        {
            self.preview_git_line = line.clone();
            return;
        }
        let line = git_diff_stat(&dir).map(|g| g.render());
        self.preview_git = Some((dir, line.clone(), now));
        self.preview_git_line = line;
    }

    /// Preview buffer height in lines (a trailing newline adds no row).
    pub fn preview_line_count(&self) -> usize {
        if self.preview.is_empty() {
            0
        } else {
            self.preview.lines().count()
        }
    }

    /// Deepest legal reader offset: last page when the height is known,
    /// last line before the first render.
    pub fn preview_max_scroll(&self) -> usize {
        let total = self.preview_line_count();
        if self.preview_visible == 0 {
            total.saturating_sub(1)
        } else {
            total.saturating_sub(self.preview_visible)
        }
    }

    /// Move the reader by `delta` lines, clamped to the buffer.
    pub fn scroll_preview(&mut self, delta: isize) {
        let max = self.preview_max_scroll();
        let next = self.preview_scroll as isize + delta;
        self.preview_scroll = next.clamp(0, max as isize) as usize;
    }

    /// Page the reader by the last rendered height (20 lines pre-render).
    fn scroll_preview_page(&mut self, up: bool) {
        let page = if self.preview_visible == 0 {
            20
        } else {
            self.preview_visible.max(1)
        } as isize;
        self.scroll_preview(if up { -page } else { page });
    }

    fn capture_preview(&mut self, tmux: &Tmux) {
        self.update_preview_cpu();
        self.update_preview_git();
        match self.selected_pane().map(|p| p.id.clone()) {
            Some(id) if is_native_pane(&id) => self.capture_native_preview(&id),
            Some(id) if is_remote_pane(&id) => self.capture_remote_preview(&id),
            Some(id) => match tmux.capture_pane_styled(&id, PREVIEW_SCROLLBACK) {
                Ok(text) => {
                    self.preview = text;
                    self.preview_width = tmux
                        .display(&id, "#{pane_width}")
                        .ok()
                        .and_then(|s| s.trim().parse().ok());
                }
                Err(_) => {
                    self.preview = "(capture failed)".to_string();
                    self.preview_width = None;
                }
            },
            None => {
                self.preview.clear();
                self.preview_width = None;
            }
        }
        // Tick re-captures keep the reader position; new content only clamps it.
        let max = self.preview_max_scroll();
        self.preview_scroll = self.preview_scroll.min(max);
    }

    /// Preview a native row over its terminal's control channel.
    fn capture_native_preview(&mut self, id: &str) {
        match crate::native::capture(id, PREVIEW_SCROLLBACK) {
            Ok(text) => {
                self.preview = text;
                self.preview_width = None;
            }
            Err(_) => {
                self.preview = "(native capture failed)".to_string();
                self.preview_width = None;
            }
        }
        self.refresh_native_cpu(id);
    }

    /// Native agent records are fetch-time, so the preview's CPU clock
    /// would freeze at one sample: re-read this row's subtree ticks live.
    fn refresh_native_cpu(&mut self, id: &str) {
        let (Some(pane), Some(rec)) = (
            self.selected_pane().filter(|p| p.id == id),
            self.agents.get(id),
        ) else {
            return;
        };
        if rec.agent.is_empty() || pane.pid == 0 {
            return;
        }
        let snap = crate::proc::Snapshot::capture();
        if let Some(hit) = snap.detect_agent(pane.pid)
            && let Some(rec) = self.agents.get_mut(id)
        {
            rec.cpu_ticks = snap.subtree_ticks(hit.pid);
        }
    }

    /// Right-click: with the preview open it closes from anywhere,
    /// otherwise row `idx` (when the click landed on one) is selected and
    /// previewed.
    pub fn right_click(&mut self, tmux: &Tmux, idx: Option<usize>) {
        if self.show_preview {
            self.toggle_preview(tmux);
            return;
        }
        if let Some(i) = idx {
            self.focus = Focus::Panes;
            self.select_pane(i);
            self.toggle_preview(tmux);
        }
    }

    /// Left-click target while the preview is open: the previewed pane
    /// itself, since the table underneath is fully covered.
    pub fn preview_jump_target(&self) -> Option<String> {
        if self.show_preview {
            self.selected_pane().map(|p| p.id.clone())
        } else {
            None
        }
    }

    /// Open/close the preview, capturing on open.
    pub fn toggle_preview(&mut self, tmux: &Tmux) {
        if self.show_preview {
            self.show_preview = false;
            return;
        }
        if self.selected_pane().is_none() {
            self.set_status("no pane selected".to_string());
            return;
        }
        self.preview_scroll = 0;
        self.preview_for = self.selected_pane().map(|p| p.id.clone());
        self.capture_preview(tmux);
        self.show_preview = true;
    }

    /// Re-capture while the preview is open so it follows the selection.
    /// A newly selected pane restarts the reader; the same pane (tick
    /// refresh, clamped arrow key) keeps its scroll position.
    pub fn follow_preview(&mut self, tmux: &Tmux) {
        if !self.show_preview {
            return;
        }
        let cur = self.selected_pane().map(|p| p.id.clone());
        if self.preview_for != cur {
            self.preview_for = cur;
            self.preview_scroll = 0;
        }
        self.capture_preview(tmux);
    }

    pub fn selected_pane(&self) -> Option<&Pane> {
        self.filtered.get(self.pane_selected)
    }

    pub fn select_pane(&mut self, idx: usize) {
        self.pane_selected = idx;
        self.clamp_pane_selection();
    }

    fn clamp_pane_selection(&mut self) {
        if self.filtered.is_empty() {
            self.pane_selected = 0;
            self.pane_scroll = 0;
        } else {
            if self.pane_selected >= self.filtered.len() {
                self.pane_selected = self.filtered.len() - 1;
            }
            let max = self.filtered.len().saturating_sub(1);
            if self.pane_scroll > max {
                self.pane_scroll = max;
            }
        }
    }

    pub fn move_pane_up(&mut self) {
        if self.pane_selected > 0 {
            self.pane_selected -= 1;
        }
    }

    pub fn move_pane_down(&mut self) {
        if !self.filtered.is_empty() && self.pane_selected + 1 < self.filtered.len() {
            self.pane_selected += 1;
        }
    }

    pub fn toggle_focus(&mut self) {
        if self.filtered.is_empty() {
            self.focus = Focus::Sessions;
        } else {
            self.focus = match self.focus {
                Focus::Sessions => Focus::Panes,
                Focus::Panes => Focus::Sessions,
            };
        }
    }

    pub fn select(&mut self, idx: usize) {
        self.selected = idx;
        self.clamp_selection();
    }

    /// Session-tab click: select the tab and refilter. With `attach`
    /// (ctrl+click) also return the attach event for the tab's session.
    /// The All tab never attaches (it isn't a session); double-click is
    /// gone (the first click already selects, so it could never work).
    pub fn click_session_segment(
        &mut self,
        idx: usize,
        attach: bool,
        tmux: &Tmux,
    ) -> Option<AppEvent> {
        if idx >= self.side_rows() {
            return None;
        }
        self.focus = Focus::Sessions;
        self.select(idx);
        self.refresh_filtered();
        self.follow_preview(tmux);
        if attach {
            if let Some(s) = self.side_session() {
                return Some(AppEvent::Attach(s.name.clone()));
            }
            self.set_status("All isn't a session (pick a tab)".to_string());
        }
        None
    }

    fn clamp_selection(&mut self) {
        let rows = self.side_rows();
        if self.selected >= rows {
            self.selected = rows.saturating_sub(1);
        }
        let max = rows.saturating_sub(1);
        if self.session_scroll > max {
            self.session_scroll = max;
        }
    }

    pub fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    pub fn move_down(&mut self) {
        if self.selected + 1 < self.side_rows() {
            self.selected += 1;
        }
    }

    fn cancel_modal(&mut self) {
        self.mode = Mode::Normal;
        self.input.clear();
    }

    /// Handle one key press. Returns an event when the TUI should exit.
    /// Dispatch a modifier-qualified binding. Custom commands run
    /// against the selected row; standard actions call the same handler
    /// the bare-key path uses, exactly as if the user had pressed the
    /// bound key without modifiers. Returns `true` when the binding
    /// fired (the caller swallows the key event).
    fn dispatch_binding(&mut self, binding: &crate::config::KeyBinding, tmux: &Tmux) -> bool {
        // Custom commands carry their own KeyBinding and are matched by
        // exact value (same modifier set + same key).
        let cmd = self
            .config
            .commands
            .iter()
            .find(|c| c.spec == *binding)
            .cloned();
        if let Some(cmd) = cmd {
            self.run_custom(&cmd);
            return true;
        }
        match self.binding_action(binding).as_str() {
            "" => false, // no standard action claimed this binding
            "new" => {
                self.do_new_session();
                true
            }
            "kill" => {
                if self.focus == Focus::Panes {
                    match self.selected_pane() {
                        Some(p) if is_native_pane(&p.id) => self.set_status(format!(
                            "can't close native tab {} from here (close it in the terminal)",
                            p.id
                        )),
                        Some(p) if is_remote_pane(&p.id) => self.set_status(format!(
                            "can't kill remote row {} (jump in to manage it)",
                            p.id
                        )),
                        Some(p) => self.mode = Mode::ConfirmKillPane(p.id.clone()),
                        None => self.set_status("no pane selected".to_string()),
                    }
                } else {
                    match self.side_session() {
                        Some(s) => self.mode = Mode::ConfirmKill(s.name.clone()),
                        None => {
                            self.set_status("pick a session row (All doesn't count)".to_string())
                        }
                    }
                }
                true
            }
            "rename" => {
                self.do_rename_session();
                true
            }
            "attach" => {
                if self.focus == Focus::Panes || self.show_preview {
                    if let Some(p) = self.selected_pane() {
                        let id = p.id.clone();
                        self.jump_event(&id);
                    } else {
                        self.set_status("no pane selected".to_string());
                    }
                } else if let Some(s) = self.side_session() {
                    self.do_nav(tmux, AppEvent::Attach(s.name.clone()));
                } else {
                    self.set_status("pick a session row (All doesn't count)".to_string());
                }
                true
            }
            "detach" => {
                self.do_detach(tmux);
                true
            }
            "quit" => {
                // Only reachable through a modifier binding; the bare q
                // path handles plain quit. Returning false lets the
                // caller swallow the event without a second AppEvent.
                false
            }
            "help" => {
                self.set_status(LEGEND.to_string());
                true
            }
            "refresh" => {
                self.refresh(tmux);
                self.fetch_native();
                true
            }
            "attend" => {
                self.do_attend();
                true
            }
            "preview" => {
                self.toggle_preview(tmux);
                true
            }
            "theme" => {
                self.cycle_theme();
                true
            }
            "density" => {
                self.cycle_density();
                true
            }
            "settings" => {
                self.open_settings();
                true
            }
            "sort" => {
                self.resort_next();
                self.follow_preview(tmux);
                true
            }
            "filter" => {
                self.open_filter();
                true
            }
            "filter_clear" => {
                if self.filter.is_empty() {
                    self.set_status("search already clear".to_string());
                } else {
                    self.filter.clear();
                    self.refresh_filtered();
                    self.set_status("search cleared".to_string());
                }
                true
            }
            "hide_shells" => {
                self.toggle_hide_quiet_shells();
                true
            }
            "spawn" => {
                self.do_spawn();
                true
            }
            "restart" => {
                match self.selected_pane() {
                    Some(p) if is_native_pane(&p.id) => self.set_status(format!(
                        "can't restart native tab {} (re-run inside the tab)",
                        p.id
                    )),
                    Some(p) if is_remote_pane(&p.id) => self.set_status(format!(
                        "can't restart remote row {} (jump in to manage it)",
                        p.id
                    )),
                    Some(p) if self.own_pane.as_deref() == Some(p.id.as_str()) => {
                        self.set_status("won't restart own pane (q then reopen)".to_string())
                    }
                    Some(p) if p.command.trim().is_empty() => {
                        self.set_status(format!("nothing to re-run in {}", p.id))
                    }
                    Some(p) => {
                        self.mode = Mode::ConfirmRestartPane {
                            id: p.id.clone(),
                            command: p.command.clone(),
                        }
                    }
                    None => self.set_status("no pane selected".to_string()),
                }
                true
            }
            "pause" => {
                self.do_pause();
                true
            }
            "hosts" => {
                self.open_hosts();
                true
            }
            "bulk" => {
                self.do_bulk();
                true
            }
            "mark" => {
                self.toggle_mark();
                true
            }
            "unmark" => {
                let n = self.marked.len();
                self.marked.clear();
                self.set_status(if n == 0 {
                    "nothing marked".to_string()
                } else {
                    format!("cleared {n} mark(s)")
                });
                true
            }
            _ => false,
        }
    }

    /// Map a [`KeyBinding`] back to the standard action name it belongs to
    /// (empty string when no standard action claims it — i.e. it was a
    /// custom command, already handled in [`dispatch_binding`]).
    fn binding_action(&self, binding: &crate::config::KeyBinding) -> String {
        for action in [
            "new",
            "kill",
            "rename",
            "attach",
            "detach",
            "quit",
            "help",
            "refresh",
            "attend",
            "preview",
            "theme",
            "density",
            "settings",
            "sort",
            "filter",
            "filter_clear",
            "hide_shells",
            "spawn",
            "restart",
            "pause",
            "hosts",
            "bulk",
            "mark",
            "unmark",
        ] {
            if self.config.spec(action) == *binding {
                return action.to_string();
            }
        }
        String::new()
    }

    /// All keyable actions and their current bindings, in dispatch order
    /// (standard actions first, then `[[command]]`s): modifier-qualified
    /// presses are matched against this list before the modifier-drop
    /// guard swallows them.
    fn all_bindings(&self) -> Vec<crate::config::KeyBinding> {
        use crate::config::KeyBinding;
        let mut out: Vec<KeyBinding> = Vec::new();
        for action in [
            "new",
            "kill",
            "rename",
            "attach",
            "detach",
            "quit",
            "help",
            "refresh",
            "attend",
            "preview",
            "theme",
            "density",
            "settings",
            "sort",
            "filter",
            "filter_clear",
            "hide_shells",
            "spawn",
            "restart",
            "pause",
            "hosts",
            "bulk",
            "mark",
            "unmark",
        ] {
            out.push(self.config.spec(action));
        }
        for c in &self.config.commands {
            out.push(c.spec.clone());
        }
        out
    }

    pub fn on_key(&mut self, key: KeyEvent, tmux: &Tmux) -> Option<AppEvent> {
        if key.modifiers.intersects(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c' | 'C'))
        {
            return Some(AppEvent::Quit);
        }
        // Alt+1..8 jumps to tabs (All + first 7 sessions), Normal mode
        // only — everywhere else Alt keys stay ignored (typing safety).
        if self.mode == Mode::Normal
            && key.modifiers == KeyModifiers::ALT
            && let KeyCode::Char(c) = key.code
            && ('1'..='8').contains(&c)
        {
            let want = (c as usize) - ('1' as usize);
            if want <= self.sessions.len() {
                self.select(want);
                self.refresh_filtered();
                self.follow_preview(tmux);
            } else {
                self.set_status(format!("no tab {c}"));
            }
            return None;
        }
        // Modifier-qualified bindings ("ctrl+s"-style): a press carrying
        // ctrl/alt/super fires the binding that asks for exactly those
        // modifiers (Normal mode only — prompt handlers own their
        // modifier policy). Anything unbound is swallowed, exactly as
        // bare-modifier presses always were (typing safety).
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
            && self.mode == Mode::Normal
        {
            for binding in self.all_bindings() {
                if binding.spec == crate::config::KeySpec::None {
                    continue;
                }
                if crate::config::key_matches(&binding, &key.code, key.modifiers)
                    && self.dispatch_binding(&binding, tmux)
                {
                    return None;
                }
            }
            return None;
        }
        match self.mode.clone() {
            Mode::PromptNew | Mode::PromptRename(_) => self.on_prompt_key(key, tmux),
            Mode::PromptFilter => self.on_filter_key(key, tmux),
            Mode::PromptSpawn { .. } => self.on_spawn_key(key, tmux),
            Mode::PromptBulk { .. } => self.on_bulk_key(key, tmux),
            Mode::ConfirmKill(_) => self.on_confirm_key(key, tmux),
            Mode::ConfirmKillPane(_) => self.on_confirm_pane_key(key, tmux),
            Mode::ConfirmRestartPane { .. } => self.on_confirm_restart_key(key, tmux),
            Mode::ConfirmKillMarked(_) => self.on_confirm_marked_key(key, tmux),
            Mode::CommandOutput { .. } => self.on_command_output_key(key),
            Mode::Hosts => self.on_hosts_key(key),
            Mode::PromptHostAdd => self.on_host_add_key(key),
            Mode::Settings => self.on_settings_key(key),
            Mode::Normal => self.on_normal_key(key, tmux),
        }
    }

    fn on_prompt_key(&mut self, key: KeyEvent, tmux: &Tmux) -> Option<AppEvent> {
        match key.code {
            KeyCode::Esc => self.cancel_modal(),
            KeyCode::Enter => {
                let value = self.input.trim().to_string();
                match self.mode.clone() {
                    Mode::PromptNew => {
                        if value.is_empty() {
                            self.cancel_modal();
                        } else {
                            match tmux.new_session(&value, None, None) {
                                Ok(_) => {
                                    self.set_status(format!("created {value}"));
                                    self.refresh(tmux);
                                    self.select_named(&value);
                                }
                                Err(e) => self.set_status(format!("create failed: {e}")),
                            }
                            self.cancel_modal();
                        }
                    }
                    Mode::PromptRename(old) => {
                        if !value.is_empty() && value != old {
                            match tmux.rename_session(&old, &value) {
                                Ok(_) => {
                                    self.set_status(format!("renamed {old} -> {value}"));
                                    self.refresh(tmux);
                                    self.select_named(&value);
                                }
                                Err(e) => self.set_status(format!("rename failed: {e}")),
                            }
                        }
                        self.cancel_modal();
                    }
                    _ => self.cancel_modal(),
                }
            }
            KeyCode::Backspace => {
                self.delete_before_cursor();
            }
            KeyCode::Delete => {
                self.delete_at_cursor();
            }
            KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(1);
            }
            KeyCode::Right => {
                let len = self.input.chars().count();
                self.cursor = (self.cursor + 1).min(len);
            }
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.input.chars().count(),
            KeyCode::Char(c) if !c.is_control() => self.insert_at_cursor(c),
            _ => {}
        }
        None
    }

    /// Filter prompt: live narrowing while typing, Enter keeps the query,
    /// Esc restores the pre-edit query (clearing lives on Esc in Normal
    /// mode, or deleting everything + Enter).
    fn on_filter_key(&mut self, key: KeyEvent, tmux: &Tmux) -> Option<AppEvent> {
        // A modifier-qualified filter_clear binding (e.g. ctrl+backspace)
        // hard-wipes the query in place and bypasses the per-key match
        // so Backspace's "delete one char" arm doesn't fire alongside
        // it. Esc keeps its restore-pre-edit semantics: clear only
        // fires when the binding is modifier-qualified.
        let clear_spec = self.config.spec("filter_clear");
        if clear_spec.spec != crate::config::KeySpec::None
            && clear_spec.mods != crossterm::event::KeyModifiers::NONE
            && crate::config::key_matches(&clear_spec, &key.code, key.modifiers)
        {
            self.input.clear();
            self.cursor = 0;
            if self.editing_standing {
                self.exclude.clear();
                self.set_status("standing filter cleared (Enter to save)".to_string());
            } else {
                self.filter.clear();
                self.set_status("search cleared".to_string());
            }
            self.refresh_filtered();
            self.follow_preview(tmux);
            return None;
        }
        match key.code {
            KeyCode::Esc => {
                if self.editing_standing {
                    self.exclude.clone_from(&self.filter_before_edit);
                } else {
                    self.filter.clone_from(&self.filter_before_edit);
                }
                self.cancel_modal();
                self.refresh_filtered();
                self.follow_preview(tmux);
                // Report before clearing the flag: the label names
                // which query survived (search vs standing draft).
                self.report_kept_query();
                if self.editing_standing {
                    self.editing_standing = false;
                    self.mode = Mode::Settings;
                }
            }
            KeyCode::Enter => {
                let q = self.input.trim().to_string();
                if self.editing_standing {
                    // Standing is exclusion-only: `zsh` saves as `!zsh`.
                    self.exclude = normalize_exclude(&q);
                    self.persist_filter();
                    self.editing_standing = false;
                    self.cancel_modal();
                    self.mode = Mode::Settings;
                    self.set_status(if self.exclude.is_empty() {
                        "standing filter cleared".to_string()
                    } else {
                        format!("standing filter saved: {}", self.exclude)
                    });
                } else {
                    self.filter = q;
                    self.cancel_modal();
                    if !self.filter.is_empty() {
                        self.set_status(format!("search kept for this session: {}", self.filter));
                    }
                }
                self.refresh_filtered();
                self.pane_selected = 0;
                self.pane_scroll = 0;
                self.follow_preview(tmux);
            }
            KeyCode::Backspace => {
                self.delete_before_cursor();
                self.apply_live_filter();
            }
            KeyCode::Delete => {
                self.delete_at_cursor();
                self.apply_live_filter();
            }
            KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(1);
            }
            KeyCode::Right => {
                let len = self.input.chars().count();
                self.cursor = (self.cursor + 1).min(len);
            }
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.input.chars().count(),
            KeyCode::Char(c) if !c.is_control() => {
                self.insert_at_cursor(c);
                self.apply_live_filter();
            }
            _ => {}
        }
        None
    }

    /// Byte offset of the character cursor (clamped).
    fn cursor_offset(&self) -> usize {
        self.input
            .char_indices()
            .nth(self.cursor)
            .map(|(i, _)| i)
            .unwrap_or(self.input.len())
    }

    fn insert_at_cursor(&mut self, c: char) {
        let at = self.cursor_offset();
        self.input.insert(at, c);
        self.cursor += 1;
    }

    fn delete_before_cursor(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor -= 1;
        let at = self.cursor_offset();
        self.input.remove(at);
    }

    fn delete_at_cursor(&mut self) {
        let at = self.cursor_offset();
        if at < self.input.len() {
            self.input.remove(at);
        }
    }

    /// Spawn modal: type the opening prompt, Tab cycles the agent
    /// profile. Enter spawns even with an empty prompt (a bare agent);
    /// Esc cancels. Everything runs in-loop (stay-open).
    fn on_spawn_key(&mut self, key: KeyEvent, tmux: &Tmux) -> Option<AppEvent> {
        let n = self.config.profiles.len();
        if n == 0 {
            self.set_status("no spawn profiles (every [[profile]] invalid?)".to_string());
            self.cancel_modal();
            return None;
        }
        match key.code {
            KeyCode::Esc => self.cancel_modal(),
            KeyCode::Tab | KeyCode::Down => {
                if let Mode::PromptSpawn { profile } = &mut self.mode {
                    *profile = (*profile + 1) % n;
                }
            }
            KeyCode::BackTab | KeyCode::Up => {
                if let Mode::PromptSpawn { profile } = &mut self.mode {
                    *profile = (*profile + n - 1) % n;
                }
            }
            KeyCode::Enter => self.submit_spawn(tmux),
            KeyCode::Backspace => {
                self.delete_before_cursor();
            }
            KeyCode::Delete => {
                self.delete_at_cursor();
            }
            KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(1);
            }
            KeyCode::Right => {
                let len = self.input.chars().count();
                self.cursor = (self.cursor + 1).min(len);
            }
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.input.chars().count(),
            KeyCode::Char(c) if !c.is_control() => self.insert_at_cursor(c),
            _ => {}
        }
        None
    }

    fn submit_spawn(&mut self, tmux: &Tmux) {
        let idx = match self.mode {
            Mode::PromptSpawn { profile } => profile,
            _ => {
                self.cancel_modal();
                return;
            }
        };
        let prompt = self.input.trim().to_string();
        let (Some(prof), Some(session)) = (
            self.config.profiles.get(idx).cloned(),
            self.spawn_target_session(),
        ) else {
            self.set_status("pick a session row (All doesn't count)".to_string());
            self.cancel_modal();
            return;
        };
        let name = spawn_window_name(&prof.name, &prompt);
        match tmux.new_window(&session, &name, prof.dir.as_deref(), Some(&prof.program)) {
            Ok(pane_id) => {
                if prompt.is_empty() {
                    self.set_status(format!("spawned {pane_id} ({})", prof.name));
                } else {
                    match tmux.send_keys(&pane_id, &prompt, true) {
                        Ok(_) => self.set_status(format!("spawned {pane_id} ({})", prof.name)),
                        Err(e) => self.set_status(format!("spawned {pane_id}, prompt failed: {e}")),
                    }
                }
                self.refresh(tmux);
                if let Some(i) = self.filtered.iter().position(|p| p.id == pane_id) {
                    self.select_pane(i);
                }
            }
            Err(e) => self.set_status(format!("spawn failed: {e}")),
        }
        self.cancel_modal();
    }

    /// Session the spawn lands in: the selected pane's session from the
    /// table, the sidebar cursor otherwise (All doesn't count).
    fn spawn_target_session(&self) -> Option<String> {
        if self.focus == Focus::Panes {
            self.selected_pane().map(|p| p.session.clone())
        } else {
            self.side_session().map(|s| s.name.clone())
        }
    }

    fn on_confirm_key(&mut self, key: KeyEvent, tmux: &Tmux) -> Option<AppEvent> {
        let name = match &self.mode {
            Mode::ConfirmKill(n) => n.clone(),
            _ => return None,
        };
        match key.code {
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => self.cancel_modal(),
            KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                match tmux.kill_session(&name) {
                    Ok(_) => {
                        self.set_status(format!("killed {name}"));
                        self.refresh(tmux);
                    }
                    Err(e) => self.set_status(format!("kill failed: {e}")),
                }
                self.cancel_modal();
            }
            _ => {}
        }
        None
    }

    fn on_normal_key(&mut self, key: KeyEvent, tmux: &Tmux) -> Option<AppEvent> {
        let code = key.code;
        let mods = key.modifiers;
        // Tab switches lists; Esc closes the preview, steps out of the table,
        // else quits. Navigation is fixed (arrow keys + mouse); the rest is
        // configurable.
        match code {
            KeyCode::Tab | KeyCode::BackTab => {
                self.toggle_focus();
                return None;
            }
            KeyCode::Up => {
                if self.focus == Focus::Panes {
                    self.move_pane_up();
                } else {
                    self.move_up();
                    self.refresh_filtered();
                }
                self.follow_preview(tmux);
                return None;
            }
            KeyCode::Down => {
                if self.focus == Focus::Panes {
                    self.move_pane_down();
                } else {
                    self.move_down();
                    self.refresh_filtered();
                }
                self.follow_preview(tmux);
                return None;
            }
            KeyCode::Left => {
                if self.focus == Focus::Sessions {
                    self.move_up();
                    self.refresh_filtered();
                    self.follow_preview(tmux);
                    return None;
                }
            }
            KeyCode::Right => {
                if self.focus == Focus::Sessions {
                    self.move_down();
                    self.refresh_filtered();
                    self.follow_preview(tmux);
                    return None;
                }
            }
            KeyCode::PageUp => {
                if self.show_preview {
                    self.scroll_preview_page(true);
                    return None;
                }
            }
            KeyCode::PageDown => {
                if self.show_preview {
                    self.scroll_preview_page(false);
                    return None;
                }
            }
            KeyCode::Home => {
                if self.show_preview {
                    self.preview_scroll = 0;
                    return None;
                }
            }
            KeyCode::End => {
                if self.show_preview {
                    self.preview_scroll = self.preview_max_scroll();
                    return None;
                }
            }
            KeyCode::Esc => {
                if self.show_preview {
                    self.show_preview = false;
                    return None;
                }
                if !self.filter.is_empty() {
                    self.filter.clear();
                    self.refresh_filtered();
                    self.set_status("search cleared".to_string());
                    return None;
                }
                if self.focus == Focus::Panes {
                    self.focus = Focus::Sessions;
                    return None;
                }
                return Some(AppEvent::Quit);
            }
            _ => {}
        }
        // Style + preview keys work from either focus.
        if key_matches(&self.config.spec("theme"), &code, mods) {
            self.cycle_theme();
            return None;
        }
        if key_matches(&self.config.spec("density"), &code, mods) {
            self.cycle_density();
            return None;
        }
        if key_matches(&self.config.spec("preview"), &code, mods) {
            self.toggle_preview(tmux);
            return None;
        }
        if key_matches(&self.config.spec("settings"), &code, mods) {
            self.open_settings();
            return None;
        }
        if key_matches(&self.config.spec("hosts"), &code, mods) {
            self.open_hosts();
            return None;
        }
        if key_matches(&self.config.spec("sort"), &code, mods) {
            self.resort_next();
            self.follow_preview(tmux);
            self.set_status(format!(
                "sort: {:?} {}",
                self.sort_key,
                if self.sort_asc { "▲" } else { "▼" }
            ));
            return None;
        }
        if key_matches(&self.config.spec("filter"), &code, mods) {
            self.open_filter();
            return None;
        }
        if key_matches(&self.config.spec("hide_shells"), &code, mods) {
            self.toggle_hide_quiet_shells();
            return None;
        }
        // Fullscreen preview owns the screen: Enter jumps to the previewed
        // pane from either focus (like left-click), instead of attaching to
        // a session bar the preview covers up.
        if self.show_preview && key_matches(&self.config.spec("attach"), &code, mods) {
            if let Some(p) = self.selected_pane() {
                let id = p.id.clone();
                return self.jump_event(&id);
            }
            self.set_status("no pane selected".to_string());
            return None;
        }
        if self.focus == Focus::Panes {
            return self.on_pane_key(key, tmux);
        }
        let cfg = &self.config;
        if key_matches(&cfg.spec("quit"), &code, mods) {
            return Some(AppEvent::Quit);
        }
        if key_matches(&cfg.spec("attach"), &code, mods) {
            if let Some(s) = self.side_session() {
                return Some(AppEvent::Attach(s.name.clone()));
            }
            self.set_status("pick a session row (All doesn't count)".to_string());
            return None;
        }
        if key_matches(&cfg.spec("attend"), &code, mods) {
            return self.do_attend();
        }
        if key_matches(&cfg.spec("new"), &code, mods) {
            self.do_new_session();
            return None;
        }
        if key_matches(&cfg.spec("spawn"), &code, mods) {
            self.do_spawn();
            return None;
        }
        if key_matches(&cfg.spec("rename"), &code, mods) {
            self.do_rename_session();
            return None;
        }
        if key_matches(&cfg.spec("kill"), &code, mods) {
            match self.side_session() {
                Some(s) => self.mode = Mode::ConfirmKill(s.name.clone()),
                None => self.set_status("pick a session row (All doesn't count)".to_string()),
            }
            return None;
        }
        if key_matches(&cfg.spec("detach"), &code, mods) {
            self.do_detach(tmux);
            return None;
        }
        if key_matches(&cfg.spec("refresh"), &code, mods) {
            self.refresh(tmux);
            self.fetch_native();
            return None;
        }
        // Custom commands run against the selected pane from either focus
        // (mirrors the Panes branch below).
        let custom = cfg
            .commands
            .iter()
            .find(|c| key_matches(&c.spec, &code, mods))
            .cloned();
        if let Some(cmd) = custom {
            self.run_custom(&cmd);
            return None;
        }
        if key_matches(&cfg.spec("help"), &code, mods) {
            // Repeated `?` toggles between the key list and the legend.
            if self
                .status
                .as_deref()
                .is_some_and(|s| s.starts_with("sessions:"))
            {
                self.set_status(LEGEND.to_string());
            } else {
                self.set_status(format!(
                    "sessions: {}=new {}=spawn {}=rename {}=kill {}=attach {}=preview {}=theme {}=settings {}=hosts {}=hide alt+1..8=tabs {}=quit",
                    cfg.key_label("new"),
                    cfg.key_label("spawn"),
                    cfg.key_label("rename"),
                    cfg.key_label("kill"),
                    cfg.key_label("attach"),
                    cfg.key_label("preview"),
                    cfg.key_label("theme"),
                    cfg.key_label("settings"),
                    cfg.key_label("hosts"),
                    cfg.key_label("hide_shells"),
                    cfg.key_label("quit"),
                ));
            }
            return None;
        }
        // Type-to-filter: an unbound printable keystroke opens the
        // filter with that char already typed, so ad-hoc filtering
        // needs no mode key first. Bound keys (and Ctrl/Alt chords)
        // were consumed above; `/` always forces it for queries that
        // would otherwise begin with a bound key (e.g. `!zsh`).
        if let KeyCode::Char(c) = code
            && Self::is_typeable(&code, &mods)
        {
            self.open_filter_seeded(c);
            return None;
        }
        None
    }

    fn select_named(&mut self, name: &str) {
        if let Some(i) = self.sessions.iter().position(|s| s.name == name) {
            self.selected = i + 1;
            self.refresh_filtered();
        }
    }

    /// Rotate the theme live (session-only; the config file is untouched).
    pub fn cycle_theme(&mut self) {
        use crate::config::THEMES;
        let cur = THEMES.iter().position(|t| *t == self.config.theme);
        let next = cur.map(|i| (i + 1) % THEMES.len()).unwrap_or(0);
        self.config.theme = THEMES[next].to_string();
        self.set_status(format!(
            "theme: {} ({}/{})",
            THEMES[next],
            next + 1,
            THEMES.len()
        ));
    }

    /// Flip table density live (session-only).
    pub fn cycle_density(&mut self) {
        self.config.density = if self.config.compact() {
            "comfortable".to_string()
        } else {
            "compact".to_string()
        };
        self.set_status(format!("density: {}", self.config.density));
    }

    /// Settings popup rows: section headers plus actionable items.
    pub fn settings_rows(&self) -> Vec<SettingRow> {
        use crate::config::{DENSITIES, THEMES};
        let mut rows = vec![SettingRow::Header("Theme")];
        for (i, t) in THEMES.iter().enumerate() {
            rows.push(SettingRow::Item {
                label: t.to_string(),
                active: self.config.palette() == *t,
                kind: SettingKind::Theme(i),
            });
        }
        rows.push(SettingRow::Header("Density"));
        for (i, d) in DENSITIES.iter().enumerate() {
            rows.push(SettingRow::Item {
                label: d.to_string(),
                active: self.config.density_name() == *d,
                kind: SettingKind::Density(i),
            });
        }
        rows.push(SettingRow::Header("Toggles"));
        rows.push(SettingRow::Item {
            label: format!(
                "notifications: {}",
                Self::onoff(self.config.notifications_enabled)
            ),
            active: self.config.notifications_enabled,
            kind: SettingKind::ToggleNotifications,
        });
        rows.push(SettingRow::Item {
            label: format!("mru tabs: {}", Self::onoff(self.config.mru_tabs)),
            active: self.config.mru_tabs,
            kind: SettingKind::ToggleMruTabs,
        });
        rows.push(SettingRow::Item {
            label: format!(
                "highlight attached: {}",
                Self::onoff(self.config.highlight_attached)
            ),
            active: self.config.highlight_attached,
            kind: SettingKind::ToggleHighlight,
        });
        rows.push(SettingRow::Header("Filters"));
        rows.push(SettingRow::Item {
            label: if self.exclude.is_empty() {
                "standing filter (excludes): (none) — Enter to edit".to_string()
            } else {
                format!(
                    "standing filter (excludes): {} — Enter to edit",
                    self.exclude
                )
            },
            active: !self.exclude.is_empty(),
            kind: SettingKind::EditStandingFilter,
        });
        rows.push(SettingRow::Header("Tray icons"));
        for r in crate::tray::icon_legend(self.config.palette()) {
            rows.push(SettingRow::Legend {
                triangle: "▼",
                color: r.hex,
                name: r.name,
                meaning: r.meaning,
            });
        }
        rows
    }

    fn onoff(b: bool) -> &'static str {
        if b { "on" } else { "off" }
    }

    /// Row indices of actionable items (headers aren't selectable).
    fn settings_item_rows(&self) -> Vec<usize> {
        self.settings_rows()
            .iter()
            .enumerate()
            .filter(|(_, r)| matches!(r, SettingRow::Item { .. }))
            .map(|(i, _)| i)
            .collect()
    }

    /// Settings sections, derived from the row list: each `Header`
    /// starts a section carrying the rows beneath it. One source of
    /// truth — the sidebar and the content pane split the same rows.
    /// Returns `(name, content row indices)` per section.
    pub fn settings_sections(&self) -> Vec<(&'static str, Vec<usize>)> {
        let mut out: Vec<(&'static str, Vec<usize>)> = Vec::new();
        for (i, r) in self.settings_rows().iter().enumerate() {
            if let SettingRow::Header(name) = r {
                out.push((name, Vec::new()));
            } else if let Some((_, rows)) = out.last_mut() {
                rows.push(i);
            }
        }
        out
    }

    /// Header row of a section: headers delimit contiguous spans, so it
    /// sits immediately before the section's first content row.
    fn settings_section_header(sections: &[(&'static str, Vec<usize>)], sec: usize) -> usize {
        sections
            .get(sec)
            .and_then(|(_, rows)| rows.first())
            .copied()
            .unwrap_or(1)
            .saturating_sub(1)
    }

    /// Section index holding a global settings row (headers belong to
    /// the section they start).
    fn settings_section_of_row(&self, row: usize) -> usize {
        let sections = self.settings_sections();
        let mut sec = 0;
        for (i, _) in sections.iter().enumerate() {
            if Self::settings_section_header(&sections, i) <= row {
                sec = i;
            } else {
                break;
            }
        }
        sec
    }

    /// Snap the displayed section + scroll to the selected item: the
    /// sidebar follows the selection, and the content starts at the
    /// section's header row.
    fn snap_settings_to_selection(&mut self) {
        let items = self.settings_item_rows();
        let row = items.get(self.settings_selected).copied().unwrap_or(0);
        let sections = self.settings_sections();
        self.settings_section = self.settings_section_of_row(row);
        let _ = sections;
        self.settings_scroll =
            Self::settings_section_header(&self.settings_sections(), self.settings_section);
    }

    /// Jump to the neighbouring section (Left/Right, sidebar clicks).
    pub fn open_settings(&mut self) {
        self.mode = Mode::Settings;
        self.settings_selected = 0;
        self.settings_scroll = 0;
        self.settings_section = 0;
    }

    pub fn open_hosts(&mut self) {
        match crate::fleet::load_hosts() {
            Ok(hosts) => self.fleet_hosts = hosts,
            Err(e) => {
                self.fleet_hosts.clear();
                self.set_status(format!("fleet: {e:#}"));
            }
        }
        self.hosts_selected = 0;
        self.hosts_scroll = 0;
        self.mode = Mode::Hosts;
    }

    /// Hosts modal: navigate the registry, add/remove hosts, fetch the
    /// fleet into the table, jump to a host's neediest remote agent.
    fn on_hosts_key(&mut self, key: KeyEvent) -> Option<AppEvent> {
        match key.code {
            KeyCode::Esc => self.cancel_modal(),
            KeyCode::Up => {
                self.hosts_selected = self.hosts_selected.saturating_sub(1);
                self.clamp_hosts_scroll();
            }
            KeyCode::Down => {
                self.hosts_selected = self.hosts_selected.saturating_add(1);
                self.clamp_hosts();
                // Push the window down so the selection stays visible
                // (clamp_hosts_scroll only ever pulled up).
                if self.hosts_selected > self.hosts_scroll {
                    self.hosts_scroll = self.hosts_selected;
                }
            }
            KeyCode::Char('a') => {
                self.mode = Mode::PromptHostAdd;
                self.input.clear();
                self.cursor = 0;
            }
            KeyCode::Char('x') | KeyCode::Char('d') => self.remove_selected_host(),
            KeyCode::Char('r') => self.fetch_fleet(),
            KeyCode::Enter => return self.jump_to_host_needy(),
            _ => {}
        }
        None
    }

    fn clamp_hosts(&mut self) {
        if self.fleet_hosts.is_empty() {
            self.hosts_selected = 0;
        } else if self.hosts_selected >= self.fleet_hosts.len() {
            self.hosts_selected = self.fleet_hosts.len() - 1;
        }
    }

    fn clamp_hosts_scroll(&mut self) {
        if self.hosts_selected < self.hosts_scroll {
            self.hosts_scroll = self.hosts_selected;
        }
        // After the list shrinks (remove_host, fetch_fleet) the scroll
        // can sit past the end; pull it back so the window isn't empty.
        if !self.fleet_hosts.is_empty() && self.hosts_scroll >= self.fleet_hosts.len() {
            self.hosts_scroll = self.fleet_hosts.len() - 1;
        }
    }

    fn remove_selected_host(&mut self) {
        let Some(host) = self.fleet_hosts.get(self.hosts_selected) else {
            self.set_status("no hosts (a to add)".to_string());
            return;
        };
        let name = host.name.clone();
        match crate::fleet::remove_host(&name) {
            Ok(true) => {
                self.set_status(format!("removed host {name}"));
                self.fleet_hosts.retain(|h| h.name != name);
                self.clamp_hosts();
                self.clamp_hosts_scroll();
            }
            Ok(false) => self.set_status(format!("host {name} already gone")),
            Err(e) => self.set_status(format!("remove failed: {e:#}")),
        }
    }

    /// Jump to the selected host's neediest cached agent (fetch first).
    fn jump_to_host_needy(&mut self) -> Option<AppEvent> {
        let Some(host) = self.fleet_hosts.get(self.hosts_selected) else {
            self.set_status("no hosts (a to add)".to_string());
            return None;
        };
        let name = host.name.clone();
        let candidates: Vec<AgentRecord> = self
            .fleet_agents
            .iter()
            .filter(|a| {
                self.fleet_index
                    .get(&a.pane)
                    .is_some_and(|(h, _)| *h == name)
            })
            .cloned()
            .collect();
        if candidates.is_empty() {
            self.set_status(format!("no cached agents for {name} (r to fetch)"));
            return None;
        }
        match self.attend_event(&candidates) {
            Some(ev) => Some(ev),
            None => {
                self.set_status(format!("{name}: all clear"));
                None
            }
        }
    }

    /// Add-host prompt: `name target [observe|control]`. Bad input keeps
    /// the prompt open (the text is half the work); Esc steps back out.
    fn on_host_add_key(&mut self, key: KeyEvent) -> Option<AppEvent> {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Hosts;
            }
            KeyCode::Enter => {
                let parts: Vec<&str> = self.input.split_whitespace().collect();
                let (name, target, mode) = match parts.as_slice() {
                    [name, target] => (*name, *target, crate::fleet::Mode::Observe),
                    [name, target, mode] => match crate::fleet::Mode::parse(mode) {
                        Some(m) => (*name, *target, m),
                        None => {
                            self.set_status(format!(
                                "want: name target [observe|control] (bad mode '{mode}')"
                            ));
                            return None;
                        }
                    },
                    _ => {
                        self.set_status("want: name target [observe|control]".to_string());
                        return None;
                    }
                };
                match crate::fleet::add_host(name, target, mode, "") {
                    Ok(()) => {
                        self.set_status(format!("added host {name}"));
                        if let Ok(hosts) = crate::fleet::load_hosts() {
                            self.fleet_hosts = hosts;
                        }
                        self.mode = Mode::Hosts;
                    }
                    Err(e) => self.set_status(format!("add failed: {e:#}")),
                }
            }
            KeyCode::Backspace => {
                self.delete_before_cursor();
            }
            KeyCode::Delete => {
                self.delete_at_cursor();
            }
            KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(1);
            }
            KeyCode::Right => {
                let len = self.input.chars().count();
                self.cursor = (self.cursor + 1).min(len);
            }
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.input.chars().count(),
            KeyCode::Char(c) if !c.is_control() => self.insert_at_cursor(c),
            _ => {}
        }
        None
    }

    /// Persist the whole standing view (theme/density/toggles/filter/
    /// sort/direction/shell-hiding) with one surgical config write.
    /// Silent on success; the caller already announced the change.
    /// Loud only when the write failed. Public for the header-click
    /// sort path, which lives in the binary.
    pub fn persist_view(&mut self) {
        self.config.filter.clone_from(&self.exclude);
        self.config.sort = self.sort_key.as_str().to_string();
        self.config.sort_desc = !self.sort_asc;
        self.config.hide_shells = self.hide_quiet_shells;
        if let Err(e) = crate::config::persist_ui_settings(&self.config.ui_persist()) {
            self.set_status(format!("view kept for this session only (not saved: {e})"));
        }
    }

    /// Save the standing exclusion to `[ui] filter` so it survives
    /// restarts. Silent on success (the status line already names the
    /// query); loud only when the write failed.
    fn persist_filter(&mut self) {
        self.persist_view();
    }

    /// Open the text filter, editing the current query in place.
    pub fn open_filter(&mut self) {
        self.mode = Mode::PromptFilter;
        self.editing_standing = false;
        self.filter_before_edit.clone_from(&self.filter);
        self.input.clone_from(&self.filter);
        self.cursor = self.input.chars().count();
    }

    /// Open the prompt editing the standing exclusion (settings row):
    /// Enter saves it to `[ui] filter`, Esc restores without saving.
    pub fn open_standing_editor(&mut self) {
        self.mode = Mode::PromptFilter;
        self.editing_standing = true;
        self.filter_before_edit.clone_from(&self.exclude);
        self.input.clone_from(&self.exclude);
        self.cursor = self.input.chars().count();
    }

    /// Printable keystroke with no Ctrl/Alt held: feeds type-to-filter.
    /// Shift is fine (uppercase queries); Ctrl/Alt chords do something
    /// else (or nothing) and must never start a filter.
    fn is_typeable(code: &KeyCode, mods: &KeyModifiers) -> bool {
        matches!(code, KeyCode::Char(c) if !c.is_control())
            && !mods.intersects(
                KeyModifiers::CONTROL
                    | KeyModifiers::ALT
                    | KeyModifiers::META
                    | KeyModifiers::SUPER
                    | KeyModifiers::HYPER,
            )
    }

    /// Open the filter with `c` already typed: type-to-filter entry for
    /// keystrokes that matched no binding.
    fn open_filter_seeded(&mut self, c: char) {
        self.open_filter();
        self.insert_at_cursor(c);
        self.apply_live_filter();
    }

    /// `h` key: flip the quiet-shell filter, reporting the hidden count.
    /// The choice persists: a dashboard that hides idle shells stays hidden.
    fn toggle_hide_quiet_shells(&mut self) {
        self.hide_quiet_shells = !self.hide_quiet_shells;
        self.refresh_filtered();
        self.persist_view();
        if self.hide_quiet_shells {
            let key = self.config.key_label("hide_shells");
            self.set_status(format!(
                "hiding {} quiet shell(s) ({key} to show)",
                self.hidden_quiet
            ));
        } else {
            self.set_status("showing all rows".to_string());
        }
    }

    /// Status line for the kept query after the prompt closes without
    /// saving: names whichever query (search or standing draft) survived.
    fn report_kept_query(&mut self) {
        let (label, q) = if self.editing_standing {
            ("standing filter", self.exclude.clone())
        } else {
            ("search", self.filter.clone())
        };
        if q.is_empty() {
            self.set_status(format!("{label} cleared"));
        } else {
            self.set_status(format!("{label}: {q}"));
        }
    }

    /// Live re-filter while typing; restarts at the top row. Edits
    /// whichever query the prompt opened for (search or standing).
    fn apply_live_filter(&mut self) {
        if self.editing_standing {
            self.exclude.clone_from(&self.input);
        } else {
            self.filter.clone_from(&self.input);
        }
        self.refresh_filtered();
        self.pane_selected = 0;
        self.pane_scroll = 0;
    }

    /// Apply the item at popup row `row` (for mouse clicks).
    pub fn settings_apply_at_row(&mut self, row: usize) -> bool {
        let kind = self.settings_rows().get(row).and_then(|r| match r {
            SettingRow::Item { kind, .. } => Some(*kind),
            SettingRow::Header(_) | SettingRow::Legend { .. } => None,
        });
        match kind {
            Some(k) => {
                self.apply_setting(k);
                true
            }
            None => false,
        }
    }

    /// Apply a setting live and persist it to the config file.
    pub fn apply_setting(&mut self, kind: SettingKind) {
        use crate::config::{DENSITIES, THEMES};
        let label = match kind {
            SettingKind::Theme(i) => {
                self.config.theme = THEMES[i % THEMES.len()].to_string();
                format!("theme: {}", self.config.theme)
            }
            SettingKind::Density(i) => {
                self.config.density = DENSITIES[i % DENSITIES.len()].to_string();
                format!("density: {}", self.config.density)
            }
            SettingKind::ToggleNotifications => {
                self.config.notifications_enabled = !self.config.notifications_enabled;
                format!(
                    "notifications: {}",
                    Self::onoff(self.config.notifications_enabled)
                )
            }
            SettingKind::ToggleHighlight => {
                self.config.highlight_attached = !self.config.highlight_attached;
                format!(
                    "highlight attached: {}",
                    Self::onoff(self.config.highlight_attached)
                )
            }
            SettingKind::ToggleMruTabs => {
                self.config.mru_tabs = !self.config.mru_tabs;
                self.apply_tab_order();
                format!("mru tabs: {}", Self::onoff(self.config.mru_tabs))
            }
            SettingKind::EditStandingFilter => {
                // Editing, not applying: the prompt saves on Enter.
                // Return early so the tail below doesn't persist
                // unrelated settings or claim anything was saved.
                self.open_standing_editor();
                return;
            }
        };
        match crate::config::persist_ui_settings(&self.config.ui_persist()) {
            Ok(()) => self.set_status(format!("{label} (saved)")),
            Err(e) => self.set_status(format!("{label} (live only: {e})")),
        }
    }

    fn move_settings_up(&mut self) {
        let before = self.settings_selected;
        self.settings_selected = self.settings_selected.saturating_sub(1);
        if self.settings_selected != before {
            self.snap_settings_to_selection();
        }
    }

    fn move_settings_down(&mut self) {
        let before = self.settings_selected;
        if self.settings_selected + 1 < self.settings_item_rows().len() {
            self.settings_selected += 1;
        }
        if self.settings_selected != before {
            self.snap_settings_to_selection();
        }
    }

    /// Mouse wheel: scroll the visible surface — the fullscreen preview
    /// reader, the output popup, the settings list, or the table/sidebar
    /// selection (scroll follows via the existing ensure-visible paths).
    /// Prompts and confirms eat it.
    pub fn on_wheel(&mut self, up: bool) {
        if self.show_preview && matches!(self.mode, Mode::Normal) {
            self.scroll_preview(if up { -3 } else { 3 });
            return;
        }
        if matches!(self.mode, Mode::CommandOutput { .. }) {
            let (len, scroll) = match &mut self.mode {
                Mode::CommandOutput { lines, scroll, .. } => (lines.len(), scroll),
                _ => unreachable!("just matched"),
            };
            if up {
                *scroll = scroll.saturating_sub(3);
            } else {
                *scroll = (*scroll + 3).min(len.saturating_sub(1));
            }
            return;
        }
        if matches!(self.mode, Mode::Settings) {
            if up {
                self.move_settings_up();
            } else {
                self.move_settings_down();
            }
        } else if matches!(self.mode, Mode::Normal) {
            let panes = self.focus == Focus::Panes;
            if up {
                if panes {
                    self.move_pane_up();
                } else {
                    self.move_up();
                }
            } else if panes {
                self.move_pane_down();
            } else {
                self.move_down();
            }
        }
    }

    fn on_settings_key(&mut self, key: KeyEvent) -> Option<AppEvent> {
        let items = self.settings_item_rows();
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                return None;
            }
            KeyCode::Up => {
                self.move_settings_up();
                return None;
            }
            KeyCode::Down => {
                self.move_settings_down();
                return None;
            }
            KeyCode::Left => {
                self.move_settings_section(-1);
                return None;
            }
            KeyCode::Right => {
                self.move_settings_section(1);
                return None;
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                if let Some(row) = items.get(self.settings_selected) {
                    self.settings_apply_at_row(*row);
                }
                return None;
            }
            _ => {}
        }
        // The settings key toggles the popup closed.
        if key_matches(&self.config.spec("settings"), &key.code, key.modifiers) {
            self.mode = Mode::Normal;
        }
        None
    }

    /// Panes a bulk prompt may reach: local tmux panes and native tabs,
    /// never remote rows and never the TUI's own pane. Marked panes win;
    /// with no marks, every attention-state agent is the target. Sorted
    /// so the modal and the send order read deterministically.
    pub fn bulk_targets(&self) -> Vec<String> {
        let live: std::collections::HashSet<&str> =
            self.panes.iter().map(|p| p.id.as_str()).collect();
        let sendable = |id: &str| -> bool {
            live.contains(id)
                && (id.starts_with('%') || is_native_pane(id))
                && self.own_pane.as_deref() != Some(id)
        };
        let mut out: Vec<String> = if self.marked.is_empty() {
            self.agents
                .values()
                .filter(|a| a.state.needs_attention() && sendable(&a.pane))
                .map(|a| a.pane.clone())
                .collect()
        } else {
            self.marked
                .iter()
                .filter(|id| sendable(id))
                .cloned()
                .collect()
        };
        out.sort();
        out
    }

    /// Open the bulk-prompt modal, or say why there is nothing to send to.
    pub fn do_bulk(&mut self) {
        let targets = self.bulk_targets();
        if targets.is_empty() {
            self.set_status(
                "nothing to prompt (mark panes, or wait for an agent to need you)".to_string(),
            );
            return;
        }
        self.input.clear();
        self.cursor = 0;
        self.mode = Mode::PromptBulk {
            targets,
            template: 0,
        };
    }

    fn on_bulk_key(&mut self, key: KeyEvent, tmux: &Tmux) -> Option<AppEvent> {
        let n = self.config.templates.len().max(1);
        match key.code {
            KeyCode::Esc => self.cancel_modal(),
            KeyCode::Tab | KeyCode::Down => {
                if let Mode::PromptBulk { template, .. } = &mut self.mode {
                    *template = (*template + 1) % n;
                }
                self.apply_bulk_template();
            }
            KeyCode::BackTab | KeyCode::Up => {
                if let Mode::PromptBulk { template, .. } = &mut self.mode {
                    *template = (*template + n - 1) % n;
                }
                self.apply_bulk_template();
            }
            KeyCode::Enter => self.submit_bulk(tmux),
            KeyCode::Backspace => self.delete_before_cursor(),
            KeyCode::Delete => self.delete_at_cursor(),
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => {
                self.cursor = (self.cursor + 1).min(self.input.chars().count());
            }
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.input.chars().count(),
            KeyCode::Char(c) if !c.is_control() => self.insert_at_cursor(c),
            _ => {}
        }
        None
    }

    /// Fill the bulk input from the cycled template.
    fn apply_bulk_template(&mut self) {
        let idx = match &self.mode {
            Mode::PromptBulk { template, .. } => *template,
            _ => return,
        };
        if let Some(t) = self.config.templates.get(idx) {
            self.input = t.text.clone();
            self.cursor = self.input.chars().count();
        }
    }

    /// Send the bulk input to every target (Enter included: these are
    /// prompts, not drafts). Failures are counted, never silent.
    fn submit_bulk(&mut self, tmux: &Tmux) {
        let Mode::PromptBulk { targets, .. } = self.mode.clone() else {
            return;
        };
        let value = self.input.trim().to_string();
        if value.is_empty() {
            self.set_status("nothing to send (type a prompt or Tab a template)".to_string());
            return;
        }
        let mut failed = 0;
        for t in &targets {
            let ok = if is_native_pane(t) {
                crate::native::send_text(t, &value, true).is_ok()
            } else {
                tmux.send_keys(t, &value, true).is_ok()
            };
            if !ok {
                failed += 1;
            }
        }
        let sent = targets.len() - failed;
        self.cancel_modal();
        self.set_status(if failed == 0 {
            format!("sent to {} pane(s)", targets.len())
        } else {
            format!("sent to {sent} pane(s) ({failed} failed)")
        });
    }

    /// Jump to the longest-waiting agent. Works from either focus.
    /// Route an attend winner: remote panes exit to ssh, local focus.
    fn attend_event(&self, agents: &[AgentRecord]) -> Option<AppEvent> {
        let winner = crate::attend::pick_attend(agents, None)?;
        if is_native_pane(&winner.pane) {
            return Some(AppEvent::FocusNative(winner.pane.clone()));
        }
        if is_remote_pane(&winner.pane) {
            let (host, session) = self.remote_target(&winner.pane)?;
            Some(AppEvent::RemoteAttach { host, session })
        } else {
            Some(AppEvent::FocusPane(winner.pane.clone()))
        }
    }

    fn do_attend(&mut self) -> Option<AppEvent> {
        match daemon::fetch_snapshot(Duration::from_millis(300)) {
            Ok(snap) => {
                let mut all = snap.agents;
                // Cached remotes join the search (marks scope to local:
                // remote rows can't be marked, so a marked search skips them).
                all.extend(self.fleet_agents.iter().cloned());
                all.extend(self.native_agents.iter().cloned());
                // Marked panes scope the search; unmarked behaves as before.
                let agents: Vec<AgentRecord> = if self.marked.is_empty() {
                    all
                } else {
                    all.into_iter()
                        .filter(|a| self.marked.contains(&a.pane))
                        .collect()
                };
                match self.attend_event(&agents) {
                    Some(ev) => Some(ev),
                    None => {
                        self.set_status(format!("all clear ({} agents)", agents.len()));
                        None
                    }
                }
            }
            Err(_) => {
                self.set_status("daemon not running".to_string());
                None
            }
        }
    }

    /// Execute a navigation jump without leaving the TUI. Only the client's
    /// view moves, so the table keeps running behind it; a failed jump
    /// (stale row) stays visible as a status line. Callers handle Quit by
    /// leaving the event loop, never by calling here.
    pub fn do_nav(&mut self, tmux: &Tmux, ev: AppEvent) {
        // Remote jumps exit the process (ssh owns the terminal), so the
        // main loop intercepts them before do_nav; a direct call can only
        // say so and stay.
        if let AppEvent::RemoteAttach { host, .. } = &ev {
            self.set_status(format!("remote attach to {host} exits the hub"));
            return;
        }
        let native = matches!(ev, AppEvent::FocusNative(_));
        let (what, result) = match ev {
            AppEvent::Attach(ref name) => (format!("attach {name}"), tmux.switch_session(name)),
            AppEvent::FocusPane(ref id) => (format!("jump {id}"), tmux.focus_pane(id)),
            AppEvent::FocusNative(ref id) => (format!("jump {id}"), crate::native::focus(id)),
            AppEvent::RemoteAttach { .. } => {
                self.set_status("remote attach exits the hub".to_string());
                return;
            }
            AppEvent::Quit => return,
        };
        if let Err(e) = result {
            // A failed target is usually a dead row; re-read so it vanishes.
            self.refresh(tmux);
            if native {
                self.fetch_native();
            }
            self.set_status(format!("{what} failed: {e:#}"));
            return;
        }
        self.refresh(tmux);
    }

    /// Space: toggle the row's mark and step the cursor so the next
    /// press lands on the next row to mark (or the just-unmarked row's
    /// predecessor, so the row itself doesn't scroll off).
    fn toggle_mark(&mut self) {
        let id = match self.selected_pane() {
            Some(p) => p.id.clone(),
            None => {
                self.set_status("no pane selected".to_string());
                return;
            }
        };
        // Marks feed bulk kill/attend: remote rows stay out so a marked
        // kill can never reach across ssh; native rows stay out because
        // the terminals expose no tab-close.
        if is_native_pane(&id) {
            self.set_status(format!("can't mark native row {id}"));
            return;
        }
        if is_remote_pane(&id) {
            self.set_status(format!("can't mark remote row {id}"));
            return;
        }
        if self.marked.remove(&id) {
            self.set_status(format!("unmarked {id}"));
            self.move_pane_up();
        } else {
            self.marked.insert(id.clone());
            self.set_status(format!("marked {id}"));
            self.move_pane_down();
        }
    }

    /// Session actions with no pane meaning (`new`/`rename`/`detach`), shared
    /// by both foci so they work from the table too — otherwise the
    /// empty-state hint names a dead key in the default focus.
    fn do_new_session(&mut self) {
        self.mode = Mode::PromptNew;
        self.input.clear();
    }

    fn do_spawn(&mut self) {
        if self.config.profiles.is_empty() {
            self.set_status("no spawn profiles (every [[profile]] invalid?)".to_string());
            return;
        }
        if self.spawn_target_session().is_none() {
            self.set_status("pick a session row (All doesn't count)".to_string());
            return;
        }
        self.mode = Mode::PromptSpawn { profile: 0 };
        self.input.clear();
        self.cursor = 0;
    }

    /// Freeze/thaw the selected pane's process tree. Read-modify by live
    /// /proc state (not a cached flag): an agent that exits while paused
    /// resumes to a loud error, never a phantom toggle.
    fn do_pause(&mut self) {
        let Some(pane) = self.selected_pane() else {
            self.set_status("no pane selected".to_string());
            return;
        };
        let (id, pid) = (pane.id.clone(), pane.pid);
        // Pause is pid-based, so native rows (real local pids) work; only
        // true remotes are out of reach.
        if is_remote_pane(&id) && !is_native_pane(&id) {
            self.set_status(format!(
                "can't pause remote row {id} (jump in to manage it)"
            ));
            return;
        }
        // Freezing your own pane stops this process: instant fake-hang.
        if self.own_pane.as_deref() == Some(id.as_str()) {
            self.set_status("won't pause own pane".to_string());
            return;
        }
        if pid == 0 {
            self.set_status(format!("no pid for pane {id}"));
            return;
        }
        if crate::proc::is_stopped(pid) {
            match crate::proc::resume_group(pid) {
                Ok(_) => self.set_status(format!("resumed {id}")),
                Err(e) => self.set_status(format!("resume failed: {e}")),
            }
        } else {
            match crate::proc::pause_group(pid) {
                Ok(_) => self.set_status(format!("paused {id}")),
                Err(e) => self.set_status(format!("pause failed: {e}")),
            }
        }
    }

    fn do_rename_session(&mut self) {
        // Panes focus renames the selected pane's session (the sidebar may
        // sit on All); Sessions focus keeps the sidebar cursor.
        if self.focus == Focus::Panes
            && let Some(p) = self.selected_pane()
            && is_native_pane(&p.id)
        {
            self.set_status("can't rename native tabs from here".to_string());
            return;
        }
        if self.focus == Focus::Panes && self.selected_pane().is_some_and(|p| is_remote_pane(&p.id))
        {
            self.set_status("can't rename remote sessions here (jump in)".to_string());
            return;
        }
        let name = if self.focus == Focus::Panes {
            self.selected_pane().map(|p| p.session.clone())
        } else {
            self.side_session().map(|s| s.name.clone())
        };
        match name {
            Some(name) => {
                self.cursor = name.chars().count();
                self.input = name.clone();
                self.mode = Mode::PromptRename(name);
            }
            None if self.focus == Focus::Panes => self.set_status("no pane selected".to_string()),
            None => self.set_status("pick a session row (All doesn't count)".to_string()),
        }
    }

    fn do_detach(&mut self, tmux: &Tmux) {
        if std::env::var("TMUX").is_ok() {
            match tmux.detach_client() {
                Ok(_) => self.set_status("detached".to_string()),
                Err(e) => self.set_status(format!("detach failed: {e}")),
            }
        } else {
            self.set_status("not inside tmux (nothing to detach)".to_string());
        }
    }

    /// Keys when the pane table has focus. Quit/refresh/help keep their
    /// global meaning; attach jumps to the pane; kill closes it.
    fn on_pane_key(&mut self, key: KeyEvent, tmux: &Tmux) -> Option<AppEvent> {
        let code = key.code;
        let mods = key.modifiers;
        let cfg = &self.config;
        if key_matches(&cfg.spec("quit"), &code, mods) {
            return Some(AppEvent::Quit);
        }
        if key_matches(&cfg.spec("attach"), &code, mods) {
            if let Some(p) = self.selected_pane() {
                let id = p.id.clone();
                return self.jump_event(&id);
            }
            self.set_status("no pane selected".to_string());
            return None;
        }
        if key_matches(&cfg.spec("kill"), &code, mods) {
            if !self.marked.is_empty() {
                let mut ids: Vec<String> = self.marked.iter().cloned().collect();
                ids.sort();
                self.mode = Mode::ConfirmKillMarked(ids);
            } else {
                match self.selected_pane() {
                    Some(p) if is_native_pane(&p.id) => self.set_status(format!(
                        "can't close native tab {} from here (close it in the terminal)",
                        p.id
                    )),
                    Some(p) if is_remote_pane(&p.id) => self.set_status(format!(
                        "can't kill remote row {} (jump in to manage it)",
                        p.id
                    )),
                    Some(p) => self.mode = Mode::ConfirmKillPane(p.id.clone()),
                    None => self.set_status("no pane selected".to_string()),
                }
            }
            return None;
        }
        if key_matches(&cfg.spec("mark"), &code, mods) {
            self.toggle_mark();
            return None;
        }
        if key_matches(&cfg.spec("unmark"), &code, mods) {
            let n = self.marked.len();
            self.marked.clear();
            self.set_status(if n == 0 {
                "nothing marked".to_string()
            } else {
                format!("cleared {n} mark(s)")
            });
            return None;
        }
        if key_matches(&cfg.spec("attend"), &code, mods) {
            return self.do_attend();
        }
        if key_matches(&cfg.spec("bulk"), &code, mods) {
            self.do_bulk();
            return None;
        }
        if key_matches(&cfg.spec("refresh"), &code, mods) {
            self.refresh(tmux);
            self.fetch_native();
            return None;
        }
        if key_matches(&cfg.spec("new"), &code, mods) {
            self.do_new_session();
            return None;
        }
        if key_matches(&cfg.spec("spawn"), &code, mods) {
            self.do_spawn();
            return None;
        }
        if key_matches(&cfg.spec("rename"), &code, mods) {
            self.do_rename_session();
            return None;
        }
        if key_matches(&cfg.spec("restart"), &code, mods) {
            match self.selected_pane() {
                Some(p) if is_native_pane(&p.id) => self.set_status(format!(
                    "can't restart native tab {} (re-run inside the tab)",
                    p.id
                )),
                Some(p) if is_remote_pane(&p.id) => self.set_status(format!(
                    "can't restart remote row {} (jump in to manage it)",
                    p.id
                )),
                Some(p) if self.own_pane.as_deref() == Some(p.id.as_str()) => {
                    self.set_status("won't restart own pane (q then reopen)".to_string())
                }
                Some(p) if p.command.trim().is_empty() => {
                    self.set_status(format!("nothing to re-run in {}", p.id))
                }
                Some(p) => {
                    self.mode = Mode::ConfirmRestartPane {
                        id: p.id.clone(),
                        command: p.command.clone(),
                    }
                }
                None => self.set_status("no pane selected".to_string()),
            }
            return None;
        }
        if key_matches(&cfg.spec("pause"), &code, mods) {
            self.do_pause();
            return None;
        }
        if key_matches(&cfg.spec("detach"), &code, mods) {
            self.do_detach(tmux);
            return None;
        }
        let custom = cfg
            .commands
            .iter()
            .find(|c| key_matches(&c.spec, &code, mods))
            .cloned();
        if let Some(cmd) = custom {
            self.run_custom(&cmd);
            return None;
        }
        if key_matches(&cfg.spec("help"), &code, mods) {
            // Repeated `?` toggles between the key list and the legend.
            if self
                .status
                .as_deref()
                .is_some_and(|s| s.starts_with("panes:"))
            {
                self.set_status(LEGEND.to_string());
                return None;
            }
            let mut help = format!(
                "panes: {}=jump {}=kill {}=spawn {}=restart {}=pause {}=preview {}=attend {}=bulk {}=mark {}=unmark {}=filter {}=hide {}=sort {}=settings {}=hosts tab=back {}=quit",
                cfg.key_label("attach"),
                cfg.key_label("kill"),
                cfg.key_label("spawn"),
                cfg.key_label("restart"),
                cfg.key_label("pause"),
                cfg.key_label("preview"),
                cfg.key_label("attend"),
                cfg.key_label("bulk"),
                cfg.key_label("mark"),
                cfg.key_label("unmark"),
                cfg.key_label("filter"),
                cfg.key_label("hide_shells"),
                cfg.key_label("sort"),
                cfg.key_label("settings"),
                cfg.key_label("hosts"),
                cfg.key_label("quit"),
            );
            if !cfg.commands.is_empty() {
                help.push_str(" custom:");
                for c in &cfg.commands {
                    help.push_str(&format!(" {}={}", c.key, c.name));
                }
            }
            self.set_status(help);
            return None;
        }
        // Type-to-filter (Panes focus): same rule as the Sessions
        // branch — unbound printable keystrokes start the filter.
        if let KeyCode::Char(c) = code
            && Self::is_typeable(&code, &mods)
        {
            self.open_filter_seeded(c);
            return None;
        }
        None
    }

    fn on_confirm_restart_key(&mut self, key: KeyEvent, tmux: &Tmux) -> Option<AppEvent> {
        let (id, command) = match &self.mode {
            Mode::ConfirmRestartPane { id, command } => (id.clone(), command.clone()),
            _ => return None,
        };
        match key.code {
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => self.cancel_modal(),
            KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                match tmux.respawn_pane(&id, &command) {
                    Ok(_) => {
                        self.set_status(format!("restarted {id}"));
                        self.refresh(tmux);
                    }
                    Err(e) => self.set_status(format!("restart failed: {e}")),
                }
                self.cancel_modal();
            }
            _ => {}
        }
        None
    }

    fn on_confirm_pane_key(&mut self, key: KeyEvent, tmux: &Tmux) -> Option<AppEvent> {
        let id = match &self.mode {
            Mode::ConfirmKillPane(id) => id.clone(),
            _ => return None,
        };
        match key.code {
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => self.cancel_modal(),
            KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                match tmux.kill_pane(&id) {
                    Ok(_) => {
                        self.set_status(format!("killed pane {id}"));
                        self.marked.remove(&id);
                        self.refresh(tmux);
                    }
                    Err(e) => self.set_status(format!("kill failed: {e}")),
                }
                self.cancel_modal();
            }
            _ => {}
        }
        None
    }

    /// Run a `[[command]]` against the selected row: expand the template,
    /// capture output (10s cap), show it in a scrollable popup. Blocks the
    /// TUI while running — custom commands should be quick queries.
    fn run_custom(&mut self, cmd: &crate::config::CustomCommand) {
        let pane = match self.selected_pane() {
            Some(p) => p.clone(),
            None => {
                self.set_status("no pane selected".to_string());
                return;
            }
        };
        // Templates expand local paths ({dir} is a remote path here).
        // Native rows are local tabs, so their paths expand fine.
        if is_remote_pane(&pane.id) && !is_native_pane(&pane.id) {
            self.set_status(format!("{}: remote rows run no local commands", cmd.name));
            return;
        }
        let rec = self.agents.get(&pane.id);
        let expanded = expand_command(&cmd.run, &pane, rec);
        match crate::automation::shell_capture(
            &expanded,
            &pane.id,
            rec,
            std::time::Duration::from_secs(10),
        ) {
            Ok(text) => {
                self.mode = Mode::CommandOutput {
                    name: cmd.name.clone(),
                    lines: text.lines().map(str::to_string).collect(),
                    scroll: 0,
                };
            }
            Err(e) => self.set_status(format!("{}: {e}", cmd.name)),
        }
    }

    fn on_command_output_key(&mut self, key: KeyEvent) -> Option<AppEvent> {
        let (len, scroll) = match &mut self.mode {
            Mode::CommandOutput { lines, scroll, .. } => (lines.len(), scroll),
            _ => return None,
        };
        match key.code {
            KeyCode::Esc | KeyCode::Enter => self.cancel_modal(),
            KeyCode::Up => *scroll = scroll.saturating_sub(1),
            KeyCode::Down => *scroll = (*scroll + 1).min(len.saturating_sub(1)),
            KeyCode::PageUp => *scroll = scroll.saturating_sub(10),
            KeyCode::PageDown => *scroll = (*scroll + 10).min(len.saturating_sub(1)),
            _ => {}
        }
        None
    }

    /// One confirmation for the whole marked set: kill each, keep going
    /// past failures, report the tally.
    fn on_confirm_marked_key(&mut self, key: KeyEvent, tmux: &Tmux) -> Option<AppEvent> {
        let ids = match &self.mode {
            Mode::ConfirmKillMarked(ids) => ids.clone(),
            _ => return None,
        };
        match key.code {
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => self.cancel_modal(),
            KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                let mut killed = 0;
                let mut failed = 0;
                let mut skipped_own = 0;
                for id in &ids {
                    // Never kill the pane hosting this TUI: a marked own
                    // pane would otherwise murder the messenger.
                    if self.own_pane.as_deref() == Some(id.as_str()) {
                        skipped_own += 1;
                        self.marked.remove(id);
                        continue;
                    }
                    match tmux.kill_pane(id) {
                        Ok(_) => {
                            killed += 1;
                            self.marked.remove(id);
                        }
                        Err(_) => failed += 1,
                    }
                }
                let mut msg = if failed == 0 {
                    format!("killed {killed} marked pane(s)")
                } else {
                    format!("killed {killed}, {failed} failed")
                };
                if skipped_own > 0 {
                    msg.push_str(", own pane skipped");
                }
                self.set_status(msg);
                self.refresh(tmux);
                self.cancel_modal();
            }
            _ => {}
        }
        None
    }
}

#[cfg(test)]
mod tests {
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
        let dir = std::env::temp_dir().join(format!("terhub-faketmux-{}", std::process::id()));
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
        let _scratch = ScratchConfig::new();
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
        let _scratch = ScratchConfig::new();
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
        let _scratch = ScratchConfig::new();
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
        app.status_since =
            Some(std::time::Instant::now() - App::STATUS_TTL - std::time::Duration::from_secs(1));
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
        // Unknown vars and escapes stay literal.
        let out = expand_command(
            "{bogus} {{pane}} {pane}",
            &Pane::test("%1", "w", "sh"),
            None,
        );
        assert_eq!(out, "{bogus} {pane} %1");
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
    fn wheel_scrolls_output_popup() {
        let mut app = app_with_panes();
        app.mode = Mode::CommandOutput {
            name: "out".to_string(),
            lines: (0..20).map(|i| format!("line {i}")).collect(),
            scroll: 10,
        };
        app.on_wheel(true);
        assert_eq!(output_scroll(&app), 7);
        app.on_wheel(false);
        assert_eq!(output_scroll(&app), 10);
        // Clamps at both ends.
        for _ in 0..20 {
            app.on_wheel(true);
        }
        assert_eq!(output_scroll(&app), 0);
        for _ in 0..30 {
            app.on_wheel(false);
        }
        assert_eq!(output_scroll(&app), 19);
    }

    #[test]
    fn wheel_moves_table_and_settings_selection() {
        let mut app = app_with_panes();
        app.focus = Focus::Panes;
        assert_eq!(app.pane_selected, 0);
        app.on_wheel(false);
        assert_eq!(app.pane_selected, 1);
        app.on_wheel(true);
        assert_eq!(app.pane_selected, 0);
        app.open_settings();
        assert_eq!(app.settings_selected, 0);
        app.on_wheel(false);
        assert_eq!(app.settings_selected, 1);
        app.on_wheel(true);
        assert_eq!(app.settings_selected, 0);
        // Prompts eat the wheel.
        app.open_filter();
        app.on_wheel(false);
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

    /// Point the config file at a scratch dir for persist tests. Holds a
    /// process-wide lock: two such tests must never interleave env swaps,
    /// or one could persist into the real config file.
    static SCRATCH_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct ScratchConfig {
        old: Option<String>,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl ScratchConfig {
        fn new() -> Self {
            let guard = SCRATCH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let dir = std::env::temp_dir().join(format!("terhub-test-{}", std::process::id()));
            std::fs::create_dir_all(dir.join("terhub")).unwrap();
            let old = std::env::var("XDG_CONFIG_HOME").ok();
            // SAFETY: lock-held and no other lib test reads XDG_CONFIG_HOME.
            unsafe { std::env::set_var("XDG_CONFIG_HOME", &dir) };
            ScratchConfig { old, _guard: guard }
        }
    }

    impl Drop for ScratchConfig {
        fn drop(&mut self) {
            // SAFETY: same confined var, restored before any other reader.
            unsafe {
                match &self.old {
                    Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                    None => std::env::remove_var("XDG_CONFIG_HOME"),
                }
            }
        }
    }

    #[test]
    fn settings_open_navigate_apply_persists() {
        let _scratch = ScratchConfig::new();
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
        let _scratch = ScratchConfig::new();
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
        let _scratch = ScratchConfig::new();
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
            },
        );
        let counts = app.session_counts();
        assert_eq!(counts.get("a"), Some(&(2, 1)));
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
        let _scratch = ScratchConfig::new();
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
    fn typing_unbound_key_opens_filter_seeded() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = app_with(&["a"]);
        let tmux = Tmux::new().unwrap();
        // `/` is unbound: opens the filter with `/` already typed.
        app.on_key(
            KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
            &tmux,
        );
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
        app.on_key(
            KeyEvent::new(KeyCode::Char('.'), KeyModifiers::NONE),
            &tmux,
        );
        assert_eq!(app.mode, Mode::PromptFilter);
        assert_eq!(app.input, ".");
    }

    #[test]
    fn bound_key_still_wins_over_typing() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = app_with_panes();
        let tmux = Tmux::new().unwrap();
        // `q` quits; it must never detour into the filter prompt.
        let ev = app.on_key(
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
            &tmux,
        );
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
        // no typing; the temporary search always starts empty.
        let cfg = AppConfig {
            filter: "!bash".to_string(),
            ..AppConfig::default()
        };
        let mut app = App::new(cfg);
        assert!(app.filter.is_empty());
        assert_eq!(app.exclude, "!bash");
        app.panes = app_with_panes().panes;
        app.refresh_filtered();
        assert_eq!(ids(&app), vec!["%1"]);
    }

    #[test]
    fn enter_in_search_never_persists() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        // Serialize with every other env-mutating lib test (hosts modal
        // resolves its fleet file through TERHUB_CONFIG too); the lock
        // is held for the whole mutation window.
        let _lock = SCRATCH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: lock-held; no other thread reads TERHUB_CONFIG now.
        let saved = std::env::var("TERHUB_CONFIG").ok();
        let dir = std::env::temp_dir().join(format!("terhub-search-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("search-nopersist.toml");
        std::fs::write(&path, "[ui]\ntheme = \"dark\"\n").unwrap();
        unsafe {
            std::env::set_var("TERHUB_CONFIG", &path);
        }
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
        assert!(!text.contains("filter = "), "search leaked into config:\n{text}");
        unsafe {
            match saved {
                Some(v) => std::env::set_var("TERHUB_CONFIG", v),
                None => std::env::remove_var("TERHUB_CONFIG"),
            }
        }
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn standing_editor_enter_persists_and_reloads() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let _lock = SCRATCH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: lock-held; no other thread reads TERHUB_CONFIG now.
        let saved = std::env::var("TERHUB_CONFIG").ok();
        let dir = std::env::temp_dir().join(format!("terhub-standing-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("standing-persist.toml");
        std::fs::write(&path, "[ui]\ntheme = \"dark\"\n").unwrap();
        unsafe {
            std::env::set_var("TERHUB_CONFIG", &path);
        }
        let mut app = app_with(&["a"]);
        let tmux = Tmux::new().unwrap();
        // The settings row opens the same prompt in standing mode.
        app.open_standing_editor();
        assert!(app.editing_standing);
        for c in "!zsh".chars() {
            app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE), &tmux);
        }
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tmux);
        assert_eq!(app.exclude, "!zsh");
        // Back in settings (where the editor was opened from), not the table.
        assert_eq!(app.mode, Mode::Settings);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("filter = \"!zsh\""), "{text}");
        // And the saved exclusion reloads through the normal loader.
        let (cfg, _) = crate::config::load_config_report(Some(path.clone()));
        assert_eq!(cfg.filter, "!zsh");
        unsafe {
            match saved {
                Some(v) => std::env::set_var("TERHUB_CONFIG", v),
                None => std::env::remove_var("TERHUB_CONFIG"),
            }
        }
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn view_seeds_sort_hide_and_exclude_from_config() {
        // The standing view is exactly what was saved: exclusion, sort
        // column + direction, and shell hiding all apply on first refresh.
        let cfg = AppConfig {
            filter: "!bash".to_string(),
            sort: "State".to_string(),
            sort_desc: false,
            hide_shells: true,
            ..AppConfig::default()
        };
        let app = App::new(cfg);
        assert_eq!(app.exclude, "!bash");
        assert_eq!(app.sort_key, SortKey::State);
        assert!(app.sort_asc);
        assert!(app.hide_quiet_shells);
        assert!(app.filter.is_empty());
    }

    #[test]
    fn sort_key_change_persists_view() {
        // ScratchConfig's guard serializes us against every other
        // env-mutating test (a concurrent TERHUB_CONFIG setter would
        // hijack config_path()); no second lock — std mutexes deadlock
        // on re-entry.
        let _scratch = ScratchConfig::new();
        let mut app = app_with_panes();
        let tmux = Tmux::new().unwrap();
        app.on_key(press(KeyCode::Char('o')), &tmux);
        assert_eq!(app.sort_key, SortKey::State);
        let text =
            std::fs::read_to_string(crate::config::config_path()).unwrap();
        assert!(text.contains("sort = \"State\""), "{text}");
        assert!(text.contains("sort_desc = false"), "{text}");
    }

    #[test]
    fn hide_shells_toggle_persists_view() {
        // Same serialization as above; ScratchConfig already holds it.
        let _scratch = ScratchConfig::new();
        let mut app = app_with_panes();
        let tmux = Tmux::new().unwrap();
        // `h` is bound by default; the toggle flips and persists.
        app.on_key(
            KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE),
            &tmux,
        );
        assert!(app.hide_quiet_shells);
        let text =
            std::fs::read_to_string(crate::config::config_path()).unwrap();
        assert!(text.contains("hide_shells = true"), "{text}");
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
        let mut app = app_with_panes();
        app.show_preview = true;
        app.preview = (0..100)
            .map(|i| format!("line {i:03}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.select_pane(1);
        app.on_wheel(false);
        assert_eq!(app.preview_scroll, 3);
        assert_eq!(app.pane_selected, 1);
        app.on_wheel(true);
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
        let _scratch = ScratchConfig::new();
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
}
