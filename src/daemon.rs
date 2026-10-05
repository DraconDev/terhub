//! terhubd: Unix-socket daemon holding the agent registry.
//!
//! The daemon merges three signals per pane: authoritative hook events pushed
//! by agents (`terhub hook ...`), process-tree agent identification, and
//! screen-pattern classification of hook-less panes from periodic
//! `capture-pane` polls. Clients talk newline-delimited JSON over the
//! socket; see [`Request`].
use crate::agent::{
    AgentRecord, AgentState, CompiledManifest, HookEvent, ScreenHit, Source, apply_event_to,
    builtin_manifests, classify_screen, compile_all_lossy, is_harness_command, is_shell_command,
    load_manifests, now_unix,
};
use crate::attend::{detect_all_transitions, notify_transition};
use crate::config;
use crate::history;
use crate::tmux::{Pane, Tmux};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader as TokioBufReader};
use tokio::net::{UnixListener, UnixStream as TokioUnixStream};

pub const POLL_SECS: u64 = 2;

/// Hook records lapse back to screen classification after 30min of silence.
pub const HOOK_STALE_SECS: u64 = 1800;

/// A working agent whose process tree burns no CPU for this long is stuck,
/// not busy.
pub const STUCK_SECS: u64 = 600;

/// Daemon timing knobs from the `[daemon]` config section (restart to
/// apply). `stuck_secs = 0` disables stuck detection entirely.
#[derive(Debug, Clone, Copy)]
pub struct Tunables {
    pub poll_secs: u64,
    pub stuck_secs: u64,
    pub stale_secs: u64,
}

impl Default for Tunables {
    fn default() -> Self {
        Tunables {
            poll_secs: POLL_SECS,
            stuck_secs: STUCK_SECS,
            stale_secs: HOOK_STALE_SECS,
        }
    }
}

impl Tunables {
    pub fn from_config(cfg: &config::AppConfig) -> Self {
        Tunables {
            poll_secs: cfg.poll_secs.max(1),
            stuck_secs: cfg.stuck_secs,
            stale_secs: cfg.stale_secs.max(1),
        }
    }
}

/// Runtime dir: explicit override (tests) → XDG runtime → per-user /tmp.
/// Empty variables are ignored so a blank `export` can't redirect us to `/`.
pub fn runtime_dir() -> PathBuf {
    if let Ok(d) = std::env::var("TERHUB_RUNTIME_DIR")
        && !d.is_empty()
    {
        return PathBuf::from(d);
    }
    if let Ok(d) = std::env::var("XDG_RUNTIME_DIR")
        && !d.is_empty()
    {
        return PathBuf::from(d).join("terhub");
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "shared".to_string());
    std::env::temp_dir().join(format!("terhub-{user}"))
}

pub fn socket_path() -> PathBuf {
    runtime_dir().join("terhubd.sock")
}

pub fn pid_path() -> PathBuf {
    runtime_dir().join("terhubd.pid")
}

/// Daemon stderr goes here (manifest warnings, bind errors, panics).
pub fn log_path() -> PathBuf {
    runtime_dir().join("terhubd.log")
}

/// Lock the registry, recovering the data when a panicked holder poisoned it.
fn lock_registry(
    registry: &Mutex<HashMap<String, AgentRecord>>,
) -> std::sync::MutexGuard<'_, HashMap<String, AgentRecord>> {
    registry.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "lowercase")]
pub enum Request {
    Ping,
    Snapshot,
    Event { event: HookEvent },
    Shutdown,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<Snapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    fn ok() -> Self {
        Response {
            ok: true,
            snapshot: None,
            error: None,
        }
    }

    fn err(msg: String) -> Self {
        Response {
            ok: false,
            snapshot: None,
            error: Some(msg),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Snapshot {
    #[serde(default)]
    pub agents: Vec<AgentRecord>,
    #[serde(default)]
    pub updated_at: u64,
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

pub struct Server {
    runtime: PathBuf,
    registry: Arc<Mutex<HashMap<String, AgentRecord>>>,
    manifests: Arc<Vec<CompiledManifest>>,
    shutdown: Arc<AtomicBool>,
    tunables: Tunables,
}

impl Server {
    pub fn load(runtime: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&runtime)
            .with_context(|| format!("create runtime dir {}", runtime.display()))?;
        let mut manifests = builtin_manifests();
        manifests.extend(load_manifests(&config::manifests_dir()));
        let (compiled, warnings) = compile_all_lossy(&manifests);
        for w in &warnings {
            eprintln!("terhubd: broken manifest skipped ({w})");
        }
        let registry = load_registry(&runtime.join("registry.json"));
        history::prune(&history::ledger_path(), now_unix(), 90);
        let tunables = Tunables::from_config(&config::load_config(None));
        Ok(Server {
            runtime,
            registry: Arc::new(Mutex::new(registry)),
            manifests: Arc::new(compiled),
            shutdown: Arc::new(AtomicBool::new(false)),
            tunables,
        })
    }

    pub async fn run(&self) -> Result<()> {
        let sock = self.runtime.join("terhubd.sock");
        let pid_file = self.runtime.join("terhubd.pid");
        if std::os::unix::net::UnixStream::connect(&sock).is_ok() {
            anyhow::bail!("a daemon is already listening on {}", sock.display());
        }
        let _ = std::fs::remove_file(&sock); // stale socket from a crash
        std::fs::write(&pid_file, std::process::id().to_string())?;
        let listener =
            UnixListener::bind(&sock).with_context(|| format!("bind {}", sock.display()))?;

        let reg = self.registry.clone();
        let manifests = self.manifests.clone();
        let runtime = self.runtime.clone();
        let tunables = self.tunables;
        tokio::spawn(async move { poll_loop(reg, manifests, runtime, tunables).await });

        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                break;
            }
            let accepted =
                tokio::time::timeout(Duration::from_millis(500), listener.accept()).await;
            let (stream, _) = match accepted {
                Ok(Ok(s)) => s,
                _ => continue,
            };
            let reg = self.registry.clone();
            let runtime = self.runtime.clone();
            let shutdown = self.shutdown.clone();
            tokio::spawn(async move { handle_conn(stream, reg, runtime, shutdown).await });
        }

        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&pid_file);
        Ok(())
    }
}

async fn handle_conn(
    stream: TokioUnixStream,
    registry: Arc<Mutex<HashMap<String, AgentRecord>>>,
    runtime: PathBuf,
    shutdown: Arc<AtomicBool>,
) {
    let (reader, mut writer) = stream.into_split();
    let mut lines = TokioBufReader::new(reader).lines();
    let line = match tokio::time::timeout(Duration::from_secs(10), lines.next_line()).await {
        Ok(Ok(Some(l))) => l,
        _ => return,
    };
    let req: Request = match serde_json::from_str(&line) {
        Ok(r) => r,
        Err(e) => {
            let _ = write_response(&mut writer, &Response::err(format!("bad request: {e}"))).await;
            return;
        }
    };
    let resp = match req {
        Request::Ping => Response::ok(),
        Request::Shutdown => {
            shutdown.store(true, Ordering::SeqCst);
            Response::ok()
        }
        Request::Snapshot => {
            let agents = {
                let reg = lock_registry(&registry);
                let mut v: Vec<AgentRecord> = reg.values().cloned().collect();
                v.sort_by(|a, b| a.pane.cmp(&b.pane));
                v
            };
            Response {
                ok: true,
                snapshot: Some(Snapshot {
                    agents,
                    updated_at: now_unix(),
                }),
                error: None,
            }
        }
        Request::Event { event } => {
            {
                let mut reg = lock_registry(&registry);
                let before = reg.clone();
                let now = now_unix();
                apply_event_to(&mut reg, &event, now);
                save_registry(&runtime.join("registry.json"), &reg);
                for t in detect_all_transitions(&before, &reg) {
                    log_entry(&history::Entry::transition(now, &t));
                    if t.to.needs_attention() {
                        notify_transition(&t);
                    }
                }
            }
            Response::ok()
        }
    };
    let _ = write_response(&mut writer, &resp).await;
}

async fn write_response(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    resp: &Response,
) -> Result<()> {
    let mut line = serde_json::to_string(resp)?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    Ok(())
}

async fn poll_loop(
    registry: Arc<Mutex<HashMap<String, AgentRecord>>>,
    manifests: Arc<Vec<CompiledManifest>>,
    runtime: PathBuf,
    tunables: Tunables,
) {
    // A zero interval would panic; config validation keeps this >= 1, and
    // the clamp makes direct construction safe too.
    let mut tick = tokio::time::interval(Duration::from_secs(tunables.poll_secs.max(1)));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut prev_sessions: Option<std::collections::HashSet<String>> = None;
    let mut prev_foreground: Option<Option<String>> = None;
    let automation_busy = Arc::new(AtomicBool::new(false));
    loop {
        tick.tick().await;
        let poll_start = now_unix();
        let prev = lock_registry(&registry).clone();
        let manifests = manifests.clone();
        let tune = tunables;
        let next = tokio::task::spawn_blocking(move || poll_sync(prev, &manifests, tune)).await;
        match next {
            Ok((mut next, _, sessions, foreground)) => {
                // Fold the poll result back under one lock hold: hooks that
                // landed during the poll are in the live registry but were
                // never in the pre-poll clone poll_sync saw, so merging them
                // here (and diffing against live, not the clone) is what
                // keeps the write-back from silently discarding them.
                let transitions = {
                    let mut reg = lock_registry(&registry);
                    merge_mid_poll_hooks(&mut next, &reg, poll_start);
                    let t = detect_all_transitions(&reg, &next);
                    *reg = next;
                    save_registry(&runtime.join("registry.json"), &reg);
                    t
                };
                let now = now_unix();
                for t in &transitions {
                    log_entry(&history::Entry::transition(now, t));
                    if t.to.needs_attention() {
                        notify_transition(t);
                    }
                }
                let sessions: std::collections::HashSet<String> = sessions.into_iter().collect();
                if let Some(prev) = prev_sessions.replace(sessions.clone()) {
                    for opened in sessions.difference(&prev) {
                        log_entry(&history::Entry::session_event(now, opened, true));
                    }
                    for closed in prev.difference(&sessions) {
                        log_entry(&history::Entry::session_event(now, closed, false));
                    }
                }
                if let Some(prev) = prev_foreground.replace(foreground.clone())
                    && prev != foreground
                {
                    log_entry(&history::Entry::human_focus(now, foreground.as_deref()));
                }
                // Automation runs detached so a slow shell action can't stall
                // polls; the in-flight guard means passes never overlap, so
                // guardrails still can't double-fire (a tick that finds a
                // pass running just skips — the running pass covers it).
                if !automation_busy.swap(true, Ordering::SeqCst) {
                    let reg = Arc::clone(&registry);
                    let busy = Arc::clone(&automation_busy);
                    tokio::spawn(async move {
                        run_automation(&reg).await;
                        busy.store(false, Ordering::SeqCst);
                    });
                }
            }
            Err(_) => break, // runtime shutting down
        }
    }
}

fn log_entry(entry: &history::Entry) {
    let _ = history::append_entry(&history::ledger_path(), entry);
}

/// Fold hook events that landed in the live registry *during* a poll back
/// into the poll result. `poll_sync` only saw the pre-poll clone, so a blind
/// write-back would silently discard them (lost update); the hook handler
/// already logged their transitions, so these merge silently. Only hook
/// records at least as fresh as `poll_start` (the second the clone was
/// taken) merge — older hooks were already carried or lapsed by the poll
/// itself and must not be resurrected.
fn merge_mid_poll_hooks(
    next: &mut HashMap<String, AgentRecord>,
    live: &HashMap<String, AgentRecord>,
    poll_start: u64,
) {
    for (pane, rec) in live {
        if rec.source == Source::Hook && rec.updated_at >= poll_start {
            next.insert(pane.clone(), rec.clone());
        }
    }
}

/// Report an automation load failure once per distinct message (the poll
/// calls every 2s; a corrupt file must warn, not spam). A clean load
/// re-arms the warning.
fn note_rules_error(err: Option<&anyhow::Error>) {
    use std::sync::{Mutex, OnceLock};
    static LAST: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    let mut last = LAST
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    match err {
        Some(e) => {
            let msg = format!("{e:#}");
            if last.as_ref() != Some(&msg) {
                eprintln!("terhubd: automation.toml: {msg}");
                *last = Some(msg);
            }
        }
        None => *last = None,
    }
}

/// Evaluate automation rules against the current registry and carry out
/// firings. The poll loop detaches this with an in-flight guard instead of
/// awaiting it, so a slow shell action delays automation, never polls —
/// while passes still never overlap and guardrails can't double-fire.
async fn run_automation(registry: &Arc<Mutex<HashMap<String, AgentRecord>>>) {
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
        let fires = history::read_rule_fires(&ledger);
        let now = now_unix();
        let tmux = Tmux::new().ok();
        for (fire, rec) in crate::automation::evaluate(&rules, &reg, &fires, now) {
            let desc = match crate::automation::execute(tmux.as_ref(), &fire, &rec) {
                Ok(d) => d,
                Err(e) => format!("FAILED: {e:?}"),
            };
            let _ = history::append_entry(
                &ledger,
                &history::Entry::rule_fire(&fire.rule, &rec, &desc, now),
            );
        }
    })
    .await;
}

/// Split the previous record into (authoritative, expired). A hook record
/// nobody refreshed recently has lapsed: screen classification takes over,
/// but the expired record still seeds state continuity so a pane that kept
/// its state doesn't renotify.
fn split_prev(
    rec: Option<&AgentRecord>,
    now: u64,
    stale_secs: u64,
) -> (Option<&AgentRecord>, Option<&AgentRecord>) {
    match rec {
        Some(r) if r.source == Source::Hook && now.saturating_sub(r.updated_at) > stale_secs => {
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
fn carry_record(old: &AgentRecord, pane: &Pane, now: u64, stale_secs: u64) -> bool {
    if old.source == Source::Screen && is_harness_command(&pane.command) {
        return false;
    }
    now.saturating_sub(old.updated_at) <= stale_secs
}

/// Advance the stuck clock for `ticks` observed under `agent`: an
/// unchanged sum keeps the old still-time, anything else (moved sum,
/// restarted agent, new agent) restarts it. Never trips on first sight.
fn track_cpu(o: &AgentRecord, agent: &str, ticks: u64, now: u64) -> (u64, u64) {
    if o.agent.eq_ignore_ascii_case(agent) && o.cpu_ticks == ticks && o.cpu_still_since > 0 {
        (ticks, o.cpu_still_since)
    } else {
        (ticks, now)
    }
}

/// Deterministic screen-change hash, stable across restarts so quiet
/// clocks survive daemon upgrades.
fn screen_hash(s: &str) -> u64 {
    crate::fnv1a(s)
}

/// Advance the visible-quiet clock: same screen hash keeps the old
/// still-time, a new hash restarts it, and no capture (`None`) preserves
/// the clock rather than resetting it on a transient failure.
fn track_quiet(prev: (u64, u64), hash: Option<u64>, now: u64) -> (u64, u64) {
    match hash {
        None => prev,
        Some(h) if h == prev.0 => prev,
        Some(h) => (h, now),
    }
}

/// Terminal editors an agent summons via $EDITOR: with agent context in
/// the tree, an editor in the foreground means awaiting input, not working.
/// Plain user editing never matches — the rule needs a process hit.
const EDITOR_COMMANDS: &[&str] = &["micro", "vim", "nvim", "vi", "nano", "emacs", "helix", "hx"];

fn is_editor_command(command: &str) -> bool {
    EDITOR_COMMANDS.contains(&command.to_lowercase().as_str())
}

/// Merge one poll's evidence into a record. Identity priority: process tree
/// beats screen guess (a `pi` binary is a fact; screen text is a hint).
/// State priority: screen waiting/error/limited beats everything here (fresh hooks
/// never reach this function); a live agent with an unreadable screen is
/// working; a working agent with a long-still CPU sum is stuck. Anonymous
/// screen states (patterns, no name) only survive paired with a process
/// hit — otherwise a micro `>` bar would impersonate an agent. Returns
/// None when nothing claims the pane.
fn merge_record(
    carried: Option<&AgentRecord>,
    lapsed: bool,
    screen: Option<ScreenHit>,
    proc_hit: Option<(&str, u64)>,
    pane: &Pane,
    now: u64,
    stuck_secs: u64,
) -> Option<AgentRecord> {
    use AgentState::{Error, Limited, Waiting, Working};
    let (agent, state, source) = match (proc_hit, screen.as_ref()) {
        (Some((pa, _)), Some(hit))
            if hit.state == Waiting || hit.state == Error || hit.state == Limited =>
        {
            (pa.to_string(), hit.state, Source::Proc)
        }
        // Agent summoned $EDITOR: the foreground program awaits keystrokes.
        (Some((pa, _)), _) if is_editor_command(&pane.command) => {
            (pa.to_string(), Waiting, Source::Proc)
        }
        (Some((pa, _)), _) => (pa.to_string(), Working, Source::Proc),
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
        Some(o) if o.state == state && o.agent == agent => (
            o.state_since,
            if lapsed {
                String::new()
            } else {
                o.summary.clone()
            },
        ),
        _ => (now, String::new()),
    };
    // Waiting/error rows show the prompt (or error) line that tripped the
    // classifier — the text you must read to act. Refreshed every poll;
    // the clock above still decides notifications. Working rows stay
    // summary-free: spinner text is noise, not information.
    if matches!(state, Waiting | Error | Limited)
        && let Some(hit) = screen.as_ref()
        && !hit.summary.is_empty()
    {
        summary = hit.summary.clone();
    }
    // Working rows stay summary-free even across carries: a prompt line
    // borrowed while waiting is rot once the agent works again (and
    // pre-gate summaries must not ride the carry forever).
    if state == Working {
        summary = String::new();
    }
    let (ticks, still) = match proc_hit {
        Some((pa, t)) => carried
            .map(|o| track_cpu(o, pa, t, now))
            .unwrap_or((t, now)),
        None => (0, 0),
    };
    let mut final_state = state;
    // stuck_secs = 0 disables stuck detection (the user's off switch).
    if final_state == Working
        && stuck_secs > 0
        && proc_hit.is_some()
        && still > 0
        && now.saturating_sub(still) >= stuck_secs
    {
        final_state = AgentState::Stuck;
        since = match carried {
            Some(o) if o.state == AgentState::Stuck && o.agent == agent => o.state_since,
            _ => now,
        };
        summary = format!(
            "no cpu for {}",
            crate::history::fmt_dur(now.saturating_sub(still))
        );
    }
    let last_turn_secs = match carried {
        Some(o) if crate::agent::turn_ended(o.state, final_state) => {
            since.saturating_sub(o.state_since)
        }
        Some(o) => o.last_turn_secs,
        None => 0,
    };
    Some(AgentRecord {
        pane: pane.id.clone(),
        agent,
        state: final_state,
        source,
        summary,
        session: pane.session.clone(),
        window: pane.window_name.clone(),
        command: pane.command.clone(),
        updated_at: now,
        state_since: since,
        cpu_ticks: ticks,
        cpu_still_since: still,
        last_turn_secs,
        // Quiet is set by the poller post-merge (it owns the capture).
        screen_hash: 0,
        quiet_since: 0,
    })
}

/// Summary fallback: the freshest non-blank screen line, so panes whose
/// agent was claimed by process (or whose hook lapsed) still say something
/// instead of showing an empty Summary cell. Hooks with real summaries and
/// screen-pattern hits keep theirs — this only fills blanks.
fn screen_fallback_summary(screen: Option<&str>) -> String {
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
/// (a prompt or error line). Working rows stay summary-free — spinner text
/// is noise, not information — and Stuck carries its own explanation from
/// [`merge_record`], so neither borrows a screen line here.
fn fallback_summary_for(state: AgentState, screen: Option<&str>) -> String {
    if matches!(
        state,
        AgentState::Waiting | AgentState::Error | AgentState::Limited
    ) {
        screen_fallback_summary(screen)
    } else {
        String::new()
    }
}

/// Returns the new registry, attention transitions to notify, the live
/// session list for open/close detection, and the foreground pane.
fn poll_sync(
    prev: HashMap<String, AgentRecord>,
    manifests: &[CompiledManifest],
    tune: Tunables,
) -> (
    HashMap<String, AgentRecord>,
    Vec<crate::attend::Transition>,
    Vec<String>,
    Option<String>,
) {
    let prev_session_list: Vec<String> = prev
        .values()
        .map(|r| r.session.clone())
        .filter(|s| !s.is_empty())
        .collect();
    let now = now_unix();
    let tmux = match Tmux::new() {
        // tmux gone: keep stale rather than wipe
        Ok(t) => t,
        Err(_) => return (prev, Vec::new(), prev_session_list, None),
    };
    let panes = match tmux.list_all_panes() {
        Ok(p) => p,
        Err(_) => return (prev, Vec::new(), prev_session_list, None),
    };
    // One process view shared by every pane: /proc is walked once per poll.
    let snap = crate::proc::Snapshot::capture();
    let mut next = HashMap::new();
    for pane in &panes {
        let (old, stale) = split_prev(prev.get(&pane.id), now, tune.stale_secs);
        let proc_hit = snap.detect_agent(pane.pid);
        // Skip idle shells nobody has claimed yet — unless a process hit
        // claims them (an agent behind a shell prompt is still an agent).
        if old.is_none() && proc_hit.is_none() && is_shell_command(&pane.command) {
            continue;
        }
        // One capture feeds classification and the visible-quiet clock.
        // Harness dashboards get hash-only: their screens name agents
        // without running them, but their pixels still go quiet.
        let screen = tmux.capture_pane(&pane.id, 30).ok();
        let hash = screen.as_ref().map(|s| screen_hash(s));
        // Hook records stay authoritative; poll only refreshes their metadata.
        // NOTE: updated_at is deliberately NOT bumped here: it measures hook
        // freshness (time since the last event), and refreshing it every poll
        // would make stale hooks live forever instead of lapsing.
        if let Some(rec) = old
            && rec.source == Source::Hook
        {
            let mut r = rec.clone();
            r.session.clone_from(&pane.session);
            r.window.clone_from(&pane.window_name);
            r.command.clone_from(&pane.command);
            // CPU tracking runs through the hook's life (state untouched) so
            // stuck evaluation is seamless when the hook lapses.
            match &proc_hit {
                Some(h) => {
                    let t = snap.subtree_ticks(h.pid);
                    (r.cpu_ticks, r.cpu_still_since) = track_cpu(&r, &h.agent, t, now);
                }
                None => {
                    r.cpu_ticks = 0;
                    r.cpu_still_since = 0;
                }
            }
            (r.screen_hash, r.quiet_since) = track_quiet((r.screen_hash, r.quiet_since), hash, now);
            if r.summary.is_empty() {
                r.summary = fallback_summary_for(r.state, screen.as_deref());
            }
            next.insert(pane.id.clone(), r);
            continue;
        }
        let screen_hit = if is_harness_command(&pane.command) {
            None
        } else {
            screen
                .as_deref()
                .and_then(|s| classify_screen(manifests, &pane.command, s))
        };
        let proc_pair = proc_hit
            .as_ref()
            .map(|h| (h.agent.as_str(), snap.subtree_ticks(h.pid)));
        let carried = old.or(stale);
        let lapsed = old.is_none() && stale.is_some();
        if let Some(mut rec) = merge_record(
            carried,
            lapsed,
            screen_hit,
            proc_pair,
            pane,
            now,
            tune.stuck_secs,
        ) {
            let prev_quiet = carried
                .map(|r| (r.screen_hash, r.quiet_since))
                .unwrap_or((0, 0));
            (rec.screen_hash, rec.quiet_since) = track_quiet(prev_quiet, hash, now);
            if rec.summary.is_empty() {
                rec.summary = fallback_summary_for(rec.state, screen.as_deref());
            }
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
                if r.summary.is_empty() {
                    r.summary = fallback_summary_for(r.state, screen.as_deref());
                }
                next.insert(pane.id.clone(), r);
            }
        }
    }
    let transitions = detect_all_transitions(&prev, &next);
    let mut sessions: Vec<String> = panes.iter().map(|p| p.session.clone()).collect();
    sessions.sort();
    sessions.dedup();
    let foreground = tmux.foreground_pane();
    (next, transitions, sessions, foreground)
}

fn load_registry(path: &Path) -> HashMap<String, AgentRecord> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return HashMap::new(),
    };
    let agents: Vec<AgentRecord> = match serde_json::from_str(&text) {
        Ok(a) => a,
        Err(_) => return HashMap::new(),
    };
    agents.into_iter().map(|a| (a.pane.clone(), a)).collect()
}

fn save_registry(path: &Path, registry: &HashMap<String, AgentRecord>) {
    let agents: Vec<&AgentRecord> = registry.values().collect();
    if let Ok(text) = serde_json::to_string_pretty(&agents) {
        let _ = std::fs::write(path, text);
    }
}

// ---------------------------------------------------------------------------
// Sync client (CLI + TUI)
// ---------------------------------------------------------------------------

pub fn send_request(req: &Request, timeout: Duration) -> Result<Response> {
    let sock = socket_path();
    let mut stream =
        UnixStream::connect(&sock).with_context(|| format!("connect {}", sock.display()))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    stream.flush()?;
    let mut reader = BufReader::new(stream);
    let mut resp = String::new();
    reader.read_line(&mut resp)?;
    serde_json::from_str(&resp).context("bad daemon response")
}

pub fn fetch_snapshot(timeout: Duration) -> Result<Snapshot> {
    let resp = send_request(&Request::Snapshot, timeout)?;
    resp.snapshot.context("daemon returned no snapshot")
}

pub fn send_event(ev: &HookEvent, timeout: Duration) -> Result<()> {
    let resp = send_request(&Request::Event { event: ev.clone() }, timeout)?;
    if resp.ok {
        Ok(())
    } else {
        anyhow::bail!(
            resp.error
                .unwrap_or_else(|| "daemon rejected event".to_string())
        )
    }
}

pub fn daemon_alive() -> bool {
    send_request(&Request::Ping, Duration::from_millis(300))
        .map(|r| r.ok)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentState;

    fn hook_record(updated_at: u64) -> AgentRecord {
        AgentRecord {
            pane: "%1".to_string(),
            agent: "claude".to_string(),
            state: AgentState::Waiting,
            source: Source::Hook,
            summary: String::new(),
            session: "s".to_string(),
            window: "w".to_string(),
            command: "claude".to_string(),
            updated_at,
            state_since: updated_at,
            cpu_ticks: 0,
            cpu_still_since: 0,
            last_turn_secs: 0,
            screen_hash: 0,
            quiet_since: 0,
        }
    }

    #[test]
    fn quiet_clock_keeps_still_restarts_change() {
        let h = screen_hash("same screen");
        // Still screen keeps the old still-time.
        assert_eq!(track_quiet((h, 100), Some(h), 200), (h, 100));
        // Changed screen restarts the clock.
        let h2 = screen_hash("new output");
        assert_ne!(h, h2);
        assert_eq!(track_quiet((h, 100), Some(h2), 200), (h2, 200));
        // Failed capture preserves the clock, never resets it.
        assert_eq!(track_quiet((h, 100), None, 200), (h, 100));
        // First sighting starts the clock now.
        assert_eq!(track_quiet((0, 0), Some(h), 200), (h, 200));
    }

    #[test]
    fn fresh_hook_stays_authoritative() {
        let now = 10_000;
        let rec = hook_record(now - 60);
        let (old, stale) = split_prev(Some(&rec), now, HOOK_STALE_SECS);
        assert!(old.is_some());
        assert!(stale.is_none());
    }

    #[test]
    fn lapsed_hook_expires_but_seeds_continuity() {
        let now = 10_000;
        let rec = hook_record(now - HOOK_STALE_SECS - 1);
        let (old, stale) = split_prev(Some(&rec), now, HOOK_STALE_SECS);
        assert!(old.is_none());
        assert!(stale.is_some());
    }

    #[test]
    fn screen_records_pass_through() {
        let now = 10_000;
        let mut rec = hook_record(0);
        rec.source = Source::Screen;
        let (old, stale) = split_prev(Some(&rec), now, HOOK_STALE_SECS);
        assert!(old.is_some());
        assert!(stale.is_none());
    }

    #[test]
    fn mid_poll_hooks_survive_the_write_back() {
        let poll_start = 50_000;
        // The poll classified %1 from its stale pre-poll clone...
        let mut next = HashMap::new();
        next.insert(
            "%1".to_string(),
            carried("claude", AgentState::Working, 0, 0, poll_start - 100),
        );
        // ...but a hook event landed mid-poll (live registry only).
        let mut live = HashMap::new();
        let mut fresh = hook_record(poll_start + 1);
        fresh.summary = "Approve plan?".to_string();
        live.insert("%1".to_string(), fresh);
        // An older hook the poll already saw must not be resurrected.
        let mut old_hook = hook_record(poll_start - 10);
        old_hook.pane = "%2".to_string();
        live.insert("%2".to_string(), old_hook);

        merge_mid_poll_hooks(&mut next, &live, poll_start);

        assert_eq!(next["%1"].state, AgentState::Waiting);
        assert_eq!(next["%1"].summary, "Approve plan?");
        assert!(!next.contains_key("%2"));
    }

    #[test]
    fn fallback_summary_takes_freshest_line() {
        assert_eq!(screen_fallback_summary(Some("old\nfresh  \n  \n")), "fresh");
        assert_eq!(screen_fallback_summary(None), "");
        assert_eq!(screen_fallback_summary(Some("  \n\t\n")), "");
        // Long lines truncate with an ellipsis, control chars wash out.
        let long = "x".repeat(100);
        let out = screen_fallback_summary(Some(&format!("a\tb\n{long}")));
        assert_eq!(out.chars().count(), 81);
        assert!(out.ends_with('…'), "{out}");
        assert_eq!(screen_fallback_summary(Some("a\tb")), "a b");
    }

    #[test]
    fn fallback_summary_skips_working_rows() {
        use crate::agent::AgentState::{Error, Limited, Stuck, Waiting, Working};
        let screen = Some("● Chrome Bridge (indefinite)");
        // Working rows stay summary-free: spinner text is noise.
        assert_eq!(fallback_summary_for(Working, screen), "");
        // Stuck carries its own explanation from merge_record.
        assert_eq!(fallback_summary_for(Stuck, screen), "");
        // Actionable states borrow the freshest screen line.
        assert_eq!(
            fallback_summary_for(Waiting, screen),
            "● Chrome Bridge (indefinite)"
        );
        assert!(!fallback_summary_for(Error, screen).is_empty());
        assert!(!fallback_summary_for(Limited, screen).is_empty());
        assert_eq!(fallback_summary_for(Waiting, None), "");
    }

    fn carried(agent: &str, state: AgentState, ticks: u64, still: u64, since: u64) -> AgentRecord {
        AgentRecord {
            pane: "%1".to_string(),
            agent: agent.to_string(),
            state,
            source: Source::Proc,
            summary: String::new(),
            session: "a".to_string(),
            window: "w".to_string(),
            command: "pi".to_string(),
            updated_at: since,
            state_since: since,
            cpu_ticks: ticks,
            cpu_still_since: still,
            last_turn_secs: 0,
            screen_hash: 0,
            quiet_since: 0,
        }
    }

    fn test_pane() -> crate::tmux::Pane {
        crate::tmux::Pane::test("%1", "w", "pi")
    }

    fn screen_hit(agent: Option<&str>, state: AgentState, summary: &str) -> Option<ScreenHit> {
        Some(ScreenHit {
            agent: agent.map(str::to_string),
            state,
            summary: summary.to_string(),
        })
    }

    #[test]
    fn harness_screen_records_drop_as_ghosts() {
        let now = 100_000;
        let mut old = carried("claude", AgentState::Working, 0, 0, now - 60);
        old.source = Source::Screen;
        let mut harness = test_pane();
        harness.command = "muxa".to_string();
        assert!(!carry_record(&old, &harness, now, HOOK_STALE_SECS));
        // Same record on a normal pane carries while fresh …
        assert!(carry_record(&old, &test_pane(), now, HOOK_STALE_SECS));
        // … and drops once stale.
        assert!(!carry_record(
            &old,
            &test_pane(),
            now + HOOK_STALE_SECS + 1,
            HOOK_STALE_SECS
        ));
        // Proc records on harnesses still carry (the agent may return).
        let mut proc_old = carried("claude", AgentState::Working, 500, now - 60, now - 60);
        proc_old.source = Source::Proc;
        assert!(carry_record(&proc_old, &harness, now, HOOK_STALE_SECS));
    }

    #[test]
    fn proc_identity_beats_screen_guess() {
        // Screen says claude, process tree says pi: trust the tree.
        let now = 100_000;
        let rec = merge_record(
            None,
            false,
            screen_hit(Some("claude"), AgentState::Working, "⠋ Working…"),
            Some(("pi", 100)),
            &test_pane(),
            now,
            STUCK_SECS,
        )
        .expect("no record");
        assert_eq!(rec.agent, "pi");
        assert_eq!(rec.state, AgentState::Working);
        assert_eq!(rec.source, Source::Proc);
        assert_eq!((rec.cpu_ticks, rec.cpu_still_since), (100, now));
        // Working rows stay summary-free: spinner text is noise.
        assert!(rec.summary.is_empty());
    }

    #[test]
    fn screen_waiting_wins_state_proc_keeps_identity() {
        let now = 100_000;
        let rec = merge_record(
            None,
            false,
            screen_hit(Some("claude"), AgentState::Waiting, "Approve? [y/n]"),
            Some(("pi", 50)),
            &test_pane(),
            now,
            STUCK_SECS,
        )
        .expect("no record");
        assert_eq!(rec.agent, "pi");
        assert_eq!(rec.state, AgentState::Waiting);
        assert_eq!(rec.source, Source::Proc);
        assert_eq!(rec.summary, "Approve? [y/n]");
    }

    #[test]
    fn screen_limited_wins_state_proc_keeps_identity() {
        let now = 100_000;
        let rec = merge_record(
            None,
            false,
            screen_hit(
                Some("codex"),
                AgentState::Limited,
                "Error: rate limit reached",
            ),
            Some(("pi", 50)),
            &test_pane(),
            now,
            STUCK_SECS,
        )
        .expect("no record");
        assert_eq!(rec.agent, "pi");
        assert_eq!(rec.state, AgentState::Limited);
        assert_eq!(rec.source, Source::Proc);
        assert_eq!(rec.summary, "Error: rate limit reached");
        assert!(rec.state.needs_attention());
    }

    #[test]
    fn proc_without_screen_is_working() {
        // The %192 case: agent binary alive, screen unreadable.
        let now = 100_000;
        let rec = merge_record(
            None,
            false,
            None,
            Some(("pi", 50)),
            &test_pane(),
            now,
            STUCK_SECS,
        )
        .expect("no record");
        assert_eq!(rec.agent, "pi");
        assert_eq!(rec.state, AgentState::Working);
        assert_eq!(rec.source, Source::Proc);
    }

    #[test]
    fn screen_only_record_keeps_screen_source() {
        let now = 100_000;
        let rec = merge_record(
            None,
            false,
            screen_hit(
                Some("codex"),
                AgentState::Error,
                "Error: rate limit reached",
            ),
            None,
            &test_pane(),
            now,
            STUCK_SECS,
        )
        .expect("no record");
        assert_eq!(rec.agent, "codex");
        assert_eq!(rec.state, AgentState::Error);
        assert_eq!(rec.source, Source::Screen);
        assert_eq!((rec.cpu_ticks, rec.cpu_still_since), (0, 0));
        assert_eq!(rec.summary, "Error: rate limit reached");
    }

    #[test]
    fn nothing_claiming_returns_none() {
        assert!(merge_record(None, false, None, None, &test_pane(), 100_000, STUCK_SECS).is_none());
    }

    #[test]
    fn anonymous_waiting_pairs_with_proc_or_drops() {
        let now = 100_000;
        // Approve prompt, no name, live pi: that agent waiting.
        let rec = merge_record(
            None,
            false,
            screen_hit(None, AgentState::Waiting, "Save changes? confirm"),
            Some(("pi", 50)),
            &test_pane(),
            now,
            STUCK_SECS,
        )
        .expect("no record");
        assert_eq!(rec.agent, "pi");
        assert_eq!(rec.state, AgentState::Waiting);
        assert_eq!(rec.source, Source::Proc);
        // Same screen, no process: a micro bar impersonating nobody.
        assert!(
            merge_record(
                None,
                false,
                screen_hit(None, AgentState::Waiting, "Save changes? confirm"),
                None,
                &test_pane(),
                now,
                STUCK_SECS,
            )
            .is_none()
        );
    }

    #[test]
    fn editor_foreground_with_agent_means_waiting() {
        let now = 100_000;
        // pi summoned $EDITOR (micro foreground): awaiting input, not working.
        let mut ed = test_pane();
        ed.command = "micro".to_string();
        let rec = merge_record(None, false, None, Some(("pi", 50)), &ed, now, STUCK_SECS)
            .expect("no record");
        assert_eq!(rec.agent, "pi");
        assert_eq!(rec.state, AgentState::Waiting);
        assert_eq!(rec.source, Source::Proc);
        // Same editor, no agent anywhere: not our business.
        assert!(merge_record(None, false, None, None, &ed, now, STUCK_SECS).is_none());
        // Same agent, non-editor foreground: still working.
        let rec = merge_record(
            None,
            false,
            None,
            Some(("pi", 50)),
            &test_pane(),
            now,
            STUCK_SECS,
        )
        .expect("no record");
        assert_eq!(rec.state, AgentState::Working);
        // An error on screen still wins over the editor foreground.
        let rec = merge_record(
            None,
            false,
            screen_hit(None, AgentState::Error, "boom"),
            Some(("pi", 50)),
            &ed,
            now,
            STUCK_SECS,
        )
        .expect("no record");
        assert_eq!(rec.state, AgentState::Error);
    }

    #[test]
    fn working_carry_drops_stale_summary() {
        let now = 100_000;
        // A prompt line borrowed while waiting is rot once the agent
        // works again — and pre-gate summaries must not ride the carry.
        let mut old = carried("pi", AgentState::Working, 50, now - 10, now - 100);
        old.summary = "● Chrome Bridge (indefinite)".to_string();
        let rec = merge_record(
            Some(&old),
            false,
            None,
            Some(("pi", 60)),
            &test_pane(),
            now,
            STUCK_SECS,
        )
        .expect("no record");
        assert_eq!(rec.state, AgentState::Working);
        assert!(rec.summary.is_empty(), "carried rot: {}", rec.summary);
    }

    #[test]
    fn waiting_summary_tracks_prompt_text() {
        // Same waiting state, new prompt line: the clock stays (no
        // renotify) but the summary follows the fresh line.
        let now = 100_000;
        let mut old = carried("pi", AgentState::Waiting, 500, now - 60, now - 300);
        old.summary = "Approve? [y/n]".to_string();
        let rec = merge_record(
            Some(&old),
            false,
            screen_hit(Some("pi"), AgentState::Waiting, "❯ Try asking me instead"),
            Some(("pi", 500)),
            &test_pane(),
            now,
            STUCK_SECS,
        )
        .expect("no record");
        assert_eq!(rec.state, AgentState::Waiting);
        assert_eq!(rec.state_since, now - 300);
        assert_eq!(rec.summary, "❯ Try asking me instead");
    }

    #[test]
    fn completed_turn_records_duration() {
        let now = 100_000;
        let old = carried("pi", AgentState::Working, 500, now - 60, now - 300);
        let rec = merge_record(
            Some(&old),
            false,
            screen_hit(Some("pi"), AgentState::Waiting, "Approve? [y/n]"),
            Some(("pi", 500)),
            &test_pane(),
            now,
            STUCK_SECS,
        )
        .expect("no record");
        assert_eq!(rec.state, AgentState::Waiting);
        assert_eq!(rec.last_turn_secs, 300);
    }

    #[test]
    fn stuck_carries_turn_without_ending_it() {
        let now = 100_000;
        let mut old = carried("pi", AgentState::Working, 500, now - 900, now - 900);
        old.last_turn_secs = 120;
        let stuck = merge_record(
            Some(&old),
            false,
            None,
            Some(("pi", 500)),
            &test_pane(),
            now,
            STUCK_SECS,
        )
        .expect("no record");
        assert_eq!(stuck.state, AgentState::Stuck);
        assert_eq!(stuck.last_turn_secs, 120);
        // Stuck → waiting: the grinding episode never completed, so the
        // previous completed turn still stands.
        let waiting = merge_record(
            Some(&stuck),
            false,
            screen_hit(Some("pi"), AgentState::Waiting, "Approve? [y/n]"),
            Some(("pi", 500)),
            &test_pane(),
            now + 10,
            STUCK_SECS,
        )
        .expect("no record");
        assert_eq!(waiting.last_turn_secs, 120);
    }

    #[test]
    fn stuck_after_still_cpu() {
        let now = 100_000;
        let old = carried("pi", AgentState::Working, 500, now - 601, now - 601);
        let rec = merge_record(
            Some(&old),
            false,
            None,
            Some(("pi", 500)),
            &test_pane(),
            now,
            STUCK_SECS,
        )
        .expect("no record");
        assert_eq!(rec.state, AgentState::Stuck);
        assert_eq!(rec.state_since, now);
        assert_eq!(rec.summary, "no cpu for 10m");
        assert!(rec.state.needs_attention());
    }

    #[test]
    fn stuck_keeps_clock_while_still() {
        let now = 100_000;
        let old = carried("pi", AgentState::Stuck, 500, now - 660, now - 660);
        let rec = merge_record(
            Some(&old),
            false,
            None,
            Some(("pi", 500)),
            &test_pane(),
            now,
            STUCK_SECS,
        )
        .expect("no record");
        assert_eq!(rec.state, AgentState::Stuck);
        assert_eq!(rec.state_since, now - 660);
        assert_eq!(rec.summary, "no cpu for 11m");
    }

    #[test]
    fn stuck_disabled_when_zero() {
        let now = 100_000;
        let old = carried("pi", AgentState::Working, 500, now - 3600, now - 3600);
        let rec = merge_record(
            Some(&old),
            false,
            None,
            Some(("pi", 500)),
            &test_pane(),
            now,
            0,
        )
        .expect("no record");
        assert_eq!(rec.state, AgentState::Working);
    }

    #[test]
    fn custom_stuck_threshold_honored() {
        // 601s of stillness trips at 600 but not at 3600.
        let now = 100_000;
        let old = carried("pi", AgentState::Working, 500, now - 601, now - 601);
        let rec = merge_record(
            Some(&old),
            false,
            None,
            Some(("pi", 500)),
            &test_pane(),
            now,
            3600,
        )
        .expect("no record");
        assert_eq!(rec.state, AgentState::Working);
    }

    #[test]
    fn custom_stale_lapses_early() {
        let now = 10_000;
        let rec = hook_record(now - 61);
        let (old, stale) = split_prev(Some(&rec), now, 60);
        assert!(old.is_none());
        assert!(stale.is_some());
        // … but stays fresh under the default window.
        let (old, stale) = split_prev(Some(&rec), now, HOOK_STALE_SECS);
        assert!(old.is_some());
        assert!(stale.is_none());
    }

    #[test]
    fn tunables_clamp_zero_poll() {
        let cfg = config::AppConfig {
            poll_secs: 0,
            stale_secs: 0,
            ..config::AppConfig::default()
        };
        let tune = Tunables::from_config(&cfg);
        assert_eq!(tune.poll_secs, 1);
        assert_eq!(tune.stale_secs, 1);
        assert_eq!(tune.stuck_secs, STUCK_SECS);
    }

    #[test]
    fn cpu_movement_clears_stuck() {
        let now = 100_000;
        let old = carried("pi", AgentState::Stuck, 500, now - 900, now - 900);
        let rec = merge_record(
            Some(&old),
            false,
            None,
            Some(("pi", 750)),
            &test_pane(),
            now,
            STUCK_SECS,
        )
        .expect("no record");
        assert_eq!(rec.state, AgentState::Working);
        assert_eq!(rec.state_since, now);
        assert_eq!(rec.cpu_still_since, now);
    }

    #[test]
    fn restarted_agent_resets_clock() {
        // Shrunk sum = new process, not stillness: never stuck on sight.
        let now = 100_000;
        let old = carried("pi", AgentState::Working, 500, now - 900, now - 900);
        let rec = merge_record(
            Some(&old),
            false,
            None,
            Some(("pi", 100)),
            &test_pane(),
            now,
            STUCK_SECS,
        )
        .expect("no record");
        assert_eq!(rec.state, AgentState::Working);
        assert_eq!(rec.cpu_still_since, now);
    }

    #[test]
    fn waiting_clears_stuck_with_fresh_clock() {
        let now = 100_000;
        let old = carried("pi", AgentState::Stuck, 500, now - 900, now - 900);
        let rec = merge_record(
            Some(&old),
            false,
            screen_hit(Some("pi"), AgentState::Waiting, "Approve? [y/n]"),
            Some(("pi", 500)),
            &test_pane(),
            now,
            STUCK_SECS,
        )
        .expect("no record");
        assert_eq!(rec.state, AgentState::Waiting);
        assert_eq!(rec.state_since, now);
    }
}
