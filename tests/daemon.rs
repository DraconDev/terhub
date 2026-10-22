//! End-to-end daemon test: spawn terhubd on a temp socket, push a hook event,
//! read it back, watch the poll expire a dead pane, shut down.
//!
//! Single test function: it owns TERHUB_RUNTIME_DIR for this test binary.
use std::process::{Child, Command};
use std::time::Duration;
use terhub::agent::{AgentState, HookEvent, Source};
use terhub::daemon::{self, Request};

fn wait_for(cond: impl Fn() -> bool, tries: u32) -> bool {
    for _ in 0..tries {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    cond()
}

struct TestDaemon {
    child: Child,
}

impl TestDaemon {
    fn start() -> Self {
        let bin = env!("CARGO_BIN_EXE_terhubd");
        let child = Command::new(bin)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn terhubd");
        assert!(
            wait_for(daemon::daemon_alive, 100),
            "daemon did not come up"
        );
        TestDaemon { child }
    }
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        let _ = daemon::send_request(&Request::Shutdown, Duration::from_secs(1));
        for _ in 0..50 {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                _ => std::thread::sleep(Duration::from_millis(100)),
            }
        }
        let _ = self.child.kill();
    }
}

#[test]
fn daemon_hook_snapshot_expire_shutdown() {
    // Fail loudly without tmux (the backend.rs house rule): a silent
    // early return reports "passed" with zero assertions executed.
    // CI installs tmux; see .github/workflows/ci.yml.
    assert!(
        terhub::tmux::Tmux::available(),
        "tmux is required for this suite (CI installs it)"
    );
    let runtime = std::env::temp_dir().join(format!("terhubd-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&runtime);
    std::fs::create_dir_all(&runtime).unwrap();
    let data = std::env::temp_dir().join(format!("terhubd-data-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    // Point at a missing config so the daemon runs on defaults no matter
    // what the developer tuned locally (a custom poll_secs would skew the
    // expiry waits below).
    let config = std::env::temp_dir().join(format!("terhubd-cfg-{}.toml", std::process::id()));
    let _ = std::fs::remove_file(&config);
    // SAFETY: this test binary runs this single test; nothing else reads the vars.
    unsafe {
        std::env::set_var("TERHUB_RUNTIME_DIR", &runtime);
        std::env::set_var("TERHUB_DATA_DIR", &data);
        std::env::set_var("TERHUB_CONFIG", &config);
        // Hermetic tmux server for this binary, regardless of the
        // runner's environment. CI does not pre-set this, and a
        // developer's active `wez` server used to be touched by
        // these tests (the 2026-10-08/09 residual gap).
        std::env::set_var(
            terhub::tmux::SOCKET_ENV,
            format!("terhub-daemon-{}", std::process::id()),
        );
    }
    let _daemon = TestDaemon::start();

    // Bootstrap a real tmux server on this client's socket: the poll can
    // only expire stale records when `list-panes` answers, and the test
    // must never depend on the operator's ambient sessions. With
    // `TERHUB_TMUX_SOCKET` unset this is a throwaway session on the
    // default server (the backend tests already create those); set, the
    // whole run is isolated on a private socket.
    let boot = terhub::tmux::Tmux::new().unwrap();
    let boot_name = format!("terhub-daemon-{}", std::process::id());
    if boot.session_exists(&boot_name) {
        boot.kill_session(&boot_name).unwrap();
    }
    boot.new_session(&boot_name, None, None).unwrap();

    // Hook event round-trips into the snapshot.
    let ev = HookEvent {
        pane: "%999999".to_string(),
        agent: "claude".to_string(),
        state: AgentState::Waiting,
        summary: "needs input".to_string(),
        prompt: "do the thing".to_string(),
        model: String::new(),
        ctx: None,
    };
    let t0 = std::time::Instant::now();
    daemon::send_event(&ev, Duration::from_secs(2)).unwrap();
    let t1 = std::time::Instant::now();
    let snap = daemon::fetch_snapshot(Duration::from_secs(2)).unwrap();
    let t2 = std::time::Instant::now();
    eprintln!("TIMING send={:?} fetch={:?} agents={:?}", t1 - t0, t2 - t1, snap.agents.iter().map(|a| a.pane.clone()).collect::<Vec<_>>());
    let rec = snap
        .agents
        .iter()
        .find(|a| a.pane == "%999999")
        .expect("hook record present");
    assert_eq!(rec.agent, "claude");
    assert_eq!(rec.state, AgentState::Waiting);
    assert_eq!(rec.source, Source::Hook);
    assert_eq!(rec.summary, "needs input");
    assert_eq!(rec.last_prompt, "do the thing");
    assert!(rec.last_prompt_at > 0);
    // Session-less hook records mint the bare agent handle.
    assert_eq!(rec.handle, "@claude");
    assert!(runtime.join("registry.json").is_file());
    // The transition was appended to the ledger.
    let ledger = data.join("activity.jsonl");
    assert!(ledger.is_file(), "ledger missing");
    let text = std::fs::read_to_string(&ledger).unwrap();
    assert!(text.contains("%999999"), "ledger lacks hook:\n{text}");
    assert!(
        text.contains("\"to\":\"waiting\""),
        "ledger lacks state:\n{text}"
    );
    // So was the prompt, as its own entry kind.
    assert!(
        text.contains("\"kind\":\"prompt\""),
        "ledger lacks prompt:\n{text}"
    );
    assert!(text.contains("do the thing"), "prompt text lost:\n{text}");

    // Manual pins round-trip on a real pane (skipped when tmux has
    // no panes at all — the daemon unit tests cover the logic).
    let tmux = terhub::tmux::Tmux::new().unwrap();
    let pinned: Option<String> = tmux
        .list_all_panes()
        .unwrap_or_default()
        .first()
        .map(|p| p.id.clone());
    if let Some(target) = pinned.clone() {
        let me = std::process::id();
        daemon::send_request(
            &Request::Register {
                pane: target.clone(),
                agent: "myagent".to_string(),
                pid: me,
                state: AgentState::Working,
            },
            Duration::from_secs(2),
        )
        .unwrap();
        let snap = daemon::fetch_snapshot(Duration::from_secs(2)).unwrap();
        let rec = snap
            .agents
            .iter()
            .find(|a| a.pane == target)
            .expect("manual pin present");
        assert_eq!(rec.source, Source::Manual);
        assert_eq!(rec.reg_pid, me);
        assert_eq!(rec.handle, "@myagent");
    }

    // The poll notices %999999 is not a real pane and expires it.
    assert!(
        wait_for(
            || {
                daemon::fetch_snapshot(Duration::from_secs(2))
                    .map(|s| s.agents.iter().all(|a| a.pane != "%999999"))
                    .unwrap_or(false)
            },
            60,
        ),
        "dead pane was not expired by the poll"
    );

    // A poll ran (the expiry above proves it): the manual pin on the
    // live pane survived it, and unregister drops it.
    if let Some(target) = pinned {
        let snap = daemon::fetch_snapshot(Duration::from_secs(2)).unwrap();
        assert!(
            snap.agents.iter().any(|a| a.pane == target),
            "manual pin did not survive the poll"
        );
        let resp = daemon::send_request(
            &Request::Unregister {
                pane: target.clone(),
            },
            Duration::from_secs(2),
        )
        .unwrap();
        assert!(resp.ok, "unregister failed: {resp:?}");
        let snap = daemon::fetch_snapshot(Duration::from_secs(2)).unwrap();
        assert!(
            snap.agents.iter().all(|a| a.pane != target),
            "unregister did not drop the pin"
        );
    }

    // Clean shutdown removes the socket. The daemon can go unresponsive
    // a moment before the file is unlinked: a ping is a 300ms request,
    // so under load it can time out while the daemon is still draining
    // the poll. Wait for the socket to actually go instead of asserting
    // on the first unresponsive answer — otherwise this asserts on a
    // race and fails for the wrong reason.
    daemon::send_request(&Request::Shutdown, Duration::from_secs(2)).unwrap();
    assert!(
        wait_for(|| !daemon::daemon_alive(), 50),
        "daemon did not stop"
    );
    assert!(
        wait_for(|| !daemon::socket_path().exists(), 50),
        "stale socket left behind"
    );

    // A hook record older than the staleness window lapses on restart: the
    // poll must not treat it as authoritative forever.
    let tmux = terhub::tmux::Tmux::new().unwrap();
    let name = format!("terhub-lapse-{}", std::process::id());
    if tmux.session_exists(&name) {
        tmux.kill_session(&name).unwrap();
    }
    tmux.new_session(&name, None, None).unwrap();
    tmux.split_window(&name).unwrap();
    let mut panes = tmux.list_panes(&name).unwrap();
    assert_eq!(panes.len(), 2);
    let pane_b = panes.pop().unwrap().id;
    let pane_a = panes.pop().unwrap().id;
    let old = terhub::agent::now_unix() - daemon::HOOK_STALE_SECS - 60;
    let mk = |pane: &str, source| terhub::agent::AgentRecord {
        pane: pane.to_string(),
        agent: "claude".to_string(),
        state: AgentState::Waiting,
        source,
        summary: "rotten".to_string(),
        session: name.clone(),
        window: "w".to_string(),
        command: "zsh".to_string(),
        updated_at: old,
        state_since: old,
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
    std::fs::write(
        runtime.join("registry.json"),
        serde_json::to_string(&[mk(&pane_a, Source::Hook), mk(&pane_b, Source::Screen)]).unwrap(),
    )
    .unwrap();
    let _daemon2 = TestDaemon::start();
    // Idle shells, no screen match: the stale hook lapses and the stale
    // screen record decays instead of showing rotten states forever.
    assert!(
        wait_for(
            || {
                daemon::fetch_snapshot(Duration::from_secs(2))
                    .map(|s| {
                        s.agents
                            .iter()
                            .all(|a| a.pane != pane_a && a.pane != pane_b)
                    })
                    .unwrap_or(false)
            },
            80,
        ),
        "stale records did not lapse"
    );
    tmux.kill_session(&name).unwrap();
    let _ = boot.kill_session(&boot_name);

    let _ = std::fs::remove_dir_all(&runtime);
    let _ = std::fs::remove_dir_all(&data);
}
