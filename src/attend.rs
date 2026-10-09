//! Attention flow: who needs you, peek reports, status line, notifications.
use crate::agent::{AgentRecord, AgentState, Source, now_unix};
use crate::daemon;
use crate::history::fmt_dur;
use crate::tmux::{Pane, Tmux};
use anyhow::Result;
use std::collections::HashMap;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Picking
// ---------------------------------------------------------------------------

/// Agents needing attention first, longest-waiting first.
pub fn needy_first(mut agents: Vec<AgentRecord>) -> Vec<AgentRecord> {
    agents.sort_by(|a, b| {
        b.state
            .needs_attention()
            .cmp(&a.state.needs_attention())
            .then(a.state_since.cmp(&b.state_since))
            .then(a.pane.cmp(&b.pane))
    });
    agents
}

/// The agent to jump to: longest-waiting, or the next after `after` when
/// cycling through everyone who needs you.
pub fn pick_attend(snapshot: &[AgentRecord], after: Option<&str>) -> Option<AgentRecord> {
    let needy: Vec<AgentRecord> = needy_first(snapshot.to_vec())
        .into_iter()
        .filter(|a| a.state.needs_attention())
        .collect();
    if needy.is_empty() {
        return None;
    }
    match after.and_then(|id| needy.iter().position(|a| a.pane == id)) {
        Some(i) => Some(needy[(i + 1) % needy.len()].clone()),
        None => Some(needy[0].clone()),
    }
}

fn attend_last_path() -> std::path::PathBuf {
    daemon::runtime_dir().join("attend_last")
}

pub fn read_last_attend() -> Option<String> {
    std::fs::read_to_string(attend_last_path())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn write_last_attend(pane: &str) {
    let _ = std::fs::write(attend_last_path(), pane);
}

// ---------------------------------------------------------------------------
// Peek
// ---------------------------------------------------------------------------

/// Resolve the tmux window holding a pane.
pub fn window_of_pane(tmux: &Tmux, pane: &str) -> Result<String> {
    Ok(tmux.display(pane, "#{window_id}")?.trim().to_string())
}

/// The caller's window; only defined inside tmux.
pub fn current_window(tmux: &Tmux) -> Result<(String, String)> {
    let pane = std::env::var("TMUX_PANE").map_err(|_| anyhow::anyhow!("peek needs tmux"))?;
    let window = window_of_pane(tmux, &pane)?;
    let session = tmux.display(&window, "#{session_name}")?.trim().to_string();
    Ok((session, window))
}

pub struct PeekRow {
    pub digit: char,
    pub pane: Pane,
    pub agent: Option<AgentRecord>,
}

const DIGITS: &str = "1234567890abcdefghijklmnopqrstuvwxyz";

/// Number the panes of one window for digit-to-jump.
pub fn peek_rows(panes: Vec<Pane>, agents: &HashMap<String, AgentRecord>) -> Vec<PeekRow> {
    panes
        .into_iter()
        .zip(DIGITS.chars())
        .map(|(pane, digit)| {
            let agent = agents.get(&pane.id).cloned();
            PeekRow { digit, pane, agent }
        })
        .collect()
}

fn truncate(s: &str, n: usize) -> String {
    let mut out: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        out.push('…');
    }
    out
}

pub fn peek_report(
    session: &str,
    window: &str,
    rows: &[PeekRow],
    total_panes: usize,
    now: u64,
) -> String {
    let needy = rows
        .iter()
        .filter(|r| r.agent.as_ref().is_some_and(|a| a.state.needs_attention()))
        .count();
    let idle = rows
        .iter()
        .filter(|r| {
            r.agent
                .as_ref()
                .is_some_and(|a| a.state == AgentState::Idle)
        })
        .count();
    let idle_note = if idle > 0 {
        format!(", {idle} idle")
    } else {
        String::new()
    };
    let mut out =
        format!("{session}:{window} — {total_panes} pane(s), {needy} need you{idle_note}\n");
    let mut newest: Option<(&str, u64)> = None;
    for row in rows {
        let mark = if row.pane.active { "*" } else { " " };
        let (agent, state, age, summary) = match &row.agent {
            Some(a) => (
                a.agent.clone(),
                a.state.label().to_string(),
                fmt_dur(a.waiting_secs(now)),
                truncate(&a.summary, 40),
            ),
            None => (
                "-".to_string(),
                "-".to_string(),
                "-".to_string(),
                String::new(),
            ),
        };
        out.push_str(&format!(
            "[{}] {}{}  {:<8} {:<7} {:<4} {:<9} {}\n",
            row.digit, mark, row.pane.id, agent, state, age, row.pane.command, summary,
        ));
        // Last prompt under its row, so the eye keeps row context.
        if let Some(a) = row.agent.as_ref()
            && a.last_prompt_at > 0
            && !a.last_prompt.is_empty()
        {
            out.push_str(&format!(
                "      ↩ {} ago: {}\n",
                fmt_dur(now.saturating_sub(a.last_prompt_at)),
                truncate(&a.last_prompt, 60),
            ));
            if newest.is_none_or(|(_, t)| a.last_prompt_at > t) {
                newest = Some((a.pane.as_str(), a.last_prompt_at));
            }
        }
    }
    if let Some((pane, at)) = newest {
        out.push_str(&format!(
            "newest prompt: {pane} ({} ago)\n",
            fmt_dur(now.saturating_sub(at))
        ));
    }
    if total_panes > rows.len() {
        // peek_rows only has digits for the first 36; say what's hidden.
        out.push_str(&format!(
            "…and {} more (no digit)\n",
            total_panes - rows.len()
        ));
    }
    out.push_str("digit to jump · q quit");
    out
}

// ---------------------------------------------------------------------------
// Status line (tmux status-right)
// ---------------------------------------------------------------------------

pub fn status_line(snapshot: &[AgentRecord], pane: Option<&str>) -> String {
    match pane {
        Some(id) => match snapshot.iter().find(|a| a.pane == id) {
            Some(a) => {
                let age = fmt_dur(a.waiting_secs(now_unix()));
                format!("{}:{} {}", a.agent, a.state.label(), age)
            }
            None => "terhub: -".to_string(),
        },
        None => {
            let needy = snapshot
                .iter()
                .filter(|a| a.state.needs_attention())
                .count();
            // Idle is disjoint from needy, so the two counters read
            // together: how much is blocked vs. how much is finished.
            // Omitted at zero — a quiet fleet shouldn't grow the line.
            let idle = snapshot
                .iter()
                .filter(|a| a.state == AgentState::Idle)
                .count();
            let idle_note = if idle > 0 {
                format!(" · {idle} idle")
            } else {
                String::new()
            };
            if needy > 0 {
                format!("◉ {needy} need you{idle_note}")
            } else {
                format!("◎ {} agent(s){idle_note}", snapshot.len())
            }
        }
    }
}

/// Text table for `terhub status`: the TUI's agent block (state, source,
/// time-in-state, last turn, quiet age) in CLI form. Unknown turn/quiet
/// clocks read `—`, mirroring the table.
pub fn status_table(agents: &[AgentRecord], now: u64) -> String {
    let mut out = String::from(
        "PANE  HANDLE       AGENT    STATE    SRC  FOR   TURN  IDLE  SESSION   SUMMARY\n",
    );
    for a in agents {
        let src = match a.source {
            Source::Hook => "hook",
            Source::Proc => "proc",
            Source::Manual => "man",
            Source::Screen => "scr",
        };
        let turn = if a.last_turn_secs == 0 {
            "—".to_string()
        } else {
            fmt_dur(a.last_turn_secs)
        };
        let idle = if a.quiet_since == 0 {
            "—".to_string()
        } else {
            fmt_dur(now.saturating_sub(a.quiet_since))
        };
        let handle = if a.handle.is_empty() {
            "—"
        } else {
            &a.handle
        };
        out.push_str(&format!(
            "{:<5} {:<12} {:<8} {:<8} {:<4} {:<5} {:<5} {:<5} {:<9} {}\n",
            a.pane,
            truncate(handle, 12),
            a.agent,
            a.state.label(),
            src,
            fmt_dur(a.waiting_secs(now)),
            turn,
            idle,
            a.session,
            a.summary,
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// Notifications
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Transition {
    pub pane: String,
    pub agent: String,
    pub from: AgentState,
    pub to: AgentState,
    pub session: String,
    pub window: String,
    pub summary: String,
}

/// Every state change between registries (new panes read as Unknown →
/// state, removed panes as state → Unknown). The ledger logs all of these;
/// only the attention subset notifies.
pub fn detect_all_transitions(
    prev: &HashMap<String, AgentRecord>,
    next: &HashMap<String, AgentRecord>,
) -> Vec<Transition> {
    let mut out = Vec::new();
    for (pane, rec) in next {
        if prev.get(pane).is_some_and(|o| o.state == rec.state) {
            continue;
        }
        let from = prev
            .get(pane)
            .map(|o| o.state)
            .unwrap_or(AgentState::Unknown);
        out.push(Transition {
            pane: pane.clone(),
            agent: rec.agent.clone(),
            from,
            to: rec.state,
            session: rec.session.clone(),
            window: rec.window.clone(),
            summary: rec.summary.clone(),
        });
    }
    // Panes present before but gone now (killed, session closed): the ledger
    // records the departure as a move to Unknown. Unknown never notifies,
    // so this is history only.
    for (pane, old) in prev {
        if next.contains_key(pane) {
            continue;
        }
        out.push(Transition {
            pane: pane.clone(),
            agent: old.agent.clone(),
            from: old.state,
            to: AgentState::Unknown,
            session: old.session.clone(),
            window: old.window.clone(),
            summary: old.summary.clone(),
        });
    }
    out.sort_by(|a, b| a.pane.cmp(&b.pane));
    out
}

/// Records that newly entered an attention state since the previous registry.
pub fn detect_transitions(
    prev: &HashMap<String, AgentRecord>,
    next: &HashMap<String, AgentRecord>,
) -> Vec<Transition> {
    detect_all_transitions(prev, next)
        .into_iter()
        .filter(|t| t.to.needs_attention())
        .collect()
}

/// A transition into Idle from a live state is a completion worth one
/// desktop note (done, not needy: no badge, no attend). Unknown→Idle is
/// a fresh discovery, not a finish, and never pings.
pub fn is_completion(from: AgentState, to: AgentState) -> bool {
    to == AgentState::Idle && !matches!(from, AgentState::Unknown | AgentState::Idle)
}

/// One desktop note for a finished agent (done, not needy: no badge,
/// no attend). Same toggle as attention notes.
pub fn notify_done(t: &Transition) {
    if !crate::config::load_config(None).notifications_enabled {
        return;
    }
    let title = format!("terhub: {} done", t.agent);
    let mut body = format!("{}:{} · {}", t.session, t.window, t.pane);
    if !t.summary.is_empty() {
        body.push_str(&format!(" — {}", truncate(&t.summary, 80)));
    }
    let _ = run_notifier("notify-send", &[title.as_str(), body.as_str()]);
}

pub fn notify_transition(t: &Transition) {
    if !crate::config::load_config(None).notifications_enabled {
        return;
    }
    let title = format!("terhub: {} needs you ({})", t.agent, t.to.label());
    let mut body = format!("{}:{} · {}", t.session, t.window, t.pane);
    if !t.summary.is_empty() {
        body.push_str(&format!(" — {}", truncate(&t.summary, 80)));
    }
    let _ = run_notifier("notify-send", &[title.as_str(), body.as_str()]);
}

fn run_notifier(cmd: &str, args: &[&str]) -> Result<()> {
    run_notifier_timeout(cmd, args, Duration::from_secs(5))
}

/// Run the notifier with a bounded wait: a hung notify-send is killed and
/// reported instead of stalling the poll loop forever.
fn run_notifier_timeout(cmd: &str, args: &[&str], timeout: Duration) -> Result<()> {
    let mut child = std::process::Command::new(cmd).args(args).spawn()?;
    let start = std::time::Instant::now();
    loop {
        match child.try_wait()? {
            Some(status) if status.success() => return Ok(()),
            Some(status) => anyhow::bail!("notifier exited {status}"),
            None if start.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("notifier timed out after {timeout:?}")
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Source;

    #[test]
    fn status_table_carries_turn_idle_and_src() {
        let now = 10_000;
        let mut a = rec("%1", AgentState::Working, now - 300);
        a.source = Source::Hook;
        a.last_turn_secs = 90;
        a.quiet_since = now - 60;
        a.summary = "building".to_string();
        a.handle = "@claude-s".to_string();
        let b = rec("%2", AgentState::Waiting, now - 120);
        let table = status_table(&[a, b], now);
        let mut lines = table.lines();
        assert_eq!(
            lines.next().unwrap(),
            "PANE  HANDLE       AGENT    STATE    SRC  FOR   TURN  IDLE  SESSION   SUMMARY"
        );
        assert_eq!(
            lines.next().unwrap(),
            "%1    @claude-s    claude   working  hook 5m    1m    1m    s         building"
        );
        // Unknown clocks read as dashes, like the TUI table.
        let row2 = lines.next().unwrap();
        assert!(row2.contains('—'), "{row2}");
    }

    #[test]
    fn completion_pings_live_to_idle_only() {
        use crate::agent::AgentState::*;
        assert!(is_completion(Working, Idle));
        assert!(is_completion(Waiting, Idle));
        assert!(is_completion(Error, Idle));
        // Fresh discoveries and non-endings stay silent.
        assert!(!is_completion(Unknown, Idle));
        assert!(!is_completion(Idle, Idle));
        assert!(!is_completion(Working, Waiting));
        assert!(!is_completion(Working, Working));
    }

    #[test]
    fn hung_notifier_times_out() {
        // A notifier that never exits must not stall the poll loop: bounded
        // wait, kill, error — all well under the child's own lifetime.
        let start = std::time::Instant::now();
        let err = run_notifier_timeout("sleep", &["30"], Duration::from_millis(100));
        assert!(err.is_err());
        assert!(start.elapsed() < Duration::from_secs(5));
        // A healthy notifier still succeeds.
        assert!(run_notifier_timeout("true", &[], Duration::from_secs(5)).is_ok());
    }

    fn rec(pane: &str, state: AgentState, since: u64) -> AgentRecord {
        AgentRecord {
            pane: pane.to_string(),
            agent: "claude".to_string(),
            state,
            source: Source::Screen,
            summary: String::new(),
            session: "s".to_string(),
            window: "w".to_string(),
            command: "claude".to_string(),
            updated_at: since,
            state_since: since,
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

    #[test]
    fn needy_sorted_longest_waiting_first() {
        let agents = vec![
            rec("%1", AgentState::Working, 100),
            rec("%2", AgentState::Waiting, 200),
            rec("%3", AgentState::Error, 150),
        ];
        let sorted = needy_first(agents);
        let panes: Vec<&str> = sorted.iter().map(|a| a.pane.as_str()).collect();
        assert_eq!(panes, vec!["%3", "%2", "%1"]);
    }

    #[test]
    fn attend_cycles_through_needy() {
        let snap = vec![
            rec("%1", AgentState::Waiting, 100),
            rec("%2", AgentState::Waiting, 200),
            rec("%3", AgentState::Working, 50),
        ];
        assert_eq!(pick_attend(&snap, None).unwrap().pane, "%1");
        assert_eq!(pick_attend(&snap, Some("%1")).unwrap().pane, "%2");
        assert_eq!(pick_attend(&snap, Some("%2")).unwrap().pane, "%1");
        assert_eq!(pick_attend(&snap, Some("%9")).unwrap().pane, "%1");
    }

    #[test]
    fn attend_none_when_all_clear() {
        let snap = vec![rec("%1", AgentState::Working, 100)];
        assert!(pick_attend(&snap, None).is_none());
        assert!(pick_attend(&[], None).is_none());
    }

    #[test]
    fn transitions_fire_once_per_entry() {
        let mut prev = HashMap::new();
        prev.insert("%1".to_string(), rec("%1", AgentState::Working, 100));
        let mut next = HashMap::new();
        next.insert("%1".to_string(), rec("%1", AgentState::Waiting, 200));
        next.insert("%2".to_string(), rec("%2", AgentState::Waiting, 200));
        let t = detect_transitions(&prev, &next);
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].from, AgentState::Working);
        // Same state again: silent.
        let t2 = detect_transitions(&next, &next);
        assert!(t2.is_empty());
    }

    #[test]
    fn all_transitions_include_quiet_states() {
        let mut prev = HashMap::new();
        prev.insert("%1".to_string(), rec("%1", AgentState::Waiting, 100));
        prev.insert("%2".to_string(), rec("%2", AgentState::Working, 100));
        let mut next = HashMap::new();
        // Waiting → working (a turn started): logged, never notified.
        next.insert("%1".to_string(), rec("%1", AgentState::Working, 200));
        // Working → idle: logged, never notified.
        next.insert("%2".to_string(), rec("%2", AgentState::Idle, 200));
        // Brand-new pane reads as Unknown → state.
        next.insert("%3".to_string(), rec("%3", AgentState::Working, 200));
        let all = detect_all_transitions(&prev, &next);
        assert_eq!(all.len(), 3);
        assert!(all.iter().any(|t| t.from == AgentState::Unknown));
        // The attention subset stays empty: nothing here needs you.
        assert!(detect_transitions(&prev, &next).is_empty());
    }

    #[test]
    fn removed_panes_log_a_departure() {
        let mut prev = HashMap::new();
        prev.insert("%1".to_string(), rec("%1", AgentState::Working, 100));
        prev.insert("%2".to_string(), rec("%2", AgentState::Waiting, 100));
        let mut next = HashMap::new();
        next.insert("%1".to_string(), rec("%1", AgentState::Working, 200));
        let all = detect_all_transitions(&prev, &next);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].pane, "%2");
        assert_eq!(all[0].from, AgentState::Waiting);
        assert_eq!(all[0].to, AgentState::Unknown);
        // Departures are history, never a ping.
        assert!(detect_transitions(&prev, &next).is_empty());
    }

    #[test]
    fn peek_names_hidden_overflow() {
        use crate::tmux::Pane;
        let panes: Vec<Pane> = (0..40)
            .map(|i| Pane::test(&format!("%{i}"), "w", "sh"))
            .collect();
        let agents = HashMap::new();
        let rows = peek_rows(panes, &agents);
        assert_eq!(rows.len(), 36);
        let report = peek_report("s", "w", &rows, 40, 0);
        assert!(report.contains("40 pane(s)"), "{report}");
        assert!(report.contains("…and 4 more"), "{report}");
    }

    #[test]
    fn peek_report_counts_idle_agents() {
        use crate::tmux::Pane;
        let now = 10_000;
        let panes = vec![
            Pane::test("%1", "w", "pi"),
            Pane::test("%2", "w", "pi"),
            Pane::test("%3", "w", "pi"),
        ];
        let mut agents = HashMap::new();
        agents.insert("%1".to_string(), rec("%1", AgentState::Waiting, now - 60));
        agents.insert("%2".to_string(), rec("%2", AgentState::Idle, now - 60));
        agents.insert("%3".to_string(), rec("%3", AgentState::Idle, now - 60));
        let rows = peek_rows(panes, &agents);
        let report = peek_report("s", "w", &rows, 3, now);
        assert!(report.contains("1 need you"), "{report}");
        assert!(report.contains("2 idle"), "{report}");
    }

    #[test]
    fn peek_report_shows_prompt_ages_and_newest() {
        use crate::tmux::Pane;
        let now = 10_000;
        let mut a = rec("%1", AgentState::Working, now - 300);
        a.last_prompt_at = now - 120;
        a.last_prompt = "fix the parser".to_string();
        let mut b = rec("%2", AgentState::Waiting, now - 60);
        b.last_prompt_at = now - 30;
        b.last_prompt = "ship it".to_string();
        let panes = vec![
            Pane::test("%1", "w", "claude"),
            Pane::test("%2", "w", "claude"),
        ];
        let agents: HashMap<String, AgentRecord> =
            [a, b].into_iter().map(|r| (r.pane.clone(), r)).collect();
        let rows = peek_rows(panes, &agents);
        let report = peek_report("s", "w", &rows, 2, now);
        assert!(report.contains("2m ago: fix the parser"), "{report}");
        assert!(report.contains("30s ago: ship it"), "{report}");
        assert!(report.contains("newest prompt: %2"), "{report}");
        // No prompts anywhere: no prompt lines, no newest note.
        let panes = vec![Pane::test("%3", "w", "sh")];
        let rows = peek_rows(panes, &HashMap::new());
        let report = peek_report("s", "w", &rows, 1, now);
        assert!(!report.contains("ago:"), "{report}");
        assert!(!report.contains("newest prompt"), "{report}");
    }

    #[test]
    fn status_line_variants() {
        let snap = vec![rec("%1", AgentState::Waiting, 0)];
        assert!(status_line(&snap, None).contains("1 need you"));
        assert!(status_line(&[], None).contains("0 agent"));
        assert!(status_line(&snap, Some("%1")).contains("claude:waiting"));
        assert_eq!(status_line(&snap, Some("%9")), "terhub: -");
    }

    #[test]
    fn status_line_counts_idle_agents() {
        // Idle and needy are disjoint, so both counters can show at once.
        let snap = vec![
            rec("%1", AgentState::Waiting, 0),
            rec("%2", AgentState::Idle, 0),
            rec("%3", AgentState::Idle, 0),
            rec("%4", AgentState::Working, 0),
        ];
        let line = status_line(&snap, None);
        assert!(line.contains("1 need you"), "{line}");
        assert!(line.contains("2 idle"), "{line}");
        // No idle rows: the counter stays out of the line entirely.
        let busy = vec![rec("%1", AgentState::Working, 0)];
        assert!(
            !status_line(&busy, None).contains("idle"),
            "{}",
            status_line(&busy, None)
        );
    }

    #[test]
    fn notifier_result_follows_exit_code() {
        assert!(run_notifier("true", &[]).is_ok());
        assert!(run_notifier("false", &[]).is_err());
        assert!(run_notifier("terhub-no-such-command", &[]).is_err());
    }
}
