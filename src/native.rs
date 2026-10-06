//! Native terminal tabs (Konsole, kitty, WezTerm) as first-class rows.
//!
//! tmux only sees its own panes; the terminals hosting them have their own
//! tabs. Each backend shells out to its terminal's control channel —
//! Konsole over `qdbus`, kitty over `kitty @`, WezTerm over `wezterm cli` —
//! so a missing tool or a dead server degrades to "no tabs", never an
//! error. Row ids are namespaced (`konsole:<kpid>:<sess>`, `kitty:<win>`,
//! `wezterm:<pane>`); fleet host names colliding with a backend name are
//! rejected so the namespaces never overlap.
//!
//! Konsole capture is the visible screen only (its scrollback offsets are
//! undocumented); kitty/WezTerm capture honors the requested line count.
//! Konsole exposes no tab-close over DBus, so native rows never offer kill.

use anyhow::{Context, Result};
use std::process::Command;

pub const BACKENDS: &[&str] = &["konsole", "kitty", "wezterm"];

/// One tab in a native terminal.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct NativeTab {
    /// Backend name ("konsole").
    pub backend: String,
    /// Backend-local id ("1535082:6" for Konsole's service pid + session).
    pub tab: String,
    /// Namespaced row id ("konsole:1535082:6").
    pub id: String,
    /// Window label ("win 1").
    pub window: String,
    /// Tab title.
    pub title: String,
    /// Session/root pid (0 when unknown).
    pub pid: u32,
    /// Foreground pid (0 when unknown).
    pub fg_pid: u32,
    /// Whether the tab currently has focus.
    pub active: bool,
}

/// True for native row ids (`konsole:…`, `kitty:…`, `wezterm:…`). Fleet
/// rows (`host:%N`) never match: backend names are reserved host names.
pub fn is_native_id(id: &str) -> bool {
    split_native_id(id).is_some()
}

/// Split a native id into (backend, backend-local id).
pub fn split_native_id(id: &str) -> Option<(&str, &str)> {
    let (backend, local) = id.split_once(':')?;
    if local.is_empty() || !BACKENDS.contains(&backend) {
        return None;
    }
    Some((backend, local))
}

fn namespaced(backend: &str, tab: &str) -> String {
    format!("{backend}:{tab}")
}

// ---------------------------------------------------------------------------
// Konsole (qdbus)
// ---------------------------------------------------------------------------

/// Konsole service names (`org.kde.konsole-<pid>`) from a bare `qdbus` list.
/// Continuation lines carry a leading space; anything else is ignored.
pub fn parse_qdbus_services(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| l.starts_with("org.kde.konsole-"))
        .map(str::to_string)
        .collect()
}

/// Child node ids from DBus introspection XML (`<node name="1"/>`).
pub fn parse_introspect_nodes(xml: &str) -> Vec<u32> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find("<node name=\"") {
        rest = &rest[i + 12..];
        if let Some(end) = rest.find('"') {
            if let Ok(n) = rest[..end].parse() {
                out.push(n);
            }
            rest = &rest[end + 1..];
        } else {
            break;
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// One integer per line (sessionList, pids); blanks and junk are skipped.
pub fn parse_int_list(text: &str) -> Vec<u32> {
    text.lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

/// Konsole pads captured lines to the window width; trailing blanks carry
/// no information. Keeps at most `lines` (0 = all).
pub fn trim_screen_lines(text: &str, lines: u32) -> String {
    let trimmed: Vec<&str> = text.lines().map(|l| l.trim_end()).collect();
    let tail = if lines == 0 || trimmed.len() <= lines as usize {
        &trimmed[..]
    } else {
        &trimmed[trimmed.len() - lines as usize..]
    };
    let mut out = tail.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

fn qdbus_bin() -> &'static str {
    // Qt5/Plasma5 systems ship qdbus6 for the Qt6 Konsole.
    for bin in ["qdbus", "qdbus6"] {
        if Command::new(bin).arg("--version").output().is_ok() {
            return bin;
        }
    }
    "qdbus"
}

fn qdbus(service: &str, path: &str, method: &str, args: &[&str]) -> Result<String> {
    let bin = qdbus_bin();
    let mut cmd = Command::new(bin);
    cmd.arg(service).arg(path).arg(method);
    for a in args {
        cmd.arg(a);
    }
    let out = cmd.output().with_context(|| format!("{bin} {method}"))?;
    if !out.status.success() {
        anyhow::bail!("{bin} {method} failed");
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn konsole_tabs() -> Result<Vec<NativeTab>> {
    let bin = qdbus_bin();
    let list = Command::new(bin)
        .output()
        .with_context(|| format!("{bin} (Konsole control channel)"))?;
    if !list.status.success() {
        anyhow::bail!("{bin} service list failed");
    }
    let services = parse_qdbus_services(&String::from_utf8_lossy(&list.stdout));
    if services.is_empty() {
        anyhow::bail!("no Konsole on the session bus");
    }
    let mut tabs = Vec::new();
    for service in &services {
        let kpid = service.rsplit('-').next().unwrap_or("?");
        let xml = qdbus(
            service,
            "/Windows",
            "org.freedesktop.DBus.Introspectable.Introspect",
            &[],
        )?;
        for win in parse_introspect_nodes(&xml) {
            let win_path = format!("/Windows/{win}");
            let sessions = parse_int_list(&qdbus(
                service,
                &win_path,
                "org.kde.konsole.Window.sessionList",
                &[],
            )?);
            let current: Option<u32> = qdbus(
                service,
                &win_path,
                "org.kde.konsole.Window.currentSession",
                &[],
            )
            .ok()
            .and_then(|t| t.trim().parse().ok());
            for sess in sessions {
                let sess_path = format!("/Sessions/{sess}");
                let title = qdbus(
                    service,
                    &sess_path,
                    "org.kde.konsole.Session.title",
                    &["1"],
                )
                .map(|t| t.trim().to_string())
                .unwrap_or_default();
                let pid: u32 = qdbus(service, &sess_path, "org.kde.konsole.Session.processId", &[])
                    .ok()
                    .and_then(|t| t.trim().parse().ok())
                    .unwrap_or(0);
                let fg_pid: u32 = qdbus(
                    service,
                    &sess_path,
                    "org.kde.konsole.Session.foregroundProcessId",
                    &[],
                )
                .ok()
                .and_then(|t| t.trim().parse().ok())
                .unwrap_or(0);
                let tab = format!("{kpid}:{sess}");
                tabs.push(NativeTab {
                    backend: "konsole".to_string(),
                    tab: tab.clone(),
                    id: namespaced("konsole", &tab),
                    window: format!("win {win}"),
                    title,
                    pid,
                    fg_pid,
                    active: current == Some(sess),
                });
            }
        }
    }
    Ok(tabs)
}

/// (service, session) for a Konsole local id (`<kpid>:<sess>`).
fn konsole_target(local: &str) -> Result<(String, u32)> {
    let (kpid, sess) = local
        .split_once(':')
        .context("bad konsole id (want <kpid>:<sess>)")?;
    let sess: u32 = sess.parse().context("bad konsole session id")?;
    Ok((format!("org.kde.konsole-{kpid}"), sess))
}

fn konsole_capture(local: &str, lines: u32) -> Result<String> {
    let (service, sess) = konsole_target(local)?;
    let text = qdbus(
        &service,
        &format!("/Sessions/{sess}"),
        "org.kde.konsole.Session.getAllDisplayedText",
        &[],
    )?;
    Ok(trim_screen_lines(&text, lines))
}

fn konsole_send(local: &str, text: &str) -> Result<()> {
    let (service, sess) = konsole_target(local)?;
    qdbus(
        &service,
        &format!("/Sessions/{sess}"),
        "org.kde.konsole.Session.sendText",
        &[text],
    )?;
    Ok(())
}

fn konsole_focus(local: &str) -> Result<()> {
    let (service, sess) = konsole_target(local)?;
    // Sessions hang off the service; only the jump needs the owning window.
    let xml = qdbus(
        &service,
        "/Windows",
        "org.freedesktop.DBus.Introspectable.Introspect",
        &[],
    )?;
    for win in parse_introspect_nodes(&xml) {
        let win_path = format!("/Windows/{win}");
        let sessions = parse_int_list(&qdbus(
            &service,
            &win_path,
            "org.kde.konsole.Window.sessionList",
            &[],
        )?);
        if sessions.contains(&sess) {
            qdbus(
                &service,
                &win_path,
                "org.kde.konsole.Window.setCurrentSession",
                &[&sess.to_string()],
            )?;
            return Ok(());
        }
    }
    anyhow::bail!("konsole session {sess} is gone")
}

fn konsole_spawn() -> Result<()> {
    let bin = qdbus_bin();
    let list = Command::new(bin).output()?;
    let services = parse_qdbus_services(&String::from_utf8_lossy(&list.stdout));
    let service = services
        .first()
        .context("no Konsole on the session bus")?;
    let xml = qdbus(
        service,
        "/Windows",
        "org.freedesktop.DBus.Introspectable.Introspect",
        &[],
    )?;
    let win = parse_introspect_nodes(&xml)
        .first()
        .copied()
        .context("Konsole has no windows")?;
    qdbus(
        service,
        &format!("/Windows/{win}"),
        "org.kde.konsole.Window.newSession",
        &[],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// kitty (`kitty @`)
// ---------------------------------------------------------------------------

/// Parse `kitty @ ls`: OS windows → tabs → windows. Shapes drift across
/// kitty versions, so everything optional is defaulted, never fatal.
pub fn parse_kitty_ls(json: &str) -> Vec<NativeTab> {
    let mut tabs = Vec::new();
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return tabs;
    };
    let windows = match value.as_array() {
        Some(w) => w,
        None => return tabs,
    };
    for os in windows {
        let os_id = os.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
        let ktabs = os.get("tabs").and_then(|v| v.as_array());
        for tab in ktabs.into_iter().flatten() {
            let tab_id = tab.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let tab_title = tab
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let wins = tab.get("windows").and_then(|v| v.as_array());
            for win in wins.into_iter().flatten() {
                let wid = win.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
                let title = win
                    .get("title")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&tab_title)
                    .to_string();
                let pid = win
                    .get("pid")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0)
                    .min(u32::MAX as u64) as u32;
                let active = win
                    .get("is_focused")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let local = wid.to_string();
                tabs.push(NativeTab {
                    backend: "kitty".to_string(),
                    tab: local.clone(),
                    id: namespaced("kitty", &local),
                    window: format!("os {os_id} tab {tab_id}"),
                    title,
                    pid,
                    fg_pid: 0,
                    active,
                });
            }
        }
    }
    tabs
}

/// Extra global args for `kitty @`: the socket comes from TERHUB_KITTY_TO
/// (plain `kitty @` only works from inside kitty itself).
fn kitty_to_args() -> Vec<String> {
    match std::env::var("TERHUB_KITTY_TO") {
        Ok(s) if !s.trim().is_empty() => vec!["--to".to_string(), s],
        _ => Vec::new(),
    }
}

fn kitty(args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("kitty");
    cmd.arg("@");
    for a in kitty_to_args() {
        cmd.arg(a);
    }
    for a in args {
        cmd.arg(a);
    }
    let out = cmd.output().context("kitty @")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("kitty @ {} failed: {}", args.join(" "), err.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn kitty_tabs() -> Result<Vec<NativeTab>> {
    let out = kitty(&["ls"]).map_err(|e| {
        if kitty_to_args().is_empty() {
            anyhow::anyhow!("{e:#} (set TERHUB_KITTY_TO to the kitty socket, or run inside kitty)")
        } else {
            e
        }
    })?;
    Ok(parse_kitty_ls(&out))
}

fn kitty_capture(local: &str, lines: u32) -> Result<String> {
    let text = kitty(&["get-text", "--extent", "all", "--match", &format!("id:{local}")])?;
    Ok(trim_screen_lines(&text, lines))
}

fn kitty_send(local: &str, text: &str) -> Result<()> {
    kitty(&["send-text", "--match", &format!("id:{local}"), text])?;
    Ok(())
}

fn kitty_focus(local: &str) -> Result<()> {
    kitty(&["focus-window", "--match", &format!("id:{local}")])?;
    Ok(())
}

fn kitty_spawn() -> Result<()> {
    kitty(&["launch", "--type", "tab"])?;
    Ok(())
}

// ---------------------------------------------------------------------------
// WezTerm (`wezterm cli`)
// ---------------------------------------------------------------------------

/// Parse `wezterm cli list --format json`. Only pane_id is required; the
/// rest degrades to tab labels.
pub fn parse_wezterm_list(json: &str) -> Vec<NativeTab> {
    let mut tabs = Vec::new();
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return tabs;
    };
    let panes = match value.as_array() {
        Some(p) => p,
        None => return tabs,
    };
    for pane in panes {
        let pane_id = match pane.get("pane_id").and_then(|v| v.as_u64()) {
            Some(n) => n,
            None => continue,
        };
        let tab_id = pane.get("tab_id").and_then(|v| v.as_u64()).unwrap_or(0);
        let window_id = pane.get("window_id").and_then(|v| v.as_u64()).unwrap_or(0);
        let title = pane
            .get("title")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("tab {tab_id}"));
        let local = pane_id.to_string();
        tabs.push(NativeTab {
            backend: "wezterm".to_string(),
            tab: local.clone(),
            id: namespaced("wezterm", &local),
            window: format!("win {window_id}"),
            title,
            pid: 0,
            fg_pid: 0,
            active: false,
        });
    }
    tabs
}

fn wezterm(args: &[&str]) -> Result<String> {
    // Never auto-start a mux server as a side effect of listing tabs.
    let mut full = vec!["cli", "--no-auto-start"];
    full.extend(args.iter().copied());
    let mut cmd = Command::new("wezterm");
    for a in &full {
        cmd.arg(a);
    }
    let out = cmd.output().context("wezterm cli")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("wezterm cli {} failed: {}", args.join(" "), err.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn wezterm_tabs() -> Result<Vec<NativeTab>> {
    Ok(parse_wezterm_list(&wezterm(&["list", "--format", "json"])?))
}

fn wezterm_capture(local: &str, lines: u32) -> Result<String> {
    let start = format!("-{}", lines.max(1));
    wezterm(&[
        "get-text",
        "--pane-id",
        local,
        "--start-line",
        &start,
        "--escapes",
    ])
}

fn wezterm_send(local: &str, text: &str) -> Result<()> {
    wezterm(&["send-text", "--pane-id", local, "--no-paste", text])?;
    Ok(())
}

fn wezterm_focus(local: &str) -> Result<()> {
    wezterm(&["activate-pane", "--pane-id", local])?;
    Ok(())
}

fn wezterm_spawn() -> Result<()> {
    wezterm(&["spawn"])?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// One backend's fetch: tabs plus the failure when there are none.
pub struct BackendReport {
    pub backend: &'static str,
    pub tabs: Vec<NativeTab>,
    pub error: Option<String>,
}

/// Enumerate every backend. Backends fail independently — a dead WezTerm
/// server never hides Konsole tabs.
pub fn list_all() -> Vec<BackendReport> {
    [("konsole", konsole_tabs as fn() -> Result<Vec<NativeTab>>),
     ("kitty", kitty_tabs),
     ("wezterm", wezterm_tabs)]
        .into_iter()
        .map(|(backend, fetch)| match fetch() {
            Ok(tabs) => BackendReport { backend, tabs, error: None },
            Err(e) => BackendReport { backend, tabs: Vec::new(), error: Some(format!("{e:#}")) },
        })
        .collect()
}

/// Screen/scrollback text for a native row id.
pub fn capture(id: &str, lines: u32) -> Result<String> {
    let (backend, local) = split_native_id(id).context("not a native row id")?;
    match backend {
        "konsole" => konsole_capture(local, lines),
        "kitty" => kitty_capture(local, lines),
        "wezterm" => wezterm_capture(local, lines),
        _ => anyhow::bail!("unknown backend {backend}"),
    }
}

/// Type into a native tab; `submit` presses Enter after.
pub fn send_text(id: &str, text: &str, submit: bool) -> Result<()> {
    let (backend, local) = split_native_id(id).context("not a native row id")?;
    let payload = if submit {
        format!("{text}\r")
    } else {
        text.to_string()
    };
    match backend {
        "konsole" => konsole_send(local, &payload),
        "kitty" => kitty_send(local, &payload),
        "wezterm" => wezterm_send(local, &payload),
        _ => anyhow::bail!("unknown backend {backend}"),
    }
}

/// Switch the terminal to a native tab (tab switch only — raising the OS
/// window is the window manager's business).
pub fn focus(id: &str) -> Result<()> {
    let (backend, local) = split_native_id(id).context("not a native row id")?;
    match backend {
        "konsole" => konsole_focus(local),
        "kitty" => kitty_focus(local),
        "wezterm" => wezterm_focus(local),
        _ => anyhow::bail!("unknown backend {backend}"),
    }
}

/// Open a fresh tab in a backend.
pub fn spawn(backend: &str) -> Result<()> {
    match backend {
        "konsole" => konsole_spawn(),
        "kitty" => kitty_spawn(),
        "wezterm" => wezterm_spawn(),
        _ => anyhow::bail!("unknown backend {backend} (want konsole|kitty|wezterm)"),
    }
}

/// /proc comm for a pid (foreground command guess).
pub fn proc_comm(pid: u32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// /proc cwd for a pid (repo dir cell).
pub fn proc_cwd(pid: u32) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/cwd"))
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_ids_split_by_backend() {
        assert_eq!(split_native_id("konsole:153:6"), Some(("konsole", "153:6")));
        assert_eq!(split_native_id("kitty:3"), Some(("kitty", "3")));
        assert_eq!(split_native_id("wezterm:7"), Some(("wezterm", "7")));
        // tmux and fleet ids are never native.
        assert_eq!(split_native_id("%1"), None);
        assert_eq!(split_native_id("dev:%1"), None);
        assert_eq!(split_native_id("konsole:"), None);
        assert_eq!(split_native_id("konsole"), None);
        assert!(is_native_id("kitty:3"));
        assert!(!is_native_id("dev:%1"));
    }

    #[test]
    fn qdbus_services_pick_konsole_lines() {
        let text = ":1.84\n org.kde.konsole-1535082\n:1.96\n org.kde.plasmashell\n";
        assert_eq!(
            parse_qdbus_services(text),
            vec!["org.kde.konsole-1535082".to_string()]
        );
        assert!(parse_qdbus_services("nothing here\n").is_empty());
    }

    #[test]
    fn introspect_nodes_parse_sorted_unique() {
        let xml = "<node><node name=\"2\"/><node name=\"1\"/><node name=\"2\"/><node name=\"x\"/></node>";
        assert_eq!(parse_introspect_nodes(xml), vec![1, 2]);
        assert!(parse_introspect_nodes("<node></node>").is_empty());
    }

    #[test]
    fn int_lists_skip_junk() {
        assert_eq!(parse_int_list("6\n24\n"), vec![6, 24]);
        assert_eq!(parse_int_list("6\nbogus\n\n24\n"), vec![6, 24]);
    }

    #[test]
    fn screen_trim_strips_padding_and_tails() {
        let text = "a  \n  \nb\n";
        assert_eq!(trim_screen_lines(text, 0), "a\n\nb\n");
        assert_eq!(trim_screen_lines(text, 2), "\nb\n");
        assert_eq!(trim_screen_lines("", 10), "");
    }

    #[test]
    fn kitty_ls_walks_os_windows() {
        let json = r#"[{"id":1,"tabs":[{"id":2,"title":"t","layout":"tall","windows":[{"id":3,"is_focused":true,"title":"w","pid":99,"cwd":"/tmp"}]}]}]"#;
        let tabs = parse_kitty_ls(json);
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0].id, "kitty:3");
        assert_eq!(tabs[0].title, "w");
        assert_eq!(tabs[0].pid, 99);
        assert!(tabs[0].active);
        // Garbage in, empty out — never a panic.
        assert!(parse_kitty_ls("bogus").is_empty());
        assert!(parse_kitty_ls("{}").is_empty());
    }

    #[test]
    fn wezterm_list_needs_only_pane_id() {
        let json = r#"[{"window_id":0,"tab_id":1,"pane_id":7,"title":""},{"pane_id":8}]"#;
        let tabs = parse_wezterm_list(json);
        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs[0].id, "wezterm:7");
        assert_eq!(tabs[0].title, "tab 1");
        assert_eq!(tabs[1].window, "win 0");
        assert!(parse_wezterm_list("bogus").is_empty());
        assert!(parse_wezterm_list(r#"[{"tab_id":1}]"#).is_empty());
    }

    #[test]
    fn bad_ids_error_cleanly() {
        assert!(capture("%1", 10).is_err());
        assert!(send_text("dev:%1", "hi", false).is_err());
        assert!(focus("nope", ).is_err());
        assert!(spawn("gnome").is_err());
    }
}
