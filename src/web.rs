//! Web: loopback-only HTTP dashboard.
//!
//! `terhub serve` binds 127.0.0.1 (never anything else) and serves an
//! embedded single-page UI plus JSON APIs: status, stats, timeline, capture,
//! and the prompt (send) / abort (interrupt) actions.
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Query},
    http::StatusCode,
    response::{Html, IntoResponse},
    routing::{get, post},
};
use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;

pub fn router() -> Router {
    Router::new()
        .route("/", get(dashboard))
        .route("/api/status", get(api_status))
        .route("/api/stats", get(api_stats))
        .route("/api/timeline", get(api_timeline))
        .route("/api/capture", get(api_capture))
        .route("/api/send", post(api_send))
        .route("/api/interrupt", post(api_interrupt))
        .layer(axum::middleware::from_fn(origin_guard))
        // Cap the request body: the API takes tiny JSON for /api/send
        // and /api/interrupt; a generous 64KB is plenty and keeps a
        // hostile or buggy client from holding a buffer.
        .layer(DefaultBodyLimit::max(64 * 1024))
}

/// Loopback origins only: the dashboard is same-origin served, so a
/// present-but-foreign Origin (CSRF from a malicious page, DNS rebinding
/// against /api/send) is rejected. Missing Origin (curl, same-origin GET)
/// is allowed — this guards browsers, not the local user.
fn origin_allowed(origin: &str) -> bool {
    // scheme://host[:port][/...], tolerating bare hosts. Exact host match:
    // 127.0.0.1.evil.com must not ride on the 127.0.0.1 prefix.
    let after_scheme = origin.rsplit("://").next().unwrap_or("");
    let host_port = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    let host = host_port.rsplit('@').next().unwrap_or("");
    let bare = match host.strip_prefix('[') {
        Some(inner) => inner.split(']').next().unwrap_or(""),
        None => host.split(':').next().unwrap_or(""),
    };
    matches!(bare, "127.0.0.1" | "localhost" | "::1")
}

async fn origin_guard(
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let foreign = req
        .headers()
        .get(axum::http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|o| !origin_allowed(o));
    if foreign {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "foreign origin"})),
        )
            .into_response();
    }
    next.run(req).await
}

/// Serve on 127.0.0.1:`port` (0 = ephemeral). Prints the URL for the caller.
pub async fn serve(port: u16) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    println!("listening on http://{}", listener.local_addr()?);
    axum::serve(listener, router()).await?;
    Ok(())
}

async fn dashboard() -> Html<&'static str> {
    Html(DASHBOARD_HTML)
}

fn daemon_unavailable() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": "daemon not running"})),
    )
}

/// Serialize a payload, or 500 instead of panicking the handler task.
fn ok_json<T: serde::Serialize>(value: &T) -> axum::response::Response {
    match serde_json::to_value(value) {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("encode failed: {e}")})),
        )
            .into_response(),
    }
}

/// Keep the latest `limit` entries (spans arrive oldest-first).
fn latest<T>(mut items: Vec<T>, limit: usize) -> Vec<T> {
    let drop = items.len().saturating_sub(limit);
    items.drain(..drop);
    items
}

/// Stats rows from already-read entries: pure compute, called inside
/// `spawn_blocking` by the handler (the ledger read + span build must not
/// run on async workers) and directly by unit tests.
fn stats_rows(
    entries: &[crate::history::Entry],
    since: Option<u64>,
    group: crate::history::GroupBy,
    now: u64,
) -> Vec<crate::history::StatsRow> {
    let spans = crate::history::build_spans(entries, now);
    crate::history::stats(&spans, since, group)
}

/// Filtered timeline spans from already-read entries: pure compute, same
/// `spawn_blocking` contract as [`stats_rows`].
fn timeline_rows(
    entries: &[crate::history::Entry],
    since: Option<u64>,
    session: Option<&str>,
    agent: Option<&str>,
    limit: usize,
    now: u64,
) -> Vec<crate::history::Span> {
    let mut spans = crate::history::build_spans(entries, now);
    spans.retain(|s| {
        since.is_none_or(|c| s.end >= c)
            && session.is_none_or(|f| s.session.contains(f))
            && agent.is_none_or(|f| s.agent.contains(f))
    });
    latest(spans, limit.clamp(1, 2000))
}

/// 500 when the blocking pool itself fails (effectively never; documents
/// the JoinError arm instead of unwrapping the handler task).
fn blocking_failed() -> axum::response::Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({"error": "compute failed"})),
    )
        .into_response()
}

async fn api_status() -> impl IntoResponse {
    match crate::daemon::fetch_snapshot(Duration::from_secs(2)) {
        Ok(snap) => ok_json(&snap),
        Err(_) => daemon_unavailable().into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct StatsQuery {
    since: Option<String>,
    group: Option<String>,
}

async fn api_stats(Query(q): Query<StatsQuery>) -> impl IntoResponse {
    use crate::history as h;
    let now = crate::agent::now_unix();
    let since = match h::parse_since(q.since.as_deref().unwrap_or("today"), now) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("{e:?}")})),
            )
                .into_response();
        }
    };
    let group = match q.group.as_deref().unwrap_or("agent") {
        "day" => h::GroupBy::Day,
        "session" => h::GroupBy::Session,
        "agent" => h::GroupBy::Agent,
        other => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("bad group '{other}'")})),
            )
                .into_response();
        }
    };
    // Ledger read + span build are blocking file/CPU work: off the workers.
    let rows = tokio::task::spawn_blocking(move || {
        stats_rows(&h::read_entries(&h::ledger_path()), since, group, now)
    })
    .await;
    match rows {
        Ok(r) => ok_json(&r),
        Err(_) => blocking_failed(),
    }
}

#[derive(Debug, Deserialize)]
struct TimelineQuery {
    since: Option<String>,
    session: Option<String>,
    agent: Option<String>,
    limit: Option<usize>,
}

async fn api_timeline(Query(q): Query<TimelineQuery>) -> impl IntoResponse {
    use crate::history as h;
    let now = crate::agent::now_unix();
    let since = match h::parse_since(q.since.as_deref().unwrap_or("today"), now) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("{e:?}")})),
            )
                .into_response();
        }
    };
    // Ledger read + span build are blocking file/CPU work: off the workers.
    let session = q.session.clone();
    let agent = q.agent.clone();
    let limit = q.limit.unwrap_or(200);
    let spans = tokio::task::spawn_blocking(move || {
        timeline_rows(
            &h::read_entries(&h::ledger_path()),
            since,
            session.as_deref(),
            agent.as_deref(),
            limit,
            now,
        )
    })
    .await;
    match spans {
        Ok(s) => ok_json(&s),
        Err(_) => blocking_failed(),
    }
}

async fn api_capture(Query(q): Query<HashMap<String, String>>) -> impl IntoResponse {
    let Some(pane) = q.get("pane") else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "missing pane"})),
        )
            .into_response();
    };
    let lines = q
        .get("lines")
        .and_then(|l| l.parse::<u32>().ok())
        .unwrap_or(60)
        .min(500);
    let tmux = match crate::tmux::Tmux::new() {
        Ok(t) => t,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("{e:?}")})),
            )
                .into_response();
        }
    };
    match tmux.capture_pane(pane, lines) {
        Ok(text) => (
            StatusCode::OK,
            Json(serde_json::json!({"pane": pane, "text": text})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("{e:?}")})),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct SendBody {
    pane: String,
    text: String,
    submit: Option<bool>,
}

async fn api_send(Json(body): Json<SendBody>) -> impl IntoResponse {
    let tmux = match crate::tmux::Tmux::new() {
        Ok(t) => t,
        Err(e) => return fail(e),
    };
    match tmux.send_keys(&body.pane, &body.text, body.submit.unwrap_or(true)) {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Err(e) => fail(e),
    }
}

#[derive(Debug, Deserialize)]
struct InterruptBody {
    pane: String,
}

async fn api_interrupt(Json(body): Json<InterruptBody>) -> impl IntoResponse {
    let tmux = match crate::tmux::Tmux::new() {
        Ok(t) => t,
        Err(e) => return fail(e),
    };
    match tmux.interrupt(&body.pane) {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Err(e) => fail(e),
    }
}

fn fail(e: anyhow::Error) -> axum::response::Response {
    // Don't echo the anyhow chain (which carries tmux stderr, paths,
    // and other local environment detail) to any loopback client.
    // Log the full error server-side and return an opaque message.
    eprintln!("terhub serve: internal error: {e:#}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({"error": "internal error"})),
    )
        .into_response()
}

const DASHBOARD_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>terhub</title>
<style>
body { background: #0d1117; color: #e6edf3; font-family: monospace; margin: 0; padding: 12px; }
h1 { font-size: 18px; margin: 0 0 4px 0; }
.sub { color: #8b949e; margin-bottom: 12px; }
.grid { display: grid; grid-template-columns: 1fr 1fr; gap: 12px; }
.panel { background: #161b22; border: 1px solid #30363d; border-radius: 6px; padding: 10px; }
.panel h2 { font-size: 14px; margin: 0 0 8px 0; color: #58a6ff; }
table { border-collapse: collapse; width: 100%; font-size: 12px; }
th { text-align: left; color: #8b949e; font-weight: normal; padding: 2px 6px; }
td { padding: 3px 6px; border-top: 1px solid #21262d; cursor: pointer; }
tr.sel td { background: #1c3a4a; }
tr:hover td { background: #1c2a3a; }
.waiting, .error { color: #ff7b72; font-weight: bold; }
.working { color: #3fb950; }
.idle, .unknown { color: #8b949e; }
pre.cap { background: #0d1117; border: 1px solid #30363d; border-radius: 4px; padding: 8px;
  height: 220px; overflow: auto; white-space: pre-wrap; font-size: 12px; }
textarea { width: 100%; height: 54px; background: #0d1117; color: #e6edf3;
  border: 1px solid #30363d; border-radius: 4px; font-family: monospace; }
button { background: #238636; color: white; border: none; border-radius: 4px;
  padding: 6px 12px; cursor: pointer; margin: 4px 4px 0 0; font-family: monospace; }
button.danger { background: #84271c; }
button:hover { filter: brightness(1.2); }
.chips span { display: inline-block; background: #21262d; border-radius: 10px;
  padding: 2px 10px; margin: 0 6px 6px 0; font-size: 12px; }
.tl { font-size: 12px; max-height: 220px; overflow: auto; }
.tl div { padding: 1px 0; border-top: 1px solid #21262d; }
.msg { min-height: 16px; color: #8b949e; font-size: 12px; }
@media (max-width: 900px) { .grid { grid-template-columns: 1fr; } }
</style>
</head>
<body>
<h1>terhub <span id="counts" class="sub"></span></h1>
<div class="sub">loopback dashboard · refreshes every 2s · <span id="daemon">…</span></div>
<div class="grid">
  <div class="panel"><h2>agents</h2><div id="agents">loading…</div></div>
  <div class="panel"><h2>pane <span id="panetitle">—</span></h2>
    <pre class="cap" id="cap">click an agent to inspect</pre>
    <textarea id="prompt" placeholder="prompt for the selected pane…"></textarea><br>
    <button id="send">send + enter</button><button id="intr" class="danger">interrupt</button>
    <div class="msg" id="msg"></div>
  </div>
</div>
<div class="grid" style="margin-top:12px">
  <div class="panel"><h2>today</h2><div class="chips" id="stats">…</div></div>
  <div class="panel"><h2>timeline</h2><div class="tl" id="tl">…</div></div>
</div>
<script>
let sel = null;
async function jget(u) { const r = await fetch(u); if (!r.ok) throw new Error(u + " " + r.status); return r.json(); }
function age(s) { if (s < 60) return s + "s"; if (s < 3600) return Math.floor(s/60) + "m"; return Math.floor(s/3600) + "h" + Math.floor(s%3600/60) + "m"; }
function esc(s) { return (s||"").replace(/&/g,"&amp;").replace(/</g,"&lt;"); }
async function refresh() {
  try {
    const st = await jget("/api/status");
    document.getElementById("daemon").textContent = "daemon ok";
    const ag = st.agents || [];
    const needy = ag.filter(a => a.state === "waiting" || a.state === "error").length;
    document.getElementById("counts").textContent = ag.length + " agents · " + needy + " need you";
    let h = "<table><tr><th>pane</th><th>agent</th><th>state</th><th>for</th><th>session</th><th>summary</th></tr>";
    const now = Math.floor(Date.now()/1000);
    for (const a of ag) {
      h += `<tr data-p="${esc(a.pane)}" class="${a.pane===sel?"sel":""}"><td>${esc(a.pane)}</td><td>${esc(a.agent)}</td><td class="${esc(a.state)}">${esc(a.state)}</td><td>${age(now-(a.state_since||now))}</td><td>${esc(a.session)}</td><td>${esc(a.summary)}</td></tr>`;
    }
    document.getElementById("agents").innerHTML = h + "</table>";
    document.querySelectorAll("#agents tr[data-p]").forEach(tr => tr.onclick = () => { sel = tr.dataset.p; refreshCap(); });
  } catch (e) { document.getElementById("daemon").textContent = "daemon down"; }
  try {
    const rows = await jget("/api/stats?since=today&group=agent");
    let h = "";
    for (const r of rows) h += `<span>${esc(r.key)} W${age(r.work)} / w${age(r.wait)} / E${age(r.err)} / i${age(r.idle)}</span>`;
    document.getElementById("stats").innerHTML = h || "(no activity yet)";
  } catch (e) {}
  try {
    const spans = await jget("/api/timeline?since=today&limit=30");
    let h = "";
    for (const s of spans.slice().reverse()) h += `<div><span class="${esc(s.state)}">${esc(s.state)}</span> ${esc(s.agent)} ${esc(s.pane)} ${esc(s.session)} ${esc(s.summary||"")}</div>`;
    document.getElementById("tl").innerHTML = h || "(empty)";
  } catch (e) {}
}
async function refreshCap() {
  if (!sel) return;
  document.getElementById("panetitle").textContent = sel;
  try {
    const c = await jget("/api/capture?pane=" + encodeURIComponent(sel) + "&lines=40");
    document.getElementById("cap").textContent = c.text || "(empty)";
  } catch (e) { document.getElementById("cap").textContent = "(capture failed)"; }
}
document.getElementById("send").onclick = async () => {
  const t = document.getElementById("prompt").value;
  if (!sel || !t) return;
  const r = await fetch("/api/send", {method: "POST", headers: {"Content-Type": "application/json"},
    body: JSON.stringify({pane: sel, text: t, submit: true})});
  document.getElementById("msg").textContent = r.ok ? "sent" : "send failed";
  document.getElementById("prompt").value = "";
  refreshCap();
};
document.getElementById("intr").onclick = async () => {
  if (!sel) return;
  const r = await fetch("/api/interrupt", {method: "POST", headers: {"Content-Type": "application/json"},
    body: JSON.stringify({pane: sel})});
  document.getElementById("msg").textContent = r.ok ? "interrupted" : "interrupt failed";
  refreshCap();
};
refresh(); setInterval(refresh, 2000); setInterval(refreshCap, 3000);
</script>
</body>
</html>
"##;

#[cfg(test)]
mod tests {
    use super::{latest, stats_rows, timeline_rows};
    use crate::agent::AgentState;
    use crate::history::{Entry, GroupBy};

    #[test]
    fn latest_keeps_the_tail() {
        assert_eq!(latest(vec![1, 2, 3, 4, 5], 3), vec![3, 4, 5]);
        assert_eq!(latest(vec![1, 2], 5), vec![1, 2]);
        assert!(latest::<u8>(Vec::new(), 10).is_empty());
    }

    fn entry(t: u64, to: AgentState) -> Entry {
        Entry {
            t,
            kind: "agent".to_string(),
            pane: "%1".to_string(),
            agent: "pi".to_string(),
            from: Some(AgentState::Idle),
            to: Some(to),
            session: "s".to_string(),
            window: "w".to_string(),
            summary: String::new(),
            event: None,
        }
    }

    #[test]
    fn stats_rows_aggregate_spans() {
        // Working 100–200, Waiting 200–300.
        let entries = vec![
            entry(100, AgentState::Working),
            entry(200, AgentState::Waiting),
        ];
        let rows = stats_rows(&entries, None, GroupBy::Agent, 300);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key, "pi");
        assert_eq!(rows[0].work, 100);
        assert_eq!(rows[0].wait, 100);
        // since clips: only the 250–300 slice counts.
        let rows = stats_rows(&entries, Some(250), GroupBy::Agent, 300);
        assert_eq!(rows[0].work, 0);
        assert_eq!(rows[0].wait, 50);
    }

    #[test]
    fn dashboard_escaper_covers_all_html_contexts() {
        // The dashboard builds agent rows with innerHTML from
        // pane/agent/session/summary strings, which include untrusted
        // remote snapshot text (fleet) — a stored-XSS sink that can
        // then POST /api/send from the victim browser. esc() is the
        // single encoding point, so it must neutralize both
        // element-text (`<`, `>`, `&`) and quoted-attribute (`"`, `'`)
        // breakouts: `data-p="${esc(a.pane)}"` pops with a bare `"`
        // and no `<` needed (`" onmouseover="...`).
        let html = super::DASHBOARD_HTML;
        for needle in [
            r#"replace(/&/g,"&amp;")"#,
            r#"replace(/</g,"&lt;")"#,
            r#"replace(/>/g,"&gt;")"#,
            r#"replace(/"/g,"&quot;")"#,
            r#"replace(/'/g,"&#39;")"#,
        ] {
            assert!(
                html.contains(needle),
                "dashboard esc() must contain {needle}"
            );
        }
    }

    #[test]
    fn loopback_origins_only() {
        use super::origin_allowed;
        assert!(origin_allowed("http://127.0.0.1:8080"));
        assert!(origin_allowed("http://localhost:3000"));
        assert!(origin_allowed("http://[::1]:8080"));
        assert!(!origin_allowed("http://evil.com"));
        assert!(!origin_allowed("http://127.0.0.1.evil.com"));
        assert!(!origin_allowed("http://localhost.evil.com"));
        assert!(!origin_allowed("null"));
        assert!(!origin_allowed(""));
    }

    #[test]
    fn timeline_rows_filter_and_limit() {
        let entries = vec![
            entry(100, AgentState::Working),
            entry(200, AgentState::Waiting),
        ];
        let all = timeline_rows(&entries, None, None, None, 200, 300);
        assert_eq!(all.len(), 2);
        assert!(timeline_rows(&entries, None, None, Some("xx"), 200, 300).is_empty());
        assert_eq!(
            timeline_rows(&entries, None, Some("s"), Some("pi"), 200, 300).len(),
            2
        );
        // Limit keeps the tail (latest first? no — oldest-first, tail cut).
        let one = timeline_rows(&entries, None, None, None, 1, 300);
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].state, AgentState::Waiting);
    }
}
