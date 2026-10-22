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
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Output, Stdio};

pub const BACKENDS: &[&str] = &["konsole", "kitty", "wezterm"];

/// Backstop for every native-backend subprocess: a wedged helper must
/// never freeze the TUI (real `kitty @` outside kitty waits 10s on
/// /dev/tty for a terminal reply that never comes).
const NATIVE_CMD_TIMEOUT_SECS: u64 = 5;

/// Hard cap on each pipe we drain. A chatty or malicious helper
/// (e.g. a runaway `kitty @ get-text --extent all`) can otherwise
/// balloon RSS without bound; we keep the first 1MB and throw the
/// rest away. The pipe itself is still fully drained so the child
/// never blocks on a full pipe.
const PIPE_CAP_BYTES: usize = 1_000_000;

/// Run `cmd` to completion, killing it past the timeout. Pipes drain on
/// reader threads so a chatty child can't deadlock the wait; stdin is
/// null so no child ever reads the terminal. Output is capped at
/// [`PIPE_CAP_BYTES`] per pipe (the rest is discarded after a full
/// drain, so a chatty helper can't balloon memory and a misbehaving
/// one still gets reaped).
fn run_timed(cmd: Command, what: &str) -> Result<Output> {
    run_with_timeout(cmd, what, NATIVE_CMD_TIMEOUT_SECS)
}

/// Same as [`run_timed`] but with a caller-supplied timeout. Public so
/// other modules (e.g. the tray's `focus_hub` xdotool calls) can bound
/// helpers that would otherwise hang the dispatch path.
pub fn run_with_timeout(mut cmd: Command, what: &str, secs: u64) -> Result<Output> {
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    use std::time::{Duration, Instant};
    // New process group so a wedged child can't strand a grandchild
    // holding our pipes open (killing the group on timeout reaps the
    // whole tree and the reader threads unblock immediately).
    cmd.process_group(0);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().with_context(|| format!("spawn {what}"))?;
    // Two reader threads (stdout/stderr are distinct types, so no
    // loop). Each drains its pipe to EOF so the child can keep
    // writing without blocking on a full kernel buffer, then
    // truncates what we keep to PIPE_CAP_BYTES.
    let mut out_pipe = child.stdout.take();
    let out_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = out_pipe.as_mut() {
            let _ = p.read_to_end(&mut buf);
        }
        if buf.len() > PIPE_CAP_BYTES {
            buf.truncate(PIPE_CAP_BYTES);
        }
        buf
    });
    let err_pipe = child.stderr.take();
    let err_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut p) = err_pipe {
            let _ = p.read_to_end(&mut buf);
        }
        if buf.len() > PIPE_CAP_BYTES {
            buf.truncate(PIPE_CAP_BYTES);
        }
        buf
    });
    let deadline = Instant::now() + Duration::from_secs(secs);
    let status = loop {
        match child.try_wait()? {
            Some(s) => break s,
            None if Instant::now() >= deadline => {
                // Kill the whole process group, not just the child:
                // the child may have exec'd a helper (kitty, qdbus)
                // that spawned its own grandchildren holding our pipes.
                let pgid = child.id() as i32;
                unsafe { libc::killpg(pgid, libc::SIGKILL) };
                let _ = child.wait();
                // Do NOT join the readers here. A join waits for EOF on
                // the pipe, and EOF only arrives once every writer is
                // gone: a descendant that escaped the killed group
                // (setsid, a daemonizing helper) still holds the write
                // end, so the join would block past the deadline --
                // the call would hang exactly when the timeout was
                // supposed to save it. The readers are detached
                // instead; they end when their pipe closes, and the
                // straggler holding it open is the very process that
                // is already past its deadline. The sibling drain
                // helper (automation::wait_drained_in_group) makes the
                // same choice for the same reason.
                drop(out_handle);
                drop(err_handle);
                anyhow::bail!("{what} timed out after {secs}s");
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    // On the happy path the readers will have finished by now
    // (child exited → pipes closed → read_to_end returned). Join so
    // a panic in a reader doesn't go silent.
    let stdout = out_handle.join().unwrap_or_default();
    let stderr = err_handle.join().unwrap_or_default();
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

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
    text.lines().filter_map(|l| l.trim().parse().ok()).collect()
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

/// Pick the first working `qdbus` binary, cached for the process so the
/// 4+ calls `konsole_tabs` makes per session don't re-probe each time.
/// (Qt5/Plasma5 systems ship `qdbus6` for the Qt6 Konsole.)
fn qdbus_bin() -> &'static str {
    static CACHE: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| {
        for bin in ["qdbus", "qdbus6"] {
            let mut probe = Command::new(bin);
            probe.arg("--version");
            if run_timed(probe, &format!("{bin} --version")).is_ok_and(|o| o.status.success()) {
                return bin;
            }
        }
        "qdbus"
    })
}

fn qdbus(service: &str, path: &str, method: &str, args: &[&str]) -> Result<String> {
    let bin = qdbus_bin();
    let mut cmd = Command::new(bin);
    cmd.arg(service).arg(path).arg(method);
    for a in args {
        cmd.arg(a);
    }
    let out = run_timed(cmd, &format!("{bin} {method}"))?;
    if !out.status.success() {
        anyhow::bail!("{bin} {method} failed");
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn konsole_tabs() -> Result<Vec<NativeTab>> {
    let bin = qdbus_bin();
    let list = run_timed(
        Command::new(bin),
        &format!("{bin} (Konsole control channel)"),
    )?;
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
                let title = qdbus(service, &sess_path, "org.kde.konsole.Session.title", &["1"])
                    .map(|t| t.trim().to_string())
                    .unwrap_or_default();
                let pid: u32 = qdbus(
                    service,
                    &sess_path,
                    "org.kde.konsole.Session.processId",
                    &[],
                )
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

/// One-shot KWin script: activate the window of `pid`, preferring the
/// one whose caption carries the tab `title` (multi-window Konsole
/// shares one pid, so the title picks the owning window).
fn kwin_focus_script(pid: u32, title: &str) -> String {
    let want = serde_json::to_string(title).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        "const wins = workspace.windowList();\n\
         let any = null, best = null;\n\
         for (const w of wins) {{\n\
         if (w.pid !== {pid}) continue;\n\
         any = w;\n\
         if (w.caption.indexOf({want}) !== -1) best = w;\n\
         }}\n\
         const t = best || any;\n\
         if (t) {{ t.minimized = false; workspace.activeWindow = t; }}\n"
    )
}

/// Write the KWin focus script to a fresh, attacker-unpredictable
/// path under a dedicated 0700 directory in the system temp dir,
/// then return the path so the caller can write the script body and
/// hand it to KWin. The file is created with `O_EXCL` (no
/// follow-symlink) and chmod 0600 so a local attacker who
/// pre-creates the path cannot race a symlink in, and the script
/// is unreadable to anyone but the calling user.
fn kwin_temp_path(pid: u32) -> std::io::Result<std::path::PathBuf> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let dir = std::env::temp_dir().join("terhub-kwin");
    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = dir.join(format!("focus-{pid}-{nonce}.js"));
    // create_new -> O_CREAT|O_EXCL: fails if the path already exists
    // (the pre-create attack).
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    Ok(path)
}

/// Raise a window by PID through KWin scripting (Wayland has no
/// xdotool). The script file is removed afterwards; non-Plasma
/// compositors fail fast at loadScript.
fn kwin_activate(pid: u32, title: &str) -> Result<()> {
    let path = kwin_temp_path(pid)?;
    std::fs::write(&path, kwin_focus_script(pid, title))?;
    let arg = path.to_string_lossy().to_string();
    let started = qdbus(
        "org.kde.KWin",
        "/Scripting",
        "org.kde.kwin.Scripting.loadScript",
        &[&arg, "terhub-focus"],
    )
    .and_then(|_| {
        qdbus(
            "org.kde.KWin",
            "/Scripting",
            "org.kde.kwin.Scripting.start",
            &[],
        )
    });
    let _ = qdbus(
        "org.kde.KWin",
        "/Scripting",
        "org.kde.kwin.Scripting.unloadScript",
        &["terhub-focus"],
    );
    let _ = std::fs::remove_file(&path);
    started.map(|_| ())
}

fn konsole_focus(local: &str) -> Result<()> {
    let (service, sess) = konsole_target(local)?;
    let kpid: u32 = local
        .split_once(':')
        .and_then(|(k, _)| k.parse().ok())
        .context("bad konsole id (want <kpid>:<sess>)")?;
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
            // Selecting the tab is invisible while Konsole sits behind
            // other windows, so the jump also raises it.
            let title = qdbus(
                &service,
                &format!("/Sessions/{sess}"),
                "org.kde.konsole.Session.title",
                &["1"],
            )
            .map(|t| t.trim().to_string())
            .unwrap_or_default();
            kwin_activate(kpid, &title)
                .with_context(|| format!("konsole tab {sess} selected but window raise failed"))?;
            return Ok(());
        }
    }
    anyhow::bail!("konsole session {sess} is gone")
}

fn konsole_spawn() -> Result<()> {
    let bin = qdbus_bin();
    let list = run_timed(
        Command::new(bin),
        &format!("{bin} (Konsole control channel)"),
    )?;
    let services = parse_qdbus_services(&String::from_utf8_lossy(&list.stdout));
    let service = services.first().context("no Konsole on the session bus")?;
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

/// Whether a `kitty @` spawn may run: an explicit socket, or signs of
/// running inside kitty. Outside kitty with no socket, `kitty @` opens
/// /dev/tty and waits 10s for a terminal reply that never comes — so
/// the spawn is skipped, not attempted. Pure for testing; `kitty`
/// feeds it the live environment.
fn kitty_probe_allowed(to: &str, listen_on: &str, window_id: &str, term: &str) -> bool {
    !to.trim().is_empty()
        || !listen_on.trim().is_empty()
        || !window_id.trim().is_empty()
        || term == "xterm-kitty"
}

fn kitty_reachable() -> bool {
    let var = |k: &str| std::env::var(k).unwrap_or_default();
    kitty_probe_allowed(
        &var("TERHUB_KITTY_TO"),
        &var("KITTY_LISTEN_ON"),
        &var("KITTY_WINDOW_ID"),
        &var("TERM"),
    )
}

fn kitty(args: &[&str]) -> Result<String> {
    if !kitty_reachable() {
        anyhow::bail!(
            "kitty @ {} skipped: not inside kitty (set TERHUB_KITTY_TO to the kitty socket, or run inside kitty)",
            args.join(" ")
        );
    }
    let mut cmd = Command::new("kitty");
    cmd.arg("@");
    for a in kitty_to_args() {
        cmd.arg(a);
    }
    for a in args {
        cmd.arg(a);
    }
    let out = run_timed(cmd, &format!("kitty @ {}", args.join(" ")))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("kitty @ {} failed: {}", args.join(" "), err.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn kitty_tabs() -> Result<Vec<NativeTab>> {
    // The not-inside-kitty hint lives in the kitty() gate error; real
    // failures here surface the helper's own stderr instead.
    Ok(parse_kitty_ls(&kitty(&["ls"])?))
}

fn kitty_capture(local: &str, lines: u32) -> Result<String> {
    let text = kitty(&[
        "get-text",
        "--extent",
        "all",
        "--match",
        &format!("id:{local}"),
    ])?;
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
    let out = run_timed(cmd, "wezterm cli")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("wezterm cli {} failed: {}", args.join(" "), err.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn wezterm_tabs() -> Result<Vec<NativeTab>> {
    Ok(parse_wezterm_list(&wezterm(&["list", "--format", "json"])?))
}

/// Lines back for wezterm get-text `--start-line`. `lines == 0` means
/// all — the same convention as [`trim_screen_lines`] — via a large
/// negative start that wezterm clamps to the top of the scrollback.
const WEZTERM_ALL_LINES: u32 = 1_000_000;

fn wezterm_start_line(lines: u32) -> String {
    if lines == 0 {
        format!("-{WEZTERM_ALL_LINES}")
    } else {
        format!("-{lines}")
    }
}

fn wezterm_capture(local: &str, lines: u32) -> Result<String> {
    let start = wezterm_start_line(lines);
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
    /// The probe failed but the cause is "nothing to report" (no Konsole
    /// service on the bus, no kitty socket, no WezTerm server): the
    /// backend is simply absent on this machine. Informational — display
    /// tones it down (dim, never the warning color). Real failures
    /// (missing binary, wedged CLI, malformed output) keep this false.
    pub informational: bool,
}

/// Enumerate every backend. Backends probe concurrently and fail
/// independently — a dead WezTerm server never hides Konsole tabs, and
/// one wedged helper costs one timeout, not the sum. Reports come back
/// in [`BACKENDS`] order regardless of finish order.
/// True when a probe error means "this backend has no instances on this
/// machine" (absent, not broken): the TUI tones these down to the dim
/// hint color instead of the warning color. A missing binary, a wedged
/// CLI, a timed-out command, or malformed output are real failures and
/// keep the warning treatment. The checks run on the *first line* of
/// the anyhow report (the bail message), which is where these phrases
/// live.
fn is_absent_backend_error(backend: &str, msg: &str) -> bool {
    let first = msg.lines().next().unwrap_or(msg).to_lowercase();
    match backend {
        "konsole" => first.contains("no konsole on the session bus"),
        "kitty" => first.contains("not inside kitty"),
        // The wezterm error embeds the CLI's own stderr, and that is
        // where "weztterm ... 13:40:13.529  ERROR" lines land: no server
        // / not-logged-in / connection-refused all mean "none here".
        "wezterm" => {
            first.contains("no wezterm server")
                || first.contains("no server")
                || first.contains("not logged in")
                || first.contains("connection refused")
                || first.contains("no connection")
                || first.contains("no such process")
                || first.contains("failed to connect")
                || first.contains("no socket")
        }
        _ => false,
    }
}

pub fn list_all() -> Vec<BackendReport> {
    std::thread::scope(|s| {
        let probes = [
            ("konsole", s.spawn(konsole_tabs)),
            ("kitty", s.spawn(kitty_tabs)),
            ("wezterm", s.spawn(wezterm_tabs)),
        ];
        probes
            .into_iter()
            .map(|(backend, handle)| {
                let (tabs, error, informational) = match handle.join() {
                    Ok(Ok(tabs)) => (tabs, None, false),
                    Ok(Err(e)) => {
                        let msg = format!("{e:#}");
                        (
                            Vec::new(),
                            Some(msg.clone()),
                            is_absent_backend_error(backend, &msg),
                        )
                    }
                    Err(_) => (
                        Vec::new(),
                        Some("backend probe panicked".to_string()),
                        false,
                    ),
                };
                BackendReport {
                    backend,
                    tabs,
                    error,
                    informational,
                }
            })
            .collect()
    })
}

/// Screen/scrollback text for a native row id. `lines` caps the tail;
/// 0 means all, on every backend.
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
    fn a_grandchild_that_escapes_the_group_cannot_hold_the_timeout() {
        // The child forks a setsid grandchild that inherits the stdout
        // pipe and sleeps. The group kill cannot reach it (it left the
        // group), so joining the pipe readers would wait for an EOF
        // that never comes: the call would hang past its own deadline.
        // The readers are detached instead, so the timeout fires.
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c")
            .arg("setsid sh -c 'sleep 30' & sleep 30");
        let start = std::time::Instant::now();
        let res = run_with_timeout(cmd, "escapee", 1);
        let elapsed = start.elapsed();
        assert!(res.is_err(), "the call reported success: {res:?}");
        assert!(
            elapsed < std::time::Duration::from_secs(8),
            "the timeout did not fire: {elapsed:?}"
        );
    }

    #[test]
    fn absent_backend_errors_are_informational() {
        // No instances: tone down, not warn.
        assert!(is_absent_backend_error(
            "konsole",
            "no Konsole on the session bus"
        ));
        assert!(is_absent_backend_error(
            "kitty",
            "kitty @ ls skipped: not inside kitty (set TERHUB_KITTY_TO ...)"
        ));
        assert!(is_absent_backend_error(
            "wezterm",
            "wezterm cli list failed: No such process (wezt server not running)"
        ));
        // Live-shape: the CLI's own "failed to connect to Socket(...)" line.
        assert!(is_absent_backend_error(
            "wezterm",
            "wezterm cli list --format json failed: ERROR wezterm > failed to connect to Socket(\"/run/user/1000/wezterm/gui-sock-1\")"
        ));
        assert!(is_absent_backend_error(
            "wezterm",
            "wezterm cli list failed: Connection refused"
        ));
        // Real failures stay loud.
        assert!(!is_absent_backend_error(
            "konsole",
            "qdbus service list failed"
        ));
        assert!(!is_absent_backend_error(
            "kitty",
            "kitty @ ls failed: socket gone"
        ));
        assert!(!is_absent_backend_error(
            "wezterm",
            "wezterm cli list failed: malformed json"
        ));
    }

    #[test]
    fn kitty_probe_allowed_matrix() {
        // Outside kitty with no socket: skip (the spawn would wait 10s
        // on /dev/tty for a reply only kitty sends).
        assert!(!kitty_probe_allowed("", "", "", "tmux-256color"));
        assert!(!kitty_probe_allowed("", "", "", "xterm-256color"));
        assert!(!kitty_probe_allowed("  ", "", "", "tmux-256color"));
        // An explicit socket always allows the probe.
        assert!(kitty_probe_allowed("/tmp/kitty", "", "", "tmux-256color"));
        // Inside kitty: any one of these is proof enough.
        assert!(kitty_probe_allowed("", "/tmp/kitty", "", "tmux-256color"));
        assert!(kitty_probe_allowed("", "", "1", "tmux-256color"));
        assert!(kitty_probe_allowed("", "", "", "xterm-kitty"));
    }

    #[test]
    fn run_timed_returns_output_and_kills_wedges() {
        let out = run_timed(Command::new("echo"), "echo").unwrap();
        assert!(out.status.success());
        // A wedged child is killed at the timeout, not awaited. A
        // 1s caller-supplied deadline keeps this fast; the production
        // 5s default is proven by the kitty integration test instead.
        // Two-sided timing: the lower bound proves the full deadline
        // was waited (an upper bound alone passes even if the timeout
        // fires instantly for the wrong reason), the upper bound is
        // hang protection. Load can only stretch elapsed, never
        // shrink it, so the lower bound cannot flake.
        let start = std::time::Instant::now();
        let err = run_with_timeout(
            {
                let mut c = Command::new("sleep");
                c.arg("30");
                c
            },
            "sleep",
            1,
        )
        .expect_err("sleep 30 should time out");
        assert!(
            format!("{err:#}").contains("timed out"),
            "unexpected error: {err:#}"
        );
        let elapsed = start.elapsed();
        assert!(
            elapsed >= std::time::Duration::from_secs(1),
            "returned before the 1s deadline: timeout path not taken ({elapsed:?})"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "timeout not enforced ({elapsed:?})"
        );
    }

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
    fn kwin_focus_script_targets_pid_and_title() {
        let script = kwin_focus_script(1535082, "tab six");
        assert!(script.contains("w.pid !== 1535082"), "{script}");
        assert!(script.contains(r#"indexOf("tab six")"#), "{script}");
        assert!(script.contains("minimized = false"), "{script}");
        assert!(script.contains("workspace.activeWindow = t"), "{script}");
        // Titles are JSON-escaped so quotes cannot break the script.
        let quoted = kwin_focus_script(1, r#"say "hi""#);
        assert!(quoted.contains(r#"indexOf("say \"hi\"")"#), "{quoted}");
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
        let xml =
            "<node><node name=\"2\"/><node name=\"1\"/><node name=\"2\"/><node name=\"x\"/></node>";
        assert_eq!(parse_introspect_nodes(xml), vec![1, 2]);
        assert!(parse_introspect_nodes("<node></node>").is_empty());
    }

    #[test]
    fn int_lists_skip_junk() {
        assert_eq!(parse_int_list("6\n24\n"), vec![6, 24]);
        assert_eq!(parse_int_list("6\nbogus\n\n24\n"), vec![6, 24]);
    }

    #[test]
    fn wezterm_zero_lines_means_all() {
        // Backend parity: konsole/kitty treat 0 as all via
        // trim_screen_lines, so capture(id, 0) must agree on wezterm.
        assert_eq!(wezterm_start_line(5), "-5");
        assert_eq!(wezterm_start_line(1), "-1");
        assert_eq!(
            wezterm_start_line(0),
            format!("-{WEZTERM_ALL_LINES}"),
            "lines=0 must mean all, not 1 line"
        );
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
        assert!(focus("nope",).is_err());
        assert!(spawn("gnome").is_err());
    }
}
