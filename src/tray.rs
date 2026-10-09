//! KDE/Plasma tray icon (`terhub tray`).
//!
//! Registers a StatusNotifierItem and keeps it in step with the daemon:
//! the icon switches to its attention variant when something needs you,
//! and the tooltip carries the counts. KDE's tray cannot draw a numeric
//! badge — the StatusNotifierItem spec has no field for one — so the
//! numbers live in the tooltip instead.

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::agent::{AgentRecord, AgentState};

const POLL: Duration = Duration::from_secs(3);
const SNAP_TIMEOUT: Duration = Duration::from_secs(2);
const SNI_PATH: &str = "/StatusNotifierItem";

/// Icon themes only resolve names they have files for, so the SVGs are
/// written into the user's icon dir on first run. The set is the same
/// across runs; the actual contents are themed at write time.
///
/// Six *files*, one *mark*: every state renders the same upside-down
/// triangle and differs only in fill color (plus opacity where "dim"
/// is the point). The StatusNotifierItem spec has no badge field, so
/// KDE picks the variant by name; the shape itself must stay constant
/// or the panel flickers between unrelated-looking glyphs.
///
/// File names carry a short content hash so a redesigned icon is a
/// *new* name Plasmashell has never resolved: its in-memory QIcon cache
/// keys on the icon name, and a same-name file edited underneath is
/// served as the stale pixmap it loaded on first sight. The on-disk
/// theme cache is invalidated by `refresh_icon_cache`; the
/// in-memory pixmap is not, so names must change.
const ICON_STATES: &[(IconState, &str); 6] = &[
    (IconState::Calm, "terhub"),
    (IconState::Attention, "terhub-attention"),
    (IconState::Idle, "terhub-idle"),
    (IconState::Disconnected, "terhub-disconnected"),
    (IconState::Live, "terhub-live"),
    (IconState::LiveIdle, "terhub-live-idle"),
];

/// Build the on-disk + SNI name for an icon, suffixed with the first 32
/// bits of an FNV-1a hash of its content. Stable across runs (no
/// `RandomState`), tiny, and identical iff the SVG is identical.
pub fn versioned_name(base: &str, svg: &str) -> String {
    let mut hash: u32 = 0x811c9dc5;
    for b in svg.as_bytes() {
        hash ^= *b as u32;
        hash = hash.wrapping_mul(0x01000193);
    }
    format!("{}-{:08x}", base, hash)
}

/// Exposed SNI icon name for a state, from the installed map. Falls
/// back to the unversioned base name when the map lacks it (a fresh
/// Tray whose first install hasn't landed yet, or a unit test without
/// one). Both the property and the run loop's change detector use
/// this, so they can never disagree about what's showing.
pub fn exposed_icon_name(names: &HashMap<IconState, String>, state: IconState) -> String {
    names.get(&state).cloned().unwrap_or_else(|| {
        ICON_STATES
            .iter()
            .find(|(s, _)| *s == state)
            .map(|(_, b)| b.to_string())
            .unwrap_or_default()
    })
}

/// Exposed SNI attention name, same contract.
pub fn exposed_attention_name(names: &HashMap<IconState, String>) -> String {
    exposed_icon_name(names, IconState::Attention)
}

/// One state the icon can be in. Distinct from [`TrayView`] so the
/// renderer stays pure and is testable with a 5-element palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

/// The colors the triangle pulls from the active theme. One mark serves
/// every state, so the palette only has to answer three questions:
/// what a neutral triangle looks like here, what an alarm looks like
/// here, and what "running" looks like (green everywhere, because that
/// is a universal signal rather than a theme expression).
///
/// There is deliberately no background tile: a tile that reads well at
/// 88px shrinks the mark to an unreadable dot at the 16px trays
/// actually use. The mark carries the whole 24px grid instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TrayPalette {
    /// The everyday triangle: calm — no hub open, no agents registered.
    mark: &'static str,
    /// The "needs you" triangle. Themes contribute their own red/pink so
    /// the alarm reads natively on the panel.
    alarm: &'static str,
    /// The live triangle: a hub is open and agents are working.
    live: &'static str,
    /// Live-idle: a hub is open and every agent has finished. A distinct
    /// hue (not a dimmer live) so a glance separates "still working" from
    /// "all done waiting".
    done: &'static str,
    /// Idle: no hub open, but agents are registered (waiting to be
    /// picked up). A distinct hue from calm so a glance separates
    /// "nothing's here" from "something's here, you haven't opened a hub".
    wait: &'static str,
    /// Disconnected: deliberately theme-neutral grey, because "the
    /// daemon is gone" should look the same whichever theme is loaded.
    muted: &'static str,
}

/// Look up a theme's tray palette. Unknown themes fall back to dracula;
/// the TUI's palette() has the same fallback, so the icon and the panel
/// stay in lockstep. Each of the six states gets its own distinct hue
/// drawn from the theme's natural accent set — not a dimmed version of
/// another state — so the legend reads at a glance.
fn palette_for_theme(theme: &str) -> TrayPalette {
    match theme {
        "gruvbox" => TrayPalette {
            mark: "#fabd2f",
            alarm: "#fb4934",
            live: "#b8bb26",
            done: "#d3869b",
            wait: "#83a598",
            muted: "#7c6f64",
        },
        "dracula" => TrayPalette {
            mark: "#8be9fd",
            alarm: "#ff5555",
            live: "#50fa7b",
            done: "#f1fa8c",
            wait: "#bd93f9",
            muted: "#6272a4",
        },
        "nord" => TrayPalette {
            mark: "#88c0d0",
            alarm: "#bf616a",
            live: "#a3be8c",
            done: "#ebcb8b",
            wait: "#b48ead",
            muted: "#4c566a",
        },
        "solarized" => TrayPalette {
            mark: "#268bd2",
            alarm: "#dc322f",
            live: "#859900",
            done: "#b58900",
            wait: "#6c71c4",
            muted: "#586e75",
        },
        "tokyonight" => TrayPalette {
            mark: "#7dcfff",
            alarm: "#f7768e",
            live: "#9ece6a",
            done: "#e0af68",
            wait: "#bb9af7",
            muted: "#414868",
        },
        "catppuccin" => TrayPalette {
            mark: "#89b4fa",
            alarm: "#f38ba8",
            live: "#a6e3a1",
            done: "#f9e2af",
            wait: "#cba6f7",
            muted: "#585b70",
        },
        "onedark" => TrayPalette {
            mark: "#56b6c2",
            alarm: "#e06c75",
            live: "#98c379",
            done: "#e5c07b",
            wait: "#c678dd",
            muted: "#5c6370",
        },
        "monokai" => TrayPalette {
            mark: "#66d9ef",
            alarm: "#f92672",
            live: "#a6e22e",
            done: "#e6db74",
            wait: "#ae81ff",
            muted: "#75715e",
        },
        "light" => TrayPalette {
            mark: "#268bd2",
            alarm: "#dc322f",
            live: "#859900",
            done: "#b58900",
            wait: "#6c71c4",
            muted: "#93a1a1",
        },
        "rose-pine" => TrayPalette {
            // Rose Pine has no green; "live" borrows the theme's pine
            // teal so the active state still reads as the working hue.
            mark: "#9ccfd8",  // foam — calm
            alarm: "#eb6f92", // love — attention
            live: "#31748f",  // pine — live
            done: "#f6c177",  // gold — live-idle
            wait: "#c4a7e7",  // iris — idle
            muted: "#6e6a86", // subtext muted — disconnected
        },
        "everforest" => TrayPalette {
            mark: "#d3c6aa",  // fg warm
            alarm: "#e67e80", // red
            live: "#a7c080",  // green
            done: "#dbbc7f",  // yellow
            wait: "#d699b6",  // purple
            muted: "#859289", // muted
        },
        "kanagawa" => TrayPalette {
            mark: "#8ea4a2",  // aqua
            alarm: "#c4746e", // red
            live: "#87a987",  // green
            done: "#c8b36c",  // yellow
            wait: "#9573a7",  // purple
            muted: "#625e5a", // muted
        },
        "ayu" => TrayPalette {
            mark: "#95e6cb",  // aqua
            alarm: "#f07178", // red
            live: "#aad94c",  // green
            done: "#ffb454",  // yellow
            wait: "#d2a6ff",  // purple
            muted: "#3e4b59", // muted
        },
        "nightfly" => TrayPalette {
            mark: "#5fb4d4",  // aqua
            alarm: "#fc514d", // red
            live: "#a1cd76",  // green
            done: "#ffcc00",  // yellow
            wait: "#c792ea",  // purple
            muted: "#5c6670", // muted
        },
        "palenight" => TrayPalette {
            mark: "#86e1fc",  // aqua
            alarm: "#f07178", // red
            live: "#c3e88d",  // green
            done: "#ffcb6b",  // yellow
            wait: "#c792ea",  // purple
            muted: "#676e95", // muted
        },
        "github" => TrayPalette {
            // GitHub Dark by default; github-light is a separate theme
            // added below for completeness.
            mark: "#58a6ff",  // blue
            alarm: "#ff7b72", // red
            live: "#3fb950",  // green
            done: "#d29922",  // yellow
            wait: "#bc8cff",  // purple
            muted: "#8b949e", // muted
        },
        "github-light" => TrayPalette {
            mark: "#0366d6",  // blue
            alarm: "#d73a49", // red
            live: "#28a745",  // green
            done: "#dbab09",  // yellow
            wait: "#6f42c1",  // purple
            muted: "#6a737d", // muted
        },
        "onehalf" => TrayPalette {
            // OneHalf Dark canonical.
            mark: "#61afef",  // blue
            alarm: "#e06c75", // red
            live: "#98c379",  // green
            done: "#e5c07b",  // yellow
            wait: "#c678dd",  // purple
            muted: "#5c6370", // muted
        },
        "onehalf-light" => TrayPalette {
            mark: "#4078f2",  // blue
            alarm: "#e45649", // red
            live: "#98c379",  // green
            done: "#f4b73e",  // yellow
            wait: "#a626a4",  // purple
            muted: "#a3a3a3", // muted
        },
        // `dark` and anything unknown fall here, matching dracula, the
        // same fallback the TUI's palette() uses.
        _ => TrayPalette {
            mark: "#8be9fd",
            alarm: "#ff5555",
            live: "#50fa7b",
            done: "#f1fa8c",
            wait: "#bd93f9",
            muted: "#6272a4",
        },
    }
}

/// The mark. One upside-down triangle for every state — the shape never
/// changes, only the fill. At tray sizes (16-22px) a color difference
/// survives the downscale where a shape difference smears into noise,
/// so a single glyph is the legible choice rather than six near-miss
/// variants of the same idea.
///
/// Sized on a 24px grid with no background plate, filling ~80% of the
/// box: the panel renders SNI icons at a fixed size, so the only way
/// to read bigger is to spend the whole viewBox. The stroke only
/// rounds the corners, it does not change the silhouette.
const TRIANGLE: &str = "M2.5 4.5 L21.5 4.5 L12 21 Z";

/// State → fill colour. Each of the six states maps to its own slot in
/// the palette so a glance separates them: live isn't a dimmer live-idle,
/// calm isn't a brighter idle, and the alarm colour is never reused for
/// anything that isn't asking for your input. See
/// `every_state_renders_a_distinct_colour`.
fn state_paint(state: IconState, p: TrayPalette) -> &'static str {
    match state {
        IconState::Calm => p.mark,
        IconState::Idle => p.wait,
        IconState::Attention => p.alarm,
        IconState::Live => p.live,
        IconState::LiveIdle => p.done,
        IconState::Disconnected => p.muted,
    }
}

/// One row in the TUI's "Tray icons" legend: the state name, the fill
/// colour it renders in for the current theme, and a one-line meaning
/// so the user can map the panel's triangle to what it's saying.
pub struct IconLegendRow {
    pub state: IconState,
    pub name: &'static str,
    pub hex: &'static str,
    pub meaning: &'static str,
}

/// Build the legend for the active theme. The same palette the icon
/// is rendered with, so the colour shown in the settings popup is
/// exactly the colour the user sees in their panel.
pub fn icon_legend(theme: &str) -> Vec<IconLegendRow> {
    let p = palette_for_theme(theme);
    let colour = |s: IconState| -> &'static str { state_paint(s, p) };
    vec![
        IconLegendRow {
            state: IconState::Live,
            name: "live",
            hex: colour(IconState::Live),
            meaning: "hub is open and agents are working",
        },
        IconLegendRow {
            state: IconState::LiveIdle,
            name: "live (idle)",
            hex: colour(IconState::LiveIdle),
            meaning: "hub is open, every agent has finished",
        },
        IconLegendRow {
            state: IconState::Attention,
            name: "attention",
            hex: colour(IconState::Attention),
            meaning: "an agent is waiting on your input",
        },
        IconLegendRow {
            state: IconState::Calm,
            name: "calm",
            hex: colour(IconState::Calm),
            meaning: "no hub is open, no agents registered",
        },
        IconLegendRow {
            state: IconState::Idle,
            name: "idle",
            hex: colour(IconState::Idle),
            meaning: "no hub is open, but agents are registered",
        },
        IconLegendRow {
            state: IconState::Disconnected,
            name: "disconnected",
            hex: colour(IconState::Disconnected),
            meaning: "the daemon is not running (start it: terhub daemon start)",
        },
    ]
}

/// Palette × state → SVG. The triangle is a constant path; only the
/// fill changes (one distinct colour per state, no opacity tricks), so
/// the rendered geometry is provably identical across themes and
/// states (see `every_state_renders_the_same_shape`). An optional
/// badge count is drawn centred in the triangle in a dark neutral —
/// the SNI spec has no badge field, so the number has to live in the
/// image itself. Two digits fit the face at panel sizes; anything
/// larger renders as `99+`.
fn render_svg(state: IconState, p: TrayPalette) -> String {
    let fill = state_paint(state, p);
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"24\" height=\"24\" viewBox=\"0 0 24 24\">\
                  <path d=\"{TRIANGLE}\" fill=\"{fill}\" stroke=\"{fill}\" stroke-width=\"1.2\" stroke-linejoin=\"round\"/>\
         </svg>",
        fill = fill,
    )
}

/// Install the tray SVGs into the user's icon theme, themed to the
/// active `theme` config. Same shape on disk across runs; each file is
/// rewritten only when its rendered content changes.
/// Write the 6 themed SVGs under versioned names, drop any stale
/// `terhub-*.svg` left over from a prior art generation, and return
/// the state→icon-name map the SNI `IconName` property uses. The
/// optional badge count is drawn into every icon so the panel reads
/// the number without a tooltip; a new count is new content, hence a
/// new versioned name the host has never cached.
pub fn install_icons(theme: &str) -> std::io::Result<HashMap<IconState, String>> {
    let dir = data_home().join("icons/hicolor/scalable/apps");
    std::fs::create_dir_all(&dir)?;
    let palette = palette_for_theme(theme);
    let mut names: HashMap<IconState, String> = HashMap::with_capacity(ICON_STATES.len());
    let mut keep: HashSet<String> = HashSet::with_capacity(ICON_STATES.len());
    for (state, base) in ICON_STATES {
        let svg = render_svg(*state, palette);
        let name = versioned_name(base, &svg);
        let path = dir.join(format!("{name}.svg"));
        if std::fs::read_to_string(&path).ok().as_deref() != Some(svg.as_str()) {
            std::fs::write(&path, svg)?;
        }
        keep.insert(format!("{name}.svg"));
        names.insert(*state, name);
    }
    // Prune any terhub-*.svg from a previous install whose name isn't
    // in the current set, so the icons dir doesn't accumulate the
    // full history of every redesign.
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for entry in rd.flatten() {
            let p = entry.path();
            if p.extension().and_then(|s| s.to_str()) != Some("svg") {
                continue;
            }
            let Some(f) = p.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if f.starts_with("terhub") && f.ends_with(".svg") && !keep.contains(f) {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
    Ok(names)
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
    // owns the TTY. Each xdotool call is bounded (2s) so a wedged
    // xdotool cannot freeze the tray dispatch path (DBus property
    // reads/updates would otherwise stall indefinitely).
    if let Some(tty) = &s.tty
        && !tty.is_empty()
    {
        let mut search = std::process::Command::new("xdotool");
        search.args(["search", "--tty", tty]).stdin(Stdio::null());
        let ids: Vec<String> =
            match crate::native::run_with_timeout(search, "xdotool search", 2) {
                Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(str::to_string)
                    .collect(),
                _ => Vec::new(),
            };
        for id in &ids {
            let mut act = std::process::Command::new("xdotool");
            act.args(["windowactivate", "--sync", id])
                .stdin(Stdio::null());
            if let Ok(o) =
                crate::native::run_with_timeout(act, "xdotool windowactivate", 2)
                && o.status.success()
            {
                return true;
            }
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
    /// Icon names (no extension) per state, versioned by the content
    /// hash so a redesigned icon is a *new* name the panel has never
    /// resolved and can't serve a stale pixmap of.
    names: std::sync::Arc<std::sync::Mutex<HashMap<IconState, String>>>,
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

    fn read_names(&self) -> HashMap<IconState, String> {
        self.names
            .lock()
            .map(|n| n.clone())
            .unwrap_or_default()
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

/// Resolve a name on `PATH` to the first executable file. Mirrors
/// the `is_file` check that `which(1)` uses, but additionally
/// requires at least one executable bit set so a stray non-executable
/// file (a README named `terhub`, a build artefact) isn't picked
/// and silently fails to spawn.
fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(bin))
        .find(|p| {
            p.metadata()
                .map(|m| m.is_file() && (m.mode() & 0o111) != 0)
                .unwrap_or(false)
        })
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
        exposed_icon_name(&self.read_names(), self.read().icon_state())
    }

    /// Shown by hosts that prefer an explicit attention glyph.
    #[zbus(property)]
    fn attention_icon_name(&self) -> String {
        exposed_attention_name(&self.read_names())
    }

    /// (icon, title, description) — the hover card. The icon field
    /// carries the versioned name (the unversioned base files are
    /// pruned on install, so the base name would resolve to nothing).
    /// The explicit name matters: the SNI spec calls this `ToolTip`
    /// (capital T-T) and zbus would otherwise derive `Tooltip`, which
    /// no host reads — leaving the hover card empty.
    #[zbus(property, name = "ToolTip")]
    fn tooltip(&self) -> (String, String, String) {
        let v = self.read();
        let names = self.read_names();
        (
            exposed_icon_name(&names, v.icon_state()),
            v.title(),
            v.description(),
        )
    }

    /// False: we handle clicks rather than popping a menu.
    #[zbus(property)]
    fn item_is_menu(&self) -> bool {
        false
    }

    /// The host only re-reads the icon when told: without this, the
    /// panel keeps the pixmap it loaded at registration no matter how
    /// often `IconName` changes underneath (field-observed: a state
    /// flip with no signal left a stale green icon showing for a grey
    /// disconnected state). Emitted whenever the exposed icon name
    /// actually changes.
    #[zbus(signal)]
    async fn new_icon(signal_emitter: &zbus::object_server::SignalEmitter<'_>) -> zbus::Result<()>;

    /// Same contract for the attention glyph.
    #[zbus(signal)]
    async fn new_attention_icon(
        signal_emitter: &zbus::object_server::SignalEmitter<'_>,
    ) -> zbus::Result<()>;

    /// Same contract for the title line.
    #[zbus(signal)]
    async fn new_title(signal_emitter: &zbus::object_server::SignalEmitter<'_>) -> zbus::Result<()>;

    /// Same contract for the hover card.
    #[zbus(signal)]
    async fn new_tool_tip(
        signal_emitter: &zbus::object_server::SignalEmitter<'_>,
    ) -> zbus::Result<()>;

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

/// True when the watcher already lists `unique`, in either form: a bare
/// service name, or the `service/path` form Plasma 6's watcher stores.
pub fn item_is_listed(entries: &[String], unique: &str) -> bool {
    entries.iter().any(|entry| {
        entry == unique || entry.strip_prefix(unique).is_some_and(|rest| rest.starts_with('/'))
    })
}

/// Plasma's watcher purges its item list when the host session goes away
/// (a Plasmashell crash or restart — field-observed 2026-10-08 18:16,
/// when a tray registered at 18:13 went permanently invisible). The
/// item's bus name survives, so every property read still answers and
/// nothing errors — the icon is simply forgotten. Re-register whenever
/// the watcher no longer lists us. Idempotent: a tick that finds us
/// listed does nothing, so this costs one property read per poll.
fn ensure_registered(watcher: &zbus::blocking::Proxy<'_>, unique: &str) {
    let listed = watcher
        .get_property::<Vec<String>>("RegisteredStatusNotifierItems")
        .unwrap_or_default();
    if item_is_listed(&listed, unique) {
        return;
    }
    match watcher.call_method("RegisterStatusNotifierItem", &(unique,)) {
        Ok(_) => println!("terhub tray: re-registered after watcher reset"),
        Err(e) => eprintln!("terhub tray: re-register failed: {e}"),
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
    let names: std::sync::Arc<std::sync::Mutex<HashMap<IconState, String>>> =
        std::sync::Arc::new(std::sync::Mutex::new(
            install_icons(&theme).map_err(anyhow::Error::from)?,
        ));
    refresh_icon_cache();
    let mut last_theme = theme;
    let conn = zbus::blocking::Connection::session()?;
    let unique = format!("org.kde.StatusNotifierItem-{}-1", std::process::id());
    let view: Shared = Arc::new(Mutex::new(TrayView::default()));
    let tray = Tray {
        view: Arc::clone(&view),
        names: std::sync::Arc::clone(&names),
    };
    conn.request_name(unique.as_str())?;
    // Export before registering: the watcher introspects the name as
    // soon as Register returns, and finds nothing if we register first.
    conn.object_server().at(SNI_PATH, tray)?;
    let emitter = conn
        .object_server()
        .interface::<_, Tray>(SNI_PATH)
        .map(|i| i.signal_emitter().clone())
        .map_err(anyhow::Error::from)?;

    let watcher = zbus::blocking::Proxy::new(
        &conn,
        "org.kde.StatusNotifierWatcher",
        "/StatusNotifierWatcher",
        "org.kde.StatusNotifierWatcher",
    )?;
    watcher.call_method("RegisterStatusNotifierItem", &(unique.as_str(),))?;
    println!("terhub tray registered — ctrl-c to remove");

    // Last exposed strings: the host only re-reads on our signals,
    // so emit NewIcon/NewAttentionIcon/NewTitle/NewToolTip exactly
    // when the corresponding value actually changes.
    let mut last_icon = String::new();
    let mut last_attention = String::new();
    let mut last_title = String::new();
    let mut last_tooltip = (String::new(), String::new(), String::new());
    loop {
        // A live update: the user might change theme in the TUI's
        // settings popup, so re-check it on every poll.
        let active = crate::config::load_config(None).theme;
        let mut next = match crate::daemon::fetch_snapshot(SNAP_TIMEOUT) {
            Ok(snap) => TrayView::from_agents(&snap.agents),
            Err(_) => TrayView::disconnected(),
        };
        next.hub_open = read_hub_session().is_some();
        if let Ok(mut v) = view.lock() {
            *v = next.clone();
        }
        // Re-render when the theme changed; the write-if-changed
        // guard inside keeps disk quiet otherwise. New content is a
        // new versioned name, so the host — told via the signals
        // below — always loads fresh.
        if active != last_theme {
            match install_icons(&active) {
                Ok(new_names) => {
                    if let Ok(mut n) = names.lock() {
                        *n = new_names;
                    }
                }
                Err(e) => eprintln!("terhub: icon install failed: {e}"),
            }
            // On-disk theme cache only; the versioned names above
            // already force fresh loads for the new palette.
            refresh_icon_cache();
            last_theme = active.clone();
        }
        // Tell the host exactly what changed. Reading a property
        // without its signal leaves the previous pixmap/text showing
        // (the failure mode that froze the old icon in place).
        let snap = view.lock().map(|v| v.clone()).unwrap_or_default();
        let map = names.lock().map(|n| n.clone()).unwrap_or_default();
        let icon = exposed_icon_name(&map, snap.icon_state());
        if icon != last_icon {
            last_icon = icon;
            if let Err(e) = zbus::block_on(Tray::new_icon(&emitter)) {
                eprintln!("terhub tray: NewIcon signal failed: {e}");
            }
        }
        let attention = exposed_attention_name(&map);
        if attention != last_attention {
            last_attention = attention;
            if let Err(e) = zbus::block_on(Tray::new_attention_icon(&emitter)) {
                eprintln!("terhub tray: NewAttentionIcon signal failed: {e}");
            }
        }
        let title = snap.title();
        if title != last_title {
            last_title = title;
            if let Err(e) = zbus::block_on(Tray::new_title(&emitter)) {
                eprintln!("terhub tray: NewTitle signal failed: {e}");
            }
        }
        let tooltip = (
            exposed_icon_name(&map, snap.icon_state()),
            snap.title(),
            snap.description(),
        );
        if tooltip != last_tooltip {
            last_tooltip = tooltip;
            if let Err(e) = zbus::block_on(Tray::new_tool_tip(&emitter)) {
                eprintln!("terhub tray: NewToolTip signal failed: {e}");
            }
        }
        // Self-heal: a host restart (or a restarted watcher) forgets every
        // item, so re-register on our own cadence instead of waiting for
        // the user to notice the icon is gone and restart us by hand.
        ensure_registered(&watcher, &unique);
        std::thread::sleep(POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Source;

    #[test]
    fn item_is_listed_matches_bare_and_path_forms() {
        let unique = "org.kde.StatusNotifierItem-123-1";
        // Missing entirely is the orphaned-icon case: re-register.
        assert!(!item_is_listed(&[], unique));
        assert!(!item_is_listed(&["org.kde.StatusNotifierItem-124-1".to_string()], unique));
        // A name that merely starts with ours is not us.
        assert!(!item_is_listed(&[format!("{unique}-suffix")], unique));
        // Both real forms: the bare name we register with, and the
        // service/path form Plasma 6's watcher stores.
        assert!(item_is_listed(&[unique.to_string()], unique));
        assert!(item_is_listed(&[format!("{unique}/StatusNotifierItem")], unique));
        // Mixed lists: found next to unrelated items.
        assert!(item_is_listed(
            &[
                "org.kde.StatusNotifierItem-9-1".to_string(),
                format!("{unique}/StatusNotifierItem"),
            ],
            unique,
        ));
    }

    #[test]
    fn icons_carry_no_badge_text() {
        // Deliberate product decision: the panel shows the state colour
        // only — no counts baked into the art. Counts live in the
        // title and tooltip (one hover away), never in the pixmap.
        // This guards the removal: if text ever returns to the SVG,
        // this fails and forces the conversation again.
        let p = palette_for_theme("dracula");
        for state in [
            IconState::Calm,
            IconState::Idle,
            IconState::Attention,
            IconState::Live,
            IconState::LiveIdle,
            IconState::Disconnected,
        ] {
            let svg = render_svg(state, p);
            assert!(!svg.contains("<text"), "{state:?} carries text:\n{svg}");
            assert!(svg.contains(TRIANGLE), "{state:?} lost its triangle");
        }
    }

    #[test]
    fn versioned_name_is_stable_and_changes_with_content() {
        // The whole reason this name exists: Plasmashell's QIcon cache
        // keys on the name, so a redesigned icon must be a *new* name
        // (same content → same name; new content → new name). The
        // hash is a 32-bit FNV, 8 hex digits.
        let a = versioned_name("terhub-live", "<svg/>");
        let b = versioned_name("terhub-live", "<svg/>");
        assert_eq!(a, b);
        assert!(a.starts_with("terhub-live-"));
        assert_eq!(a.len(), "terhub-live-".len() + 8);
        // Any byte changed → a new name. Plasmashell will then load
        // from disk instead of returning the stale pixmap.
        let c = versioned_name("terhub-live", "<svg />");
        assert_ne!(a, c);
        // Different base, same content → different name.
        let d = versioned_name("terhub", "<svg/>");
        assert_ne!(a, d);
    }

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
        // No theme should fall through to the default by accident, and
        // every colour must be a literal 7-char hex — a bad literal is
        // what makes an SVG silently invalid in the panel.
        for theme in crate::config::THEMES {
            let p = palette_for_theme(theme);
            for field in [p.mark, p.alarm, p.live, p.done, p.wait, p.muted] {
                assert!(
                    field.starts_with('#') && field.len() == 7,
                    "theme {theme} field {field:?} is not a 7-char hex"
                );
            }
        }
    }

    #[test]
    fn every_state_is_told_apart_by_colour_not_shape() {

        // The whole point of the redesign: one mark, distinguished by
        // colour. Every adjacent pair of states must differ, or two
        // states collapse into the same icon in the panel.
        let p = palette_for_theme("dracula");
        let states = [
            IconState::Calm,
            IconState::Idle,
            IconState::Attention,
            IconState::Live,
            IconState::LiveIdle,
            IconState::Disconnected,
        ];
        for (i, a) in states.iter().enumerate() {
            for b in states.iter().skip(i + 1) {
                assert_ne!(
                    render_svg(*a, p),
                    render_svg(*b, p),
                    "{a:?} and {b:?} render identically — two states share one icon"
                );
            }
        }
    }

    #[test]
    fn every_state_renders_the_same_shape() {
        // The triangle path must be present exactly once per state, so
        // the geometry is identical and only colour/opacity vary.
        for state in [
            IconState::Calm,
            IconState::Idle,
            IconState::Attention,
            IconState::Live,
            IconState::LiveIdle,
            IconState::Disconnected,
        ] {
            let svg = render_svg(state, palette_for_theme("dracula"), None);
            assert_eq!(
                svg.matches(TRIANGLE).count(),
                1,
                "{state:?} triangle count wrong: {svg}"
            );
            assert!(!svg.contains("linearGradient"), "{state:?} kept a gradient");
            assert!(svg.contains("viewBox=\"0 0 24 24\""), "{state:?} grid drifted");
        }
    }

    #[test]
    fn unknown_theme_falls_back_safely() {
        // Empty, garbage, weird: should still produce a valid SVG.
        for bad in ["", "not-a-theme", "DRACULA'", " "] {
            let p = palette_for_theme(bad);
            let svg = render_svg(IconState::Calm, p);
            assert!(svg.contains("<svg"), "{bad} produced no <svg>: {svg}");
            assert!(svg.contains(TRIANGLE), "{bad} missing the triangle");
        }
    }

    #[test]
    fn render_svg_themes_differ_but_state_stable() {
        // The same state rendered for two themes must differ (color
        // injection works) and the geometric shape must match (the
        // triangle is a constant path, so nothing drifts per theme).
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
            // the geometry, and must match across themes. This
            // guarantees the only thing that varies is the palette.
            let strip_colors = |s: &str, p: TrayPalette| {
                let mut s = s.to_string();
                for c in [p.mark, p.alarm, p.live, p.done, p.wait, p.muted] {
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
        let drac = render_svg(IconState::Attention, palette_for_theme("dracula"), None);
        assert!(drac.contains("#ff5555"), "dracula alarm missing: {drac}");
        let nord = render_svg(IconState::Attention, palette_for_theme("nord"), None);
        assert!(nord.contains("#bf616a"), "nord alarm missing: {nord}");
    }
}
