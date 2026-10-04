//! MCP: stdio server so coding agents can orchestrate terhub.
//!
//! Speaks MCP over stdio (LSP-style Content-Length framing, JSON-RPC 2.0).
//! Methods: initialize, ping, tools/list, tools/call. Tools: terhub_status,
//! terhub_capture, terhub_send, terhub_wait, terhub_attend.
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
            None => return Ok(()), // EOF
            Some(req) => {
                if let Some(resp) = handle(&req) {
                    write_frame(&resp)?;
                }
            }
        }
    }
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

fn read_frame(reader: &mut impl BufRead) -> Result<Option<Value>> {
    loop {
        let mut headers = Vec::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line)? == 0 {
                return Ok(None); // EOF
            }
            if line.trim().is_empty() {
                break;
            }
            headers.push(line);
        }
        let Some(n) = content_length(&headers) else {
            continue; // unframed garbage: skip, keep reading
        };
        if n > MAX_FRAME_BYTES {
            eprintln!("terhub mcp: frame too large ({n} bytes), skipping");
            // Drain the body so the next read starts at a frame boundary;
            // without this the body bytes parse as headers and every later
            // frame misaligns (a fake Content-Length inside the body can
            // even swallow the next real frame whole).
            let mut body = reader.by_ref().take(n as u64);
            if std::io::copy(&mut body, &mut std::io::sink()).is_err() {
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
        "initialize" => (
            "result",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "terhub", "version": env!("CARGO_PKG_VERSION")},
            }),
        ),
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
             "state": {"type": "string", "description": "Filter: working|waiting|error|idle|stuck|limited"}}}},
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

fn tool_status(args: &Value) -> Result<String, String> {
    let filter = args
        .get("state")
        .and_then(|s| s.as_str())
        .and_then(crate::agent::AgentState::parse);
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
    let state = args
        .get("state")
        .and_then(|s| s.as_str())
        .map(crate::agent::AgentState::parse)
        .unwrap_or(Some(crate::agent::AgentState::Waiting))
        .ok_or_else(|| "bad state (working|waiting|error|idle|stuck|limited)".to_string())?;
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
        let mut input =
            format!("Content-Length: {}\r\n\r\n{body1}", body1.len()).into_bytes();
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
}
