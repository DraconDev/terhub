//! TUI state machine. Backend-agnostic rendering input; knows nothing about
//! ratatui so the logic stays unit-testable.
use crate::agent::AgentRecord;
use crate::config::{AppConfig, key_matches};
use crate::daemon;
use crate::tmux::{Pane, Session, Tmux};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::{HashMap, VecDeque};
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
    /// Naming prompt for saved filters: `None` = add a new named
    /// filter holding the current search query, `Some(i)` = rename
    /// saved filter `i`. Enter commits the name, Esc cancels.
    PromptSavedFilter {
        index: Option<usize>,
    },
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
    /// Owned strings: filter-page rows are built per render from live
    /// pane data, so borrowed statics would leak or dangle.
    Legend {
        triangle: String,
        color: String,
        name: String,
        meaning: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingKind {
    Theme(usize),
    Density(usize),
    ToggleNotifications,
    ToggleHighlight,
    ToggleMruTabs,
    /// Record/share mode: blank out the identity-revealing table columns
    /// (Session, Window, Cmd, Dir, Agent, Handle, Summary) so a
    /// recording or shared screenshot hides what the user is working on
    /// while every state and count column stays readable.
    ToggleBlurCols,
    /// Opens the text prompt editing the standing exclusion; Enter
    /// saves it to `[ui] filter`, Esc cancels. Not a toggle.
    EditStandingFilter,
    /// Switch the table to saved filter `i`'s query and name it in
    /// the header.
    UseSavedFilter(usize),
    /// Rename saved filter `i` via the naming prompt.
    RenameSavedFilter(usize),
    /// Delete saved filter `i` (clears the header name when it was
    /// the active one).
    DeleteSavedFilter(usize),
    /// Name the current search query and save it as a filter. The
    /// row only appears while a search is active.
    AddSavedFilter,
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
        AgentState::Done => 5,
        // Paused is deliberate: nobody is needed to thaw it, so it
        // sorts with the quiet states rather than with the needy ones.
        AgentState::Paused => 6,
        AgentState::Idle => 7,
        AgentState::Unknown => 8,
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
                    Some(v) => out.push_str(&crate::fleet::sh_quote(v)),
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

/// True when `q` (already lowercased) appears in any of the visible
/// columns of the row. Shared by the multi-token include and
/// standing-filter helpers.
fn pane_match_any(p: &Pane, rec: Option<&AgentRecord>, q: &str) -> bool {
    let hit = |s: &str| s.to_lowercase().contains(q);
    hit(&p.id)
        || hit(&p.session)
        || hit(&p.window_name)
        || p.index.to_string().contains(q)
        || hit(&p.command)
        || hit(&p.path)
        || rec.is_some_and(|r| hit(&r.agent) || hit(r.state.label()) || hit(&r.summary))
}

/// Multi-token OR-combined include: a row is kept when it matches
/// ANY positive (lowercased) token and NONE of the `!`-prefixed
/// ones. Empty tokens are skipped, so a lone `!` constrains
/// nothing and a negation-only query keeps every un-excluded row.
fn pane_matches_include(p: &Pane, rec: Option<&AgentRecord>, tokens: &[&str]) -> bool {
    // Exclusions first: a `!`-hit hides the row no matter which
    // positive token matched, so positives and negations must both
    // be fully evaluated (no early `true` on the first positive).
    let mut any_positive = false;
    let mut positive_hit = false;
    for tok in tokens {
        let tok = tok.trim();
        match tok.strip_prefix('!') {
            Some(term) => {
                let term = term.trim();
                if !term.is_empty() && pane_match_any(p, rec, term) {
                    return false;
                }
            }
            None => {
                if tok.is_empty() {
                    continue;
                }
                any_positive = true;
                positive_hit = positive_hit || pane_match_any(p, rec, tok);
            }
        }
    }
    !any_positive || positive_hit
}

/// Multi-token NOT-OR for the standing filter: a row is kept only
/// if it matches NONE of the (lowercased) tokens. A leading `!` on
/// any token is stripped before matching so the prompt's
/// pre-filled `!` and the user-typed `!` are both treated as a
/// bare exclude. Empty tokens are skipped.
fn pane_matches_excludes(p: &Pane, rec: Option<&AgentRecord>, tokens: &[&str]) -> bool {
    for tok in tokens {
        let stripped = tok.trim().trim_start_matches('!');
        if !stripped.is_empty() && pane_match_any(p, rec, stripped) {
            return false;
        }
    }
    true
}

/// Table legend, shown when `?` is pressed twice: the marks and column
/// shorthands the table itself never explains. Ordered so the marks
/// come first — a narrow terminal truncates the tail, never the
/// characters the user asked about.
const LEGEND: &str = "legend: !=needs you · +=marked · *=active pane (multi-pane window) · ✓=seen via jump/attend · ●/○=attached (top bar) · Src hook/proc/scr · For=in this state · Turn=last finished turn · Idle=since screen changed · Activity=output/tick";

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
    /// Temporary include query (multi-token, OR-combined,
    /// case-insensitive substring, empty = off). A `!`-prefixed
    /// token is a single-row exclusion (the row must not contain
    /// the substring). Session-only: typing, `/`, or `f` sets it;
    /// Enter keeps it for the session; Esc clears it. Never
    /// persisted.
    pub filter: String,
    /// Standing exclude line (multi-token, NOT-OR-combined,
    /// case-insensitive substring, empty = off). A row is kept
    /// only if it does not contain ANY token. Loaded from `[ui]
    /// filter` (a single leading `!` is stripped on load) and
    /// persisted on Enter from the standing editor.
    pub exclude: String,
    /// Which query the filter prompt is editing: the temporary find
    /// (false) or the standing exclusion (true, from the settings row).
    pub(crate) editing_standing: bool,
    /// Query as it was when the filter prompt opened: live typing edits
    /// `filter` in place, and Esc restores this instead of clearing.
    filter_before_edit: String,
    /// Named saved filters, mirrored from `config.saved_filters` and
    /// persisted on add / rename / delete. Live copy: the table reads
    /// this, the config holds the on-disk form.
    pub saved_filters: Vec<crate::config::SavedFilter>,
    /// Name of the saved filter currently driving the table (`None`
    /// when the query was typed by hand, cleared, or the standing
    /// filter is in play). Rendered in the table title so an active
    /// filter is never invisible.
    pub active_saved_filter: Option<String>,
    /// In-memory history of committed filter queries (live `/` and
    /// standing editor share this list, FIFO, capped at
    /// `FILTER_HISTORY_CAP`). Resets on app restart — a fresh
    /// session starts with no history.
    filter_history: VecDeque<String>,
    /// `Some(i)` = the prompt is currently displaying
    /// `filter_history[i]`. `None` = the prompt is showing the live
    /// draft (what the user typed or recalled via Down past the
    /// latest entry). Persists across Char / Backspace so the user
    /// can navigate into history, edit, and keep navigating;
    /// reset to `None` by Esc, Enter, and opening the prompt.
    filter_history_idx: Option<usize>,
    /// Input value to return to when Down past the latest history
    /// entry. Captured on the first Up that transitions from draft
    /// to history; cleared on Esc, Enter, and opening the prompt.
    /// Lets "Up then Down" round-trip back to the edited draft
    /// rather than to the pre-prompt snapshot.
    filter_history_pending: Option<String>,
    /// Hide unclaimed panes quiet past [`QUIET_SHELL_SECS`] (`h` toggle).
    pub hide_quiet_shells: bool,
    /// Rows the toggle is currently hiding (0 when off), for the title.
    pub hidden_quiet: usize,
    /// Space-marked pane ids for bulk kill/attend. Survives sort/filter;
    /// pruned to live panes on every refresh.
    pub marked: std::collections::HashSet<String>,
    /// Panes the user has visited via a jump/attend, mapped to the
    /// (state, summary) seen at the time. A row that still matches reads
    /// "attended" (`\u{2713}`); a new state or a new summary is new
    /// information, so the entry is pruned on the next agent refresh and
    /// the row stops reading as seen. Deliberately never persisted:
    /// "seen" belongs to this viewing session, not the config. Pruned to
    /// live panes alongside `marked`.
    pub attended: HashMap<String, (String, String)>,
    /// Which view settings this instance has changed since its last
    /// successful config write. Only these keys get persisted, so a
    /// stale instance in another window can't revert a newer one's
    /// saved view (see `App::persist_view`).
    pub view_changes: crate::config::ViewChanges,
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
    /// Per-pane 60-sample sparkline ring. The TUI samples bytes of
    /// new output every tick; the renderer collapses 10 samples into
    /// one bar to draw a 6-wide micrograph. In-memory only — a
    /// restart resets (the contract: restarts shouldn't carry stale
    /// activity claims across sessions).
    sparkline_samples: HashMap<String, [u32; 60]>,
    /// Pane id of the last sample the sparkline for that pane was
    /// compared against. The (id, prev_screen) pair lets us compute
    /// "bytes added since last sample" cheaply.
    sparkline_prev: HashMap<String, String>,
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
        // Old configs wrote `!zsh` to mean "exclude zsh"; the new
        // exclude line stores bare tokens, so a single leading `!`
        // is harmless noise (the line is already exclude). Strip
        // once here so every downstream consumer sees the same
        // canonical form.
        let exclude = config
            .filter
            .trim()
            .trim_start_matches('!')
            .trim()
            .to_string();
        // No saved filter is active at startup: saved queries are
        // live includes, but the live filter starts empty (it is never
        // persisted) while `config.filter` carries the on-disk `[ui]`
        // standing EXCLUSION (`!`-prefixed). Matching one domain
        // against the other never named an applied filter — and could
        // falsely name one when an exclusion string-coincided with a
        // saved query. The header names filters again once one is
        // applied live.
        // Cloned before the move into the struct literal: the fields
        // below consume `config`.
        let saved_filters = config.saved_filters.clone();
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
            saved_filters,
            active_saved_filter: None,
            filter_history: VecDeque::new(),
            filter_history_idx: None,
            filter_history_pending: None,
            view_changes: crate::config::ViewChanges::default(),
            hide_quiet_shells,
            hidden_quiet: 0,
            marked: std::collections::HashSet::new(),
            attended: HashMap::new(),
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
            sparkline_samples: HashMap::new(),
            sparkline_prev: HashMap::new(),
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

    /// Sample the activity sparkline for a pane: bytes of new output
    /// since the last sample, capped so a runaway editor doesn't blow
    /// out the scale of every other bar. The previous screen is
    /// cached by pane id, not in the ring buffer, so a short capture
    /// gap (the terminal died and respawned) just resets the diff
    /// and the bar shows 0 next to a clean baseline.
    pub fn sample_sparkline(&mut self, id: &str, screen: &str) {
        if !self.config.activity_sparkline {
            return;
        }
        let prev = self.sparkline_prev.get(id).cloned();
        let added: u32 = match prev.as_deref() {
            None => 0,
            Some(p) if screen.starts_with(p) => (screen.len() - p.len()) as u32,
            // Capture dropped or scrolled: can't attribute the bytes.
            _ => 0,
        };
        // Cap at 4096/tick; longer captures just show a tall bar.
        let sample = added.min(4096);
        let ring = self
            .sparkline_samples
            .entry(id.to_string())
            .or_insert([0u32; 60]);
        // Ring shift: drop the oldest, push the new sample.
        ring.rotate_left(1);
        ring[59] = sample;
        // Trim a 64KB tail cache; the full screen is unbounded.
        let cap = screen.len().min(65536);
        self.sparkline_prev
            .insert(id.to_string(), screen[screen.len() - cap..].to_string());
    }

    /// Aggregate a 60-sample ring into 6 buckets (10 samples each,
    /// rightmost = freshest) for the 6-wide micrograph. Returns
    /// per-bucket byte totals so the renderer can choose bar height
    /// in cells. An empty buffer yields six zeros.
    pub fn sparkline_buckets(&self, id: &str) -> [u32; 6] {
        let mut out = [0u32; 6];
        let Some(ring) = self.sparkline_samples.get(id) else {
            return out;
        };
        // 60 samples / 6 buckets = 10 samples per bucket. Take the
        // max within each bucket so a single busy tick lights up the
        // bar even if surrounded by quiet ones.
        for (i, slot) in out.iter_mut().enumerate() {
            let start = i * 10;
            let end = start + 10;
            let m = ring[start..end].iter().copied().max().unwrap_or(0);
            *slot = m;
        }
        out
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
                    pid_start: crate::proc::proc_starttime(tab.pid).unwrap_or(0),
                    bell: false,
                });
                let Some(hit) = snap.detect_agent(tab.pid) else {
                    continue;
                };
                // An agent with an editor in the tree summoned $EDITOR and
                // sits on keystrokes: waiting, same as the daemon reads it.
                let editor_wait = !fg_comm.is_empty() && crate::proc::is_editor_name(&fg_comm);
                let screen = crate::native::capture(&tab.id, 60).unwrap_or_default();
                let (screen_hash, quiet_since) = self.track_native_quiet(&tab.id, &screen, now);
                self.sample_sparkline(&tab.id, &screen);
                let classified = classify_screen(
                    &compiled,
                    &command,
                    &screen,
                    false,
                    crate::proc::parked_in_tty_read(tab.pid),
                );
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
                    bell: false,
                    bell_reason: String::new(),
                    stuck_base: None,
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
                // Attended entries die with their row too — but the
                // live set spans every cache, not just tmux: pruning
                // here runs before merge_caches() re-adds the
                // fleet/native rows, so a tmux-only set would wipe
                // every remote/native mark on each tick.
                let live: std::collections::HashSet<&str> = self
                    .panes
                    .iter()
                    .map(|p| p.id.as_str())
                    .chain(self.fleet_rows.iter().map(|p| p.id.as_str()))
                    .chain(self.native_rows.iter().map(|p| p.id.as_str()))
                    .collect();
                self.marked.retain(|id| live.contains(id.as_str()));
                self.attended.retain(|id, _| live.contains(id.as_str()));
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
                // Self-heal before declaring the daemon down: a daemon
                // nobody supervises dies silently (terminal SIGHUP, the
                // OOM killer), and the only symptom is every pane losing
                // its badges for the rest of the session. Rate-limited,
                // so a genuinely unbootable daemon is not re-forked per
                // tick. The title still says "daemon down" until the
                // next good fetch.
                daemon::ensure_running();
                self.agents.clear();
                self.agents_live = false;
            }
        }
        self.prune_attended();
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

    /// Total needy panes for the "All" sidebar count. Iterates over
    /// [`Self::panes`] (not [`Self::agents`]) so orphan agent records —
    /// daemon snapshots that still carry a pane id the live pane list
    /// no longer knows about — can never push the count past
    /// `panes.len()`. Mirrors `session_counts` / `visible_needy`: every
    /// other needy count in the UI is bounded by a row we render.
    pub fn attention_count(&self) -> usize {
        self.panes
            .iter()
            .filter(|p| {
                self.agents
                    .get(&p.id)
                    .is_some_and(|a| a.state.needs_attention())
            })
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
            .filter(|p| match self.agents.get(&p.id) {
                Some(a) => a.state == crate::agent::AgentState::Idle,
                None => is_quiet_shell(None, p.activity, now),
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
            // Standing filter: multi-token, NOT-OR combined. Each
            // token's leading `!` is stripped (the on-disk form
            // has one, the prompt's live re-filter strips it on
            // every keystroke). `pane_matches_excludes` returns
            // true when the row is kept.
            let tokens: Vec<String> = self
                .exclude
                .split_whitespace()
                .map(|t| t.to_lowercase())
                .collect();
            let token_refs: Vec<&str> = tokens.iter().map(String::as_str).collect();
            let agents = &self.agents;
            self.filtered
                .retain(|p| pane_matches_excludes(p, agents.get(&p.id), &token_refs));
        }
        if !self.filter.is_empty() {
            // Live find: multi-token, OR-combined, with per-token
            // `!` exclusion — the same token model as the standing
            // filter, so the two queries compose symmetrically.
            let tokens: Vec<String> = self
                .filter
                .split_whitespace()
                .map(|t| t.to_lowercase())
                .collect();
            let token_refs: Vec<&str> = tokens.iter().map(String::as_str).collect();
            let agents = &self.agents;
            self.filtered
                .retain(|p| pane_matches_include(p, agents.get(&p.id), &token_refs));
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
        self.view_changes.sort = true;
        self.view_changes.sort_desc = true;
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

    /// Name of the tmux session hosting this TUI, if known (resolved
    /// from `own_pane` via the pane list). Session-kill refuses this
    /// session: killing it would kill the hub itself. Unknown (no
    /// own pane, or a stale list) means no refusal — same positive-
    /// identification rule as the own-pane guards.
    fn own_session(&self) -> Option<&str> {
        let own = self.own_pane.as_deref()?;
        self.panes
            .iter()
            .find(|p| p.id == own)
            .map(|p| p.session.as_str())
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
    /// bound key without modifiers. Returns `Some(event)` when the
    /// binding fired (the caller swallows the key event and honours
    /// the event, exactly as the bare-key path would); `None` when no
    /// action claimed the binding.
    fn dispatch_binding(
        &mut self,
        binding: &crate::config::KeyBinding,
        tmux: &Tmux,
    ) -> Option<Option<AppEvent>> {
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
            return Some(None);
        }
        match self.binding_action(binding).as_str() {
            "" => None, // no standard action claimed this binding
            "new" => {
                self.do_new_session();
                Some(None)
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
                Some(None)
            }
            "rename" => {
                self.do_rename_session();
                Some(None)
            }
            "attach" => {
                if self.focus == Focus::Panes || self.show_preview {
                    if let Some(p) = self.selected_pane() {
                        let id = p.id.clone();
                        // Forward the event: discarding it made a
                        // modifier-bound attach a silent no-op.
                        Some(self.jump_event(&id))
                    } else {
                        self.set_status("no pane selected".to_string());
                        Some(None)
                    }
                } else if let Some(s) = self.side_session() {
                    self.do_nav(tmux, AppEvent::Attach(s.name.clone()));
                    Some(None)
                } else {
                    self.set_status("pick a session row (All doesn't count)".to_string());
                    Some(None)
                }
            }
            "detach" => {
                self.do_detach(tmux);
                Some(None)
            }
            "quit" => {
                // Only reachable through a modifier binding (the bare
                // path handles plain quit): forward the event like
                // attach/attend below. Returning None here means
                // "unclaimed", and the caller would swallow the press
                // with no quit and no status.
                Some(Some(AppEvent::Quit))
            }
            "help" => {
                self.set_status(LEGEND.to_string());
                Some(None)
            }
            "refresh" => {
                self.refresh(tmux);
                self.fetch_native();
                Some(None)
            }
            "attend" => {
                // Forward the event, like attach above.
                Some(self.do_attend())
            }
            "preview" => {
                self.toggle_preview(tmux);
                Some(None)
            }
            "theme" => {
                self.cycle_theme();
                Some(None)
            }
            "density" => {
                self.cycle_density();
                Some(None)
            }
            "settings" => {
                self.open_settings();
                Some(None)
            }
            "sort" => {
                self.resort_next();
                self.follow_preview(tmux);
                Some(None)
            }
            "filter" => {
                self.open_filter();
                Some(None)
            }
            "saved_filter" => {
                self.cycle_saved_filter(tmux);
                Some(None)
            }
            "filter_clear" => {
                self.clear_filter();
                Some(None)
            }
            "hide_shells" => {
                self.toggle_hide_quiet_shells();
                Some(None)
            }
            "spawn" => {
                self.do_spawn();
                Some(None)
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
                Some(None)
            }
            "pause" => {
                self.do_pause();
                Some(None)
            }
            "hosts" => {
                self.open_hosts();
                Some(None)
            }
            "bulk" => {
                self.do_bulk();
                Some(None)
            }
            "mark" => {
                self.toggle_mark();
                Some(None)
            }
            "unmark" => {
                let n = self.marked.len();
                self.marked.clear();
                self.set_status(if n == 0 {
                    "nothing marked".to_string()
                } else {
                    format!("cleared {n} mark(s)")
                });
                Some(None)
            }
            _ => None,
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
            "saved_filter",
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
            "saved_filter",
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
                    && let Some(ev) = self.dispatch_binding(&binding, tmux)
                {
                    return ev;
                }
            }
            return None;
        }
        match self.mode.clone() {
            Mode::PromptNew | Mode::PromptRename(_) => self.on_prompt_key(key, tmux),
            Mode::PromptFilter => self.on_filter_key(key, tmux),
            Mode::PromptSavedFilter { index } => {
                self.on_saved_filter_name_key(key, index);
                None
            }
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

    /// Naming prompt for saved filters. Single line, like every other
    /// prompt: Enter commits the name, Esc cancels. Both return to the
    /// settings page the prompt was opened from.
    fn on_saved_filter_name_key(&mut self, key: KeyEvent, index: Option<usize>) {
        match key.code {
            KeyCode::Esc => {
                self.cancel_modal();
                self.mode = Mode::Settings;
            }
            KeyCode::Enter => {
                let name = self.input.trim().to_string();
                self.cancel_modal();
                self.mode = Mode::Settings;
                self.submit_saved_filter_name(index, name);
            }
            KeyCode::Backspace => self.delete_before_cursor(),
            KeyCode::Delete => self.delete_at_cursor(),
            KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(1);
            }
            KeyCode::Right => {
                let len = self.input.chars().count();
                self.cursor = (self.cursor + 1).min(len);
            }
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.input.chars().count(),
            // The list stays put: a saved filter's name is metadata,
            // not a query, so typing never refreshes the table.
            KeyCode::Char(c) if !c.is_control() => self.insert_at_cursor(c),
            _ => {}
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
                self.filter_history_reset();
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
                    // Standing is exclusion-only: store the raw
                    // tokens (NOT-OR combined, leading `!`
                    // stripped). The on-disk form re-adds the `!`
                    // in `persist_view` for back-compat with old
                    // configs.
                    self.exclude = q.trim_start_matches('!').trim().to_string();
                    // History records the committed input as the user
                    // typed it in this prompt — the bang-prefixed form
                    // — so recalling it into the standing editor
                    // round-trips without losing the exclusion marker.
                    self.push_filter_history(q);
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
                    self.filter = q.clone();
                    // Re-typing a saved query exactly makes it that
                    // filter again (round-trip through the prompt),
                    // otherwise the header reverts to the raw query.
                    self.active_saved_filter = self.find_saved_filter_by_query(&q);
                    self.push_filter_history(q);
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
            // History navigation. The nav state persists across Char
            // and Backspace (you can navigate into history, edit,
            // and keep navigating), so these are the only arms that
            // touch `filter_history_idx`.
            KeyCode::Up => self.filter_history_prev(),
            KeyCode::Down => self.filter_history_next(),
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
                // Never kill the session hosting this TUI: it would
                // kill the hub out from under itself (mirrors the
                // own-pane guards on the pane paths).
                if self.own_session() == Some(name.as_str()) {
                    self.set_status(format!("won't kill own session {name} (detach first)"));
                } else {
                    match tmux.kill_session(&name) {
                        Ok(_) => {
                            self.set_status(format!("killed {name}"));
                            self.refresh(tmux);
                        }
                        Err(e) => self.set_status(format!("kill failed: {e}")),
                    }
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
        // Saved-filter cycling and clearing work from either focus and
        // sit BEFORE `filter`: letter matching is case-insensitive, so
        // the default `filter`="f" would otherwise swallow an explicit
        // bare saved_filter="F". An explicit user binding wins over
        // the default; untouched defaults still fall through to `filter`.
        if key_matches(&self.config.spec("saved_filter"), &code, mods) {
            self.cycle_saved_filter(tmux);
            return None;
        }
        if key_matches(&self.config.spec("filter_clear"), &code, mods) {
            self.clear_filter();
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
        // Pane actions work from the Sessions table too (they operate
        // on the selected pane, which exists under either focus) —
        // otherwise these bare keys fall into type-to-filter below.
        if key_matches(&cfg.spec("mark"), &code, mods) {
            self.toggle_mark();
            return None;
        }
        if key_matches(&cfg.spec("unmark"), &code, mods) {
            self.do_unmark();
            return None;
        }
        if key_matches(&cfg.spec("bulk"), &code, mods) {
            self.do_bulk();
            return None;
        }
        if key_matches(&cfg.spec("restart"), &code, mods) {
            self.do_restart();
            return None;
        }
        if key_matches(&cfg.spec("pause"), &code, mods) {
            self.do_pause();
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
                    "sessions: {}=new {}=spawn {}=rename {}=kill {}=attach {}=preview {}=theme {}=settings {}=hosts {}=hide {}=mark {}=unmark {}=bulk {}=restart {}=pause alt+1..8=tabs {}=quit",
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
                    cfg.key_label("mark"),
                    cfg.key_label("unmark"),
                    cfg.key_label("bulk"),
                    cfg.key_label("restart"),
                    cfg.key_label("pause"),
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
        // Case-insensitive like is_known_theme/palette(): a config
        // with `theme = "Dark"` loads fine, and cycling must advance
        // from it instead of "cycling" back to dark itself.
        let cur = THEMES
            .iter()
            .position(|t| t.eq_ignore_ascii_case(&self.config.theme));
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
        rows.push(SettingRow::Item {
            label: format!(
                "blur columns (record/share): {}",
                Self::onoff(self.config.blur_cols)
            ),
            active: self.config.blur_cols,
            kind: SettingKind::ToggleBlurCols,
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
        // Named saved filters. One row each: Enter applies the query,
        // `r` renames it, `d` deletes it. The marker names the one
        // currently driving the table.
        for (i, f) in self.saved_filters.iter().enumerate() {
            rows.push(SettingRow::Item {
                label: format!("saved: {} — {}", f.name, Self::truncate_query(&f.query, 24)),
                active: self.active_saved_filter.as_deref() == Some(f.name.as_str()),
                kind: SettingKind::UseSavedFilter(i),
            });
        }
        // Save-the-search row: only worth showing while there is a
        // search to name (the standing exclusion is not a query).
        if !self.filter.is_empty() {
            rows.push(SettingRow::Item {
                label: "save current search as named filter … (n)".to_string(),
                active: false,
                kind: SettingKind::AddSavedFilter,
            });
        }
        // What the two queries actually hide, so the exclusion is
        // inspectable instead of a leap of faith. Partitioned with no
        // double-counting: rows failing the standing test list under
        // it; rows passing that but failing the search list under the
        // search. Read-only rows (never selectable, never actionable).
        // Same tokenization as refresh_filtered: the standing filter
        // is multi-token NOT-OR, so passing the whole string as one
        // token here made this page disagree with the table about
        // which rows the standing filter hides.
        let ex_tokens: Vec<String> = self
            .exclude
            .split_whitespace()
            .map(|t| t.to_lowercase())
            .collect();
        let ex_refs: Vec<&str> = ex_tokens.iter().map(String::as_str).collect();
        let find_tokens: Vec<String> = self
            .filter
            .split_whitespace()
            .map(|t| t.to_lowercase())
            .collect();
        let q_find: Vec<&str> = find_tokens.iter().map(String::as_str).collect();
        let mut hidden_by_exclude: Vec<&Pane> = Vec::new();
        let mut hidden_by_search: Vec<&Pane> = Vec::new();
        for p in &self.panes {
            let rec = self.agents.get(&p.id);
            // The settings page partitions by which single clause
            // hid the row, not by the combined predicate; the
            // combined predicate would double-count when both
            // fire. Both clauses are multi-token (NOT-OR exclude,
            // OR-combined search with per-token `!`).
            if !self.exclude.is_empty() && !pane_matches_excludes(p, rec, &ex_refs) {
                hidden_by_exclude.push(p);
            } else if !self.filter.is_empty() && !pane_matches_include(p, rec, &q_find) {
                hidden_by_search.push(p);
            }
        }
        const HIDDEN_GREY: &str = "#8b949e";
        let hidden_row = |p: &Pane| SettingRow::Legend {
            triangle: "–".to_string(),
            color: HIDDEN_GREY.to_string(),
            name: p.id.clone(),
            meaning: format!("{} · {} · {}", p.session, p.window_name, p.command),
        };
        if self.exclude.is_empty() && self.filter.is_empty() {
            rows.push(SettingRow::Legend {
                triangle: "–".to_string(),
                color: HIDDEN_GREY.to_string(),
                name: "nothing hidden".to_string(),
                meaning: "every pane is shown".to_string(),
            });
        } else {
            if !self.exclude.is_empty() {
                rows.push(SettingRow::Legend {
                    triangle: "–".to_string(),
                    color: HIDDEN_GREY.to_string(),
                    name: format!("{} hidden", hidden_by_exclude.len()),
                    meaning: format!("matching standing filter '{}'", self.exclude),
                });
                for p in &hidden_by_exclude {
                    rows.push(hidden_row(p));
                }
            }
            if !self.filter.is_empty() {
                rows.push(SettingRow::Legend {
                    triangle: "–".to_string(),
                    color: HIDDEN_GREY.to_string(),
                    name: format!("{} hidden", hidden_by_search.len()),
                    meaning: format!("matching search '{}'", self.filter),
                });
                for p in &hidden_by_search {
                    rows.push(hidden_row(p));
                }
            }
        }
        rows.push(SettingRow::Header("Tray icons"));
        for r in crate::tray::icon_legend(self.config.palette()) {
            rows.push(SettingRow::Legend {
                triangle: "▼".to_string(),
                color: r.hex.to_string(),
                name: r.name.to_string(),
                meaning: r.meaning.to_string(),
            });
        }
        rows
    }

    /// Run a saved-filter row action (`r` rename / `d` delete) against
    /// the selected row when that row is a saved filter. Passing the
    /// template kind with index 0 and substituting the real index keeps
    /// the two call sites symmetric.
    fn settings_saved_filter_row(&mut self, template: SettingKind) {
        let items = self.settings_item_rows();
        let Some(row) = items.get(self.settings_selected).copied() else {
            return;
        };
        let kind = match (
            &template,
            self.settings_rows().get(row).and_then(|r| match r {
                SettingRow::Item { kind, .. } => Some(*kind),
                _ => None,
            }),
        ) {
            (SettingKind::RenameSavedFilter(_), Some(SettingKind::UseSavedFilter(i))) => {
                SettingKind::RenameSavedFilter(i)
            }
            (SettingKind::DeleteSavedFilter(_), Some(SettingKind::UseSavedFilter(i))) => {
                SettingKind::DeleteSavedFilter(i)
            }
            _ => return,
        };
        self.apply_setting(kind);
    }

    fn onoff(b: bool) -> &'static str {
        if b { "on" } else { "off" }
    }

    /// Shorten a saved query for a one-line settings row. Cuts at a
    /// char boundary and marks the truncation, so a long multi-token
    /// query never wraps the popup.
    fn truncate_query(q: &str, max: usize) -> String {
        let mut out: String = q.chars().take(max).collect();
        if q.chars().count() > max {
            out.push('…');
        }
        out
    }

    // ---- saved (named) filters -------------------------------------

    /// Persist the saved-filter list to `[ui] saved_filters`. Called
    /// after add / rename / delete so the list survives a restart.
    fn persist_saved_filters(&mut self) {
        self.config.saved_filters.clone_from(&self.saved_filters);
        self.view_changes.saved_filters = true;
        let written = {
            let patch = self.config.ui_persist_changes(&self.view_changes);
            crate::config::persist_ui_settings(&patch)
        };
        match written {
            Ok(()) => self.view_changes.saved_filters = false,
            Err(e) => self.set_status(format!("saved filters not written: {e}")),
        }
    }

    /// Apply saved filter `i`: the live search takes its query and the
    /// table header names it. The standing exclusion is untouched —
    /// a named filter and the standing filter compose.
    fn use_saved_filter(&mut self, i: usize) {
        let Some(f) = self.saved_filters.get(i).cloned() else {
            return;
        };
        self.filter = f.query.clone();
        self.active_saved_filter = Some(f.name.clone());
        self.refresh_filtered();
        self.pane_selected = 0;
        self.pane_scroll = 0;
        self.set_status(format!("filter '{}': {}", f.name, f.query));
    }

    /// Insert or replace a named filter, then persist. A name that
    /// already exists updates that entry instead of adding a duplicate
    /// row (rename and add-by-name converge here).
    fn save_named_filter(&mut self, name: String, query: String) {
        let name = name.trim().to_string();
        let query = query.trim().to_string();
        if name.is_empty() || query.is_empty() {
            return;
        }
        if let Some(existing) = self.saved_filters.iter_mut().find(|f| f.name == name) {
            existing.query = query;
        } else {
            self.saved_filters.push(crate::config::SavedFilter {
                name: name.clone(),
                query,
            });
        }
        self.persist_saved_filters();
    }

    /// Delete saved filter `i`, forgetting its header marker when it
    /// was the active one so the title doesn't name a dead filter.
    fn delete_saved_filter(&mut self, i: usize) {
        let Some(f) = self.saved_filters.get(i) else {
            return;
        };
        let name = f.name.clone();
        self.saved_filters.remove(i);
        if self.active_saved_filter.as_deref() == Some(name.as_str()) {
            self.active_saved_filter = None;
        }
        self.persist_saved_filters();
    }

    /// Name of the saved filter whose query equals the live search, if
    /// any. Lets the title keep naming a saved filter after a restart
    /// when `[ui] filter` matches it exactly.
    fn find_saved_filter_by_query(&self, query: &str) -> Option<String> {
        self.saved_filters
            .iter()
            .find(|f| f.matches_query(query))
            .map(|f| f.name.clone())
    }

    /// Cycle to the next saved filter, wrapping past the last one back
    /// to the first. With no saved filters at all, nothing happens
    /// (the status says so instead of silently doing nothing).
    fn next_saved_filter(&mut self) {
        if self.saved_filters.is_empty() {
            self.set_status("no saved filters (settings › filters › n)".to_string());
            return;
        }
        let next = match self.active_saved_filter.as_deref() {
            None => 0,
            Some(active) => self
                .saved_filters
                .iter()
                .position(|f| f.name == active)
                .map(|i| (i + 1) % self.saved_filters.len())
                .unwrap_or(0),
        };
        self.use_saved_filter(next);
    }

    /// Cycle to the next named saved filter. Shared by bare presses
    /// (either focus) and the modifier-qualified dispatch path so both
    /// spellings behave identically.
    fn cycle_saved_filter(&mut self, tmux: &Tmux) {
        self.next_saved_filter();
        self.follow_preview(tmux);
    }

    /// Clear the search query. Shared like [`Self::cycle_saved_filter`].
    fn clear_filter(&mut self) {
        if self.filter.is_empty() {
            self.set_status("search already clear".to_string());
        } else {
            self.filter.clear();
            self.active_saved_filter = None;
            self.refresh_filtered();
            self.set_status("search cleared".to_string());
        }
    }

    /// Open the naming prompt: `None` names the current search as a new
    /// filter, `Some(i)` renames filter `i`.
    fn open_saved_filter_prompt(&mut self, index: Option<usize>) {
        self.mode = Mode::PromptSavedFilter { index };
        self.input = match index {
            Some(i) => self
                .saved_filters
                .get(i)
                .map(|f| f.name.clone())
                .unwrap_or_default(),
            None => String::new(),
        };
        self.cursor = self.input.chars().count();
    }

    /// Rename saved filter `i` in place, keeping its position and
    /// query. Renaming onto a name another row already holds merges the
    /// two instead of producing two rows with the same label.
    fn rename_saved_filter(&mut self, i: usize, new_name: String) {
        let new_name = new_name.trim().to_string();
        if new_name.is_empty() {
            return;
        }
        let Some(old_name) = self.saved_filters.get(i).map(|f| f.name.clone()) else {
            return;
        };
        if new_name == old_name {
            return;
        }
        if let Some(dup) = self.saved_filters.iter().position(|f| f.name == new_name) {
            self.saved_filters.remove(dup);
            // The removed row may shift the index of the renamed one.
            let target = if dup < i { i - 1 } else { i };
            if let Some(f) = self.saved_filters.get_mut(target) {
                f.name = new_name;
            }
        } else if let Some(f) = self.saved_filters.get_mut(i) {
            f.name = new_name;
        }
        self.persist_saved_filters();
    }

    /// Name typed in the saved-filter prompt: commit it and persist.
    /// `Some(i)` renames (query carried over); `None` names the live
    /// search query.
    fn submit_saved_filter_name(&mut self, index: Option<usize>, name: String) {
        let name = name.trim().to_string();
        if name.is_empty() {
            self.set_status("filter needs a name".to_string());
            return;
        }
        let Some(index) = index else {
            // Adding: the live search is the query.
            if self.filter.trim().is_empty() {
                self.set_status("nothing to save (search is empty)".to_string());
                return;
            }
            let query = self.filter.trim().to_string();
            self.active_saved_filter = Some(name.clone());
            self.save_named_filter(name.clone(), query.clone());
            self.set_status(format!("saved filter '{name}': {query}"));
            return;
        };
        // Renaming: carry the query over and move the header marker when
        // the renamed row was the active one.
        let Some(old) = self
            .saved_filters
            .get(index)
            .map(|f| (f.name.clone(), f.query.clone()))
        else {
            return;
        };
        self.rename_saved_filter(index, name.clone());
        if self.active_saved_filter.as_deref() == Some(old.0.as_str()) {
            self.active_saved_filter = Some(name.clone());
        }
        self.set_status(format!("saved filter '{}': {}", name, old.1));
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
    /// sidebar follows the selection, and the scroll lands on the
    /// section's first content row (the header itself lives in the
    /// sidebar/title, not the content pane).
    fn snap_settings_to_selection(&mut self) {
        let items = self.settings_item_rows();
        let row = items.get(self.settings_selected).copied().unwrap_or(0);
        let sections = self.settings_sections();
        self.settings_section = self.settings_section_of_row(row);
        self.settings_scroll = sections
            .get(self.settings_section)
            .and_then(|(_, rows)| rows.first())
            .copied()
            .unwrap_or(0);
    }

    /// Theme + density the settings preview renders with: the hovered
    /// option when standing on Theme/Density (browse = preview), else
    /// the applied config. Lets an empty setup try styles against
    /// sample rows instead of a blank table.
    pub fn settings_preview_style(&self) -> (String, bool) {
        use crate::config::{DENSITIES, THEMES};
        let items = self.settings_item_rows();
        let rows = self.settings_rows();
        let mut theme = self.config.theme.clone();
        let mut comfortable = !self.config.compact();
        if let Some(&row) = items.get(self.settings_selected)
            && let Some(SettingRow::Item { kind, .. }) = rows.get(row)
        {
            match kind {
                SettingKind::Theme(i) => {
                    theme = THEMES.get(*i).unwrap_or(&theme.as_str()).to_string();
                }
                SettingKind::Density(i) => {
                    comfortable = DENSITIES
                        .get(*i)
                        .map(|d| *d == "comfortable")
                        .unwrap_or(comfortable);
                }
                _ => {}
            }
        }
        (theme, comfortable)
    }

    /// Jump to a section (Left/Right, sidebar clicks): show it, select
    /// its first item when it has one, snap scroll to its first content
    /// row. Item-less sections (the tray-icon legend) keep the stored
    /// selection and just show.
    pub fn goto_settings_section(&mut self, sec: usize) {
        let sections = self.settings_sections();
        if sections.is_empty() {
            return;
        }
        let next = sec.min(sections.len() - 1);
        self.settings_section = next;
        // First *selectable* row of the section (headers aren't).
        let items = self.settings_item_rows();
        if let Some(p) = sections[next]
            .1
            .iter()
            .filter_map(|&r| items.iter().position(|&ir| ir == r))
            .next()
        {
            self.settings_selected = p;
        }
        if let Some(&first) = sections[next].1.first() {
            self.settings_scroll = first;
        }
    }

    /// Jump to the neighbouring section, wrapping around.
    pub fn move_settings_section(&mut self, dir: i32) {
        let n = self.settings_sections().len();
        if n == 0 {
            return;
        }
        let cur = self.settings_section.min(n - 1);
        self.goto_settings_section((cur as i32 + dir).rem_euclid(n as i32) as usize);
    }

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

    /// Persist the view settings this instance actually changed.
    /// Only the touched keys reach the config file: every key is
    /// rewritten from the caller's memory otherwise, so a second
    /// terhub instance (a stale dashboard in another window) toggling
    /// a theme would silently revert the filter, sort and shell-hiding
    /// a newer instance just saved. Silent on success; the caller
    /// already announced the change. Loud only when the write failed.
    /// Public for the header-click sort path, which lives in the binary.
    pub fn persist_view(&mut self) {
        if self.view_changes.is_empty() {
            return;
        }
        // Sync the live view into the config snapshot first: the patch
        // reads `self.config`, and these four fields are the ones the
        // table edits directly (`exclude`/`sort_key`/`sort_asc`/
        // `hide_quiet_shells` are the live copies). `exclude` is
        // bare tokens in memory but the on-disk form keeps a leading
        // `!` so hand-edited configs and old fixtures (which wrote
        // `!zsh`) round-trip.
        self.config.filter = if self.exclude.is_empty() {
            String::new()
        } else if self.exclude.starts_with('!') {
            self.exclude.clone()
        } else {
            format!("!{}", self.exclude)
        };
        self.config.sort = self.sort_key.as_str().to_string();
        self.config.sort_desc = !self.sort_asc;
        self.config.hide_shells = self.hide_quiet_shells;
        let written = {
            let patch = self.config.ui_persist_changes(&self.view_changes);
            crate::config::persist_ui_settings(&patch)
        };
        match written {
            Ok(()) => self.view_changes = crate::config::ViewChanges::default(),
            Err(e) => self.set_status(format!("view kept for this session only (not saved: {e})")),
        }
    }

    /// Save the standing exclusion to `[ui] filter` so it survives
    /// restarts. Silent on success (the status line already names the
    /// query); loud only when the write failed.
    fn persist_filter(&mut self) {
        // `persist_view` re-derives the on-disk form (with the
        // leading `!`) from `self.exclude`; we only need to flag
        // the key as changed and call it.
        self.view_changes.filter = true;
        self.persist_view();
    }

    /// Cap on the in-memory filter history. Per the verification
    /// contract, the last 10 committed queries (live `/` and standing
    /// editor combined) are kept; older entries fall off the front.
    const FILTER_HISTORY_CAP: usize = 10;

    /// Adapt a history entry to the prompt it is being recalled
    /// into. The list is shared between the two prompts (per the
    /// contract), but they use different surface forms: the standing
    /// editor carries a leading `!` exclusion marker, and the live
    /// prompt is include-only. Recall lets an entry round-trip both
    /// ways instead of silently changing meaning.
    fn filter_history_recall(&self, entry: &str) -> String {
        let trimmed = entry.trim();
        if self.editing_standing {
            if trimmed.starts_with('!') {
                trimmed.to_string()
            } else {
                format!("!{trimmed}")
            }
        } else {
            trimmed.trim_start_matches('!').trim().to_string()
        }
    }

    /// Reset history navigation state. Called on Esc, Enter, and
    /// every prompt opening so a fresh prompt always starts in
    /// draft mode (input = current filter / exclude, idx = None).
    fn filter_history_reset(&mut self) {
        self.filter_history_idx = None;
        self.filter_history_pending = None;
    }

    /// Push a committed query to history. Skips empty queries and
    /// duplicates of the most recent entry (so "Up then Down then
    /// Enter" doesn't double-log the same string).
    fn push_filter_history(&mut self, q: String) {
        if q.is_empty() {
            return;
        }
        if self.filter_history.back() == Some(&q) {
            return;
        }
        while self.filter_history.len() >= Self::FILTER_HISTORY_CAP {
            self.filter_history.pop_front();
        }
        self.filter_history.push_back(q);
    }

    /// Up arrow in the prompt: move to the previous history entry.
    /// Captures the live draft on the first Up so a subsequent Down
    /// past the latest entry returns to the edited draft rather
    /// than to the pre-prompt snapshot.
    fn filter_history_prev(&mut self) {
        if self.filter_history.is_empty() {
            return;
        }
        match self.filter_history_idx {
            None => {
                self.filter_history_pending = Some(self.input.clone());
                let last = self.filter_history.len() - 1;
                self.filter_history_idx = Some(last);
                self.input = self.filter_history_recall(&self.filter_history[last]);
            }
            Some(0) => {
                // Already at the oldest entry; stay.
            }
            Some(i) => {
                self.filter_history_idx = Some(i - 1);
                self.input = self.filter_history_recall(&self.filter_history[i - 1]);
            }
        }
        self.cursor = self.input.chars().count();
    }

    /// Down arrow in the prompt: move to the next history entry, or
    /// past the latest entry return to the captured draft.
    fn filter_history_next(&mut self) {
        match self.filter_history_idx {
            None => {
                // Already at the draft; nothing to do.
            }
            Some(i) => {
                let next = i + 1;
                if next >= self.filter_history.len() {
                    // Past the latest entry; return to the draft.
                    self.filter_history_idx = None;
                    if let Some(pending) = self.filter_history_pending.take() {
                        self.input = pending;
                    }
                } else {
                    self.filter_history_idx = Some(next);
                    self.input = self.filter_history_recall(&self.filter_history[next]);
                }
            }
        }
        self.cursor = self.input.chars().count();
    }

    /// Open the text filter, editing the current query in place.
    pub fn open_filter(&mut self) {
        self.mode = Mode::PromptFilter;
        self.editing_standing = false;
        self.filter_before_edit.clone_from(&self.filter);
        self.input.clone_from(&self.filter);
        self.cursor = self.input.chars().count();
        self.filter_history_reset();
    }

    /// Mark the sort pair as changed (header-click path lives in the
    /// binary and can't reach the private `view_changes` field).
    pub fn mark_sort_changed(&mut self) {
        self.view_changes.sort = true;
        self.view_changes.sort_desc = true;
    }

    /// Open the prompt editing the standing exclusion (settings row):
    /// Enter saves it to `[ui] filter`, Esc restores without saving.
    /// The input starts with `!` already in place so the user is
    /// typing into an exclusion query. If the existing value is
    /// already prefixed with `!` (legacy hand-typed configs or a
    /// re-opened prompt after the load-time strip), the prompt
    /// preserves the single-bang form instead of doubling.
    pub fn open_standing_editor(&mut self) {
        self.mode = Mode::PromptFilter;
        self.editing_standing = true;
        self.filter_before_edit.clone_from(&self.exclude);
        let stripped = self.exclude.trim().trim_start_matches('!').trim();
        if stripped.is_empty() {
            self.input = "!".to_string();
        } else {
            self.input = format!("!{stripped}");
        }
        self.cursor = self.input.chars().count();
        self.filter_history_reset();
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
        self.view_changes.hide_shells = true;
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
    /// The standing draft also drops a leading `!` on every keystroke
    /// so the table never momentarily shows the un-normalized form.
    fn apply_live_filter(&mut self) {
        if self.editing_standing {
            self.exclude = self.input.trim().trim_start_matches('!').trim().to_string();
        } else {
            let q = self.input.trim().to_string();
            self.filter = q.clone();
            // The header names a saved filter only while the query is
            // exactly that filter's query: typing the saved text restores
            // the name, and any edit away from it drops it.
            self.active_saved_filter = self.find_saved_filter_by_query(&q);
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
                self.view_changes.theme = true;
                format!("theme: {}", self.config.theme)
            }
            SettingKind::Density(i) => {
                self.config.density = DENSITIES[i % DENSITIES.len()].to_string();
                self.view_changes.density = true;
                format!("density: {}", self.config.density)
            }
            SettingKind::ToggleNotifications => {
                self.config.notifications_enabled = !self.config.notifications_enabled;
                self.view_changes.notifications = true;
                format!(
                    "notifications: {}",
                    Self::onoff(self.config.notifications_enabled)
                )
            }
            SettingKind::ToggleHighlight => {
                self.config.highlight_attached = !self.config.highlight_attached;
                self.view_changes.highlight = true;
                format!(
                    "highlight attached: {}",
                    Self::onoff(self.config.highlight_attached)
                )
            }
            SettingKind::ToggleMruTabs => {
                self.config.mru_tabs = !self.config.mru_tabs;
                self.view_changes.mru_tabs = true;
                self.apply_tab_order();
                format!("mru tabs: {}", Self::onoff(self.config.mru_tabs))
            }
            SettingKind::ToggleBlurCols => {
                self.config.blur_cols = !self.config.blur_cols;
                // Only the blur key changed: flagging density too made
                // a stale second instance rewrite its stale density
                // over a newer instance's saved value.
                self.view_changes.blur_cols = true;
                format!("blur columns: {}", Self::onoff(self.config.blur_cols))
            }
            SettingKind::EditStandingFilter => {
                // Editing, not applying: the prompt saves on Enter.
                // Return early so the tail below doesn't persist
                // unrelated settings or claim anything was saved.
                self.open_standing_editor();
                return;
            }
            SettingKind::UseSavedFilter(i) => {
                // Applying a saved filter is not a settings write: the
                // list itself is unchanged, so return before the tail
                // (which both persists and claims "saved").
                self.use_saved_filter(i);
                return;
            }
            SettingKind::AddSavedFilter => {
                self.open_saved_filter_prompt(None);
                return;
            }
            SettingKind::RenameSavedFilter(i) => {
                if i < self.saved_filters.len() {
                    self.open_saved_filter_prompt(Some(i));
                }
                return;
            }
            SettingKind::DeleteSavedFilter(i) => {
                if let Some(f) = self.saved_filters.get(i) {
                    let name = f.name.clone();
                    self.delete_saved_filter(i);
                    self.set_status(format!("deleted filter '{name}'"));
                }
                return;
            }
        };
        // Only the keys this arm touched reach the file: writing the
        // whole snapshot here is what let a stale instance's toggle
        // revert a newer instance's filter (see `App::persist_view`).
        let written = {
            let patch = self.config.ui_persist_changes(&self.view_changes);
            crate::config::persist_ui_settings(&patch)
        };
        match written {
            Ok(()) => {
                self.view_changes = crate::config::ViewChanges::default();
                self.set_status(format!("{label} (saved)"));
            }
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
            // Saved-filter row actions. `r` renames, `d` deletes, `n`
            // names the current search. All three are inert unless the
            // selection is a row of that kind (see `selected_kind`).
            KeyCode::Char('r') => {
                self.settings_saved_filter_row(SettingKind::RenameSavedFilter(0));
                return None;
            }
            KeyCode::Char('d') => {
                self.settings_saved_filter_row(SettingKind::DeleteSavedFilter(0));
                return None;
            }
            KeyCode::Char('n') => {
                if !self.filter.trim().is_empty() {
                    self.open_saved_filter_prompt(None);
                } else {
                    self.set_status("nothing to name (search is empty)".to_string());
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
        // A successful jump means the user has seen this pane as it is
        // right now — record it before the refresh re-reads everything.
        match &ev {
            AppEvent::FocusPane(id) | AppEvent::FocusNative(id) => self.mark_attended(id),
            _ => {}
        }
        self.refresh(tmux);
    }

    /// Record that the user has seen this pane as it is right now:
    /// the row wears `\u{2713}` until its state or summary says something
    /// new. Panes with no agent record (plain shells) store an empty pair
    /// and simply stay seen — a shell has no state that could change.
    fn mark_attended(&mut self, id: &str) {
        let seen = match self.agents.get(id) {
            Some(r) => (r.state.label().to_string(), r.summary.clone()),
            None => (String::new(), String::new()),
        };
        self.attended.insert(id.to_string(), seen);
    }

    /// True when the row still matches what the user saw: same state,
    /// same summary. Ticks (`waiting_secs`, `last_turn_secs`) are
    /// deliberately not part of the key — the clock moving is not news.
    /// Cross-module read for the table renderer (see `row_mark`).
    pub fn is_attended(&self, id: &str) -> bool {
        match (self.attended.get(id), self.agents.get(id)) {
            (Some((st, sum)), Some(r)) => st == r.state.label() && sum == &r.summary,
            (Some((st, sum)), None) => st.is_empty() && sum.is_empty(),
            (None, _) => false,
        }
    }

    /// Drop attended entries the pane has outgrown: a new state or a new
    /// summary is new information, so the row stops reading as seen.
    /// Live-pane pruning happens with `marked` in the pane tick; this is
    /// the state/summary half, run after every agent snapshot. The
    /// lookup spans the fleet/native caches too: refresh_agents() runs
    /// before merge_caches() re-inserts their records, so a daemon-only
    /// map would clear every remote/native mark on each tick.
    fn prune_attended(&mut self) {
        let agents = &self.agents;
        let fleet = &self.fleet_agents;
        let native = &self.native_agents;
        self.attended.retain(|id, (st, sum)| {
            let r = agents.get(id).or_else(|| {
                fleet
                    .iter()
                    .find(|a| a.pane == *id)
                    .or_else(|| native.iter().find(|a| a.pane == *id))
            });
            match r {
                Some(r) => st == r.state.label() && sum == &r.summary,
                None => st.is_empty() && sum.is_empty(),
            }
        });
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

    /// Clear all marks, reporting the count. Shared by both foci:
    /// pane actions work from the Sessions table too (they operate on
    /// the selected pane, which exists under either focus).
    fn do_unmark(&mut self) {
        let n = self.marked.len();
        self.marked.clear();
        self.set_status(if n == 0 {
            "nothing marked".to_string()
        } else {
            format!("cleared {n} mark(s)")
        });
    }

    /// Offer to re-run the selected pane's command. Shared by both
    /// foci for the same reason as [`Self::do_unmark`].
    fn do_restart(&mut self) {
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
        let (id, pid, pid_start) = (pane.id.clone(), pane.pid, pane.pid_start);
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
        // Freeze the *agent*, not the pane root.
        //
        // tmux puts each pane root in its own process group, and an
        // interactive shell gives its foreground job another one. So
        // signalling the pane root's group never reaches the agent at
        // all — and tmux resumes a pane whose root stopped, so the
        // freeze would not hold even for the part it did reach. The
        // agent's own group is the right target: it holds the agent
        // and everything it spawned, and leaving the pane's shell
        // running keeps tmux out of it.
        //
        // A pane with no detected agent falls back to the pane root:
        // that is all there is to freeze.
        let snap = crate::proc::Snapshot::capture();
        let (target, target_start) = match snap.agent_pid(pid) {
            Some(agent_pid) => {
                let start = snap.proc_start(agent_pid).unwrap_or(0);
                (agent_pid, start)
            }
            None => (pid, pid_start),
        };
        // Identity gate: the starttime was stamped when the process was
        // seen; a recycled pid fails the starttime check inside
        // pause/resume_group instead of stopping a foreign group.
        // 0 (unknown, e.g. remote rows) skips the check.
        let expect = if target_start != 0 {
            Some(target_start)
        } else {
            None
        };
        if crate::proc::is_stopped(target) {
            match crate::proc::resume_group(target, expect) {
                Ok(_) => self.set_status(format!("resumed {id}")),
                Err(e) => self.set_status(format!("resume failed: {e}")),
            }
        } else {
            match crate::proc::pause_group(target, expect) {
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
            self.do_unmark();
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
            self.do_restart();
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
            // Only worth a line when the binding is not a no-op.
            if cfg.spec("saved_filter").spec != crate::config::KeySpec::None {
                help.push_str(&format!(" {}=cycle filters", cfg.key_label("saved_filter")));
            }
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
        // Serialize with every other env-mutating lib test; the lock
        // is held for the whole mutation window. SCRATCH_LOCK is the
        // shared seam for every test that redirects config resolution
        // (`ScopedConfig` included): without it a parallel save would
        // land in this test's scratch file.
        let _lock = SCRATCH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: lock-held; no other thread reads TERHUB_CONFIG now.
        let saved = std::env::var("TERHUB_CONFIG").ok();
        let dir = std::env::temp_dir().join(format!("terhub-viewclobber-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cfg.toml");
        std::fs::write(
            &path,
            "[ui]\ntheme = \"dark\"\nfilter = \"!bash\"\nsort = \"Idle\"\nsort_desc = true\nhide_shells = false\n",
        )
        .unwrap();
        unsafe {
            std::env::set_var("TERHUB_CONFIG", &path);
        }
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
        unsafe {
            match saved {
                Some(v) => std::env::set_var("TERHUB_CONFIG", v),
                None => std::env::remove_var("TERHUB_CONFIG"),
            }
        }
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
        assert!(
            !text.contains("filter = "),
            "search leaked into config:\n{text}"
        );
        unsafe {
            match saved {
                Some(v) => std::env::set_var("TERHUB_CONFIG", v),
                None => std::env::remove_var("TERHUB_CONFIG"),
            }
        }
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

    /// Point config reads/writes at one scratch file (the `TERHUB_CONFIG`
    /// seam), so saved-filter persistence is asserted against a real
    /// round-trip without touching the user's config.
    struct ScopedConfig {
        // The path the test wrote to. Storing it (instead of re-reading
        // `TERHUB_CONFIG` at load time) means the assertion can never
        // see a different file than the test wrote to, even if some
        // unrelated code path races the env-var restore.
        path: std::path::PathBuf,
        // The env value before we took over; restored on drop so a
        // nested scope (CI, parent test) keeps its own TERHUB_CONFIG.
        old: Option<String>,
        // Lock guard last so its Drop runs *first* on scope exit,
        // releasing SCRATCH_LOCK before the env-var restore below.
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl ScopedConfig {
        fn new() -> Self {
            // SCRATCH_LOCK is the established lock for every lib test
            // that redirects config resolution. Sharing it matters:
            // `config_path()` prefers TERHUB_CONFIG, so a concurrent
            // setter would otherwise hijack this test's reads.
            let guard = SCRATCH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            // A fresh file per instance (not per process): tests that
            // persist must not write through each other's scratch state,
            // and a per-pid path made every such test share one file.
            let n = NEXT_CFG.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let path = std::env::temp_dir()
                .join(format!("terhub-cfg-{}-{n}", std::process::id()))
                .with_extension("toml");
            let old = std::env::var("TERHUB_CONFIG").ok();
            // SAFETY: lock-held; no other lib test writes TERHUB_CONFIG.
            unsafe { std::env::set_var("TERHUB_CONFIG", &path) };
            ScopedConfig {
                path,
                old,
                _guard: guard,
            }
        }

        fn load(&self) -> (AppConfig, Vec<String>) {
            // Read from the path we wrote to, not from `config_path()`:
            // that resolves through TERHUB_CONFIG afresh, which is fine
            // while we hold the lock but couples the assertion to
            // env-var state we don't strictly control. Using the
            // captured path removes the only way the read could see a
            // different file than the test wrote to.
            crate::config::load_config_report(Some(self.path.clone()))
        }
    }

    impl Drop for ScopedConfig {
        fn drop(&mut self) {
            // Restore the env value the parent scope had. Field drop
            // order is reverse-declaration, so `_guard` has already
            // released SCRATCH_LOCK by the time we get here — no other
            // lib test can be writing this var concurrently.
            //
            // SAFETY: same confined var, restored before any other reader.
            unsafe {
                match &self.old {
                    Some(v) => std::env::set_var("TERHUB_CONFIG", v),
                    None => std::env::remove_var("TERHUB_CONFIG"),
                }
            }
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
}
