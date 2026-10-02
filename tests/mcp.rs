//! MCP stdio conversation test: framed initialize/list/call against `terhub
//! mcp`, first without a daemon (graceful errors), then with one (live data).
//!
//! Single test: owns TERHUB_RUNTIME_DIR for this binary.
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::Duration;

struct Mcp {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<std::process::ChildStdout>,
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
            reader: BufReader::new(stdout),
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
        let mut len: Option<usize> = None;
        loop {
            let mut line = String::new();
            self.reader.read_line(&mut line).unwrap();
            if line.trim().is_empty() {
                break;
            }
            if let Some((k, v)) = line.split_once(':')
                && k.trim().eq_ignore_ascii_case("content-length")
            {
                len = v.trim().parse().ok();
            }
        }
        let mut buf = vec![0u8; len.expect("framed response")];
        self.reader.read_exact(&mut buf).unwrap();
        serde_json::from_slice(&buf).unwrap()
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
    if !terhub::tmux::Tmux::available() {
        return;
    }
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

    mcp.stop();
    let _ =
        terhub::daemon::send_request(&terhub::daemon::Request::Shutdown, Duration::from_secs(2));
    for _ in 0..50 {
        match daemon.try_wait() {
            Ok(Some(_)) => break,
            _ => std::thread::sleep(Duration::from_millis(100)),
        }
    }
    let _ = daemon.kill();
    let _ = std::fs::remove_dir_all(&runtime);
}
