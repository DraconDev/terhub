//! KDE/Plasma tray icon (`terhub tray`).
//!
//! Registers a StatusNotifierItem and keeps it in step with the daemon:
//! the icon switches to its attention variant when something needs you,
//! and the tooltip carries the counts. KDE's tray cannot draw a numeric
//! badge — the StatusNotifierItem spec has no field for one — so the
//! numbers live in the tooltip instead.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::agent::{AgentRecord, AgentState};

const POLL: Duration = Duration::from_secs(3);
const SNAP_TIMEOUT: Duration = Duration::from_secs(2);
const SNI_PATH: &str = "/StatusNotifierItem";

/// Icon themes only resolve names they have files for, so the SVGs are
/// written into the user's icon dir on first run.
const ICONS: [(&str, &str); 2] = [
    ("terhub.svg", include_str!("../assets/terhub.svg")),
    (
        "terhub-attention.svg",
        include_str!("../assets/terhub-attention.svg"),
    ),
];

/// What the tray shows, derived from one daemon snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayView {
    pub needy: usize,
    pub idle: usize,
    pub total: usize,
    /// True when the daemon could not be reached.
    pub connected: bool,
}

impl TrayView {
    pub fn from_agents(agents: &[AgentRecord]) -> Self {
        TrayView {
            needy: agents.iter().filter(|a| a.state.needs_attention()).count(),
            idle: agents.iter().filter(|a| a.state == AgentState::Idle).count(),
            total: agents.len(),
            connected: true,
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
        if self.needy > 0 {
            d.push_str("\nclick to open the hub");
        }
        d
    }

    /// Drives both the icon variant and the SNI status flag.
    pub fn needs_attention(&self) -> bool {
        self.connected && self.needy > 0
    }

    pub fn icon_name(&self) -> &'static str {
        if self.needs_attention() {
            "terhub-attention"
        } else {
            "terhub"
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
        }
    }
}

/// Install the tray SVGs into the user's icon theme.
pub fn install_icons() -> std::io::Result<PathBuf> {
    let dir = data_home().join("icons/hicolor/scalable/apps");
    std::fs::create_dir_all(&dir)?;
    for (name, svg) in ICONS {
        let path = dir.join(name);
        // Only write when changed: Plasma watches these dirs, and a
        // needless rewrite on every launch would churn its cache.
        if std::fs::read_to_string(&path).ok().as_deref() != Some(svg) {
            std::fs::write(&path, svg)?;
        }
    }
    Ok(dir)
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
        self.view.lock().map(|v| v.clone()).unwrap_or_else(|_| TrayView {
            connected: false,
            ..TrayView::default()
        })
    }

    /// Primary click: open the hub in a terminal. `TERMHUB_TERMINAL`
    /// overrides the guess; the first terminal that exists wins.
    fn open_hub() {
        let cmds: Vec<Vec<&str>> = match std::env::var("TERMHUB_TERMINAL") {
            Ok(custom) => vec![custom.split_whitespace().collect()],
            Err(_) => vec![
                vec!["konsole", "-e"],
                vec!["foot"],
                vec!["alacritty", "-e"],
                vec!["kitty"],
                vec!["xterm", "-e"],
            ],
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

    fn activate(&self) {
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
    install_icons()?;
    refresh_icon_cache();

    let conn = zbus::blocking::Connection::session()?;
    let unique = format!("org.kde.StatusNotifierItem-{}-1", std::process::id());
    let view: Shared = Arc::new(Mutex::new(TrayView::default()));
    let tray = Tray {
        view: Arc::clone(&view),
    };
    conn.request_name(
        &unique,
        zbus::fdo::NameFlag::ReplaceExisting,
        zbus::fdo::RequestNameReply::PrimaryOwner,
    )?;
    // Export before registering: the watcher introspects the name as
    // soon as Register returns, and finds nothing if we register first.
    conn.export(SNI_PATH, tray)?;

    let watcher = zbus::blocking::Proxy::new(
        &conn,
        "org.kde.StatusNotifierWatcher",
        "/StatusNotifierWatcher",
        "org.kde.StatusNotifierWatcher",
    )?;
    watcher.call_method("RegisterStatusNotifierItem", &(unique.as_str(),))?;
    println!("terhub tray registered — ctrl-c to remove");

    loop {
        let next = match crate::daemon::fetch_snapshot(SNAP_TIMEOUT) {
            Ok(snap) => TrayView::from_agents(&snap.agents),
            Err(_) => TrayView::disconnected(),
        };
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
        assert_eq!(v.icon_name(), "terhub");
        assert_eq!(v.title(), "terhub — 1 idle");
    }

    #[test]
    fn tooltip_states_read_as_sentences() {
        let busy = TrayView::from_agents(&[rec(AgentState::Waiting)]);
        assert_eq!(busy.title(), "terhub — 1 need you");
        let mixed = TrayView::from_agents(&[
            rec(AgentState::Waiting),
            rec(AgentState::Idle),
        ]);
        assert_eq!(mixed.title(), "terhub — 1 need you · 1 idle");
        let calm = TrayView::from_agents(&[rec(AgentState::Working)]);
        assert_eq!(calm.title(), "terhub — 1 agent(s)");
        assert!(!calm.description().contains("click"), "{}", calm.description());
    }

    #[test]
    fn daemon_down_never_reads_as_all_clear() {
        let v = TrayView::disconnected();
        assert_eq!(v.title(), "terhub — daemon down");
        assert!(!v.needs_attention());
        assert_eq!(v.icon_name(), "terhub");
        assert!(v.description().contains("daemon start"), "{}", v.description());
    }
}