//! MCP stdio conversation test: framed initialize/list/call against `terhub
//! mcp`, first without a daemon (graceful errors), then with one (live data).
//!
//! Single test: owns TERHUB_RUNTIME_DIR for this binary.
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

struct Mcp {
    child: Child,
    stdin: ChildStdin,
    /// Shared with the read thread so the per-call deadline (recv_timeout)
    /// can interrupt a wedged server. A plain `BufReader` is not `Send`
    /// for this pattern; wrapping in `Arc<Mutex<_>>` lets the read
    /// thread take the lock once and stream both header lines and the
    /// body without holding the main thread.
    reader: Arc<Mutex<BufReader<std::process::ChildStdout>>>,
}

impl Mcp {
    fn spawn() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_terhub"))
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn terhub mcp");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        Mcp {
            child,
            stdin,
            reader: Arc::new(Mutex::new(BufReader::new(stdout))),
        }
    }

    fn call(&mut self, id: u64, method: &str, params: Value) -> Value {
        let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let text = serde_json::to_string(&body).unwrap();
        write!(self.stdin, "Content-Length: {}\r\n\r\n{text}", text.len()).unwrap();
        self.stdin.flush().unwrap();
        self.read_response()
    }

    fn read_response(&mut self) -> Value {
        // 5s is generous for loopback stdio: the test asserts a fast
        // round-trip, and a wedged child used to hang the suite here.
        self.read_response_timeout(5)
    }

    fn read_response_timeout(&mut self, secs: u64) -> Value {
        let reader = Arc::clone(&self.reader);
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut guard = match reader.lock() {
                Ok(g) => g,
                Err(e) => {
                    let _ = tx.send(Err(format!("reader mutex poisoned: {e}")));
                    return;
                }
            };
            let mut len: Option<usize> = None;
            loop {
                let mut line = String::new();
                if let Err(e) = guard.read_line(&mut line) {
                    let _ = tx.send(Err(format!("read header: {e}")));
                    return;
                }
                if line.trim().is_empty() {
                    break;
                }
                if let Some((k, v)) = line.split_once(':')
                    && k.trim().eq_ignore_ascii_case("content-length")
                {
                    len = v.trim().parse().ok();
                }
            }
            let n = match len {
                Some(n) => n,
                None => {
                    let _ = tx.send(Err("framed response missing Content-Length".into()));
                    return;
                }
            };
            let mut buf = vec![0u8; n];
            if let Err(e) = guard.read_exact(&mut buf) {
                let _ = tx.send(Err(format!("read body: {e}")));
                return;
            }
            match serde_json::from_slice(&buf) {
                Ok(v) => {
                    let _ = tx.send(Ok(v));
                }
                Err(e) => {
                    let _ = tx.send(Err(format!("parse: {e}")));
                }
            }
        });
        match rx.recv_timeout(Duration::from_secs(secs)).unwrap_or_else(|_| {
            panic!("terhub mcp did not respond within {secs}s")
        }) {
            Ok(v) => v,
            Err(e) => panic!("mcp read_response: {e}"),
        }
    }

    fn stop(mut self) {
        drop(self.stdin);
        for _ in 0..50 {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                _ => std::thread::sleep(Duration::from_millis(100)),
            }
        }
        let _ = self.child.kill();
    }
}

fn wait_for(cond: impl Fn() -> bool, tries: u32) -> bool {
    for _ in 0..tries {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    cond()
}

#[test]
fn mcp_conversation_without_and_with_daemon() {
    // Fail loudly without tmux (the backend.rs house rule): a silent
    // early return reports "passed" with zero assertions executed.
    // CI installs tmux; see .github/workflows/ci.yml.
    assert!(
        terhub::tmux::Tmux::available(),
        "tmux is required for this suite (CI installs it)"
    );
    let runtime = std::env::temp_dir().join(format!("terhub-mcp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&runtime);
    std::fs::create_dir_all(&runtime).unwrap();
    // SAFETY: single test in this binary.
    unsafe {
        std::env::set_var("TERHUB_RUNTIME_DIR", &runtime);
    }

    let mut mcp = Mcp::spawn();
    let init = mcp.call(1, "initialize", json!({}));
    assert_eq!(init["result"]["serverInfo"]["name"], "terhub");
    assert_eq!(init["id"], 1);

    let list = mcp.call(2, "tools/list", json!({}));
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec![
            "terhub_status",
            "terhub_capture",
            "terhub_send",
            "terhub_wait",
            "terhub_attend"
        ]
    );

    // No daemon: tools fail gracefully, not with a protocol error.
    let status = mcp.call(
        3,
        "tools/call",
        json!({"name": "terhub_status", "arguments": {}}),
    );
    assert_eq!(status["result"]["isError"], true);
    assert!(
        status["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("daemon not running")
    );

    let bad_tool = mcp.call(4, "tools/call", json!({"name": "nope", "arguments": {}}));
    assert_eq!(bad_tool["result"]["isError"], true);

    let bad_method = mcp.call(5, "bogus/method", json!({}));
    assert_eq!(bad_method["error"]["code"], -32601);

    // Start a daemon on the same socket and push a hook; status sees it.
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_terhubd"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn terhubd");
    assert!(
        wait_for(terhub::daemon::daemon_alive, 100),
        "daemon did not come up"
    );
    terhub::daemon::send_event(
        &terhub::agent::HookEvent {
            pane: "%424242".to_string(),
            agent: "codex".to_string(),
            state: terhub::agent::AgentState::Working,
            summary: "mcp probe".to_string(),
            prompt: String::new(),
            model: String::new(),
            ctx: None,
        },
        Duration::from_secs(2),
    )
    .unwrap();
    let status = mcp.call(
        6,
        "tools/call",
        json!({"name": "terhub_status", "arguments": {}}),
    );
    assert_eq!(status["result"].get("isError"), None);
    let text = status["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("%424242"), "missing pane:\n{text}");
    assert!(text.contains("codex"), "missing agent:\n{text}");

    let attend = mcp.call(
        7,
        "tools/call",
        json!({"name": "terhub_attend", "arguments": {}}),
    );
    assert_eq!(attend["result"].get("isError"), None);

    // A pending terhub_wait must not wedge the server: ping answers
    // while the 20s wait is still pending. (Single-threaded server:
    // ping queues behind the wait and read_response's 5s deadline
    // fires first.) No agent is ever in "error", so the wait runs
    // the full 20s.
    {
        let body = json!({"jsonrpc": "2.0", "id": 8, "method": "tools/call",
            "params": {"name": "terhub_wait", "arguments": {"state": "error", "timeout_secs": 20}}});
        let text = serde_json::to_string(&body).unwrap();
        write!(mcp.stdin, "Content-Length: {}\r\n\r\n{text}", text.len()).unwrap();
        mcp.stdin.flush().unwrap();
    }
    let pong = mcp.call(9, "ping", json!({}));
    assert_eq!(pong["id"], 9, "ping queued behind terhub_wait");
    let waited = mcp.read_response_timeout(25);
    assert_eq!(waited["id"], 8);
    assert!(
        waited["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("timeout after 20s"),
        "unexpected wait outcome: {waited}"
    );

    // Client disconnect aborts a pending wait: dropping stdin with a
    // 120s wait in flight must exit the server promptly, not after
    // the wait times out.
    {
        let body = json!({"jsonrpc": "2.0", "id": 10, "method": "tools/call",
            "params": {"name": "terhub_wait", "arguments": {"state": "error", "timeout_secs": 120}}});
        let text = serde_json::to_string(&body).unwrap();
        write!(mcp.stdin, "Content-Length: {}\r\n\r\n{text}", text.len()).unwrap();
        mcp.stdin.flush().unwrap();
    }
    std::thread::sleep(Duration::from_secs(2));
    let Mcp {
        mut child,
        stdin,
        ..
    } = mcp;
    drop(stdin);
    let mut exited = false;
    for _ in 0..100 {
        match child.try_wait() {
            Ok(Some(_)) => {
                exited = true;
                break;
            }
            _ => std::thread::sleep(Duration::from_millis(100)),
        }
    }
    let _ =
        terhub::daemon::send_request(&terhub::daemon::Request::Shutdown, Duration::from_secs(2));
    let _ = child.kill();
    for _ in 0..50 {
        match daemon.try_wait() {
            Ok(Some(_)) => break,
            _ => std::thread::sleep(Duration::from_millis(100)),
        }
    }
    let _ = daemon.kill();
    assert!(exited, "mcp server survived client disconnect during terhub_wait");
    let _ = std::fs::remove_dir_all(&runtime);
}
