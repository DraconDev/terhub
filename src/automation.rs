//! Automation: rules that act on agent state.
//!
//! Rules live in `automation.toml` next to the config. Evaluation is a pure
//! function of (rules, registry snapshot, ledger fire history, now), so the
//! daemon and `automation test` share identical logic. Guardrails are
//! mandatory: entry delay, per-pane cooldown, and per-rule hourly cap.
use crate::agent::{AgentRecord, AgentState};
use crate::tmux::Tmux;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
pub struct Rule {
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// State that arms the rule: waiting|error|working|idle|stuck|limited
    /// (aliases accepted, see `AgentState::parse`).
    pub on: String,
    /// Optional agent-name substring filter (case-insensitive).
    #[serde(default)]
    pub agent: Option<String>,
    /// Optional summary substring filter (case-insensitive).
    #[serde(default)]
    pub match_summary: Option<String>,
    /// Seconds the state must hold before firing.
    #[serde(default)]
    pub after_secs: u64,
    #[serde(default = "default_cooldown")]
    pub cooldown_secs: u64,
    #[serde(default = "default_cap")]
    pub hourly_cap: u32,
    /// "send-keys" or "shell".
    pub action: String,
    /// Text to type (send-keys).
    #[serde(default)]
    pub text: Option<String>,
    /// Command to run (shell).
    #[serde(default)]
    pub command: Option<String>,
}

fn default_true() -> bool {
    true
}

fn default_cooldown() -> u64 {
    3600
}

fn default_cap() -> u32 {
    3
}

#[derive(Debug, Deserialize, Default)]
struct RuleFile {
    #[serde(default)]
    rule: Vec<Rule>,
}

pub fn automation_path() -> PathBuf {
    crate::config::config_path()
        .parent()
        .map(|p| p.join("automation.toml"))
        .unwrap_or_else(|| PathBuf::from("automation.toml"))
}

pub fn default_automation_text() -> String {
    r#"# terhub automation: rules that act on agent state. Disabled by default;
# set enabled = true to arm a rule. Guardrails (delay, per-pane cooldown,
# hourly cap) always apply.

[[rule]]
name = "resume-after-limit"
enabled = false
on = "waiting"
match_summary = "limit"
after_secs = 300
cooldown_secs = 3600
hourly_cap = 3
action = "send-keys"
text = "continue"
"#
    .to_string()
}

pub fn ensure_automation() -> Result<PathBuf> {
    let path = automation_path();
    if !path.exists() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, default_automation_text())?;
    }
    Ok(path)
}

pub fn load_rules() -> Vec<Rule> {
    let path = automation_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    toml::from_str::<RuleFile>(&text)
        .map(|f| f.rule)
        .unwrap_or_default()
}

/// Static problems with a rule (bad state, unknown action, missing payload).
pub fn validate(rule: &Rule) -> Vec<String> {
    let mut problems = Vec::new();
    if AgentState::parse(&rule.on).is_none() {
        problems.push(format!("bad on='{}'", rule.on));
    }
    match rule.action.as_str() {
        "send-keys" => {
            if rule.text.as_ref().is_none_or(|t| t.is_empty()) {
                problems.push("send-keys needs text".to_string());
            }
        }
        "shell" => {
            if rule.command.as_ref().is_none_or(|c| c.is_empty()) {
                problems.push("shell needs command".to_string());
            }
        }
        other => problems.push(format!("bad action='{other}'")),
    }
    if rule.hourly_cap == 0 {
        problems.push("hourly_cap = 0 never fires".to_string());
    }
    problems
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    SendKeys { text: String },
    Shell { command: String },
}

#[derive(Debug, Clone)]
pub struct Fire {
    pub rule: String,
    pub pane: String,
    pub action: Action,
}

#[derive(Debug, Clone)]
pub enum Verdict {
    Fire(Fire),
    /// Matches but the entry delay has not elapsed (seconds left).
    WaitDelay(u64),
    /// Matches but the pane is in cooldown (seconds left).
    Cooldown(u64),
    /// Matches but the rule hit its hourly cap.
    Capped,
}

/// Pure evaluation: (rules, registry, past fires, now) -> per-match verdicts.
/// Past fires are (rule, pane, unixtime) triples from the ledger.
pub fn dry_run(
    rules: &[Rule],
    registry: &HashMap<String, AgentRecord>,
    fires: &[(String, String, u64)],
    now: u64,
) -> Vec<(String, String, Verdict)> {
    let mut out = Vec::new();
    // Fires decided in this pass count toward the cap immediately, so one
    // tick can never exceed hourly_cap no matter how many panes match.
    let mut pass_fires: HashMap<&str, u32> = HashMap::new();
    for rule in rules {
        if !rule.enabled || !validate(rule).is_empty() {
            continue;
        }
        let want = match AgentState::parse(&rule.on) {
            Some(s) => s,
            None => continue,
        };
        let action = match rule.action.as_str() {
            "send-keys" => Action::SendKeys {
                text: rule.text.clone().unwrap_or_default(),
            },
            "shell" => Action::Shell {
                command: rule.command.clone().unwrap_or_default(),
            },
            _ => continue,
        };
        let mut candidates: Vec<&AgentRecord> = registry
            .values()
            .filter(|rec| {
                if rec.state != want {
                    return false;
                }
                if let Some(f) = rule.agent.as_ref()
                    && !rec.agent.to_lowercase().contains(&f.to_lowercase())
                {
                    return false;
                }
                if let Some(f) = rule.match_summary.as_ref()
                    && !rec.summary.to_lowercase().contains(&f.to_lowercase())
                {
                    return false;
                }
                true
            })
            .collect();
        // Longest-held first: scarce cap slots go to the panes that have
        // waited longest instead of wherever the hash order lands.
        candidates.sort_by(|a, b| (a.state_since, &a.pane).cmp(&(b.state_since, &b.pane)));
        for rec in candidates {
            let held = now.saturating_sub(rec.state_since);
            if held < rule.after_secs {
                out.push((
                    rule.name.clone(),
                    rec.pane.clone(),
                    Verdict::WaitDelay(rule.after_secs - held),
                ));
                continue;
            }
            let last_fire = fires
                .iter()
                .filter(|(r, p, _)| r == &rule.name && p == &rec.pane)
                .map(|(_, _, t)| *t)
                .max();
            if let Some(t) = last_fire
                && now.saturating_sub(t) < rule.cooldown_secs
            {
                out.push((
                    rule.name.clone(),
                    rec.pane.clone(),
                    Verdict::Cooldown(rule.cooldown_secs - now.saturating_sub(t)),
                ));
                continue;
            }
            let recent = fires
                .iter()
                .filter(|(r, _, t)| r == &rule.name && now.saturating_sub(*t) < 3600)
                .count() as u32
                + pass_fires.get(rule.name.as_str()).copied().unwrap_or(0);
            if recent >= rule.hourly_cap {
                out.push((rule.name.clone(), rec.pane.clone(), Verdict::Capped));
                continue;
            }
            *pass_fires.entry(rule.name.as_str()).or_insert(0) += 1;
            out.push((
                rule.name.clone(),
                rec.pane.clone(),
                Verdict::Fire(Fire {
                    rule: rule.name.clone(),
                    pane: rec.pane.clone(),
                    action: action.clone(),
                }),
            ));
        }
    }
    out.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    out
}

/// The firings due right now (verdicts filtered to [`Verdict::Fire`]).
pub fn evaluate(
    rules: &[Rule],
    registry: &HashMap<String, AgentRecord>,
    fires: &[(String, String, u64)],
    now: u64,
) -> Vec<(Fire, AgentRecord)> {
    let by_pane: HashMap<&str, &AgentRecord> =
        registry.values().map(|r| (r.pane.as_str(), r)).collect();
    dry_run(rules, registry, fires, now)
        .into_iter()
        .filter_map(|(_, pane, verdict)| match verdict {
            Verdict::Fire(fire) => by_pane.get(pane.as_str()).map(|rec| (fire, (*rec).clone())),
            _ => None,
        })
        .collect()
}

/// Shell-action wall clock: a hanging command must not stall the daemon.
pub const SHELL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// A child process reaped with its pipes drained.
#[derive(Debug)]
pub struct DrainedChild {
    pub status: std::process::ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Why [`wait_drained`] gave up.
#[derive(Debug)]
pub enum DrainError {
    Timeout,
    WaitFailed(String),
}

/// Wait for a piped child with a deadline while draining stdout/stderr on
/// reader threads. Draining *during* the wait (not after exit) is
/// load-bearing: a child that fills the 64KB pipe buffer would otherwise
/// wedge until the timeout even though it was ready to finish.
pub fn wait_drained(
    child: &mut std::process::Child,
    timeout: std::time::Duration,
) -> Result<DrainedChild, DrainError> {
    use std::io::Read;
    // Detached (not scoped) readers: on the timeout path grandchildren may
    // still hold the pipes open, so joining there would block past the
    // deadline — the old code returned without draining, and so do we.
    let out_reader = child.stdout.take().map(|mut o| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = o.read_to_end(&mut buf);
            buf
        })
    });
    let err_reader = child.stderr.take().map(|mut e| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = e.read_to_end(&mut buf);
            buf
        })
    });
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {}
            Err(e) => return Err(DrainError::WaitFailed(e.to_string())),
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(DrainError::Timeout);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let joined = |h: Option<std::thread::JoinHandle<Vec<u8>>>| {
        h.map(|j| j.join().unwrap_or_default()).unwrap_or_default()
    };
    Ok(DrainedChild {
        status,
        stdout: joined(out_reader),
        stderr: joined(err_reader),
    })
}

/// Run a shell command and capture fuller output for interactive display
/// (custom TUI commands). Same TERHUB_* environment as rule actions, but
/// stderr is kept (marked) and output caps at 4000 chars, not one line.
pub fn shell_capture(
    command: &str,
    pane: &str,
    rec: Option<&AgentRecord>,
    timeout: std::time::Duration,
) -> Result<String, String> {
    let (agent, session, window, state) = match rec {
        Some(r) => (
            r.agent.as_str(),
            r.session.as_str(),
            r.window.as_str(),
            r.state.label(),
        ),
        None => ("", "", "", ""),
    };
    let mut child = std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .env("TERHUB_PANE", pane)
        .env("TERHUB_AGENT", agent)
        .env("TERHUB_SESSION", session)
        .env("TERHUB_WINDOW", window)
        .env("TERHUB_STATE", state)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn failed: {e}"))?;
    let drained = match wait_drained(&mut child, timeout) {
        Ok(d) => d,
        Err(DrainError::Timeout) => return Err(format!("timed out after {}s", timeout.as_secs())),
        Err(DrainError::WaitFailed(e)) => return Err(format!("wait failed: {e}")),
    };
    let status = drained.status;
    let out = drained.stdout;
    let err = drained.stderr;
    let mut text = String::from_utf8_lossy(&out).trim().to_string();
    let err_text = String::from_utf8_lossy(&err);
    let err_text = err_text.trim();
    if !err_text.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str("[stderr]\n");
        text.push_str(err_text);
    }
    if text.is_empty() {
        text = if status.success() {
            "ok".to_string()
        } else {
            format!("exit {}", status.code().unwrap_or(-1))
        };
    } else if !status.success() {
        text = format!("exit {}\n{text}", status.code().unwrap_or(-1));
    }
    if text.chars().count() > 4000 {
        text = text.chars().take(3999).collect::<String>() + "…";
    }
    Ok(text)
}

/// Carry out one firing. Returns a one-line description for the ledger.
pub fn execute(tmux: Option<&Tmux>, fire: &Fire, rec: &AgentRecord) -> Result<String> {
    execute_with_timeout(tmux, fire, rec, SHELL_TIMEOUT)
}

/// [`execute`] with an explicit shell timeout (tests use a short one).
pub fn execute_with_timeout(
    tmux: Option<&Tmux>,
    fire: &Fire,
    rec: &AgentRecord,
    timeout: std::time::Duration,
) -> Result<String> {
    match &fire.action {
        Action::SendKeys { text } => {
            let tmux = tmux.context("no tmux")?;
            tmux.send_keys(&fire.pane, text, true)?;
            Ok(format!(
                "send-keys to {}: '{}'",
                fire.pane,
                truncate(text, 60)
            ))
        }
        Action::Shell { command } => {
            let mut child = std::process::Command::new("sh")
                .arg("-c")
                .arg(command)
                .env("TERHUB_PANE", &fire.pane)
                .env("TERHUB_AGENT", &rec.agent)
                .env("TERHUB_SESSION", &rec.session)
                .env("TERHUB_WINDOW", &rec.window)
                .env("TERHUB_STATE", rec.state.label())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()?;
            let drained = match wait_drained(&mut child, timeout) {
                Ok(d) => d,
                Err(DrainError::Timeout) => {
                    anyhow::bail!("shell '{}' timed out", truncate(command, 40))
                }
                Err(DrainError::WaitFailed(e)) => anyhow::bail!("{e}"),
            };
            let out = std::process::Output {
                status: drained.status,
                stdout: drained.stdout,
                stderr: Vec::new(),
            };
            let stdout_text = String::from_utf8_lossy(&out.stdout);
            let text = if stdout_text.trim().is_empty() {
                if out.status.success() {
                    "ok".to_string()
                } else {
                    out.status.to_string()
                }
            } else {
                truncate(stdout_text.trim(), 80)
            };
            Ok(format!("shell '{}': {}", truncate(command, 40), text))
        }
    }
}

fn truncate(s: &str, n: usize) -> String {
    let mut out: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Source;

    fn rec(pane: &str, state: AgentState, since: u64, summary: &str) -> AgentRecord {
        AgentRecord {
            pane: pane.to_string(),
            agent: "claude".to_string(),
            state,
            source: Source::Hook,
            summary: summary.to_string(),
            session: "s".to_string(),
            window: "w".to_string(),
            command: "claude".to_string(),
            updated_at: since,
            state_since: since,
            cpu_ticks: 0,
            cpu_still_since: 0,
            last_turn_secs: 0,
            screen_hash: 0,
            quiet_since: 0,
        }
    }

    #[test]
    fn corrupt_rules_file_errors_instead_of_empty() {
        let dir =
            std::env::temp_dir().join(format!("terhub-rules-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bad = dir.join("automation.toml");
        std::fs::write(&bad, "rule = [unclosed\n").unwrap();
        assert!(load_rules_from(&bad).is_err());
        let missing = dir.join("nope.toml");
        assert!(load_rules_from(&missing).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn rule() -> Rule {
        Rule {
            name: "resume".to_string(),
            enabled: true,
            on: "waiting".to_string(),
            agent: None,
            match_summary: Some("limit".to_string()),
            after_secs: 300,
            cooldown_secs: 3600,
            hourly_cap: 1,
            action: "send-keys".to_string(),
            text: Some("continue".to_string()),
            command: None,
        }
    }

    fn registry() -> HashMap<String, AgentRecord> {
        let mut reg = HashMap::new();
        reg.insert(
            "%1".to_string(),
            rec("%1", AgentState::Waiting, 1000, "rate limit hit"),
        );
        reg.insert("%2".to_string(), rec("%2", AgentState::Working, 1000, ""));
        reg
    }

    #[test]
    fn delay_blocks_then_fires() {
        let rules = vec![rule()];
        let reg = registry();
        // Held only 100s of the required 300s.
        let out = dry_run(&rules, &reg, &[], 1100);
        assert!(matches!(out[0].2, Verdict::WaitDelay(200)), "{out:?}");
        // Held long enough: fires. %2 never matches (wrong state).
        let out = dry_run(&rules, &reg, &[], 1400);
        assert_eq!(out.len(), 1);
        assert!(matches!(&out[0].2, Verdict::Fire(f) if f.pane == "%1"));
    }

    #[test]
    fn cooldown_and_cap_guard() {
        let rules = vec![rule()];
        let reg = registry();
        let fires = vec![("resume".to_string(), "%1".to_string(), 1300)];
        let out = dry_run(&rules, &reg, &fires, 1400);
        assert!(matches!(out[0].2, Verdict::Cooldown(_)), "{out:?}");
        // Short cooldown but hourly cap (1) still blocks.
        let mut capped = rule();
        capped.cooldown_secs = 60;
        let out = dry_run(&[capped], &reg, &fires, 1400);
        assert!(matches!(out[0].2, Verdict::Capped), "{out:?}");
    }

    #[test]
    fn future_dated_fire_cools_down_without_panic() {
        let rules = vec![rule()];
        let reg = registry();
        // Clock stepped back (or a hand-edited ledger): the last fire is in
        // the future. Must read as "just fired", not panic on `now - t`.
        let fires = vec![("resume".to_string(), "%1".to_string(), 1500)];
        let out = dry_run(&rules, &reg, &fires, 1400);
        assert!(matches!(out[0].2, Verdict::Cooldown(3600)), "{out:?}");
    }

    #[test]
    fn disabled_and_invalid_rules_skipped() {
        let mut bad = rule();
        bad.enabled = false;
        let mut invalid = rule();
        invalid.name = "invalid".to_string();
        invalid.on = "bogus".to_string();
        let out = dry_run(&[bad, invalid], &registry(), &[], 9999);
        assert!(out.is_empty());
        assert!(validate(&rule()).is_empty());
    }

    #[test]
    fn cap_holds_within_one_pass() {
        let mut reg = HashMap::new();
        for i in 0..5 {
            let pane = format!("%{i}");
            reg.insert(
                pane.clone(),
                rec(&pane, AgentState::Waiting, 1000, "rate limit hit"),
            );
        }
        let out = dry_run(&[rule()], &reg, &[], 1400);
        let fires = out
            .iter()
            .filter(|(_, _, v)| matches!(v, Verdict::Fire(_)))
            .count();
        // hourly_cap is 1: exactly one fire per pass, the rest capped.
        assert_eq!(fires, 1, "{out:?}");
        assert_eq!(out.len(), 5);
    }

    #[test]
    fn summary_filter_applies() {
        let mut r = rule();
        r.match_summary = Some("approve".to_string());
        let out = dry_run(&[r], &registry(), &[], 9999);
        assert!(out.is_empty(), "{out:?}");
    }

    #[test]
    fn zero_cap_is_invalid() {
        let mut r = rule();
        r.hourly_cap = 0;
        assert!(
            validate(&r).iter().any(|p| p.contains("hourly_cap")),
            "cap 0 must be flagged"
        );
        // And an invalid rule never fires.
        let out = dry_run(&[r], &registry(), &[], 9999);
        assert!(out.is_empty());
    }

    #[test]
    fn longest_waiting_gets_the_cap_slot() {
        let mut reg = HashMap::new();
        reg.insert(
            "%1".to_string(),
            rec("%1", AgentState::Waiting, 1000, "rate limit hit"),
        );
        reg.insert(
            "%2".to_string(),
            rec("%2", AgentState::Waiting, 500, "rate limit hit"),
        );
        // Cap 1: %2 held the state twice as long, so it fires and %1 caps.
        let out = dry_run(&[rule()], &reg, &[], 1400);
        assert_eq!(out.len(), 2);
        assert!(matches!(&out[0].2, Verdict::Capped), "{out:?}");
        assert!(
            matches!(&out[1].2, Verdict::Fire(f) if f.pane == "%2"),
            "{out:?}"
        );
    }

    #[test]
    fn hanging_shell_action_times_out() {
        let fire = Fire {
            rule: "r".to_string(),
            pane: "%1".to_string(),
            action: Action::Shell {
                command: "sleep 30".to_string(),
            },
        };
        let rec = rec("%1", AgentState::Waiting, 0, "");
        let err =
            execute_with_timeout(None, &fire, &rec, std::time::Duration::from_secs(1)).unwrap_err();
        assert!(format!("{err:?}").contains("timed out"), "{err:?}");
    }

    #[test]
    fn wait_drained_reports_timeout() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let err = wait_drained(&mut child, std::time::Duration::from_millis(200)).unwrap_err();
        assert!(matches!(err, DrainError::Timeout));
    }

    #[test]
    fn shell_action_survives_pipe_filling_output() {
        // ~230KB: far past the 64KB pipe buffer that used to wedge the
        // wait loop until the timeout. Must complete, not time out.
        let fire = Fire {
            rule: "r".to_string(),
            pane: "%1".to_string(),
            action: Action::Shell {
                command: "seq 1 30000".to_string(),
            },
        };
        let rec = rec("%1", AgentState::Waiting, 0, "");
        let out = execute_with_timeout(None, &fire, &rec, std::time::Duration::from_secs(10))
            .expect("verbose shell action must not time out");
        assert!(out.contains("shell 'seq 1 30000'"), "{out}");
    }

    #[test]
    fn shell_capture_survives_pipe_filling_output() {
        let out = shell_capture(
            "seq 1 30000",
            "%1",
            None,
            std::time::Duration::from_secs(10),
        )
        .expect("verbose capture must not time out");
        assert!(out.starts_with("1\n2\n3"), "{out:.100}");
    }
}
