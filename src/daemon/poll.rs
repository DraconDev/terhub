//! The poll cycle and the state machine it feeds.
//!
//! One poll walks every pane, captures its screen, classifies it, and
//! merges the result with what the daemon already knew. The state
//! machine (the merge, the stuck gate, the quiet clocks) lives here
//! too: it is the part of the daemon that decides what a pane *is*,
//! and it is worth reading on its own.

use super::{Tunables, lock_registry, log_entry};
// Only the test-only `merge_record` wrapper reads these; the poller
// passes them in.
#[cfg(test)]
use super::{DONE_IDLE_SECS, QUIET_IDLE_SECS, WAITING_IDLE_SECS};
use crate::agent::{
    AgentRecord, AgentState, CompiledManifest, HookEvent, ScreenHit, Source, apply_event_to,
    classify_screen, is_harness_command, is_shell_command, now_unix,
};
use crate::attend::detect_all_transitions;
use crate::history;
use crate::proc::is_editor_name as is_editor_command;
use crate::tmux::{Pane, Tmux};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

/// The pause clock carried in from the previous poll, if the record was
/// already paused.
///
/// The merge re-derives the state *and its clock* from live evidence, so
/// by the time the pause is applied the carried clock is gone. It has to
/// be read before the merge, or "paused for 2h" would read "paused for
/// 2s" — the clock would reset on every poll.
pub(crate) fn carried_pause_since(rec: Option<&AgentRecord>) -> Option<u64> {
    rec.filter(|r| r.state == AgentState::Paused)
        .map(|r| r.state_since)
}

/// A pane that was paused and is running again must not inherit the
/// pause's silence.
///
/// The still-clock is "the tick sum last changed" — while the agent is
/// frozen it does not change, so the clock ages for as long as the pane
/// stays paused. On resume, the first poll may still see the same tick
/// sum (the agent has not burned a tick yet), and the stuck gate would
/// then fire on a clock that measured the *freeze* rather than the
/// agent's work: a hang that never happened.
///
/// Clearing the clock gives the resumed agent a fresh chance. The next
/// poll either sees a moved tick sum (the agent is working again, and
/// the gate stays quiet) or keeps the fresh clock and ages it normally,
/// so a genuinely idle agent still becomes stuck.
pub(crate) fn clear_pause_silence(rec: &mut AgentRecord, still_paused: bool) {
    if !still_paused && rec.state == AgentState::Paused {
        rec.cpu_still_since = 0;
    }
}

/// Mark a record as paused, or leave it alone.
///
/// A pane whose agent process is stopped is deliberately frozen, not
/// hung. A stopped process burns no CPU, so the Stuck gate — which
/// reads "no CPU" as "hung" — would report terhub's own `z` as a dead
/// agent, escalate it, and notify the user about a hang that does not
/// exist.
///
/// The state *is* the registry's record: it is persisted with the rest
/// of the agent record, so the pane reads as paused in the TUI, in
/// `terhub status`, and after a daemon restart. The evidence is read
/// from the process state every poll and never carried, so a resumed
/// pane is working again on the next tick and the mark cannot go
/// stale.
///
/// `prev_paused_since` is the clock carried in from the previous poll
/// (see [`carried_pause_since`]): the moment the pane was first seen
/// frozen. Without it the clock would restart on every poll.
pub(crate) fn apply_paused(
    rec: &mut AgentRecord,
    paused: bool,
    now: u64,
    prev_paused_since: Option<u64>,
) {
    if !paused {
        return;
    }
    rec.state = AgentState::Paused;
    rec.state_since = prev_paused_since.unwrap_or(now);
    rec.summary = "paused".to_string();
}

/// Pin a manual agent record (`terhub register`). Same agent + state
/// refreshes the pin in place (new pid wins, clock kept); anything else
/// is a fresh pin with a fresh clock. Pure registry work for the lock.
pub(crate) fn apply_register(
    reg: &mut HashMap<String, AgentRecord>,
    pane: &str,
    agent: &str,
    pid: u32,
    state: AgentState,
    now: u64,
) {
    let agent = crate::proc::canonical_agent(agent);
    match reg.get(pane) {
        // Same pin refreshed: the new pid wins, the clock is kept so a
        // re-register after an agent restart doesn't renotify.
        Some(old) if old.source == Source::Manual && old.agent == agent && old.state == state => {
            let mut rec = old.clone();
            rec.reg_pid = pid;
            rec.updated_at = now;
            rec.cpu_ticks = 0;
            rec.cpu_still_since = 0;
            rec.agent_start = 0;
            reg.insert(pane.to_string(), rec);
        }
        _ => {
            // Fresh pin (or a pin over real evidence): fresh clock, but
            // keep pane metadata and model/ctx so the row isn't blank.
            let old = reg.get(pane);
            // A changed agent drops the name so ensure_handles mints a
            // fresh one (same rule as hook events).
            let handle = match old {
                Some(o) if o.agent == agent => o.handle.clone(),
                _ => String::new(),
            };
            // Entering Working (or a new agent taking the pane) is
            // activity in itself, pixels or not: restart the
            // visible-quiet clock (same rule as hook events).
            let fresh_work =
                state == AgentState::Working || old.map(|r| r.agent != agent).unwrap_or(true);
            reg.insert(
                pane.to_string(),
                AgentRecord {
                    pane: pane.to_string(),
                    agent,
                    state,
                    source: Source::Manual,
                    summary: String::new(),
                    session: old.map(|r| r.session.clone()).unwrap_or_default(),
                    window: old.map(|r| r.window.clone()).unwrap_or_default(),
                    command: old.map(|r| r.command.clone()).unwrap_or_default(),
                    updated_at: now,
                    state_since: now,
                    cpu_ticks: 0,
                    cpu_still_since: 0,
                    agent_start: 0,
                    last_turn_secs: old.map(|r| r.last_turn_secs).unwrap_or(0),
                    screen_hash: old.map(|r| r.screen_hash).unwrap_or(0),
                    quiet_since: if fresh_work {
                        now
                    } else {
                        old.map(|r| r.quiet_since).unwrap_or(0)
                    },
                    last_prompt_at: old.map(|r| r.last_prompt_at).unwrap_or(0),
                    last_prompt: old.map(|r| r.last_prompt.clone()).unwrap_or_default(),
                    handle,
                    model: old.map(|r| r.model.clone()).unwrap_or_default(),
                    ctx_pct: old.and_then(|r| r.ctx_pct),
                    reg_pid: pid,
                    bell: false,
                    bell_reason: String::new(),
                    stuck_base: None,
                },
            );
        }
    }
}

/// Drop a manual pin (`terhub unregister`). Only Manual records go:
/// a hook/proc/screen record under the pane is real evidence, never
/// removed by unregister. Returns true when a pin was dropped.
pub(crate) fn apply_unregister(reg: &mut HashMap<String, AgentRecord>, pane: &str) -> bool {
    match reg.get(pane) {
        Some(old) if old.source == Source::Manual => {
            reg.remove(pane);
            true
        }
        _ => false,
    }
}

/// Apply one hook event to the registry and report its transitions.
/// Pure registry work — no disk, no subprocess — so callers run it under
/// the lock and emit side effects after unlocking.
pub(crate) fn apply_hook_event(
    reg: &mut HashMap<String, AgentRecord>,
    event: &HookEvent,
    now: u64,
) -> Vec<crate::attend::Transition> {
    let before = reg.clone();
    apply_event_to(reg, event, now);
    crate::agent::ensure_handles(reg);
    detect_all_transitions(&before, reg)
}

/// Fire the notification for one transition, detached.
///
/// The notifier is a subprocess (`notify-send`) with a 5s bound. Waiting
/// for it inline parked the tokio worker that ran it — stretching the
/// poll cycle by 5s for every attention transition. On the blocking
/// pool, detached, the poll moves on and the toast still goes out.
pub(crate) fn notify_detached(t: &crate::attend::Transition) {
    let t = t.clone();
    // Decide *here*, on the thread that noticed the transition. The
    // notifier itself runs detached on a blocking thread, and a config
    // read there would answer for a moment that has already passed —
    // and, in the test suite, on a thread that cannot see the test's
    // own config at all.
    if !crate::attend::notifications_enabled() {
        return;
    }
    // Spawn the child here too: resolving the program against the
    // dispatcher's PATH cannot then be undone by a PATH change that
    // lands before the detached task gets to run.
    let child = if t.to.needs_attention() {
        crate::attend::spawn_transition_notifier(&t)
    } else if crate::attend::is_completion(t.from, t.to) {
        crate::attend::spawn_done_notifier(&t)
    } else {
        None
    };
    if let Some(child) = child {
        drop(tokio::task::spawn_blocking(move || {
            crate::attend::wait_notifier(child);
        }));
    }
}

/// Emit transition side effects: ledger lines always, a desktop
/// notification for attention states, a one-shot note for completions.
/// Runs WITHOUT the registry lock — a slow notify-send must never stall
/// polls or snapshots.
///
/// The ledger write is awaited — the ledger's order is the record of
/// what happened, so it must not be reordered by a detached task. The
/// notification is not: it runs detached (see [`notify_detached`]), so
/// a slow notifier cannot stall polls or snapshots.
pub(crate) async fn emit_transitions(transitions: &[crate::attend::Transition], now: u64) {
    for t in transitions {
        log_entry(history::Entry::transition(now, t)).await;
        notify_detached(t);
    }
}

/// Fold hook events that landed in the live registry *during* a poll back
/// into the poll result. `poll_sync` only saw the pre-poll clone, so a blind
/// write-back would silently discard them (lost update); the hook handler
/// already logged their transitions, so these merge silently. Only hook
/// records at least as fresh as `poll_start` (the second the clone was
/// taken) merge — older hooks were already carried or lapsed by the poll
/// itself and must not be resurrected.
pub(crate) fn merge_mid_poll_hooks(
    next: &mut HashMap<String, AgentRecord>,
    live: &HashMap<String, AgentRecord>,
    poll_start: u64,
    live_panes: Option<&std::collections::HashSet<String>>,
) {
    for (pane, rec) in live {
        // A Hook record for a pane the poll never listed is a ghost —
        // a dead pane's last event, or a forged one — and must not be
        // re-inserted into a result rebuilt from live panes only, where
        // it would linger to the stale window. Manual pins are exempt:
        // deliberate human state must survive a poll that merely missed
        // its pane. Unknown live set (outage) keeps the old
        // preserve-everything behavior.
        let listed = match live_panes {
            None => true,
            Some(set) => set.contains(pane),
        };
        if rec.source == Source::Hook && rec.updated_at >= poll_start && listed {
            next.insert(pane.clone(), rec.clone());
        }
        // A register that landed mid-poll is fresher than the poll's
        // clone and wins the same way hook events do.
        if rec.source == Source::Manual && rec.updated_at >= poll_start {
            next.insert(pane.clone(), rec.clone());
        }
    }
    // ...while an unregister that landed mid-poll must not be
    // resurrected from the pre-poll clone.
    let dropped: Vec<String> = next
        .iter()
        .filter(|(pane, rec)| rec.source == Source::Manual && !live.contains_key(*pane))
        .map(|(pane, _)| pane.clone())
        .collect();
    for pane in dropped {
        next.remove(&pane);
    }
}

/// Log a recurring-condition message once per distinct text (poll-frequency
/// callers must warn, not spam). `None` re-arms the key.
pub(crate) fn note_sticky(key: &'static str, msg: Option<String>) {
    use std::sync::{Mutex, OnceLock};
    static LAST: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    let mut last = LAST
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    match msg {
        Some(m) => {
            if last.get(key) != Some(&m) {
                eprintln!("terhubd: {m}");
                last.insert(key.to_string(), m);
            }
        }
        None => {
            last.remove(key);
        }
    }
}

/// Report an automation load failure through the sticky logger: a corrupt
/// file warns once, a clean load re-arms.
pub(crate) fn note_rules_error(err: Option<&anyhow::Error>) {
    note_sticky("automation", err.map(|e| format!("automation.toml: {e:#}")));
}

/// RAII handle that clears the automation in-flight flag on drop, so a
/// cancelled or panicking task doesn't permanently disable rule firing.
pub(crate) struct AutomationGuard(pub(crate) Arc<AtomicBool>);

impl Drop for AutomationGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Evaluate automation rules against the current registry and carry out
/// firings. The poll loop detaches this with an in-flight guard instead of
/// awaiting it, so a slow shell action delays automation, never polls —
/// while passes still never overlap and guardrails can't double-fire.
pub(crate) async fn run_automation(registry: &Arc<Mutex<HashMap<String, AgentRecord>>>) {
    let rules = match crate::automation::load_rules() {
        Ok(r) => {
            note_rules_error(None);
            r
        }
        Err(e) => {
            note_rules_error(Some(&e));
            return;
        }
    };
    if !rules.iter().any(|r| r.enabled) {
        return;
    }
    let reg = lock_registry(registry).clone();
    let _ = tokio::task::spawn_blocking(move || {
        let ledger = history::ledger_path();
        // Fail closed: an unreadable ledger must not read as "nothing
        // has fired". The fires *are* the guardrail state — losing them
        // resets every cooldown and cap, so a rule would fire again
        // immediately, and again on every tick after that.
        let fires = match history::read_rule_fires_checked(&ledger) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("terhubd: ledger unreadable, skipping automation pass ({e})");
                return;
            }
        };
        let now = now_unix();
        let tmux = Tmux::new().ok();
        for (fire, rec) in crate::automation::evaluate(&rules, &reg, &fires, now) {
            let desc = match crate::automation::execute(tmux.as_ref(), &fire, &rec) {
                Ok(d) => d,
                Err(e) => format!("FAILED: {e:?}"),
            };
            // The action has already run, so a failed record is not a
            // no-op: without it the cooldown never engages and the cap
            // never increments, and the rule fires again on the next
            // tick. Stop this pass rather than run it again.
            if let Err(e) = history::append_entry(
                &ledger,
                &history::Entry::rule_fire(&fire.rule, &rec, &desc, now),
            ) {
                eprintln!("terhubd: could not record rule fire ({}): {e}", fire.rule);
                return;
            }
        }
    })
    .await;
}

/// Wall-clock age for expiry checks. A backward jump (NTP step,
/// suspend/resume, VM restore) leaves pre-jump stamps ahead of `now`:
/// the elapsed real time is unknown, so expiry fails toward fresh
/// evidence (expired) instead of wedging blind until the clock catches
/// up. Stuck-age measurement deliberately stays saturating elsewhere —
/// an alarm must never fire on unknown elapsed.
pub(crate) fn elapsed_for_expiry(now: u64, stamp: u64) -> u64 {
    if now < stamp {
        u64::MAX // clock jumped backwards: treat the deadline as met
    } else {
        now - stamp
    }
}

/// Split the previous record into (authoritative, expired). A hook record
/// nobody refreshed recently has lapsed: screen classification takes over,
/// but the expired record still seeds state continuity so a pane that kept
/// its state doesn't renotify.
pub(crate) fn split_prev(
    rec: Option<&AgentRecord>,
    now: u64,
    stale_secs: u64,
) -> (Option<&AgentRecord>, Option<&AgentRecord>) {
    match rec {
        Some(r)
            if r.source == Source::Hook && elapsed_for_expiry(now, r.updated_at) > stale_secs =>
        {
            (None, Some(r))
        }
        other => (other, None),
    }
}

/// One poll pass (blocking tmux calls; runs on a blocking thread).
/// Carry last-known state only while it has recent evidence behind it.
/// Screen records on harness panes are ghosts by construction (screen
/// classification never runs there) and drop immediately instead of
/// lingering out their 30 minutes.
pub(crate) fn carry_record(old: &AgentRecord, pane: &Pane, now: u64, stale_secs: u64) -> bool {
    if old.source == Source::Screen && is_harness_command(&pane.command) {
        return false;
    }
    elapsed_for_expiry(now, old.updated_at) <= stale_secs
}

/// Advance the stuck clock for `ticks` observed under `agent`: an
/// unchanged sum from the same process keeps the old still-time, anything
/// else (moved sum, restarted agent, new agent, recycled pid) restarts
/// it. Never trips on first sight. A zero `start` on either side means
/// unknown (legacy caller, unparsable stat, pre-upgrade registry) and
/// skips the identity check rather than forcing a restart.
pub(crate) fn track_cpu(
    o: &AgentRecord,
    agent: &str,
    ticks: u64,
    start: u64,
    now: u64,
) -> (u64, u64) {
    let same_proc = o.agent_start == 0 || start == 0 || o.agent_start == start;
    if o.agent.eq_ignore_ascii_case(agent)
        && o.cpu_ticks == ticks
        && o.cpu_still_since > 0
        && same_proc
    {
        (ticks, o.cpu_still_since)
    } else {
        (ticks, now)
    }
}

/// Strip animation noise from a capture before hashing: footer-carousel
/// lines (`Tip: …`, full-text rotation), braille spinner frames (⠙⠋),
/// block-element meters (█▆▄), and digit runs (elapsed counters, token
/// rates, relative times). Without this a spinner redraw restarts the
/// quiet clock every poll and Idle pegs near zero forever. Digit-only
/// semantic steps ("1 failed" → "2 failed") hide from the hash too — the
/// summary comparison in [`track_quiet_merged`] catches the ones the
/// classifier surfaces.
pub(crate) fn denoise_screen(s: &str) -> String {
    let s = crate::agent::strip_tip_lines(s);
    let mut out = String::with_capacity(s.len());
    let mut in_digits = false;
    for c in s.chars() {
        if ('\u{2800}'..='\u{28ff}').contains(&c) || ('\u{2580}'..='\u{259f}').contains(&c) {
            in_digits = false;
        } else if c.is_ascii_digit() {
            if !in_digits {
                out.push('#');
                in_digits = true;
            }
        } else {
            out.push(c);
            in_digits = false;
        }
    }
    out
}

/// Deterministic screen-change hash, stable across restarts so quiet
/// clocks survive daemon upgrades. Hashes the denoised capture, so
/// spinner frames and counter ticks don't count as screen changes.
/// (One break: hashes from before the denoise upgrade never match, so
/// every quiet clock restarts once on the first post-upgrade poll.)
pub(crate) fn screen_hash(s: &str) -> u64 {
    crate::fnv1a(&denoise_screen(s))
}

/// Advance the visible-quiet clock: same screen hash keeps the old
/// still-time, a new hash restarts it, and no capture (`None`) preserves
/// the clock rather than resetting it on a transient failure.
pub(crate) fn track_quiet(prev: (u64, u64), hash: Option<u64>, now: u64) -> (u64, u64) {
    match hash {
        None => prev,
        Some(h) if h == prev.0 => prev,
        Some(h) => (h, now),
    }
}

/// Merge-post quiet update: a screen change restarts the clock, and so
/// does a changed non-empty summary (it catches digit-only steps like
/// "1 failed" → "2 failed" that denoising hides from the hash). A wiped
/// summary is decay, not activity, and stuck rows are exempt outright —
/// their "no cpu for {dur}" ticker would otherwise read fresh forever.
pub(crate) fn track_quiet_merged(
    prev: (u64, u64),
    hash: Option<u64>,
    carried_summary: &str,
    new_summary: &str,
    state: AgentState,
    now: u64,
) -> (u64, u64) {
    let (h, q) = track_quiet(prev, hash, now);
    if state != AgentState::Stuck && !new_summary.is_empty() && new_summary != carried_summary {
        (h, now)
    } else {
        (h, q)
    }
}

/// Post-merge quiet update: the poller's verbatim step. A screen change
/// restarts the clock (via [`track_quiet_merged`]), and so does entering
/// Working — the state flip itself is activity, and spinner frames are
/// denoised out of the hash, so without this a pane that just started
/// working keeps showing the previous stillness in the Idle column until
/// its pixels visibly move. Demotions into Idle keep the old clock —
/// that stillness is the point.
pub(crate) fn refresh_quiet(
    carried: Option<&AgentRecord>,
    hash: Option<u64>,
    rec: &AgentRecord,
    now: u64,
) -> (u64, u64) {
    let prev_quiet = carried
        .map(|r| (r.screen_hash, r.quiet_since))
        .unwrap_or((0, 0));
    // Pre-fallback summaries: the fill below derives from the same
    // screen the hash already judged, so only merge verdicts count.
    let carried_summary = carried.map(|r| r.summary.as_str()).unwrap_or("");
    let (h, mut q) = track_quiet_merged(
        prev_quiet,
        hash,
        carried_summary,
        &rec.summary,
        rec.state,
        now,
    );
    let fresh_work = carried
        .map(|r| r.state != AgentState::Working || r.agent != rec.agent)
        .unwrap_or(true);
    if rec.state == AgentState::Working && fresh_work {
        q = now;
    }
    (h, q)
}

/// Inputs to the shared stuck gate: a live process (`has_proc`) burning
/// no CPU (`still`) for `stuck_secs` on a Working|Idle row is hung —
/// unless the screen has been quiet past `quiet_idle_secs` (finished,
/// not hung) or the gate is disabled (`stuck_secs = 0`).
/// `prev_stuck_since` carries the clock across polls while the trip
/// persists (no renotify); None starts it at now. Stillness is measured
/// with saturating age on purpose: after a backward clock jump a
/// future-dated `still` reads as zero elapsed (no trip) rather than
/// expired — an alarm must never fire on unknown elapsed; see
/// `elapsed_for_expiry`, which applies the opposite (fail-toward-fresh)
/// direction only where a safe fallback exists.
pub(crate) struct StuckGate {
    has_proc: bool,
    still: u64,
    quiet_for: u64,
    now: u64,
    stuck_secs: u64,
    quiet_idle_secs: u64,
    prev_stuck_since: Option<u64>,
}

/// The stuck gate, shared by [`merge_record_owned`] and the Hook|Manual
/// carry branch. Returns the Stuck since-clock and summary on trip.
pub(crate) fn stuck_gate_trip(state: AgentState, g: &StuckGate) -> Option<(u64, String)> {
    if !matches!(state, AgentState::Working | AgentState::Idle) {
        return None;
    }
    if g.stuck_secs == 0 || !g.has_proc || g.still == 0 {
        return None;
    }
    if g.now.saturating_sub(g.still) < g.stuck_secs {
        return None;
    }
    // A long-quiet screen is finished, not hung (the `samai` fix).
    if g.quiet_idle_secs > 0 && g.quiet_for >= g.quiet_idle_secs {
        return None;
    }
    Some((
        g.prev_stuck_since.unwrap_or(g.now),
        format!(
            "no cpu for {}",
            crate::history::fmt_dur(g.now.saturating_sub(g.still))
        ),
    ))
}

/// Stuck evaluation for a Hook|Manual-carried record (post CPU/quiet
/// refresh). Hook authority means state carries forward instead of being
/// re-derived, so the trip saves the hook's verdict in `stuck_base`: a
/// still-still CPU keeps Stuck without renotifying, and recovered CPU
/// restores the verdict (clocking the flip, blanking the trip summary so
/// the screen fallback refills it). A hook-claimed Stuck (no trip base)
/// is never touched.
pub(crate) fn apply_hook_stuck_gate(
    r: &mut AgentRecord,
    has_proc: bool,
    now: u64,
    stuck_secs: u64,
    quiet_idle_secs: u64,
) {
    let underlying = r.stuck_base.unwrap_or(r.state);
    let quiet_for = if r.quiet_since > 0 {
        now.saturating_sub(r.quiet_since)
    } else {
        0
    };
    let prev_since = r.stuck_base.map(|_| r.state_since);
    if let Some((since, sum)) = stuck_gate_trip(
        underlying,
        &StuckGate {
            has_proc,
            still: r.cpu_still_since,
            quiet_for,
            now,
            stuck_secs,
            quiet_idle_secs,
            prev_stuck_since: prev_since,
        },
    ) {
        if r.stuck_base.is_none() {
            r.stuck_base = Some(underlying);
        }
        r.state = AgentState::Stuck;
        r.state_since = since;
        r.summary = sum;
    } else if r.stuck_base.is_some() {
        r.state = underlying;
        r.stuck_base = None;
        r.state_since = now;
        r.summary = String::new();
    }
}

/// Reset-each-pass bell evidence: mirror this poll's tmux bell flag onto
/// the record, never carry it. The flag is actionable only when the
/// classifier's bell_override fired — a manifest hit upgraded to Waiting
/// with the `🔔 ` summary prefix. A bare bell with no hit is shell noise
/// (dropped by bell_override), so the record stays clear. The reason is
/// the matched waiting pattern with the prefix stripped, so the chip can
/// render `🔔 y/n` instead of just `🔔`.
pub(crate) fn bell_evidence(hit: Option<&ScreenHit>, pane_bell: bool) -> (bool, String) {
    if !pane_bell {
        return (false, String::new());
    }
    match hit {
        Some(h) => match h.summary.strip_prefix("🔔 ") {
            Some(reason) if !reason.is_empty() => (true, reason.to_string()),
            // Override fired on an empty match line: still a real bell.
            Some(_) => (true, "bell".to_string()),
            // No prefix: bell_override declined (e.g. an Error/Stuck row
            // keeps its verdict), so there is no bell evidence to record.
            None => (false, String::new()),
        },
        None => (false, String::new()),
    }
}

/// Merge one poll's evidence into a record, legacy entry: no snapshot in
/// scope here, so editor ownership falls back to the old rule (editor
/// foreground plus any process hit) and the starttime is unknown (0).
/// Test-only: the poller calls [`merge_record_owned`] with the real
/// subtree check and starttime instead.
#[cfg(test)]
pub(crate) fn merge_record(
    carried: Option<&AgentRecord>,
    lapsed: bool,
    screen: Option<ScreenHit>,
    proc_hit: Option<(&str, u64)>,
    pane: &Pane,
    now: u64,
    stuck_secs: u64,
) -> Option<AgentRecord> {
    merge_record_owned(
        carried,
        lapsed,
        screen,
        proc_hit.map(|(pa, t)| (pa, t, 0)),
        pane,
        is_editor_command(&pane.command),
        now,
        stuck_secs,
        QUIET_IDLE_SECS,
        WAITING_IDLE_SECS,
        DONE_IDLE_SECS,
    )
}

/// Merge one poll's evidence into a record. Identity priority: process tree
/// beats screen guess (a `pi` binary is a fact; screen text is a hint).
/// State priority: screen waiting/error/limited/done beats everything here (fresh hooks
/// never reach this function); a live agent with an unreadable screen is
/// working; a working agent with a long-still CPU sum is stuck. Anonymous
/// screen states (patterns, no name) only survive paired with a process
/// hit — otherwise a micro `>` bar would impersonate an agent. Returns
/// None when nothing claims the pane.
///
/// `has_editor` is the poller's verdict that the foreground editor sits
/// inside this agent's own subtree — a background agent never claims a
/// sibling editor. The proc triple is (agent, subtree ticks, starttime).
#[allow(clippy::too_many_arguments)] // one verdict per evidence source; a struct would churn 25 test sites
pub(crate) fn merge_record_owned(
    carried: Option<&AgentRecord>,
    lapsed: bool,
    screen: Option<ScreenHit>,
    proc_hit: Option<(&str, u64, u64)>,
    pane: &Pane,
    has_editor: bool,
    now: u64,
    stuck_secs: u64,
    quiet_idle_secs: u64,
    waiting_idle_secs: u64,
    done_idle_secs: u64,
) -> Option<AgentRecord> {
    use AgentState::{Done, Error, Idle, Limited, Waiting, Working};
    let (agent, state, source) = match (proc_hit, screen.as_ref()) {
        // A done screen beats a live process: finished agents keep their
        // REPL open. (Stale banners can't pin this: freshest-line-wins
        // means live footer chrome below them classifies first.)
        (Some((pa, _, _)), Some(hit))
            if hit.state == Waiting
                || hit.state == Error
                || hit.state == Limited
                || hit.state == Idle
                || hit.state == Done =>
        {
            (pa.to_string(), hit.state, Source::Proc)
        }
        // Agent summoned $EDITOR: the foreground program awaits keystrokes —
        // but only its own editor, never a sibling's.
        (Some((pa, _, _)), _) if is_editor_command(&pane.command) && has_editor => {
            (pa.to_string(), Waiting, Source::Proc)
        }
        (Some((pa, _, _)), _) => (pa.to_string(), Working, Source::Proc),
        (None, Some(hit)) => {
            let sa = hit.agent.as_ref()?;
            (sa.clone(), hit.state, Source::Screen)
        }
        (None, None) => return None,
    };
    // A lapsed hook record still seeds continuity: same state keeps its
    // clock (no renotify) but drops its summary — hook text from 30+
    // minutes ago is rot, not information.
    let (mut since, mut summary) = match carried {
        Some(o) if o.state == state && o.agent.eq_ignore_ascii_case(&agent) => (
            o.state_since,
            if lapsed {
                String::new()
            } else {
                o.summary.clone()
            },
        ),
        _ => (now, String::new()),
    };
    // Waiting/error/limited rows show the prompt (or error) line that
    // tripped the classifier — the text you must read to act. Idle and
    // Done rows show the completion line, for the same reason. Refreshed
    // every poll; the clock above still decides notifications. Working
    // rows stay summary-free: spinner text is noise, not information.
    if matches!(state, Waiting | Error | Limited | Idle | Done)
        && let Some(hit) = screen.as_ref()
        && !hit.summary.is_empty()
    {
        summary = hit.summary.clone();
    }
    // Working rows show the agent's published phase — the one line that
    // says what it is doing right now ("Goal · Verifying the result").
    // Spinner text is noise and a carried summary is rot, so neither
    // survives; the phase is re-derived from this poll's screen, so it
    // can never ride the carry from a state it no longer belongs to.
    if state == Working {
        summary = screen
            .as_ref()
            .and_then(|hit| hit.phase.clone())
            .unwrap_or_default();
    }
    let (ticks, still, astart) = match proc_hit {
        Some((pa, t, s)) => carried
            .map(|o| {
                let (ti, st) = track_cpu(o, pa, t, s, now);
                (ti, st, s)
            })
            .unwrap_or((t, now, s)),
        None => (0, 0, 0),
    };
    let mut final_state = state;
    // How long the screen has been frozen — the one signal that
    // separates a *hung* agent from a *finished* one. A hung agent's
    // UI loop dies with its spinner: the screen freezes *recently* and
    // the process stops burning CPU. A finished agent's screen has
    // been quiet for a long time (its REPL sits idle). Both read
    // "no CPU" — only the quiescence clock tells them apart. 0 means
    // "not tracked yet" (first poll), which the gate treats as
    // recently-active so a real hang is caught on the first tick.
    let quiet_for = carried
        .map(|o| o.quiet_since)
        .filter(|q| *q > 0)
        .map(|q| now.saturating_sub(q))
        .unwrap_or(0);
    // Stuck is decided by CPU stillness on a live process: a leaf that
    // stopped making forward progress. It fires on Working and Idle
    // rows (a finished-looking idle row whose spinner just died is a
    // hang that finished), and `stuck_secs = 0` disables the gate
    // entirely. The quiescence guard is the discriminator: a screen
    // that has been frozen past `quiet_idle_secs` is finished, not
    // hung, so it must not be re-flagged stuck — this is the `samai`
    // fix. A screen that froze *recently* (spinner just stopped) with
    // no CPU is the genuine hang, and the gate still catches it. The
    // same gate (shared helper) runs on Hook|Manual carries, which
    // otherwise never evaluate Stuck.
    let prev_stuck_since = match carried {
        Some(o) if o.state == AgentState::Stuck && o.agent.eq_ignore_ascii_case(&agent) => {
            Some(o.state_since)
        }
        _ => None,
    };
    if let Some((stuck_since, stuck_summary)) = stuck_gate_trip(
        final_state,
        &StuckGate {
            has_proc: proc_hit.is_some(),
            still,
            quiet_for,
            now,
            stuck_secs,
            quiet_idle_secs,
            prev_stuck_since,
        },
    ) {
        final_state = AgentState::Stuck;
        since = stuck_since;
        summary = stuck_summary;
    }
    // Quiescence is evidence in its own right: a live process whose
    // screen has not moved is finished. The Stuck gate above already
    // declined to fire on a long-quiet screen, so this promotion is
    // what lands a finished pane on `Idle`. The order is deliberate:
    // gate first (so a recently-frozen hang escalates before the
    // demotion can mask it), then promotion.
    if final_state == Working && quiet_idle_secs > 0 && quiet_for >= quiet_idle_secs {
        final_state = Idle;
        since = match carried {
            Some(o) if o.state == Idle => o.state_since,
            _ => now,
        };
        // No summary here on purpose. The poller re-evaluates quiet with
        // track_quiet_merged right after this, and a summary *derived
        // from the quiet clock* reads as fresh activity and resets the
        // clock this branch just measured — the row flickers Idle for
        // exactly one poll and falls back to Working forever. The Idle
        // column already shows the duration; `fallback_summary_for`
        // fills the cell with the last screen line instead.
        summary = String::new();
    }
    // A completion that stopped being news is idle. The `done` verdict
    // comes from a banner that stays in the scrollback, so the classifier
    // re-matches it on every poll and the row would read `done` forever —
    // never counting towards the idle summary, and indistinguishable from
    // a mid-turn agent to anyone scanning the column. Once the screen has
    // been quiet past `done_idle_secs` the turn is over and the agent is
    // waiting for its next prompt: that is idle.
    //
    // The summary is kept, unlike the working flip above: it is the
    // completion line itself ("✓ Todo list 6/6 completed"), it is stable,
    // and being stable it does not feed the quiet clock. What decays is
    // the state label, not the information.
    //
    // The carried clock is preserved when the row was already idle, so the
    // demotion does not restart the idle duration on every poll — the
    // classifier re-derives `done` from the same unchanged screen, and a
    // `since = now` here would leave the row flickering between the two
    // labels forever.
    if final_state == Done && done_idle_secs > 0 && quiet_for >= done_idle_secs {
        final_state = Idle;
        since = match carried {
            Some(o) if o.state == Idle => o.state_since,
            _ => now,
        };
    }
    // A prompt nobody has touched for a long time is no longer urgent.
    // Agents whose CLI ends every turn with a bare `>` (gemini, and
    // anything with the same shape) classify as `waiting` forever once
    // the work is done, so without this they sit in "need you" long
    // after you walked away. Deliberately a much longer threshold than
    // the working flip: stepping away for a few minutes must not
    // silence a question. The summary is kept — it is the question you
    // would have come back to read — and being stable it does not feed
    // the quiet clock.
    if final_state == Waiting && waiting_idle_secs > 0 && quiet_for >= waiting_idle_secs {
        final_state = Idle;
        since = match carried {
            Some(o) if o.state == Idle => o.state_since,
            _ => now,
        };
    }
    let last_turn_secs = match carried {
        Some(o) if crate::agent::turn_ended(o.state, final_state) => {
            since.saturating_sub(o.state_since)
        }
        Some(o) => o.last_turn_secs,
        None => 0,
    };
    // Handles follow the agent, not the pane: a changed agent drops the
    // old name so ensure_handles mints a fresh one.
    let handle = match carried {
        Some(o) if o.agent == agent => o.handle.clone(),
        _ => String::new(),
    };
    // Model/ctx ride a same-agent carry (polls never report them); on
    // an agent change they belong to the *old* agent and are dropped,
    // like the handle and the pinned pid (a new agent owns new ones).
    let (model, ctx_pct, reg_pid) = match carried {
        Some(o) if o.agent == agent => (o.model.clone(), o.ctx_pct, o.reg_pid),
        _ => (String::new(), None, 0),
    };
    // Same rule for the last prompt (computed before `agent` moves).
    let (last_prompt_at, last_prompt) = match carried {
        Some(o) if o.agent == agent => (o.last_prompt_at, o.last_prompt.clone()),
        _ => (0, String::new()),
    };
    Some(AgentRecord {
        pane: pane.id.clone(),
        agent,
        state: final_state,
        source,
        summary,
        handle,
        model,
        ctx_pct,
        reg_pid,
        session: pane.session.clone(),
        window: pane.window_name.clone(),
        command: pane.command.clone(),
        updated_at: now,
        state_since: since,
        cpu_ticks: ticks,
        cpu_still_since: still,
        agent_start: astart,
        last_turn_secs,
        // Quiet is set by the poller post-merge (it owns the capture).
        screen_hash: 0,
        quiet_since: 0,
        // Polls never wipe hook prompts: carry the old cut forward —
        // but only for the same agent (see `last_prompt` above).
        last_prompt_at,
        last_prompt,
        // Bell evidence is re-derived every poll (see `bell_evidence`),
        // never carried: a bell that rang in an earlier poll must not
        // keep claiming the pane is ringing now. The poll overwrites
        // both fields with the fresh evidence right after the merge.
        bell: false,
        bell_reason: String::new(),
        // Merge re-derives state every poll: no trip base survives it.
        stuck_base: None,
    })
}

/// Summary fallback: the freshest non-blank screen line, so panes whose
/// agent was claimed by process (or whose hook lapsed) still say something
/// instead of showing an empty Summary cell. Hooks with real summaries and
/// screen-pattern hits keep theirs — this only fills blanks.
pub(crate) fn screen_fallback_summary(screen: Option<&str>) -> String {
    const MAX: usize = 80;
    let line = screen
        .unwrap_or("")
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .unwrap_or("");
    let mut out: String = line
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX)
        .collect();
    if line.chars().count() > MAX {
        out.push('…');
    }
    out
}

/// Screen-line fallback gated to states whose screen names actionable text
/// (a prompt, error, or completion line). Working rows stay summary-free —
/// spinner text is noise, not information — and Stuck carries its own
/// explanation from [`merge_record`], so neither borrows a screen line here.
pub(crate) fn fallback_summary_for(state: AgentState, screen: Option<&str>) -> String {
    if matches!(
        state,
        AgentState::Waiting
            | AgentState::Error
            | AgentState::Limited
            | AgentState::Idle
            | AgentState::Done
    ) {
        screen_fallback_summary(screen)
    } else {
        String::new()
    }
}

/// Outage result: the registry carries over untouched, and `outage` tells
/// the poll loop to skip session/foreground diffing — unknown is not
/// empty, and diffing it would log phantom open/close/focus events.
/// One poll pass: the next registry, session names, foreground pane,
/// outage flag, and the panes tmux listed (None when the listing
/// failed, so mid-poll ghost filtering stays off rather than dropping
/// a real event mid-outage).
pub(crate) type PollOutput = (
    HashMap<String, AgentRecord>,
    Vec<String>,
    Option<String>,
    bool,
    Option<std::collections::HashSet<String>>,
);

pub(crate) fn outage_result(prev: HashMap<String, AgentRecord>) -> PollOutput {
    // Outage: the pane listing failed, so the live set is unknown —
    // ghost filtering stays off rather than risk dropping a real event
    // mid-outage.
    (prev, Vec::new(), None, true, None)
}

/// Take the single-instance flock for `runtime` (non-blocking). The
/// returned file must be kept alive for the daemon's whole run; the
/// lock releases when it drops (exit, crash, or shutdown alike).
pub fn acquire_daemon_lock(runtime: &std::path::Path) -> Result<std::fs::File> {
    use std::os::unix::io::AsRawFd;
    let lock_path = runtime.join("terhubd.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        // Never truncate: the lock file only ever holds an flock, and
        // its contents are irrelevant (a previous holder may still
        // have written a pid).
        .truncate(false)
        // Close-on-exec: without it every process the daemon spawns
        // (notify-send, an automation rule, a tmux call) inherits the
        // lock fd and keeps the single-instance lock alive in its own
        // right — a child that outlives the daemon would block the next
        // start for as long as it lives. A forked child duplicates the
        // open-file-description, so drop() in the parent does NOT release
        // the lock while such a child lives; O_CLOEXEC ends that at
        // execve instead of at child exit.
        .custom_flags(libc::O_CLOEXEC)
        .open(&lock_path)
        .with_context(|| format!("open lock {}", lock_path.display()))?;
    // SAFETY: flock on a live fd; LOCK_NB so a second daemon fails
    // fast instead of queueing behind the first one's lifetime.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        anyhow::bail!(
            "another terhub daemon already holds {}",
            lock_path.display()
        );
    }
    Ok(file)
}

/// Returns the new registry, the live session list for open/close
/// detection, the foreground pane, and whether tmux was unreachable
/// (sessions/foreground unknown, not empty). Transitions are NOT computed
/// here: the poll loop merges mid-poll hooks first and diffs after, so any
/// diff here would be discarded and recomputed.
pub(crate) fn poll_sync(
    prev: HashMap<String, AgentRecord>,
    manifests: &[CompiledManifest],
    tune: Tunables,
) -> PollOutput {
    match Tmux::new() {
        // tmux gone: keep stale rather than wipe
        Ok(t) => poll_sync_with(&t, prev, manifests, tune),
        Err(_) => outage_result(prev),
    }
}

/// Serialises every test that forks a child, and the flock test below.
///
/// `flock` is per open-file-description, and `fork` duplicates the
/// description rather than the lock: a child forked while a test holds the
/// daemon lock keeps that lock alive until the child exits, even after the
/// parent has dropped its own fd. Taking the lock test's whole body and
/// every fork site under one guard keeps the two from interleaving, so the
/// "released on drop" arm measures drop semantics instead of whatever
/// sibling happened to fork next. (Rust opens files O_CLOEXEC, so the fd
/// is closed at execve — the window is fork→exec, but that window is
/// exactly where a fast script child still holds it.)
pub(crate) static FORK_GUARD: Mutex<()> = Mutex::new(());

pub(crate) fn fork_guard() -> std::sync::MutexGuard<'static, ()> {
    // Poison-tolerant on purpose: a panicking test must not wedge every
    // other test in the binary behind a lock nobody can take.
    FORK_GUARD.lock().unwrap_or_else(|e| e.into_inner())
}

/// The cheap per-pane facts a poll needs before it can classify
/// anything.
///
/// They are computed in one pass over the pane list so the screen
/// captures — the slow part, one `tmux capture-pane` child per pane —
/// can start before the classification loop that consumes them.
pub(crate) struct PanePrelude<'a> {
    /// The last known record for this pane, if it is still fresh.
    old: Option<&'a AgentRecord>,
    /// The last known record, but past its hook freshness window.
    stale: Option<&'a AgentRecord>,
    /// The live agent process tree under the pane, if any.
    proc_hit: Option<crate::proc::AgentHit>,
    /// The pane's agent process is stopped on purpose.
    paused: bool,
}

pub(crate) fn poll_sync_with(
    tmux: &Tmux,
    prev: HashMap<String, AgentRecord>,
    manifests: &[CompiledManifest],
    tune: Tunables,
) -> PollOutput {
    // Guarded: the fake tmux is a script this calls exec, and every fork
    // must not land inside a concurrent flock assertion. The capture
    // workers fork too, but they do it while this guard is still held.
    let _guard = fork_guard();
    let now = now_unix();
    // One process view shared by every pane: /proc is walked once per
    // poll, and the snapshot is reused for label resolution so we don't
    // walk it a second time inside list_all_panes.
    let snap = crate::proc::Snapshot::capture();
    let panes = match tmux.list_all_panes_with_snap(&snap) {
        Ok(p) => p,
        Err(_) => return outage_result(prev),
    };
    // One cheap pass over the panes first. The captures are what a poll
    // actually waits on, so they are all started together instead of
    // one after another — and this pass decides which panes deserve a
    // capture at all.
    let mut preludes: Vec<Option<PanePrelude<'_>>> = Vec::with_capacity(panes.len());
    let mut wanted: Vec<usize> = Vec::new();
    for (i, pane) in panes.iter().enumerate() {
        let (old, stale) = split_prev(prev.get(&pane.id), now, tune.stale_secs);
        let proc_hit = snap.detect_agent(pane.pid);
        // The pauser registry: a pane whose agent process is *stopped*
        // is paused, not hung. A stopped process burns no CPU, so the
        // Stuck gate would otherwise report terhub's own `z` as a dead
        // agent and escalate an alarm terhub caused itself.
        //
        // Read from the process state every poll, never carried: a
        // resumed pane is working again on the next tick, and a stale
        // "paused" mark could never expire. The pane root counts too —
        // a shell frozen with its agent still running is just as
        // deliberately stopped.
        let paused = proc_hit.as_ref().is_some_and(|h| h.stopped) || snap.is_stopped(pane.pid);
        // Skip idle shells nobody has claimed yet — unless a process hit
        // claims them (an agent behind a shell prompt is still an agent).
        if old.is_none() && proc_hit.is_none() && is_shell_command(&pane.command) {
            preludes.push(None);
            continue;
        }
        wanted.push(i);
        preludes.push(Some(PanePrelude {
            old,
            stale,
            proc_hit,
            paused,
        }));
    }
    // Every screen that matters, captured at once.
    let screens = crate::capture::capture_screens(tmux, &panes, &wanted);
    let mut next = HashMap::new();
    for (i, pane) in panes.iter().enumerate() {
        let Some(PanePrelude {
            old,
            stale,
            proc_hit,
            paused,
        }) = preludes[i].take()
        else {
            continue;
        };
        // One capture feeds classification and the visible-quiet clock.
        // Harness dashboards get hash-only: their screens name agents
        // without running them, but their pixels still go quiet.
        // A pane whose capture failed has no screen, exactly as before.
        let screen = screens.get(&pane.id).cloned();
        let hash = screen.as_ref().map(|s| screen_hash(s));
        // Hook and manual records stay authoritative; poll only refreshes
        // their metadata. The one exception is a hook whose agent is no
        // longer the pane's live agent: a process tree that names a
        // *different* agent is not a hint, it is the replacement (the
        // exit-pi/start-muse pane reuse). Letting the ended agent's last
        // hook claim the pane anyway would mask the live agent until the
        // hook stale window ran out. Manual pins are exempt: a human
        // named that agent precisely because detection missed it.
        // NOTE: updated_at is deliberately NOT bumped here: it measures hook
        // freshness (time since the last event), and refreshing it every poll
        // would make stale hooks live forever instead of lapsing. Manual
        // pins never lapse (split_prev keeps them), so the same hold applies.
        // Screen classification runs before the hook branch: a screen
        // that names a *different* agent is replacement evidence too
        // (custom/interpreter-launched agents the proc matcher cannot
        // see), not just a live process tree.
        let screen_hit = if is_harness_command(&pane.command) {
            None
        } else {
            screen.as_deref().and_then(|s| {
                classify_screen(
                    manifests,
                    &pane.command,
                    s,
                    pane.bell,
                    crate::proc::parked_in_tty_read(pane.pid),
                )
            })
        };
        // Bell evidence for this pass, derived once: every branch below
        // mirrors it fresh instead of carrying the old cut.
        let (bell_now, bell_reason_now) = bell_evidence(screen_hit.as_ref(), pane.bell);
        let replaced_by_live_agent = old.is_some_and(|rec| {
            rec.source == Source::Hook
                && (proc_hit
                    .as_ref()
                    .is_some_and(|hit| !hit.agent.eq_ignore_ascii_case(&rec.agent))
                    || screen_hit
                        .as_ref()
                        .and_then(|hit| hit.agent.as_ref())
                        .is_some_and(|agent| !agent.eq_ignore_ascii_case(&rec.agent)))
        });
        if let Some(rec) = old
            && matches!(rec.source, Source::Hook | Source::Manual)
            && !replaced_by_live_agent
        {
            let mut r = rec.clone();
            // A hook `idle` is defeasible: pi's `agent_end` fires when
            // the main turn ends, but retry loops, detached audit
            // workers, and background subagents keep going after it
            // (the screen keeps showing a Working spinner, a Retrying
            // countdown, or RUNNING audit rows). If the live screen
            // proves same-agent work — or a needs-you verdict — the
            // hook's verdict is stale and the screen wins. Manual pins
            // are exempt: a human named that agent precisely because
            // detection missed it.
            //
            // Anonymous screen hits only count with a live process
            // behind them (otherwise a micro `>` bar would
            // impersonate an agent — the same rule the merge uses).
            // Hook `working` stays authoritative and is never demoted
            // here: `turn_start` means the user prompted, while
            // `agent_end` can fire early.
            if r.source == Source::Hook
                && r.state == AgentState::Idle
                && let Some(hit) = screen_hit.as_ref()
                && matches!(
                    hit.state,
                    AgentState::Working
                        | AgentState::Waiting
                        | AgentState::Error
                        | AgentState::Limited
                )
                && hit.agent.as_ref().is_none_or(|a| a.eq_ignore_ascii_case(&r.agent))
                && (hit.agent.as_ref().is_some_and(|a| a.eq_ignore_ascii_case(&r.agent))
                    || proc_hit
                        .as_ref()
                        .is_some_and(|h| h.agent.eq_ignore_ascii_case(&r.agent)))
            {
                r.state = hit.state;
                r.state_since = now;
                r.stuck_base = None;
                r.summary = if hit.state == AgentState::Working {
                    hit.phase.clone().unwrap_or_default()
                } else {
                    hit.summary.clone()
                };
            }
            // A pane that was paused and is running again must not
            // inherit the freeze's silence: the still-clock measured
            // the pause, not the agent's work.
            clear_pause_silence(&mut r, paused);
            r.session.clone_from(&pane.session);
            r.window.clone_from(&pane.window_name);
            r.command.clone_from(&pane.command);
            // CPU tracking runs through the hook's life (state untouched) so
            // stuck evaluation is seamless when the hook lapses — and the
            // shared stuck gate runs here too, so a hung hook-claimed pane
            // reads Stuck instead of Working-until-lapse.
            let has_proc = match &proc_hit {
                Some(h) => {
                    let t = snap.subtree_ticks(h.pid);
                    (r.cpu_ticks, r.cpu_still_since) = track_cpu(&r, &h.agent, t, h.start, now);
                    r.agent_start = h.start;
                    true
                }
                // No detection hit, but the pin names a process: track CPU
                // under it so stuck detection works for pinned agents.
                None if r.source == Source::Manual && r.reg_pid != 0 => {
                    let Some(start) = snap.proc_start(r.reg_pid) else {
                        // Pinned process gone: the pin drops even though
                        // the pane is alive.
                        continue;
                    };
                    let agent = r.agent.clone();
                    let t = snap.subtree_ticks(r.reg_pid);
                    (r.cpu_ticks, r.cpu_still_since) = track_cpu(&r, &agent, t, start, now);
                    r.agent_start = start;
                    true
                }
                None => {
                    r.cpu_ticks = 0;
                    r.cpu_still_since = 0;
                    r.agent_start = 0;
                    false
                }
            };
            (r.screen_hash, r.quiet_since) = track_quiet((r.screen_hash, r.quiet_since), hash, now);
            // A stopped agent is paused, not hung: the stuck gate must
            // not fire on a tree that was frozen on purpose.
            if !paused {
                apply_hook_stuck_gate(&mut r, has_proc, now, tune.stuck_secs, tune.quiet_idle_secs);
            }
            // Bell is reset-each-pass: mirror this poll's flag, never carry.
            (r.bell, r.bell_reason) = (bell_now, bell_reason_now.clone());
            if r.summary.is_empty() {
                r.summary = fallback_summary_for(r.state, screen.as_deref());
            }
            // The hook branch carries the state (and its clock) forward,
            // so the pause clock is already in `r`.
            let prev_pause = carried_pause_since(Some(&r));
            apply_paused(&mut r, paused, now, prev_pause);
            next.insert(pane.id.clone(), r);
            continue;
        }
        let proc_pair = proc_hit
            .as_ref()
            .map(|h| (h.agent.as_str(), snap.subtree_ticks(h.pid), h.start));
        // The editor gate only fires when the editor sits inside this
        // agent's own subtree — a background agent never claims a sibling.
        let has_editor = is_editor_command(&pane.command)
            && proc_hit
                .as_ref()
                .is_some_and(|h| snap.subtree_has_editor(h.pid));
        let carried = old.or(stale);
        let lapsed = old.is_none() && stale.is_some();
        // The pause clock has to be read *before* the merge: the merge
        // re-derives the state and its clock from live evidence, and
        // the pause is applied on top of the result.
        let prev_pause = carried_pause_since(carried);
        // A pane that was paused and is running again must not inherit
        // the freeze's silence — the still-clock measured the pause, not
        // the agent's work, and the merge's stuck gate reads that clock.
        let mut carried_owned = carried.cloned();
        if let Some(c) = carried_owned.as_mut() {
            clear_pause_silence(c, paused);
        }
        let carried = carried_owned.as_ref();
        if let Some(mut rec) = merge_record_owned(
            carried,
            lapsed,
            screen_hit,
            proc_pair,
            pane,
            has_editor,
            now,
            tune.stuck_secs,
            tune.quiet_idle_secs,
            tune.waiting_idle_secs,
            tune.done_idle_secs,
        ) {
            (rec.screen_hash, rec.quiet_since) = refresh_quiet(carried, hash, &rec, now);
            // Bell is reset-each-pass: mirror this poll's flag, never carry.
            (rec.bell, rec.bell_reason) = (bell_now, bell_reason_now.clone());
            if rec.summary.is_empty() {
                rec.summary = fallback_summary_for(rec.state, screen.as_deref());
            }
            // A stopped agent is paused, not hung. This runs last: the
            // merge's own stuck gate may have said "stuck" (a stopped
            // process burns no CPU), and the pause is the better
            // explanation — so it overwrites the state, the clock and
            // the summary.
            apply_paused(&mut rec, paused, now, prev_pause);
            next.insert(pane.id.clone(), rec);
        } else if let Some(o) = old {
            // Pane alive but nothing matches: keep last known state only
            // while it has recent evidence behind it, else drop it rather
            // than show a state nobody can confirm anymore. (updated_at is
            // evidence time, deliberately not refreshed by the poll. The CPU
            // trackers reset: this poll found no agent process.)
            if carry_record(o, pane, now, tune.stale_secs) {
                let mut r = o.clone();
                r.session.clone_from(&pane.session);
                r.window.clone_from(&pane.window_name);
                r.command.clone_from(&pane.command);
                r.cpu_ticks = 0;
                r.cpu_still_since = 0;
                // Quiet is screen evidence, not agent evidence: keep the
                // clock running through the carry.
                (r.screen_hash, r.quiet_since) =
                    track_quiet((r.screen_hash, r.quiet_since), hash, now);
                // Bell is reset-each-pass: mirror this poll's flag, never carry.
                (r.bell, r.bell_reason) = (bell_now, bell_reason_now.clone());
                if r.summary.is_empty() {
                    r.summary = fallback_summary_for(r.state, screen.as_deref());
                }
                // Same rule as everywhere else: a stopped pane is
                // paused, not "working". The carried state is a memory
                // of a tree that is currently frozen.
                let prev_pause = carried_pause_since(Some(&r));
                apply_paused(&mut r, paused, now, prev_pause);
                next.insert(pane.id.clone(), r);
            }
        }
    }
    let mut sessions: Vec<String> = panes.iter().map(|p| p.session.clone()).collect();
    sessions.sort();
    sessions.dedup();
    let foreground = tmux.foreground_pane();
    let live_panes = panes.iter().map(|p| p.id.clone()).collect();
    (next, sessions, foreground, false, Some(live_panes))
}
