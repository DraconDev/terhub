//! TUI state machine. Backend-agnostic rendering input; knows nothing about
//! ratatui so the logic stays unit-testable.
use crate::agent::AgentRecord;
use crate::config::{AppConfig, key_matches};
use crate::daemon;
use crate::tmux::{Pane, Session, Tmux};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::HashMap;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Normal,
    PromptNew,
    PromptRename(String),
    PromptFilter,
    ConfirmKill(String),
    ConfirmKillPane(String),
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingKind {
    Theme(usize),
    Density(usize),
    ToggleNotifications,
    ToggleHighlight,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppEvent {
    Attach(String),
    FocusPane(String),
    Quit,
}

/// Which list keyboard input drives: the session sidebar or the pane table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    Sessions,
    #[default]
    Panes,
}

/// Lines of scrollback captured for the fullscreen preview.
pub const PREVIEW_LINES: u32 = 60;

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
}

/// `o` key cycle order: most useful sorts first.
const SORT_CYCLE: &[SortKey] = &[
    SortKey::Idle,
    SortKey::State,
    SortKey::For,
    SortKey::Turn,
    SortKey::Agent,
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

fn summary_text(rec: Option<&AgentRecord>) -> &str {
    rec.map(|r| r.summary.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("—")
}

/// Attention first, unknowns last.
fn state_rank(rec: Option<&AgentRecord>) -> u8 {
    use crate::agent::AgentState;
    match rec.map(|r| r.state) {
        Some(AgentState::Waiting) => 0,
        Some(AgentState::Stuck) => 1,
        Some(AgentState::Error) => 2,
        Some(AgentState::Limited) => 3,
        Some(AgentState::Working) => 4,
        Some(AgentState::Idle) => 5,
        Some(AgentState::Unknown) => 6,
        None => 7,
    }
}

fn src_name(rec: Option<&AgentRecord>) -> &'static str {
    match rec {
        Some(r) if r.source == crate::agent::Source::Hook => "hook",
        Some(r) if r.source == crate::agent::Source::Proc => "proc",
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

/// Case-insensitive substring match across every text column. `q` must
/// already be lowercased.
fn pane_matches(p: &Pane, rec: Option<&AgentRecord>, q: &str) -> bool {
    let hit = |s: &str| s.to_lowercase().contains(q);
    hit(&p.id)
        || hit(&p.session)
        || hit(&p.window_name)
        || p.index.to_string().contains(q)
        || hit(&p.command)
        || hit(&p.path)
        || rec.is_some_and(|r| hit(&r.agent) || hit(r.state.label()) || hit(&r.summary))
}

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
    /// Table sort: column plus direction (default: recency, newest first).
    pub sort_key: SortKey,
    pub sort_asc: bool,
    /// Free-text row filter (case-insensitive substring, empty = off).
    pub filter: String,
    /// Space-marked pane ids for bulk kill/attend. Survives sort/filter;
    /// pruned to live panes on every refresh.
    pub marked: std::collections::HashSet<String>,
    /// Filtered-list index under the mouse cursor, if any. Cleared on
    /// every list rebuild (an index can't survive a reorder).
    pub hover: Option<usize>,
}

impl App {
    pub fn new(config: AppConfig) -> Self {
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
            agents: HashMap::new(),
            agents_live: true,
            session_scroll: 0,
            pane_scroll: 0,
            cursor: 0,
            settings_selected: 0,
            settings_scroll: 0,
            sort_key: SortKey::Idle,
            sort_asc: false,
            filter: String::new(),
            marked: std::collections::HashSet::new(),
            hover: None,
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
                self.refresh_filtered();
                // Drop marks for dead panes; survivors keep theirs.
                let live: std::collections::HashSet<&str> =
                    self.panes.iter().map(|p| p.id.as_str()).collect();
                self.marked.retain(|id| live.contains(id.as_str()));
            }
            Err(e) => {
                self.panes.clear();
                self.filtered.clear();
                self.marked.clear();
                self.set_status(format!("panes error: {e}"));
            }
        }
        self.refresh_agents();
        if self.show_preview {
            self.capture_preview(tmux);
        }
        if self.filtered.is_empty() {
            self.focus = Focus::Sessions;
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
        self.set_status(text);
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

    /// Rebuild the visible table rows from the sidebar filter, keeping the
    /// selected pane when it survives the filter.
    pub fn refresh_filtered(&mut self) {
        self.hover = None;
        let keep = self.selected_pane().map(|p| p.id.clone());
        let session = self.side_session().map(|s| s.name.clone());
        self.filtered = match session {
            None => self.panes.clone(),
            Some(name) => self
                .panes
                .iter()
                .filter(|p| p.session == name)
                .cloned()
                .collect(),
        };
        if !self.filter.is_empty() {
            let q = self.filter.to_lowercase();
            let agents = &self.agents;
            self.filtered
                .retain(|p| pane_matches(p, agents.get(&p.id), &q));
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

    fn capture_preview(&mut self, tmux: &Tmux) {
        match self.selected_pane().map(|p| p.id.clone()) {
            Some(id) => match tmux.capture_pane_styled(&id, PREVIEW_LINES) {
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
        self.capture_preview(tmux);
        self.show_preview = true;
    }

    /// Re-capture while the preview is open so it follows the selection.
    pub fn follow_preview(&mut self, tmux: &Tmux) {
        if self.show_preview {
            self.capture_preview(tmux);
        }
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
    pub fn on_key(&mut self, key: KeyEvent, tmux: &Tmux) -> Option<AppEvent> {
        if key.modifiers.intersects(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c' | 'C'))
        {
            return Some(AppEvent::Quit);
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
        {
            return None;
        }
        match self.mode.clone() {
            Mode::PromptNew | Mode::PromptRename(_) => self.on_prompt_key(key, tmux),
            Mode::PromptFilter => self.on_filter_key(key, tmux),
            Mode::ConfirmKill(_) => self.on_confirm_key(key, tmux),
            Mode::ConfirmKillPane(_) => self.on_confirm_pane_key(key, tmux),
            Mode::ConfirmKillMarked(_) => self.on_confirm_marked_key(key, tmux),
            Mode::CommandOutput { .. } => self.on_command_output_key(key),
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
    /// Esc clears it.
    fn on_filter_key(&mut self, key: KeyEvent, tmux: &Tmux) -> Option<AppEvent> {
        match key.code {
            KeyCode::Esc => {
                self.filter.clear();
                self.cancel_modal();
                self.refresh_filtered();
                self.follow_preview(tmux);
                self.set_status("filter cleared".to_string());
            }
            KeyCode::Enter => {
                self.filter = self.input.trim().to_string();
                self.cancel_modal();
                self.refresh_filtered();
                self.pane_selected = 0;
                self.pane_scroll = 0;
                self.follow_preview(tmux);
                if !self.filter.is_empty() {
                    self.set_status(format!("filter: {}", self.filter));
                }
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
            KeyCode::Esc => {
                if self.show_preview {
                    self.show_preview = false;
                    return None;
                }
                if !self.filter.is_empty() {
                    self.filter.clear();
                    self.refresh_filtered();
                    self.set_status("filter cleared".to_string());
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
        if key_matches(&self.config.spec("theme"), code) {
            self.cycle_theme();
            return None;
        }
        if key_matches(&self.config.spec("density"), code) {
            self.cycle_density();
            return None;
        }
        if key_matches(&self.config.spec("preview"), code) {
            self.toggle_preview(tmux);
            return None;
        }
        if key_matches(&self.config.spec("settings"), code) {
            self.open_settings();
            return None;
        }
        if key_matches(&self.config.spec("sort"), code) {
            self.resort_next();
            self.follow_preview(tmux);
            self.set_status(format!(
                "sort: {:?} {}",
                self.sort_key,
                if self.sort_asc { "▲" } else { "▼" }
            ));
            return None;
        }
        if key_matches(&self.config.spec("filter"), code) {
            self.open_filter();
            return None;
        }
        // Fullscreen preview owns the screen: Enter jumps to the previewed
        // pane from either focus (like left-click), instead of attaching to
        // a session bar the preview covers up.
        if self.show_preview && key_matches(&self.config.spec("attach"), code) {
            if let Some(p) = self.selected_pane() {
                return Some(AppEvent::FocusPane(p.id.clone()));
            }
            self.set_status("no pane selected".to_string());
            return None;
        }
        if self.focus == Focus::Panes {
            return self.on_pane_key(code, tmux);
        }
        let cfg = &self.config;
        if key_matches(&cfg.spec("quit"), code) {
            return Some(AppEvent::Quit);
        }
        if key_matches(&cfg.spec("attach"), code) {
            if let Some(s) = self.side_session() {
                return Some(AppEvent::Attach(s.name.clone()));
            }
            self.set_status("pick a session row (All doesn't count)".to_string());
            return None;
        }
        if key_matches(&cfg.spec("attend"), code) {
            return self.do_attend();
        }
        if key_matches(&cfg.spec("new"), code) {
            self.do_new_session();
            return None;
        }
        if key_matches(&cfg.spec("rename"), code) {
            self.do_rename_session();
            return None;
        }
        if key_matches(&cfg.spec("kill"), code) {
            match self.side_session() {
                Some(s) => self.mode = Mode::ConfirmKill(s.name.clone()),
                None => self.set_status("pick a session row (All doesn't count)".to_string()),
            }
            return None;
        }
        if key_matches(&cfg.spec("detach"), code) {
            self.do_detach(tmux);
            return None;
        }
        if key_matches(&cfg.spec("refresh"), code) {
            self.refresh(tmux);
            return None;
        }
        if key_matches(&cfg.spec("help"), code) {
            self.set_status(format!(
                "sessions: {}=new {}=rename {}=kill {}=attach {}=preview {}=theme {}=settings {}=quit",
                cfg.key("new"),
                cfg.key("rename"),
                cfg.key("kill"),
                cfg.key("attach"),
                cfg.key("preview"),
                cfg.key("theme"),
                cfg.key("settings"),
                cfg.key("quit"),
            ));
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
            label: format!(
                "highlight attached: {}",
                Self::onoff(self.config.highlight_attached)
            ),
            active: self.config.highlight_attached,
            kind: SettingKind::ToggleHighlight,
        });
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

    pub fn open_settings(&mut self) {
        self.mode = Mode::Settings;
        self.settings_selected = 0;
        self.settings_scroll = 0;
    }

    /// Open the text filter, editing the current query in place.
    pub fn open_filter(&mut self) {
        self.mode = Mode::PromptFilter;
        self.input.clone_from(&self.filter);
        self.cursor = self.input.chars().count();
    }

    /// Live re-filter while typing; restarts at the top row.
    fn apply_live_filter(&mut self) {
        self.filter.clone_from(&self.input);
        self.refresh_filtered();
        self.pane_selected = 0;
        self.pane_scroll = 0;
    }

    /// Apply the item at popup row `row` (for mouse clicks).
    pub fn settings_apply_at_row(&mut self, row: usize) -> bool {
        let kind = self.settings_rows().get(row).and_then(|r| match r {
            SettingRow::Item { kind, .. } => Some(*kind),
            SettingRow::Header(_) => None,
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
        };
        match crate::config::persist_ui_settings(
            &self.config.theme,
            &self.config.density,
            self.config.notifications_enabled,
            self.config.highlight_attached,
        ) {
            Ok(()) => self.set_status(format!("{label} (saved)")),
            Err(e) => self.set_status(format!("{label} (live only: {e})")),
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
                self.settings_selected = self.settings_selected.saturating_sub(1);
                return None;
            }
            KeyCode::Down => {
                if self.settings_selected + 1 < items.len() {
                    self.settings_selected += 1;
                }
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
        if key_matches(&self.config.spec("settings"), key.code) {
            self.mode = Mode::Normal;
        }
        None
    }

    /// Jump to the longest-waiting agent. Works from either focus.
    fn do_attend(&mut self) -> Option<AppEvent> {
        match daemon::fetch_snapshot(Duration::from_millis(300)) {
            Ok(snap) => {
                // Marked panes scope the search; unmarked behaves as before.
                let agents: Vec<AgentRecord> = if self.marked.is_empty() {
                    snap.agents
                } else {
                    snap.agents
                        .into_iter()
                        .filter(|a| self.marked.contains(&a.pane))
                        .collect()
                };
                match crate::attend::pick_attend(&agents, None) {
                    Some(a) => Some(AppEvent::FocusPane(a.pane.clone())),
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

    /// Space: toggle the row's mark and step down, file-manager style, so
    /// a run of rows marks with repeated presses.
    fn toggle_mark(&mut self) {
        let id = match self.selected_pane() {
            Some(p) => p.id.clone(),
            None => {
                self.set_status("no pane selected".to_string());
                return;
            }
        };
        if self.marked.remove(&id) {
            self.set_status(format!("unmarked {id}"));
        } else {
            self.marked.insert(id.clone());
            self.set_status(format!("marked {id}"));
        }
        self.move_pane_down();
    }

    /// Session actions with no pane meaning (`new`/`rename`/`detach`), shared
    /// by both foci so they work from the table too — otherwise the
    /// empty-state hint names a dead key in the default focus.
    fn do_new_session(&mut self) {
        self.mode = Mode::PromptNew;
        self.input.clear();
    }

    fn do_rename_session(&mut self) {
        match self.side_session().map(|s| s.name.clone()) {
            Some(name) => {
                self.cursor = name.chars().count();
                self.input = name.clone();
                self.mode = Mode::PromptRename(name);
            }
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
    fn on_pane_key(&mut self, code: KeyCode, tmux: &Tmux) -> Option<AppEvent> {
        let cfg = &self.config;
        if key_matches(&cfg.spec("quit"), code) {
            return Some(AppEvent::Quit);
        }
        if key_matches(&cfg.spec("attach"), code) {
            if let Some(p) = self.selected_pane() {
                return Some(AppEvent::FocusPane(p.id.clone()));
            }
            self.set_status("no pane selected".to_string());
            return None;
        }
        if key_matches(&cfg.spec("kill"), code) {
            if !self.marked.is_empty() {
                let mut ids: Vec<String> = self.marked.iter().cloned().collect();
                ids.sort();
                self.mode = Mode::ConfirmKillMarked(ids);
            } else {
                match self.selected_pane() {
                    Some(p) => self.mode = Mode::ConfirmKillPane(p.id.clone()),
                    None => self.set_status("no pane selected".to_string()),
                }
            }
            return None;
        }
        if key_matches(&cfg.spec("mark"), code) {
            self.toggle_mark();
            return None;
        }
        if key_matches(&cfg.spec("unmark"), code) {
            let n = self.marked.len();
            self.marked.clear();
            self.set_status(if n == 0 {
                "nothing marked".to_string()
            } else {
                format!("cleared {n} mark(s)")
            });
            return None;
        }
        if key_matches(&cfg.spec("attend"), code) {
            return self.do_attend();
        }
        if key_matches(&cfg.spec("refresh"), code) {
            self.refresh(tmux);
            return None;
        }
        if key_matches(&cfg.spec("new"), code) {
            self.do_new_session();
            return None;
        }
        if key_matches(&cfg.spec("rename"), code) {
            self.do_rename_session();
            return None;
        }
        if key_matches(&cfg.spec("detach"), code) {
            self.do_detach(tmux);
            return None;
        }
        let custom = cfg
            .commands
            .iter()
            .find(|c| key_matches(&c.spec, code))
            .cloned();
        if let Some(cmd) = custom {
            self.run_custom(&cmd);
            return None;
        }
        if key_matches(&cfg.spec("help"), code) {
            let mut help = format!(
                "panes: {}=jump {}=kill {}=preview {}=attend {}=mark {}=unmark {}=filter {}=sort {}=settings tab=back {}=quit",
                cfg.key("attach"),
                cfg.key("kill"),
                cfg.key("preview"),
                cfg.key("attend"),
                cfg.key("mark"),
                cfg.key("unmark"),
                cfg.key("filter"),
                cfg.key("sort"),
                cfg.key("settings"),
                cfg.key("quit"),
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
                for id in &ids {
                    match tmux.kill_pane(id) {
                        Ok(_) => {
                            killed += 1;
                            self.marked.remove(id);
                        }
                        Err(_) => failed += 1,
                    }
                }
                self.set_status(if failed == 0 {
                    format!("killed {killed} marked pane(s)")
                } else {
                    format!("killed {killed}, {failed} failed")
                });
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
        app.config.theme = "solarized".to_string();
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
        // Toggles flip too.
        for _ in 0..10 {
            app.on_key(press(KeyCode::Down), &tmux);
        }
        app.on_key(press(KeyCode::Enter), &tmux);
        assert!(!app.config.highlight_attached);
        app.on_key(press(KeyCode::Esc), &tmux);
        assert_eq!(app.mode, Mode::Normal);
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
                last_turn_secs: 0,
                screen_hash: 0,
                quiet_since: 0,
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
            last_turn_secs: 0,
            screen_hash: 0,
            quiet_since: 0,
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
        // Reopen edits in place; Esc clears everything.
        app.on_key(press(KeyCode::Char('f')), &tmux);
        assert_eq!(app.input, "ba");
        app.on_key(press(KeyCode::Esc), &tmux);
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.filter, "");
        assert_eq!(app.filtered.len(), 2);
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
    fn sort_key_o_cycles_with_status() {
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
