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
        }
    }

    /// States where the human should look at the pane.
    pub fn needs_attention(self) -> bool {
        matches!(
            self,
            AgentState::Waiting | AgentState::Error | AgentState::Stuck
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
}

pub struct CompiledManifest {
    pub agent: String,
    pub commands: Vec<String>,
    working: Vec<Regex>,
    waiting: Vec<Regex>,
    error: Vec<Regex>,
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
        })
    }

    pub fn matches_command(&self, cmd: &str) -> bool {
        let cmd = cmd.to_lowercase();
        self.commands.iter().any(|c| cmd.contains(c.as_str()))
    }

    /// Classify screen text. The freshest signal wins: the newest matching
    /// line decides, so current output overrides stale scrollback (an old
    /// "error:" in prose can't pin Error forever while the agent works).
    /// Within one line, error beats waiting beats working.
    /// Freshest matching line wins; the returned summary is that line,
    /// cleaned for the Summary column.
    pub fn classify(&self, screen: &str) -> Option<(AgentState, String)> {
        let tail: Vec<&str> = screen.lines().rev().take(15).collect();
        let mut hit = None;
        for line in tail.iter().rev() {
            if self.error.iter().any(|r| r.is_match(line)) {
                hit = Some((AgentState::Error, screen_summary_line(line)));
            } else if self.waiting.iter().any(|r| r.is_match(line)) {
                hit = Some((AgentState::Waiting, screen_summary_line(line)));
            } else if self.working.iter().any(|r| r.is_match(line)) {
                hit = Some((AgentState::Working, screen_summary_line(line)));
            }
        }
        hit
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
                "rate limit".to_string(),
                "usage limit".to_string(),
                "something went wrong".to_string(),
                "traceback".to_string(),
            ],
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
            error: vec![
                "\\b(?-i:E)rror\\b".to_string(),
                "rate limit".to_string(),
                "\\bfailed\\b".to_string(),
            ],
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
            error: vec![
                "\\b(?-i:E)rror\\b".to_string(),
                "\\bfailed\\b".to_string(),
                "quota".to_string(),
            ],
        },
    ]
}

/// Load user manifests from a directory (`*.toml`); invalid files are skipped.
pub fn load_manifests(dir: &Path) -> Vec<Manifest> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => continue,
        };
        if let Ok(m) = toml::from_str::<Manifest>(&text) {
            out.push(m);
        }
    }
    out
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
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn error_beats_working() {
        let m = compiled();
        let screen = "⠋ Working…\nError: rate limit reached\n";
        assert_eq!(
            classify_screen(&m, "codex", screen),
            hit(Some("codex"), AgentState::Error, "Error: rate limit reached")
        );
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
        let screen = "── ⠦ Working ──\nError: rate limit reached\n";
        assert_eq!(
            classify_screen(&m, "codex", screen),
            hit(Some("codex"), AgentState::Error, "Error: rate limit reached")
        );
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
        });
        let (ok, warnings) = compile_all_lossy(&manifests);
        assert_eq!(ok.len(), 3);
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
