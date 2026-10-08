//! KDE/Plasma tray icon (`terhub tray`).
//!
//! Registers a StatusNotifierItem and keeps it in step with the daemon:
//! the icon switches to its attention variant when something needs you,
//! and the tooltip carries the counts. KDE's tray cannot draw a numeric
//! badge — the StatusNotifierItem spec has no field for one — so the
//! numbers live in the tooltip instead.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::agent::{AgentRecord, AgentState};

const POLL: Duration = Duration::from_secs(3);
const SNAP_TIMEOUT: Duration = Duration::from_secs(2);
const SNI_PATH: &str = "/StatusNotifierItem";

/// Icon themes only resolve names they have files for, so the SVGs are
/// written into the user's icon dir on first run.
/// Icon themes only resolve names they have files for, so the SVGs are
/// written into the user's icon dir on first run. The set is the same
/// across runs; the actual contents are themed at write time.
const ICON_STATES: &[(IconState, &str); 6] = &[
    (IconState::Calm, "terhub.svg"),
    (IconState::Attention, "terhub-attention.svg"),
    (IconState::Idle, "terhub-idle.svg"),
    (IconState::Disconnected, "terhub-disconnected.svg"),
    (IconState::Live, "terhub-live.svg"),
    (IconState::LiveIdle, "terhub-live-idle.svg"),
];

/// One state the icon can be in. Distinct from [`TrayView`] so the
/// renderer stays pure and is testable with a 5-element palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconState {
    Calm,
    Idle,
    Attention,
    Live,
    LiveIdle,
    Disconnected,
}

impl TrayView {
    pub fn icon_state(&self) -> IconState {
        if !self.connected {
            return IconState::Disconnected;
        }
        if self.needs_attention() {
            return IconState::Attention;
        }
        match (self.hub_open, self.idle) {
            (true, 0) => IconState::Live,
            (true, _) => IconState::LiveIdle,
            (false, 0) => IconState::Calm,
            (false, _) => IconState::Idle,
        }
    }
}

/// Five color accents the T-mark pulls from the active theme. Each
/// plays a specific role in the icon: tile and stem are the everyday
/// surface, focus and header add visual interest, attached and a
/// reserved alarm handle the "needs you" case so a theme's existing
/// red/yellow does the alarm work without us overriding it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TrayPalette {
    /// Rounded-square tile behind the T.
    tile: &'static str,
    /// T crossbar gradient start (left edge) and end (right edge).
    crossbar_a: &'static str,
    crossbar_b: &'static str,
    /// T vertical stem left half.
    stem_a: &'static str,
    /// T vertical stem right half (focus color, low opacity in idle).
    stem_b: &'static str,
    /// Live corner dot. Stays green across themes — "running" is a
    /// universal signal, not a theme expression.
    live_dot: &'static str,
    /// Attention badge background (alarm) and foreground (the plus).
    alarm_bg: &'static str,
    alarm_fg: &'static str,
}

/// Look up a theme's tray palette. Unknown themes fall back to dracula;
/// the TUI's palette() has the same fallback, so the icon and the panel
/// stay in lockstep.
fn palette_for_theme(theme: &str) -> TrayPalette {
    match theme {
        "gruvbox" => TrayPalette {
            tile: "#3c3836",
            crossbar_a: "#fabd2f",
            crossbar_b: "#b8bb26",
            stem_a: "#fabd2f",
            stem_b: "#fe8019",
            live_dot: "#b8bb26",
            alarm_bg: "#fb4934",
            alarm_fg: "#282828",
        },
        "dracula" => TrayPalette {
            tile: "#282a36",
            crossbar_a: "#8be9fd",
            crossbar_b: "#f1fa8c",
            stem_a: "#8be9fd",
            stem_b: "#ff79c6",
            live_dot: "#50fa7b",
            alarm_bg: "#ff5555",
            alarm_fg: "#f8f8f2",
        },
        "nord" => TrayPalette {
            tile: "#3b4252",
            crossbar_a: "#88c0d0",
            crossbar_b: "#ebcb8b",
            stem_a: "#88c0d0",
            stem_b: "#b48ead",
            live_dot: "#a3be8c",
            alarm_bg: "#bf616a",
            alarm_fg: "#eceff4",
        },
        "solarized" => TrayPalette {
            tile: "#002b36",
            crossbar_a: "#268bd2",
            crossbar_b: "#b58900",
            stem_a: "#268bd2",
            stem_b: "#6c71c4",
            live_dot: "#859900",
            alarm_bg: "#dc322f",
            alarm_fg: "#fdf6e3",
        },
        "tokyonight" => TrayPalette {
            tile: "#1f2335",
            crossbar_a: "#7dcfff",
            crossbar_b: "#e0af68",
            stem_a: "#7dcfff",
            stem_b: "#bb9af7",
            live_dot: "#9ece6a",
            alarm_bg: "#f7768e",
            alarm_fg: "#1f2335",
        },
        "catppuccin" => TrayPalette {
            tile: "#1e2030",
            crossbar_a: "#89b4fa",
            crossbar_b: "#f9e2af",
            stem_a: "#89b4fa",
            stem_b: "#cba6f7",
            live_dot: "#a6e3a1",
            alarm_bg: "#f38ba8",
            alarm_fg: "#11111b",
        },
        "onedark" => TrayPalette {
            tile: "#282c34",
            crossbar_a: "#56b6c2",
            crossbar_b: "#e5c07b",
            stem_a: "#56b6c2",
            stem_b: "#c678dd",
            live_dot: "#98c379",
            alarm_bg: "#e06c75",
            alarm_fg: "#282c34",
        },
        "monokai" => TrayPalette {
            tile: "#272822",
            crossbar_a: "#a6e22e",
            crossbar_b: "#e6db74",
            stem_a: "#a6e22e",
            stem_b: "#f92672",
            live_dot: "#a6e22e",
            alarm_bg: "#f92672",
            alarm_fg: "#f8f8f2",
        },
        "light" => TrayPalette {
            tile: "#eee8d5",
            crossbar_a: "#268bd2",
            crossbar_b: "#b58900",
            stem_a: "#268bd2",
            stem_b: "#6c71c4",
            live_dot: "#859900",
            alarm_bg: "#dc322f",
            alarm_fg: "#fdf6e3",
        },
        // Default fallback matches dracula.
        _ => TrayPalette {
            tile: "#282a36",
            crossbar_a: "#8be9fd",
            crossbar_b: "#f1fa8c",
            stem_a: "#8be9fd",
            stem_b: "#ff79c6",
            live_dot: "#50fa7b",
            alarm_bg: "#ff5555",
            alarm_fg: "#f8f8f2",
        },
    }
}

/// State × palette → SVG. The T-mark carries three colors
/// (crossbar gradient + stem two-tone) so the icon changes with the
/// theme, while the corner accent (none / red / green / strikethrough)
/// carries the alert signal. The same brand mark, themed.
fn render_svg(state: IconState, p: TrayPalette) -> String {
    let body = match state {
        IconState::Calm => format!(
            "<rect x=\"4.4\" y=\"5.4\" width=\"13.2\" height=\"2.4\" rx=\"0.5\" fill=\"url(#cb)\"/>\
             <rect x=\"9.0\" y=\"5.4\" width=\"2.0\" height=\"11\" fill=\"{stem_a}\"/>\
             <rect x=\"11.0\" y=\"5.4\" width=\"2.0\" height=\"11\" fill=\"{stem_b}\"/>",
            stem_a = p.stem_a,
            stem_b = p.stem_b,
        ),
        IconState::Idle => format!(
            "<rect x=\"4.4\" y=\"5.4\" width=\"13.2\" height=\"2.4\" rx=\"0.5\" fill=\"url(#cb)\" opacity=\"0.85\"/>\
             <rect x=\"9.0\" y=\"5.4\" width=\"2.0\" height=\"11\" fill=\"{stem_a}\"/>\
             <rect x=\"11.0\" y=\"5.4\" width=\"2.0\" height=\"11\" fill=\"{stem_b}\" opacity=\"0.55\"/>",
            stem_a = p.stem_a,
            stem_b = p.stem_b,
        ),
        IconState::Attention => format!(
            "<rect x=\"4.4\" y=\"5.4\" width=\"13.2\" height=\"2.4\" rx=\"0.5\" fill=\"url(#cb)\"/>\
             <rect x=\"9.0\" y=\"5.4\" width=\"2.0\" height=\"11\" fill=\"{stem_a}\"/>\
             <rect x=\"11.0\" y=\"5.4\" width=\"2.0\" height=\"11\" fill=\"{stem_b}\"/>\
             <circle cx=\"17.4\" cy=\"4.6\" r=\"3.0\" fill=\"{abg}\"/>\
             <path d=\"M16.3 4.6 L18.5 4.6 M17.4 3.5 L17.4 5.7\" stroke=\"{afg}\" stroke-width=\"1.2\" stroke-linecap=\"round\"/>",
            stem_a = p.stem_a,
            stem_b = p.stem_b,
            abg = p.alarm_bg,
            afg = p.alarm_fg,
        ),
        IconState::Live => format!(
            "<rect x=\"4.4\" y=\"5.4\" width=\"13.2\" height=\"2.4\" rx=\"0.5\" fill=\"url(#cb)\"/>\
             <rect x=\"9.0\" y=\"5.4\" width=\"2.0\" height=\"11\" fill=\"{stem_a}\"/>\
             <rect x=\"11.0\" y=\"5.4\" width=\"2.0\" height=\"11\" fill=\"{stem_b}\"/>\
             <circle cx=\"17.6\" cy=\"4.6\" r=\"2.6\" fill=\"{tile}\" opacity=\"0.95\"/>\
             <circle cx=\"17.6\" cy=\"4.6\" r=\"1.6\" fill=\"{dot}\"/>",
            stem_a = p.stem_a,
            stem_b = p.stem_b,
            tile = p.tile,
            dot = p.live_dot,
        ),
        IconState::LiveIdle => format!(
            "<rect x=\"4.4\" y=\"5.4\" width=\"13.2\" height=\"2.4\" rx=\"0.5\" fill=\"url(#cb)\" opacity=\"0.85\"/>\
             <rect x=\"9.0\" y=\"5.4\" width=\"2.0\" height=\"11\" fill=\"{stem_a}\"/>\
             <rect x=\"11.0\" y=\"5.4\" width=\"2.0\" height=\"11\" fill=\"{stem_b}\" opacity=\"0.55\"/>\
             <circle cx=\"17.6\" cy=\"4.6\" r=\"2.6\" fill=\"{tile}\" opacity=\"0.95\"/>\
             <circle cx=\"17.6\" cy=\"4.6\" r=\"1.6\" fill=\"{dot}\"/>",
            stem_a = p.stem_a,
            stem_b = p.stem_b,
            tile = p.tile,
            dot = p.live_dot,
        ),
        IconState::Disconnected => format!(
            "<rect x=\"4.4\" y=\"5.4\" width=\"13.2\" height=\"2.4\" rx=\"0.5\" fill=\"{stem_b}\" opacity=\"0.4\"/>\
             <rect x=\"9.0\" y=\"5.4\" width=\"2.0\" height=\"11\" fill=\"{stem_b}\" opacity=\"0.5\"/>\
             <rect x=\"11.0\" y=\"5.4\" width=\"2.0\" height=\"11\" fill=\"{stem_b}\" opacity=\"0.4\"/>\
             <line x1=\"2\" y1=\"20\" x2=\"20\" y2=\"2\" stroke=\"{tile}\" stroke-width=\"2.4\" stroke-linecap=\"round\"/>\
             <line x1=\"2\" y1=\"20\" x2=\"20\" y2=\"2\" stroke=\"{stem_b}\" stroke-width=\"1.3\" stroke-linecap=\"round\"/>",
            stem_b = p.stem_b,
            tile = p.tile,
        ),
    };
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"22\" height=\"22\" viewBox=\"0 0 22 22\">\
         <defs>\
           <linearGradient id=\"cb\" x1=\"0\" y1=\"0\" x2=\"1\" y2=\"0\">\
             <stop offset=\"0%\" stop-color=\"{a}\"/>\
             <stop offset=\"100%\" stop-color=\"{b}\"/>\
           </linearGradient>\
         </defs>\
         <rect x=\"1\" y=\"1\" width=\"20\" height=\"20\" rx=\"5\" fill=\"{tile}\"/>\
         {body}\
         </svg>",
        a = p.crossbar_a,
        b = p.crossbar_b,
        tile = p.tile,
        body = body,
    )
}

/// Install the tray SVGs into the user's icon theme, themed to the
/// active `theme` config. Same shape on disk across runs; each file is
/// rewritten only when its rendered content changes.
pub fn install_icons(theme: &str) -> std::io::Result<PathBuf> {
    let dir = data_home().join("icons/hicolor/scalable/apps");
    std::fs::create_dir_all(&dir)?;
    let palette = palette_for_theme(theme);
    for (state, name) in ICON_STATES {
        let path = dir.join(name);
        let svg = render_svg(*state, palette);
        // Only write when changed: Plasma watches these dirs, and a
        // needless rewrite on every launch would churn its cache.
        if std::fs::read_to_string(&path).ok().as_deref() != Some(svg.as_str()) {
            std::fs::write(&path, svg)?;
        }
    }
    Ok(dir)
}

/// What the tray shows, derived from one daemon snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayView {
    pub needy: usize,
    pub idle: usize,
    pub total: usize,
    /// True when the daemon could not be reached.
    pub connected: bool,
    /// A live hub TUI the click should focus instead of opening another.
    pub hub_open: bool,
}

impl TrayView {
    pub fn from_agents(agents: &[AgentRecord]) -> Self {
        TrayView {
            needy: agents.iter().filter(|a| a.state.needs_attention()).count(),
            idle: agents
                .iter()
                .filter(|a| a.state == AgentState::Idle)
                .count(),
            total: agents.len(),
            connected: true,
            hub_open: false,
        }
    }

    /// The daemon being down is a different fact from "nothing to do",
    /// so it never renders as a misleading zero.
    pub fn disconnected() -> Self {
        TrayView {
            connected: false,
            ..TrayView::default()
        }
    }

    /// Tooltip title: the headline a glance should answer.
    pub fn title(&self) -> String {
        if !self.connected {
            return "terhub — daemon down".to_string();
        }
        match (self.needy, self.idle) {
            (0, 0) => format!("terhub — {} agent(s)", self.total),
            (0, idle) => format!("terhub — {idle} idle"),
            (needy, 0) => format!("terhub — {needy} need you"),
            (needy, idle) => format!("terhub — {needy} need you · {idle} idle"),
        }
    }

    /// Tooltip body: the breakdown behind the headline.
    pub fn description(&self) -> String {
        if !self.connected {
            return "start it with: terhub daemon start".to_string();
        }
        let mut d = format!(
            "{} tracked · {} waiting on you · {} idle",
            self.total, self.needy, self.idle
        );
        d.push_str(if self.hub_open {
            "\nclick to focus the open hub"
        } else {
            "\nclick to open the hub"
        });
        d
    }

    /// Drives the SNI `status` flag: "NeedsAttention" makes Plasma flag
    /// the panel entry; everything else reads as plain active.
    /// Disconnected is not "needs you" — the user is not waiting on
    /// anything, the daemon is.
    pub fn needs_attention(&self) -> bool {
        self.connected && self.needy > 0
    }

    /// Drives the icon variant. Attention always wins over the live
    /// badge; the live-idle pair is for the long-running case (a hub
    /// open and every tracked agent has finished).
    pub fn icon_name(&self) -> &'static str {
        if !self.connected {
            return "terhub-disconnected";
        }
        if self.needs_attention() {
            return "terhub-attention";
        }
        match (self.hub_open, self.idle) {
            (true, 0) => "terhub-live",
            (true, _) => "terhub-live-idle",
            (false, 0) => "terhub",
            (false, _) => "terhub-idle",
        }
    }
}

impl Default for TrayView {
    fn default() -> Self {
        TrayView {
            needy: 0,
            idle: 0,
            total: 0,
            connected: true,
            hub_open: false,
        }
    }
}

fn data_home() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            home.join(".local/share")
        })
}

/// Nudge the shell cache so a freshly written icon shows up without a
/// logout. Best-effort: not every desktop ships these tools.
fn refresh_icon_cache() {
    let icons = data_home().join("icons");
    for tool in ["gtk-update-icon-cache", "qt6-update-icon-cache"] {
        let ok = std::process::Command::new(tool)
            .arg(&icons)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            return;
        }
    }
}

// ---------------------------------------------------------------------------
// Running-hub advertisement
// ---------------------------------------------------------------------------

/// A live `terhub` TUI, advertised so the tray can focus it instead of
/// starting a second one.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HubSession {
    pub pid: u32,
    /// tmux pane id, when the TUI runs inside tmux. This is the reliable
    /// way back to it: `switch-client` lands the user on the pane.
    pub pane: Option<String>,
    /// Controlling terminal, used as a fallback for window activation.
    pub tty: Option<String>,
}

pub const HUB_SESSION_FILE: &str = "hub-session.json";

pub fn hub_session_path() -> PathBuf {
    crate::daemon::runtime_dir().join(HUB_SESSION_FILE)
}

fn process_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// Pull `TMUX_PANE` out of a NUL-separated `/proc/<pid>/environ` blob.
fn tmux_pane_from_environ(blob: &[u8]) -> Option<String> {
    blob.split(|b| *b == 0)
        .find(|kv| kv.starts_with(b"TMUX_PANE="))
        .map(|kv| String::from_utf8_lossy(&kv["TMUX_PANE=".len()..]).into_owned())
        .filter(|s| !s.is_empty())
}

/// True when this process is a bare `terhub` TUI — argv is exactly
/// ["terhub"], so the tray (`terhub tray`) and any subcommand are out.
fn is_tui_cmdline(blob: &[u8]) -> bool {
    let mut parts = blob.split(|b| *b == 0).filter(|p| !p.is_empty());
    matches!(parts.next(), Some(a) if a == b"terhub") && parts.next().is_none()
}

/// Find a running TUI by scanning /proc. This is the fallback for a hub
/// started before it learned to advertise itself: without it, a stale
/// TUI is invisible and every click spawns a duplicate.
fn scan_for_tui() -> Option<HubSession> {
    let me = std::process::id();
    for entry in std::fs::read_dir("/proc").ok()? {
        let Ok(entry) = entry else { continue };
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        if pid == me {
            continue;
        }
        let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        if !is_tui_cmdline(&cmdline) {
            continue;
        }
        let environ = std::fs::read(format!("/proc/{pid}/environ")).unwrap_or_default();
        let tty = std::fs::read_link(format!("/proc/{pid}/fd/0"))
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
            .filter(|s| s.starts_with("/dev/"));
        return Some(HubSession {
            pid,
            pane: tmux_pane_from_environ(&environ),
            tty,
        });
    }
    None
}

/// The advertisement file alone: no /proc fallback, so this is pure and
/// testable without depending on what else is running on the machine.
fn read_session_file() -> Option<HubSession> {
    let raw = std::fs::read_to_string(hub_session_path()).ok()?;
    let s: HubSession = serde_json::from_str(&raw).ok()?;
    process_alive(s.pid).then_some(s)
}

/// Read the advertised hub, ignoring one whose process is gone. A TUI
/// that was killed rather than quit leaves the file behind, and a stale
/// pid must not make the tray think a hub is open forever.
///
/// Falls back to a /proc scan so a hub predating the advertisement — or
/// one whose file was removed — is still found.
pub fn read_hub_session() -> Option<HubSession> {
    if let Some(s) = read_session_file() {
        return Some(s);
    }
    // Missing, unparseable, or dead: clear it so it cannot mislead.
    let _ = std::fs::remove_file(hub_session_path());
    scan_for_tui()
}

/// Removes the advertisement when the TUI exits.
pub struct HubSessionGuard;

impl HubSessionGuard {
    /// Advertise this process as the running hub.
    pub fn publish() -> Self {
        let session = HubSession {
            pid: std::process::id(),
            pane: std::env::var("TMUX_PANE").ok().filter(|p| !p.is_empty()),
            tty: std::env::var("TTY").ok().or_else(|| {
                // /proc/self/fd/0 points at the controlling terminal.
                std::fs::read_link("/proc/self/fd/0")
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned())
                    .filter(|s| s.starts_with("/dev/"))
            }),
        };
        if let Ok(json) = serde_json::to_string(&session) {
            let _ = std::fs::write(hub_session_path(), json);
        }
        HubSessionGuard
    }
}

impl Drop for HubSessionGuard {
    fn drop(&mut self) {
        // Only clear it if it is still ours: a second hub may have
        // started after this one exited.
        if let Some(s) = read_hub_session()
            && s.pid == std::process::id()
        {
            let _ = std::fs::remove_file(hub_session_path());
        }
    }
}

/// Bring the user back to an already-running hub. Returns false when we
/// have no addressable way to focus it, so the caller can avoid spawning
/// a duplicate TUI instead.
pub fn focus_hub(s: &HubSession) -> bool {
    if let Some(pane) = &s.pane
        && !pane.is_empty()
        && let Ok(tmux) = crate::tmux::Tmux::new()
        && tmux.focus_pane(pane).is_ok()
    {
        return true;
    }
    // Not in tmux (or the jump failed): raise the terminal window that
    // owns the TTY.
    if let Some(tty) = &s.tty
        && !tty.is_empty()
    {
        let ok = std::process::Command::new("xdotool")
            .args(["search", "--tty", tty])
            .output()
            .map(|o| {
                let ids: Vec<String> = String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(str::to_string)
                    .collect();
                ids.iter().any(|id| {
                    std::process::Command::new("xdotool")
                        .args(["windowactivate", "--sync", id])
                        .status()
                        .map(|s| s.success())
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false);
        if ok {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// StatusNotifierItem
// ---------------------------------------------------------------------------

/// Shared, mutable view the poll loop writes and the D-Bus properties
/// read.
type Shared = Arc<Mutex<TrayView>>;

struct Tray {
    view: Shared,
}

impl Tray {
    fn read(&self) -> TrayView {
        self.view
            .lock()
            .map(|v| v.clone())
            .unwrap_or_else(|_| TrayView {
                connected: false,
                ..TrayView::default()
            })
    }

    /// Primary click: open the hub in a terminal. `TERMHUB_TERMINAL`
    /// overrides the guess; the first terminal that exists wins.
    fn open_hub() {
        let cmds: Vec<Vec<String>> = match std::env::var("TERMHUB_TERMINAL") {
            Ok(custom) => vec![custom.split_whitespace().map(str::to_string).collect()],
            Err(_) => [
                vec!["konsole", "-e"],
                vec!["foot"],
                vec!["alacritty", "-e"],
                vec!["kitty"],
                vec!["xterm", "-e"],
            ]
            .into_iter()
            .map(|v| v.into_iter().map(str::to_string).collect())
            .collect(),
        };
        for argv in cmds {
            let Some((head, rest)) = argv.split_first() else {
                continue;
            };
            if which(head).is_some() {
                let mut cmd = std::process::Command::new(head);
                cmd.args(rest);
                cmd.arg("terhub");
                let _ = cmd.spawn();
                return;
            }
        }
    }
}

fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(bin))
        .find(|p| p.is_file())
}

#[zbus::interface(name = "org.kde.StatusNotifierItem")]
impl Tray {
    #[zbus(property)]
    fn category(&self) -> String {
        "ApplicationStatus".to_string()
    }

    #[zbus(property)]
    fn id(&self) -> String {
        "terhub".to_string()
    }

    #[zbus(property)]
    fn title(&self) -> String {
        self.read().title()
    }

    #[zbus(property)]
    fn status(&self) -> String {
        if self.read().needs_attention() {
            "NeedsAttention".to_string()
        } else {
            "Active".to_string()
        }
    }

    #[zbus(property)]
    fn icon_name(&self) -> String {
        self.read().icon_name().to_string()
    }

    /// Shown by hosts that prefer an explicit attention glyph.
    #[zbus(property)]
    fn attention_icon_name(&self) -> String {
        "terhub-attention".to_string()
    }

    /// (icon, title, description) — the hover card.
    #[zbus(property)]
    fn tooltip(&self) -> (String, String, String) {
        let v = self.read();
        (v.icon_name().to_string(), v.title(), v.description())
    }

    /// False: we handle clicks rather than popping a menu.
    #[zbus(property)]
    fn item_is_menu(&self) -> bool {
        false
    }

    /// Primary click: go back to the hub that is already open, or open
    /// one. Never both — a second TUI would fight the first for the pane.
    fn activate(&self) {
        if let Some(session) = read_hub_session()
            && focus_hub(&session)
        {
            return;
        }
        Self::open_hub();
    }

    fn scroll(&self, _delta: i32, _orientation: &str) {
        // No scroll actions; the spec allows a no-op here.
    }

    fn context_menu(&self, _x: i32, _y: i32) {
        // Deliberately empty: a context menu needs real actions to be
        // worth having, and `ItemIsMenu` is already false.
    }
}

/// Register the tray icon and keep it fed from the daemon. Runs until
/// the process is killed; dropping the connection removes the icon.
pub fn run() -> anyhow::Result<()> {
    // The active theme is what the user is reading the panel in: it
    // decides which palette of accent colors the icon pulls from. The
    // TUI's load_config() owns the theme, so we go through it rather
    // than parsing the TOML ourselves.
    let theme = crate::config::load_config(None).theme;
    install_icons(&theme)?;
    refresh_icon_cache();
    let mut last_theme = theme;
    let conn = zbus::blocking::Connection::session()?;
    let unique = format!("org.kde.StatusNotifierItem-{}-1", std::process::id());
    let view: Shared = Arc::new(Mutex::new(TrayView::default()));
    let tray = Tray {
        view: Arc::clone(&view),
    };
    conn.request_name(unique.as_str())?;
    // Export before registering: the watcher introspects the name as
    // soon as Register returns, and finds nothing if we register first.
    conn.object_server().at(SNI_PATH, tray)?;

    let watcher = zbus::blocking::Proxy::new(
        &conn,
        "org.kde.StatusNotifierWatcher",
        "/StatusNotifierWatcher",
        "org.kde.StatusNotifierWatcher",
    )?;
    watcher.call_method("RegisterStatusNotifierItem", &(unique.as_str(),))?;
    println!("terhub tray registered — ctrl-c to remove");

    loop {
        // A live update: the user might change theme in the TUI's
        // settings popup, so re-check it on every poll. (A full re-render
        // is cheap and the diff in `install_icons` only writes when the
        // SVG actually changed.)
        let active = crate::config::load_config(None).theme;
        if active != last_theme {
            if let Err(e) = install_icons(&active) {
                eprintln!("terhub: re-themed icon install failed: {e}");
            } else {
                // The cache hold at startup is what makes the FIRST
                // install visible; skip it here and Plasma keeps rastering
                // the previous palette's PNG forever (icons on disk change,
                // panel does not).
                refresh_icon_cache();
            }
            last_theme = active;
        }
        let mut next = match crate::daemon::fetch_snapshot(SNAP_TIMEOUT) {
            Ok(snap) => TrayView::from_agents(&snap.agents),
            Err(_) => TrayView::disconnected(),
        };
        next.hub_open = read_hub_session().is_some();
        if let Ok(mut v) = view.lock() {
            *v = next;
        }
        std::thread::sleep(POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Source;

    fn rec(state: AgentState) -> AgentRecord {
        AgentRecord {
            pane: "%1".to_string(),
            agent: "pi".to_string(),
            state,
            source: Source::Proc,
            summary: String::new(),
            session: "a".to_string(),
            window: "w".to_string(),
            command: "pi".to_string(),
            updated_at: 1,
            state_since: 1,
            cpu_ticks: 0,
            cpu_still_since: 0,
            agent_start: 0,
            last_turn_secs: 0,
            screen_hash: 0,
            quiet_since: 0,
            last_prompt_at: 0,
            last_prompt: String::new(),
            handle: String::new(),
            model: String::new(),
            ctx_pct: None,
            reg_pid: 0,
        }
    }

    #[test]
    fn counts_come_from_the_snapshot() {
        let agents = vec![
            rec(AgentState::Waiting),
            rec(AgentState::Idle),
            rec(AgentState::Idle),
            rec(AgentState::Working),
            rec(AgentState::Stuck),
        ];
        let v = TrayView::from_agents(&agents);
        assert_eq!(v.needy, 2, "waiting + stuck both need you");
        assert_eq!(v.idle, 2);
        assert_eq!(v.total, 5);
        assert!(v.needs_attention());
        assert_eq!(v.icon_name(), "terhub-attention");
    }

    #[test]
    fn idle_alone_never_flags_attention() {
        let v = TrayView::from_agents(&[rec(AgentState::Idle)]);
        assert!(!v.needs_attention(), "finished work is not urgent");
        assert_eq!(v.icon_name(), "terhub-idle");
        assert_eq!(v.title(), "terhub — 1 idle");
    }

    #[test]
    fn tooltip_states_read_as_sentences() {
        let busy = TrayView::from_agents(&[rec(AgentState::Waiting)]);
        assert_eq!(busy.title(), "terhub — 1 need you");
        let mixed = TrayView::from_agents(&[rec(AgentState::Waiting), rec(AgentState::Idle)]);
        assert_eq!(mixed.title(), "terhub — 1 need you · 1 idle");
        let calm = TrayView::from_agents(&[rec(AgentState::Working)]);
        assert_eq!(calm.title(), "terhub — 1 agent(s)");
        // The icon is always clickable, so the hint is always offered;
        // it just says which way the click goes.
        assert!(
            calm.description().contains("click to open the hub"),
            "{}",
            calm.description()
        );
    }

    #[test]
    fn open_hub_gets_its_own_icon() {
        let v = TrayView::from_agents(&[rec(AgentState::Working)]);
        assert_eq!(v.icon_name(), "terhub");
        let mut live = v.clone();
        live.hub_open = true;
        assert_eq!(live.icon_name(), "terhub-live");
        assert!(
            live.description().contains("focus the open hub"),
            "{}",
            live.description()
        );
    }

    #[test]
    fn idle_no_hub_gets_idle_icon() {
        // The long-running "everything finished" case, the most common
        // sit-on-the-panel icon in a typical day.
        let v = TrayView::from_agents(&[rec(AgentState::Idle), rec(AgentState::Working)]);
        assert_eq!(v.idle, 1);
        assert_eq!(v.icon_name(), "terhub-idle");
        assert!(!v.needs_attention());
    }

    #[test]
    fn idle_with_hub_open_gets_live_idle() {
        // A hub is open and every tracked agent has finished: live base
        // with the idle color so the eye learns the two live variants.
        let v = TrayView::from_agents(&[rec(AgentState::Idle)]);
        let mut live = v;
        live.hub_open = true;
        assert_eq!(live.icon_name(), "terhub-live-idle");
    }

    #[test]
    fn working_with_hub_open_stays_live() {
        // 0 idle + hub open is the only case that uses the non-idle live
        // variant. Idle=N here means no agent has finished yet.
        let v = TrayView::from_agents(&[rec(AgentState::Working)]);
        let mut live = v;
        live.hub_open = true;
        assert_eq!(live.icon_name(), "terhub-live");
    }

    #[test]
    fn disconnected_has_its_own_icon() {
        let v = TrayView::disconnected();
        assert_eq!(v.icon_name(), "terhub-disconnected");
        assert!(!v.needs_attention());
        // A hub being open while the daemon is down still reads as
        // disconnected: the live badge would be a lie (we can't reach
        // the hub's pane).
        let mut v = v;
        v.hub_open = true;
        assert_eq!(v.icon_name(), "terhub-disconnected");
    }

    #[test]
    fn attention_outranks_the_live_badge() {
        // Both facts matter, but "needs you" is the one that must change
        // the icon: the running hub is still discoverable from the title.
        let v = TrayView::from_agents(&[rec(AgentState::Waiting)]);
        assert_eq!(v.icon_name(), "terhub-attention");
        let mut live = v;
        live.hub_open = true;
        assert_eq!(live.icon_name(), "terhub-attention");
        assert!(live.needs_attention());
    }

    #[test]
    fn session_file_round_trips_and_ignores_dead_pids() {
        let dir = std::env::temp_dir().join(format!("terhub-hubsess-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Point the helper at a scratch dir for the duration.
        unsafe { std::env::set_var("TERHUB_RUNTIME_DIR", &dir) };

        let session = HubSession {
            pid: std::process::id(),
            pane: Some("%42".into()),
            tty: Some("/dev/pts/7".into()),
        };
        std::fs::write(hub_session_path(), serde_json::to_string(&session).unwrap()).unwrap();
        assert_eq!(read_session_file(), Some(session));

        // A hub that was killed leaves the file behind: it must not read
        // as open forever.
        std::fs::write(
            hub_session_path(),
            serde_json::to_string(&HubSession {
                pid: 999_999_999,
                pane: Some("%9".into()),
                tty: None,
            })
            .unwrap(),
        )
        .unwrap();
        // The pure file reader must not hand back a dead pid. (The
        // public entry point would fall through to the /proc scan, which
        // depends on what else this machine is running.)
        assert_eq!(read_session_file(), None);

        unsafe { std::env::remove_var("TERHUB_RUNTIME_DIR") };
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cmdline_filter_excludes_subcommands() {
        // The tray itself must never be mistaken for a TUI.
        assert!(is_tui_cmdline(b"terhub\0"));
        assert!(!is_tui_cmdline(b"terhub\0tray\0"));
        assert!(!is_tui_cmdline(b"/usr/bin/terhubd\0"));
        assert!(!is_tui_cmdline(b""));
    }

    #[test]
    fn tmux_pane_is_read_out_of_environ() {
        let env = b"SHELL=/bin/zsh\0TMUX_PANE=%603\0TERM=xterm\0";
        assert_eq!(tmux_pane_from_environ(env).as_deref(), Some("%603"));
        // Absent, or empty, is not a pane.
        assert_eq!(tmux_pane_from_environ(b"SHELL=/bin/zsh\0"), None);
        assert_eq!(tmux_pane_from_environ(b"TMUX_PANE=\0"), None);
    }

    #[test]
    fn daemon_down_never_reads_as_all_clear() {
        let v = TrayView::disconnected();
        assert_eq!(v.title(), "terhub — daemon down");
        assert!(!v.needs_attention());
        assert_eq!(v.icon_name(), "terhub-disconnected");
        assert!(
            v.description().contains("daemon start"),
            "{}",
            v.description()
        );
    }

    #[test]
    fn icon_state_matrix() {
        // Disconnected wins over every other fact.
        let mut v = TrayView::disconnected();
        v.hub_open = true;
        v.needy = 5;
        assert_eq!(v.icon_state(), IconState::Disconnected);
        // Attention wins over hub_open and over zero idle.
        let v = TrayView::from_agents(&[rec(AgentState::Waiting)]);
        let mut a = v;
        a.hub_open = true;
        assert_eq!(a.icon_state(), IconState::Attention);
        // Live + zero idle (hub running, agents busy).
        let mut live_busy = TrayView::from_agents(&[rec(AgentState::Working)]);
        live_busy.hub_open = true;
        assert_eq!(live_busy.icon_state(), IconState::Live);
        // Live + non-zero idle (hub running, agents finished).
        let mut live_idle = TrayView::from_agents(&[
            rec(AgentState::Idle),
            rec(AgentState::Idle),
            rec(AgentState::Working),
        ]);
        live_idle.hub_open = true;
        assert_eq!(live_idle.icon_state(), IconState::LiveIdle);
        // Idle without hub: still uses the dedicated idle icon.
        let v = TrayView::from_agents(&[rec(AgentState::Idle), rec(AgentState::Working)]);
        assert_eq!(v.icon_state(), IconState::Idle);
        // Pure working without idle: calm.
        let v = TrayView::from_agents(&[rec(AgentState::Working)]);
        assert_eq!(v.icon_state(), IconState::Calm);
    }

    #[test]
    fn every_known_theme_has_a_palette() {
        // No theme should fall through to the default by accident.
        for theme in crate::config::THEMES {
            let p = palette_for_theme(theme);
            // Hex of length 7, starts with '#', no whitespace.
            for field in [
                p.tile,
                p.crossbar_a,
                p.crossbar_b,
                p.stem_a,
                p.stem_b,
                p.live_dot,
                p.alarm_bg,
                p.alarm_fg,
            ] {
                assert!(
                    field.starts_with('#') && field.len() == 7,
                    "theme {theme} field {field:?} is not a 7-char hex"
                );
            }
        }
    }

    #[test]
    fn unknown_theme_falls_back_safely() {
        // Empty, garbage, weird: should still produce a valid SVG.
        for bad in ["", "not-a-theme", "DRACULA'", " "] {
            let p = palette_for_theme(bad);
            let svg = render_svg(IconState::Calm, p);
            assert!(svg.contains("<svg"), "{bad} produced no <svg>: {svg}");
            assert!(svg.contains("url(#cb)"), "{bad} missing crossbar gradient");
        }
    }

    #[test]
    fn render_svg_themes_differ_but_state_stable() {
        // The same state rendered for two themes must differ (color
        // injection works) and the structural shape must match (state
        // branch picked correctly).
        let drac = palette_for_theme("dracula");
        let nord = palette_for_theme("nord");
        for state in [
            IconState::Calm,
            IconState::Idle,
            IconState::Attention,
            IconState::Live,
            IconState::LiveIdle,
            IconState::Disconnected,
        ] {
            let a = render_svg(state, drac);
            let b = render_svg(state, nord);
            assert_ne!(a, b, "themes produced identical SVG for {state:?}");
            // Strip every theme color from both: the remaining body is
            // the geometric shape, and must match across themes. This
            // guarantees the only thing that varies is the palette.
            let strip_colors = |s: &str, p: TrayPalette| {
                let mut s = s.to_string();
                for c in [
                    p.tile,
                    p.crossbar_a,
                    p.crossbar_b,
                    p.stem_a,
                    p.stem_b,
                    p.live_dot,
                    p.alarm_bg,
                    p.alarm_fg,
                ] {
                    s = s.replace(c, "X");
                }
                s
            };
            let a_struct = strip_colors(&a, drac);
            let b_struct = strip_colors(&b, nord);
            assert_eq!(a_struct, b_struct, "geometry differs for {state:?}");
        }
    }

    #[test]
    fn attention_carry_uses_alarm_color() {
        // The alarm badge background must come from the palette, not
        // the old hard-coded red. Dracula's alarm is #ff5555, Nord's
        // alarm is #bf616a.
        let drac = render_svg(IconState::Attention, palette_for_theme("dracula"));
        assert!(drac.contains("#ff5555"), "dracula alarm missing: {drac}");
        let nord = render_svg(IconState::Attention, palette_for_theme("nord"));
        assert!(nord.contains("#bf616a"), "nord alarm missing: {nord}");
    }
}
