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
            "unknown" => Some(AgentState::Unknown),
            _ => None,
        }
    }

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
        }
    }

    /// States where the human should look at the pane.
    pub fn needs_attention(self) -> bool {
        matches!(
            self,
            AgentState::Waiting | AgentState::Error | AgentState::Stuck | AgentState::Limited
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
    /// Stable local name (`@pi-wez`), minted once by the daemon, empty
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

/// Mint a handle (`@agent-session`, `-2`/`-3`… on collision) against the
/// used set. An empty session mints bare `@agent`.
pub fn mint_handle(used: &std::collections::HashSet<String>, agent: &str, session: &str) -> String {
    let agent = slug_handle_part(agent);
    let session = slug_handle_part(session);
    let base = if session.is_empty() {
        format!("@{agent}")
    } else {
        format!("@{agent}-{session}")
    };
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
        let (agent, session) = match registry.get(&pane) {
            Some(r) => (r.agent.clone(), r.session.clone()),
            None => continue,
        };
        let handle = mint_handle(&used, &agent, &session);
        used.insert(handle.clone());
        if let Some(r) = registry.get_mut(&pane) {
            r.handle = handle;
        }
    }
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
                | AgentState::Idle
                | AgentState::Done
        )
}

impl AgentRecord {
    pub fn waiting_secs(&self, now: u64) -> u64 {
        now.saturating_sub(self.state_since)
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

/// Screen patterns for one agent family (TOML-serializable).
#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub agent: String,
    /// Pane-command substrings (case-insensitive) identifying this agent.
    #[serde(default)]
    pub commands: Vec<String>,
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
    pub fn classify(&self, screen: &str) -> Option<(AgentState, String, Option<String>)> {
        let mut tail: Vec<&str> = screen.lines().rev().take(15).collect();
        tail.reverse();
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
            if self.limited.iter().any(|r| r.is_match(line)) {
                hit = Some((AgentState::Limited, i));
                need = true;
            } else if self.error.iter().any(|r| r.is_match(line)) {
                hit = Some((AgentState::Error, i));
                need = true;
            } else if self.waiting.iter().any(|r| r.is_match(line)) {
                hit = Some((AgentState::Waiting, i));
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
        // A vetoed prompt is an input box under a live task, not a plea:
        // fall back to the freshest working line, or no verdict at all
        // (the caller pairs that with process evidence, or drops it).
        if veto && matches!(hit, Some((AgentState::Waiting, _))) {
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
        hit.map(|(state, i)| {
            let mut line = tail[i];
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
        })
    }
}

pub fn builtin_manifests() -> Vec<Manifest> {
    vec![
        Manifest {
            agent: "claude".to_string(),
            commands: vec!["claude".to_string(), "claude-code".to_string()],
            working: vec![
                "esc to interrupt".to_string(),
                "working…".to_string(),
                "⠋|⠙|⠹|⠸|⠼|⠴|⠦|⠧|⠇|⠏".to_string(),
                "✻.*(thinking|working|cooking)".to_string(),
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
            waiting_unless: vec![],
            busy: vec![],
            phase: vec![],
        },
        Manifest {
            agent: "codex".to_string(),
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
            // muse keeps its `❯` input box visible under a live task, so
            // the prompt alone never proves waiting: an interrupt offer
            // anywhere in the tail vetoes the prompt verdict.
            agent: "muse".to_string(),
            commands: vec!["muse".to_string()],
            working: vec![
                "esc to interrupt".to_string(),
                "calling tools".to_string(),
                "thinking \\(".to_string(),
                "⠋|⠙|⠹|⠸|⠼|⠴|⠦|⠧|⠇|⠏".to_string(),
            ],
            waiting: vec![
                "^❯".to_string(),
                "\\[y/n\\]".to_string(),
                "press enter to send".to_string(),
            ],
            idle: vec![],
            idle_fallback: vec![],
            waiting_unless: vec!["esc to interrupt".to_string()],
            busy: vec![],
            phase: vec![],
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
            done: vec![],
        },
        Manifest {
            // The "Message · Enter send" box only returns when the turn is
            // over (live inference shows the Running footer instead), while
            // the "Ask Mcode" placeholder persists under live tasks — so
            // the box means done and the placeholder means nothing. Failure
            // counts stay Tasks-anchored: bare "failed" would fire on prose
            // like "SPEC-200 failed".
            agent: "minimax-code".to_string(),
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
        let hit = m.classify(screen).map(|(state, summary, phase)| ScreenHit {
            agent: Some(m.agent.clone()),
            state,
            summary,
            phase,
        });
        return park_annotate(bell_override(hit, bell), parked);
    }
    let flat = screen.to_lowercase();
    for m in manifests {
        if let Some((state, summary, phase)) = m.classify(screen) {
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
        assert_eq!(mint_handle(&used, "pi", "wez"), "@pi-wez");
        assert_eq!(mint_handle(&used, "pi", ""), "@pi");
        assert_eq!(
            mint_handle(&used, "claude", "My Project!"),
            "@claude-my-project"
        );
        let mut used: HashSet<String> = ["@pi-wez".to_string()].into_iter().collect();
        assert_eq!(mint_handle(&used, "pi", "wez"), "@pi-wez-2");
        used.insert("@pi-wez-2".to_string());
        assert_eq!(mint_handle(&used, "pi", "wez"), "@pi-wez-3");
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
        assert_eq!(reg["%1"].handle, "@pi-wez");
        assert_eq!(reg["%9"].handle, "@pi-wez-2");
        // Re-runs never re-mint: handles are stable for the record's life.
        ensure_handles(&mut reg);
        assert_eq!(reg["%1"].handle, "@pi-wez");
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
            m.classify("Summary\nall tasks passed · 0 failed\n")
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
            .classify("Summary\nall tasks passed · 0 failed\n")
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
            m.classify("Summary\nall tasks passed · 0 failed\n")
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
        // Contract: a real captured question menu reads Waiting, and
        // the row's summary points at the question so the human can
        // tell which evidence fired.
        let hit = pi_manifest()
            .classify(PI_QUESTION_SCREEN)
            .expect("question menu must classify");
        assert_eq!(hit.0, AgentState::Waiting, "{hit:?}");
        assert!(
            hit.1.contains("Enter to select") || hit.1.contains("Run the next SPEC"),
            "summary should name the question, got {:?}",
            hit.1
        );
    }

    #[test]
    fn pi_manifest_leaves_real_working_screen_alone() {
        // Contract: a real mid-work screen must NOT read waiting. The
        // chrome is present here exactly as on a finished pane, which
        // is why the manifest claims no working/idle patterns at all.
        assert_ne!(
            pi_manifest().classify(PI_WORKING_SCREEN).map(|(s, _, _)| s),
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
                .classify(PI_FINISHED_SCREEN)
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
            m.classify("Error: 429 rate limit reached, try again later\n")
                .unwrap()
                .0,
            AgentState::Limited
        );
        assert_eq!(
            m.classify("upstream quota exceeded for this account\n")
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
    fn prompt_without_live_task_is_waiting() {
        let m = compiled();
        // Settled muse: completed lines carry no interrupt offer, so the
        // bare prompt reads waiting.
        let screen = "◆ Ran 2 commands · stuff · 28s · ctrl+o\n───\n❯\n";
        assert_eq!(
            classify_screen(&m, "muse-bin-1.4.2-R4684.1", screen, false, false),
            hit(
                Some("muse"),
                AgentState::Waiting,
                "◆ Ran 2 commands · stuff · 28s · ctrl+o"
            )
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
        assert_eq!(m.classify("busy doing things\n❯\n"), None);
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
        // The %23 case: quota prose under an unknown command reads as
        // anonymous limited, pairable with process identity.
        let screen =
            "- Three of the four asset groups are ready to generate the moment quota returns.\n";
        assert_eq!(
            classify_screen(&m, "node", screen, false, false),
            hit(
                None,
                AgentState::Limited,
                "- Three of the four asset groups are ready to generate the moment quota returns."
            )
        );
        // Bare 429 likewise.
        assert_eq!(
            classify_screen(&m, "node", "HTTP 429 slow down\n", false, false),
            hit(None, AgentState::Limited, "HTTP 429 slow down")
        );
        assert!(AgentState::Limited.needs_attention());
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
            got.phase.as_deref().unwrap_or_default().contains("audit-surfaces"),
            "phase should name the running task: {got:?}"
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
