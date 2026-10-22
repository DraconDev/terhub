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
                // notification, per the spec). The requests run
                // concurrently -- see handle_batch.
                std::thread::spawn(move || {
                    if let Some(resp) = handle_batch(batch) {
                        let _ = write_frame_locked(&resp);
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

/// Run `f` over every item on its own thread, collecting the results
/// in the items' order.
///
/// The JSON-RPC batch path goes through here: a `terhub_wait` (up to
/// 600s) in one request must not hold up the others in the same
/// batch. A panicking item is dropped rather than taking the whole
/// batch down with it.
fn parallel_map<T, R, F>(items: Vec<T>, f: F) -> Vec<R>
where
    T: Send + 'static,
    R: Send + 'static,
    F: Fn(T) -> R + Send + Sync + 'static,
{
    let f = std::sync::Arc::new(f);
    items
        .into_iter()
        .map(|item| {
            let f = f.clone();
            std::thread::spawn(move || f(item))
        })
        .collect::<Vec<_>>()
        .into_iter()
        .filter_map(|h| h.join().ok())
        .collect()
}

/// Answer a JSON-RPC batch: every request is handled on its own
/// thread and the replies are collected into the single array the spec
/// asks for. A notification (no `id`) gets no reply.
fn handle_batch(batch: Vec<Value>) -> Option<Value> {
    let responses: Vec<Value> = parallel_map(batch, |req| handle(&req))
        .into_iter()
        .flatten()
        .collect();
    if responses.is_empty() {
        None
    } else {
        Some(Value::Array(responses))
    }
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
        {"name": "terhub_status", "description": "Agent states: which panes run agents, who is waiting or errored. Returns JSON.",
         "inputSchema": {"type": "object", "properties": {
             "state": {"type": "string", "description": "Filter: working|waiting|error|idle|done|stuck|limited|paused"},
             "session": {"type": "string", "description": "Filter: only this tmux session"},
             "pane": {"type": "string", "description": "Filter: only this pane (e.g. %3)"},
             "agent": {"type": "string", "description": "Filter: only this agent (e.g. claude)"},
             "needs_attention": {"type": "boolean", "description": "Filter: only agents waiting on the human"},
             "limit": {"type": "number", "description": "Max agents to return (default: all)"},
             "format": {"type": "string", "description": "json (default) or text (the legacy one-line-per-agent form)"}}}},
        {"name": "terhub_history", "description": "The activity ledger: state transitions, prompts and session events. Returns JSON.",
         "inputSchema": {"type": "object", "properties": {
             "since": {"type": "string", "description": "Window: today|yesterday|week|month|all, 24h, 7d, or YYYY-MM-DD (default: today)"},
             "limit": {"type": "number", "description": "Max entries to return (default: 100)"},
             "kind": {"type": "string", "description": "Filter: agent|session|rule|human|prompt"},
             "pane": {"type": "string", "description": "Filter: only this pane"},
             "session": {"type": "string", "description": "Filter: only this tmux session"},
             "prompts": {"type": "boolean", "description": "Return the recent prompts instead of the transitions"},
             "stats": {"type": "boolean", "description": "Include per-agent time-in-state totals"}}}},
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
        "terhub_history" => tool_history(args),
        "terhub_capture" => tool_capture(args),
        "terhub_send" => tool_send(args),
        "terhub_wait" => tool_wait(args),
        "terhub_attend" => tool_attend(),
        _ => Err(format!("unknown tool: {name}")),
    }
}

fn snapshot() -> Result<crate::daemon::Snapshot, crate::daemon::DaemonError> {
    crate::daemon::fetch_snapshot(Duration::from_secs(2))
}

/// Optional `state` argument: absent means unfiltered, but a present
/// value that parses to nothing is the caller's mistake, not "no filter".
fn state_filter(args: &Value) -> Result<Option<crate::agent::AgentState>, String> {
    match args.get("state").and_then(|s| s.as_str()) {
        None => Ok(None),
        Some(s) => crate::agent::AgentState::parse(s).map(Some).ok_or_else(|| {
            "bad state (working|waiting|error|idle|done|stuck|limited|paused)".to_string()
        }),
    }
}

/// The filters `terhub_status` understands.
///
/// Every one is optional. A filter that is *present but empty* is the
/// caller's mistake, not a filter that matches everything: an empty
/// `session` would silently select every pane and look like the filter
/// simply did not work.
#[derive(Default)]
struct StatusFilters {
    state: Option<crate::agent::AgentState>,
    session: Option<String>,
    pane: Option<String>,
    agent: Option<String>,
    needs_attention: bool,
    limit: Option<usize>,
}

/// Read an optional string argument, rejecting an empty one.
fn optional_str(args: &Value, name: &str) -> Result<Option<String>, String> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.trim().is_empty() => Err(format!("{name} must not be empty")),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("{name} must be a string")),
    }
}

impl StatusFilters {
    fn read(args: &Value) -> Result<Self, String> {
        let limit = match args.get("limit") {
            None | Some(Value::Null) => None,
            Some(v) => {
                let n = v
                    .as_u64()
                    .ok_or_else(|| "limit must be a number".to_string())?;
                if n == 0 {
                    return Err("limit must be at least 1".to_string());
                }
                Some(n.min(1000) as usize)
            }
        };
        Ok(Self {
            state: state_filter(args)?,
            session: optional_str(args, "session")?,
            pane: optional_str(args, "pane")?,
            agent: optional_str(args, "agent")?,
            needs_attention: args
                .get("needs_attention")
                .and_then(|b| b.as_bool())
                .unwrap_or(false),
            limit,
        })
    }

    fn matches(&self, a: &crate::agent::AgentRecord) -> bool {
        if let Some(s) = self.state
            && a.state != s
        {
            return false;
        }
        if let Some(s) = &self.session
            && a.session != *s
        {
            return false;
        }
        if let Some(p) = &self.pane
            && a.pane != *p
        {
            return false;
        }
        if let Some(g) = &self.agent
            && !a.agent.eq_ignore_ascii_case(g)
        {
            return false;
        }
        if self.needs_attention && !a.state.needs_attention() {
            return false;
        }
        true
    }
}

/// One agent as JSON: the fields an operator (or a model) acts on.
fn agent_json(a: &crate::agent::AgentRecord, now: u64) -> Value {
    json!({
        "pane": a.pane,
        "agent": a.agent,
        "state": a.state.label(),
        "state_since": a.state_since,
        "waiting_secs": a.waiting_secs(now),
        "session": a.session,
        "window": a.window,
        "handle": a.handle,
        "model": a.model,
        "ctx_pct": a.ctx_pct,
        "last_turn_secs": a.last_turn_secs,
        "summary": a.summary,
        "source": a.source.label(),
        "updated_at": a.updated_at,
        "needs_attention": a.state.needs_attention(),
    })
}

/// The legacy one-line-per-agent rendering, kept for callers that
/// asked for `format: "text"`.
fn status_text(agents: &[crate::agent::AgentRecord]) -> String {
    if agents.is_empty() {
        return "no agents".to_string();
    }
    let now = crate::agent::now_unix();
    let mut out = String::new();
    for a in agents {
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
    out.trim_end().to_string()
}

/// Agent states, as JSON.
///
/// JSON is the default because the old plain-text blob made every
/// caller parse it: the fields are already there, and a model reading
/// `ctx_pct` as a number beats it reading "ctx=42%" out of a sentence.
/// The old form stays one argument away (`format: "text"`) for the
/// callers that want it.
fn tool_status(args: &Value) -> Result<String, String> {
    let filters = StatusFilters::read(args)?;
    let as_text = args
        .get("format")
        .and_then(|f| f.as_str())
        .map(|f| f.trim().eq_ignore_ascii_case("text"))
        .unwrap_or(false);
    let snap = snapshot().map_err(|e| e.message())?;
    let agents: Vec<crate::agent::AgentRecord> = snap
        .agents
        .into_iter()
        .filter(|a| filters.matches(a))
        .collect();
    // Needy first: the same order the text form has always used.
    let agents = crate::attend::needy_first(agents);
    let agents: Vec<crate::agent::AgentRecord> = match filters.limit {
        Some(n) => agents.into_iter().take(n).collect(),
        None => agents,
    };
    if as_text {
        return Ok(status_text(&agents));
    }
    let now = crate::agent::now_unix();
    let rows: Vec<Value> = agents.iter().map(|a| agent_json(a, now)).collect();
    let doc = json!({
        "count": rows.len(),
        "agents": rows,
    });
    serde_json::to_string_pretty(&doc).map_err(|e| format!("cannot render status: {e}"))
}

/// One ledger entry as JSON.
fn entry_json(e: &crate::history::Entry) -> Value {
    json!({
        "t": e.t,
        "kind": e.kind,
        "pane": e.pane,
        "agent": e.agent,
        "from": e.from.map(|s| s.label()),
        "to": e.to.map(|s| s.label()),
        "session": e.session,
        "window": e.window,
        "summary": e.summary,
        "event": e.event,
    })
}

/// The activity ledger: transitions, prompts, session events, rule
/// fires.
///
/// This is the same ledger the web UI reads, so an agent that can
/// drive terhub can also ask what happened in it — the question the
/// plain-text `terhub_status` could never answer.
fn tool_history(args: &Value) -> Result<String, String> {
    let now = crate::agent::now_unix();
    // Default to today: the ledger grows without bound, and "all" on
    // a long-lived install is a multi-second read.
    let since_arg = args
        .get("since")
        .and_then(|s| s.as_str())
        .unwrap_or("today")
        .trim();
    if since_arg.is_empty() {
        return Err("since must not be empty".to_string());
    }
    let since = crate::history::parse_since(since_arg, now).map_err(|e| format!("{e:#}"))?;
    let limit = match args.get("limit") {
        None | Some(Value::Null) => 100usize,
        Some(v) => {
            let n = v
                .as_u64()
                .ok_or_else(|| "limit must be a number".to_string())?;
            if n == 0 {
                return Err("limit must be at least 1".to_string());
            }
            n.min(1000) as usize
        }
    };
    let kind = optional_str(args, "kind")?.map(|k| k.trim().to_lowercase());
    if let Some(k) = &kind
        && !matches!(k.as_str(), "agent" | "session" | "rule" | "human" | "prompt")
    {
        return Err("bad kind (agent|session|rule|human|prompt)".to_string());
    }
    let pane = optional_str(args, "pane")?;
    let session = optional_str(args, "session")?;
    let prompts = args
        .get("prompts")
        .and_then(|p| p.as_bool())
        .unwrap_or(false);
    let want_stats = args
        .get("stats")
        .and_then(|s| s.as_bool())
        .unwrap_or(false);

    let path = crate::history::ledger_path();
    // Windowed: the entries inside the window, plus the anchors that
    // keep the spans open across its edge.
    let entries = crate::history::read_entries_windowed(&path, since);

    let mut rows: Vec<Value> = Vec::new();
    if prompts {
        for e in crate::history::recent_prompts(&entries, pane.as_deref(), limit) {
            rows.push(entry_json(&e));
        }
    } else {
        let mut kept: Vec<&crate::history::Entry> = entries.iter().collect();
        if let Some(k) = &kind {
            kept.retain(|e| &e.kind == k);
        }
        if let Some(p) = &pane {
            kept.retain(|e| &e.pane == p);
        }
        if let Some(s) = &session {
            kept.retain(|e| &e.session == s);
        }
        // Newest first: "the last 10" means the last 10, not the first
        // 10 of the day.
        for e in kept.iter().rev().take(limit) {
            rows.push(entry_json(e));
        }
    }

    let mut doc = json!({
        "count": rows.len(),
        "since": since,
        "entries": rows,
    });
    if want_stats {
        let spans = crate::history::build_spans(&entries, now);
        let grouped = crate::history::stats(&spans, since, crate::history::GroupBy::Agent);
        doc["stats"] = serde_json::to_value(grouped).unwrap_or(Value::Null);
    }
    serde_json::to_string_pretty(&doc).map_err(|e| format!("cannot render history: {e}"))
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
        // A daemon that is merely slow must not abort the wait: the
        // wait has a deadline of its own, and one slow poll (the
        // daemon is mid-write, the disk is busy) is not the answer
        // "nothing happened yet". A daemon that is *gone* still is:
        // that one is worth reporting straight away.
        let snap = match snapshot() {
            Ok(s) => s,
            Err(e @ crate::daemon::DaemonError::Slow { .. }) => {
                let _ = e;
                if Instant::now() >= deadline {
                    return Err(format!("timeout after {timeout}s"));
                }
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
            Err(e) => return Err(e.message()),
        };
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
    let snap = snapshot().map_err(|e| e.message())?;
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

    fn record(pane: &str, agent: &str, state: crate::agent::AgentState, session: &str) -> crate::agent::AgentRecord {
        crate::agent::AgentRecord {
            pane: pane.to_string(),
            agent: agent.to_string(),
            state,
            source: crate::agent::Source::Hook,
            summary: "needs input".to_string(),
            session: session.to_string(),
            window: "@1".to_string(),
            command: "claude".to_string(),
            updated_at: 1_000,
            state_since: 900,
            cpu_ticks: 0,
            cpu_still_since: 0,
            agent_start: 0,
            last_turn_secs: 42,
            screen_hash: 0,
            quiet_since: 0,
            last_prompt_at: 0,
            last_prompt: String::new(),
            handle: "@pi-wez".to_string(),
            model: "opus-4.5".to_string(),
            ctx_pct: Some(42),
            reg_pid: 0,
            bell: false,
            bell_reason: String::new(),
            stuck_base: None,
        }
    }

    #[test]
    fn the_status_filters_narrow_the_agent_set() {
        let agents = vec![
            record("%1", "claude", crate::agent::AgentState::Waiting, "work"),
            record("%2", "codex", crate::agent::AgentState::Working, "work"),
            record("%3", "claude", crate::agent::AgentState::Working, "play"),
        ];
        let kept = |args: Value| -> Vec<String> {
            let f = StatusFilters::read(&args).expect("filters parse");
            agents
                .iter()
                .filter(|a| f.matches(a))
                .map(|a| a.pane.clone())
                .collect()
        };
        assert_eq!(kept(json!({})), vec!["%1", "%2", "%3"]);
        assert_eq!(kept(json!({"state": "working"})), vec!["%2", "%3"]);
        assert_eq!(kept(json!({"pane": "%1"})), vec!["%1"]);
        assert_eq!(kept(json!({"agent": "CLAUDE"})), vec!["%1", "%3"]);
        assert_eq!(kept(json!({"session": "play"})), vec!["%3"]);
        assert_eq!(
            kept(json!({"needs_attention": true})),
            vec!["%1"],
            "only the waiting agent needs the human"
        );
        // A filter that is present but empty is a mistake, not a filter
        // that matches everything.
        assert!(StatusFilters::read(&json!({"session": ""})).is_err());
        assert!(StatusFilters::read(&json!({"state": "nonsense"})).is_err());
        assert!(StatusFilters::read(&json!({"limit": 0})).is_err());
    }

    #[test]
    fn an_agent_renders_as_the_fields_a_caller_acts_on() {
        let a = record("%1", "claude", crate::agent::AgentState::Waiting, "work");
        let v = agent_json(&a, 1_000);
        assert_eq!(v["pane"], "%1");
        assert_eq!(v["agent"], "claude");
        assert_eq!(v["state"], "waiting");
        assert_eq!(v["session"], "work");
        assert_eq!(v["model"], "opus-4.5");
        assert_eq!(v["ctx_pct"], 42);
        assert_eq!(v["last_turn_secs"], 42);
        assert_eq!(v["handle"], "@pi-wez");
        assert_eq!(v["source"], "hook");
        assert_eq!(v["needs_attention"], true);
        assert_eq!(v["waiting_secs"], 100);
    }

    #[test]
    fn a_ledger_entry_renders_its_transition() {
        let e = crate::history::Entry::transition(
            1_234,
            &crate::attend::Transition {
                pane: "%7".to_string(),
                agent: "pi".to_string(),
                from: crate::agent::AgentState::Working,
                to: crate::agent::AgentState::Waiting,
                session: "wez".to_string(),
                window: "@3".to_string(),
                summary: "needs input".to_string(),
            },
        );
        let v = entry_json(&e);
        assert_eq!(v["t"], 1_234);
        assert_eq!(v["kind"], "agent");
        assert_eq!(v["pane"], "%7");
        assert_eq!(v["from"], "working");
        assert_eq!(v["to"], "waiting");
        assert_eq!(v["session"], "wez");
        assert_eq!(v["summary"], "needs input");
    }

    #[test]
    fn the_history_tool_is_advertised() {
        let names: Vec<String> = tools()["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert!(names.contains(&"terhub_history".to_string()), "{names:?}");
        assert!(names.contains(&"terhub_status".to_string()), "{names:?}");
    }

    #[test]
    fn a_batch_answers_with_one_array_and_no_notification_replies() {
        let batch = vec![
            json!({"jsonrpc":"2.0","id":1,"method":"ping"}),
            json!({"jsonrpc":"2.0","method":"ping"}),
            json!({"jsonrpc":"2.0","id":2,"method":"ping"}),
        ];
        let resp = handle_batch(batch).expect("the batch answers");
        let arr = resp.as_array().expect("the reply is an array");
        assert_eq!(arr.len(), 2, "a notification must not get a reply");
        let ids: Vec<i64> = arr.iter().map(|r| r["id"].as_i64().unwrap()).collect();
        assert_eq!(ids, vec![1, 2]);
    }

    #[test]
    fn the_batch_runs_its_requests_at_the_same_time() {
        // A `terhub_wait` (up to 600s) inside a batch must not delay the
        // other requests in it: the batch runs them concurrently.
        let start = std::time::Instant::now();
        let out: Vec<u64> = parallel_map(vec![1u64, 2, 3], |n| {
            std::thread::sleep(std::time::Duration::from_millis(300));
            n
        });
        let elapsed = start.elapsed();
        assert_eq!(out, vec![1, 2, 3]);
        assert!(
            elapsed < std::time::Duration::from_millis(700),
            "the requests ran one after another: {elapsed:?}"
        );
    }

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
