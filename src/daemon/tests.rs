//! The daemon's own tests.
//!
//! They live in their own file because they are the bulk of the
//! module: the implementation is worth reading on its own, and a test
//! file that sits under it hides how much of the file is test.
use super::*;
use crate::agent::AgentState;
use crate::agent::{ScreenHit, Source};

/// Process-wide counter for fake-tmux temp dirs. `std::process::id()`
/// alone made concurrent sibling tests share a dir, so a `screen.txt`
/// rewrite in one test could leak into another. The counter is the
/// established pattern (`NEXT_CFG` in `src/app.rs`); kept local to
/// this module so a cross-crate helper isn't introduced for a
/// test-only concern.
static NEXT_TEST_DIR: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[test]
fn a_versioned_request_carries_the_protocol_version() {
    // The wire shape is what an old client and a new daemon share:
    // the command stays where it was, the version rides along.
    let line = serde_json::to_string(&WireRequest::from(&Request::Snapshot)).unwrap();
    assert_eq!(line, r#"{"cmd":"snapshot","proto":1}"#);

    // A pre-versioned client sends the old shape: no proto at all.
    // That is how the daemon recognises a legacy client.
    let legacy: WireRequest = serde_json::from_str(r#"{"cmd":"snapshot"}"#).unwrap();
    assert_eq!(legacy.cmd, Cmd::Snapshot);
    assert_eq!(legacy.proto, None, "no proto field means a legacy client");

    // The old wire shapes still parse into the same commands.
    let reg: WireRequest = serde_json::from_str(
        r#"{"cmd":"register","pane":"%1","agent":"pi","pid":7,"state":"working"}"#,
    )
    .unwrap();
    assert_eq!(reg.proto, None);
    let req = Request::try_from(reg).unwrap();
    match req {
        Request::Register {
            pane,
            agent,
            pid,
            state,
        } => {
            assert_eq!(pane, "%1");
            assert_eq!(agent, "pi");
            assert_eq!(pid, 7);
            assert_eq!(state, AgentState::Working);
        }
        other => panic!("wrong command: {other:?}"),
    }

    // A command whose payload is missing is an error, not a panic.
    let broken: WireRequest = serde_json::from_str(r#"{"cmd":"unregister"}"#).unwrap();
    assert!(Request::try_from(broken).is_err());
}

#[test]
fn a_legacy_client_gets_states_it_can_decode() {
    // The whole point of the down-projection: a client that predates
    // the `paused` state cannot decode it, and one unknown variant
    // used to fail the whole snapshot — which the client then
    // reported as "daemon not running" while the daemon was fine.
    assert_eq!(legacy_state(AgentState::Paused), AgentState::Idle);
    // Every other state is one the legacy client already knows.
    for state in [
        AgentState::Unknown,
        AgentState::Working,
        AgentState::Waiting,
        AgentState::Error,
        AgentState::Done,
        AgentState::Idle,
        AgentState::Stuck,
        AgentState::Limited,
    ] {
        assert_eq!(legacy_state(state), state, "{state:?} must survive");
    }
}

/// A response the client can read: one agent in the given state.
fn response_with_state(state: &str) -> String {
    format!(
        r#"{{"ok":true,"snapshot":{{"agents":[{{"pane":"%1","agent":"pi","state":"{state}","source":"proc","updated_at":1,"state_since":1}}],"updated_at":2}},"proto":1,"build":"0.2.3+abc"}}"#
    )
}

#[test]
fn an_unknown_agent_state_keeps_the_pane_visible() {
    // A state this build has never heard of is future information,
    // not corruption: the pane stays listed, as unknown.
    let resp = decode_response(&response_with_state("hyperfocus")).unwrap();
    let agents = resp.snapshot.unwrap().agents;
    assert_eq!(agents.len(), 1, "the pane must survive an unknown state");
    assert_eq!(agents[0].state, AgentState::Unknown);
}

#[test]
fn an_unreadable_response_never_says_daemon_not_running() {
    // A daemon that answered must never be reported as absent: the
    // fix for a dead daemon (start one) is useless here.
    let raw = r#"{"ok":true,"snapshot":{"agents":[{"pane":"%1","agent":"pi","state":"working","source":"proc","updated_at":1,"state_since":1,"ctx_pct":"high"}],"updated_at":2},"proto":1,"build":"0.2.3+abc"}"#;
    let err = decode_response(raw).unwrap_err();
    let msg = err.message();
    assert!(
        !msg.contains("daemon not running"),
        "a live daemon must not be reported as absent: {msg}"
    );
    assert!(
        msg.contains("daemon running but the response could not be read"),
        "unreadable: {msg}"
    );
    assert!(
        !err.is_unreachable(),
        "only a missing daemon is unreachable"
    );
}

#[test]
fn a_version_mismatch_names_both_versions() {
    // The diagnostic has to say what to do, not just that something
    // is wrong: "run `make build`" is the whole fix.
    let raw =
        r#"{"ok":true,"snapshot":{"agents":[],"updated_at":2},"proto":99,"build":"9.9.9+zzz"}"#;
    let err = decode_response(raw).unwrap();
    assert!(err.ok);
    // A decodable response from another version is not an error, but
    // it is worth saying out loud.
    let resp = SnapshotResponse {
        snapshot: err.snapshot.unwrap(),
        daemon_proto: err.proto,
        daemon_build: err.build,
    };
    let note = compat_note(&resp).expect("a version mismatch must be reported");
    assert!(note.contains("client proto 1"), "{note}");
    assert!(note.contains("daemon proto 99"), "{note}");
    assert!(note.contains("make build"), "{note}");

    let build = build_note(&resp).expect("a build mismatch must be reported");
    assert!(build.contains("9.9.9+zzz"), "{build}");
}

#[test]
fn a_matching_daemon_reports_nothing() {
    let resp = SnapshotResponse {
        snapshot: Snapshot::default(),
        daemon_proto: Some(PROTO_VERSION),
        daemon_build: Some(build_label()),
    };
    assert!(compat_note(&resp).is_none());
    assert!(build_note(&resp).is_none());
}

#[test]
fn a_legacy_daemon_is_reported_as_legacy() {
    // No proto at all: the daemon predates the versioned protocol.
    // It still serves usable data, so this is a note, not a failure.
    let resp = SnapshotResponse {
        snapshot: Snapshot::default(),
        daemon_proto: None,
        daemon_build: None,
    };
    let note = compat_note(&resp).expect("a legacy daemon must be reported");
    assert!(note.contains("make build"), "{note}");
}

#[test]
fn respawn_backoff_gates_repeated_attempts() {
    // A daemon that cannot come up must not be re-forked every tick:
    // only the first attempt after a quiet period is due.
    let backoff = RESPAWN_BACKOFF.as_millis() as u64;
    assert!(respawn_due(1_000, 0), "never tried: first attempt is due");
    assert!(
        !respawn_due(1_000 + backoff - 1, 1_000),
        "inside the backoff: no re-fork"
    );
    assert!(
        respawn_due(1_000 + backoff, 1_000),
        "backoff elapsed: retry is due"
    );
    // A clock that jumped backwards (NTP, suspend) must not wedge
    // the self-heal off forever.
    assert!(respawn_due(500, 1_000), "backwards clock: retry is due");
}

#[test]
fn hook_event_applies_without_side_effects() {
    // The lock-held half of hook handling: mutate the registry and
    // report transitions. Ledger + notify emission is the caller's job
    // after unlocking, so a slow notify-send never stalls polls.
    let mut reg = std::collections::HashMap::new();
    let ev = crate::agent::HookEvent {
        pane: "%1".to_string(),
        agent: "pi".to_string(),
        state: AgentState::Waiting,
        summary: "Approve?".to_string(),
        prompt: String::new(),
        model: String::new(),
        ctx: None,
    };
    let transitions = apply_hook_event(&mut reg, &ev, 100_000);
    assert_eq!(reg["%1"].state, AgentState::Waiting);
    assert_eq!(reg["%1"].summary, "Approve?");
    assert_eq!(transitions.len(), 1);
    assert!(transitions[0].to.needs_attention());
}

#[test]
fn register_upserts_manual_record() {
    let mut reg = HashMap::new();
    apply_register(&mut reg, "%7", "myagent", 4242, AgentState::Working, 1000);
    let rec = &reg["%7"];
    assert_eq!(rec.source, Source::Manual);
    assert_eq!(rec.agent, "myagent");
    assert_eq!(rec.state, AgentState::Working);
    assert_eq!(rec.reg_pid, 4242);
    assert_eq!(rec.updated_at, 1000);
    assert_eq!(rec.state_since, 1000);
    // Same agent + state refreshes the pin in place: new pid wins,
    // the clock is kept (no renotify downstream).
    apply_register(&mut reg, "%7", "myagent", 5000, AgentState::Working, 2000);
    let rec = &reg["%7"];
    assert_eq!(rec.reg_pid, 5000);
    assert_eq!(rec.updated_at, 2000);
    assert_eq!(rec.state_since, 1000);
    // A new state is a fresh pin with a fresh clock.
    apply_register(&mut reg, "%7", "myagent", 5000, AgentState::Waiting, 3000);
    let rec = &reg["%7"];
    assert_eq!(rec.state, AgentState::Waiting);
    assert_eq!(rec.state_since, 3000);
}

#[test]
fn unregister_drops_only_manual() {
    let mut reg = HashMap::new();
    apply_register(&mut reg, "%7", "myagent", 0, AgentState::Working, 1000);
    reg.insert("%1".to_string(), hook_record(1000));
    assert!(apply_unregister(&mut reg, "%7"));
    assert!(!reg.contains_key("%7"));
    // A hook record is real evidence: unregister leaves it alone.
    assert!(!apply_unregister(&mut reg, "%1"));
    assert!(reg.contains_key("%1"));
    // Nothing pinned, nothing dropped.
    assert!(!apply_unregister(&mut reg, "%9"));
}

#[test]
fn manual_pin_never_lapses() {
    // split_prev must keep Manual pins on the authoritative side no
    // matter how old: only unregister (or pane/pid death) drops them.
    let mut rec = hook_record(100);
    rec.source = Source::Manual;
    rec.pane = "%7".to_string();
    let (old, stale) = split_prev(Some(&rec), 100_000, 1800);
    assert!(old.is_some());
    assert!(stale.is_none());
}

#[test]
fn merge_carries_model_and_ctx() {
    // Poll evidence never reports model/ctx: the merge carries the
    // old cut forward like the prompt, never blanks it.
    let mut old = carried("pi", AgentState::Working, 10, 90, 50);
    old.model = "opus-4.5".to_string();
    old.ctx_pct = Some(72);
    let rec = merge_record(
        Some(&old),
        false,
        None,
        Some(("pi", 10)),
        &test_pane(),
        100,
        300,
    )
    .expect("proc hit claims the pane");
    assert_eq!(rec.model, "opus-4.5");
    assert_eq!(rec.ctx_pct, Some(72));
    // Manual pins carry their pid through merges too.
    let mut old = carried("myagent", AgentState::Working, 10, 90, 50);
    old.source = Source::Manual;
    old.reg_pid = 4242;
    let rec = merge_record(
        Some(&old),
        false,
        None,
        Some(("myagent", 10)),
        &test_pane(),
        100,
        300,
    )
    .expect("proc hit claims the pane");
    assert_eq!(rec.reg_pid, 4242);
}

#[tokio::test]
async fn poll_loop_exits_on_shutdown() {
    // A preset shutdown flag stops the loop before the first tick: no
    // tmux, no registry writes, prompt return.
    let registry = Arc::new(Mutex::new(HashMap::new()));
    let shutdown = Arc::new(AtomicBool::new(true));
    let dir = std::env::temp_dir().join(format!("terhub-poll-shutdown-{}", std::process::id()));
    let guard = ensure_runtime_dir(&dir).unwrap();
    tokio::time::timeout(
        Duration::from_secs(5),
        poll_loop(
            registry,
            Arc::new(vec![]),
            dir,
            guard,
            Tunables::default(),
            shutdown,
        ),
    )
    .await
    .expect("poll_loop ignored shutdown");
}

#[tokio::test]
async fn failed_bind_leaves_no_pid_file() {
    // The pid file must be written only after a successful bind: a
    // concurrent-start loser that fails to bind must not clobber the
    // winner's pid with its own dead one. A >108-char socket path
    // fails the bind deterministically.
    let runtime = std::env::temp_dir().join(format!(
        "terhub-pid-order-{}-{}",
        std::process::id(),
        "p".repeat(100)
    ));
    std::fs::create_dir_all(&runtime).unwrap();
    let server = Server {
        runtime: runtime.clone(),
        runtime_guard: ensure_runtime_dir(&runtime).unwrap(),
        registry: Arc::new(Mutex::new(HashMap::new())),
        manifests: Arc::new(vec![]),
        shutdown: Arc::new(AtomicBool::new(false)),
        tunables: Tunables::default(),
    };
    server.run().await.expect_err("overlong socket bound?");
    assert!(
        !runtime.join("terhubd.pid").exists(),
        "loser wrote a pid file"
    );
    let _ = std::fs::remove_dir_all(&runtime);
}

#[test]
fn poll_sync_outage_flags_unknown() {
    // tmux unreachable: the registry carries over untouched, and the
    // outage flag tells the poll loop to skip session/foreground
    // diffing (no phantom open/close/focus events either direction).
    let mut prev = HashMap::new();
    prev.insert("%1".to_string(), hook_record(99_000));
    let tmux = crate::tmux::Tmux::test("/nonexistent-tmux-binary".into());
    let (next, sessions, foreground, outage, _) =
        poll_sync_with(&tmux, prev, &[], Tunables::default());
    assert!(outage);
    assert_eq!(next.len(), 1);
    assert_eq!(next["%1"].state, AgentState::Waiting);
    assert!(sessions.is_empty());
    assert_eq!(foreground, None);
}

/// Fake tmux listing a single `%50` pane running `sleep` (pid
/// substituted in): capture prints a bare prompt, everything else is
/// empty. Fields are separated like the real PANE_FORMAT.
fn fake_tmux_one_sleep_pane(dir: &std::path::Path, pid: u32) -> crate::tmux::Tmux {
    use std::os::unix::fs::PermissionsExt;
    let sep = crate::tmux::SEP;
    let line = [
        "%50", "@1", "w", "1", "0", "sleep", "/tmp", "1", "80", "24", "sess", "0",
    ]
    .join(sep);
    // The payload travels as a printf *argument*: the pane id's `%`
    // would otherwise be eaten as a format specifier.
    let payload = format!("{line}{sep}{pid}");
    let script = format!(
        "#!/bin/sh\ncase \"$1\" in\n  list-panes) printf '%s\\n' '{payload}' ;;\n  capture-pane) printf '$ \\n' ;;\n  *) printf '' ;;\nesac\n"
    );
    let path = dir.join("tmux");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    crate::tmux::Tmux::test(path)
}

fn manual_record(pane: &str, pid: u32, updated_at: u64) -> AgentRecord {
    let mut rec = hook_record(updated_at);
    rec.pane = pane.to_string();
    rec.agent = "myagent".to_string();
    rec.state = AgentState::Working;
    rec.source = Source::Manual;
    rec.reg_pid = pid;
    rec
}

#[test]
fn manual_pin_without_pid_survives_stale_window() {
    // A pid-less pin on a live pane is carried forever: staleness is
    // a hook concept (event freshness), pins persist by design.
    let dir = std::env::temp_dir().join(format!("terhub-poll-manual-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let tmux = fake_tmux_one_sleep_pane(&dir, 999_999_999);
    let now = now_unix();
    let mut prev = HashMap::new();
    prev.insert(
        "%50".to_string(),
        manual_record("%50", 0, now.saturating_sub(99_999)),
    );
    let (next, _, _, outage, _) = poll_sync_with(&tmux, prev, &[], Tunables::default());
    assert!(!outage);
    let rec = next.get("%50").expect("manual pin carried");
    assert_eq!(rec.source, Source::Manual);
    assert_eq!(rec.state, AgentState::Working);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn manual_pin_tracks_reg_pid_cpu() {
    // A live reg_pid feeds the CPU/stuck clocks like a proc hit; the
    // nonzero agent_start proves the pin path ran (the generic carry
    // leaves it zero).
    let mut child = {
        // Guarded: a forked child inherits every fd this process holds,
        // including a sibling test's daemon-lock fd (threads share one
        // fd table), and would hold that lock until its execve.
        let _guard = fork_guard();
        std::process::Command::new("sleep")
            .arg("30")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("spawn sleep")
    };
    let pid = child.id();
    let dir = std::env::temp_dir().join(format!("terhub-poll-manualpid-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let tmux = fake_tmux_one_sleep_pane(&dir, pid);
    let now = now_unix();
    let mut prev = HashMap::new();
    prev.insert("%50".to_string(), manual_record("%50", pid, now));
    let (next, _, _, _, _) = poll_sync_with(&tmux, prev, &[], Tunables::default());
    let rec = next.get("%50").expect("manual pin carried");
    assert!(rec.agent_start > 0, "reg_pid ticks not tracked");
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn live_muse_replaces_fresh_pi_hook_after_pane_reuse() {
    // Pane reuse, as seen live: Pi emitted its final `session ended`
    // hook, exited, and Muse started in the same pane seconds later.
    // The Pi hook is still inside its freshness window, but the live
    // process tree names Muse — the replacement must win immediately
    // instead of the pane reading `pi · idle · session ended` while
    // Muse is visibly thinking on screen.
    //
    // Per-test unique dir: a process-wide counter (see `NEXT_TEST_DIR`)
    // plus the pid — `std::process::id()` alone made concurrent
    // sibling tests share a dir, so a `screen.txt` rewrite in one
    // could leak into this test's `fake_tmux_file_screen` / `screen.txt`
    // reads. The audit's "1/25" flake was this class.
    let n = NEXT_TEST_DIR.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir =
        std::env::temp_dir().join(format!("terhub-poll-replaced-{}-{}", std::process::id(), n));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // A *live* fake muse: a script named like the real binary,
    // alive for the poll. (A symlink to coreutils `sleep` exits
    // instantly — argv0 is foreign — leaving a zombie whose comm
    // used to match as a "live" agent; detection now skips
    // zombies, so the fake must actually run.)
    let muse_bin = dir.join("muse-bin-1.4.6-R6000.1");
    std::fs::write(&muse_bin, "#!/bin/sh\nsleep 30\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&muse_bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut muse = {
        // Guarded: this child inherits every fd the process currently
        // holds open, including any sibling test's daemon-lock fd.
        // Threads share one fd table, so an unguarded fork here would
        // duplicate a lock the sibling is mid-drop on and hold it until
        // this child execve's.
        let _guard = fork_guard();
        std::process::Command::new(&muse_bin)
            .arg("30")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("spawn fake muse")
    };
    fake_tmux_file_screen(&dir, muse.id(), "muse-bin-1.4.6-R6000.1");
    std::fs::write(
        dir.join("screen.txt"),
        "◇ Thinking (3m 48s · esc to interrupt)\n\n❯ \n  muse-wonder-1.0 · xhigh\n",
    )
    .unwrap();
    let tmux = crate::tmux::Tmux::test(dir.join("tmux"));
    let now = now_unix();
    let mut pi = hook_record(now);
    pi.pane = "%50".to_string();
    pi.agent = "pi".to_string();
    pi.state = AgentState::Idle;
    pi.summary = "session ended".to_string();
    pi.command = "pi".to_string();
    let mut prev = HashMap::new();
    prev.insert("%50".to_string(), pi);

    let (next, _, _, outage, _) =
        poll_sync_with(&tmux, prev, &builtin_compiled(), Tunables::default());

    assert!(!outage);
    let rec = next.get("%50").expect("live muse record");
    assert_eq!(rec.agent, "muse");
    assert_eq!(rec.source, Source::Proc);
    assert_eq!(rec.state, AgentState::Working);
    let _ = muse.kill();
    let _ = muse.wait();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn screen_only_replacement_supersedes_fresh_hook() {
    // Same pane-reuse shape, but the replacement is invisible to
    // the process matcher (here the pane pid is a plain `sleep`):
    // the screen names muse, so the dead pi hook must still lose
    // instead of masking the pane for the 30-minute stale window.
    let dir =
        std::env::temp_dir().join(format!("terhub-poll-screenreplaced-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut sleeper = {
        // Guarded: see `manual_pin_tracks_reg_pid_cpu` — a forked child
        // inherits the process's fds, so it must not land while a
        // sibling holds the daemon lock.
        let _guard = fork_guard();
        std::process::Command::new("sleep")
            .arg("30")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("spawn sleep")
    };
    fake_tmux_file_screen(&dir, sleeper.id(), "muse");
    std::fs::write(
        dir.join("screen.txt"),
        "◇ Thinking (3m 48s · esc to interrupt)\n\n❯ \n  muse-wonder-1.0 · xhigh\n",
    )
    .unwrap();
    let tmux = crate::tmux::Tmux::test(dir.join("tmux"));
    let now = now_unix();
    let mut pi = hook_record(now);
    pi.pane = "%50".to_string();
    pi.agent = "pi".to_string();
    pi.state = AgentState::Idle;
    pi.summary = "session ended".to_string();
    pi.command = "pi".to_string();
    let mut prev = HashMap::new();
    prev.insert("%50".to_string(), pi);

    let (next, _, _, outage, _) =
        poll_sync_with(&tmux, prev, &builtin_compiled(), Tunables::default());

    assert!(!outage);
    let rec = next.get("%50").expect("live muse record");
    assert_eq!(rec.agent, "muse");
    let _ = sleeper.kill();
    let _ = sleeper.wait();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn replacement_does_not_inherit_predecessor_metadata() {
    // On an agent change the new record must not display the old
    // agent's model, context percentage, or last prompt.
    let now = 100_000;
    let mut old = carried("pi", AgentState::Idle, 0, 0, now - 60);
    old.model = "pi-model-1".to_string();
    old.ctx_pct = Some(42);
    old.last_prompt = "the pi prompt".to_string();
    old.last_prompt_at = now - 30;
    old.handle = "pi-handle".to_string();
    old.reg_pid = 4242;
    let rec = merge_record_owned(
        Some(&old),
        false,
        None,
        Some(("muse", 10, 100)),
        &test_pane(),
        false,
        now,
        STUCK_SECS,
        QUIET_IDLE_SECS,
        WAITING_IDLE_SECS,
        DONE_IDLE_SECS,
    )
    .expect("no record");
    assert_eq!(rec.agent, "muse");
    assert!(rec.model.is_empty(), "model leaked: {}", rec.model);
    assert_eq!(rec.ctx_pct, None);
    assert!(rec.last_prompt.is_empty());
    assert_eq!(rec.last_prompt_at, 0);
    assert_eq!(rec.reg_pid, 0);
}

#[test]
fn manual_pin_drops_when_reg_pid_dead() {
    // The pinned process is gone: the pin drops even though the pane
    // is alive (pid 999999999 cannot exist: over the kernel maximum).
    let dir = std::env::temp_dir().join(format!("terhub-poll-manualdead-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let tmux = fake_tmux_one_sleep_pane(&dir, 999_999_999);
    let now = now_unix();
    let mut prev = HashMap::new();
    prev.insert("%50".to_string(), manual_record("%50", 999_999_999, now));
    let (next, _, _, _, _) = poll_sync_with(&tmux, prev, &[], Tunables::default());
    assert!(!next.contains_key("%50"), "dead-pin record should drop");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Fake tmux with a file-backed screen: rewrite `screen.txt` between
/// polls to change what the pane shows. Command + pid substituted in.
fn fake_tmux_file_screen(dir: &std::path::Path, pid: u32, command: &str) {
    use std::os::unix::fs::PermissionsExt;
    let sep = crate::tmux::SEP;
    let line = [
        "%50", "@1", "w", "1", "0", command, "/tmp", "1", "80", "24", "sess", "0",
    ]
    .join(sep);
    let payload = format!("{line}{sep}{pid}");
    let screen = dir.join("screen.txt");
    let script = format!(
        "#!/bin/sh\ncase \"$1\" in\n  list-panes) printf '%s\\n' '{payload}' ;;\n  capture-pane) cat '{}' ;;\n  *) printf '' ;;\nesac\n",
        screen.display()
    );
    let path = dir.join("tmux");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(&screen, "$ \n").unwrap();
}

fn builtin_compiled() -> Vec<crate::agent::CompiledManifest> {
    let (compiled, _) = crate::agent::compile_all_lossy(&crate::agent::builtin_manifests());
    compiled
}

#[test]
fn digit_only_error_step_keeps_hash_but_resets_quiet() {
    // "Error: 1 failed" → "Error: 2 failed": digit-fuzzing hides the
    // step from the screen hash (same hash), but the changed summary
    // still restarts the quiet clock (a new failure is activity).
    let dir = std::env::temp_dir().join(format!("terhub-poll-digits-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    fake_tmux_file_screen(&dir, 999_999_999, "claude");
    let tmux = crate::tmux::Tmux::test(dir.join("tmux"));
    let manifests = builtin_compiled();
    let screen = dir.join("screen.txt");
    std::fs::write(&screen, "Error: 1 failed\n$ \n").unwrap();
    let (next, _, _, _, _) = poll_sync_with(&tmux, HashMap::new(), &manifests, Tunables::default());
    let first = next.get("%50").expect("error record").clone();
    assert_eq!(first.summary, "Error: 1 failed");
    std::thread::sleep(std::time::Duration::from_secs(2));
    std::fs::write(&screen, "Error: 2 failed\n$ \n").unwrap();
    let (next, _, _, _, _) = poll_sync_with(&tmux, next, &manifests, Tunables::default());
    let second = next.get("%50").expect("error record carried").clone();
    assert_eq!(second.summary, "Error: 2 failed");
    assert_eq!(
        second.screen_hash, first.screen_hash,
        "digit-only step should not change the hash"
    );
    assert!(
        second.quiet_since > first.quiet_since,
        "changed summary should restart quiet"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn spinner_frames_do_not_restart_quiet() {
    // Spinner frame + elapsed counter advance, nothing else: the
    // quiet clock keeps growing instead of pegging near zero.
    let dir = std::env::temp_dir().join(format!("terhub-poll-spinner-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    fake_tmux_file_screen(&dir, 999_999_999, "claude");
    let tmux = crate::tmux::Tmux::test(dir.join("tmux"));
    let manifests = builtin_compiled();
    let screen = dir.join("screen.txt");
    std::fs::write(&screen, "⠙ Running 13min40s\n$ \n").unwrap();
    let (next, _, _, _, _) = poll_sync_with(&tmux, HashMap::new(), &manifests, Tunables::default());
    let first = next.get("%50").expect("working record").clone();
    assert_eq!(first.state, AgentState::Working);
    std::thread::sleep(std::time::Duration::from_secs(2));
    std::fs::write(&screen, "⠋ Running 13min44s\n$ \n").unwrap();
    let (next, _, _, _, _) = poll_sync_with(&tmux, next, &manifests, Tunables::default());
    let second = next.get("%50").expect("working record carried").clone();
    assert_eq!(
        second.quiet_since, first.quiet_since,
        "spinner-only change should not restart quiet"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn tip_rotation_does_not_restart_quiet() {
    // Footer tip carousel swaps full text every few seconds: the
    // quiet clock must ignore it like any other animation, while the
    // verdict beside it still stands.
    let dir = std::env::temp_dir().join(format!("terhub-poll-tips-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    fake_tmux_file_screen(&dir, 999_999_999, "claude");
    let tmux = crate::tmux::Tmux::test(dir.join("tmux"));
    let manifests = builtin_compiled();
    let screen = dir.join("screen.txt");
    std::fs::write(&screen, "Error: static\nTip: /context shows budget\n$ \n").unwrap();
    let (next, _, _, _, _) = poll_sync_with(&tmux, HashMap::new(), &manifests, Tunables::default());
    let first = next.get("%50").expect("error record").clone();
    assert_eq!(first.state, AgentState::Error);
    std::thread::sleep(std::time::Duration::from_secs(2));
    std::fs::write(&screen, "Error: static\nTip: /steer guides response\n$ \n").unwrap();
    let (next, _, _, _, _) = poll_sync_with(&tmux, next, &manifests, Tunables::default());
    let second = next.get("%50").expect("error record carried").clone();
    assert_eq!(second.summary, "Error: static");
    assert_eq!(
        second.quiet_since, first.quiet_since,
        "tip-only change should not restart quiet"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn continuity_ignores_agent_case() {
    // track_cpu matches agents case-insensitively; state continuity
    // must agree, or a "PI"→"pi" wobble resets the clock every poll.
    let now = 100_000;
    // Fresh still-clock (no stuck trip): only the clock continuity matters.
    let mut old = carried("PI", AgentState::Working, 50, now - 60, now - 600);
    old.agent_start = 100;
    let rec = merge_record_owned(
        Some(&old),
        false,
        None,
        Some(("pi", 50, 100)),
        &test_pane(),
        false,
        now,
        STUCK_SECS,
        QUIET_IDLE_SECS,
        WAITING_IDLE_SECS,
        DONE_IDLE_SECS,
    )
    .expect("no record");
    assert_eq!(rec.state_since, now - 600);
    assert_eq!(rec.cpu_still_since, now - 60);
}

#[test]
fn frozen_screen_makes_a_live_agent_idle() {
    let now = 100_000;
    // The real failure case: the process is alive and its REPL keeps
    // burning CPU, so Stuck never trips, but the screen has not moved
    // for longer than the quiet threshold — it finished. No
    // completion text, no pattern.
    let mut old = carried("pi", AgentState::Working, 500, now - 4, now - 900);
    old.quiet_since = now - (QUIET_IDLE_SECS + 300);
    let rec = merge_record_owned(
        Some(&old),
        false,
        None,
        Some(("pi", 500, 100)),
        &test_pane(),
        false,
        now,
        STUCK_SECS,
        QUIET_IDLE_SECS,
        WAITING_IDLE_SECS,
        DONE_IDLE_SECS,
    )
    .expect("no record");
    assert_eq!(rec.state, AgentState::Idle, "frozen screen read as busy");
    assert!(rec.summary.is_empty(), "{:?}", rec.summary);
    assert_eq!(rec.state_since, now);
    // The whole working stretch is the completed turn.
    assert_eq!(rec.last_turn_secs, 900);
}

#[test]
fn quiet_idle_survives_the_poll_loop() {
    // The merge alone is not the feature: the poller re-evaluates quiet
    // with refresh_quiet right after, and a summary the flip itself
    // invented reads as fresh activity and resets the clock the flip
    // just measured. Drive three polls to prove Idle survives.
    let now = 100_000;
    let mut old = carried("pi", AgentState::Working, 500, now - 4, now - 900);
    old.quiet_since = now - (QUIET_IDLE_SECS + 300);
    let h = 0xABCD_1234_u64; // screen has not moved across all polls
    old.screen_hash = h;

    let rec = merge_record_owned(
        Some(&old),
        false,
        None,
        Some(("pi", 500, 100)),
        &test_pane(),
        false,
        now,
        STUCK_SECS,
        QUIET_IDLE_SECS,
        WAITING_IDLE_SECS,
        DONE_IDLE_SECS,
    )
    .expect("no record");
    assert_eq!(rec.state, AgentState::Idle);

    // What the poller does next, verbatim.
    let (nh, nq) = refresh_quiet(Some(&old), Some(h), &rec, now);
    assert_eq!(
        nq,
        now - (QUIET_IDLE_SECS + 300),
        "the flip's own summary reset the clock it reads"
    );

    // Poll 2: same screen, so it must still be Idle — not one-poll flicker.
    let mut carried2 = rec.clone();
    carried2.screen_hash = nh;
    carried2.quiet_since = nq;
    let rec2 = merge_record_owned(
        Some(&carried2),
        false,
        None,
        Some(("pi", 500, 100)),
        &test_pane(),
        false,
        now + 2,
        STUCK_SECS,
        QUIET_IDLE_SECS,
        WAITING_IDLE_SECS,
        DONE_IDLE_SECS,
    )
    .expect("no record");
    assert_eq!(
        rec2.state,
        AgentState::Idle,
        "flickered back to working after one poll"
    );
    assert_eq!(rec2.state_since, now, "idle clock must not restart");

    // And it stays put on a third poll.
    let mut carried3 = rec2.clone();
    carried3.screen_hash = nh;
    carried3.quiet_since = nq;
    let rec3 = merge_record_owned(
        Some(&carried3),
        false,
        None,
        Some(("pi", 500, 100)),
        &test_pane(),
        false,
        now + 4,
        STUCK_SECS,
        QUIET_IDLE_SECS,
        WAITING_IDLE_SECS,
        DONE_IDLE_SECS,
    )
    .expect("no record");
    assert_eq!(rec3.state, AgentState::Idle);
}

#[test]
fn resumed_work_restarts_the_idle_clock() {
    // The reported bug: a pane that just started working again kept
    // showing the previous stillness in the Idle column until its
    // pixels visibly moved (spinner frames are denoised out of the
    // hash, so a busy screen can hash unchanged for minutes). The
    // state flip itself is activity: entering Working restarts the
    // visible-quiet clock the same poll.
    let now = 100_000;
    let h = 0xABCD_1234_u64; // unchanged screen, still hash
    let mut old = carried("pi", AgentState::Idle, 500, now - 900, now - 900);
    old.quiet_since = now - 900;
    old.screen_hash = h;
    // Merge alone flips on a recently-quiet screen (fresh CPU ticks
    // keep Stuck out of it) ...
    let mut fresh = old.clone();
    fresh.quiet_since = now - 2;
    let rec = merge_record_owned(
        Some(&fresh),
        false,
        None,
        Some(("pi", 900, 100)),
        &test_pane(),
        false,
        now,
        STUCK_SECS,
        QUIET_IDLE_SECS,
        WAITING_IDLE_SECS,
        DONE_IDLE_SECS,
    )
    .expect("no record");
    assert_eq!(rec.state, AgentState::Working);
    // ... and the poller's verbatim step restarts the clock even
    // when the screen did not move this poll. (`old` still carries
    // the stale clock here on purpose: the restart must not depend
    // on the screen having moved first.)
    let (nh, nq) = refresh_quiet(Some(&old), Some(h), &rec, now);
    assert_eq!(nh, h);
    assert_eq!(nq, now, "resumed work kept showing the old stillness");
    // Steady working keeps the screen clock (no whitewash): the
    // stuck smell still accumulates on a frozen screen.
    let mut steady = rec.clone();
    steady.screen_hash = h;
    steady.quiet_since = now - 60;
    let (sh, sq) = refresh_quiet(Some(&steady), Some(h), &steady, now + 60);
    assert_eq!((sh, sq), (h, now - 60));
    // A demotion into Idle keeps the old clock — that stillness is
    // the whole reason it demoted.
    let mut working = carried("pi", AgentState::Working, 500, now - 4, now - 900);
    working.quiet_since = now - (QUIET_IDLE_SECS + 300);
    working.screen_hash = h;
    let demoted = merge_record_owned(
        Some(&working),
        false,
        None,
        Some(("pi", 500, 100)),
        &test_pane(),
        false,
        now,
        STUCK_SECS,
        QUIET_IDLE_SECS,
        WAITING_IDLE_SECS,
        DONE_IDLE_SECS,
    )
    .expect("no record");
    assert_eq!(demoted.state, AgentState::Idle);
    let (_, dq) = refresh_quiet(Some(&working), Some(h), &demoted, now);
    assert_eq!(
        dq,
        now - (QUIET_IDLE_SECS + 300),
        "demotion must not whitewash stillness"
    );
}

#[test]
fn idle_flips_back_to_working_when_the_screen_moves() {
    let now = 100_000;
    let mut old = carried("pi", AgentState::Idle, 500, now - 900, now - 900);
    old.quiet_since = now - 900;
    // quiet_since is one poll stale, so a resumed screen shows up here
    // as "recently quiet". Fresh CPU ticks keep Stuck out of it.
    let mut fresh = old.clone();
    fresh.quiet_since = now - 2;
    let rec = merge_record_owned(
        Some(&fresh),
        false,
        None,
        Some(("pi", 900, 100)),
        &test_pane(),
        false,
        now,
        STUCK_SECS,
        QUIET_IDLE_SECS,
        WAITING_IDLE_SECS,
        DONE_IDLE_SECS,
    )
    .expect("no record");
    assert_eq!(rec.state, AgentState::Working);
    assert_eq!(rec.state_since, now);
}

#[test]
fn a_busy_working_row_shows_its_phase_not_the_spinner() {
    // The phase is the one line that says what the agent is doing
    // ("Goal · Verifying the result"). Spinner text is noise, and a
    // summary carried over from a waiting row is rot — neither
    // belongs on a working row.
    let now = 100_000;
    let hit = Some(ScreenHit {
        agent: Some("minimax-code".to_string()),
        state: AgentState::Working,
        summary: "⠋ Loading 33min23s · ⚡ 478 tok/s".to_string(),
        phase: Some("Goal · Verifying the result · 54min59s active".to_string()),
    });
    let rec = merge_record_owned(
        None,
        false,
        hit,
        Some(("minimax-code", 500, 100)),
        &test_pane(),
        false,
        now,
        STUCK_SECS,
        QUIET_IDLE_SECS,
        WAITING_IDLE_SECS,
        DONE_IDLE_SECS,
    )
    .expect("no record");
    assert_eq!(rec.state, AgentState::Working);
    assert_eq!(
        rec.summary, "Goal · Verifying the result · 54min59s active",
        "the working row must show what the agent is doing"
    );
}

#[test]
fn a_working_row_without_a_phase_shows_nothing() {
    // A plain working agent has no phase: the row stays summary-free
    // rather than wearing the spinner's ticker as its text.
    let now = 100_000;
    let rec = merge_record_owned(
        None,
        false,
        screen_hit(Some("minimax-code"), AgentState::Working, "⠋ Loading"),
        Some(("minimax-code", 500, 100)),
        &test_pane(),
        false,
        now,
        STUCK_SECS,
        QUIET_IDLE_SECS,
        WAITING_IDLE_SECS,
        DONE_IDLE_SECS,
    )
    .expect("no record");
    assert_eq!(rec.state, AgentState::Working);
    assert!(rec.summary.is_empty(), "no phase, no summary");
}

#[test]
fn quiet_idle_below_threshold_stays_working() {
    let now = 100_000;
    let mut old = carried("pi", AgentState::Working, 500, now - 4, now - 900);
    old.quiet_since = now - 60;
    let rec = merge_record_owned(
        Some(&old),
        false,
        None,
        Some(("pi", 500, 100)),
        &test_pane(),
        false,
        now,
        STUCK_SECS,
        QUIET_IDLE_SECS,
        WAITING_IDLE_SECS,
        DONE_IDLE_SECS,
    )
    .expect("no record");
    assert_eq!(
        rec.state,
        AgentState::Working,
        "silent tool run read as idle"
    );
}

#[test]
fn quiet_idle_zero_disables_the_flip() {
    let now = 100_000;
    let mut old = carried("pi", AgentState::Working, 500, now - 4, now - 900);
    old.quiet_since = now - 100_000;
    let rec = merge_record_owned(
        Some(&old),
        false,
        None,
        Some(("pi", 500, 100)),
        &test_pane(),
        false,
        now,
        STUCK_SECS,
        0,
        0,
        0,
    )
    .expect("no record");
    assert_eq!(rec.state, AgentState::Working, "0 must disable the flip");
}

#[test]
fn done_decays_to_idle_once_the_completion_is_old_news() {
    // A finished agent whose screen has been quiet past `done_idle_secs`
    // is waiting for its next prompt — that is idle. Without this decay
    // the completion banner stays in the scrollback, the classifier
    // re-matches it on every poll, and the row reads `done` forever: it
    // never counts towards the idle summary, and to anyone scanning the
    // column it is indistinguishable from an agent mid-turn.
    let now = 100_000;
    let mut old = carried("minimax-code", AgentState::Done, 500, now - 4, now - 900);
    old.quiet_since = now - (DONE_IDLE_SECS + 300);
    old.summary = "✓ Todo list 6/6 completed".to_string();
    let rec = merge_record_owned(
        Some(&old),
        false,
        screen_hit(
            Some("minimax-code"),
            AgentState::Done,
            "✓ Todo list 6/6 completed",
        ),
        Some(("minimax-code", 500, 100)),
        &test_pane(),
        false,
        now,
        STUCK_SECS,
        QUIET_IDLE_SECS,
        WAITING_IDLE_SECS,
        DONE_IDLE_SECS,
    )
    .expect("no record");
    assert_eq!(
        rec.state,
        AgentState::Idle,
        "a stale completion must read idle, not done"
    );
    assert_eq!(
        rec.summary, "✓ Todo list 6/6 completed",
        "the completion itself is not lost when the label decays"
    );
}

#[test]
fn done_below_the_threshold_stays_done() {
    // The decay is a decay, not a demotion on sight: a fresh
    // completion is still news and must keep its own label — that is
    // what lets it float above the idle mass when sorted by state.
    let now = 100_000;
    let mut old = carried("minimax-code", AgentState::Done, 500, now - 4, now - 900);
    old.quiet_since = now - (DONE_IDLE_SECS - 60);
    let rec = merge_record_owned(
        Some(&old),
        false,
        screen_hit(
            Some("minimax-code"),
            AgentState::Done,
            "✓ Todo list 6/6 completed",
        ),
        Some(("minimax-code", 500, 100)),
        &test_pane(),
        false,
        now,
        STUCK_SECS,
        QUIET_IDLE_SECS,
        WAITING_IDLE_SECS,
        DONE_IDLE_SECS,
    )
    .expect("no record");
    assert_eq!(
        rec.state,
        AgentState::Done,
        "a fresh completion is still done"
    );
}

#[test]
fn done_idle_zero_disables_the_decay() {
    let now = 100_000;
    let mut old = carried("minimax-code", AgentState::Done, 500, now - 4, now - 900);
    old.quiet_since = now - 100_000;
    let rec = merge_record_owned(
        Some(&old),
        false,
        screen_hit(
            Some("minimax-code"),
            AgentState::Done,
            "✓ Todo list 6/6 completed",
        ),
        Some(("minimax-code", 500, 100)),
        &test_pane(),
        false,
        now,
        STUCK_SECS,
        QUIET_IDLE_SECS,
        WAITING_IDLE_SECS,
        0,
    )
    .expect("no record");
    assert_eq!(rec.state, AgentState::Done, "0 must disable the decay");
}

#[test]
fn done_decay_survives_the_poll_loop() {
    // The merge alone is not the feature: the classifier re-derives
    // `done` from the same unchanged screen on every poll, so the
    // demotion has to be sticky. If the idle clock restarted on each
    // poll the row would flicker between `done` and `idle` forever.
    let now = 100_000;
    let mut old = carried("minimax-code", AgentState::Done, 500, now - 4, now - 900);
    old.quiet_since = now - (DONE_IDLE_SECS + 300);
    old.summary = "✓ Todo list 6/6 completed".to_string();
    let h = 0xABCD_1234_u64; // screen has not moved across all polls
    old.screen_hash = h;

    let rec = merge_record_owned(
        Some(&old),
        false,
        screen_hit(
            Some("minimax-code"),
            AgentState::Done,
            "✓ Todo list 6/6 completed",
        ),
        Some(("minimax-code", 500, 100)),
        &test_pane(),
        false,
        now,
        STUCK_SECS,
        QUIET_IDLE_SECS,
        WAITING_IDLE_SECS,
        DONE_IDLE_SECS,
    )
    .expect("no record");
    assert_eq!(rec.state, AgentState::Idle);

    // What the poller does next, verbatim. The completion summary is
    // stable, so it must not read as fresh activity and reset the very
    // quiet clock the decay just measured.
    let (nh, nq) = refresh_quiet(Some(&old), Some(h), &rec, now);
    assert_eq!(
        nq,
        now - (DONE_IDLE_SECS + 300),
        "the completion summary reset the quiet clock"
    );

    // Poll 2: the unchanged screen still classifies as done, but the
    // row is already idle — it must stay idle, and its clock must not
    // restart.
    let mut carried2 = rec.clone();
    carried2.screen_hash = nh;
    carried2.quiet_since = nq;
    let rec2 = merge_record_owned(
        Some(&carried2),
        false,
        screen_hit(
            Some("minimax-code"),
            AgentState::Done,
            "✓ Todo list 6/6 completed",
        ),
        Some(("minimax-code", 500, 100)),
        &test_pane(),
        false,
        now + 2,
        STUCK_SECS,
        QUIET_IDLE_SECS,
        WAITING_IDLE_SECS,
        DONE_IDLE_SECS,
    )
    .expect("no record");
    assert_eq!(
        rec2.state,
        AgentState::Idle,
        "flickered back to done after one poll"
    );
    assert_eq!(rec2.state_since, now, "idle clock must not restart");
    assert_eq!(
        rec2.summary, "✓ Todo list 6/6 completed",
        "the completion survives the decay"
    );
}

#[test]
fn a_working_row_is_never_decayed_by_the_done_rule() {
    // The decay is scoped to `done`. A pane that is working must not
    // be demoted by it, however old its last completion was — the
    // quiet clock says "the screen has not moved", and a live agent
    // that has not moved is handled by the working flip, not this one.
    let now = 100_000;
    let mut old = carried("minimax-code", AgentState::Done, 500, now - 4, now - 900);
    // Past the done threshold but short of the working flip, so the
    // working rule stays out of it too.
    old.quiet_since = now - (DONE_IDLE_SECS + 300);
    let rec = merge_record_owned(
        Some(&old),
        false,
        screen_hit(Some("minimax-code"), AgentState::Working, ""),
        Some(("minimax-code", 900, 100)),
        &test_pane(),
        false,
        now,
        STUCK_SECS,
        QUIET_IDLE_SECS,
        WAITING_IDLE_SECS,
        DONE_IDLE_SECS,
    )
    .expect("no record");
    assert_eq!(
        rec.state,
        AgentState::Working,
        "a live agent is not decayed"
    );
}

#[test]
fn long_untouched_waiting_prompt_becomes_idle() {
    let now = 100_000;
    // gemini's finished turn: the screen ends on a bare `>` that the
    // classifier reads as waiting, and nobody has touched it for 40
    // minutes. It must stop demanding attention.
    let mut old = carried("gemini", AgentState::Waiting, 500, now - 4, now - 5_000);
    old.quiet_since = now - 2_400;
    let rec = merge_record_owned(
        Some(&old),
        false,
        screen_hit(Some("gemini"), AgentState::Waiting, "1 artifact to review"),
        Some(("gemini", 500, 100)),
        &test_pane(),
        false,
        now,
        STUCK_SECS,
        QUIET_IDLE_SECS,
        WAITING_IDLE_SECS,
        DONE_IDLE_SECS,
    )
    .expect("no record");
    assert_eq!(rec.state, AgentState::Idle, "stale prompt still nagging");
    // The question survives: that is what you came back to read.
    assert!(rec.summary.contains("artifact"), "{:?}", rec.summary);
}

#[test]
fn fresh_waiting_prompt_keeps_flagging() {
    let now = 100_000;
    // Stepping away for a few minutes must not silence a question.
    let mut old = carried("gemini", AgentState::Waiting, 500, now - 4, now - 200);
    old.quiet_since = now - 300;
    let rec = merge_record_owned(
        Some(&old),
        false,
        screen_hit(Some("gemini"), AgentState::Waiting, "approve?"),
        Some(("gemini", 500, 100)),
        &test_pane(),
        false,
        now,
        STUCK_SECS,
        QUIET_IDLE_SECS,
        WAITING_IDLE_SECS,
        DONE_IDLE_SECS,
    )
    .expect("no record");
    assert_eq!(rec.state, AgentState::Waiting);
    assert!(rec.state.needs_attention());
}

#[test]
fn waiting_idle_zero_keeps_every_prompt_flagged() {
    let now = 100_000;
    let mut old = carried("gemini", AgentState::Waiting, 500, now - 4, now - 5_000);
    old.quiet_since = now - 100_000;
    let rec = merge_record_owned(
        Some(&old),
        false,
        screen_hit(Some("gemini"), AgentState::Waiting, "approve?"),
        Some(("gemini", 500, 100)),
        &test_pane(),
        false,
        now,
        STUCK_SECS,
        QUIET_IDLE_SECS,
        0,
        DONE_IDLE_SECS,
    )
    .expect("no record");
    assert_eq!(
        rec.state,
        AgentState::Waiting,
        "0 must disable the demotion"
    );
}

#[test]
fn finished_quiet_pane_reads_idle_not_stuck() {
    // The `samai` report: a pane whose screen has been quiet for
    // longer than `quiet_idle_secs` (900s) and whose process has
    // not burned CPU for longer than `stuck_secs` (600s). The
    // agent's REPL is still alive, so `still` keeps counting —
    // but the long-quiet screen is the stronger signal: the pane
    // is *finished*, not hung. The Stuck gate's quiescence guard
    // declines to fire, and the promotion below lands it on
    // `Idle`. Without that guard, a finished pane reads `stuck`
    // forever — the misclassification the user saw on `samai`.
    let now = 100_000;
    let mut old = carried("pi", AgentState::Working, 500, now - 900, now - 900);
    old.quiet_since = now - 900;
    let rec = merge_record_owned(
        Some(&old),
        false,
        None,
        Some(("pi", 500, 100)),
        &test_pane(),
        false,
        now,
        STUCK_SECS,
        QUIET_IDLE_SECS,
        WAITING_IDLE_SECS,
        DONE_IDLE_SECS,
    )
    .expect("no record");
    assert_eq!(
        rec.state,
        AgentState::Idle,
        "a finished-and-quiet pane must read idle, not stuck: {rec:?}"
    );
}

#[test]
fn recently_frozen_pane_reads_stuck() {
    // The genuine hang the Stuck gate exists for: the spinner
    // stopped *recently* (screen froze 30s ago — well inside
    // `quiet_idle_secs`), and the process has not burned CPU for
    // past `stuck_secs`. No quiescence promotion applies (the
    // screen was not long-quiet), so Stuck wins. This is the
    // case the old `stuck_outranks_quiescent_idle` described, but
    // with the screen freeze recent: a long-frozen screen is
    // finished, not hung.
    let now = 100_000;
    let mut old = carried("pi", AgentState::Working, 500, now - 900, now - 900);
    old.quiet_since = now - 30;
    let rec = merge_record_owned(
        Some(&old),
        false,
        None,
        Some(("pi", 500, 100)),
        &test_pane(),
        false,
        now,
        STUCK_SECS,
        QUIET_IDLE_SECS,
        WAITING_IDLE_SECS,
        DONE_IDLE_SECS,
    )
    .expect("no record");
    assert_eq!(
        rec.state,
        AgentState::Stuck,
        "a recently-frozen pane is a real hang: {rec:?}"
    );
    // A pane already on Idle that froze *recently* still escalates:
    // a finished-looking row whose spinner just died is a hang
    // that finished, not a clean finish.
    let mut idle = carried("pi", AgentState::Idle, 500, now - 900, now - 900);
    idle.quiet_since = now - 30;
    let rec = merge_record_owned(
        Some(&idle),
        false,
        None,
        Some(("pi", 500, 100)),
        &test_pane(),
        false,
        now,
        STUCK_SECS,
        QUIET_IDLE_SECS,
        WAITING_IDLE_SECS,
        DONE_IDLE_SECS,
    )
    .expect("no record");
    assert_eq!(
        rec.state,
        AgentState::Stuck,
        "idle masked a recent hang: {rec:?}"
    );
    // … but one that has been frozen for a long time stays Idle:
    // that is a finished pane, not a hang (the samai case).
    let mut idle_long = carried("pi", AgentState::Idle, 500, now - 900, now - 900);
    idle_long.quiet_since = now - 900;
    let rec = merge_record_owned(
        Some(&idle_long),
        false,
        None,
        Some(("pi", 500, 100)),
        &test_pane(),
        false,
        now,
        STUCK_SECS,
        QUIET_IDLE_SECS,
        WAITING_IDLE_SECS,
        DONE_IDLE_SECS,
    )
    .expect("no record");
    assert_eq!(
        rec.state,
        AgentState::Idle,
        "long-quiet idle must stay idle: {rec:?}"
    );
}

#[test]
fn runtime_dir_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("terhub-dirperms-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // Simulate a pre-existing umask-permissive dir.
    let mut p = std::fs::metadata(&dir).unwrap().permissions();
    p.set_mode(0o755);
    std::fs::set_permissions(&dir, p).unwrap();
    ensure_runtime_dir(&dir).unwrap();
    let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn runtime_files_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("terhub-fileperms-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // Socket: simulate a umask-permissive bind, then tighten.
    let sock = dir.join("terhubd.sock");
    let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    let mut p = std::fs::metadata(&sock).unwrap().permissions();
    p.set_mode(0o777);
    std::fs::set_permissions(&sock, p).unwrap();
    set_owner_only(&sock).unwrap();
    let mode = std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "socket must be owner-only");
    drop(listener);
    // Pid file: atomic write must land 0600 straight away.
    let pid_file = dir.join("terhubd.pid");
    write_pid_file(&pid_file, "12345").unwrap();
    let mode = std::fs::metadata(&pid_file).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "pid file must be owner-only");
    assert_eq!(std::fs::read_to_string(&pid_file).unwrap(), "12345");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn peer_credential_policy_admits_only_our_uid() {
    // The policy, split out so it is testable without privileges:
    // a peer whose uid the platform reports must be our own uid.
    let ours = 1000;
    assert!(peer_allowed(Some(1000), ours), "our own uid is admitted");
    assert!(!peer_allowed(Some(0), ours), "another uid is refused");
    assert!(!peer_allowed(Some(1001), ours), "another uid is refused");
    // A platform that cannot report a Unix socket peer leaves the
    // socket's 0600 mode as the only gate.
    assert!(peer_allowed(None, ours));
}

#[tokio::test]
async fn peer_uid_is_the_kernel_reported_peer() {
    // SO_PEERCRED reports the *other* end's uid; on a socketpair
    // that is this process, which is exactly the value the accept
    // loop compares against our own uid. If this ever stops
    // reporting, the check fails closed instead of admitting
    // everyone.
    let (a, _b) = UnixStream::pair().unwrap();
    a.set_nonblocking(true).unwrap();
    let stream = TokioUnixStream::from_std(a).unwrap();
    let peer = peer_uid(&stream).expect("peer credential read");
    assert_eq!(peer, Some(our_uid()));
    // And the policy admits that peer.
    assert!(peer_allowed(peer, our_uid()));
}

#[test]
fn a_daemon_that_answers_too_late_is_slow_not_missing() {
    // A peer that accepts the connection and then says nothing: the
    // read runs into the socket's receive timeout. That is "the
    // daemon is too slow right now", not "the daemon is gone" --
    // reporting it as missing would send the user off to start a
    // second daemon.
    let dir = std::env::temp_dir().join(format!("terhub-slow-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("slow.sock");
    let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    let _acceptor = std::thread::spawn(move || {
        if let Ok((_conn, _)) = listener.accept() {
            // Accept and stay silent: the client's read times out.
            std::thread::sleep(Duration::from_secs(30));
        }
    });
    let err = send_request_at(&sock, &Request::Ping, Duration::from_millis(300))
        .expect_err("the silent peer must not answer");
    assert!(
        matches!(err, DaemonError::Slow { .. }),
        "a timed-out read must not be reported as a missing daemon: {err:?}"
    );
    assert!(
        err.message().contains("too slow"),
        "the message must say the daemon is slow: {}",
        err.message()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn daemon_log_survives_a_missing_runtime_dir_and_is_appended_to() {
    use std::os::unix::fs::PermissionsExt;
    // The fresh-install case: the runtime dir does not exist when
    // the log is opened. The log must still be there afterwards —
    // a daemon whose stderr has nowhere to go loses its manifest
    // warnings, bind errors and panics.
    let dir = std::env::temp_dir().join(format!("terhub-log-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    assert!(!dir.exists(), "the dir must not exist yet");
    let mut f = open_daemon_log(&dir).expect("the log opens on a fresh install");
    use std::io::Write;
    f.write_all(b"first run\n").unwrap();
    drop(f);
    let log = dir.join("terhubd.log");
    assert!(log.is_file(), "the log was not created");
    // Owner-only, like every other runtime file: daemon stderr can
    // carry prompt text.
    let mode = std::fs::metadata(&log).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "the log must be owner-only");
    // The runtime dir is created with the daemon's 0700 mode.
    let dmode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(dmode, 0o700, "the runtime dir must be owner-only");
    // A respawn appends: the previous run's stderr is where a
    // crash or a failed bind is explained.
    let mut f = open_daemon_log(&dir).expect("the log reopens");
    f.write_all(b"second run\n").unwrap();
    drop(f);
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(
        text.contains("first run") && text.contains("second run"),
        "the respawn truncated the previous run's log: {text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_slow_notifier_does_not_stall_the_caller() {
    // The notifier is a desktop subprocess with a 5s bound. Waiting
    // for it inline parked the tokio worker that ran it and
    // stretched the poll cycle by 5s per attention transition.
    // The detached dispatch must return immediately.
    //
    // The lock is the process-wide env scratch lock: this test
    // redirects PATH, and a concurrent setter in another module
    // would hijack the program lookup.
    let _lock = crate::app::tests::SCRATCH_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("terhub-notify-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // A fake notify-send that takes 2s: the real one is a desktop
    // service that can wedge exactly like this. It leaves a marker
    // so the test can prove the child really ran — a notifier that
    // never started would make the timing assertion vacuous.
    let fake = dir.join("notify-send");
    let marker = dir.join("notified");
    std::fs::write(
        &fake,
        format!("#!/bin/sh\ntouch '{}'\nsleep 2\n", marker.display()),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let cfg = dir.join("config.toml");
    std::fs::write(&cfg, "[notifications]\nenabled = true\n").unwrap();
    // The config path is per-thread, so this test's "notifications
    // on" cannot leak into a sibling test's view of the config.
    let _cfg = crate::config::override_config_path(Some(cfg));
    let old_path = std::env::var("PATH").ok();
    // SAFETY: the scratch lock is held, so no other lib test is
    // rewriting PATH.
    unsafe {
        std::env::set_var(
            "PATH",
            format!("{}:{}", dir.display(), old_path.clone().unwrap_or_default()),
        );
    }
    struct Restore {
        path: Option<String>,
    }
    impl Drop for Restore {
        fn drop(&mut self) {
            // SAFETY: the scratch lock is still held (the guard
            // outlives this struct: it is declared first).
            unsafe {
                match self.path.take() {
                    Some(p) => std::env::set_var("PATH", p),
                    None => std::env::remove_var("PATH"),
                }
            }
        }
    }
    let _restore = Restore { path: old_path };

    let t = crate::attend::Transition {
        pane: "%1".to_string(),
        agent: "pi".to_string(),
        from: AgentState::Working,
        to: AgentState::Waiting,
        session: "s".to_string(),
        window: "w".to_string(),
        summary: "needs input".to_string(),
    };
    let start = std::time::Instant::now();
    notify_detached(&t);
    let elapsed = start.elapsed();
    assert!(
        elapsed < std::time::Duration::from_millis(500),
        "the notifier was awaited inline: {elapsed:?}"
    );
    // The notifier really ran: it is spawned eagerly on the
    // dispatching thread, so the child exists by the time the
    // dispatch returns — and it is this thread's config that
    // decided notifications were on at all. The marker is written
    // by the child, so wait for it rather than assume it has
    // already run.
    let mut started = false;
    for _ in 0..200 {
        if marker.exists() {
            started = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(started, "the notifier never started");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn runtime_symlink_swap_detected() {
    use std::os::unix::fs::PermissionsExt;
    let base = std::env::temp_dir().join(format!("terhub-symlink-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let real = base.join("real");
    std::fs::create_dir_all(&real).unwrap();
    let link = base.join("rt");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    // Symlink at check time is refused outright.
    assert!(ensure_runtime_dir(&link).is_err());
    // A checked dir verifies clean ...
    let guard = ensure_runtime_dir(&real).unwrap();
    guard.verify().unwrap();
    // ... but swapping the guarded path for a symlink afterwards
    // is detected: rename dir away, link the name at an attacker dir.
    let moved = base.join("real-moved");
    std::fs::rename(&real, &moved).unwrap();
    let evil = base.join("evil");
    std::fs::create_dir_all(&evil).unwrap();
    std::os::unix::fs::symlink(&evil, &real).unwrap();
    assert!(guard.verify().is_err(), "dir->symlink swap went undetected");
    // Same for a plain dir replacement (rename swap).
    std::fs::remove_file(&real).unwrap();
    std::fs::create_dir_all(&real).unwrap();
    let mut p = std::fs::metadata(&real).unwrap().permissions();
    p.set_mode(0o700);
    std::fs::set_permissions(&real, p).unwrap();
    assert!(guard.verify().is_err(), "dir replacement went undetected");
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn path_daemon_bin_must_be_safe_to_exec() {
    use std::os::unix::fs::PermissionsExt;
    let base = std::env::temp_dir().join(format!("terhub-pathbin-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let good_dir = base.join("good");
    let evil_dir = base.join("evil");
    std::fs::create_dir_all(&good_dir).unwrap();
    std::fs::create_dir_all(&evil_dir).unwrap();
    // Evil dir first in PATH: a world-writable `terhubd` there must
    // be skipped, and the good install later in PATH must win.
    let evil_bin = evil_dir.join("terhubd");
    std::fs::write(&evil_bin, "#!/bin/sh\nexit 1\n").unwrap();
    let mut p = std::fs::metadata(&evil_bin).unwrap().permissions();
    p.set_mode(0o777);
    std::fs::set_permissions(&evil_bin, p).unwrap();
    let good_bin = good_dir.join("terhubd");
    std::fs::write(&good_bin, "#!/bin/sh\nexit 0\n").unwrap();
    let mut p = std::fs::metadata(&good_bin).unwrap().permissions();
    p.set_mode(0o755);
    std::fs::set_permissions(&good_bin, p).unwrap();
    let path_var =
        std::ffi::OsString::from(format!("{}:{}", evil_dir.display(), good_dir.display()));
    let found = find_in_path("terhubd", &path_var).unwrap();
    assert_eq!(found, good_bin);
    // A non-regular file (directory) is not executable-as-daemon.
    let dir_only = base.join("dironly");
    std::fs::create_dir_all(dir_only.join("terhubd")).unwrap();
    let path_var = std::ffi::OsString::from(dir_only.join("terhubd").parent().unwrap());
    assert!(find_in_path("terhubd", &path_var).is_err());
    // Nothing at all: fail closed, no bare-name fallback.
    let empty = base.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let path_var = std::ffi::OsString::from(&empty);
    let err = find_in_path("terhubd", &path_var).unwrap_err();
    assert!(err.to_string().contains("no `terhubd` found in PATH"));
    // Only an unsafe candidate: rejected, not returned.
    let path_var = std::ffi::OsString::from(&evil_dir);
    let err = find_in_path("terhubd", &path_var).unwrap_err();
    assert!(err.to_string().contains("unsafe candidate(s) rejected"));
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn daemon_lock_excludes_second_holder() {
    // Two daemons started together must not both pass: the second
    // flock attempt fails while the first holder lives.
    //
    // Held under FORK_GUARD so no sibling forks a child while this
    // test holds the lock. flock is per open-file-description: a
    // forked child *duplicates* the OFD, so a child forked while a
    // lock is held keeps that lock alive past the parent's drop() —
    // O_CLOEXEC closes the duplicate only at execve, which leaves a
    // fork→execve window a fast script child still occupies.
    //
    // "Released on drop" is deliberately NOT asserted here: no
    // in-process assertion can distinguish "the parent dropped the
    // lock" from "some sibling's child is between fork and execve".
    // That arm lives in tests/daemon_lock.rs, its own test binary
    // where no sibling forks anything.
    let _guard = fork_guard();
    let dir = std::env::temp_dir().join(format!("terhub-lock-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let first = acquire_daemon_lock(&dir).expect("first lock");
    assert!(
        acquire_daemon_lock(&dir).is_err(),
        "second daemon locked too"
    );
    drop(first);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn oversize_request_rejected_without_oom() {
    // A 2MB (valid!) hook event exceeds the 1MB request cap: truncated,
    // rejected, never applied — instead of read whole into memory.
    let registry = Arc::new(Mutex::new(HashMap::new()));
    let shutdown = Arc::new(AtomicBool::new(false));
    let dir = std::env::temp_dir().join(format!("terhub-bigreq-{}", std::process::id()));
    let (server, client) = tokio::net::UnixStream::pair().unwrap();
    let reg = registry.clone();
    let rt = dir.clone();
    let guard = ensure_runtime_dir(&dir).unwrap();
    let handle = tokio::spawn(async move { handle_conn(server, reg, rt, guard, shutdown).await });
    let big = serde_json::to_string(&Request::Event {
        event: crate::agent::HookEvent {
            pane: "%1".to_string(),
            agent: "pi".to_string(),
            state: AgentState::Waiting,
            summary: "s".repeat(2 * 1024 * 1024),
            prompt: String::new(),
            model: String::new(),
            ctx: None,
        },
    })
    .unwrap();
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let (rd, mut wr) = client.into_split();
    // The write may stall once the server stops draining (that's the
    // cap working); abort it after the verdict instead of joining.
    let write_task = tokio::spawn(async move {
        let _ = wr.write_all(big.as_bytes()).await;
        let _ = wr.write_all(b"\n").await;
    });
    let mut lines = tokio::io::BufReader::new(rd).lines();
    let resp: Response = serde_json::from_str(
        &tokio::time::timeout(Duration::from_secs(5), lines.next_line())
            .await
            .expect("no response")
            .expect("read failed")
            .expect("empty response"),
    )
    .unwrap();
    write_task.abort();
    assert!(!resp.ok, "oversize request accepted");
    assert!(
        lock_registry(&registry).is_empty(),
        "truncated event applied"
    );
    handle.await.unwrap();
}

#[tokio::test]
async fn conn_tasks_shed_load_past_the_cap() {
    // One permit: the first idle connection holds it for its whole
    // 10s read wait; the second is shed (peer sees EOF) instead of
    // piling up another registry-holding task. The permit moves in
    // admit_conn itself, so there is no race with task startup.
    use tokio::io::AsyncBufReadExt;
    let registry = Arc::new(Mutex::new(HashMap::new()));
    let shutdown = Arc::new(AtomicBool::new(false));
    let dir = std::env::temp_dir().join(format!("terhub-shed-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let permits = Arc::new(tokio::sync::Semaphore::new(1));
    let admit = |stream| {
        admit_conn(
            stream,
            registry.clone(),
            dir.clone(),
            ensure_runtime_dir(&dir).unwrap(),
            shutdown.clone(),
            permits.clone(),
        )
    };
    let (s1, c1) = tokio::net::UnixStream::pair().unwrap();
    admit(s1);
    let (s2, c2) = tokio::net::UnixStream::pair().unwrap();
    admit(s2); // shed: server side dropped
    let (rd2, _w2) = c2.into_split();
    let mut lines2 = tokio::io::BufReader::new(rd2).lines();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), lines2.next_line())
            .await
            .expect("shed peer hung")
            .expect("shed peer errored"),
        None,
        "over-cap connection was not shed"
    );
    // The admitted connection is still held, not shed.
    let (rd1, _w1) = c1.into_split();
    let mut lines1 = tokio::io::BufReader::new(rd1).lines();
    assert!(
        tokio::time::timeout(Duration::from_millis(300), lines1.next_line())
            .await
            .is_err(),
        "admitted connection closed early"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn capped_line_reader_refuses_flood() {
    use std::io::Cursor;
    let mut big = vec![b'x'; 100];
    big.push(b'\n');
    assert!(read_capped_line(&mut Cursor::new(big), 16).is_err());
    let mut ok = Cursor::new(b"{\"ok\":true}\n".to_vec());
    assert_eq!(read_capped_line(&mut ok, 16).unwrap(), "{\"ok\":true}\n");
    // Truncated (no newline before EOF within the cap) is rejected
    // rather than returned as a parseable partial line.
    let mut truncated = Cursor::new(b"{\"ok\":true".to_vec());
    assert!(read_capped_line(&mut truncated, 16).is_err());
    // A line whose content is exactly `cap` bytes is rejected
    // (the trailing newline would push it over the cap).
    let mut exact = vec![b'x'; 16];
    exact.push(b'\n');
    assert!(read_capped_line(&mut Cursor::new(exact), 16).is_err());
}

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

#[test]
fn denoise_screen_ignores_spinners_and_counters() {
    // minimax spinner frame + elapsed counter: identical when clean.
    let a = "⠙ Running 13min40s · ⚡ 15092 tok/s\n$ ";
    let b = "⠋ Running 13min44s · ⚡ 15092 tok/s\n$ ";
    assert_eq!(denoise_screen(a), denoise_screen(b));
    assert_eq!(screen_hash(a), screen_hash(b));
    // pi block-meter animation + cost/relative-time creep: identical.
    let c = "[█▆▄▂▁▂ LIVE] · $15.443 · 5s ago\n$ ";
    let d = "[▆█▆▄▂▁ LIVE] · $15.444 · 0s ago\n$ ";
    assert_eq!(denoise_screen(c), denoise_screen(d));
    // Digit-only semantic steps hide too (the summary comparison in
    // track_quiet_merged catches the classifier-visible ones).
    assert_eq!(denoise_screen("1 failed"), denoise_screen("2 failed"));
    // Real changes still differ: new text, new lines, error lines.
    assert_ne!(denoise_screen("ok"), denoise_screen("Error: boom"));
    assert_ne!(denoise_screen("$ "), denoise_screen("done\n$ "));
    assert_ne!(denoise_screen("pass"), denoise_screen("pass\nfail"));
}

#[test]
fn track_quiet_merged_matrix() {
    // Same screen + same summary: the clock grows.
    assert_eq!(
        track_quiet_merged((7, 100), Some(7), "e1", "e1", AgentState::Error, 200),
        (7, 100)
    );
    // Same screen + changed summary: a new failure is activity.
    assert_eq!(
        track_quiet_merged(
            (7, 100),
            Some(7),
            "1 failed",
            "2 failed",
            AgentState::Error,
            200
        ),
        (7, 200)
    );
    // Changed screen: resets regardless of summaries.
    assert_eq!(
        track_quiet_merged((7, 100), Some(9), "e1", "e1", AgentState::Error, 200),
        (9, 200)
    );
    // A wiped summary is decay, not activity.
    assert_eq!(
        track_quiet_merged((7, 100), Some(7), "e1", "", AgentState::Error, 200),
        (7, 100)
    );
    // Stuck rows are exempt: their "no cpu for {dur}" ticker must not
    // read fresh forever.
    assert_eq!(
        track_quiet_merged(
            (7, 100),
            Some(7),
            "no cpu for 10m",
            "no cpu for 11m",
            AgentState::Stuck,
            200
        ),
        (7, 100)
    );
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
fn backward_clock_jump_lapses_hooks_and_drops_carries() {
    // NTP step / suspend / VM restore: pre-jump stamps sit ahead of
    // `now`. Expiry fails toward fresh evidence instead of wedging
    // blind until the clock catches up.
    let now = 10_000;
    let rec = hook_record(now + 3_600); // stamped an hour "ahead"
    let (old, stale) = split_prev(Some(&rec), now, HOOK_STALE_SECS);
    assert!(old.is_none(), "future-stamped hook must lapse");
    assert!(stale.is_some());
    let mut screen = hook_record(now + 3_600);
    screen.source = Source::Screen;
    assert!(
        !carry_record(&screen, &test_pane(), now, HOOK_STALE_SECS),
        "future-stamped carry must drop"
    );
    // Ordinary stamps are untouched by the helper.
    assert_eq!(elapsed_for_expiry(now, now - 60), 60);
    assert_eq!(elapsed_for_expiry(now, now), 0);
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

    let listed: std::collections::HashSet<String> = ["%1".to_string()].into_iter().collect();
    merge_mid_poll_hooks(&mut next, &live, poll_start, Some(&listed));

    assert_eq!(next["%1"].state, AgentState::Waiting);
    assert_eq!(next["%1"].summary, "Approve plan?");
    assert!(!next.contains_key("%2"));
}

#[test]
fn mid_poll_hook_for_unlisted_pane_is_a_ghost() {
    // A fresh Hook record for a pane the poll never listed (dead
    // pane, forged event) must not resurrect into a result rebuilt
    // from live panes — while a Manual pin for the same pane (human
    // state) and any fresh hook under an unknown live set (outage)
    // still merge.
    let poll_start = 50_000;
    let mut next = HashMap::new();
    let mut live = HashMap::new();
    let mut ghost = hook_record(poll_start + 1);
    ghost.pane = "%9".to_string();
    live.insert("%9".to_string(), ghost);
    let mut pin = hook_record(poll_start + 1);
    pin.pane = "%8".to_string();
    pin.source = Source::Manual;
    live.insert("%8".to_string(), pin);
    let listed: std::collections::HashSet<String> = ["%1".to_string()].into_iter().collect();
    merge_mid_poll_hooks(&mut next, &live, poll_start, Some(&listed));
    assert!(!next.contains_key("%9"), "ghost hook resurrected");
    assert!(next.contains_key("%8"), "manual pin must survive");
    // Outage (live set unknown): preserve, never drop.
    merge_mid_poll_hooks(&mut next, &live, poll_start, None);
    assert!(next.contains_key("%9"), "outage must preserve hooks");
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
    use crate::agent::AgentState::{Done, Error, Limited, Stuck, Waiting, Working};
    let screen = Some("● Chrome Bridge (indefinite)");
    // Done rows show what finished, like Idle rows.
    assert_eq!(
        fallback_summary_for(Done, screen),
        "● Chrome Bridge (indefinite)"
    );
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

fn test_pane() -> crate::tmux::Pane {
    crate::tmux::Pane::test("%1", "w", "pi")
}

fn screen_hit(agent: Option<&str>, state: AgentState, summary: &str) -> Option<ScreenHit> {
    Some(ScreenHit {
        agent: agent.map(str::to_string),
        state,
        summary: summary.to_string(),
        phase: None,
    })
}

#[test]
fn hook_summary_is_enriched_and_working_hook_promoted_for_a_question() {
    let mut rec = carried("pi", AgentState::Waiting, 0, 0, 10);
    rec.source = Source::Hook;
    rec.summary = "waiting on custom".to_string();
    let question = screen_hit(
        Some("pi"),
        AgentState::Waiting,
        "Should this be a comparison document only?",
    );
    assert_eq!(
        hook_question_summary(&rec, question.as_ref()),
        Some("Should this be a comparison document only?")
    );

    // No prompt text from the screen, or a different state/source, must
    // not replace the hook's own summary.
    let generic = screen_hit(Some("pi"), AgentState::Waiting, "Enter to select");
    assert_eq!(hook_question_summary(&rec, generic.as_ref()), None);

    // An explicit GLLA picker overrides a stale `turn started` hook too:
    // the picker is blocking even if the hook's current state is Working.
    let mut working = carried("pi", AgentState::Working, 0, 0, 10);
    working.source = Source::Hook;
    let decision = screen_hit(
        Some("pi"),
        AgentState::Waiting,
        "Decision needed — Refresh docs/turso-surface-gap.md",
    );
    assert_eq!(
        hook_question_summary(&working, decision.as_ref()),
        Some("Decision needed — Refresh docs/turso-surface-gap.md")
    );

    rec.source = Source::Proc;
    assert_eq!(hook_question_summary(&rec, question.as_ref()), None);
}

#[test]
fn idle_verdict_keeps_its_completion_line() {
    // Done rows show what finished ("✓ Todo list…"), like waiting
    // rows show the plea — not a blank. And a done screen beats a
    // live process: finished agents keep their REPL open.
    for proc in [None, Some(("minimax-code", 10))] {
        let rec = merge_record(
            None,
            false,
            screen_hit(Some("minimax-code"), AgentState::Idle, "✓ Todo list 9/9"),
            proc,
            &test_pane(),
            100,
            300,
        )
        .expect("screen verdict claims the pane");
        assert_eq!(rec.state, AgentState::Idle, "proc: {proc:?}");
        assert_eq!(rec.summary, "✓ Todo list 9/9");
    }
}

#[test]
fn done_verdict_beats_live_process() {
    // A finished agent with a live REPL must read Done, not Working:
    // the Done screen verdict promotes over the process hit, and the
    // completion line rides along like Idle's.
    for proc in [None, Some(("pi", 10))] {
        let rec = merge_record(
            None,
            false,
            screen_hit(Some("pi"), AgentState::Done, "✓ Todo list 9/9"),
            proc,
            &test_pane(),
            100,
            300,
        )
        .expect("screen verdict claims the pane");
        assert_eq!(rec.state, AgentState::Done, "proc: {proc:?}");
        assert_eq!(rec.summary, "✓ Todo list 9/9");
    }
}

#[test]
fn working_to_done_records_turn_time() {
    // turn_ended covers Done: a finished turn banks its duration
    // instead of recording last_turn_secs = 0.
    assert!(crate::agent::turn_ended(
        AgentState::Working,
        AgentState::Done
    ));
    let now = 100_000;
    let old = carried("pi", AgentState::Working, 50, now - 10, now - 120);
    let rec = merge_record(
        Some(&old),
        false,
        screen_hit(Some("pi"), AgentState::Done, "✓ done"),
        Some(("pi", 60)),
        &test_pane(),
        now,
        300,
    )
    .expect("done merge");
    assert_eq!(rec.state, AgentState::Done);
    assert_eq!(rec.last_turn_secs, 120);
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
    let rec =
        merge_record(None, false, None, Some(("pi", 50)), &ed, now, STUCK_SECS).expect("no record");
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
fn sibling_editor_does_not_wait() {
    // Background pi plus a sibling micro: the editor is not pi's
    // $EDITOR, so pi stays on its own verdict path (Working here).
    let now = 100_000;
    let mut ed = test_pane();
    ed.command = "micro".to_string();
    let rec = merge_record_owned(
        None,
        false,
        None,
        Some(("pi", 50, 100)),
        &ed,
        false,
        now,
        STUCK_SECS,
        QUIET_IDLE_SECS,
        WAITING_IDLE_SECS,
        DONE_IDLE_SECS,
    )
    .expect("no record");
    assert_eq!(rec.agent, "pi");
    assert_eq!(rec.state, AgentState::Working);
    assert_eq!(rec.source, Source::Proc);
}

#[test]
fn owned_editor_still_waits() {
    // micro inside pi's own subtree: pi summoned $EDITOR, awaiting input.
    let now = 100_000;
    let mut ed = test_pane();
    ed.command = "micro".to_string();
    let rec = merge_record_owned(
        None,
        false,
        None,
        Some(("pi", 50, 100)),
        &ed,
        true,
        now,
        STUCK_SECS,
        QUIET_IDLE_SECS,
        WAITING_IDLE_SECS,
        DONE_IDLE_SECS,
    )
    .expect("no record");
    assert_eq!(rec.agent, "pi");
    assert_eq!(rec.state, AgentState::Waiting);
    assert_eq!(rec.source, Source::Proc);
}

#[test]
fn pid_reuse_restarts_still_clock() {
    // Same agent name, same tick sum — but a different starttime, so the
    // pid was recycled: the new process must not inherit the old clock.
    let now = 100_000;
    let mut old = carried("pi", AgentState::Working, 50, now - 600, now - 600);
    old.agent_start = 100;
    let rec = merge_record_owned(
        Some(&old),
        false,
        None,
        Some(("pi", 50, 200)),
        &test_pane(),
        false,
        now,
        STUCK_SECS,
        QUIET_IDLE_SECS,
        WAITING_IDLE_SECS,
        DONE_IDLE_SECS,
    )
    .expect("no record");
    assert_eq!(rec.cpu_ticks, 50);
    assert_eq!(rec.cpu_still_since, now);
    assert_eq!(rec.agent_start, 200);
}

#[test]
fn same_process_keeps_still_clock() {
    // Same agent, same ticks, same starttime: one continuing process.
    let now = 100_000;
    let mut old = carried("pi", AgentState::Working, 50, now - 600, now - 600);
    old.agent_start = 100;
    let rec = merge_record_owned(
        Some(&old),
        false,
        None,
        Some(("pi", 50, 100)),
        &test_pane(),
        false,
        now,
        STUCK_SECS,
        QUIET_IDLE_SECS,
        WAITING_IDLE_SECS,
        DONE_IDLE_SECS,
    )
    .expect("no record");
    assert_eq!(rec.cpu_ticks, 50);
    assert_eq!(rec.cpu_still_since, now - 600);
    assert_eq!(rec.agent_start, 100);
}

#[test]
fn registry_round_trip_is_atomic_and_tolerant() {
    let dir = std::env::temp_dir().join(format!("terhub-reg-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("registry.json");
    // One corrupt record must not sink the good one.
    let good = carried("pi", AgentState::Working, 1, 2, 100);
    let text = format!(
        "[{}, {{\"pane\":42}}]",
        serde_json::to_string(&good).unwrap()
    );
    std::fs::write(&path, &text).unwrap();
    let loaded = load_registry(&path);
    assert_eq!(loaded.len(), 1);
    assert!(loaded.contains_key(&good.pane));
    // Atomic save round-trips and leaves no tmp litter.
    let mut reg = HashMap::new();
    reg.insert(good.pane.clone(), good);
    save_registry(&path, &reg);
    assert_eq!(load_registry(&path).len(), 1);
    assert!(!path.with_extension("json.tmp").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_paused_agent_survives_a_registry_round_trip() {
    // The pauser registry *is* the persisted record: terhub has to
    // remember a pane it paused across a daemon restart, or the
    // pane comes back as "stuck" the moment the daemon bounces.
    let dir = std::env::temp_dir().join(format!("terhub-reg-paused-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("registry.json");
    let mut rec = carried("pi", AgentState::Working, 1, 2, 100);
    apply_paused(&mut rec, true, 100, None);
    assert_eq!(rec.state, AgentState::Paused);
    let mut reg = HashMap::new();
    let pane = rec.pane.clone();
    reg.insert(pane.clone(), rec);
    save_registry(&path, &reg);
    // The mark is on disk, in the readable spelling.
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(
        raw.contains("\"paused\""),
        "registry must carry the paused mark"
    );
    // And it comes back as paused, not as a dropped record.
    let loaded = load_registry(&path);
    let back = loaded
        .get(&pane)
        .expect("the paused record must survive the round trip");
    assert_eq!(back.state, AgentState::Paused);
    assert_eq!(back.summary, "paused");
    assert!(
        !back.state.needs_attention(),
        "a remembered pause must not come back as an alarm"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn repeated_saves_leave_no_tmp_litter() {
    // Back-to-back saves can share a nanos tick: the attempt counter
    // keeps every tmp name unique, and each rename consumes its tmp.
    let dir = std::env::temp_dir().join(format!("terhub-reg-nolitter-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("registry.json");
    let mut reg = HashMap::new();
    reg.insert(
        "%1".to_string(),
        carried("pi", AgentState::Working, 1, 2, 100),
    );
    save_registry(&path, &reg);
    save_registry(&path, &reg);
    save_registry(&path, &reg);
    let names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["registry.json".to_string()]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn stale_save_tmps_swept_but_live_ones_kept() {
    // The sweep is mtime-vs-start: drive it directly with explicit
    // starts so the test never depends on wall-clock ordering.
    let dir = std::env::temp_dir().join(format!("terhub-reg-sweep-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("registry.json");
    let pid = std::process::id();
    let own = dir.join(format!("registry.tmp.{pid}.999"));
    let foreign = dir.join(format!("registry.tmp.{}.999", pid.wrapping_add(1)));
    std::fs::write(&own, b"stale").unwrap();
    std::fs::write(&foreign, b"stale").unwrap();
    // Both predate a future start: only our pid is swept.
    sweep_stale_tmps(&path, pid, now_unix() + 3600);
    assert!(!own.exists(), "own stale tmp not swept");
    assert!(foreign.exists(), "foreign tmp must never be touched");
    // A file newer than the start (a live peer's in-flight tmp)
    // is kept even on our pid.
    std::fs::write(&own, b"live").unwrap();
    sweep_stale_tmps(&path, pid, 0);
    assert!(own.exists(), "live tmp must never be touched");
    let _ = std::fs::remove_dir_all(&dir);
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
fn a_paused_agent_is_not_reported_as_stuck() {
    // The false alarm this fixes: a stopped process burns no CPU,
    // so the stuck gate says "hung" — and terhub's own `z` is what
    // stopped it. The pause is the better explanation, so it wins.
    let now = 100_000;
    let old = carried("pi", AgentState::Working, 500, now - 601, now - 601);
    let mut rec = merge_record(
        Some(&old),
        false,
        None,
        Some(("pi", 500)),
        &test_pane(),
        now,
        STUCK_SECS,
    )
    .expect("no record");
    // Without the pause the gate does fire — that is the bug.
    assert_eq!(rec.state, AgentState::Stuck);
    // With the pause it does not.
    apply_paused(&mut rec, true, now, None);
    assert_eq!(rec.state, AgentState::Paused);
    assert_eq!(rec.summary, "paused");
    assert!(
        !rec.state.needs_attention(),
        "a paused agent needs nobody: it must not escalate"
    );
}

#[test]
fn the_pause_clock_survives_the_next_poll() {
    // The bug this catches: the merge re-derives the state *and its
    // clock* from live evidence, so a naive "reset the clock when
    // the state changes" would restart the pause every poll and
    // "paused for 2h" would read "paused for 2s".
    let now = 100_000;
    // Poll 1: the pane is stopped, the merge says "stuck".
    let old = carried("pi", AgentState::Working, 500, now - 601, now - 601);
    let mut rec = merge_record(
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
    apply_paused(&mut rec, true, now, None);
    assert_eq!(rec.state_since, now, "the pause starts its own clock");

    // Poll 2: the same thing happens again, an hour later. The
    // carried pause clock has to survive it.
    let later = now + 3600;
    let prev_pause = carried_pause_since(Some(&rec));
    assert_eq!(prev_pause, Some(now), "the paused clock is carried in");
    let mut rec2 = merge_record(
        Some(&rec),
        false,
        None,
        Some(("pi", 500)),
        &test_pane(),
        later,
        STUCK_SECS,
    )
    .expect("no record");
    // The merge re-derived the state and its own clock...
    assert_eq!(rec2.state, AgentState::Stuck);
    assert_eq!(rec2.state_since, later, "the merge reset the clock");
    // ...but the pause must put the original one back.
    apply_paused(&mut rec2, true, later, prev_pause);
    assert_eq!(rec2.state, AgentState::Paused);
    assert_eq!(
        rec2.state_since, now,
        "an hour later, the pause is still an hour old"
    );
}

#[test]
fn a_pause_that_was_not_carried_starts_its_own_clock() {
    // A pane stopped between polls: there is no carried clock, so
    // the pause starts now. That is the best answer available —
    // the moment of the stop is not observable, only the moment of
    // the sighting.
    let now = 100_000;
    let mut rec = carried("pi", AgentState::Working, 500, now - 10, now - 10);
    assert_eq!(carried_pause_since(Some(&rec)), None);
    apply_paused(&mut rec, true, now, None);
    assert_eq!(rec.state, AgentState::Paused);
    assert_eq!(rec.state_since, now);
}

#[test]
fn a_resumed_agent_is_not_immediately_reported_as_stuck() {
    // The false alarm this catches: while the agent is frozen it
    // burns no CPU, so the still-clock ages for the whole pause. On
    // resume the first poll can still see the same tick sum, and
    // the stuck gate would fire on a clock that measured the
    // *freeze* — reporting a hang that never happened, one poll
    // after the pause the user deliberately ended.
    let now = 100_000;
    let mut paused = carried("pi", AgentState::Working, 500, now - 601, now - 601);
    apply_paused(&mut paused, true, now, None);
    assert_eq!(paused.state, AgentState::Paused);
    // The pause ages the clock: the frozen agent burned nothing.
    paused.cpu_still_since = now - 601;
    // The resume: the pane is running again.
    let mut resumed = paused.clone();
    clear_pause_silence(&mut resumed, false);
    assert_eq!(
        resumed.cpu_still_since, 0,
        "the freeze's silence must not be inherited"
    );
    // And the merge must not call that a hang.
    let rec = merge_record(
        Some(&resumed),
        false,
        None,
        Some(("pi", 500)),
        &test_pane(),
        now,
        STUCK_SECS,
    )
    .expect("no record");
    assert_ne!(
        rec.state,
        AgentState::Stuck,
        "a resumed agent is working again, not hung"
    );
}

#[test]
fn a_pause_that_is_still_on_keeps_its_silence() {
    // The other half: while the pane is still paused, the clock is
    // the clock — clearing it would restart it every poll and the
    // paused duration would never age.
    let now = 100_000;
    let mut rec = carried("pi", AgentState::Working, 500, now - 601, now - 601);
    apply_paused(&mut rec, true, now, None);
    rec.cpu_still_since = now - 601;
    clear_pause_silence(&mut rec, true);
    assert_eq!(rec.cpu_still_since, now - 601);
}

#[test]
fn a_resumed_agent_keeps_its_real_state() {
    // The pause is read from the process state every poll and
    // never carried, so a resumed pane is working again at once.
    let now = 100_000;
    let old = carried("pi", AgentState::Working, 500, now - 601, now - 601);
    let mut rec = merge_record(
        Some(&old),
        false,
        None,
        Some(("pi", 500)),
        &test_pane(),
        now,
        STUCK_SECS,
    )
    .expect("no record");
    apply_paused(&mut rec, false, now, None);
    assert_eq!(rec.state, AgentState::Stuck);
    assert_ne!(rec.summary, "paused");
}

#[test]
fn pausing_starts_its_own_clock() {
    let now = 100_000;
    let mut rec = carried("pi", AgentState::Working, 500, now - 10, now - 10);
    apply_paused(&mut rec, true, now, None);
    assert_eq!(rec.state, AgentState::Paused);
    assert_eq!(rec.state_since, now, "the pause starts the state clock");
    // A second poll keeps the original clock, not a fresh one: the
    // caller hands back what the record carried.
    let prev = carried_pause_since(Some(&rec));
    assert_eq!(prev, Some(now));
    apply_paused(&mut rec, true, now + 30, prev);
    assert_eq!(rec.state_since, now);
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
fn hook_carry_trips_stuck_and_recovers() {
    // The Hook|Manual carry branch runs the shared stuck gate: a hung
    // hook-claimed pane trips Stuck (saving the hook's verdict), keeps
    // its clock while still, and restores the verdict when CPU
    // recovers — instead of reading Working until the 30-minute lapse
    // or latching Stuck forever.
    let now = 100_000;
    let mut r = hook_record(now - 1800);
    r.state = AgentState::Working;
    r.cpu_ticks = 500;
    r.cpu_still_since = now - 601;
    r.quiet_since = now; // screen fresh: a hang, not finished
    apply_hook_stuck_gate(&mut r, true, now, STUCK_SECS, 3600);
    assert_eq!(r.state, AgentState::Stuck);
    assert_eq!(r.stuck_base, Some(AgentState::Working));
    assert_eq!(r.state_since, now);
    assert_eq!(r.summary, "no cpu for 10m");
    // Still still: clock kept, no renotify.
    let mut r2 = r.clone();
    apply_hook_stuck_gate(&mut r2, true, now + 60, STUCK_SECS, 3600);
    assert_eq!(r2.state, AgentState::Stuck);
    assert_eq!(r2.state_since, now);
    // CPU recovered: hook's verdict restored, flip clocked, trip
    // summary blanked for the screen fallback.
    r2.cpu_still_since = now + 60;
    apply_hook_stuck_gate(&mut r2, true, now + 60, STUCK_SECS, 3600);
    assert_eq!(r2.state, AgentState::Working);
    assert_eq!(r2.stuck_base, None);
    assert_eq!(r2.state_since, now + 60);
    assert!(r2.summary.is_empty());
}

#[test]
fn hook_carry_stuck_gate_respects_exemptions() {
    let now = 100_000;
    let mut hung = hook_record(now - 1800);
    hung.state = AgentState::Working;
    hung.cpu_ticks = 500;
    hung.cpu_still_since = now - 601;
    // Long-quiet screen: finished, not hung.
    hung.quiet_since = now - 7200;
    apply_hook_stuck_gate(&mut hung, true, now, STUCK_SECS, 3600);
    assert_eq!(hung.state, AgentState::Working);
    // Disabled gate: never trips.
    hung.quiet_since = now;
    apply_hook_stuck_gate(&mut hung, true, now, 0, 3600);
    assert_eq!(hung.state, AgentState::Working);
    // No live process: never trips.
    apply_hook_stuck_gate(&mut hung, false, now, STUCK_SECS, 3600);
    assert_eq!(hung.state, AgentState::Working);
    // A hook-claimed Stuck (no trip base) is never touched.
    let mut claimed = hook_record(now - 100);
    claimed.state = AgentState::Stuck;
    claimed.cpu_still_since = now - 601;
    apply_hook_stuck_gate(&mut claimed, true, now, STUCK_SECS, 3600);
    assert_eq!(claimed.state, AgentState::Stuck);
    assert_eq!(claimed.stuck_base, None);
}

#[test]
fn the_merge_never_carries_bell_evidence() {
    // The contract on the field is "reset every poll (never
    // carried)": the bell is re-derived from the fresh screen
    // evidence, so a bell that rang in an earlier poll must not
    // keep claiming the pane is ringing now.
    let mut old = carried("pi", AgentState::Waiting, 10, 90, 50);
    old.bell = true;
    old.bell_reason = "y/N".to_string();
    let rec = merge_record(
        Some(&old),
        false,
        None,
        Some(("pi", 10)),
        &test_pane(),
        100,
        300,
    )
    .expect("proc hit claims the pane");
    assert!(!rec.bell, "the merge carried a stale bell");
    assert!(
        rec.bell_reason.is_empty(),
        "the merge carried a stale bell reason: {:?}",
        rec.bell_reason
    );
}

#[test]
fn bell_evidence_mirrors_actionable_flag_only() {
    // Flag + classifier upgrade (🔔 prefix) → recordable with reason.
    let hit = screen_hit(Some("pi"), AgentState::Waiting, "🔔 Approve? [y/n]");
    assert_eq!(
        bell_evidence(hit.as_ref(), true),
        (true, "Approve? [y/n]".to_string())
    );
    // Bare bell with no manifest hit is shell noise: stays clear.
    assert_eq!(bell_evidence(None, true), (false, String::new()));
    // No flag: stays clear even with a waiting hit on screen.
    let hit = screen_hit(Some("pi"), AgentState::Waiting, "Approve? [y/n]");
    assert_eq!(bell_evidence(hit.as_ref(), false), (false, String::new()));
    // Flag but no upgrade (bell_override kept Error): stays clear —
    // the verdict, not the bell, is the evidence.
    let hit = screen_hit(Some("pi"), AgentState::Error, "Traceback");
    assert_eq!(bell_evidence(hit.as_ref(), true), (false, String::new()));
}

#[test]
fn hook_idle_yields_to_live_screen_evidence() {
    // Live bug: pi's `agent_end` fires when the main turn ends, but
    // retry loops, detached audit workers, and background subagents
    // keep going after it. A hook idle with a live Working screen
    // (same agent, process-confirmed) promotes instead of carrying
    // idle forward.
    use crate::daemon::poll::hook_idle_promotion;
    let now = 100_000;
    let mut idle_hook = hook_record(now - 60);
    idle_hook.agent = "pi".to_string();
    idle_hook.state = AgentState::Idle;
    idle_hook.source = Source::Hook;
    // Anonymous Working hit + same-agent process: promote, and the
    // working summary is the phase (what it is doing), not a spinner.
    let mut hit = screen_hit(None, AgentState::Working, "── ⠴ Working ──");
    hit.as_mut().unwrap().phase = Some("── ⠴ Working ──".to_string());
    let got = hook_idle_promotion(&idle_hook, hit.as_ref(), Some("pi"), now);
    assert_eq!(
        got,
        Some((AgentState::Working, now, "── ⠴ Working ──".to_string())),
        "live work must override a stale hook idle"
    );
    // Attention verdicts promote with their own line as the summary.
    let hit = screen_hit(None, AgentState::Limited, "Error: 429 quota exceeded");
    let got = hook_idle_promotion(&idle_hook, hit.as_ref(), Some("pi"), now);
    assert_eq!(
        got.map(|(s, _, _)| s),
        Some(AgentState::Limited),
        "a quota wall must not hide behind hook idle"
    );
    // No process behind an anonymous hit: no promotion (a micro `>`
    // bar must never impersonate an agent).
    let hit = screen_hit(None, AgentState::Working, "── ⠴ Working ──");
    assert_eq!(
        hook_idle_promotion(&idle_hook, hit.as_ref(), None, now),
        None,
        "anonymous work without a process must not promote"
    );
    // A named hit for the same agent promotes without a process.
    let hit = screen_hit(Some("pi"), AgentState::Working, "── ⠴ Working ──");
    assert!(
        hook_idle_promotion(&idle_hook, hit.as_ref(), None, now).is_some(),
        "a named same-agent hit is self-confirming"
    );
    // A hit naming a different agent never promotes (replacement
    // handling owns that case, not this one).
    let hit = screen_hit(Some("claude"), AgentState::Working, "loading");
    assert_eq!(
        hook_idle_promotion(&idle_hook, hit.as_ref(), Some("pi"), now),
        None,
        "a foreign-agent hit must not promote a pi idle"
    );
    // Non-idle hooks are untouched: `turn_start` working stays
    // authoritative even over a quiet screen.
    let mut working_hook = idle_hook.clone();
    working_hook.state = AgentState::Working;
    let hit = screen_hit(None, AgentState::Idle, "done");
    assert_eq!(
        hook_idle_promotion(&working_hook, hit.as_ref(), Some("pi"), now),
        None,
        "hook working must never demote here"
    );
    // Idle/Done screens never promote (nothing to promote to).
    let hit = screen_hit(None, AgentState::Idle, "done");
    assert_eq!(
        hook_idle_promotion(&idle_hook, hit.as_ref(), Some("pi"), now),
        None
    );
    assert_eq!(hook_idle_promotion(&idle_hook, None, Some("pi"), now), None);
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
