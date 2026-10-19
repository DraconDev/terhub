//! MCP: stdio server so coding agents can orchestrate terhub.
//!
//! Speaks MCP over stdio (LSP-style Content-Length framing, JSON-RPC 2.0).
//! Methods: initialize, ping, tools/list, tools/call. Tools: terhub_status,
//! terhub_capture, terhub_send, terhub_wait, terhub_attend.
//!
//! Each frame is handled on its own thread (stdout writes serialized
//! under a lock), so a long terhub_wait never wedges the server, and
//! stdin EOF returns from the loop so the process — and every pending
//! tool with it — exits when the client disconnects.
//!
//! Only framed responses go to stdout; diagnostics go to stderr.
use anyhow::Result;
use serde_json::{Value, json};
use std::io::{BufRead, Read, Write};
use std::time::{Duration, Instant};

pub fn run() -> Result<()> {
    let stdin = std::io::stdin();
    let mut reader = std::io::BufReader::new(stdin.lock());
    loop {
        match read_frame(&mut reader)? {
            // EOF: the client is gone. Return so the process exits,
            // which aborts any tool still running on a worker thread
            // (a 600s terhub_wait must not outlive its client).
            None => return Ok(()),
            Some(Value::Array(batch)) => {
                // JSON-RPC batch: handle each request and reply with a
                // JSON array (omitted entirely if every request was a
                // notification, per the spec).
                std::thread::spawn(move || {
                    let mut responses = Vec::with_capacity(batch.len());
                    for req in batch {
                        if let Some(resp) = handle(&req) {
                            responses.push(resp);
                        }
                    }
                    if !responses.is_empty() {
                        let _ = write_frame_locked(&Value::Array(responses));
                    }
                });
            }
            // One thread per frame: a long tool (terhub_wait) runs
            // off the server thread, so ping and other tools stay
            // responsive while it polls. Responses carry ids, so
            // out-of-order completion is spec-fine.
            Some(req) => {
                std::thread::spawn(move || {
                    if let Some(resp) = handle(&req) {
                        let _ = write_frame_locked(&resp);
                    }
                });
            }
        }
    }
}

/// Serialize stdout writes: concurrent tool threads must not
/// interleave frames. A failed write means the client is gone, so
/// there is nobody to report it to.
fn write_frame_locked(value: &Value) -> Result<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    let _guard = LOCK
        .get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    write_frame(value)
}

fn content_length(headers: &[String]) -> Option<usize> {
    headers.iter().find_map(|h| {
        h.split_once(':').and_then(|(k, v)| {
            if k.trim().eq_ignore_ascii_case("content-length") {
                v.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
    })
}

/// Largest MCP frame we'll allocate for. The length comes from the peer,
/// so an uncapped `vec![0; n]` would let a bogus header OOM the server.
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// Header flood guards: real frames carry 1–2 short headers. A peer that
/// never sends a blank line gets its frame drained and skipped, not OOM.
const MAX_HEADER_LINES: usize = 64;
const MAX_HEADER_LINE_BYTES: usize = 16 * 1024;

/// Content-Length parsed from raw header bytes (works even on lines that
/// will later fail UTF-8 conversion or flood caps): skipping a frame still
/// has to drain its body to hold the frame boundary.
fn content_length_raw(line: &[u8]) -> Option<usize> {
    let colon = line.iter().position(|b| *b == b':')?;
    let (k, v) = line.split_at(colon);
    let key: Vec<u8> = k
        .iter()
        .filter(|b| !b.is_ascii_whitespace())
        .copied()
        .collect();
    if !key.eq_ignore_ascii_case(b"content-length") {
        return None;
    }
    let val: Vec<u8> = v[1..]
        .iter()
        .filter(|b| !b.is_ascii_whitespace())
        .copied()
        .collect();
    std::str::from_utf8(&val).ok()?.parse().ok()
}

/// Drain a skipped frame's body so the next read starts at a frame
/// boundary; without this the body bytes parse as headers and every later
/// frame misaligns (a fake Content-Length inside the body can even swallow
/// the next real frame whole). EOF mid-drain ends the stream.
fn drain_body(reader: &mut impl BufRead, n: usize) -> Result<bool> {
    let mut body = reader.by_ref().take(n as u64);
    match std::io::copy(&mut body, &mut std::io::sink()) {
        Ok(got) => Ok(got == n as u64),
        Err(_) => Ok(false),
    }
}

fn read_frame(reader: &mut impl BufRead) -> Result<Option<Value>> {
    loop {
        // Headers arrive as bytes first: a non-UTF8 line abandons the frame
        // (already resynced at the blank line below) instead of read_line's
        // UTF-8 error exiting the whole server.
        let mut raw_headers = Vec::new();
        let mut flooded = false;
        let mut body_len = None;
        loop {
            let mut raw = Vec::new();
            let n = reader
                .by_ref()
                .take((MAX_HEADER_LINE_BYTES + 1) as u64)
                .read_until(b'\n', &mut raw)?;
            if n == 0 {
                return Ok(None); // EOF
            }
            if raw.len() > MAX_HEADER_LINE_BYTES || raw_headers.len() >= MAX_HEADER_LINES {
                flooded = true;
            }
            if raw.iter().all(|b| b.is_ascii_whitespace()) {
                break;
            }
            if body_len.is_none() {
                body_len = content_length_raw(&raw);
            }
            // Flooded lines drain to the blank line without accumulating.
            if !flooded {
                raw_headers.push(raw);
            }
        }
        if flooded {
            eprintln!("terhub mcp: header flood, skipping frame");
            if let Some(n) = body_len
                && !drain_body(reader, n)?
            {
                return Ok(None);
            }
            continue;
        }
        let mut headers = Vec::with_capacity(raw_headers.len());
        let mut utf8_ok = true;
        for raw in raw_headers {
            match String::from_utf8(raw) {
                Ok(l) => headers.push(l),
                Err(_) => {
                    utf8_ok = false;
                    break;
                }
            }
        }
        if !utf8_ok {
            eprintln!("terhub mcp: non-utf8 header, skipping frame");
            if let Some(n) = body_len
                && !drain_body(reader, n)?
            {
                return Ok(None);
            }
            continue;
        }
        let Some(n) = content_length(&headers) else {
            continue; // unframed garbage: skip, keep reading
        };
        if n > MAX_FRAME_BYTES {
            eprintln!("terhub mcp: frame too large ({n} bytes), skipping");
            if !drain_body(reader, n)? {
                return Ok(None);
            }
            continue;
        }
        let mut buf = vec![0u8; n];
        if let Err(e) = reader.read_exact(&mut buf) {
            // Truncated mid-frame (usually EOF): stop instead of dying loudly.
            if e.kind() == std::io::ErrorKind::UnexpectedEof {
                return Ok(None);
            }
            return Err(e.into());
        }
        match serde_json::from_slice(&buf) {
            Ok(v) => return Ok(Some(v)),
            Err(e) => {
                eprintln!("terhub mcp: bad frame: {e}");
                continue;
            }
        }
    }
}

fn write_frame(value: &Value) -> Result<()> {
    let body = serde_json::to_string(value)?;
    let mut out = std::io::stdout().lock();
    write!(out, "Content-Length: {}\r\n\r\n{body}", body.len())?;
    out.flush()?;
    Ok(())
}

fn handle(req: &Value) -> Option<Value> {
    let id = req.get("id")?.clone(); // notifications get no reply
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let (key, val) = match method {
        "initialize" => {
            // Echo the client's requested protocolVersion when it's
            // one we support; fall back to the latest known version
            // otherwise (clients use this for capability negotiation).
            let client_pv = params
                .get("protocolVersion")
                .and_then(|v| v.as_str())
                .unwrap_or("2024-11-05");
            let supported = ["2024-11-05", "2024-10-07"];
            let pv = if supported.contains(&client_pv) {
                client_pv
            } else {
                "2024-11-05"
            };
            (
                "result",
                json!({
                    "protocolVersion": pv,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "terhub", "version": env!("CARGO_PKG_VERSION")},
                }),
            )
        }
        "ping" => ("result", json!({})),
        "tools/list" => ("result", json!({"tools": tools()})),
        "tools/call" => ("result", call_tool(&params)),
        _ => (
            "error",
            json!({"code": -32601, "message": format!("unknown method: {method}")}),
        ),
    };
    let mut map = serde_json::Map::new();
    map.insert("jsonrpc".to_string(), json!("2.0"));
    map.insert("id".to_string(), id);
    map.insert(key.to_string(), val);
    Some(Value::Object(map))
}

fn tools() -> Value {
    json!([
        {"name": "terhub_status", "description": "Agent states: which panes run agents, who is waiting or errored.",
         "inputSchema": {"type": "object", "properties": {
             "state": {"type": "string", "description": "Filter: working|waiting|error|idle|stuck|limited|paused"}}}},
        {"name": "terhub_capture", "description": "Recent screen content of a pane.",
         "inputSchema": {"type": "object", "properties": {
             "pane": {"type": "string"}, "lines": {"type": "number"}},
             "required": ["pane"]}},
        {"name": "terhub_send", "description": "Type text into a pane, submitting with Enter by default.",
         "inputSchema": {"type": "object", "properties": {
             "pane": {"type": "string"}, "text": {"type": "string"}, "submit": {"type": "boolean"}},
             "required": ["pane", "text"]}},
        {"name": "terhub_wait", "description": "Wait until a pane (or any agent) reaches a state. Times out with an error.",
         "inputSchema": {"type": "object", "properties": {
             "pane": {"type": "string"}, "state": {"type": "string"}, "timeout_secs": {"type": "number"}}}},
        {"name": "terhub_attend", "description": "Longest-waiting agent needing attention.",
         "inputSchema": {"type": "object", "properties": {}}},
    ])
}

fn call_tool(params: &Value) -> Value {
    let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);
    match tool_result(name, &args) {
        Ok(text) => json!({"content": [{"type": "text", "text": text}]}),
        Err(text) => json!({"content": [{"type": "text", "text": text}], "isError": true}),
    }
}

fn tool_result(name: &str, args: &Value) -> Result<String, String> {
    match name {
        "terhub_status" => tool_status(args),
        "terhub_capture" => tool_capture(args),
        "terhub_send" => tool_send(args),
        "terhub_wait" => tool_wait(args),
        "terhub_attend" => tool_attend(),
        _ => Err(format!("unknown tool: {name}")),
    }
}

fn snapshot() -> Result<crate::daemon::Snapshot, String> {
    crate::daemon::fetch_snapshot(Duration::from_secs(2))
        .map_err(|_| "daemon not running (terhub daemon start)".to_string())
}

/// Optional `state` argument: absent means unfiltered, but a present
/// value that parses to nothing is the caller's mistake, not "no filter".
fn state_filter(args: &Value) -> Result<Option<crate::agent::AgentState>, String> {
    match args.get("state").and_then(|s| s.as_str()) {
        None => Ok(None),
        Some(s) => crate::agent::AgentState::parse(s).map(Some).ok_or_else(|| {
            "bad state (working|waiting|error|idle|stuck|limited|paused)".to_string()
        }),
    }
}

fn tool_status(args: &Value) -> Result<String, String> {
    let filter = state_filter(args)?;
    let snap = snapshot()?;
    let mut agents = snap.agents;
    if let Some(f) = filter {
        agents.retain(|a| a.state == f);
    }
    if agents.is_empty() {
        return Ok("no agents".to_string());
    }
    let now = crate::agent::now_unix();
    let mut out = String::new();
    for a in crate::attend::needy_first(agents) {
        out.push_str(&format!(
            "{} {} {} {}s {} {}\n",
            a.pane,
            a.agent,
            a.state.label(),
            a.waiting_secs(now),
            a.session,
            a.summary,
        ));
    }
    Ok(out.trim_end().to_string())
}

fn tool_capture(args: &Value) -> Result<String, String> {
    let pane = args
        .get("pane")
        .and_then(|p| p.as_str())
        .ok_or_else(|| "missing pane".to_string())?;
    let lines = args
        .get("lines")
        .and_then(|l| l.as_u64())
        .unwrap_or(60)
        .min(500) as u32;
    let tmux = crate::tmux::Tmux::new().map_err(|e| format!("{e:?}"))?;
    let text = tmux
        .capture_pane(pane, lines)
        .map_err(|e| format!("{e:?}"))?;
    if text.trim().is_empty() {
        Ok("(empty)".to_string())
    } else {
        Ok(text)
    }
}

fn tool_send(args: &Value) -> Result<String, String> {
    let pane = args
        .get("pane")
        .and_then(|p| p.as_str())
        .ok_or_else(|| "missing pane".to_string())?;
    let text = args
        .get("text")
        .and_then(|t| t.as_str())
        .ok_or_else(|| "missing text".to_string())?;
    let submit = args.get("submit").and_then(|s| s.as_bool()).unwrap_or(true);
    let tmux = crate::tmux::Tmux::new().map_err(|e| format!("{e:?}"))?;
    tmux.send_keys(pane, text, submit)
        .map_err(|e| format!("{e:?}"))?;
    Ok(format!("sent {} chars to {pane}", text.chars().count()))
}

fn tool_wait(args: &Value) -> Result<String, String> {
    let pane = args.get("pane").and_then(|p| p.as_str());
    let state = state_filter(args)?.unwrap_or(crate::agent::AgentState::Waiting);
    let timeout = args
        .get("timeout_secs")
        .and_then(|t| t.as_u64())
        .unwrap_or(60)
        .clamp(1, 600);
    let deadline = Instant::now() + Duration::from_secs(timeout);
    loop {
        let snap = snapshot()?;
        let hit = match pane {
            Some(p) => snap
                .agents
                .iter()
                .find(|a| a.pane == p && a.state == state)
                .map(|a| format!("{} {} {}", a.pane, a.agent, a.state.label())),
            None => snap
                .agents
                .iter()
                .find(|a| a.state == state)
                .map(|a| format!("{} {} {}", a.pane, a.agent, a.state.label())),
        };
        if let Some(h) = hit {
            return Ok(h);
        }
        if Instant::now() >= deadline {
            return Err(format!("timeout after {timeout}s"));
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn tool_attend() -> Result<String, String> {
    let snap = snapshot()?;
    match crate::attend::pick_attend(&snap.agents, None) {
        Some(a) => Ok(format!(
            "{} {} {} {} {}",
            a.pane,
            a.agent,
            a.state.label(),
            a.session,
            a.summary
        )),
        None => Ok(format!("all clear ({} agents)", snap.agents.len())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn framed(body: &str) -> Vec<u8> {
        format!("Content-Length: {}\r\n\r\n{body}", body.len()).into_bytes()
    }

    #[test]
    fn valid_frame_parses() {
        let mut cur = Cursor::new(framed(r#"{"jsonrpc":"2.0","id":1}"#));
        let v = read_frame(&mut cur).unwrap().unwrap();
        assert_eq!(v["id"], 1);
    }

    #[test]
    fn giant_length_is_skipped_not_allocated() {
        // A lying 18EB header must not OOM: skipped, then EOF.
        let input = b"Content-Length: 18446744073709551615\r\n\r\n{}".to_vec();
        let mut cur = Cursor::new(input);
        assert!(read_frame(&mut cur).unwrap().is_none());
    }

    #[test]
    fn oversize_frame_does_not_swallow_next_frame() {
        // Body over the cap ending in a fake Content-Length whose window
        // spans past the body end: without the drain, the reader parses
        // the fake header and eats the real frame as "body" bytes.
        let pad = "A".repeat(MAX_FRAME_BYTES);
        let body1 = format!("{pad}\r\nContent-Length: 500\r\n\r\n{}", "B".repeat(60));
        let mut input = format!("Content-Length: {}\r\n\r\n{body1}", body1.len()).into_bytes();
        let real = r#"{"jsonrpc":"2.0","id":7,"method":"ping"}"#;
        input.extend_from_slice(&framed(real));
        let mut cur = Cursor::new(input);
        let v = read_frame(&mut cur).expect("read").expect("frame");
        assert_eq!(v.get("method").and_then(|m| m.as_str()), Some("ping"));
        assert_eq!(v.get("id").and_then(|i| i.as_u64()), Some(7));
    }

    #[test]
    fn truncated_body_stops_quietly() {
        let mut full = framed(r#"{"jsonrpc":"2.0","id":2}"#);
        full.truncate(full.len() - 5);
        let mut cur = Cursor::new(full);
        assert!(read_frame(&mut cur).unwrap().is_none());
    }

    #[test]
    fn non_utf8_header_skips_frame() {
        // Garbage bytes in the headers abandon one frame (body drained to
        // hold the boundary); the server keeps serving instead of exiting
        // on read_line's UTF-8 error.
        let mut input = b"\xff\xfe\nContent-Length: 2\r\n\r\n{}".to_vec();
        input.extend_from_slice(&framed(r#"{"jsonrpc":"2.0","id":9}"#));
        let mut cur = Cursor::new(input);
        let v = read_frame(&mut cur)
            .expect("server exited")
            .expect("no frame");
        assert_eq!(v.get("id").and_then(|i| i.as_u64()), Some(9));
    }

    #[test]
    fn header_flood_skips_frame() {
        // 100 junk headers (with a valid Content-Length!) must not
        // accumulate unboundedly: the frame is drained and skipped.
        let mut input = Vec::new();
        for i in 0..100 {
            input.extend_from_slice(format!("X-Junk-{i}: padding\r\n").as_bytes());
        }
        input.extend_from_slice(b"Content-Length: 2\r\n\r\n{}");
        let real = r#"{"jsonrpc":"2.0","id":11,"method":"ping"}"#;
        input.extend_from_slice(&framed(real));
        let mut cur = Cursor::new(input);
        let v = read_frame(&mut cur).expect("read").expect("frame");
        assert_eq!(v.get("id").and_then(|i| i.as_u64()), Some(11));
    }

    #[test]
    fn bad_state_filter_is_an_error() {
        use crate::agent::AgentState;
        assert_eq!(state_filter(&serde_json::json!({})).unwrap(), None);
        assert_eq!(
            state_filter(&serde_json::json!({"state": "waiting"})).unwrap(),
            Some(AgentState::Waiting)
        );
        assert!(state_filter(&serde_json::json!({"state": "bogus"})).is_err());
    }
}
