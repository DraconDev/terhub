//! Agent awareness: state model, hook events, screen-pattern manifests.
use anyhow::{Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AgentState {
    #[default]
    Unknown,
    Working,
    Waiting,
    Error,
    Idle,
    /// Working, but the agent's process tree has burned no CPU for
    /// STUCK_SECS: hung, not busy.
    Stuck,
    /// Usage/quota wall hit: the agent is paused until reset, not broken.
    /// Quota language is telemetry about limits, never a crash report.
    Limited,
}

impl AgentState {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "working" | "work" | "busy" | "running" | "start" => Some(AgentState::Working),
            "waiting" | "wait" | "input" | "attention" | "blocked" | "needs-input" | "stop" => {
                Some(AgentState::Waiting)
            }
            "error" | "failed" | "fail" => Some(AgentState::Error),
            "idle" | "done" | "complete" | "completed" => Some(AgentState::Idle),
            "stuck" | "hung" => Some(AgentState::Stuck),
            "limited" | "limit" | "quota" | "capped" => Some(AgentState::Limited),
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
            AgentState::Stuck => "stuck",
            AgentState::Limited => "limited",
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Hook,
    Screen,
    /// Process-tree identification: the agent binary is really there.
    Proc,
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
}

/// A working turn completed: Working moved to a terminal-ish state.
pub fn turn_ended(from: AgentState, to: AgentState) -> bool {
    from == AgentState::Working
        && matches!(
            to,
            AgentState::Waiting | AgentState::Error | AgentState::Limited | AgentState::Idle
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
    /// Waiting veto: when any of these match the tail, a waiting verdict
    /// falls back to the freshest working line (or no verdict). For agents
    /// whose input prompt stays visible during work — `❯` below a live
    /// spinner is an input box, not a plea for input.
    #[serde(default)]
    pub waiting_unless: Vec<String>,
}

pub struct CompiledManifest {
    pub agent: String,
    pub commands: Vec<String>,
    working: Vec<Regex>,
    waiting: Vec<Regex>,
    error: Vec<Regex>,
    limited: Vec<Regex>,
    waiting_unless: Vec<Regex>,
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
    /// beats waiting beats working ("Error: rate limit reached" is a quota
    /// wall, not a crash).
    /// Freshest matching line wins; the returned summary is that line,
    /// cleaned for the Summary column.
    pub fn classify(&self, screen: &str) -> Option<(AgentState, String)> {
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
        for (i, line) in tail.iter().enumerate() {
            let is_work = self.working.iter().any(|r| r.is_match(line));
            if is_work {
                work = Some(i);
            }
            if self.limited.iter().any(|r| r.is_match(line)) {
                hit = Some((AgentState::Limited, i));
            } else if self.error.iter().any(|r| r.is_match(line)) {
                hit = Some((AgentState::Error, i));
            } else if self.waiting.iter().any(|r| r.is_match(line)) {
                hit = Some((AgentState::Waiting, i));
            } else if is_work {
                hit = Some((AgentState::Working, i));
            }
        }
        // A vetoed prompt is an input box under a live task, not a plea:
        // fall back to the freshest working line, or no verdict at all
        // (the caller pairs that with process evidence, or drops it).
        if veto && matches!(hit, Some((AgentState::Waiting, _))) {
            hit = work.map(|i| (AgentState::Working, i));
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
            (state, screen_summary_line(line))
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
            waiting_unless: vec![],
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
            waiting_unless: vec![],
        },
        Manifest {
            agent: "gemini".to_string(),
            commands: vec!["gemini".to_string()],
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
            waiting_unless: vec![],
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
            waiting_unless: vec!["esc to interrupt".to_string()],
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
pub fn classify_screen(
    manifests: &[CompiledManifest],
    command: &str,
    screen: &str,
) -> Option<ScreenHit> {
    if let Some(m) = manifests.iter().find(|m| m.matches_command(command)) {
        return m.classify(screen).map(|(state, summary)| ScreenHit {
            agent: Some(m.agent.clone()),
            state,
            summary,
        });
    }
    let flat = screen.to_lowercase();
    for m in manifests {
        if let Some((state, summary)) = m.classify(screen) {
            let agent = flat
                .contains(m.agent.to_lowercase().as_str())
                .then(|| m.agent.clone());
            return Some(ScreenHit {
                agent,
                state,
                summary,
            });
        }
    }
    None
}

/// Merge helper for tests and the daemon: fold hook events over a registry.
pub fn apply_event_to(registry: &mut HashMap<String, AgentRecord>, ev: &HookEvent, now: u64) {
    match registry.get(&ev.pane) {
        Some(old) if old.state == ev.state && old.agent == ev.agent => {
            let mut rec = old.clone();
            rec.source = Source::Hook;
            rec.summary = ev.summary.clone();
            rec.updated_at = now;
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
            registry.insert(
                ev.pane.clone(),
                AgentRecord {
                    pane: ev.pane.clone(),
                    agent: ev.agent.clone(),
                    state: ev.state,
                    source: Source::Hook,
                    summary: ev.summary.clone(),
                    session: old.map(|r| r.session.clone()).unwrap_or_default(),
                    window: old.map(|r| r.window.clone()).unwrap_or_default(),
                    command: old.map(|r| r.command.clone()).unwrap_or_default(),
                    updated_at: now,
                    state_since: now,
                    cpu_ticks: 0,
                    cpu_still_since: 0,
                    agent_start: 0,
                    last_turn_secs,
                    // A hook event is not a screen change: carry the clock.
                    screen_hash: old.map(|r| r.screen_hash).unwrap_or(0),
                    quiet_since: old.map(|r| r.quiet_since).unwrap_or(0),
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

    fn compiled() -> Vec<CompiledManifest> {
        compile_all(&builtin_manifests()).unwrap()
    }

    fn hit(agent: Option<&str>, state: AgentState, summary: &str) -> Option<ScreenHit> {
        Some(ScreenHit {
            agent: agent.map(str::to_string),
            state,
            summary: summary.to_string(),
        })
    }

    #[test]
    fn state_parse_aliases() {
        assert_eq!(AgentState::parse("working"), Some(AgentState::Working));
        assert_eq!(AgentState::parse("needs-input"), Some(AgentState::Waiting));
        assert_eq!(AgentState::parse("BLOCKED"), Some(AgentState::Waiting));
        assert_eq!(AgentState::parse("failed"), Some(AgentState::Error));
        assert_eq!(AgentState::parse("done"), Some(AgentState::Idle));
        assert_eq!(AgentState::parse("quota"), Some(AgentState::Limited));
        assert_eq!(AgentState::parse("CAPPED"), Some(AgentState::Limited));
        assert_eq!(AgentState::parse("bogus"), None);
    }

    #[test]
    fn claude_waiting_screen() {
        let m = compiled();
        let screen = "…\n❯ Try asking me to build something\n";
        assert_eq!(
            classify_screen(&m, "claude", screen),
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
            classify_screen(&m, "node", screen),
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
            classify_screen(&m, "node", "⠋ Working… (esc to interrupt)\n"),
            hit(None, AgentState::Working, "⠋ Working… (esc to interrupt)")
        );
        // … and a micro command bar is waiting-without-identity, not gemini.
        assert_eq!(
            classify_screen(&m, "micro", "> command\nSave changes? confirm\n"),
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
            classify_screen(&m, "muse-bin-1.4.2-R4684.1", screen),
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
            classify_screen(&m, "muse-bin-1.4.2-R4684.1", screen),
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
        assert_eq!(classify_screen(&m, "muse-bin-1.4.2", &screen), None);
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
            waiting_unless: vec!["busy".to_string()],
        })
        .unwrap();
        assert_eq!(m.classify("busy doing things\n❯\n"), None);
    }

    #[test]
    fn error_beats_working() {
        let m = compiled();
        let screen = "⠋ Working…\nError: connection reset\n";
        assert_eq!(
            classify_screen(&m, "codex", screen),
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
            classify_screen(&m, "codex", screen),
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
            classify_screen(&m, "node", screen),
            hit(
                None,
                AgentState::Limited,
                "- Three of the four asset groups are ready to generate the moment quota returns."
            )
        );
        // Bare 429 likewise.
        assert_eq!(
            classify_screen(&m, "node", "HTTP 429 slow down\n"),
            hit(None, AgentState::Limited, "HTTP 429 slow down")
        );
        assert!(AgentState::Limited.needs_attention());
    }

    #[test]
    fn plain_shell_screen_is_unknown() {
        let m = compiled();
        assert_eq!(classify_screen(&m, "zsh", "$ ls\nfoo\n"), None);
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
            waiting_unless: vec![],
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
    fn error_substring_is_not_an_error() {
        let m = compiled();
        // "errors"/"errorhandler" must not trip whole-word error patterns.
        let screen = "$ fix errorhandler\n3 errors logged, retrying\n";
        assert_eq!(classify_screen(&m, "codex", screen), None);
        let screen = "$ fix errorhandler\n0 failed builds, all green\n";
        assert_eq!(
            classify_screen(&m, "gemini", screen),
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
        assert_eq!(classify_screen(&m, "claude", screen), None);
        let screen = "Approve this plan? [y/n]\n";
        assert_eq!(
            classify_screen(&m, "claude", screen),
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
            classify_screen(&m, "claude", screen),
            hit(Some("claude"), AgentState::Working, "── ⠦ Working ──")
        );
        // Old work, fresh error at the bottom: error.
        let screen = "── ⠦ Working ──\nError: socket timeout\n";
        assert_eq!(
            classify_screen(&m, "codex", screen),
            hit(Some("codex"), AgentState::Error, "Error: socket timeout")
        );
    }

    #[test]
    fn bare_prompt_borrows_context_line() {
        let m = compiled();
        // Empty prompt: the task line above, not the box rule, not "❯".
        let screen = "◆ docs + gate + install\n──────────\n❯\n";
        assert_eq!(
            classify_screen(&m, "claude", screen),
            hit(
                Some("claude"),
                AgentState::Waiting,
                "◆ docs + gate + install"
            )
        );
        // Nothing textual above: keep the thin line, honestly.
        let screen = "──────────\n❯\n";
        assert_eq!(
            classify_screen(&m, "claude", screen),
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
        assert_eq!(classify_screen(&m, "claude", screen), None);
        assert_eq!(classify_screen(&m, "codex", screen), None);
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
            waiting_unless: vec![],
        });
        let (ok, warnings) = compile_all_lossy(&manifests);
        assert_eq!(ok.len(), 4);
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
