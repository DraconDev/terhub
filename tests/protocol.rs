//! The protocol contract, end to end over a real socket.
//!
//! One test function: it owns TERHUB_RUNTIME_DIR for this test binary,
//! and the fake daemons it binds need that same directory.
//!
//! The failure this suite exists to prevent is precise and was real: a
//! client that predates the `paused` agent state could not decode the
//! daemon's snapshot, and every client error path collapsed that into
//! "daemon not running" — so a healthy daemon serving 23 agents looked
//! like no daemon at all, and every pane in the TUI went blank. The
//! daemon now versions the protocol and answers a pre-versioned client
//! in a shape that client can still read.
mod common;

use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;
use terhub::agent::AgentState;
use terhub::daemon::{self, Request};

/// Ask the daemon something the old way: one line of JSON, no
/// protocol version — exactly what a pre-versioned client sends.
fn raw_request(socket: &Path, line: &str) -> String {
    let mut stream = std::os::unix::net::UnixStream::connect(socket).expect("connect to daemon");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set timeout");
    stream.write_all(line.as_bytes()).expect("write request");
    stream.write_all(b"\n").expect("write newline");
    stream.flush().expect("flush");
    let mut out = String::new();
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                out.push_str(&String::from_utf8_lossy(&buf[..n]));
                if out.contains('\n') {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    out
}

/// A daemon that answers every request with one canned line.
fn fake_daemon(runtime: &Path, payload: &str) -> std::thread::JoinHandle<()> {
    let sock = runtime.join("terhubd.sock");
    let _ = std::fs::remove_file(&sock);
    let listener = std::os::unix::net::UnixListener::bind(&sock).expect("bind fake daemon");
    let payload = payload.to_string();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(payload.as_bytes());
            let _ = stream.write_all(b"\n");
        }
    })
}

#[test]
fn mixed_builds_keep_working() {
    let sandbox = common::Sandbox::new("protocol");
    let socket = daemon::socket_path();

    // --- a versioned client sees the true state -------------------------
    {
        let daemon_guard = common::TestDaemon::start();
        let resp = daemon::send_request(
            &Request::Register {
                pane: "%77".to_string(),
                agent: "pi".to_string(),
                pid: 4242,
                state: AgentState::Paused,
            },
            Duration::from_secs(2),
        )
        .expect("register");
        assert!(resp.ok, "register rejected: {:?}", resp.error);

        let snap = daemon::fetch_snapshot(Duration::from_secs(2)).expect("snapshot");
        let pane = snap
            .agents
            .iter()
            .find(|a| a.pane == "%77")
            .expect("registered pane is listed");
        assert_eq!(
            pane.state,
            AgentState::Paused,
            "a versioned client gets the true state"
        );

        // The daemon names its protocol version and its build, so a
        // client can tell what it is talking to.
        let detailed = daemon::fetch_snapshot_detailed(Duration::from_secs(2)).expect("detailed");
        assert_eq!(detailed.daemon_proto, Some(daemon::PROTO_VERSION));
        assert_eq!(
            detailed.daemon_build.as_deref(),
            Some(daemon::build_label().as_str())
        );
        assert!(
            daemon::compat_note(&detailed).is_none(),
            "matching builds report nothing"
        );
        assert!(daemon::build_note(&detailed).is_none());

        // --- a pre-versioned client still gets a usable answer ------------
        let raw = raw_request(&socket, r#"{"cmd":"snapshot"}"#);
        let value: serde_json::Value = serde_json::from_str(raw.trim()).expect("valid json");
        let agents = value["snapshot"]["agents"].as_array().expect("agents");
        let legacy = agents
            .iter()
            .find(|a| a["pane"] == "%77")
            .expect("the pane is listed for a legacy client too");
        assert_eq!(
            legacy["state"], "idle",
            "a legacy client cannot decode 'paused'; it gets the quiet legacy state"
        );
        // Every state in a legacy answer must be one a legacy client
        // knows — otherwise the old client is broken again.
        for agent in agents {
            let state = agent["state"].as_str().unwrap_or("unknown");
            assert!(
                [
                    "unknown", "working", "waiting", "error", "done", "idle", "stuck", "limited"
                ]
                .contains(&state),
                "legacy answer carries a state a legacy client cannot decode: {state}"
            );
        }
        drop(daemon_guard);
    }

    // --- a daemon that speaks another version is named, not hidden ------
    {
        let payload =
            r#"{"ok":true,"snapshot":{"agents":[],"updated_at":2},"proto":99,"build":"9.9.9+zzz"}"#;
        let _fake = fake_daemon(&sandbox.runtime, payload);
        let resp = daemon::fetch_snapshot_detailed(Duration::from_secs(2)).expect("snapshot");
        let note = daemon::compat_note(&resp).expect("a version mismatch must be reported");
        assert!(note.contains("client proto 1"), "{note}");
        assert!(note.contains("daemon proto 99"), "{note}");
        assert!(note.contains("make build"), "{note}");
        let build = daemon::build_note(&resp).expect("a build mismatch must be reported");
        assert!(build.contains("9.9.9+zzz"), "{build}");
    }

    // --- a daemon that answers but cannot be read is not "not running" ---
    {
        // A field of the wrong type: the answer is unusable, but a
        // daemon did answer, and the message has to say so — "start a
        // daemon" is the wrong advice here.
        let payload = r#"{"ok":true,"snapshot":{"agents":[{"pane":"%1","agent":"pi","state":"working","source":"proc","updated_at":1,"state_since":1,"ctx_pct":"high"}],"updated_at":2},"proto":1,"build":"0.2.3+abc"}"#;
        let _fake = fake_daemon(&sandbox.runtime, payload);
        let err = daemon::fetch_snapshot(Duration::from_secs(2)).expect_err("must not decode");
        let msg = err.message();
        assert!(
            !msg.contains("daemon not running"),
            "a daemon that answered must not be reported as absent: {msg}"
        );
        assert!(
            msg.contains("daemon running but the response could not be read"),
            "{msg}"
        );
        assert!(
            !err.is_unreachable(),
            "only a missing daemon is unreachable"
        );
    }

    // --- no daemon at all is the one case that says "not running" -------
    {
        let _ = std::fs::remove_file(daemon::socket_path());
        let err = daemon::fetch_snapshot(Duration::from_millis(300)).expect_err("no daemon");
        assert!(err.is_unreachable(), "a missing daemon is unreachable");
        assert!(
            err.message().contains("daemon not running"),
            "{}",
            err.message()
        );
    }
}
