//! Agent awareness: state model, hook events, screen-pattern manifests.
use anyhow::{Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AgentState {
    #[default]
    Unknown,
    Working,
    Waiting,
    Error,
    /// Quiet between turns, but the agent *has* finished something
    /// (a completion banner matched, the process exited 0, the screen
    /// shows a verdicted footer). Distinct from `Idle` — Idle means
    /// "never ran / never finished", Done means "ran, finished,
    /// waiting for the next prompt". `Done` is never attention.
    Done,
    Idle,
    /// Working, but the agent's process tree has burned no CPU for
    /// STUCK_SECS: hung, not busy.
    Stuck,
    /// Usage/quota wall hit: the agent is paused until reset, not broken.
    /// Quota language is telemetry about limits, never a crash report.
    Limited,
    /// The agent's process tree is stopped (`SIGSTOP`) — deliberately
    /// frozen, by terhub's own `z` or by anyone else. Distinct from
    /// `Stuck`: a stopped process burns no CPU, so without this state
    /// a paused agent reads as a hung one and terhub escalates the
    /// alarm it caused. Never attention: nobody is needed to thaw it.
    Paused,
    /// The agent is repeating the same thing: the same error, the same
    /// command, the same request, over and over, instead of making
    /// progress (a provider 400-loop, a re-run verification command,
    /// an unanswered question asked again). Distinct from `Stuck` —
    /// this one is *busy*, re-doing work that never lands; and from
    /// `Error` — one failure is news, the same one ten times is a
    /// loop. Needs the human: it will burn tokens until someone stops
    /// it.
    Looping,
}

/// Forward-compatible decode: a state name this build does not know
/// becomes [`AgentState::Unknown`] instead of failing the decode.
///
/// A snapshot is one JSON document, so a single unknown enum variant
/// used to fail the *whole* response: a client that predated the
/// `paused` state answered "daemon not running" while a healthy
/// daemon was serving, and every pane in the TUI went blank. A name
/// this build has never heard is future information, not corruption —
/// decode it as unknown and keep the pane visible.
///
/// Serialization is unchanged (lowercase variant names), so the
/// registry and the activity ledger keep their current format.
impl<'de> Deserialize<'de> for AgentState {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Ok(AgentState::parse(&raw).unwrap_or(AgentState::Unknown))
    }
}

impl AgentState {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "working" | "work" | "busy" | "running" | "start" => Some(AgentState::Working),
            "waiting" | "wait" | "input" | "attention" | "blocked" | "needs-input" | "stop" => {
                Some(AgentState::Waiting)
            }
            "error" | "failed" | "fail" => Some(AgentState::Error),
            "idle" | "ready" => Some(AgentState::Idle),
            "done" | "finished" | "complete" | "completed" => Some(AgentState::Done),
            "stuck" | "hung" => Some(AgentState::Stuck),
            "limited" | "limit" | "quota" | "capped" => Some(AgentState::Limited),
            "paused" | "stopped" | "frozen" | "suspended" => Some(AgentState::Paused),
            "looping" | "repeating" | "repeat" => Some(AgentState::Looping),
            "unknown" => Some(AgentState::Unknown),
            _ => None,
        }
    }

    /// The canonical state name — the spelling the wire uses.
    ///
    /// This is the machine name: it is what the JSON snapshot carries,
    /// what `AgentState::parse` accepts, and what a rule's `on = ...`
    /// is matched against. Keep it in step with the serde spelling.
    pub fn label(self) -> &'static str {
        match self {
            AgentState::Unknown => "unknown",
            AgentState::Working => "working",
            AgentState::Waiting => "waiting",
            AgentState::Error => "error",
            AgentState::Idle => "idle",
            AgentState::Done => "done",
            AgentState::Stuck => "stuck",
            AgentState::Limited => "limited",
            AgentState::Paused => "paused",
            AgentState::Looping => "looping",
        }
    }

    /// The name a human reads.
    ///
    /// `limited` shows as **quota**: the state means "the agent hit a
    /// usage wall and is waiting the limit out", and `quota` says
    /// that in one word. `limited` on its own reads like a priority
    /// ("this one is less important") rather than a wall, and the
    /// whole point of the state is that the row is *not* asking the
    /// operator for anything.
    ///
    /// The wire keeps `limited` (see the serde spelling), so an older
    /// TUI, MCP client or remote `terhub` still decodes a snapshot
    /// from a newer daemon. `AgentState::parse` accepts both names,
    /// so `--state quota` and `--state limited` both work.
    pub fn display_label(self) -> &'static str {
        match self {
            AgentState::Limited => "quota",
            other => other.label(),
        }
    }

    /// The human-facing label when the summary gives enough context.
    ///
    /// `waiting` is the protocol state; `question` is the more useful
    /// description when the waiting summary is the actual question being
    /// put to the operator. Generic input prompts and hook-only
    /// `waiting on custom` records remain `waiting`.
    pub fn display_label_for(self, summary: &str) -> &'static str {
        if self == AgentState::Waiting && is_question_summary(summary) {
            "question"
        } else {
            self.display_label()
        }
    }

    /// States where the human should look at the pane.
    pub fn needs_attention(self) -> bool {
        matches!(
            self,
            AgentState::Waiting
                | AgentState::Error
                | AgentState::Stuck
                | AgentState::Limited
                | AgentState::Looping
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Hook,
    Screen,
    /// Process-tree identification: the agent binary is really there.
    Proc,
    /// Pinned by `terhub register`: the detector missed this agent, a
    /// human named it. Carried while its pane (and pid, if any) lives.
    Manual,
}

impl Source {
    /// The source's name in full.
    ///
    /// The table renders its own short forms ("hook", "proc", "man",
    /// "scr"); this is the unabbreviated name, for anything that
    /// reports a record outside the table — the MCP tools, JSON
    /// output, a log line.
    pub fn label(self) -> &'static str {
        match self {
            Source::Hook => "hook",
            Source::Screen => "screen",
            Source::Proc => "proc",
            Source::Manual => "manual",
        }
    }
}

/// Forward-compatible decode for [`Source`], for the same reason as
/// [`AgentState`]: a source this build does not know must not fail the
/// whole snapshot. It decodes as [`Source::Screen`] — the weakest
/// evidence there is, and exactly what the UI already renders for any
/// source it cannot place.
impl<'de> Deserialize<'de> for Source {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Ok(match raw.trim().to_lowercase().as_str() {
            "hook" => Source::Hook,
            "proc" => Source::Proc,
            "manual" => Source::Manual,
            _ => Source::Screen,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRecord {
    pub pane: String,
    pub agent: String,
    pub state: AgentState,
    pub source: Source,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub session: String,
    #[serde(default)]
    pub window: String,
    #[serde(default)]
    pub command: String,
    pub updated_at: u64,
    pub state_since: u64,
    /// Last observed CPU-tick sum of the agent's process subtree (0 = none).
    #[serde(default)]
    pub cpu_ticks: u64,
    /// When the tick sum last changed (0 = unknown); stuck clock.
    #[serde(default)]
    pub cpu_still_since: u64,
    /// starttime (stat field 22) of the agent process behind `cpu_ticks`
    /// (0 = unknown, e.g. pre-upgrade registries). Compared on carry so a
    /// recycled pid never inherits the previous owner's still-clock.
    #[serde(default)]
    pub agent_start: u64,
    /// Last completed working turn, seconds (0 = none yet). A turn ends
    /// when Working moves to waiting/error/limited/idle; stuck doesn't
    /// end a turn (the same turn is grinding), it just carries this.
    #[serde(default)]
    pub last_turn_secs: u64,
    /// FNV-1a hash of the last captured screen (0 = none yet).
    #[serde(default)]
    pub screen_hash: u64,
    /// When the captured screen last changed (0 = unknown). tmux activity
    /// counts every repaint, so agent TUIs that redraw identical cells
    /// look busy forever; this clock only restarts on visible change.
    #[serde(default)]
    pub quiet_since: u64,
    /// When the user last prompted this agent via hook (0 = never).
    #[serde(default)]
    pub last_prompt_at: u64,
    /// That prompt's text, capped (empty = none yet).
    #[serde(default)]
    pub last_prompt: String,
    /// Stable local name (`@pi`), minted once by the daemon, empty
    /// until then. Survives pane churn inside the session; fleet rows
    /// never carry one (handles are local-only).
    #[serde(default)]
    pub handle: String,
    /// Model id reported by hooks/wrappers (`opus-4.5`, empty = unknown).
    #[serde(default)]
    pub model: String,
    /// Context-window fullness 0-100 (None = unknown).
    #[serde(default)]
    pub ctx_pct: Option<u8>,
    /// Agent pid pinned by `register` (0 = none). The poll tracks CPU
    /// under it so stuck detection works for manually pinned agents.
    #[serde(default)]
    pub reg_pid: u32,
    /// Mirror of the pane's tmux `window_bell_flag`, reset every poll
    /// (never carried): true only when the flag fired AND the classifier
    /// upgraded a manifest hit on it. Together with `bell_reason` it
    /// composes the needs-you evidence: `bell` is the kernel's
    /// yes/no, `bell_reason` is what the screen showed when the bell
    /// rang.
    ///
    /// The user-visible bell is the classifier's "🔔 …" summary prefix
    /// in the pane row; these two fields are the daemon-side evidence
    /// behind it (the tmux flag alone is not enough — a bare bell in
    /// the text is shell noise), and they travel to machine consumers
    /// through the JSON snapshot (`terhub status --json`).
    #[serde(default)]
    pub bell: bool,
    /// Short human label for the bell event: "bell", "y/N", "press
    /// enter", etc. Empty when no bell has fired since reset. Set
    /// in lockstep with `bell`; cleared together. It is the
    /// classifier's summary minus its "🔔 " prefix, so it says what
    /// the screen showed when the bell rang.
    #[serde(default)]
    pub bell_reason: String,
    /// Pre-trip state saved when the stuck gate fires on a Hook|Manual
    /// carry (None = gate never tripped). The hook branch carries state
    /// forward instead of re-deriving it, so without this the trip would
    /// latch Stuck until the 30-minute hook lapse; the saved verdict is
    /// restored when CPU recovers. Merge re-derives every poll and always
    /// resets this to None.
    #[serde(default)]
    pub stuck_base: Option<AgentState>,
}

/// Prompt text kept on the record (the ledger keeps the longer cut).
pub const RECORD_PROMPT_CAP: usize = 200;

/// Cap `s` to `max` chars (char-safe).
pub fn cap_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

/// Slug for handle parts: lowercase alnum runs, anything else folds to
/// one dash, edge dashes trimmed (`My Project!` → `my-project`).
pub fn slug_handle_part(s: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            dash = false;
        } else if !out.is_empty() && !dash {
            out.push('-');
            dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Mint a handle (`@agent`, `-2`/`-3`… on collision) against the
/// used set.
pub fn mint_handle(used: &std::collections::HashSet<String>, agent: &str) -> String {
    let agent = slug_handle_part(agent);
    let base = format!("@{agent}");
    if !used.contains(&base) {
        return base;
    }
    let mut n = 2;
    loop {
        let cand = format!("{base}-{n}");
        if !used.contains(&cand) {
            return cand;
        }
        n += 1;
    }
}

/// Give every agent-carrying record without one a handle. Panes mint in
/// sorted order so suffixes don't depend on HashMap iteration; already
/// minted records (and agent-less ones) are never touched.
pub fn ensure_handles(registry: &mut std::collections::HashMap<String, AgentRecord>) {
    let mut used: std::collections::HashSet<String> = registry
        .values()
        .filter_map(|r| {
            if r.handle.is_empty() {
                None
            } else {
                Some(r.handle.clone())
            }
        })
        .collect();
    let mut panes: Vec<String> = registry
        .iter()
        .filter(|(_, r)| r.handle.is_empty() && !r.agent.is_empty())
        .map(|(pane, _)| pane.clone())
        .collect();
    panes.sort();
    for pane in panes {
        let agent = match registry.get(&pane) {
            Some(r) => r.agent.clone(),
            None => continue,
        };
        let handle = mint_handle(&used, &agent);
        used.insert(handle.clone());
        if let Some(r) = registry.get_mut(&pane) {
            r.handle = handle;
        }
    }
}

/// Rewrite persisted handles minted by the old session-bearing scheme
/// (`@agent-session`, `@agent-session-N`) into the sessionless one:
/// matching records are cleared so the next [`ensure_handles`] pass
/// re-mints them. Handles that don't look like the old mint exactly
/// (e.g. `@claude-s`, or `@agent` with no session part) are left alone.
/// Runs once per daemon load; new-format handles never match again.
///
/// The one exception worth naming: a session that slugs to pure digits
/// (`2`) makes its handle indistinguishable from a mint suffix, so it
/// is rewritten on every load. The rewrite is deterministic, so the
/// assignment comes back the same each time — it just isn't the
/// no-op the other records get.
pub fn strip_legacy_handles(
    registry: &mut std::collections::HashMap<String, AgentRecord>,
) -> usize {
    let mut stale: Vec<String> = Vec::new();
    for (pane, r) in registry.iter() {
        if r.handle.is_empty() || r.agent.is_empty() {
            continue;
        }
        let base = format!(
            "@{}-{}",
            slug_handle_part(&r.agent),
            slug_handle_part(&r.session)
        );
        let legacy = r.handle == base
            || r.handle
                .strip_prefix(&format!("{base}-"))
                .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
        // session.is_empty() would mint a base of `@agent-`, which no
        // real handle can start with — skip it rather than rely on that.
        if legacy && !slug_handle_part(&r.session).is_empty() {
            stale.push(pane.clone());
        }
    }
    let stripped = stale.len();
    for pane in stale {
        if let Some(r) = registry.get_mut(&pane) {
            r.handle.clear();
        }
    }
    stripped
}

/// Resolve a `@handle` CLI arg to its pane id (exact match only).
/// Non-handle args resolve to None (the caller passes them through).
pub fn resolve_handle(records: &[AgentRecord], target: &str) -> Option<String> {
    if !target.starts_with('@') {
        return None;
    }
    records
        .iter()
        .find(|r| r.handle == target)
        .map(|r| r.pane.clone())
}

/// A working turn completed: Working moved to a terminal-ish state.
/// Done counts: a finished agent's turn duration is real time worked,
/// not zero — omitting it recorded last_turn_secs = 0 on every
/// Working→Done transition.
pub fn turn_ended(from: AgentState, to: AgentState) -> bool {
    from == AgentState::Working
        && matches!(
            to,
            AgentState::Waiting
                | AgentState::Error
                | AgentState::Limited
                | AgentState::Looping
                | AgentState::Idle
                | AgentState::Done
        )
}

impl AgentRecord {
    pub fn waiting_secs(&self, now: u64) -> u64 {
        now.saturating_sub(self.state_since)
    }

    pub fn display_label(&self) -> &'static str {
        self.state.display_label_for(&self.summary)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookEvent {
    pub pane: String,
    pub agent: String,
    pub state: AgentState,
    #[serde(default)]
    pub summary: String,
    /// The user's prompt text (UserPromptSubmit / before_agent_start);
    /// empty for lifecycle-only events.
    #[serde(default)]
    pub prompt: String,
    /// Model id (`hook --model`), empty when the reporter doesn't know.
    #[serde(default)]
    pub model: String,
    /// Context fullness 0-100 (`hook --ctx`), None when unknown.
    #[serde(default)]
    pub ctx: Option<u8>,
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Pane commands that are session harnesses/dashboards, not agents. Their
/// screens mention agents without running them (muxa lists watched
/// sessions; terhub's own TUI renders agent tables), so screen
/// classification skips them entirely — only hooks or a real agent process
/// inside can claim the pane.
pub fn is_harness_command(cmd: &str) -> bool {
    matches!(cmd.trim().to_lowercase().as_str(), "muxa" | "terhub")
}

/// Pane commands that are plain shells, not agents. Screen classification
/// skips these unless a record already exists for the pane.
pub fn is_shell_command(cmd: &str) -> bool {
    matches!(
        cmd.trim().to_lowercase().as_str(),
        "sh" | "bash"
            | "zsh"
            | "fish"
            | "dash"
            | "ksh"
            | "tcsh"
            | "tmux"
            | "ssh"
            | "mosh"
            | "-sh"
            | "-bash"
            | "-zsh"
    )
}

// ---------------------------------------------------------------------------
// Manifests
// ---------------------------------------------------------------------------

/// Lines of the capture the verdict is read from, unless a manifest
/// asks for a deeper window (`Manifest::tail`).
pub const DEFAULT_TAIL: usize = 15;

/// Screen patterns for one agent family (TOML-serializable).
#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub agent: String,
    /// Pane-command substrings (case-insensitive) identifying this agent.
    #[serde(default)]
    pub commands: Vec<String>,
    /// How many lines of the capture the verdict is read from.
    ///
    /// The default ([`DEFAULT_TAIL`]) is the freshest slice of the
    /// screen: enough to hold a status line and the message above it,
    /// and no more, so old scrollback can never outvote the current
    /// screen. Raise it for an agent whose live state does not sit at
    /// the bottom edge — opencode's composer is anchored to the top of
    /// a tall pane (a fresh session draws its logo where the
    /// transcript will grow), leaving its status line dozens of lines
    /// above the bottom. That status line is the only honest "am I
    /// working" answer there, so it is worth reading that far.
    ///
    /// A deeper window never widens what counts as *needing you*: the
    /// error/quota/waiting markers are still read from the freshest
    /// [`DEFAULT_TAIL`] lines only, so an old failure sitting in the
    /// scrollback stays history.
    #[serde(default)]
    pub tail: Option<usize>,
    #[serde(default)]
    pub working: Vec<String>,
    #[serde(default)]
    pub waiting: Vec<String>,
    #[serde(default)]
    pub error: Vec<String>,
    /// Quota/rate-limit language: reads as Limited, never Error.
    #[serde(default)]
    pub limited: Vec<String>,
    /// Done language (completion banners, all-tasks-verdicts): reads
    /// as `Done` — finished, not waiting. Kept separate from waiting
    /// so a done agent never badges as needing input, and from
    /// `idle_fallback` (a chrome box) so a clean banner isn't shadowed
    /// by stale scrollback.
    #[serde(default)]
    pub done: Vec<String>,
    /// Legacy Done-as-Idle list: a one-line backwards-compat alias
    /// kept so existing manifests keep working. Patterns here count
    /// as `Done` if there's no `done:` list; if both are present,
    /// `done:` wins and `idle:` is ignored. New manifests should use
    /// `done:`.
    #[serde(default)]
    pub idle: Vec<String>,
    /// Done chrome (input boxes, idle footers): reads as Idle, but only
    /// as a fallback — chrome sits below everything, so freshest-wins
    /// alone would let it shadow real verdicts above. It applies when no
    /// waiting/error/limited line matched anywhere in the tail, and loses
    /// to live activity below it (but beats stale activity above it: a
    /// spinner scrolled past the returned input box is history).
    #[serde(default)]
    pub idle_fallback: Vec<String>,
    /// Waiting veto: when any of these match the tail, a waiting verdict
    /// falls back to the freshest working line (or no verdict). For agents
    /// whose input prompt stays visible during work — `❯` below a live
    /// spinner is an input box, not a plea for input.
    #[serde(default)]
    pub waiting_unless: Vec<String>,
    /// Lines that prove the agent is still working *on its own* — a
    /// running goal, a live background task. A `busy` match is working
    /// evidence and a veto at once: the quiet verdicts that describe a
    /// turn that ended (a `done` banner, the input box that came back)
    /// must not outrank work that is still in flight.
    #[serde(default)]
    pub busy: Vec<String>,
    /// The agent's published activity line — what it is doing right now
    /// (`◎ Goal · Verifying the result · 54min59s active`). A `phase`
    /// match is everything a `busy` match is, and it also supplies the
    /// Summary text: a busy pane has no verdict to show, and the phase is
    /// the one line that says what the agent is up to.
    #[serde(default)]
    pub phase: Vec<String>,
}

pub struct CompiledManifest {
    pub agent: String,
    pub commands: Vec<String>,
    /// Lines of the capture the verdict is read from.
    tail: usize,
    working: Vec<Regex>,
    waiting: Vec<Regex>,
    error: Vec<Regex>,
    limited: Vec<Regex>,
    done: Vec<Regex>,
    idle: Vec<Regex>,
    idle_fallback: Vec<Regex>,
    waiting_unless: Vec<Regex>,
    busy: Vec<Regex>,
    phase: Vec<Regex>,
}

impl CompiledManifest {
    pub fn compile(m: &Manifest) -> Result<Self> {
        fn compile_all(agent: &str, kind: &str, pats: &[String]) -> Result<Vec<Regex>> {
            pats.iter()
                .map(|p| {
                    let pat = if p.starts_with("(?i)") {
                        p.clone()
                    } else {
                        format!("(?i){p}")
                    };
                    Regex::new(&pat).with_context(|| format!("bad {kind} pattern in {agent}: {p}"))
                })
                .collect()
        }
        Ok(CompiledManifest {
            agent: m.agent.clone(),
            commands: m.commands.iter().map(|c| c.to_lowercase()).collect(),
            tail: m.tail.unwrap_or(DEFAULT_TAIL).max(1),
            working: compile_all(&m.agent, "working", &m.working)?,
            waiting: compile_all(&m.agent, "waiting", &m.waiting)?,
            error: compile_all(&m.agent, "error", &m.error)?,
            limited: compile_all(&m.agent, "limited", &m.limited)?,
            waiting_unless: compile_all(&m.agent, "waiting_unless", &m.waiting_unless)?,
            busy: compile_all(&m.agent, "busy", &m.busy)?,
            phase: compile_all(&m.agent, "phase", &m.phase)?,
            // `done` and `idle` are siblings: a manifest that
            // already moved to `done:` gets an empty `idle:` list
            // (the field still compiles to a no-op regex vector).
            // Manifests that haven't migrated still get their
            // `idle:` patterns, treated as `Done` at classify time.
            done: compile_all(&m.agent, "done", &m.done)?,
            idle: compile_all(&m.agent, "idle", &m.idle)?,
            idle_fallback: compile_all(&m.agent, "idle_fallback", &m.idle_fallback)?,
        })
    }

    pub fn matches_command(&self, cmd: &str) -> bool {
        let cmd = cmd.to_lowercase();
        self.commands.iter().any(|c| cmd.contains(c.as_str()))
    }

    /// Classify screen text. The freshest signal wins: the newest matching
    /// line decides, so current output overrides stale scrollback (an old
    /// "error:" in prose can't pin Error forever while the agent works).
    /// Within one line, the most specific signal wins: limited beats error
    /// beats waiting beats working beats idle ("Error: rate limit reached"
    /// is a quota wall, not a crash; a completion marker beside live
    /// activity is stale scrollback, not done-ness).
    /// Freshest matching line wins; the returned summary is that line,
    /// cleaned for the Summary column.
    ///
    /// `visible` is how many of the capture's last lines are the pane's
    /// visible area, and it only feeds the repetition check's smaller
    /// bar (see `repeated_content`); pass 0 when it is unknown, which
    /// counts the whole capture again.
    pub fn classify(
        &self,
        screen: &str,
        visible: usize,
    ) -> Option<(AgentState, String, Option<String>)> {
        let all_lines: Vec<&str> = screen.lines().collect();
        let mut tail: Vec<&str> = all_lines.iter().rev().take(self.tail).copied().collect();
        tail.reverse();
        let tail_offset = all_lines.len().saturating_sub(tail.len());
        // The fresh window: the only lines a "needs you" marker
        // (error, quota, waiting) may be read from. A manifest with a
        // deep tail still honours those markers only here — an old
        // failure sitting far up the scrollback is history, not
        // something that needs the operator now. With the default
        // tail this is the whole window, so nothing changes.
        let fresh_from = tail.len().saturating_sub(DEFAULT_TAIL);
        // The veto scans the whole capture, not just the verdict tail:
        // tall status regions (task lists, goal panels) can push the
        // live-task line above the freshest 15 while the prompt stays
        // glued below it.
        let veto = screen
            .lines()
            .any(|line| self.waiting_unless.iter().any(|r| r.is_match(line)));
        let mut hit = None;
        let mut work = None;
        let mut need = false;
        let mut fallback = None;
        // The freshest quota wall in the tail, and the freshest waiting
        // hit that actually said something (a bare input box says
        // nothing — see `is_prompt_chrome`).
        let mut limited_at = None;
        let mut asked_at = None;
        // The freshest busy line (a running goal, a live background task)
        // and the freshest phase line (the agent saying what it is up to).
        let mut busy = None;
        let mut phase = None;
        for (i, line) in tail.iter().enumerate() {
            // A busy line is working evidence: the agent is still doing
            // something on its own, even if its own turn has ended. A
            // phase line is only the summary — the same text can mean
            // "running" or "paused", and only the busy list knows which.
            let is_busy = self.busy.iter().any(|r| r.is_match(line));
            if is_busy {
                busy = Some(i);
            }
            if self.phase.iter().any(|r| r.is_match(line)) {
                phase = Some(i);
            }
            let is_work = self.working.iter().any(|r| r.is_match(line)) || is_busy;
            if is_work {
                work = Some(i);
            }
            // A need marker only counts inside the fresh window (see
            // `fresh_from`): a deep tail must not turn old scrollback
            // into a red row.
            let fresh = i >= fresh_from;
            if fresh && self.limited.iter().any(|r| r.is_match(line)) && is_quota_wall(line) {
                hit = Some((AgentState::Limited, i));
                limited_at = Some(i);
                need = true;
            } else if fresh && self.error.iter().any(|r| r.is_match(line)) {
                hit = Some((AgentState::Error, i));
                need = true;
            } else if fresh && self.waiting.iter().any(|r| r.is_match(line)) {
                hit = Some((AgentState::Waiting, i));
                if !is_prompt_chrome(line) {
                    asked_at = Some(i);
                }
                need = true;
            } else if is_work {
                hit = Some((AgentState::Working, i));
            } else if self.idle.iter().any(|r| r.is_match(line)) {
                // Legacy `idle:` list (pre-Done migration): promotes
                // to `Done`. Within-line precedence unchanged — a
                // hint of need or activity on the same line outranks
                // a completion marker.
                hit = Some((AgentState::Done, i));
            } else if self.done.iter().any(|r| r.is_match(line)) {
                // New `done:` list: explicit `Done` verdict.
                hit = Some((AgentState::Done, i));
            }
            if self.idle_fallback.iter().any(|r| r.is_match(line)) {
                fallback = Some(i);
            }
        }
        // A quota wall outranks the input box under it.
        //
        // The freshest-line rule hands the verdict to whatever sits
        // lowest on the screen, and the lowest thing on an agent's
        // screen is its input box — `>` for gemini, `❯` for claude.
        // The box is chrome: it carries no words, it is there before
        // the turn and after it. But the box is also the *only* line
        // the operator can act on, so a pane whose agent is out of
        // quota and retrying every minute used to read as `waiting` —
        // "needs you" — for an agent nobody can unblock before the
        // limit resets. The wall above the box is the news: the agent
        // is out of quota, and it will keep saying so until the reset.
        //
        // Three things keep this honest:
        //   - the wall must be *older* than the box (the freshest line
        //     still wins),
        //   - the box must be bare chrome (a question with words in it
        //     is a real plea and keeps the `waiting` verdict),
        //   - and no work may have happened below the wall (a live
        //     task under it means the agent moved on, and the wall is
        //     history).
        if let Some(li) = limited_at
            && let Some((AgentState::Waiting, wi)) = hit
            && wi > li
            && is_prompt_chrome(tail[wi])
            && asked_at.is_none_or(|a| a < li)
            && work.is_none_or(|w| w <= li)
            && busy.is_none_or(|b| b <= li)
        {
            hit = Some((AgentState::Limited, li));
        }
        // A vetoed prompt is an input box under a live task, not a plea:
        // fall back to the freshest working line, or no verdict at all
        // (the caller pairs that with process evidence, or drops it).
        //
        // The veto is only for *chrome*, which is what the rule was
        // written for: a bare `❯`/`>` sits under a live spinner and
        // says nothing. A waiting line that carries words is a real
        // plea, and it can legitimately sit on a screen that also
        // offers the interrupt — muse draws its blocking question
        // picker with a footer reading `Enter to select … Esc to
        // interrupt`, so an unscoped veto silently converted "blocked
        // on the operator" into "working" (exactly the failure this
        // classifier exists to prevent).
        if veto
            && let Some((AgentState::Waiting, wi)) = hit
            && is_prompt_chrome(tail[wi])
        {
            hit = work.map(|i| (AgentState::Working, i));
        }
        // A busy line is a veto for the quiet verdicts. A `done` banner
        // and the returned input box describe a turn that ended; a
        // running goal or a live background task says the work did not.
        // Without this, an agent that finished a turn but is still
        // verifying reads as finished — and the operator sends it a
        // message into the middle of a running goal.
        if busy.is_some() && matches!(hit, Some((AgentState::Done | AgentState::Idle, _))) {
            hit = work.map(|i| (AgentState::Working, i));
        }
        // Done chrome (input boxes) applies last: never over need (even
        // vetoed need — a live task outranks chrome), never over a banner
        // (which summarizes better), and never over live activity below
        // it — but it beats stale activity above it. Chrome is the
        // weakest signal: without an explicit `done:` banner, the box
        // alone reads as `Idle` (quiet, never finished) instead of
        // `Done` (quiet, finished) — the user can promote it with a
        // pattern in the manifest if they want the distinction.
        if let Some(fi) = fallback {
            // A busy line outranks the chrome: the input box that came
            // back means the turn ended, not that the work stopped.
            let dominated = need
                || busy.is_some()
                || match hit {
                    None => false,
                    Some((AgentState::Waiting | AgentState::Error | AgentState::Limited, _)) => {
                        true
                    }
                    Some((AgentState::Done, _)) => true,
                    Some((AgentState::Idle, _)) => true,
                    Some((AgentState::Working, wi)) => wi >= fi,
                    Some(_) => true,
                };
            if !dominated {
                hit = Some((AgentState::Idle, fi));
            }
        }
        let verdict = hit.map(|(state, i)| {
            let mut line = tail[i];
            // A blocking questionnaire is still the canonical `waiting`
            // state on the wire, but when we can see the actual prompt,
            // show that question in Summary. The `question` display tag
            // is derived from that text; instruction chrome such as
            // "Enter to select" is not a useful answer to "what is it
            // asking me?".
            if state == AgentState::Waiting && is_question_text(line) {
                let phase = phase.map(|pi| screen_summary_line(tail[pi]));
                return (state, screen_question_summary(line), phase);
            }
            if state == AgentState::Waiting
                && let Some(question) = question_from_menu(&all_lines, tail_offset + i)
            {
                let phase = phase.map(|pi| screen_summary_line(tail[pi]));
                return (state, question, phase);
            }
            // A quota wall summarizes as the wall's own words: the
            // count-down it publishes ("quota · resets in 1h23m57s") if
            // it published one, else the line as printed. The wall's
            // marketing ("Please upgrade your subscription…") is what
            // the operator does *not* need in a 40-column row; the
            // number — when it comes back — is what they do.
            if state == AgentState::Limited {
                let phase = phase.map(|pi| screen_summary_line(tail[pi]));
                return (state, limited_summary(line), phase);
            }
            // A bare prompt ("❯") says nothing: show the nearest line
            // above it with actual text — usually the question or the
            // current task. Box rules and blanks carry no meaning.
            if screen_summary_line(line).chars().count() <= 2 {
                for above in tail[..i].iter().rev() {
                    if above.chars().any(|c| c.is_alphanumeric()) {
                        line = above;
                        break;
                    }
                }
            }
            let phase = phase.map(|pi| screen_summary_line(tail[pi]));
            (state, screen_summary_line(line), phase)
        });
        // One state is not a line: repetition is a property of the
        // whole capture (the same error, the same command, over and
        // over — the scrollback's own evidence that the agent is
        // stuck re-doing something), so it is checked here, after the
        // freshest-line verdict is known.
        //
        // It replaces a working/idle/error verdict: a pane whose
        // screen prints `Error: 400: ...` ten times is not merely an
        // error (one failure is news; the same one ten times is a
        // loop) and it is certainly not `working` or `idle`, which is
        // what it read before this check existed. It does not replace
        // a question (the human's answer is the more urgent thing)
        // nor a quota wall ("wait for the reset" is the more useful
        // verdict), and its own summary is the repeated line, so the
        // operator sees *what* is repeating.
        if let Some(repeating) = repeated_content(screen, visible) {
            let strong = verdict.as_ref().is_some_and(|(state, summary, _)| {
                matches!(state, AgentState::Limited)
                    || state.display_label_for(summary) == "question"
            });
            if !strong {
                return Some((
                    AgentState::Looping,
                    format!("repeating: {}", screen_summary_line(&repeating)),
                    verdict.map(|(_, _, phase)| phase).unwrap_or(None),
                ));
            }
        }
        verdict
    }
}

pub fn builtin_manifests() -> Vec<Manifest> {
    vec![
        Manifest {
            agent: "claude".to_string(),
            tail: None,
            commands: vec!["claude".to_string(), "claude-code".to_string()],
            working: vec![
                "esc to interrupt".to_string(),
                "working…".to_string(),
                "⠋|⠙|⠹|⠸|⠼|⠴|⠦|⠧|⠇|⠏".to_string(),
                "✻.*(thinking|working|cooking)".to_string(),
                "shell still running".to_string(),
                "shells still running".to_string(),
                "bypass permissions on.*shell".to_string(),
                "·\\s*\\d+\\s*shell\\b".to_string(),
            ],
            waiting: vec![
                "^❯".to_string(),
                "try asking".to_string(),
                "do you want to proceed".to_string(),
                "\\bapprove\\b".to_string(),
                "enter to confirm".to_string(),
                "\\[y/n\\]".to_string(),
            ],
            error: vec![
                // Capital E only: lowercase "an error:" is prose.
                "\\b(?-i:E)rror:".to_string(),
                "something went wrong".to_string(),
                "traceback".to_string(),
            ],
            limited: vec![
                "rate limit".to_string(),
                "usage limit".to_string(),
                "quota".to_string(),
                "\\b429\\b".to_string(),
            ],
            done: vec![],
            idle: vec![],
            idle_fallback: vec![],
            waiting_unless: vec![
                "esc to interrupt".to_string(),
                "shell still running".to_string(),
                "shells still running".to_string(),
                "bypass permissions on.*shell".to_string(),
                "·\\s*\\d+\\s*shell\\b".to_string(),
            ],
            busy: vec![
                "shell still running".to_string(),
                "shells still running".to_string(),
                "bypass permissions on.*shell".to_string(),
                "·\\s*\\d+\\s*shell\\b".to_string(),
            ],
            phase: vec![
                "shell still running".to_string(),
                "shells still running".to_string(),
                "bypass permissions on.*shell".to_string(),
                "·\\s*\\d+\\s*shell\\b".to_string(),
            ],
        },
        Manifest {
            agent: "codex".to_string(),
            tail: None,
            commands: vec!["codex".to_string()],
            working: vec![
                "esc to interrupt".to_string(),
                "working".to_string(),
                "⠋|⠙|⠹|⠸|⠼|⠴|⠦|⠧|⠇|⠏".to_string(),
            ],
            waiting: vec![
                "^❯".to_string(),
                "would you like".to_string(),
                "\\bapprove\\b".to_string(),
                "\\[y/n\\]".to_string(),
                "press enter to (send|confirm)".to_string(),
            ],
            error: vec!["\\b(?-i:E)rror\\b".to_string(), "\\bfailed\\b".to_string()],
            limited: vec![
                "rate limit".to_string(),
                "usage limit".to_string(),
                "quota".to_string(),
                "\\b429\\b".to_string(),
            ],
            done: vec![],
            idle: vec![],
            idle_fallback: vec![],
            waiting_unless: vec![],
            busy: vec![],
            phase: vec![],
        },
        Manifest {
            agent: "gemini".to_string(),
            tail: None,
            // `agy` is a Gemini-frontend CLI (screens footer "Gemini X
            // Flash"): pinning the command keeps quota/waiting screens on
            // the gemini manifest even when they name other models (the
            // quota wall lists Claude/GPT models as a group).
            commands: vec!["gemini".to_string(), "agy".to_string()],
            working: vec![
                "esc to cancel".to_string(),
                "working".to_string(),
                "⠋|⠙|⠹|⠸|⠼|⠴|⠦|⠧|⠇|⠏".to_string(),
            ],
            waiting: vec![
                "^>".to_string(),
                "type your message".to_string(),
                "\\bapprove\\b".to_string(),
                "\\bconfirm\\b".to_string(),
            ],
            error: vec!["\\b(?-i:E)rror\\b".to_string(), "\\bfailed\\b".to_string()],
            limited: vec![
                "rate limit".to_string(),
                "usage limit".to_string(),
                "quota".to_string(),
                "\\b429\\b".to_string(),
            ],
            done: vec![],
            idle: vec![],
            idle_fallback: vec![],
            waiting_unless: vec![],
            // The Gemini CLI keeps its input box (`>`) on screen for the
            // whole turn, so the prompt above says nothing about whether
            // the agent is done — it is chrome, exactly like the `❯` box
            // muse keeps under a live task. What the CLI publishes
            // instead is the agent bar below the box: one row per
            // running subagent or background task —
            //
            //   `● Agent(id: title)  <activity> · <elapsed>`
            //   `● [HH:MM:SS] <command> running`
            //
            // Those rows are the CLI's own "work in flight" marker
            // ("Show active subagents and background tasks below the
            // input box"), and they live below the prompt, so the
            // freshest-line rule reads them as the latest word on the
            // screen: a pane whose bar is populated is working, not
            // waiting. When the last subagent finishes the bar goes with
            // it, and the prompt becomes the freshest match again —
            // which is the honest "your turn" signal.
            busy: vec![
                "^\\s*●\\s+Agent\\(".to_string(),
                "^\\s*●\\s+\\[.*\\brunning\\b".to_string(),
            ],
            // The same lines double as the phase: for a working row the
            // Summary shows what the agent is doing (which subagent,
            // what it is doing, for how long) instead of a spinner.
            phase: vec![
                "^\\s*●\\s+Agent\\(".to_string(),
                "^\\s*●\\s+\\[.*\\brunning\\b".to_string(),
            ],
        },
        Manifest {
            // muse keeps its `❯` input box visible under a live task *and*
            // after the turn ends, so the composer can never be the
            // verdict: it is chrome. What actually says something is the
            // interrupt offer (`esc to interrupt`) on the live spinner,
            // the `◆ Worked for <dur> · <time>` turn-completion banner,
            // and the structured-question picker (`Request user input`
            // with numbered options and an `Enter to select …` footer).
            //
            // The picker is why the interrupt offer cannot be an
            // unscoped veto here: muse prints `Esc to interrupt` in the
            // picker footer too, so a waiting verdict on a footer that
            // carries words is a real plea (the veto now only fires on
            // bare chrome — see `classify`).
            agent: "muse".to_string(),
            tail: None,
            commands: vec!["muse".to_string()],
            working: vec![
                "esc to interrupt".to_string(),
                "calling tools".to_string(),
                "thinking \\(".to_string(),
                "⠋|⠙|⠹|⠸|⠼|⠴|⠦|⠧|⠇|⠏".to_string(),
            ],
            waiting: vec![
                // The blocking question picker's footer. It is muse's
                // own "stopped on you" shape: the numbered options sit
                // above it, the composer is still drawn below it.
                "enter to select".to_string(),
                "↑/↓ to move".to_string(),
                "tab for an optional note".to_string(),
                // The permission card ("Allow once / Reject once /
                // Allow for this session / Block for this session"): the
                // card's own option labels are the plea, and the agent is
                // stopped on the operator exactly like the question picker
                // above. Anchored to the start of a line (with the option
                // number the card prints) so prose that merely mentions
                // "allow once" cannot raise a false alarm.
                "^\\s*(?:\\d+\\.\\s*)?allow once\\b".to_string(),
                "^\\s*(?:\\d+\\.\\s*)?reject once\\b".to_string(),
                "^\\s*(?:\\d+\\.\\s*)?always allow\\b".to_string(),
                "^\\s*(?:\\d+\\.\\s*)?allow for this session\\b".to_string(),
                "^\\s*(?:\\d+\\.\\s*)?block for this session\\b".to_string(),
                "\\[y/n\\]".to_string(),
                "press enter to send".to_string(),
            ],
            done: vec![
                // The turn-completion banner. A finished muse screen has
                // nothing else: the composer coming back is quiet, not
                // done, and it is also there mid-task.
                "^◆ Worked for".to_string(),
            ],
            idle: vec![],
            // No `idle_fallback`: muse's composer (`❯`, bare or carrying a
            // rotating hint) is drawn under a live task *and* at rest, so it
            // says nothing about whose turn it is. It used to sit in
            // `waiting`, which made every finished muse pane read "needs you"
            // until the 30-minute decay — the false alarm this manifest must
            // not raise. Finish detection stays with the `Worked for` banner
            // above and the quiet clock.
            idle_fallback: vec![],
            waiting_unless: vec!["esc to interrupt".to_string()],
            // Muse's own background-task rows, drawn under the composer
            // while a watcher or a background shell is still alive:
            //
            //   `└ ◈ Re-run nix profile upgrade  running   1h 25m 03s`
            //   `└ ◈ Wait for nix profile upgrade  watching  2h 26m 41s`
            //
            // The turn that started it has ended, the work has not — the
            // exact case `busy` exists for. The `└ last event …` line on a
            // live spinner is a different row and matches neither verb.
            busy: vec!["^\\s*└\\s.*\\b(?:running|watching)\\b".to_string()],
            phase: vec!["^\\s*└\\s.*\\b(?:running|watching)\\b".to_string()],
            error: vec![
                "\\b(?-i:E)rror\\b".to_string(),
                "traceback".to_string(),
                "something went wrong".to_string(),
            ],
            limited: vec![
                "rate limit".to_string(),
                "usage limit".to_string(),
                "quota".to_string(),
                "\\b429\\b".to_string(),
            ],
        },
        Manifest {
            // The "Message · Enter send" box only returns when the turn is
            // over (live inference shows the Running footer instead), while
            // the "Ask Mcode" placeholder persists under live tasks — so
            // the box means done and the placeholder means nothing. Failure
            // counts stay Tasks-anchored: bare "failed" would fire on prose
            // like "SPEC-200 failed".
            agent: "minimax-code".to_string(),
            tail: None,
            commands: vec!["minimax-code".to_string(), "minimax".to_string()],
            working: vec![
                "esc stop".to_string(),
                "running \\d".to_string(),
                "loading \\d".to_string(),
                "⠋|⠙|⠹|⠸|⠼|⠴|⠦|⠧|⠇|⠏".to_string(),
            ],
            waiting: vec![
                "do you want to proceed".to_string(),
                "\\bapprove\\b".to_string(),
                "\\[y/n\\]".to_string(),
                "enter to confirm".to_string(),
            ],
            done: vec![
                "✓ goal complete".to_string(),
                "todo list \\d+/\\d+ completed".to_string(),
            ],
            idle: vec![],
            // The input box only returns when the turn is over (live
            // inference replaces it with the Running footer), but as
            // bottom chrome it must never shadow verdicts above — hence
            // fallback, not idle. (Bare "tok/s" is deliberately NOT a
            // working signal: Completed lines keep a stale rate forever
            // and would pin working.)
            idle_fallback: vec!["enter send".to_string()],
            error: vec![
                "\\b(?-i:E)rror\\b".to_string(),
                "traceback".to_string(),
                "something went wrong".to_string(),
                "× tasks".to_string(),
                "tasks? · [1-9]\\d* failed".to_string(),
            ],
            limited: vec![
                "rate limit".to_string(),
                "usage limit".to_string(),
                "quota".to_string(),
                "\\b429\\b".to_string(),
            ],
            waiting_unless: vec![],
            // The goal loop is still running: `◎ Goal · Verifying the
            // result · 54min59s active`. This line says *what* the
            // agent is doing, so it doubles as the Summary text — it is
            // the one thing an operator wants to read on a pane whose
            // own turn has ended but whose work has not.
            //
            // The pattern is deliberately loose about the phase name
            // (`Verifying the result`, `Waiting for background tasks`,
            // `Active`) so a new phase is picked up for free. It does
            // require the trailing `active`, which is what separates a
            // running goal from the goal-loop's own stopped banners
            // (`GOAL · paused by GLLA`, `GOAL · work saved`).
            phase: vec!["goal · .*active".to_string()],
            // Background work continues after the turn's own turn ends:
            // `◐ Tasks · 1 agent active · /tasks details`. A pane whose
            // todo list reads `4/4 completed` is not finished — this
            // line is what says so.
            busy: vec![
                "tasks? · \\d+ (agents?|background) active".to_string(),
                // A *running* goal — the phase list above is looser on
                // purpose (it only reads the line), so the busy list
                // names the phases that mean work in flight. A paused
                // goal (`Goal · Paused · 58min35s active`) is work on
                // hold, and must not be counted as busy.
                "goal · (active|verifying|waiting)[^·]* · .*active".to_string(),
            ],
        },
        Manifest {
            // pi (this repo's harness). Attention states are always
            // claimed; working states are claimed through live-only
            // markers — lines the UI shows while work is in flight
            // and clears when the turn ends. The bottom chrome (git
            // line, the ↑/↓/cost stats line, the Chrome Bridge line)
            // renders identically while the model streams and after
            // the transcript stops, so chrome is deliberately NOT
            // matched: a `working` list built on it would pin
            // finished panes as busy. The markers below all vanish on
            // completion (the spinner bar clears, the retry line goes
            // with the loop, the audit worker rows disappear, the
            // GLLA status flips to IDLE/COMPLETE) — so a match means
            // live work, not stale scrollback. Before this manifest
            // existed, pi panes only classified through
            // `classify_screen`'s cross-manifest fallback — i.e. a
            // `waiting` verdict came from claude's `^❯` happening to
            // match, which is luck, not a contract.
            agent: "pi".to_string(),
            tail: None,
            commands: vec!["pi".to_string()],
            working: vec![
                // Live spinner bar (`── ⠦ Working ──`).
                "──.*Working".to_string(),
                // Automatic retry loop after a quota failure
                // (`── ⠸ Retrying (11/15) in 59s...`).
                "Retrying \\(\\d+/\\d+\\)".to_string(),
                // Detached GLLA audit worker still grinding after the
                // main turn ended (`Audit running · thinking…`).
                "Audit running".to_string(),
                // Revived background subagent (`async subagent ·
                // background ... running`).
                "async subagent.*running".to_string(),
                // GLLA status rows for live work (`● list item ·
                // auditing · RUNNING`, `● GOAL · working · RETRY
                // ARMED`). Finished rows read IDLE/COMPLETE/paused
                // instead, so these never pin a done pane.
                "· RUNNING ·".to_string(),
                "goal · working".to_string(),
            ],
            waiting: vec![
                // The blocking question UI (ask_user_question /
                // questionnaire). These lines exist only while pi is
                // waiting on the human, so they are the honest
                // needs-you signal — no veto needed, because the menu
                // itself is blocking: pi shows nothing below it.
                // `^❯ \d+\.` is the caret on a numbered option,
                // which the plain input box never renders.
                "enter to select".to_string(),
                "esc to cancel".to_string(),
                "tab to switch questions".to_string(),
                // GLLA's non-numbered decision picker:
                // `↑↓ navigate  enter select  escape/ctrl+c cancel`.
                "↑↓ navigate".to_string(),
                "enter select".to_string(),
                r"^❯ \d+\.".to_string(),
                "type something".to_string(),
            ],
            // No `done`/`idle`/`idle_fallback`: a finished pi pane
            // prints nothing distinctive, the transcript just stops.
            // Finish detection belongs to the hook (`agent_end` →
            // idle) and the quiet clock, not to screen text.
            done: vec![],
            idle: vec![],
            idle_fallback: vec![],
            error: vec![
                "\\b(?-i:E)rror\\b".to_string(),
                "traceback".to_string(),
                "something went wrong".to_string(),
            ],
            limited: vec![
                "rate limit".to_string(),
                "usage limit".to_string(),
                "quota".to_string(),
                "\\b429\\b".to_string(),
            ],
            waiting_unless: vec![],
            busy: vec![],
            // Working rows show what the agent is doing instead of a
            // spinner: the retry count, the audit worker's live line,
            // the GLLA status row. Same live-only lines as `working`
            // above, so the summary can never ride stale scrollback.
            phase: vec![
                "──.*Working".to_string(),
                "Retrying \\(\\d+/\\d+\\)".to_string(),
                "Audit running".to_string(),
                "async subagent.*running".to_string(),
                "· RUNNING ·".to_string(),
                "goal · working".to_string(),
            ],
        },
        Manifest {
            // opencode (sst/opencode) TUI. Its status bar is the
            // contract, and it is a good one: while a request is in
            // flight the footer's left slot renders the shimmer
            // spinner plus "esc interrupt"; when the session is not
            // running the same slot renders the project location
            // (`~/dir:branch`). The footer is therefore the one line
            // that says "a turn is running" — and unlike a spinner
            // frame it is live-only: it is gone the moment the turn
            // ends, so it cannot pin a finished pane as busy the way
            // a chrome line would.
            //
            // Without this manifest every opencode pane read
            // `working` forever. The process tree proved an agent was
            // there, no manifest claimed the screen, and the
            // shimmering footer kept the screen hash changing — so
            // the quiet clock never reached its idle threshold
            // either. The fix is the footer's own two states: the
            // interrupt offer means working, the footer chrome
            // (which is present in both states) means the session is
            // not running.
            agent: "opencode".to_string(),
            // A fresh opencode session draws its logo where the
            // transcript will grow, so the composer — and the status
            // line that says whether a request is running — sits far
            // above the bottom edge of a tall pane, with only blank
            // rows and the app's own bottom bar below it. The status
            // line is the only honest answer there, so the verdict is
            // read from further up than the default window. The
            // need markers stay in the fresh window, so an old error
            // in the scrollback cannot turn this into a red row.
            tail: Some(60),
            commands: vec!["opencode".to_string()],
            working: vec![
                // The footer's interrupt offer. "esc interrupt" is the
                // resting shape; one esc press turns it into "esc
                // again to interrupt". Both mean "a request is in
                // flight", so both are working evidence — the
                // second one especially, since an operator who just
                // pressed esc is still waiting on the agent.
                "esc( again)?( to)? interrupt".to_string(),
            ],
            // opencode's question picker. It replaces the composer
            // (the status bar goes with it), so nothing below it can
            // outvote the verdict: the agent is blocked on the human
            // and the picker line is the proof. The question text
            // sits above the options, so the summary carries the
            // question, not the key hints.
            waiting: vec!["↑↓ select".to_string()],
            error: vec![
                // Capital E only: lowercase "an error:" is prose.
                "\\b(?-i:E)rror:".to_string(),
                "something went wrong".to_string(),
                "traceback".to_string(),
            ],
            limited: vec![
                "rate limit".to_string(),
                "usage limit".to_string(),
                "quota".to_string(),
                "\\b429\\b".to_string(),
            ],
            done: vec![],
            idle: vec![],
            // The footer's own chrome: the command-palette hint that
            // sits in every opencode status bar, busy or not. It is
            // the *only* line that is always there, so it is what an
            // idle opencode pane can be recognised by. It is chrome,
            // hence `idle_fallback` and not `working`: the busy
            // footer carries the interrupt offer on the same line,
            // and within one line working outranks chrome.
            //
            // The chrome is deliberately opencode-specific: this
            // manifest is also tried over screens whose command
            // matched nothing, so a loose pattern here would claim
            // other agents' panes.
            idle_fallback: vec![
                // The footer's command-palette hint. The pattern is
                // deliberately a prefix ("ctrl+p comm"), not the whole
                // word: opencode truncates the hint when the pane is
                // narrow ("ctrl+p comma"), and the truncated form is
                // still the same line.
                "ctrl\\+p comm".to_string(),
                // The composer's gradient bar (a `╹` and a run of `▀`).
                // It is opencode's signature and it survives a narrow
                // pane, where the footer sheds everything but the
                // location. Both markers are chrome: the bar is drawn
                // whether or not a request is running, so it only ever
                // says "this is an opencode composer", and the
                // interrupt offer on the footer line is what says
                // "running".
                "╹▀".to_string(),
            ],
            waiting_unless: vec![],
            busy: vec![],
            // No phase: the footer's chrome line is noise as a
            // summary (a row of spinner glyphs), and a working row
            // with no phase simply shows no summary.
            phase: vec![],
        },
    ]
}

/// Load user manifests from a directory (`*.toml`), reporting unreadable
/// or invalid files instead of silently dropping them. A typo in one user
/// manifest must not blind the daemon without a word.
pub fn load_manifests(dir: &Path) -> (Vec<Manifest>, Vec<String>) {
    let mut out = Vec::new();
    let mut warnings = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return (out, warnings),
    };
    // read_dir order is filesystem-dependent; classification is
    // first-match-wins, so sort by file name for deterministic matches.
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                warnings.push(format!("manifest {}: unreadable ({e:#})", path.display()));
                continue;
            }
        };
        match toml::from_str::<Manifest>(&text) {
            Ok(m) => out.push(m),
            Err(e) => warnings.push(format!(
                "manifest {}: invalid ({})",
                path.display(),
                e.to_string().replace('\n', " ")
            )),
        }
    }
    (out, warnings)
}

pub fn compile_all(manifests: &[Manifest]) -> Result<Vec<CompiledManifest>> {
    manifests.iter().map(CompiledManifest::compile).collect()
}

/// Compile, skipping broken manifests with a warning each instead of failing
/// the whole set. A typo in one user manifest must not blind the daemon.
pub fn compile_all_lossy(manifests: &[Manifest]) -> (Vec<CompiledManifest>, Vec<String>) {
    let mut ok = Vec::new();
    let mut warnings = Vec::new();
    for m in manifests {
        match CompiledManifest::compile(m) {
            Ok(c) => ok.push(c),
            Err(e) => warnings.push(format!("manifest '{}': {e:?}", m.agent)),
        }
    }
    (ok, warnings)
}

/// A classifier hit: optional identity, a state, and the winning line —
/// the prompt or error text behind the Summary column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenHit {
    pub agent: Option<String>,
    pub state: AgentState,
    pub summary: String,
    /// The agent's published activity line, when it publishes one (see
    /// `Manifest::phase`). Independent of the verdict text: a busy pane
    /// has no verdict to show, and this is what it is doing instead.
    pub phase: Option<String>,
}

/// A raw screen line, cleaned for one-line display: trim, collapse runs
/// of whitespace (indented prompts, box art), cap the length.
pub fn screen_summary_line(line: &str) -> String {
    const MAX: usize = 80;
    let flat: String = line.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= MAX {
        return flat;
    }
    flat.chars().take(MAX.saturating_sub(1)).collect::<String>() + "…"
}

/// How many times the same error or command line must appear in one
/// screen before the pane reads as repeating it. Three copies deep,
/// with the newest one in the second half of what the pane is
/// displaying right now: fewer than that, and the pane is not looping
/// yet; older than that, and it is the scrollback of an earlier round
/// of work, not the loop the user is in now.
const REPEAT_OCCURRENCES: u32 = 3;

/// How far consecutive copies of a repeated line may sit apart before
/// they stop being one live streak. A provider retry usually prints
/// the same error again within a few lines; a copy tens of lines later
/// is instead an old line being encountered a second time. This is
/// the lease on a repeated line: one fresh copy in the second half of
/// the screen can repopulate an already-live streak, but it cannot,
/// by itself, revive a finished one. A new loop must earn a fresh
/// three-copy streak.
const REPEAT_STREAK_GAP_LINES: usize = 8;

/// Whether one line's occurrence indexes form a currently live
/// streak: at least three copies, no gap between consecutive copies
/// longer than the lease window, and the live copy in the second half
/// of what the pane displays.
fn is_live_repetition(positions: &[usize], boundary: usize) -> bool {
    let mut streak = 0u32;
    let mut previous: Option<usize> = None;
    for &position in positions {
        streak = match previous {
            Some(p) if position.saturating_sub(p) <= REPEAT_STREAK_GAP_LINES => streak + 1,
            _ => 1,
        };
        previous = Some(position);
        if streak >= REPEAT_OCCURRENCES && position >= boundary {
            return true;
        }
    }
    false
}

/// The shortest line worth treating as content: anything shorter is
/// chrome (`… (3 earlier lines)`, a box border, a token counter).
const REPEAT_MIN_LEN: usize = 20;

/// Whether a (whitespace-collapsed) line is terminal chrome rather
/// than content the agent wrote: box-drawing, rules, separators, and
/// the collapsed-output markers TUIs print over their own `…` blocks.
///
/// Chrome is what a repeated line must never be: a diff-stat block
/// repeats a border glyph legitimately, and an audit note that an
/// agent rewrote three times before parking is a cleared conversation,
/// not a loop. Both are excluded, and the exclusion is what the loop
/// check means by "the same thing".
fn is_chrome_line(flat: &str) -> bool {
    if flat.is_empty() {
        return true;
    }
    // No letters at all: borders, rules, the gradient bar, spinner
    // runs and token counters are drawn shapes, not content.
    if !flat.chars().any(char::is_alphabetic) {
        return true;
    }
    // A collapsed-output marker (`... (5 earlier lines, ctrl+o to
    // expand)`, `+1707 …`): the TUI's own bookkeeping, there because
    // the transcript is long, not because the agent says it.
    flat.starts_with("...") && (flat.contains("earlier lines") || flat.contains("more lines"))
}

/// Whether a content line counts as something to loop on: it must be a
/// command the agent ran or a failure it hit. A repeated sentence
/// (an audit note, a table row, a file listing) is ordinary content;
/// a repeated command or error is an agent doing the same thing again
/// and getting the same result.
fn is_repeatable(flat: &str) -> bool {
    let lower = flat.to_ascii_lowercase();
    flat.starts_with("$ ")
        || flat.starts_with("> ")
        || lower.contains("error:")
        || lower.contains("failed")
        || lower.contains("failure:")
        || lower.contains("fatal:")
        || lower.contains("rate limit")
        || lower.contains("usage limit")
        || lower.contains("quota")
}

/// Collapse a run of digits to `#`, so `Retrying (3/15) in 60s` and
/// `Retrying (4/15) in 59s` are the same line. Misleading on its own,
/// but only ever applied to lines that are already error/command-shaped,
/// where the digits are a counter and the counter is not the content.
fn digits_normalized(flat: &str) -> String {
    let mut out = String::with_capacity(flat.len());
    let mut in_digits = false;
    for c in flat.chars() {
        if c.is_ascii_digit() {
            if !in_digits {
                out.push('#');
            }
            in_digits = true;
        } else {
            out.push(c);
            in_digits = false;
        }
    }
    out
}

/// The line this screen is repeating, if any: the same error or the
/// same command printed over and over — the agent is not progressing,
/// it is doing the same thing again and getting the same result (a
/// provider 400-loop is the live shape: model-recovery retries print
/// the same error line every cycle while the pane reads plain `idle`).
///
/// `visible` is how many of the capture's last lines are the pane's
/// visible area (the rest is scrollback). The freshest copy of the
/// repeated line must be in that area's second half: a copy anywhere
/// above it is the story of an earlier round of work, not of the one
/// the pane is in now. Passing 0 means the whole capture counts as
/// visible.
///
/// Returns the *display* form of the most repeated line (the
/// freshest copy, so the operator reads the latest instance).
pub fn repeated_content(screen: &str, visible: usize) -> Option<String> {
    use std::collections::HashMap;
    // Positions and freshest literal form for every normal form seen.
    // A candidate is judged by the one placed last on the screen, not
    // by every historical copy in the capture; counting stale copies
    // into a lease would let a long-finished run look current again.
    let mut positions: HashMap<String, Vec<usize>> = HashMap::new();
    let mut fresh: HashMap<String, String> = HashMap::new();
    let total = screen.lines().count();
    // The second half of the visible area. `visible == 0` means there
    // is no pane behind this screen — the whole capture is visible.
    let within = if visible == 0 {
        total
    } else {
        visible.min(total)
    };
    let boundary = total.saturating_sub(within / 2);
    for (i, line) in screen.lines().enumerate() {
        let flat: String = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if flat.chars().count() < REPEAT_MIN_LEN || is_chrome_line(&flat) || !is_repeatable(&flat) {
            continue;
        }
        let key = digits_normalized(&flat);
        positions.entry(key.clone()).or_default().push(i);
        fresh.insert(key, flat);
    }
    // Promote only the most frequent currently live answer. A stale
    // candidate must not win merely because it was observed last.
    positions
        .into_iter()
        .filter(|(_, p)| is_live_repetition(p, boundary))
        .max_by(|(_, a), (_, b)| a.len().cmp(&b.len()))
        .map(|(key, _)| fresh.get(&key).cloned().unwrap_or_default())
}

/// Whether a summary is an actual question text suitable for the
/// human-facing `question` tag. The wire state remains `waiting`.
fn is_question_summary(summary: &str) -> bool {
    let text = summary.trim().trim_end_matches(['"', '\'', '”', '’', ')']);
    let lower = text.to_lowercase();
    is_question_text(text)
        || lower.starts_with("decision needed")
        || lower.starts_with("your decision is needed")
}

fn is_question_text(summary: &str) -> bool {
    let text = summary.trim().trim_end_matches(['\"', '\'', '”', '’', ')']);
    text.ends_with('?') && text.chars().any(char::is_alphanumeric)
}

/// Trim a question for the one-line Summary column without trimming
/// away the question mark that drives the more-specific display tag.
fn screen_question_summary(line: &str) -> String {
    const MAX: usize = 80;
    let flat = line.split_whitespace().collect::<Vec<_>>().join(" ");
    // A boxed question (opencode draws its picker inside the
    // composer's border) carries the border into the summary; the
    // question is what the operator has to read.
    let flat = flat
        .trim_start_matches(['┃', '│', '|'])
        .trim_start()
        .to_string();
    if flat.chars().count() <= MAX {
        return flat;
    }
    flat.chars().take(MAX.saturating_sub(2)).collect::<String>() + "…?"
}

/// Find the question/decision heading above a blocking choice menu.
///
/// Only a recognized menu footer (or one of its selected options) opens
/// the search, and the heading has to have actual choices between it and
/// the waiting marker. That keeps an old question in the transcript from
/// renaming a generic wait.
fn question_from_menu(lines: &[&str], waiting_at: usize) -> Option<String> {
    let wait_line = *lines.get(waiting_at)?;
    let lower = wait_line.to_lowercase();
    let menu_marker = lower.contains("enter to select")
        || lower.contains("tab to switch questions")
        || lower.contains("type something")
        || lower.contains("↑↓ navigate")
        || lower.contains("enter select")
        || lower.contains("enter submit")
        || lower.contains("escape/ctrl+c cancel")
        || is_choice_line(wait_line);
    if !menu_marker {
        return None;
    }
    let start = waiting_at.saturating_sub(40);
    for question_at in (start..waiting_at).rev() {
        let question = lines[question_at].trim();
        if !is_question_summary(question) {
            continue;
        }
        if lines[question_at + 1..=waiting_at]
            .iter()
            .any(|line| is_choice_line(line))
        {
            return Some(if is_question_text(question) {
                screen_question_summary(question)
            } else {
                screen_summary_line(question)
            });
        }
    }
    None
}

fn is_choice_line(line: &str) -> bool {
    let line = line.trim_start();
    let line = line.strip_prefix("❯ ").unwrap_or(line);
    // muse marks the *selected* option with `⟩`, the others with plain
    // indentation, so the picked line carries the caret instead of a
    // leading number.
    let line = line.strip_prefix("⟩ ").unwrap_or(line);
    // A boxed line: opencode draws its question picker inside the
    // composer's `┃` border, so the option number sits behind the
    // border, not at the start of the line.
    let line = line.trim_start_matches(['┃', '│', '|', '┃']).trim_start();
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    (digits > 0 && line[digits..].starts_with(". "))
        || line.starts_with("Type something.")
        || line.starts_with("→ ")
        || line.starts_with("/goal tweak ")
        || line.starts_with("/list tweak ")
}

/// Is this line a quota *wall*, or an agent merely talking about one?
///
/// A `limited` pattern matches the word, and agents write about quota
/// constantly — "ready to run the moment either quota resets", "the
/// rate limit needs to clear". Those are status reports about someone
/// else's limit (or the operator's own budget), and tagging them
/// `quota` fires a needs-you alarm for a pane that is working fine.
///
/// A wall announces itself, so a wall line has to look like an
/// announcement: it carries a wall marker (a warning glyph, an
/// `Error:`, an HTTP status) or it states the limit in the past tense
/// ("has been reached", "exceeded", "ran out"). Prose that *plans*
/// around a limit ("once it resets", "needs to clear", "would you
/// like") never does either.
///
/// Deliberately not a keyword blacklist: the missing signal is the
/// positive one — an agent narrating a limit rarely says "exceeded".
pub fn is_quota_wall(line: &str) -> bool {
    // Markers that only appear when something actually failed.
    const WALL_MARKERS: [&str; 10] = [
        "⚠",
        "⛔",
        "error:",
        "error ",
        "failed",
        "exceeded",
        "has been reached",
        "have been reached",
        "limit reached",
        "quota reached",
    ];
    // Past-tense statements of a limit that was hit.
    const SPENT: [&str; 11] = [
        "ran out",
        "run out",
        "used up",
        "exhausted",
        "depleted",
        "maxed out",
        "hit the limit",
        "reached the limit",
        "out of quota",
        "out of credits",
        "no credits",
    ];
    let lower = line.to_lowercase();
    if WALL_MARKERS.iter().any(|m| lower.contains(m)) {
        return true;
    }
    if SPENT.iter().any(|m| lower.contains(m)) {
        return true;
    }
    // A number alone is not a wall: `1,429 spans were processed` and
    // `cost 429 tokens` both contain it. A 429 is a *status code*, so
    // it needs its transport context (an `error`/`http`/`status` word
    // beside it) or its canonical reason phrase.
    if contains_token(&lower, "429") {
        const STATUS_CONTEXT: [&str; 5] = ["http", "status", "code", "error", "429 too many"];
        if STATUS_CONTEXT.iter().any(|c| lower.contains(c)) {
            return true;
        }
    }
    false
}

/// Is `needle` present as its own token — not glued to letters or
/// digits on either side? `"error 429"` and `"(429)"` contain it;
/// `"1429"` does not.
fn contains_token(haystack: &str, needle: &str) -> bool {
    let mut from = 0;
    while let Some(at) = haystack[from..].find(needle) {
        let start = from + at;
        let end = start + needle.len();
        let before_ok = haystack[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric());
        let after_ok = haystack[end..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric());
        if before_ok && after_ok {
            return true;
        }
        from = end;
    }
    false
}

/// A line with no words on it — `>`, `❯`, `│ > │` — is the agent's
/// input box, not a plea.
///
/// The box is chrome: it sits at the bottom of the screen before the
/// turn, during it and after it, and it never says anything. The
/// classifier must not read it as "the agent needs you" when the
/// screen underneath it says something else (a quota wall), and the
/// summary builder must not print it as the row's evidence.
pub fn is_prompt_chrome(line: &str) -> bool {
    let flat = screen_summary_line(line);
    !flat.is_empty() && !flat.chars().any(|c| c.is_alphanumeric())
}

/// The count-down a quota wall publishes, when it publishes one.
///
/// A wall that says when it lifts ("Resets in 1h23m57s", "try again
/// in 30s") hands the operator the one number that matters. The
/// clause is taken from the wall's own words — no guessing, no
/// pattern list per vendor — and only a clause that carries both a
/// clock word and a duration counts, so prose that merely mentions
/// quota ("ready to generate the moment quota returns") is not
/// mistaken for a count-down.
fn quota_countdown(line: &str) -> Option<String> {
    const CLOCK_WORDS: [&str; 9] = [
        "reset",
        "resets",
        "available",
        "retry",
        "retries",
        "retrying",
        "try again",
        "resume",
        "expire",
    ];
    let mut found = None;
    // Sentences, not lines: the wall's own message is one line with
    // several sentences in it, and only one of them is the clock.
    for sentence in line.split(". ") {
        let sentence = sentence.trim().trim_end_matches(['.', '!', '?']).trim();
        if sentence.is_empty() {
            continue;
        }
        let lower = sentence.to_lowercase();
        if !CLOCK_WORDS.iter().any(|w| lower.contains(w)) {
            continue;
        }
        if !has_duration(&lower) {
            continue;
        }
        // The last count-down wins: a wall that repeats itself prints
        // a shrinking timer, and the newest one is the truth.
        found = Some(sentence.to_string());
    }
    found
}

/// Does this text carry a duration — `1h23m57s`, `30s`, `3pm`,
/// `45 minutes`? A digit run followed by a time unit.
fn has_duration(text: &str) -> bool {
    const UNITS: [&str; 16] = [
        "hours", "hour", "minutes", "minute", "seconds", "second", "days", "day", "hrs", "hr",
        "mins", "min", "secs", "sec", "am", "pm",
    ];
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if !chars[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        while i < chars.len() && chars[i].is_ascii_digit() {
            i += 1;
        }
        let mut j = i;
        if j < chars.len() && chars[j] == ' ' {
            j += 1;
        }
        let unit: String = chars[j..]
            .iter()
            .take_while(|c| c.is_ascii_alphabetic())
            .collect::<String>();
        if UNITS.contains(&unit.as_str()) {
            return true;
        }
        // Bare unit letter: `1h23m57s`, `30s`, `5m`. Only a letter
        // directly after the digits counts, so `2026-10-28` and a
        // word like `2 assets` do not.
        if j < chars.len() && matches!(chars[j], 'h' | 'm' | 's' | 'd') {
            return true;
        }
    }
    false
}

/// The Summary line for a quota wall.
///
/// The wall's own count-down when it has one — `quota · resets in
/// 1h23m57s` — because that is what the operator wants from a 40
/// column row: not that the agent is stuck, but when it comes back.
/// Failing a count-down, the clause that names the limit ("The usage
/// limit has been reached") rather than the head of the line: a 429
/// wrapper puts 40 columns of JSON before the sentence a human can
/// use, and a Summary column that starts with `{"type":"error"` is
/// no summary at all. Without either, the wall's line stands as
/// printed, so the evidence the agent gave is never lost.
fn limited_summary(line: &str) -> String {
    // A 429 wall often wraps its sentence in a JSON error body, where
    // the only human words live in `"message"`. Read those when they
    // are there; fall back to the line as printed.
    let inner = json_message(line);
    for src in [inner.as_deref(), Some(line)].into_iter().flatten() {
        if let Some(clause) = quota_countdown(src) {
            return screen_summary_line(&format!("quota · {}", lower_first(&clause)));
        }
    }
    if let Some(inner) = inner {
        // Inside a JSON body the sentence *is* the summary, even when
        // it survives on its own: the wrapper around it is noise.
        let clause = quota_limit_clause(&inner).unwrap_or(inner);
        return screen_summary_line(&format!("quota · {}", lower_first(clause.trim())));
    }
    if let Some(clause) = quota_limit_clause(line) {
        // Only rewrite when the row would otherwise cut the clause off:
        // `Error: rate limit reached` is already the best possible
        // summary, while a long wrapper buries it. If the clause
        // survives the truncation, keep the line exactly as printed.
        let shown = screen_summary_line(line);
        if !shown.contains(&clause) {
            return screen_summary_line(&format!("quota · {}", lower_first(&clause)));
        }
    }
    screen_summary_line(line)
}

/// The `"message"` field of a JSON error body, when the line carries
/// one. Escapes are left alone: the message is display text, and a
/// wall that escapes its own prose is still readable as printed.
///
/// An unterminated quote is normal — the terminal wraps the wall's own
/// JSON across lines, so the closing quote usually lives on the next
/// one. In that case the message is taken to the end of the line, and
/// the sentence split that follows does the trimming.
fn json_message(line: &str) -> Option<String> {
    let start = line.find("\"message\"")? + "\"message\"".len();
    let rest = line[start..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let msg = match rest.find('"') {
        Some(end) => &rest[..end],
        None => rest,
    }
    .trim();
    (!msg.is_empty()).then(|| msg.to_string())
}

/// Lower the first letter: a clause reads as a phrase after the
/// middot, not as the start of a sentence.
fn lower_first(s: &str) -> String {
    let mut out = s.to_string();
    if let Some(first) = out.get_mut(..1) {
        first.make_ascii_lowercase();
    }
    out
}

/// The sentence of a quota wall that names the wall itself — the part
/// a human can read — skipping the transport noise around it.
fn quota_limit_clause(line: &str) -> Option<String> {
    const LIMIT_WORDS: [&str; 8] = [
        "usage limit",
        "quota",
        "rate limit",
        "rate_limit",
        "limit has been reached",
        "limit reached",
        "credits",
        "429",
    ];
    for sentence in line.split(". ") {
        let sentence = sentence.trim().trim_end_matches(['.', '!', '?']).trim();
        if sentence.is_empty() {
            continue;
        }
        let lower = sentence.to_lowercase();
        if LIMIT_WORDS.iter().any(|w| lower.contains(w)) {
            return Some(sentence.to_string());
        }
    }
    None
}

/// Identify the agent for a pane and classify its screen. A command match
/// pins the agent; otherwise patterns still yield a state, but the agent
/// stays unnamed unless the screen mentions it — a bare spinner is a
/// working state with no identity, and a micro `>` bar is not gemini.
/// Callers pair anonymous states with process identity, or drop them.
/// Drop footer-carousel tips from a capture. Terminal chrome rotates
/// full text every few seconds; it must trip neither the classifier (a
/// tip mentioning "quota" is not a quota wall) nor the quiet hash. Two
/// shapes: whole lines starting with `Tip:`, and minimax's right-aligned
/// inline tail (`…Enter send…   Tip: /slash-cmd`), cut at the slash form
/// (every observed tip is a slash command; prose reading `Tip: /…` is
/// contrived enough to sacrifice). Anything else starting with "Tip" stays.
pub fn strip_tip_lines(screen: &str) -> String {
    screen
        .lines()
        .filter_map(|l| {
            if l.trim_start().starts_with("Tip:") {
                return None;
            }
            match l.find("Tip: /") {
                // Right-aligned tail: trim the padding too, or its width
                // still fingerprints the tip length.
                Some(i) => Some(l[..i].trim_end().to_string()),
                None => Some(l.to_string()),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn classify_screen(
    manifests: &[CompiledManifest],
    command: &str,
    screen: &str,
    bell: bool,
    parked: bool,
) -> Option<ScreenHit> {
    classify_screen_owned(manifests, command, screen, bell, parked, 0)
}

/// The same classifier with the pane's visible height: how many of
/// the capture's last lines are actually on the screen. Only feeds
/// the repetition check's smaller bar — anything above the
/// boundary is scrollback, and a repeat that stopped there ended
/// when the round of work finished.
///
/// `0` means "unknown" and counts the whole capture again, so callers
/// without a pane (previews, tests) are not gated by staleness.
pub fn classify_screen_owned(
    manifests: &[CompiledManifest],
    command: &str,
    screen: &str,
    bell: bool,
    parked: bool,
    visible: usize,
) -> Option<ScreenHit> {
    // Bell evidence is layered on top of pattern matching, not in
    // place of it. We append a synthetic line `🔔 bell` to the
    // capture when the kernel flag fired; any manifest `waiting:`
    // pattern that matches a real screen shape (e.g. `^❯`,
    // `\[y/n\]`) still drives the verdict the same way. A bell
    // without a matching manifest agent is ignored: a bare shell
    // beeping on completion is *not* a needs-you reason.
    //
    // `parked` is the *necessary* half of the cascade: the kernel
    // already knows the leaf is blocked waiting on stdin, but a
    // parked process alone (an `nvim`, a `fzf`, a `python` REPL)
    // isn't asking the human a question. We surface the parked
    // signal in the summary so the chip can render `⏸ tty-read`,
    // but we *don't* manufacture a Waiting verdict when the
    // pattern classifier's freshest-wins logic returned Working.
    // When parked AND bell both fire, the synthetic line carries
    // both evidence tokens so the existing waiting patterns can
    // match the conjunction.
    let augmented: String;
    let screen = if bell && parked {
        augmented = format!("{screen}\n🔔 bell · tty-read\n");
        augmented.as_str()
    } else {
        screen
    };
    let clean = strip_tip_lines(screen);
    let screen = clean.as_str();
    if let Some(m) = manifests.iter().find(|m| m.matches_command(command)) {
        let hit = m
            .classify(screen, visible)
            .map(|(state, summary, phase)| ScreenHit {
                agent: Some(m.agent.clone()),
                state,
                summary,
                phase,
            });
        return park_annotate(bell_override(hit, bell), parked);
    }
    let flat = screen.to_lowercase();
    for m in manifests {
        if let Some((state, summary, phase)) = m.classify(screen, visible) {
            let agent = flat
                .contains(m.agent.to_lowercase().as_str())
                .then(|| m.agent.clone());
            let hit = Some(ScreenHit {
                agent,
                state,
                summary,
                phase,
            });
            return park_annotate(bell_override(hit, bell), parked);
        }
    }
    bell_override(None, bell)
}

/// Park annotation: when the kernel reports the leaf process
/// parked in `n_tty_read`, append a `⏸ tty-read` token to the
/// summary. This does NOT promote Working → Waiting (the pattern
/// classifier's tail-driven verdict is the source of truth for
/// the verdict itself; a parked `nvim` is parked on a vim key, not
/// an input plea). It only annotates the existing verdict so the
/// row chip can show *why* a pane is sitting at `n_tty_read`.
fn park_annotate(hit: Option<ScreenHit>, parked: bool) -> Option<ScreenHit> {
    if !parked {
        return hit;
    }
    hit.map(|mut h| {
        if !h.summary.contains("tty-read") {
            h.summary = format!("⏸ tty-read · {}", h.summary);
        }
        h
    })
}

/// If the bell flag fired, prefer the Waiting verdict (and surface
/// `bell` in the summary the reason chip reads). A bare bell with no
/// manifest match — e.g. a shell that beeped on completion — is
/// dropped: it was noise, not a plea for input.
///
/// The bell never outranks work the screen proves is happening. A
/// terminal bell is a *notification* ("something happened"), and
/// tmux's flag is sticky — it stays on until the window is visited —
/// so a flag that fired minutes ago says nothing about this poll. An
/// agent that is demonstrably mid-work (a spinner, a running subagent
/// bar) stays Working: the Gemini CLI rings the bell when a subagent
/// finishes, and reading that as "needs you" pinned six busy panes
/// in `waiting` forever. This is the same rule the park annotation
/// already follows: a kernel signal annotates the verdict, it does not
/// manufacture one.
fn bell_override(hit: Option<ScreenHit>, bell: bool) -> Option<ScreenHit> {
    if !bell {
        return hit;
    }
    match hit {
        Some(mut h) => {
            // Bell alone upgrades to Waiting; the summary carries
            // `🔔 bell` so the chip can render it next to whatever
            // waiting line matched. If the underlying state is
            // already attention (Error/Stuck/Limited) we keep that
            // — a bell during a crashed agent is redundant noise.
            // Working is kept too: the screen says the agent is busy,
            // and the bell cannot argue with that.
            if matches!(
                h.state,
                AgentState::Waiting | AgentState::Idle | AgentState::Done
            ) {
                h.state = AgentState::Waiting;
                h.summary = format!("🔔 {}", h.summary);
            }
            Some(h)
        }
        // No manifest hit + bell fired = nothing actionable. The
        // program was just a bare shell that beeped. Let the caller
        // treat it like a normal non-match.
        None => None,
    }
}

/// The source a record keeps after a hook event lands on it.
///
/// A manual pin (`terhub register`) is deliberate human state: it
/// never lapses (`split_prev` keeps it), the poll merge leaves it
/// alone ("manual pins are exempt"), and `terhub unregister` is the
/// only thing that removes it. A hook event about the *same* agent
/// updates the pin's content — state, summary, prompt — without
/// demoting it to a hook record: the demotion would silently hand the
/// record to the hook's 30-minute freshness clock and make
/// `unregister` refuse to remove it ("no manual pin on {pane}").
///
/// A hook event about a *different* agent is the pane being repurposed
/// (the exit-pi/start-muse case the poll merge already reasons about);
/// it replaces the pin outright.
fn source_after_hook(old: Option<&AgentRecord>, agent: &str) -> Source {
    match old {
        Some(r) if r.source == Source::Manual && r.agent == agent => Source::Manual,
        _ => Source::Hook,
    }
}

/// Merge helper for tests and the daemon: fold hook events over a registry.
/// The hook label canonicalizes first (`claude-code` → `claude`), so hook
/// records compare equal to proc/screen records in every downstream check.
pub fn apply_event_to(registry: &mut HashMap<String, AgentRecord>, ev: &HookEvent, now: u64) {
    let agent = crate::proc::canonical_agent(&ev.agent);
    match registry.get(&ev.pane) {
        Some(old) if old.state == ev.state && old.agent == agent => {
            let mut rec = old.clone();
            rec.source = source_after_hook(Some(old), &agent);
            rec.summary = ev.summary.clone();
            rec.updated_at = now;
            if !ev.prompt.is_empty() {
                rec.last_prompt = cap_chars(&ev.prompt, RECORD_PROMPT_CAP);
                rec.last_prompt_at = now;
            }
            if !ev.model.is_empty() {
                rec.model = ev.model.clone();
            }
            if ev.ctx.is_some() {
                rec.ctx_pct = ev.ctx;
            }
            registry.insert(ev.pane.clone(), rec);
        }
        _ => {
            // State or agent moved on: fresh clock, but keep the old pane
            // metadata so the record isn't nameless until the next poll.
            let old = registry.get(&ev.pane);
            let last_turn_secs = match old {
                Some(o) if turn_ended(o.state, ev.state) => now.saturating_sub(o.state_since),
                Some(o) => o.last_turn_secs,
                None => 0,
            };
            // Handles follow the agent: a changed agent drops the old
            // name so ensure_handles mints a fresh one.
            let handle = match old {
                Some(o) if o.agent == agent => o.handle.clone(),
                _ => String::new(),
            };
            // Entering Working (or a new agent taking the pane) is
            // activity in itself, pixels or not: restart the
            // visible-quiet clock so the Idle column reads fresh
            // instead of the previous stillness.
            let fresh_work =
                ev.state == AgentState::Working || old.map(|r| r.agent != agent).unwrap_or(true);
            // Resolved before the record is built: the agent name moves
            // into the record below.
            let source = source_after_hook(old, &agent);
            registry.insert(
                ev.pane.clone(),
                AgentRecord {
                    pane: ev.pane.clone(),
                    agent,
                    state: ev.state,
                    source,
                    summary: ev.summary.clone(),
                    handle,
                    // Model/ctx/pid ride along when the event knows them,
                    // else the old cut carries (a hook that can't report
                    // them must not blank a wrapper's earlier report).
                    model: if ev.model.is_empty() {
                        old.map(|r| r.model.clone()).unwrap_or_default()
                    } else {
                        ev.model.clone()
                    },
                    ctx_pct: ev.ctx.or_else(|| old.and_then(|r| r.ctx_pct)),
                    reg_pid: old.map(|r| r.reg_pid).unwrap_or(0),
                    session: old.map(|r| r.session.clone()).unwrap_or_default(),
                    window: old.map(|r| r.window.clone()).unwrap_or_default(),
                    command: old.map(|r| r.command.clone()).unwrap_or_default(),
                    updated_at: now,
                    state_since: now,
                    cpu_ticks: 0,
                    cpu_still_since: 0,
                    agent_start: 0,
                    last_turn_secs,
                    // A hook event is not a screen change: carry the hash.
                    // Fresh work restarts the visible-quiet clock (see
                    // `fresh_work` above); demotions into Idle keep the
                    // old clock — that stillness is the point.
                    screen_hash: old.map(|r| r.screen_hash).unwrap_or(0),
                    quiet_since: if fresh_work {
                        now
                    } else {
                        old.map(|r| r.quiet_since).unwrap_or(0)
                    },
                    // A prompt-less event keeps the old prompt, not a blank.
                    last_prompt_at: if ev.prompt.is_empty() {
                        old.map(|r| r.last_prompt_at).unwrap_or(0)
                    } else {
                        now
                    },
                    last_prompt: if ev.prompt.is_empty() {
                        old.map(|r| r.last_prompt.clone()).unwrap_or_default()
                    } else {
                        cap_chars(&ev.prompt, RECORD_PROMPT_CAP)
                    },
                    // Bell evidence is screen-driven; a hook event
                    // never sets or clears it, so carry forward.
                    bell: old.map(|r| r.bell).unwrap_or(false),
                    bell_reason: old.map(|r| r.bell_reason.clone()).unwrap_or_default(),
                    stuck_base: None,
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broken_manifests_warn_instead_of_vanishing() {
        let dir = std::env::temp_dir().join(format!("terhub-manifests-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("good.toml"),
            "agent = \"custom\"\nwaiting = [\"approve?\"]\n",
        )
        .unwrap();
        std::fs::write(dir.join("bad.toml"), "agent = [unclosed\n").unwrap();
        std::fs::write(dir.join("notes.txt"), "ignored").unwrap();
        let (manifests, warnings) = load_manifests(&dir);
        assert_eq!(manifests.len(), 1);
        assert_eq!(manifests[0].agent, "custom");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("bad.toml"), "{}", warnings[0]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn manifests_load_in_filename_order() {
        // First-match-wins classification needs deterministic order:
        // files created z-to-a must still load a-to-z.
        let dir =
            std::env::temp_dir().join(format!("terhub-manifests-order-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["h", "g", "f", "e", "d", "c", "b", "a"] {
            std::fs::write(
                dir.join(format!("{name}.toml")),
                format!("agent = \"{name}\"\nwaiting = [\"{name} waiting\"]\n"),
            )
            .unwrap();
        }
        let (manifests, warnings) = load_manifests(&dir);
        assert!(warnings.is_empty(), "{warnings:?}");
        let agents: Vec<&str> = manifests.iter().map(|m| m.agent.as_str()).collect();
        assert_eq!(agents, ["a", "b", "c", "d", "e", "f", "g", "h"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn handles_mint_stable_and_suffixed() {
        use std::collections::HashSet;
        let used: HashSet<String> = HashSet::new();
        assert_eq!(mint_handle(&used, "pi"), "@pi");
        assert_eq!(mint_handle(&used, "My Agent!"), "@my-agent");
        let mut used: HashSet<String> = ["@pi".to_string()].into_iter().collect();
        assert_eq!(mint_handle(&used, "pi"), "@pi-2");
        used.insert("@pi-2".to_string());
        assert_eq!(mint_handle(&used, "pi"), "@pi-3");
    }

    #[test]
    fn strip_legacy_handles_rewrites_only_old_mint() {
        let mk = |pane: &str, agent: &str, session: &str, handle: &str| AgentRecord {
            pane: pane.to_string(),
            agent: agent.to_string(),
            state: AgentState::Working,
            source: Source::Hook,
            summary: String::new(),
            session: session.to_string(),
            window: String::new(),
            command: String::new(),
            updated_at: 0,
            state_since: 0,
            cpu_ticks: 0,
            cpu_still_since: 0,
            agent_start: 0,
            last_turn_secs: 0,
            screen_hash: 0,
            quiet_since: 0,
            last_prompt_at: 0,
            last_prompt: String::new(),
            handle: handle.to_string(),
            model: String::new(),
            ctx_pct: None,
            reg_pid: 0,
            bell: false,
            bell_reason: String::new(),
            stuck_base: None,
        };
        let mut reg = HashMap::new();
        // Old mint, bare and suffixed: both go.
        reg.insert("%1".to_string(), mk("%1", "pi", "wez", "@pi-wez"));
        reg.insert("%2".to_string(), mk("%2", "pi", "wez", "@pi-wez-2"));
        // Old mint with a slugged session.
        reg.insert(
            "%3".to_string(),
            mk("%3", "claude", "My Project!", "@claude-my-project"),
        );
        reg.insert(
            "%4".to_string(),
            mk("%4", "claude", "My Project!", "@claude-my-project-2"),
        );
        // Not the old mint: sessionless (new), a bare session, and a
        // hand-set name that merely starts alike — all keep.
        reg.insert("%5".to_string(), mk("%5", "pi", "wez", "@pi"));
        reg.insert("%6".to_string(), mk("%6", "pi", "", "@pi-2"));
        reg.insert(
            "%7".to_string(),
            mk("%7", "claude", "s", "@claude-s-custom"),
        );
        // Old mint shape but the suffix isn't digits: keep.
        reg.insert("%8".to_string(), mk("%8", "pi", "wez", "@pi-wez-two"));
        assert_eq!(strip_legacy_handles(&mut reg), 4);
        assert_eq!(reg["%1"].handle, "");
        assert_eq!(reg["%2"].handle, "");
        assert_eq!(reg["%3"].handle, "");
        assert_eq!(reg["%4"].handle, "");
        assert_eq!(reg["%5"].handle, "@pi");
        assert_eq!(reg["%6"].handle, "@pi-2");
        assert_eq!(reg["%7"].handle, "@claude-s-custom");
        assert_eq!(reg["%8"].handle, "@pi-wez-two");
        // After stripping, ensure_handles re-mints sessionless against
        // the surviving set, in sorted pane order: @pi and @pi-2 stay
        // taken, so the two pi records take @pi-3/@pi-4 and the two
        // claude records take @claude/@claude-2.
        ensure_handles(&mut reg);
        assert_eq!(reg["%1"].handle, "@pi-3");
        assert_eq!(reg["%2"].handle, "@pi-4");
        assert_eq!(reg["%3"].handle, "@claude");
        assert_eq!(reg["%4"].handle, "@claude-2");
    }

    #[test]
    fn ensure_handles_keeps_minted_assigns_sorted() {
        let mut reg = HashMap::new();
        for pane in ["%9", "%1"] {
            reg.insert(
                pane.to_string(),
                AgentRecord {
                    pane: pane.to_string(),
                    agent: "pi".to_string(),
                    state: AgentState::Working,
                    source: Source::Hook,
                    summary: String::new(),
                    session: "wez".to_string(),
                    window: String::new(),
                    command: String::new(),
                    updated_at: 0,
                    state_since: 0,
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
        }
        ensure_handles(&mut reg);
        // Sorted panes mint in order: lowest pane takes the bare handle.
        assert_eq!(reg["%1"].handle, "@pi");
        assert_eq!(reg["%9"].handle, "@pi-2");
        // Re-runs never re-mint: handles are stable for the record's life.
        ensure_handles(&mut reg);
        assert_eq!(reg["%1"].handle, "@pi");
        // Agent-less records stay handle-less.
        reg.insert(
            "%2".to_string(),
            AgentRecord {
                pane: "%2".to_string(),
                ..reg["%1"].clone()
            },
        );
        reg.get_mut("%2").unwrap().agent.clear();
        reg.get_mut("%2").unwrap().handle.clear();
        ensure_handles(&mut reg);
        assert!(reg["%2"].handle.is_empty());
    }

    #[test]
    fn resolve_handle_matches_exact() {
        let recs = vec![AgentRecord {
            pane: "%1".to_string(),
            agent: "pi".to_string(),
            state: AgentState::Working,
            source: Source::Hook,
            summary: String::new(),
            session: "wez".to_string(),
            window: String::new(),
            command: String::new(),
            updated_at: 0,
            state_since: 0,
            cpu_ticks: 0,
            cpu_still_since: 0,
            agent_start: 0,
            last_turn_secs: 0,
            screen_hash: 0,
            quiet_since: 0,
            last_prompt_at: 0,
            last_prompt: String::new(),
            handle: "@pi-wez".to_string(),
            model: String::new(),
            ctx_pct: None,
            reg_pid: 0,
            bell: false,
            bell_reason: String::new(),
            stuck_base: None,
        }];
        assert_eq!(resolve_handle(&recs, "@pi-wez"), Some("%1".to_string()));
        assert_eq!(resolve_handle(&recs, "@pi-other"), None);
        assert_eq!(resolve_handle(&recs, "%1"), None);
    }

    #[test]
    fn a_hook_event_does_not_demote_a_manual_pin() {
        // A manual pin (`terhub register`) is deliberate human state:
        // it never lapses, the poll merge leaves it alone, and
        // `terhub unregister` is the only thing that removes it. A hook
        // event about the same agent updates the pin's content but must
        // not re-label it as a hook record: the demotion hands the
        // record to the hook's 30-minute freshness clock and makes
        // unregister refuse to remove it.
        let mut reg = HashMap::new();
        let mut pinned = AgentRecord {
            pane: "%1".to_string(),
            agent: "pi".to_string(),
            state: AgentState::Waiting,
            source: Source::Manual,
            summary: String::new(),
            handle: String::new(),
            model: String::new(),
            ctx_pct: None,
            reg_pid: 4242,
            session: String::new(),
            window: String::new(),
            command: String::new(),
            updated_at: 100,
            state_since: 100,
            cpu_ticks: 0,
            cpu_still_since: 0,
            agent_start: 0,
            last_turn_secs: 0,
            screen_hash: 0,
            quiet_since: 0,
            last_prompt_at: 0,
            last_prompt: String::new(),
            bell: false,
            bell_reason: String::new(),
            stuck_base: None,
        };
        pinned.summary = "needs input".to_string();
        reg.insert("%1".to_string(), pinned);

        // Same state, same agent: the pin keeps its source, and the
        // hook's fresh summary still lands.
        let ev = HookEvent {
            pane: "%1".to_string(),
            agent: "pi".to_string(),
            state: AgentState::Waiting,
            summary: "needs input".to_string(),
            prompt: String::new(),
            model: String::new(),
            ctx: None,
        };
        apply_event_to(&mut reg, &ev, 200);
        assert_eq!(
            reg["%1"].source,
            Source::Manual,
            "the hook event demoted the manual pin"
        );
        assert_eq!(reg["%1"].reg_pid, 4242, "the pin lost its pid");

        // A different state, same agent: still the human's pin.
        let ev = HookEvent {
            pane: "%1".to_string(),
            agent: "pi".to_string(),
            state: AgentState::Working,
            summary: "working".to_string(),
            prompt: String::new(),
            model: String::new(),
            ctx: None,
        };
        apply_event_to(&mut reg, &ev, 300);
        assert_eq!(
            reg["%1"].source,
            Source::Manual,
            "a state change demoted the manual pin"
        );

        // A different agent is the pane being repurposed: the hook
        // replaces the pin outright (the poll merge reasons the same
        // way about exit-pi/start-muse).
        let ev = HookEvent {
            pane: "%1".to_string(),
            agent: "claude".to_string(),
            state: AgentState::Working,
            summary: String::new(),
            prompt: String::new(),
            model: String::new(),
            ctx: None,
        };
        apply_event_to(&mut reg, &ev, 400);
        assert_eq!(
            reg["%1"].source,
            Source::Hook,
            "a different agent must take the pane over"
        );
        assert_eq!(reg["%1"].agent, "claude");
    }

    #[test]
    fn hook_prompt_carries_to_record() {
        let mut reg = HashMap::new();
        let ev = HookEvent {
            pane: "%1".to_string(),
            agent: "pi".to_string(),
            state: AgentState::Working,
            summary: String::new(),
            prompt: "fix it".to_string(),
            model: String::new(),
            ctx: None,
        };
        apply_event_to(&mut reg, &ev, 100);
        assert_eq!(reg["%1"].last_prompt, "fix it");
        assert_eq!(reg["%1"].last_prompt_at, 100);
        // A prompt-less refresh keeps the old prompt, not a blank.
        let ev2 = HookEvent {
            pane: "%1".to_string(),
            agent: "pi".to_string(),
            state: AgentState::Working,
            summary: String::new(),
            prompt: String::new(),
            model: String::new(),
            ctx: None,
        };
        apply_event_to(&mut reg, &ev2, 200);
        assert_eq!(reg["%1"].last_prompt, "fix it");
        assert_eq!(reg["%1"].last_prompt_at, 100);
    }

    #[test]
    fn hook_idle_to_working_restarts_quiet_clock() {
        // Reported bug: starting work in an idle pane flipped State to
        // working but the Idle column kept showing the old stillness
        // until the pixels visibly moved. Entering Working is activity
        // in itself, so the hook restarts the visible-quiet clock.
        let now = 100_000;
        let mut reg = HashMap::new();
        let idle = HookEvent {
            pane: "%1".to_string(),
            agent: "pi".to_string(),
            state: AgentState::Idle,
            summary: "session ended".to_string(),
            prompt: String::new(),
            model: String::new(),
            ctx: None,
        };
        apply_event_to(&mut reg, &idle, now - 900);
        // Long-still screen behind the idle record.
        reg.get_mut("%1").unwrap().quiet_since = now - 900;
        let work = HookEvent {
            pane: "%1".to_string(),
            agent: "pi".to_string(),
            state: AgentState::Working,
            summary: String::new(),
            prompt: "fix it".to_string(),
            model: String::new(),
            ctx: None,
        };
        apply_event_to(&mut reg, &work, now);
        assert_eq!(reg["%1"].state, AgentState::Working);
        assert_eq!(reg["%1"].state_since, now);
        assert_eq!(
            reg["%1"].quiet_since, now,
            "resumed work kept showing the old stillness"
        );
        // A demotion back into Idle keeps the clock — that stillness
        // is the whole reason it demoted.
        let idle2 = HookEvent {
            pane: "%1".to_string(),
            agent: "pi".to_string(),
            state: AgentState::Idle,
            summary: "response finished".to_string(),
            prompt: String::new(),
            model: String::new(),
            ctx: None,
        };
        apply_event_to(&mut reg, &idle2, now + 10);
        assert_eq!(reg["%1"].state, AgentState::Idle);
        assert_eq!(
            reg["%1"].quiet_since, now,
            "demotion must not whitewash stillness"
        );
        // A new agent taking the pane restarts the clock whatever it
        // reports: the predecessor's stillness is not its own.
        let takeover = HookEvent {
            pane: "%1".to_string(),
            agent: "muse".to_string(),
            state: AgentState::Waiting,
            summary: "approve?".to_string(),
            prompt: String::new(),
            model: String::new(),
            ctx: None,
        };
        apply_event_to(&mut reg, &takeover, now + 20);
        assert_eq!(reg["%1"].agent, "muse");
        assert_eq!(reg["%1"].quiet_since, now + 20);
    }

    #[test]
    fn hook_event_sets_model_and_ctx() {
        let mut reg = HashMap::new();
        let ev = HookEvent {
            pane: "%1".to_string(),
            agent: "pi".to_string(),
            state: AgentState::Working,
            summary: String::new(),
            prompt: String::new(),
            model: "opus-4.5".to_string(),
            ctx: Some(72),
        };
        apply_event_to(&mut reg, &ev, 100);
        assert_eq!(reg["%1"].model, "opus-4.5");
        assert_eq!(reg["%1"].ctx_pct, Some(72));
        // Same-state refresh updates both.
        let ev2 = HookEvent {
            pane: "%1".to_string(),
            agent: "pi".to_string(),
            state: AgentState::Working,
            summary: String::new(),
            prompt: String::new(),
            model: "opus-4.6".to_string(),
            ctx: Some(80),
        };
        apply_event_to(&mut reg, &ev2, 200);
        assert_eq!(reg["%1"].model, "opus-4.6");
        assert_eq!(reg["%1"].ctx_pct, Some(80));
        // An event that doesn't know keeps the old values, not blanks.
        let ev3 = HookEvent {
            pane: "%1".to_string(),
            agent: "pi".to_string(),
            state: AgentState::Waiting,
            summary: String::new(),
            prompt: String::new(),
            model: String::new(),
            ctx: None,
        };
        apply_event_to(&mut reg, &ev3, 300);
        assert_eq!(reg["%1"].model, "opus-4.6");
        assert_eq!(reg["%1"].ctx_pct, Some(80));
    }

    #[test]
    fn hook_agent_names_canonicalize() {
        // Hooks report raw labels; the registry stores canonical names so
        // hook records compare equal to proc/screen records downstream.
        let mut reg = HashMap::new();
        let ev = HookEvent {
            pane: "%1".to_string(),
            agent: "claude-code".to_string(),
            state: AgentState::Waiting,
            summary: String::new(),
            prompt: String::new(),
            model: String::new(),
            ctx: None,
        };
        apply_event_to(&mut reg, &ev, 100_000);
        assert_eq!(reg["%1"].agent, "claude");
        let ev2 = HookEvent {
            pane: "%2".to_string(),
            agent: "MyAgent".to_string(),
            state: AgentState::Working,
            summary: String::new(),
            prompt: String::new(),
            model: String::new(),
            ctx: None,
        };
        apply_event_to(&mut reg, &ev2, 100_000);
        assert_eq!(reg["%2"].agent, "myagent");
    }

    fn compiled() -> Vec<CompiledManifest> {
        compile_all(&builtin_manifests()).unwrap()
    }

    fn hit(agent: Option<&str>, state: AgentState, summary: &str) -> Option<ScreenHit> {
        Some(ScreenHit {
            agent: agent.map(str::to_string),
            state,
            summary: summary.to_string(),
            phase: None,
        })
    }

    #[test]
    fn state_parse_aliases() {
        assert_eq!(AgentState::parse("working"), Some(AgentState::Working));
        assert_eq!(AgentState::parse("needs-input"), Some(AgentState::Waiting));
        assert_eq!(AgentState::parse("BLOCKED"), Some(AgentState::Waiting));
        assert_eq!(AgentState::parse("failed"), Some(AgentState::Error));
        // "done" used to fold into Idle; it's now its own variant. The
        // classifier still does the heavier lifting (process exit 0,
        // banner match) but parsing must agree.
        assert_eq!(AgentState::parse("done"), Some(AgentState::Done));
        assert_eq!(AgentState::parse("complete"), Some(AgentState::Done));
        assert_eq!(AgentState::parse("idle"), Some(AgentState::Idle));
        assert_eq!(AgentState::parse("ready"), Some(AgentState::Idle));
        assert_eq!(AgentState::parse("quota"), Some(AgentState::Limited));
        assert_eq!(AgentState::parse("CAPPED"), Some(AgentState::Limited));
        assert_eq!(AgentState::parse("bogus"), None);
    }

    #[test]
    fn done_state_is_quiet_not_attention() {
        // A finished agent must never badge as needing action: it's
        // the visual whole point of the Done FYI row.
        assert!(!AgentState::Done.needs_attention());
    }

    #[test]
    fn paused_state_parses_and_labels() {
        // The pauser registry writes "paused" into the registry and the
        // TUI reads it back, so the label and the parse must agree.
        assert_eq!(AgentState::parse("paused"), Some(AgentState::Paused));
        assert_eq!(AgentState::parse("stopped"), Some(AgentState::Paused));
        assert_eq!(AgentState::parse("FROZEN"), Some(AgentState::Paused));
        assert_eq!(AgentState::parse("suspended"), Some(AgentState::Paused));
        assert_eq!(AgentState::Paused.label(), "paused");
        // Label and parse round-trip: whatever we render, we can read.
        assert_eq!(
            AgentState::parse(AgentState::Paused.label()),
            Some(AgentState::Paused)
        );
    }

    #[test]
    fn an_unknown_state_name_decodes_as_unknown() {
        // A name this build has never heard of is future information,
        // not corruption. The whole snapshot is one JSON document, so
        // failing here used to fail the entire response — and the
        // client then reported "daemon not running" while a healthy
        // daemon was serving. The pane must survive.
        let state: AgentState = serde_json::from_str("\"hyperfocus\"").unwrap();
        assert_eq!(state, AgentState::Unknown);
        // The known names still decode exactly.
        for (name, want) in [
            ("paused", AgentState::Paused),
            ("working", AgentState::Working),
            ("waiting", AgentState::Waiting),
            ("error", AgentState::Error),
            ("done", AgentState::Done),
            ("idle", AgentState::Idle),
            ("stuck", AgentState::Stuck),
            ("limited", AgentState::Limited),
            ("unknown", AgentState::Unknown),
        ] {
            let got: AgentState = serde_json::from_str(&format!("\"{name}\"")).unwrap();
            assert_eq!(got, want, "{name}");
        }
    }

    #[test]
    fn an_unknown_source_decodes_as_screen() {
        // Same rule as the state: the weakest evidence there is, so the
        // UI renders it exactly as it renders any source it cannot
        // place, and the record survives.
        let source: Source = serde_json::from_str("\"telepathy\"").unwrap();
        assert_eq!(source, Source::Screen);
        let known: Source = serde_json::from_str("\"hook\"").unwrap();
        assert_eq!(known, Source::Hook);
    }

    #[test]
    fn paused_state_is_quiet_not_attention() {
        // The whole point of the pauser registry: a pane terhub paused
        // itself must never be reported as needing the user. It is
        // deliberate, not a hang, not an error.
        assert!(!AgentState::Paused.needs_attention());
    }

    #[test]
    fn paused_state_round_trips_through_serde() {
        // The registry persists the state as a lowercase string. A state
        // that cannot be read back would silently drop the pane's
        // "paused" mark on the next daemon restart.
        let json = serde_json::to_string(&AgentState::Paused).unwrap();
        assert_eq!(json, "\"paused\"");
        assert_eq!(
            serde_json::from_str::<AgentState>(&json).unwrap(),
            AgentState::Paused
        );
    }

    #[test]
    fn a_paused_record_round_trips_through_serde() {
        // The full record is what the registry writes: the paused mark
        // has to survive the trip through registry.json.
        let rec = AgentRecord {
            pane: "%3".to_string(),
            agent: "pi".to_string(),
            state: AgentState::Paused,
            source: Source::Proc,
            summary: "paused".to_string(),
            session: "work".to_string(),
            window: "1".to_string(),
            command: "pi".to_string(),
            updated_at: 1_700_000_000,
            state_since: 1_700_000_000,
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
        };
        let json = serde_json::to_string(&rec).unwrap();
        assert!(json.contains("\"paused\""), "state must persist as paused");
        let back: AgentRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.state, AgentState::Paused);
        assert_eq!(back.summary, "paused");
    }

    #[test]
    fn bell_flip_promotes_to_waiting_with_reason() {
        // A pane with a matching manifest agent whose tmux
        // `window_bell_flag` flipped since the last tick must
        // classify as Waiting with a `🔔` reason in the summary —
        // the bell is the only "look at me" evidence on a screen that
        // shows a finished turn and nothing else.
        let m = CompiledManifest::compile(&Manifest {
            tail: None,
            agent: "x".to_string(),
            commands: vec!["x".to_string()],
            working: vec!["loading".to_string()],
            waiting: vec!["^❯".to_string()],
            error: vec![],
            limited: vec![],
            done: vec!["^✓".to_string()],
            idle: vec![],
            idle_fallback: vec![],
            waiting_unless: vec![],
            busy: vec![],
            phase: vec![],
        })
        .unwrap();
        let manifests = vec![m];
        // A finished turn reads as Done; the bell turns it into the
        // "needs you" verdict, and the summary carries the `🔔` reason
        // so the chip can render it.
        let finished_screen = "✓ todo list 6/6 completed\n";
        assert_eq!(
            classify_screen(&manifests, "x", finished_screen, false, false).map(|h| h.state),
            Some(AgentState::Done)
        );
        let hit = classify_screen(&manifests, "x", finished_screen, true, false).unwrap();
        assert_eq!(
            hit.state,
            AgentState::Waiting,
            "bell didn't upgrade to Waiting"
        );
        assert!(
            hit.summary.contains("🔔"),
            "summary missing bell reason: {hit:?}"
        );
    }

    #[test]
    fn bell_never_outranks_live_work() {
        // The bell is a notification, and tmux's flag is sticky: it
        // stays on until the window is visited. An agent that is
        // demonstrably mid-work keeps that verdict — the Gemini CLI
        // rings the bell when a subagent finishes, and reading that as
        // "needs you" pinned busy panes in `waiting` forever.
        let m = CompiledManifest::compile(&Manifest {
            tail: None,
            agent: "x".to_string(),
            commands: vec!["x".to_string()],
            working: vec!["loading".to_string()],
            waiting: vec!["^❯".to_string()],
            error: vec![],
            limited: vec![],
            done: vec![],
            idle: vec![],
            idle_fallback: vec![],
            waiting_unless: vec![],
            busy: vec![],
            phase: vec![],
        })
        .unwrap();
        let manifests = vec![m];
        let working_screen = "loading 0.1\n";
        assert_eq!(
            classify_screen(&manifests, "x", working_screen, false, false).map(|h| h.state),
            Some(AgentState::Working)
        );
        // With bell: the screen's verdict stands.
        let hit = classify_screen(&manifests, "x", working_screen, true, false).unwrap();
        assert_eq!(
            hit.state,
            AgentState::Working,
            "bell overrode live work: {hit:?}"
        );
        assert!(
            !hit.summary.contains("🔔"),
            "a working row must not wear a bell chip: {hit:?}"
        );
    }

    #[test]
    fn parked_promotes_with_prompt_tail_demotes_without() {
        // The cascade contract: a kernel-reported parked process
        // (`S` in `n_tty_read`) plus a prompt-shape line in the
        // freshest 15 lines must read as Waiting. The same parked
        // state *without* a prompt-shape line must read as Working
        // (not Waiting) — every `nvim` / `fzf` / REPL is parked
        // but not asking the human a question.
        let m = CompiledManifest::compile(&Manifest {
            tail: None,
            agent: "x".to_string(),
            commands: vec!["x".to_string()],
            working: vec!["loading".to_string()],
            waiting: vec!["^❯".to_string()],
            error: vec![],
            limited: vec![],
            done: vec![],
            idle: vec![],
            idle_fallback: vec![],
            waiting_unless: vec![],
            busy: vec![],
            phase: vec![],
        })
        .unwrap();
        let manifests = vec![m];
        // Parked + prompt tail: Waiting (the pattern drives it,
        // the park annotation is informational).
        let prompt_screen = "loading 0.1\n❯ do you want to proceed?\n";
        let hit = classify_screen(&manifests, "x", prompt_screen, false, true).unwrap();
        assert_eq!(
            hit.state,
            AgentState::Waiting,
            "parked + prompt didn't promote"
        );
        assert!(
            hit.summary.contains("⏸ tty-read"),
            "summary missing park annotation: {hit:?}"
        );
        // Parked + no prompt tail: stays Working (the contract:
        // a parked process alone is not a needs-you reason).
        let quiet_screen = "loading 0.1\n";
        let hit = classify_screen(&manifests, "x", quiet_screen, false, true).unwrap();
        assert_eq!(
            hit.state,
            AgentState::Working,
            "parked alone wrongly promoted to Waiting"
        );
    }

    #[test]
    fn done_pattern_promotes_matching_tail() {
        // A manifest that moved its completion banner to `done:`
        // must classify a matching screen as `Done`, not `Idle`.
        let m = CompiledManifest::compile(&Manifest {
            tail: None,
            agent: "x".to_string(),
            commands: vec!["x".to_string()],
            working: vec![],
            waiting: vec![],
            error: vec![],
            limited: vec![],
            done: vec!["all tasks passed".to_string()],
            idle: vec![],
            idle_fallback: vec![],
            waiting_unless: vec![],
            busy: vec![],
            phase: vec![],
        })
        .unwrap();
        assert_eq!(
            m.classify("Summary\nall tasks passed · 0 failed\n", 0)
                .unwrap()
                .0,
            AgentState::Done
        );
    }

    #[test]
    fn default_manifest_does_not_promote_done() {
        // Contract item 6: a default manifest (one that hasn't migrated
        // to `done:`) must NOT promote a completion banner to `Done`.
        // All four builtin manifests ship with `done: vec![]` and
        // `idle: vec![]`, so "all tasks passed" is a no-signal line —
        // it falls through to `Working` (the default) or `Idle` (when
        // the manifest has a chrome box), never `Done`. A regression
        // here would silently mark every default-builtin pane Done.
        let m = CompiledManifest::compile(&Manifest {
            tail: None,
            agent: "x".to_string(),
            commands: vec!["x".to_string()],
            working: vec!["loading".to_string()],
            waiting: vec!["^❯".to_string()],
            error: vec![],
            limited: vec![],
            done: vec![],
            idle: vec![],
            idle_fallback: vec![],
            waiting_unless: vec![],
            busy: vec![],
            phase: vec![],
        })
        .unwrap();
        let state = m
            .classify("Summary\nall tasks passed · 0 failed\n", 0)
            .map(|(s, _, _)| s);
        assert_ne!(
            state,
            Some(AgentState::Done),
            "default manifest with no `done:` and no `idle:` must NOT promote to Done (got {state:?})"
        );
    }

    #[test]
    fn legacy_idle_list_promotes_to_done() {
        // Goal text: legacy `idle:` lists are repurposed as the source
        // for `Done` classifications (the rename would be silent
        // breakage otherwise). A manifest that hasn't migrated to
        // `done:` still works — its `idle:` patterns promote matches
        // to `Done`, the same verdict a `done:` list would give.
        let m = CompiledManifest::compile(&Manifest {
            tail: None,
            agent: "x".to_string(),
            commands: vec!["x".to_string()],
            working: vec![],
            waiting: vec![],
            error: vec![],
            limited: vec![],
            done: vec![],
            idle: vec!["all tasks passed".to_string()],
            idle_fallback: vec![],
            waiting_unless: vec![],
            busy: vec![],
            phase: vec![],
        })
        .unwrap();
        assert_eq!(
            m.classify("Summary\nall tasks passed · 0 failed\n", 0)
                .unwrap()
                .0,
            AgentState::Done,
            "legacy `idle:` list must still promote to Done — the bare rename would be silent breakage"
        );
    }

    /// Real pi screen, captured from a pane sitting at pi's blocking
    /// question UI (options with a `❯` caret, numbered, `Type something.`
    /// in the tail), followed by the footer that names the keys. Repo
    /// paths sanitized. This is the shape a `waiting` verdict must come
    /// from — before the pi manifest existed it only matched because
    /// claude's `^❯` happened to line up.
    const PI_QUESTION_SCREEN: &str = "\
 What should I work on in football-forever?
\
❯ 1. Run the next SPEC loop pass
\
     Resume the SPEC.md / src/ self-audit loop. Read SPEC.md §0.2 to find the next open `### 15.X` slot, impl
\
     unit, smoke, prod, svelte-check).
\
  2. Start the dev server
\
     Boot `bun run dev:football-forever` (or the local equivalent) so you can poke at it in the browser via ch
\
  3. Run the platform audit
\
     Run `bun run audit` from web/games to check gitlink drift, vendored-deps integrity, favicons, static-asse
\
     summary.
\
  4. Read PROGRESS.md / SPEC.md and propose
\
     Read the current spec + progress, then come back with a concrete proposal of the highest-leverage next st
\
  5. Type something.
\
──────────────────────────────────────
\
 Enter to select · ↑/↓ to navigate · n to add notes · Esc to cancel · Ctrl+] to collapse
";

    /// Real pi screen mid-work: a subagent audit's widget lines, then
    /// the bottom chrome (git line, stats line, Chrome Bridge line).
    /// The chrome is identical here and on a finished pane, so this
    /// fixture is what proves the manifest claims nothing for working.
    const PI_WORKING_SCREEN: &str = "\
──────────────────────────────────────
\
──────────────────────────────────────
\
 ⏸ ok reloaded aduit it · paused · total 2h 49m
\
 ├─ completion audit: parked — no review recorded · bounded retry scheduled
\
 ├─ the stored completion claim was not evaluated — the audit waits while the item is pau
\
 ├─ lifecycle: safely parked · owner: detached auditor recovery · queue empty
\
 │  last host activity 1h 06m ago · next: automatic auditor retry in 14m 59s
\
 ├─ The completion claim is stored and re-probes itself every 15 minutes, so nothing is nee
\
 ├─ audits: 7 reviews · last infrastructure failure 0s ago
\
 ├─ model: primary opencode-go/step-5-preview-free · inherited from session
\
 └─ fallbacks: minimax/MiniMax-M3.1-Flash → opencode-go/muse-spark-1.3-contributor
\
~/dev/repo (main)
\
↑5.5M ↓256k R131M CH98.8% $7.425 7.8%/1.0M (auto)
\
● Chrome Bridge (indefinite) glla: ⏸ paused · completion audit parked — no review record
";

    /// Real pi screen from a finished pane: transcript tail, then the
    /// same bottom chrome with no question menu. Must not read as
    /// waiting — a stale-prompt regression here would put every idle
    /// pi pane in the red-triangle count.
    const PI_FINISHED_SCREEN: &str = "\
 ⏸ Remove the gate-unprotected line count from SPEC.md and re-verify every other quoted line
\
 ├─ lifecycle: safely parked · owner: manual action · queue empty
\
 │  last host activity 0s ago · next: /list resume
\
 ├─ blocked — resume to continue
\
 ├─ Blocked on the one step only you can perform: entering goal-drafting mode. I attempted
\
 ├─ saved — 32.5k tok spent · 2 audits · resumes exactly here
\
 ├─ Run /goal (bare). If the harness offers the \"replace current objective\" picker, choose
\
 ├─ audits: 2 reviews · 2 disapproved · last disapproved 1h 08m ago
\
 ├─ model: primary opencode-go/step-5-preview-free · inherited from session
\
 ├─ fallbacks: minimax/MiniMax-M3.1-Flash → opencode-go/muse-spark-1.3-contributor
\
 ... (widget truncated)
\
~/dev/repo (main)
\
↑5.8M ↓549k R187M CH99.4% $3.694 16.6%/1.0M (auto)
\
● Chrome Bridge (indefinite) glla: ⏸ action needed · next: /list resume · safely parked
";

    fn pi_manifest() -> CompiledManifest {
        CompiledManifest::compile(
            &builtin_manifests()
                .into_iter()
                .find(|m| m.agent == "pi")
                .expect("a builtin pi manifest must exist"),
        )
        .unwrap()
    }

    #[test]
    fn pi_manifest_claims_the_pi_command() {
        // Without this the whole change is inert: the daemon hands
        // classify_screen the unwrapped pane command (`pi`), so a
        // manifest that doesn't match it never runs.
        let m = pi_manifest();
        assert!(
            m.matches_command("pi"),
            "pi manifest must match the pi command"
        );
        assert!(
            m.matches_command("PI"),
            "command matching is case-insensitive"
        );
    }

    #[test]
    fn pi_manifest_classifies_real_question_menu_as_waiting() {
        // The wire state remains Waiting, but the human-facing label and
        // summary are specific: this pane is asking this question, not
        // merely waiting in the abstract.
        let hit = pi_manifest()
            .classify(PI_QUESTION_SCREEN, 0)
            .expect("question menu must classify");
        assert_eq!(hit.0, AgentState::Waiting, "{hit:?}");
        assert_eq!(hit.1, "What should I work on in football-forever?");
        assert_eq!(hit.0.display_label_for(&hit.1), "question");
        assert_eq!(
            AgentState::Waiting.display_label_for("waiting on custom"),
            "waiting"
        );
    }

    #[test]
    fn glla_decision_picker_is_a_question_not_working() {
        // Screenshot shape: GLLA offers `/goal tweak` choices in its own
        // arrow-key picker. It is not pi's numbered `❯ 1.` menu, but the
        // footer still makes clear that the operator must choose.
        let screen = concat!(
            "Decision needed — Refresh docs/turso-surface-gap.md as a current comparison\n",
            "→ /goal tweak \\\"Permit the credential-leak repair...\\\"\n",
            "  /goal tweak \\\"Keep strict document-only scope...\\\"\n",
            "  /goal tweak \\\"Keep the repair as-is...\\\"\n",
            "↑↓ navigate   enter select   escape/ctrl+c cancel\n",
        );
        let hit = pi_manifest()
            .classify(screen, 0)
            .expect("decision picker must classify");
        assert_eq!(hit.0, AgentState::Waiting);
        assert_eq!(
            hit.1,
            "Decision needed — Refresh docs/turso-surface-gap.md as a current comparison"
        );
        assert_eq!(hit.0.display_label_for(&hit.1), "question");

        // Once the decision overlay is gone and the live spinner is back,
        // stale picker text must not keep the pane in question.
        let screen = format!("{screen}── ⠙ Working ──\n");
        assert_eq!(
            pi_manifest().classify(&screen, 0).map(|hit| hit.0),
            Some(AgentState::Working)
        );
    }

    #[test]
    fn long_question_keeps_question_label_after_summary_truncation() {
        let question = "Which approach should I use for this long-running task when the initial plan is incomplete and the remaining options have different tradeoffs?";
        assert!(question.chars().count() > 80);
        let screen = format!(
            "{question}\n❯ 1. Continue with the recommended plan\n  2. Pause and ask for more context\nEnter to select · ↑/↓ to navigate\n"
        );
        let hit = pi_manifest()
            .classify(&screen, 0)
            .expect("question menu must classify");
        assert_eq!(hit.0, AgentState::Waiting);
        assert!(
            hit.1.ends_with('?'),
            "truncation dropped question mark: {hit:?}"
        );
        assert_eq!(hit.0.display_label_for(&hit.1), "question");
    }

    #[test]
    fn pi_manifest_leaves_real_working_screen_alone() {
        // Contract: a real mid-work screen must NOT read waiting. The
        // chrome is present here exactly as on a finished pane, which
        // is why the manifest claims no working/idle patterns at all.
        assert_ne!(
            pi_manifest()
                .classify(PI_WORKING_SCREEN, 0)
                .map(|(s, _, _)| s),
            Some(AgentState::Waiting),
            "working screen must not read as waiting"
        );
    }

    #[test]
    fn pi_manifest_leaves_real_finished_screen_alone() {
        // Contract: a finished pane's chrome-only screen must not read
        // as waiting either. Both negatives come from real captures so
        // a pattern that fires on live traffic fails here.
        assert_ne!(
            pi_manifest()
                .classify(PI_FINISHED_SCREEN, 0)
                .map(|(s, _, _)| s),
            Some(AgentState::Waiting),
            "finished screen must not read as waiting"
        );
    }

    #[test]
    fn pi_manifest_detects_quota_wall_as_limited() {
        // A quota wall is the one attention state pi paints as plain
        // text rather than a menu, so it needs its own list.
        let m = pi_manifest();
        assert_eq!(
            m.classify("Error: 429 rate limit reached, try again later\n", 0)
                .unwrap()
                .0,
            AgentState::Limited
        );
        assert_eq!(
            m.classify("upstream quota exceeded for this account\n", 0)
                .unwrap()
                .0,
            AgentState::Limited
        );
    }

    #[test]
    fn pi_panes_no_longer_depend_on_the_borrowed_claude_pattern() {
        // Before the manifest, a pi pane only classified because the
        // cross-manifest fallback ran claude's `^❯` over its screen.
        // The verdict must now come from pi's own manifest, with the
        // agent named so the Agent column and Src stay honest.
        let manifests = builtin_manifests()
            .iter()
            .map(CompiledManifest::compile)
            .collect::<Result<Vec<_>>>()
            .unwrap();
        let hit = classify_screen(&manifests, "pi", PI_QUESTION_SCREEN, false, false)
            .expect("question menu must classify");
        assert_eq!(hit.state, AgentState::Waiting);
        assert_eq!(
            hit.agent.as_deref(),
            Some("pi"),
            "the verdict must come from the pi manifest, not a borrowed one"
        );
    }

    #[test]
    fn cross_manifest_fallback_still_classifies_unknown_agents() {
        // The fallback is load-bearing for every agent without a
        // manifest (crush, opencode, aider…). pi gaining one must not
        // quietly take it away, so pin the shape with an unknown
        // command whose screen names and matches claude's patterns.
        // (The fallback names the agent only when the screen mentions
        // it — same rule as the command-matched path.)
        let manifests = builtin_manifests()
            .iter()
            .map(CompiledManifest::compile)
            .collect::<Result<Vec<_>>>()
            .unwrap();
        let screen = "claude assistant\n❯ Try asking me to build something\n";
        let hit = classify_screen(&manifests, "crush", screen, false, false)
            .expect("unknown agent must still classify via fallback");
        assert_eq!(hit.state, AgentState::Waiting);
        assert_eq!(hit.agent.as_deref(), Some("claude"));
    }

    #[test]
    fn claude_waiting_screen() {
        let m = compiled();
        let screen = "…\n❯ Try asking me to build something\n";
        assert_eq!(
            classify_screen(&m, "claude", screen, false, false),
            hit(
                Some("claude"),
                AgentState::Waiting,
                "❯ Try asking me to build something"
            )
        );
    }

    #[test]
    fn claude_working_screen() {
        let m = compiled();
        let screen = "Claude Code\n⠋ Working… (esc to interrupt)\n";
        assert_eq!(
            // command unknown; patterns plus the name still claim it
            classify_screen(&m, "node", screen, false, false),
            hit(
                Some("claude"),
                AgentState::Working,
                "⠋ Working… (esc to interrupt)"
            )
        );
    }

    #[test]
    fn patterns_without_name_yield_anonymous_state() {
        let m = compiled();
        // Spinner alone: working, but nobody's …
        assert_eq!(
            classify_screen(&m, "node", "⠋ Working… (esc to interrupt)\n", false, false),
            hit(None, AgentState::Working, "⠋ Working… (esc to interrupt)")
        );
        // … and a micro command bar is waiting-without-identity, not gemini.
        assert_eq!(
            classify_screen(
                &m,
                "micro",
                "> command\nSave changes? confirm\n",
                false,
                false
            ),
            hit(None, AgentState::Waiting, "Save changes? confirm")
        );
    }

    #[test]
    fn prompt_under_live_task_is_working_not_waiting() {
        let m = compiled();
        // %90 shape: muse input box below a live spinner. The `❯` alone
        // would read waiting; the interrupt offer vetoes it.
        let screen = "◈ Calling tools (27m 24s · esc to interrupt)\n───\n❯\n";
        assert_eq!(
            classify_screen(&m, "muse-bin-1.4.2-R4684.1", screen, false, false),
            hit(
                Some("muse"),
                AgentState::Working,
                "◈ Calling tools (27m 24s · esc to interrupt)"
            )
        );
    }

    #[test]
    fn finished_muse_turn_reads_done_not_waiting() {
        let m = compiled();
        // Settled muse: the turn-completion banner is the honest "my
        // turn ended" signal. It must not read `waiting` — the composer
        // below it is chrome, drawn under a live task as well, and
        // `waiting` is the badge that means "blocked on you".
        let screen = "◆ Worked for 25m 21s · 7:42 PM · 9/Nov\n───\n❯\n";
        assert_eq!(
            classify_screen(&m, "muse-bin-1.4.2-R4684.1", screen, false, false),
            hit(
                Some("muse"),
                AgentState::Done,
                "◆ Worked for 25m 21s · 7:42 PM · 9/Nov"
            )
        );
        // A composer carrying a rotating placeholder hint is the same
        // chrome: still no plea, so still no `waiting`.
        let screen = "───\n❯ /resume reopens a past session (--last for the latest)\n───\n  muse-spark-1.3 · max · ~/Dev/x · YOLO\n";
        assert_eq!(
            classify_screen(&m, "muse-bin-1.4.2-R4684.1", screen, false, false),
            None,
            "the composer hint is chrome, not a plea"
        );
    }

    #[test]
    fn muse_question_picker_reads_question_despite_the_interrupt_footer() {
        let m = compiled();
        // Live shape: muse's structured-question picker draws its own
        // footer — `Enter to select · ↑/↓ to move · Tab for an optional
        // note · Esc to interrupt`. The `Esc to interrupt` offer is
        // also muse's working marker, so an unscoped `waiting_unless`
        // veto silently turned the blocked pane into `working` and the
        // operator never saw the question.
        let screen = concat!(
            "◈ Request user input Workspace — running (1h 50m 31s)\n",
            "\n",
            "  The remaining premium-pass work is in browser-extensions-shared, outside this workspace. How should I proceed?\n",
            "\n",
            "  ⟩ 1. Switch workspace (Recommended)  You reopen with browser-extensions-shared as root; I continue the premium pass there.\n",
            "    2. Stay here, escalate             I try to work cross-workspace from here; writes may be denied and need approval.\n",
            "\n",
            "  Enter to select · ↑/↓ to move · Tab for an optional note · Esc to interrupt\n",
            "\n",
            "─────\n",
            "❯\n",
            "─────\n",
            "  muse-spark-1.3 · max · ~/Dev/x · YOLO\n",
        );
        let hit = classify_screen(&m, "muse-bin-1.4.14-R7309.1", screen, false, false)
            .expect("a blocked muse pane must classify");
        assert_eq!(hit.agent.as_deref(), Some("muse"));
        assert_eq!(hit.state, AgentState::Waiting, "{hit:?}");
        assert_eq!(
            hit.state.display_label_for(&hit.summary),
            "question",
            "the row must read `question`, not `working`: {hit:?}"
        );
        // The summary is the question itself, capped for the column and
        // still ending on the `?` that drives the `question` tag.
        assert!(
            hit.summary
                .starts_with("The remaining premium-pass work is in browser-extensions-shared"),
            "{}",
            hit.summary
        );
        assert!(hit.summary.ends_with("?"), "{}", hit.summary);
    }

    #[test]
    fn muse_permission_card_reads_waiting() {
        let m = compiled();
        // The approval card is a second blocking shape: the agent is
        // stopped until the operator picks a lane.
        let screen = concat!(
            "  Run shell command?\n",
            "    rm -rf node_modules && bun install\n",
            "  1. Allow once\n",
            "  2. Reject once\n",
            "  3. Allow for this session\n",
            "  4. Block for this session\n",
            "─────\n",
            "❯\n",
        );
        let got = classify_screen(&m, "muse-bin-1.4.14-R7309.1", screen, false, false)
            .expect("the approval card must classify");
        assert_eq!(got.state, AgentState::Waiting, "{got:?}");
        // Prose that merely mentions the labels is not a card: a
        // transcript line about approving something must not raise a
        // needs-you alarm.
        let prose = "  I will allow once the audit finishes, then rerun it.\n─────\n❯\n";
        assert_ne!(
            classify_screen(&m, "muse-bin-1.4.14-R7309.1", prose, false, false).map(|h| h.state),
            Some(AgentState::Waiting),
            "prose must not read as a permission card"
        );
    }

    #[test]
    fn muse_background_task_row_reads_working_with_its_phase() {
        let m = compiled();
        // The agent's own turn ended (`Worked for …` banner) but its
        // background watcher is still alive: the busy row below the
        // composer says the work did not.
        let screen = concat!(
            "◆ Worked for 7m 04s · 6:58 PM · 9/Nov\n",
            "─────\n",
            "❯ Ask to monitor a running build or log and get pinged on changes\n",
            "─────\n",
            "main · ↓ to select\n",
            "└ ◈ Wait for nix profile upgrade to finish  watching   2h 26m 41s\n",
            "  muse-spark-1.3 · max · ~/Dev/empire · YOLO\n",
        );
        let got = classify_screen(&m, "muse-bin-1.4.14-R7309.1", screen, false, false)
            .expect("a pane with a live background task must classify");
        assert_eq!(got.state, AgentState::Working, "{got:?}");
        assert!(
            got.phase
                .as_deref()
                .unwrap_or_default()
                .contains("nix profile upgrade"),
            "phase should name the running task: {got:?}"
        );
    }

    #[test]
    fn veto_reaches_above_the_verdict_tail() {
        let m = compiled();
        // %90 shape at its tallest: the live line sits above the
        // freshest 15 while the prompt stays glued below. The prompt
        // would read waiting, but the veto kills the verdict (the
        // daemon pairs that with process evidence).
        let mut screen = String::from("◈ Calling tools (5m 26s · esc to interrupt)\n");
        for i in 0..20 {
            screen.push_str(&format!("filler line {i}\n"));
        }
        screen.push_str("❯\n");
        assert_eq!(
            classify_screen(&m, "muse-bin-1.4.2", &screen, false, false),
            None
        );
    }

    #[test]
    fn veto_without_working_line_yields_no_verdict() {
        let m = CompiledManifest::compile(&Manifest {
            tail: None,
            agent: "v".to_string(),
            commands: vec!["v".to_string()],
            working: vec![],
            waiting: vec!["^❯".to_string()],
            error: vec![],
            limited: vec![],
            done: vec![],
            idle: vec![],
            idle_fallback: vec![],
            waiting_unless: vec!["busy".to_string()],
            busy: vec![],
            phase: vec![],
        })
        .unwrap();
        assert_eq!(m.classify("busy doing things\n❯\n", 0), None);
    }

    #[test]
    fn error_beats_working() {
        let m = compiled();
        let screen = "⠋ Working…\nError: connection reset\n";
        assert_eq!(
            classify_screen(&m, "codex", screen, false, false),
            hit(Some("codex"), AgentState::Error, "Error: connection reset")
        );
    }

    #[test]
    fn quota_wall_reads_limited() {
        let m = compiled();
        // "Error: rate limit" is a quota wall, not a crash — limited wins
        // over error within the line.
        let screen = "⠋ Working…\nError: rate limit reached\n";
        assert_eq!(
            classify_screen(&m, "codex", screen, false, false),
            hit(
                Some("codex"),
                AgentState::Limited,
                "Error: rate limit reached"
            )
        );
        // The %23 case, corrected: this line only *mentions* quota. It
        // is the agent narrating what it will do "the moment quota
        // returns" — a working pane, not a wall. It used to read as
        // anonymous limited and fire a needs-you alarm; live panes hit
        // the same false positive on prose like "ready to run the moment
        // either quota resets".
        let screen =
            "- Three of the four asset groups are ready to generate the moment quota returns.\n";
        assert_eq!(
            classify_screen(&m, "node", screen, false, false),
            None,
            "prose about a quota is not a quota wall"
        );
        // Bare 429 likewise.
        assert_eq!(
            classify_screen(&m, "node", "HTTP 429 slow down\n", false, false),
            hit(None, AgentState::Limited, "HTTP 429 slow down")
        );
        assert!(AgentState::Limited.needs_attention());
    }

    #[test]
    fn quota_prose_is_not_a_quota_wall() {
        // Both lines were live: agents narrating a limit they were
        // waiting out. Tagging them `quota` raised a needs-you alarm for
        // panes that were working — the alarm has to be earned by an
        // actual wall.
        let m = compiled();
        for screen in [
            concat!(
                "one pushed bright. The plain-prompt version is ready to run ",
                "the moment either quota resets.\n"
            ),
            concat!(
                "Next action for you: run /goal resume once Codex or MMX ",
                "quota is back and I'll generate the plain-prompt set.\n"
            ),
            concat!(
                "To see the quality improvements, the rate limit needs to ",
                "clear (or you can raise the budget in Settings → Usage). ",
                "Would you like me to reload?\n"
            ),
            "The page is being rate-limited by the provider right now: 228 spans\n",
            "1,429 spans were processed\n",
            "cost 429 tokens so far\n",
        ] {
            let got = classify_screen(&m, "pi", screen, false, false);
            assert_ne!(
                got.as_ref().map(|h| h.state),
                Some(AgentState::Limited),
                "prose tagged as a wall: {screen}{got:?}"
            );
        }
        // …while a real wall still lands, including the shapes that do
        // not use the word "limit" at all.
        for screen in [
            "⚠ Individual quota reached. Please upgrade. Resets in 2h5m35s.\n",
            "Error: Retry failed after 15 attempts: 429 {\"type\":\"error\"}\n",
            "upstream quota exhausted for this account\n",
            "Quota exhausted\nWeekly Limit Remaining\n",
        ] {
            let got = classify_screen(&m, "pi", screen, false, false);
            assert_eq!(
                got.as_ref().map(|h| h.state),
                Some(AgentState::Limited),
                "wall not detected: {screen}{got:?}"
            );
        }
    }

    /// The live shape from a gemini/agy pane out of quota: the wall's
    /// message repeated every retry, and the `>` input box glued under
    /// it. The box is the freshest line on screen, so the freshest-line
    /// rule used to hand it the verdict and the pane read `waiting` —
    /// "needs you" — for an agent nobody can unblock until the reset.
    const AGY_QUOTA_SCREEN: &str = "\
⚠ Individual quota reached. Please upgrade your subscription to increase your limits. Resets in 1h23m57s.\n\
Error ID: 7b3ba3a4-9e9a-47ae-857b-b174b08a0214-227\n\
\n\
⚠ Individual quota reached. Please upgrade your subscription to increase your limits. Resets in 1h23m57s.\n\
Error ID: 7b3ba3a4-9e9a-47ae-857b-b174b08a0214-229\n\
───────────────────────────────────────────────────────────────────────────────\n\
>\n\
───────────────────────────────────────────────────────────────────────────────\n\
? for shortcuts                                                                              Gemini 3.8 Flash · high\n";

    #[test]
    fn a_bare_input_box_under_a_quota_wall_does_not_outrank_it() {
        let m = compiled();
        // The verdict is the wall, not the box — and the summary carries
        // the reset, so the operator reads when the agent comes back
        // instead of a request id.
        assert_eq!(
            classify_screen(&m, "agy", AGY_QUOTA_SCREEN, false, false),
            hit(
                Some("gemini"),
                AgentState::Limited,
                "quota · resets in 1h23m57s"
            )
        );
    }

    #[test]
    fn a_real_question_under_a_quota_wall_keeps_the_waiting_verdict() {
        let m = compiled();
        // The wall is old news; the box below it now asks something with
        // words in it ("retry now?"). That is a plea, and it outranks
        // the wall — the human can act on it.
        let screen = "⚠ Individual quota reached. Resets in 1h23m57s.\n> retry now (y/n)\n";
        let got = classify_screen(&m, "agy", screen, false, false);
        assert_eq!(
            got,
            hit(Some("gemini"), AgentState::Waiting, "> retry now (y/n)")
        );
    }

    #[test]
    fn work_below_a_quota_wall_means_the_wall_is_history() {
        let m = compiled();
        // The wall scrolled up and the CLI's own live-task bar sits under
        // the box (the layout the gemini manifest documents). A stale
        // wall must not be resurrected as `quota` over live work.
        let screen = "⚠ Individual quota reached. Resets in 1h23m57s.\n\
>\n\
● Agent(teamwork: Lead) Generating · 20s\n";
        let got = classify_screen(&m, "agy", screen, false, false);
        assert_eq!(got.map(|h| h.state), Some(AgentState::Working));
    }

    #[test]
    fn quota_summary_keeps_the_wall_when_it_publishes_no_countdown() {
        let m = compiled();
        // No clock in the message: the wall's own line stands, so the
        // evidence the agent printed is never replaced by a guess.
        assert_eq!(
            classify_screen(&m, "codex", "Error: rate limit reached\n", false, false),
            hit(
                Some("codex"),
                AgentState::Limited,
                "Error: rate limit reached"
            )
        );
    }

    #[test]
    fn a_429_wrapped_in_json_summarizes_as_its_message() {
        let m = compiled();
        // The live shape from a `pi` pane that burned its retries: the
        // human sentence is inside the provider's JSON error body, and
        // the terminal wraps that body across lines — so the closing
        // quote is not on this line. Without unwrapping it, a 40-column
        // Summary reads `{"type":"error","error":{"ty…`.
        let screen = concat!(
            "Error: Retry failed after 15 attempts: 429 {\"type\":\"error\",",
            "\"error\":{\"type\":\"rate_limit_error\",\"message\":\"The Token Plan ",
            "usage limit has been reached. To avoid interruption, please upgrade\n",
            "\n",
            ">\n",
        );
        assert_eq!(
            classify_screen(&m, "pi", screen, false, false),
            hit(
                Some("pi"),
                AgentState::Limited,
                "quota · the Token Plan usage limit has been reached"
            )
        );
    }

    #[test]
    fn quota_countdown_is_read_only_when_it_carries_a_duration() {
        // Prose that mentions quota and a clock word but no duration is
        // not a countdown — the classifier must not rewrite it into one.
        assert_eq!(quota_countdown("quota returns when the limit resets"), None);
        assert_eq!(
            quota_countdown("Resets in 2h1m27s.").as_deref(),
            Some("Resets in 2h1m27s")
        );
        assert_eq!(
            quota_countdown("Usage limit hit. Try again in 45 minutes.").as_deref(),
            Some("Try again in 45 minutes")
        );
        // A date is not a duration.
        assert_eq!(quota_countdown("Resets on 2026-10-28"), None);
    }

    #[test]
    fn quota_displays_as_quota_and_wire_stays_limited() {
        // Humans read `quota`; the wire keeps `limited`, so a snapshot
        // from a newer daemon still decodes on an older client.
        assert_eq!(AgentState::Limited.display_label(), "quota");
        assert_eq!(AgentState::Limited.label(), "limited");
        assert_eq!(
            serde_json::to_string(&AgentState::Limited).unwrap(),
            "\"limited\""
        );
        // Both names are filters.
        assert_eq!(AgentState::parse("quota"), Some(AgentState::Limited));
        assert_eq!(AgentState::parse("limited"), Some(AgentState::Limited));
        // Every other state keeps one name.
        for s in [
            AgentState::Working,
            AgentState::Waiting,
            AgentState::Error,
            AgentState::Idle,
            AgentState::Done,
            AgentState::Stuck,
            AgentState::Paused,
            AgentState::Unknown,
        ] {
            assert_eq!(s.display_label(), s.label(), "{s:?} has two names");
        }
    }

    /// The opencode TUI's status bar, in the two shapes it takes.
    /// Left slot: shimmer + interrupt offer while a request is in
    /// flight, project location when the session is not running.
    const OPENCODE_FOOTER_CHROME: &str =
        "  ~/Dev/terhub:main    79.7K (8%)  ctrl+p commands    …/terhub:main";
    const OPENCODE_FOOTER_BUSY: &str =
        "   ⬝⬝⬝■■■ esc interrupt    479.6K (46%)  ctrl+p commands    ~/Dev/terhub:main";

    /// An opencode screen: the composer box, the model line, the
    /// gradient bar and the status bar, exactly as the TUI draws it.
    fn opencode_screen(footer: &str) -> String {
        let mut s = String::from(
            "  \u{2503}\n  \u{2503}\n  \u{2503}\n  \u{2503}  Build auto \u{b7} Crouton Alpha Free Default / OpenCode \u{b7} max\n",
        );
        s.push_str(footer);
        s.push('\n');
        s
    }

    #[test]
    fn opencode_idle_footer_reads_idle() {
        let m = compiled();
        // The opencode status bar without the interrupt offer: the
        // session is not running, so the pane is idle. Before the
        // opencode manifest every such pane read `working` forever.
        let screen = opencode_screen(OPENCODE_FOOTER_CHROME);
        assert_eq!(
            classify_screen(&m, "opencode", &screen, false, false),
            hit(
                Some("opencode"),
                AgentState::Idle,
                "~/Dev/terhub:main 79.7K (8%) ctrl+p commands …/terhub:main"
            )
        );
    }

    #[test]
    fn opencode_busy_footer_reads_working() {
        let m = compiled();
        // The same bar, busy: the interrupt offer sits on the same
        // line as the chrome, and working outranks chrome within one
        // line.
        let screen = opencode_screen(OPENCODE_FOOTER_BUSY);
        assert_eq!(
            classify_screen(&m, "opencode", &screen, false, false),
            hit(
                Some("opencode"),
                AgentState::Working,
                "⬝⬝⬝■■■ esc interrupt 479.6K (46%) ctrl+p commands ~/Dev/terhub:main"
            )
        );
    }

    #[test]
    fn opencode_armed_interrupt_offer_is_still_working() {
        let m = compiled();
        // One esc press turns the offer into "esc again to interrupt".
        // The agent is still running, so the pane must not read idle.
        let screen = opencode_screen(
            "   ⬝⬝⬝■■■ esc again to interrupt    479.6K (46%)  ctrl+p commands    ~/Dev/terhub:main",
        );
        assert_eq!(
            classify_screen(&m, "opencode", &screen, false, false),
            hit(
                Some("opencode"),
                AgentState::Working,
                "⬝⬝⬝■■■ esc again to interrupt 479.6K (46%) ctrl+p commands ~/Dev/terhub:main"
            )
        );
    }

    #[test]
    fn opencode_narrow_footer_still_reads_idle() {
        let m = compiled();
        // A narrow pane sheds the footer down to the bare location, so
        // the chrome hint is gone. The composer's gradient bar is not:
        // it is opencode's signature and it is always there.
        let screen = "  ┃\n  ┃ Ask anything...\n  ┃\n  ╹▀▀▀▀\n /tmp\n";
        assert_eq!(
            classify_screen(&m, "opencode", screen, false, false),
            hit(Some("opencode"), AgentState::Idle, "╹▀▀▀▀")
        );
    }

    #[test]
    fn opencode_composer_above_the_bottom_edge_reads_idle() {
        let m = compiled();
        // A fresh opencode session anchors its composer to the top of
        // a tall pane: the logo sits where the transcript will grow,
        // and only blank rows and the app's bottom bar follow. The
        // verdict still has to come from the composer, so this
        // manifest reads further up than the default window.
        let mut screen = String::new();
        for _ in 0..20 {
            screen.push('\n');
        }
        screen.push_str("  ┃\n");
        screen.push_str("  ┃  Build auto · Crouton Alpha Free Default / OpenCode · max\n");
        screen.push_str("  ╹▀▀▀▀▀▀▀▀\n");
        screen.push_str(" …/dev/main  shift+tab agents  ctrl+p comma\n");
        for _ in 0..15 {
            screen.push('\n');
        }
        screen.push_str("  ⊙ 2 MCP /mcps\n");
        assert_eq!(
            classify_screen(&m, "opencode", &screen, false, false),
            hit(
                Some("opencode"),
                AgentState::Idle,
                "…/dev/main shift+tab agents ctrl+p comma"
            )
        );
    }

    #[test]
    fn opencode_question_picker_reads_waiting_with_the_question() {
        let m = compiled();
        // opencode's question picker: the composer is replaced by the
        // question, so nothing sits below it to outvote the verdict.
        // The summary must be the question, not the key hints.
        let screen = "  ┃  Which lane should I switch to?\n  ┃\n  ┃  1. Use Agnes\n  ┃     Set .env.local primary to Agnes\n  ┃  2. Use MiniMax\n  ┃     Paid, uses Token Plan quota.\n  ┃\n  ┃  ↑↓ select  enter submit  esc dismiss\n  ┃\n";
        assert_eq!(
            classify_screen(&m, "opencode", screen, false, false),
            hit(
                Some("opencode"),
                AgentState::Waiting,
                "Which lane should I switch to?"
            )
        );
    }

    #[test]
    fn a_need_marker_in_a_long_screen_still_counts() {
        let m = compiled();
        // The fresh window is the last 15 lines whatever the screen's
        // length: a failure at the bottom is still a failure.
        let mut screen = String::new();
        for i in 0..30 {
            screen.push_str(&format!("filler {i}\n"));
        }
        screen.push_str("Error: connection reset\n");
        assert_eq!(
            classify_screen(&m, "codex", &screen, false, false),
            hit(Some("codex"), AgentState::Error, "Error: connection reset")
        );
    }

    #[test]
    fn opencode_old_error_stays_history() {
        let m = compiled();
        // The deep window must not turn an old failure into a red row:
        // the need markers are read from the fresh window only, so an
        // error from the top of the scrollback is history.
        let mut screen = String::from("Error: build failed\n");
        for _ in 0..20 {
            screen.push_str("filler\n");
        }
        screen.push_str(" …/dev/main  ctrl+p comm\n");
        assert_eq!(
            classify_screen(&m, "opencode", &screen, false, false),
            hit(Some("opencode"), AgentState::Idle, "…/dev/main ctrl+p comm")
        );
    }

    #[test]
    fn looping_state_parses_and_labels() {
        assert_eq!(AgentState::parse("looping"), Some(AgentState::Looping));
        assert_eq!(AgentState::parse("REPEATING"), Some(AgentState::Looping));
        assert_eq!(AgentState::Looping.label(), "looping");
        assert_eq!(AgentState::Looping.display_label(), "looping");
        // Label and parse round-trip: whatever we render, we can read.
        assert_eq!(
            AgentState::parse(AgentState::Looping.label()),
            Some(AgentState::Looping)
        );
        // A loop is the operator's problem to solve, like a hang.
        assert!(AgentState::Looping.needs_attention());
        // A legacy client cannot read `looping`; the daemon projects it
        // to the nearest attention state (never a quiet one).
        assert_eq!(
            crate::daemon::legacy_state(AgentState::Looping),
            AgentState::Stuck
        );
    }

    #[test]
    fn a_repeated_error_line_is_looping() {
        // The model-recovery loop: the same provider error printed
        // once per retry cycle while the pane reads plain idle.
        let err = "Error: 400: {\"message\":\"Provider returned error\",\"code\":400}";
        let mut screen = String::new();
        for i in 0..5 {
            screen.push_str(&format!("├─ why: main model unknown — 400: attempt {i}\n"));
            screen.push_str(err);
            screen.push('\n');
            screen.push_str("Main model recovery: retrying openrouter/stealth/crouton-alpha\n");
        }
        screen.push_str("  ── ⠼ Working ──\n");
        screen.push_str("~/Dev/dracon-strategy:main\n");
        assert_eq!(
            repeated_content(&screen, 0),
            Some("Error: 400: {\"message\":\"Provider returned error\",\"code\":400}".to_string())
        );
    }

    #[test]
    fn a_repeated_command_line_is_looping() {
        // The audit agent re-running the same verification sweep: the
        // same command line, over and over, instead of advancing.
        let mut screen = String::new();
        for _ in 0..4 {
            screen.push_str("$ cd extensions/standalones/g2a-helper && grep -o \"fixed in\" findings.md | sort | uniq\n");
            screen.push_str("  fixed in fdc1262079\n");
        }
        screen.push_str("── ⠋ Working ──\n");
        assert!(repeated_content(&screen, 0).is_some());
    }

    #[test]
    fn a_retry_banner_alone_is_not_a_loop() {
        // A retry banner is how healthy recovery looks: the counter
        // changes, and the line is a status banner, not a result or a
        // command. The loop detection belongs to the errors behind
        // the retries, not the banner itself.
        let screen = "phase one\nRetrying (3/15) in 60s\nphase two\nRetrying (4/15) in 59s\nphase three\nRetrying (5/15) in 58s\n";
        assert_eq!(repeated_content(screen, 0), None);
    }

    #[test]
    fn an_old_run_of_errors_is_history_not_a_loop() {
        // The tabmon shapes: the same error printed seven times while
        // the pane was busy, then the work finished — the completion
        // text and the `glla: IDLE` close-out now sit below them. The
        // verdict belongs to what the pane is doing *now*.
        let err = "Error: Request timed out.";
        let mut screen = String::new();
        for _ in 0..7 {
            screen.push_str(err);
            screen.push('\n');
            screen
                .push_str("glla: provider error (Request timed out.) — retrying automatically.\n");
        }
        for i in 0..33 {
            screen.push_str(&format!("work character row {i} with work and more work\n"));
        }
        screen.push_str("1,448 tests green, typecheck clean, fresh zip rebuilt.\n");
        // A 50-row pane: the visible area is the capture's last 50
        // lines, whose second half is the last 25 — the timeouts are
        // all above it.
        assert_eq!(repeated_content(&screen, 50), None);
    }

    #[test]
    fn a_recent_run_of_errors_is_a_loop() {
        // The same errors, but the loop is happening at the bottom of
        // the screen, right now.
        let err = "Error: Request timed out.";
        let mut screen = String::new();
        for i in 0..33 {
            screen.push_str(&format!("work character row {i} with work and more work\n"));
        }
        for _ in 0..4 {
            screen.push_str(err);
            screen.push('\n');
            screen
                .push_str("glla: provider error (Request timed out.) — retrying automatically.\n");
        }
        assert_eq!(
            repeated_content(&screen, 50),
            Some("Error: Request timed out.".to_string())
        );
    }

    #[test]
    fn a_lone_fresh_copy_cannot_revive_a_finished_run() {
        // The tabmon shape again, plus a later isolated hit in the
        // live half of the pane. Three old copies started the run;
        // the isolated copy after 33 work rows does not continue
        // that streak, so it cannot make the finished run live again.
        let err = "Error: Request timed out.";
        let mut screen = String::new();
        for _ in 0..3 {
            screen.push_str(err);
            screen.push('\n');
            screen
                .push_str("glla: provider error (Request timed out.) — retrying automatically.\n");
        }
        for i in 0..33 {
            screen.push_str(&format!("work character row {i} with work and more work\n"));
        }
        screen.push_str(err);
        screen.push('\n');
        assert_eq!(repeated_content(&screen, 50), None);
    }

    #[test]
    fn a_new_streak_can_restart_a_loop() {
        // The contrary case: only a fresh, tight three-copy streak is
        // enough to declare a loop. Historical copies of the same
        // line are not needed, and they do not let one new copy act
        // as the whole streak.
        let err = "Error: Request timed out.";
        let mut screen = String::new();
        for i in 0..33 {
            screen.push_str(&format!("work character row {i} with work and more work\n"));
        }
        for _ in 0..4 {
            screen.push_str(err);
            screen.push('\n');
            screen
                .push_str("glla: provider error (Request timed out.) — retrying automatically.\n");
        }
        assert_eq!(
            repeated_content(&screen, 50),
            Some("Error: Request timed out.".to_string())
        );
    }

    #[test]
    fn the_most_frequent_live_line_is_the_repeated_answer() {
        let first = "Error: 400: first repeated provider failure for this agent";
        let second = "Error: 400: second repeated provider failure for this agent";
        let mut screen = String::new();
        for i in 0..4 {
            screen.push_str(&format!("opening work row {i} with some more work\n"));
        }
        for _ in 0..4 {
            screen.push_str(first);
            screen.push('\n');
        }
        screen.push_str("plain work explaining the current task\n");
        for _ in 0..3 {
            screen.push_str(second);
            screen.push('\n');
        }
        assert_eq!(repeated_content(&screen, 0), Some(first.to_string()));
    }

    #[test]
    fn two_repeats_are_not_a_loop() {
        // Two instances is a coincidence (a table row, a reused
        // command). Three is a loop.
        let screen = "Error: 400: provider returned error code 400\nwork\nError: 400: provider returned error code 400\n";
        assert_eq!(repeated_content(screen, 0), None);
    }

    #[test]
    fn a_slow_repeat_still_counts_when_it_is_on_screen() {
        // Three copies of the same line with the newest one in the
        // visible area: a slower loop that has not rushed to four yet.
        let err = "Error: 400: {\"message\":\"Provider returned error\",\"code\":400}";
        let mut screen = String::new();
        for _ in 0..3 {
            screen.push_str(err);
            screen.push('\n');
            screen.push_str("working on it\n");
        }
        assert!(repeated_content(&screen, 8).is_some());
    }

    #[test]
    fn three_repeats_in_the_scrollback_do_not_count() {
        // The same line three times, all of it in the scrollback of a
        // finished round of work: the verdict belongs to the pane's
        // current screen, not to yesterday's loop.
        let cmd = "$ cd extensions/standalones/g2a-helper && grep -o \"fixed in\" findings.md | sort | uniq";
        let mut screen = String::new();
        for _ in 0..3 {
            screen.push_str(cmd);
            screen.push('\n');
            screen.push_str("member of the audit loop\n");
        }
        // The visible area is the very bottom of the screen: every
        // copy sits above it, in the scrollback where earlier rounds
        // of work leave their evidence.
        assert_eq!(repeated_content(&screen, 1), None);
        // The same three copies, with the whole capture visible, are
        // the loop.
        assert!(repeated_content(&screen, 100).is_some());
    }

    #[test]
    fn a_repeated_prose_line_is_not_a_loop() {
        // An audit note that an agent wrote three times before
        // parking is a cleared conversation, not an active loop.
        let mut screen = String::new();
        for _ in 0..3 {
            screen.push_str(
                "Claim submitted — audit pending (nonterminal; the final verdict posts later)\n",
            );
            screen.push_str("Quick summary while it settles:\n");
            screen.push('\n');
        }
        assert_eq!(repeated_content(&screen, 0), None);
    }

    #[test]
    fn chrome_and_collapse_markers_do_not_count() {
        // Box art, rules, separators and collapsed-output markers are
        // the TUI's bookkeeping, there because the transcript is long.
        let mut screen = String::new();
        for _ in 0..4 {
            screen.push_str("├───────────────────────────────┼───────────────────────────\n");
            screen.push_str("... (5 earlier lines, ctrl+o to expand)\n");
            screen.push_str("── ⠼ Working ────────────────────────\n");
        }
        assert_eq!(repeated_content(&screen, 0), None);
    }

    #[test]
    fn pi_loop_verdict_wins_over_working_and_idle() {
        let m = compiled();
        // A pi pane whose scrollback has ten of the same error and a
        // live spinner: looping, not working.
        let err = "Error: 400: {\"message\":\"Provider returned error\",\"code\":400}";
        let mut screen = String::from("pi TUI\n");
        for i in 0..4 {
            screen.push_str(&format!("├─ why: main model unknown — 400: attempt {i}\n"));
            screen.push_str(err);
            screen.push('\n');
            screen.push_str(
                "Main model recovery: retrying openrouter/stealth/crouton-alpha (same-model)\n",
            );
        }
        screen.push_str("  ── ⠼ Working ───────────────────────────────────────────\n");
        screen.push_str("~/Dev/dracon-strategy:main\n");
        let hit = classify_screen(&m, "pi", &screen, false, false).expect("hit");
        assert_eq!(hit.state, AgentState::Looping);
        assert_eq!(hit.agent.as_deref(), Some("pi"));
        assert!(
            hit.summary.starts_with("repeating: Error: 400"),
            "{}",
            hit.summary
        );
    }

    #[test]
    fn a_question_outranks_a_loop() {
        // A blocked question is more urgent than the loop behind it:
        // the human's answer is what unsticks the pane.
        let m = compiled();
        let mut screen = String::from("pi TUI\n");
        let err = "Error: 400: {\"message\":\"Provider returned error\",\"code\":400}";
        for i in 0..4 {
            screen.push_str(&format!("Error: 400: attempt {i}\n"));
            screen.push_str(err);
            screen.push('\n');
        }
        screen.push_str("  When you pick a different music theme, what actually happens?\n");
        screen.push_str("  ❯ 1. Moves, music stays same (Recommended)\n");
        screen.push_str("  ❯ 2. Doesn't move at all\n");
        screen.push_str("  ❯ 3. No music anywhere on Sound\n");
        screen.push_str("Enter to select · ↑/↓ to navigate\n");
        // The loop evidence is really here; the question must win anyway.
        assert!(repeated_content(&screen, 0).is_some());
        let hit = classify_screen(&m, "pi", &screen, false, false).expect("hit");
        assert_eq!(hit.state, AgentState::Waiting, "{}", hit.summary);
        assert_eq!(hit.state.display_label_for(&hit.summary), "question");
        assert_eq!(
            hit.summary,
            "When you pick a different music theme, what actually happens?"
        );
    }

    #[test]
    fn a_quota_wall_outranks_a_loop() {
        // A quota wall is actionable (wait for the reset); the loop
        // behind it is the same fact restated.
        let m = compiled();
        let mut screen = String::from("claude TUI\n");
        for i in 0..4 {
            screen.push_str(&format!("Error: attempt {i}: rate limit reached\n"));
        }
        screen.push_str("❯\n");
        // The loop evidence is really here; the wall must win anyway.
        assert!(repeated_content(&screen, 0).is_some());
        let hit = classify_screen(&m, "claude", &screen, false, false).expect("hit");
        assert_eq!(hit.state, AgentState::Limited, "{}", hit.summary);
    }

    #[test]
    fn a_single_failure_is_an_error_not_a_loop() {
        // One failure is news. The loop verdict needs the same thing
        // three times.
        let m = compiled();
        let screen = "pi TUI\nError: 400: {\"message\":\"Provider returned error\",\"code\":400}\n── ⠼ Working ──\n";
        let hit = classify_screen(&m, "pi", screen, false, false).expect("hit");
        assert_eq!(hit.state, AgentState::Working);
    }

    #[test]
    fn opencode_stale_interrupt_offer_does_not_pin_working() {
        let m = compiled();
        // The transcript can mention the offer ("press esc interrupt to
        // cancel"). The footer is the fresher line, so the pane reads
        // idle — a stale mention is history, not a live turn.
        let mut screen = String::from("  press esc interrupt to cancel the run\n");
        screen.push_str(&opencode_screen(OPENCODE_FOOTER_CHROME));
        assert_eq!(
            classify_screen(&m, "opencode", &screen, false, false),
            hit(
                Some("opencode"),
                AgentState::Idle,
                "~/Dev/terhub:main 79.7K (8%) ctrl+p commands …/terhub:main"
            )
        );
    }

    #[test]
    fn opencode_chrome_does_not_claim_other_agents() {
        let m = compiled();
        // The chrome pattern is opencode-specific on purpose: this
        // manifest is also tried over screens whose command matched
        // nothing, so a loose pattern here would claim other agents'
        // panes.
        assert_eq!(
            classify_screen(&m, "node", "⠋ Working… (esc to interrupt)\n", false, false),
            // Anonymous: the screen never names an agent, and no
            // manifest claims the command.
            hit(None, AgentState::Working, "⠋ Working… (esc to interrupt)")
        );
        // pi's own footer is not opencode's.
        assert_eq!(
            classify_screen(&m, "pi", "── ⠦ Working ──\n", false, false),
            Some(ScreenHit {
                agent: Some("pi".to_string()),
                state: AgentState::Working,
                summary: "── ⠦ Working ──".to_string(),
                phase: Some("── ⠦ Working ──".to_string()),
            })
        );
    }

    #[test]
    fn bare_prompt_chrome_is_recognized_and_words_are_not() {
        assert!(is_prompt_chrome(">"));
        assert!(is_prompt_chrome("❯"));
        assert!(is_prompt_chrome("  │ > │  "));
        // Empty is not chrome either way: nothing on the line means
        // nothing to read, and the caller must not treat a blank as a
        // box it should suppress.
        assert!(!is_prompt_chrome(""));
        assert!(!is_prompt_chrome("> retry now (y/n)"));
        assert!(!is_prompt_chrome("❯ Try asking me to build something"));
    }

    #[test]
    fn plain_shell_screen_is_unknown() {
        let m = compiled();
        assert_eq!(
            classify_screen(&m, "zsh", "$ ls\nfoo\n", false, false),
            None
        );
    }

    #[test]
    fn bad_regex_rejected() {
        let m = Manifest {
            agent: "x".to_string(),
            tail: None,
            commands: vec![],
            working: vec!["([".to_string()],
            waiting: vec![],
            error: vec![],
            limited: vec![],
            done: vec![],
            idle: vec![],
            idle_fallback: vec![],
            waiting_unless: vec![],
            busy: vec![],
            phase: vec![],
        };
        assert!(CompiledManifest::compile(&m).is_err());
    }

    #[test]
    fn shell_detection() {
        assert!(is_shell_command("zsh"));
        assert!(is_shell_command("-bash"));
        assert!(!is_shell_command("claude"));
        assert!(!is_shell_command("muse-bin-1.4.2"));
    }

    #[test]
    fn harness_detection() {
        assert!(is_harness_command("muxa"));
        assert!(is_harness_command("terhub"));
        assert!(!is_harness_command("claude"));
        assert!(!is_harness_command("zsh"));
    }

    #[test]
    fn hook_lifecycle_aliases() {
        // DaemonMode-style hook states from spawning agents.
        assert_eq!(AgentState::parse("start"), Some(AgentState::Working));
        assert_eq!(AgentState::parse("stop"), Some(AgentState::Waiting));
        assert_eq!(AgentState::parse("stuck"), Some(AgentState::Stuck));
        assert_eq!(AgentState::parse("hung"), Some(AgentState::Stuck));
        assert!(AgentState::Stuck.needs_attention());
    }

    #[test]
    fn agy_command_classifies_as_gemini() {
        // Live: six panes run the `agy` CLI (a Gemini frontend). The
        // command must pin the gemini manifest so quota/waiting screens
        // classify as gemini even when they name other models (the quota
        // wall lists "Claude Opus, Claude Sonnet, GPT-OSS" as a group).
        let m = compiled();
        let gemini = m.iter().find(|x| x.agent == "gemini").unwrap();
        assert!(
            gemini.matches_command("agy"),
            "gemini manifest must claim agy"
        );
        // Waiting prompt with the gemini footer, no other agent named.
        let screen = ">\n? for shortcuts  Gemini 3.8 Flash · high";
        assert_eq!(
            classify_screen(&m, "agy", screen, false, false),
            hit(Some("gemini"), AgentState::Waiting, ">")
        );
        // Quota wall naming Claude models: state Limited, agent gemini.
        let screen = "Quota exhausted\nCLAUDE AND GPT MODELS\nModels within this group: Claude Opus, Claude Sonnet, GPT-OSS\nWeekly Limit Remaining\nQuota is consumed\n? for shortcuts  Gemini 3.8 Flash · high";
        let got = classify_screen(&m, "agy", screen, false, false);
        assert_eq!(
            got.as_ref().and_then(|h| h.agent.clone()),
            Some("gemini".to_string()),
            "quota wall misattributed: {got:?}"
        );
        assert_eq!(got.map(|h| h.state), Some(AgentState::Limited));
        // Bare quota lines with no agent word at all still claim gemini
        // through the command pin (previously agent=None -> dropped).
        let screen = "Individual quota reached. Please upgrade your subscription. Resets in 49m23s.\n>\n? for shortcuts  Gemini 3.8 Flash · high";
        let got = classify_screen(&m, "agy", screen, false, false);
        assert!(
            got.is_some(),
            "quota screen with gemini footer must classify"
        );
    }

    #[test]
    fn gemini_agent_bar_means_working_not_waiting() {
        // Live (Gemini CLI, the `agy` frontend): the input box stays on
        // screen for the whole turn, so the `>` above says nothing about
        // whether the agent is done. The agent bar below it lists the
        // running subagents, and its presence means the work is still in
        // flight — a pane whose bar is populated is working.
        let m = compiled();
        let screen = r"  The team is executing the unblocking tasks.
────────────────────────────────────────────────────────────
>
────────────────────────────────────────────────────────────
  ● Agent(teamwork_preview_swe: SWE Light Orchestrator)  Generating · 8m56s
  ● Agent(teamwork_preview: Teamwork Multi-Agent System)  Generating · 8m47s
────────────────────────────────────────────────────────────
? for shortcuts  Gemini 3.8 Flash · high · 2 subagent(s)
";
        let got = classify_screen(&m, "agy", screen, false, false)
            .expect("an agy screen with an agent bar must classify");
        assert_eq!(got.agent.as_deref(), Some("gemini"));
        assert_eq!(
            got.state,
            AgentState::Working,
            "live subagents are work in flight, not a plea: {got:?}"
        );
        // The phase names what is running, so a working row says more
        // than "busy".
        let phase = got.phase.clone().unwrap_or_default();
        assert!(
            phase.contains("Agent(") && phase.contains("Generating"),
            "phase should name the running subagent: {got:?}"
        );
        // Same screen once the last subagent finished: the bar is gone,
        // the prompt is the freshest match again, and the agent's turn
        // is over — the human's move is next.
        let finished = r"  The team is executing the unblocking tasks.
────────────────────────────────────────────────────────────
>
────────────────────────────────────────────────────────────
? for shortcuts  Gemini 3.8 Flash · high
";
        let got = classify_screen(&m, "agy", finished, false, false)
            .expect("an agy screen at the prompt must classify");
        assert_eq!(
            got.state,
            AgentState::Waiting,
            "a finished turn waits for the human: {got:?}"
        );
        // A background task row is the same signal: the bar lists
        // running background tasks next to the subagents.
        let task = r"  ○ Checking 1 task (task-660) (ctrl+o to expand)
⣾  Managing tasks...
>
────────────────────────────────────────────────────────────
  ● [01:57:02] node scripts/audit-surfaces.mjs running
────────────────────────────────────────────────────────────
esc to cancel  Gemini 3.8 Flash · high · 1 task(s) · /tasks
";
        let got = classify_screen(&m, "agy", task, false, false)
            .expect("an agy screen with a task row must classify");
        assert_eq!(
            got.state,
            AgentState::Working,
            "a running background task is work in flight: {got:?}"
        );
        assert!(
            got.phase
                .as_deref()
                .unwrap_or_default()
                .contains("audit-surfaces"),
            "phase should name the running task: {got:?}"
        );
    }

    #[test]
    fn pi_live_markers_mean_working_not_idle() {
        // Live (pi): `agent_end` fires when the main turn ends, but
        // retry loops, detached audit workers, and background
        // subagents keep going after it. These markers are live-only
        // UI — each one clears when its work ends — so a match means
        // the pane is still grinding, not finished.
        let m = compiled();
        // NOTE: the pane command is the terhub-labelled one (`pi`),
        // not tmux's raw `systemd-inhibit` — labelling pins the pi
        // manifest, so the verdict carries the pi name and phase.
        // Spinner bar above the GLLA status.
        let screen = "── ⠴ Working ──\n● GOAL · working · RETRY ARMED · age 54m\n● Chrome Bridge (indefinite)\n";
        let got =
            classify_screen(&m, "pi", screen, false, false).expect("pi spinner bar must classify");
        assert_eq!(got.agent.as_deref(), Some("pi"));
        assert_eq!(got.state, AgentState::Working, "spinner bar: {got:?}");
        assert!(
            got.phase
                .as_deref()
                .unwrap_or_default()
                .to_lowercase()
                .contains("working"),
            "phase should name the live work: {got:?}"
        );
        // Automatic retry loop after a quota failure.
        let screen = "Error: 429 quota exceeded\n── ⠸ Retrying (11/15) in 59s... (escape to cancel) ──\n● Chrome Bridge (indefinite)\n";
        let got =
            classify_screen(&m, "pi", screen, false, false).expect("pi retry loop must classify");
        assert_eq!(
            got.state,
            AgentState::Working,
            "a retry countdown is work in flight, not idleness: {got:?}"
        );
        // Detached audit worker outliving the main turn.
        let screen = "● list item · auditing · RUNNING · age 6h 49m\n├─ Audit running · thinking… · detached worker\n│ last action: read → background.ts · 10s ago\n● Chrome Bridge (indefinite)\n";
        let got =
            classify_screen(&m, "pi", screen, false, false).expect("pi audit worker must classify");
        assert_eq!(
            got.state,
            AgentState::Working,
            "a running audit worker is work in flight: {got:?}"
        );
        // Revived background subagent.
        let screen = "async subagent · background\n  ⠏ scout [fresh] · running (minimax/MiniMax-M3.1-Flash · thinking low)\n● GOAL · working · RETRY ARMED · age 54m\n● Chrome Bridge (indefinite)\n";
        let got = classify_screen(&m, "pi", screen, false, false)
            .expect("pi background subagent must classify");
        assert_eq!(
            got.state,
            AgentState::Working,
            "a running background subagent is work in flight: {got:?}"
        );
        // A finished turn shows none of the markers: completed
        // transcript plus the identical chrome. No working verdict —
        // finish detection stays with the hook and the quiet clock.
        let finished = "• completion audit approved.\n● Chrome Bridge (indefinite) glla: IDLE · idle · no action\n";
        assert_eq!(
            classify_screen(&m, "pi", finished, false, false),
            None,
            "a finished pi turn must not pin working"
        );
    }

    #[test]
    fn error_substring_is_not_an_error() {
        let m = compiled();
        // "errors"/"errorhandler" must not trip whole-word error patterns.
        let screen = "$ fix errorhandler\n3 errors logged, retrying\n";
        assert_eq!(classify_screen(&m, "codex", screen, false, false), None);
        let screen = "$ fix errorhandler\n0 failed builds, all green\n";
        assert_eq!(
            classify_screen(&m, "gemini", screen, false, false),
            hit(
                Some("gemini"),
                AgentState::Error,
                "0 failed builds, all green"
            )
        );
    }

    #[test]
    fn approval_words_need_boundaries() {
        let m = compiled();
        // "disapproved"/"approved" in audit chatter must not read as a prompt.
        let screen = "completion review disapproved — fix the gap\n• completion audit approved\n";
        assert_eq!(classify_screen(&m, "claude", screen, false, false), None);
        let screen = "Approve this plan? [y/n]\n";
        assert_eq!(
            classify_screen(&m, "claude", screen, false, false),
            hit(
                Some("claude"),
                AgentState::Waiting,
                "Approve this plan? [y/n]"
            )
        );
    }

    #[test]
    fn freshest_signal_wins() {
        let m = compiled();
        // Old error, new work below: working.
        let screen = "reporting an Error: timeout\n── ⠦ Working ──\n$ doing things\n";
        assert_eq!(
            classify_screen(&m, "claude", screen, false, false),
            hit(Some("claude"), AgentState::Working, "── ⠦ Working ──")
        );
        // Old work, fresh error at the bottom: error.
        let screen = "── ⠦ Working ──\nError: socket timeout\n";
        assert_eq!(
            classify_screen(&m, "codex", screen, false, false),
            hit(Some("codex"), AgentState::Error, "Error: socket timeout")
        );
    }

    /// A live mcode pane whose own turn ended while its goal kept
    /// running — captured verbatim from the terminal.
    const MCODE_VERIFYING_SCREEN: &str = "\
    └ Completed in 15min14s · ⚡ 819 tok/s

    ◎ Goal · Verifying the result · 54min59s active
  1.1M tokens · 3 turns
  read pi-session-2026-10-22T08-02-36-824Z_01a162b2-2898-740b-b435-5b68c45985b3.html and uadit · /goal pause · /goal edit · /goal clear
    ✓ Todo list 4/4 completed
  ◐ Tasks · 1 agent active · /tasks details
    Message · Enter send · Ctrl+J newline
    ─────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────
  ›  Ask Mcode to do anything
    ─────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────";

    #[test]
    fn a_running_goal_reads_working_with_its_phase() {
        // The todo list says `4/4 completed` and the input box is back,
        // but the goal loop is running and a subagent is still active.
        // Reading that as `done` told the operator the pane was free to
        // interrupt — it was not.
        let m = compiled();
        let hit = classify_screen(&m, "minimax-code", MCODE_VERIFYING_SCREEN, false, false)
            .expect("a verdict");
        assert_eq!(
            hit.state,
            AgentState::Working,
            "a running goal is work in flight, not a finished turn"
        );
        assert_eq!(
            hit.phase.as_deref(),
            Some("◎ Goal · Verifying the result · 54min59s active"),
            "the phase is the one line that says what the agent is doing"
        );
    }

    #[test]
    fn a_goal_phase_outranks_a_completed_todo_list() {
        // Without the tasks line the todo banner is fresher than the goal
        // line, so freshest-wins alone would read `done`. The busy veto
        // is what keeps the verdict honest.
        let m = compiled();
        let screen = "\
    ◎ Goal · Verifying the result · 54min59s active
    ✓ Todo list 4/4 completed
    Message · Enter send · Ctrl+J newline";
        let hit = classify_screen(&m, "minimax-code", screen, false, false).expect("a verdict");
        assert_eq!(
            hit.state,
            AgentState::Working,
            "the goal outranks the banner"
        );
        assert!(hit.phase.is_some(), "the phase survives the veto");
    }

    #[test]
    fn a_stopped_goal_banner_is_not_a_running_goal() {
        // The goal-loop's stopped banners (`paused by GLLA`, `work
        // saved`) are not a running goal: no phase, and no busy veto to
        // override the quiet verdicts underneath them.
        let m = compiled();
        let screen = "\
    ● GOAL · paused by GLLA · RETRY ARMED · 22/26 ▰▰▰▰▱ tasks · age 7h 43m
    Message · Enter send · Ctrl+J newline";
        let hit = classify_screen(&m, "minimax-code", screen, false, false).expect("a verdict");
        assert_ne!(hit.state, AgentState::Working, "a paused goal is not busy");
        assert!(hit.phase.is_none(), "a stopped banner is not a phase");
    }

    #[test]
    fn a_paused_goal_is_not_busy_but_still_says_so() {
        // The `◎ Goal · Paused · 58min35s active` form: the goal is on
        // hold. It must not read as busy — but it is still the one line
        // that says what the agent is up to, so the phase shows.
        let m = compiled();
        let screen = "\
    ◎ Goal · Paused · 58min35s active
  792K tokens · 2 turns
    Message · Enter send · Ctrl+J newline";
        let hit = classify_screen(&m, "minimax-code", screen, false, false).expect("a verdict");
        assert_ne!(hit.state, AgentState::Working, "a paused goal is not busy");
        assert_eq!(
            hit.phase.as_deref(),
            Some("◎ Goal · Paused · 58min35s active"),
            "the phase still says what the agent is up to"
        );
    }

    #[test]
    fn a_finished_turn_without_a_goal_is_still_done() {
        // The busy veto must not turn every finished pane busy: with no
        // goal line and no active tasks, the completed todo list is the
        // verdict it always was.
        let m = compiled();
        let screen = "\
    ✓ Todo list 9/9 completed
    Message · Enter send · Ctrl+J newline";
        let hit = classify_screen(&m, "minimax-code", screen, false, false).expect("a verdict");
        assert_eq!(hit.state, AgentState::Done);
        assert!(hit.phase.is_none());
    }

    #[test]
    fn minimax_completion_reads_done() {
        let m = compiled();
        // Finished task banners: `done`, not working — and not waiting
        // either, which is reserved for genuine input pleas. The
        // minimax manifest's `idle:` list was migrated to `done:`.
        assert_eq!(
            classify_screen(
                &m,
                "minimax-code",
                "✓ Goal complete · 31min38s · 352K tokens\n",
                false,
                false
            ),
            hit(
                Some("minimax-code"),
                AgentState::Done,
                "✓ Goal complete · 31min38s · 352K tokens"
            )
        );
        assert_eq!(
            classify_screen(
                &m,
                "minimax-code",
                "✓ Todo list 15/15 completed\n",
                false,
                false
            ),
            hit(
                Some("minimax-code"),
                AgentState::Done,
                "✓ Todo list 15/15 completed"
            )
        );
        // Done loses within-line ties: any hint of need or activity wins.
        assert_eq!(
            classify_screen(
                &m,
                "minimax-code",
                "⠙ ✓ Goal complete · 31min38s\n",
                false,
                false
            ),
            hit(
                Some("minimax-code"),
                AgentState::Working,
                "⠙ ✓ Goal complete · 31min38s"
            )
        );
        // A failed task run is an error (and beats the goal banner below it).
        assert_eq!(
            classify_screen(
                &m,
                "minimax-code",
                "✓ Goal complete · 2h11min8s\n× Tasks · 1 failed · 5 results ready\n",
                false,
                false
            ),
            hit(
                Some("minimax-code"),
                AgentState::Error,
                "× Tasks · 1 failed · 5 results ready"
            )
        );
        // Partial progress is not completion…
        assert_eq!(
            classify_screen(&m, "minimax-code", "Todo list 3/15\n", false, false),
            None
        );
        // …prose failures are not task failures…
        assert_eq!(
            classify_screen(
                &m,
                "minimax-code",
                "SPEC-200 failed when gates ran\n",
                false,
                false
            ),
            None
        );
        // …a zero count is not a failure…
        assert_eq!(
            classify_screen(
                &m,
                "minimax-code",
                "Tasks · 0 failed · /tasks details\n",
                false,
                false
            ),
            None
        );
        // The input box only returns when the turn is over (live
        // inference replaces it with the Running footer), so the box
        // alone means done — while the Ask placeholder persists under
        // live tasks and means nothing.
        assert_eq!(
            classify_screen(
                &m,
                "minimax-code",
                "Message · Enter send · Ctrl+J newline\n",
                false,
                false
            ),
            hit(
                Some("minimax-code"),
                AgentState::Idle,
                "Message · Enter send · Ctrl+J newline"
            )
        );
        assert_eq!(
            classify_screen(
                &m,
                "minimax-code",
                "› Ask Mcode to do anything\n",
                false,
                false
            ),
            None
        );
        // A stale throughput rate on a Completed line must not pin
        // working: live inference always brings a spinner verb too.
        assert_eq!(
            classify_screen(
                &m,
                "minimax-code",
                "└ Completed in 1min22s · ⚡ 6535 tok/s\n",
                false,
                false
            ),
            None
        );
        // The box is bottom chrome: it never shadows need above it.
        assert_eq!(
            classify_screen(
                &m,
                "minimax-code",
                "× Tasks · 1 failed\nMessage · Enter send · Ctrl+J newline\n",
                false,
                false
            ),
            hit(
                Some("minimax-code"),
                AgentState::Error,
                "× Tasks · 1 failed"
            )
        );
        // …never shadows a banner (which also summarizes better)…
        assert_eq!(
            classify_screen(
                &m,
                "minimax-code",
                "✓ Todo list 9/9 completed\nMessage · Enter send · Ctrl+J newline\n",
                false,
                false
            ),
            hit(
                Some("minimax-code"),
                AgentState::Done,
                "✓ Todo list 9/9 completed"
            )
        );
        // …loses to live activity below it…
        assert_eq!(
            classify_screen(
                &m,
                "minimax-code",
                "Message · Enter send · Ctrl+J newline\n⠙ Running 5min1s\n",
                false,
                false
            ),
            hit(
                Some("minimax-code"),
                AgentState::Working,
                "⠙ Running 5min1s"
            )
        );
        // …but beats stale activity above it.
        assert_eq!(
            classify_screen(
                &m,
                "minimax-code",
                "⠙ Running 5min1s\nMessage · Enter send · Ctrl+J newline\n",
                false,
                false
            ),
            hit(
                Some("minimax-code"),
                AgentState::Idle,
                "Message · Enter send · Ctrl+J newline"
            )
        );
        // Live inference still reads working.
        assert_eq!(
            classify_screen(
                &m,
                "minimax-code",
                "⠙ Running 13min40s · ⚡ 15092 tok/s\n",
                false,
                false
            ),
            hit(
                Some("minimax-code"),
                AgentState::Working,
                "⠙ Running 13min40s · ⚡ 15092 tok/s"
            )
        );
    }

    #[test]
    fn tip_lines_stripped_before_classify() {
        let m = compiled();
        // A footer tip must never trip the classifier, even one whose
        // words would match ("quota" is a limited pattern).
        assert_eq!(
            classify_screen(
                &m,
                "claude",
                "$ working\nTip: quota for free tier\n",
                false,
                false
            ),
            None
        );
        // …while real verdicts beside a tip still land, with real summaries.
        assert_eq!(
            classify_screen(
                &m,
                "claude",
                "Tip: quota for free tier\nError: socket timeout\n",
                false,
                false
            ),
            hit(Some("claude"), AgentState::Error, "Error: socket timeout")
        );
        // Only the footer prefix strips: prose merely starting with Tip stays.
        // (Line-join normalizes the trailing newline; classification and
        // hashing are line-based, so this is invisible downstream.)
        assert_eq!(strip_tip_lines("Tipper: hello\nok\n"), "Tipper: hello\nok");
        assert_eq!(strip_tip_lines("  Tip: indented\nok\n"), "ok");
        // minimax's inline tail truncates at the slash form…
        assert_eq!(
            strip_tip_lines("Message · Enter send      Tip: /steer guides\nok\n"),
            "Message · Enter send\nok"
        );
        // …but a non-slash Tip: mid-line is prose, not chrome.
        assert_eq!(
            strip_tip_lines("Note Tip: restart it\nok\n"),
            "Note Tip: restart it\nok"
        );
    }

    #[test]
    fn bare_prompt_borrows_context_line() {
        let m = compiled();
        // Empty prompt: the task line above, not the box rule, not "❯".
        let screen = "◆ docs + gate + install\n──────────\n❯\n";
        assert_eq!(
            classify_screen(&m, "claude", screen, false, false),
            hit(
                Some("claude"),
                AgentState::Waiting,
                "◆ docs + gate + install"
            )
        );
        // Nothing textual above: keep the thin line, honestly.
        let screen = "──────────\n❯\n";
        assert_eq!(
            classify_screen(&m, "claude", screen, false, false),
            hit(Some("claude"), AgentState::Waiting, "❯")
        );
    }

    #[test]
    fn summary_line_is_collapsed_and_capped() {
        assert_eq!(
            screen_summary_line("  Approve?   [y/n]  "),
            "Approve? [y/n]"
        );
        assert_eq!(screen_summary_line(""), "");
        let long = format!("Error: {}", "x".repeat(200));
        let capped = screen_summary_line(&long);
        assert_eq!(capped.chars().count(), 80);
        assert!(capped.ends_with('…'));
        assert!(capped.starts_with("Error: xxx"));
    }

    #[test]
    fn lowercase_error_is_prose() {
        let m = compiled();
        let screen = "one of them was reporting an error: a retry would be a no-op\n";
        assert_eq!(classify_screen(&m, "claude", screen, false, false), None);
        assert_eq!(classify_screen(&m, "codex", screen, false, false), None);
    }

    #[test]
    fn lossy_compile_skips_broken_manifest() {
        let mut manifests = builtin_manifests();
        manifests.push(Manifest {
            tail: None,
            agent: "broken".to_string(),
            commands: vec![],
            working: vec!["([".to_string()],
            waiting: vec![],
            error: vec![],
            limited: vec![],
            done: vec![],
            idle: vec![],
            idle_fallback: vec![],
            waiting_unless: vec![],
            busy: vec![],
            phase: vec![],
        });
        let (ok, warnings) = compile_all_lossy(&manifests);
        assert_eq!(ok.len(), builtin_manifests().len());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("broken"));
    }

    #[test]
    fn event_merge_keeps_pane_metadata() {
        let mut reg = HashMap::new();
        let ev = HookEvent {
            pane: "%1".to_string(),
            agent: "claude".to_string(),
            state: AgentState::Working,
            summary: "a".to_string(),
            prompt: String::new(),
            model: String::new(),
            ctx: None,
        };
        apply_event_to(&mut reg, &ev, 100);
        reg.get_mut("%1").unwrap().session = "work".to_string();
        let ev2 = HookEvent {
            state: AgentState::Waiting,
            summary: "b".to_string(),
            ..ev
        };
        apply_event_to(&mut reg, &ev2, 200);
        assert_eq!(reg["%1"].session, "work");
        assert_eq!(reg["%1"].state_since, 200);
    }

    #[test]
    fn hook_turn_end_records_duration() {
        let mut reg = HashMap::new();
        let work = HookEvent {
            pane: "%1".to_string(),
            agent: "claude".to_string(),
            state: AgentState::Working,
            summary: "a".to_string(),
            prompt: String::new(),
            model: String::new(),
            ctx: None,
        };
        apply_event_to(&mut reg, &work, 100);
        apply_event_to(&mut reg, &work, 200);
        let wait = HookEvent {
            state: AgentState::Waiting,
            summary: "b".to_string(),
            ..work
        };
        apply_event_to(&mut reg, &wait, 400);
        assert_eq!(reg["%1"].last_turn_secs, 300);
        // Same-state repeats don't rewrite it.
        apply_event_to(&mut reg, &wait, 500);
        assert_eq!(reg["%1"].last_turn_secs, 300);
    }

    #[test]
    fn event_merge_keeps_state_since() {
        let mut reg = HashMap::new();
        let ev = HookEvent {
            pane: "%1".to_string(),
            agent: "claude".to_string(),
            state: AgentState::Working,
            summary: "a".to_string(),
            prompt: String::new(),
            model: String::new(),
            ctx: None,
        };
        apply_event_to(&mut reg, &ev, 100);
        apply_event_to(&mut reg, &ev, 200);
        assert_eq!(reg["%1"].state_since, 100);
        assert_eq!(reg["%1"].updated_at, 200);
        let ev2 = HookEvent {
            state: AgentState::Waiting,
            summary: "b".to_string(),
            ..ev
        };
        apply_event_to(&mut reg, &ev2, 300);
        assert_eq!(reg["%1"].state_since, 300);
    }
}

#[cfg(test)]
mod sparkline_tests {
    use crate::ui::sparkline_glyphs;

    #[test]
    fn empty_ring_renders_flat_ridge() {
        // A fresh pane with no samples shows the smallest step in
        // every bucket — a flat ridge reads as "quiet, watching",
        // not as a missing column.
        let out = sparkline_glyphs(&[0u32; 6]);
        assert_eq!(out, "▁▁▁▁▁▁");
    }

    #[test]
    fn six_buckets_have_six_glyphs() {
        // The contract: 6 cells, one per bucket, regardless of input.
        let out = sparkline_glyphs(&[10, 20, 30, 40, 50, 60]);
        assert_eq!(out.chars().count(), 6);
        // A monotonically rising ring must render with non-decreasing
        // glyph heights (the whole point: you can see activity
        // ramping at a glance, no numbers required).
        let mut prev = '▁';
        for c in out.chars() {
            assert!(
                (c as u32) >= (prev as u32),
                "non-monotonic sparkline: {prev} > {c}"
            );
            prev = c;
        }
    }
}
