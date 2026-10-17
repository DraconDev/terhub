//! Web dashboard test: spawn `terhub serve` on an ephemeral loopback port,
//! check the dashboard and APIs with plain TCP, first without a daemon
//! (graceful 503s), then with one (live agent data).
//!
//! Single test: owns TERHUB_RUNTIME_DIR for this binary.
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

fn wait_for(cond: impl Fn() -> bool, tries: u32) -> bool {
    for _ in 0..tries {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    cond()
}

struct Server {
    child: Child,
    port: u16,
}

impl Server {
    fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_terhub"))
            .args(["serve", "--port", "0"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn terhub serve");
        let stdout = child.stdout.take().unwrap();
        // Bounded read: a panic in terhub serve before its first
        // listen-line write (or a wedged bind) would otherwise hang
        // the suite forever. Read on a thread, recv with a deadline.
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            let _ = reader.read_line(&mut line);
            let _ = tx.send(line);
        });
        let line = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("terhub serve did not print a listen line within 5s");
        // "listening on http://127.0.0.1:PORT"
        let port: u16 = line
            .trim()
            .rsplit(':')
            .next()
            .unwrap()
            .parse()
            .expect("port in listen line");
        assert!(wait_for(
            || TcpStream::connect(("127.0.0.1", port)).is_ok(),
            50
        ));
        Server { child, port }
    }

    fn get(&self, path: &str) -> (u16, String) {
        self.request("GET", path, None)
    }

    fn post(&self, path: &str, body: &str) -> (u16, String) {
        self.request("POST", path, Some(body))
    }

    fn request(&self, method: &str, path: &str, body: Option<&str>) -> (u16, String) {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        // Bounded read: a wedged server (e.g. tmux capture hanging
        // behind /api/capture) used to block the suite forever here.
        // 5s is generous for a loopback response.
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let body = body.unwrap_or("");
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n{body}",
            body.len(),
        )
        .unwrap();
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).unwrap();
        let text = String::from_utf8_lossy(&raw).into_owned();
        let code: u16 = text
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        let body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
        (code, body)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

#[test]
fn web_dashboard_and_apis() {
    // Fail loudly without tmux (the backend.rs house rule): a silent
    // early return reports "passed" with zero assertions executed.
    // CI installs tmux; see .github/workflows/ci.yml.
    assert!(
        terhub::tmux::Tmux::available(),
        "tmux is required for this suite (CI installs it)"
    );
    let runtime = std::env::temp_dir().join(format!("terhub-web-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&runtime);
    std::fs::create_dir_all(&runtime).unwrap();
    // SAFETY: single test in this binary.
    unsafe {
        std::env::set_var("TERHUB_RUNTIME_DIR", &runtime);
    }

    let server = Server::start();

    let (code, body) = server.get("/");
    assert_eq!(code, 200);
    assert!(body.contains("<title>terhub</title>"), "no dashboard");
    assert!(body.contains("api/status"), "no api wiring");
    // Stored-XSS guard on the served bytes (not just the const): the
    // dashboard renders untrusted fleet snapshot strings with
    // innerHTML, so the esc() sink must encode quotes as well as
    // angle brackets — a `" onmouseover="...` pane needs no `<`.
    assert!(
        body.contains("replace(/\"/g,\"&quot;\")"),
        "served dashboard esc() must encode double quotes"
    );
    assert!(
        body.contains("replace(/'/g,\"&#39;\")"),
        "served dashboard esc() must encode single quotes"
    );

    // No daemon yet: live status degrades to 503, not 500 — while
    // stats/timeline keep serving ledger history (frozen, not stale).
    let (code, body) = server.get("/api/status");
    assert_eq!(code, 503);
    assert!(body.contains("daemon not running"), "body: {body}");
    let (code, _) = server.get("/api/stats?since=all&group=agent");
    assert_eq!(code, 200);
    let (code, _) = server.get("/api/timeline?since=all&limit=10");
    assert_eq!(code, 200);

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
            pane: "%616161".to_string(),
            agent: "gemini".to_string(),
            state: terhub::agent::AgentState::Waiting,
            summary: "web probe".to_string(),
            prompt: String::new(),
            model: String::new(),
            ctx: None,
        },
        Duration::from_secs(2),
    )
    .unwrap();

    let (code, body) = server.get("/api/status");
    assert_eq!(code, 200);
    assert!(body.contains("%616161"), "body: {body}");
    assert!(body.contains("gemini"), "body: {body}");

    let (code, _) = server.get("/api/stats?since=all&group=agent");
    assert_eq!(code, 200);
    let (code, body) = server.get("/api/stats?since=bogus&group=agent");
    assert_eq!(code, 400, "body: {body}");

    let (code, _) = server.get("/api/timeline?since=all&limit=10");
    assert_eq!(code, 200);

    // Prompting a nonexistent pane fails cleanly (no side effects).
    let (code, _) = server.post(
        "/api/send",
        r#"{"pane":"%999999","text":"hi","submit":true}"#,
    );
    assert_eq!(code, 500);
    let (code, _) = server.post("/api/interrupt", r#"{"pane":"%999999"}"#);
    assert_eq!(code, 500);

    let _ =
        terhub::daemon::send_request(&terhub::daemon::Request::Shutdown, Duration::from_secs(2));
    for _ in 0..50 {
        match daemon.try_wait() {
            Ok(Some(_)) => break,
            _ => std::thread::sleep(Duration::from_millis(100)),
        }
    }
    let _ = daemon.kill();
    drop(server);
    let _ = std::fs::remove_dir_all(&runtime);
}
