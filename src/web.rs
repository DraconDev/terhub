//! Web: loopback-only HTTP dashboard.
//!
//! `terhub serve` binds 127.0.0.1 (never anything else) and serves an
//! embedded single-page UI plus JSON APIs: status, stats, timeline, capture,
//! and the prompt (send) / abort (interrupt) actions.
use axum::{
    Json, Router,
    extract::Query,
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
    let spans = h::build_spans(&h::read_entries(&h::ledger_path()), now);
    let rows = h::stats(&spans, since, group);
    ok_json(&rows)
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
    let mut spans = h::build_spans(&h::read_entries(&h::ledger_path()), now);
    spans.retain(|s| {
        since.is_none_or(|c| s.end >= c)
            && q.session.as_ref().is_none_or(|f| s.session.contains(f))
            && q.agent.as_ref().is_none_or(|f| s.agent.contains(f))
    });
    let limit = q.limit.unwrap_or(200).clamp(1, 2000);
    let spans = latest(spans, limit);
    ok_json(&spans)
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
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({"error": format!("{e:?}")})),
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
    use super::latest;

    #[test]
    fn latest_keeps_the_tail() {
        assert_eq!(latest(vec![1, 2, 3, 4, 5], 3), vec![3, 4, 5]);
        assert_eq!(latest(vec![1, 2], 5), vec![1, 2]);
        assert!(latest::<u8>(Vec::new(), 10).is_empty());
    }
}
