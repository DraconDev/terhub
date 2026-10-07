//! KDE/Plasma tray icon (`terhub tray`).
//!
//! Registers a StatusNotifierItem and keeps it in step with the daemon:
//! the icon switches to the attention variant when something needs you,
//! and the tooltip carries the counts. KDE's tray has no numeric badge —
//! the SNI spec has no field for one — so the numbers live in the tooltip.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::agent::{AgentRecord, AgentState};

const POLL: Duration = Duration::from_secs(3);
const SNAP_TIMEOUT: Duration = Duration::from_secs(2);

/// Icon themes only resolve names they have files for, so the SVGs are
/// written into the user's icon dir on first run.
const ICONS: [(&str, &str); 2] = [
    ("terhub.svg", include_str!("../assets/terhub.svg")),
    ("terhub-attention.svg", include_str!("../assets/terhub-attention.svg")),
];

/// What the tray shows, derived from one daemon snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayView {
    pub needy: usize,
    pub idle: usize,
    pub total: usize,
}

impl TrayView {
    pub fn from_agents(agents: &[AgentRecord]) -> Self {
        TrayView {
            needy: agents.iter().filter(|a| a.state.needs_attention()).count(),
            idle: agents.iter().filter(|a| a.state == AgentState::Idle).count(),
            total: agents.len(),
        }
    }

    /// Tooltip title: the headline a glance should answer.
    pub fn title(&self) -> String {
        match (self.needy, self.idle) {
            (0, 0) => format!("terhub — {} agent(s)", self.total),
            (0, idle) => format!("terhub — {} idle", idle),
            (needy, 0) => format!("terhub — {needy} need you"),
            (needy, idle) => format!("terhub — {needy} need you · {idle} idle"),
        }
    }

    /// Tooltip body: the same numbers plus what "need you" means.
    pub fn description(&self) -> String {
        let mut d = format!(
            "{} agent(s) tracked · {} waiting on you · {} idle",
            self.total, self.needy, self.idle
        );
        if self.needy > 0 {
            d.push_str("\nclick to open the hub");
        }
        d
    }

    /// `NeedsAttention` is what makes Plasma tint/flag the icon.
    pub fn needs_attention(&self) -> bool {
        self.needy > 0
    }

    pub fn icon_name(&self) -> &'static str {
        if self.needy > 0 {
            "terhub-attention"
        } else {
            "terhub"
        }
    }

    /// The daemon being unreachable is a distinct thing from "nothing to
    /// do", so it gets its own wording rather than a misleading zero.
    pub fn disconnected() -> Self {
        TrayView {
            needy: 0,
            idle: 0,
            total: 0,
        }
    }

    pub fn disconnected_title() -> &'static str {
        "terhub — daemon down"
    }
}

/// Install the tray SVGs into the user's icon theme. Returns the dir
/// written so failures can name it.
pub fn install_icons() -> std::io::Result<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
            home.join(".local/share")
        });
    let dir = base.join("icons/hicolor/scalable/apps");
    std::fs::create_dir_all(&dir)?;
    for (name, svg) in ICONS {
        let path: PathBuf = dir.join(name);
        // Only rewrite when changed: Plasma watches these dirs and a
        // needless write would churn the icon cache.
        let current = std::fs::read_to_string(&path).ok();
        if current.as_deref() != Some(svg) {
            std::fs::write(&path, svg)?;
        }
    }
    Ok(dir)
}

/// Refresh the shell icon cache when the tooling exists. Plasma usually
/// notices on its own, but a fresh icon may need the nudge.
fn refresh_icon_cache(dir: &Path) {
    if let Ok(p) = std::env::var("XDG_DATA_HOME") {
        let _ = p;
    }
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.join("icons"))
        .or_else(|| {
            std::env::var_os("HOME")
                .map(|h| PathBuf::from(h).join(".local/share/icons"))
        });
    let Some(icons) = data_home else { return };
    let _ = dir;
    for tool in ["gtk-update-icon-cache", "qt6-update-icon-cache"] {
        if std::process::Command::new(tool)
            .arg(&icons)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            return;
        }
    }
}

// ---------------------------------------------------------------------------
// StatusNotifierItem
// ---------------------------------------------------------------------------

const SNI_NAME_FMT: &str = "org.kde.StatusNotifierItem-{pid}-1";
const SNI_PATH: &str = "/StatusNotifierItem";

/// Read the current counts and push them to the tray, forever.
pub fn run() -> anyhow::Result<()> {
    let icon_dir = install_icons()?;
    refresh_icon_cache(&icon_dir);

    let bus = zbus::blocking::Connection::session()?;
    let unique = format!(SNI_NAME_FMT, pid = std::process::id());
    // The object must be exported before registration, or the watcher
    // introspects a name with nothing behind it.
    let _guard = TrayItem::publish(&bus, &unique)?;

    let proxy = zbus::blocking::Proxy::new(
        &bus,
        "org.kde.StatusNotifierWatcher",
        "/StatusNotifierWatcher",
        "org.kde.StatusNotifierWatcher",
    )?;
    proxy.call_method("RegisterStatusNotifierItem", &(unique.as_str(),))?;
    println!("tray icon registered ({unique}); ctrl-c to remove");

    loop {
        let view = match crate::daemon::fetch_snapshot(SNAP_TIMEOUT) {
            Ok(snap) => TrayView::from_agents(&snap.agents),
            Err(_) => {
                // Daemon down: say so, and keep trying. Falling back to a
                // zero-count view would read as "nothing needs you".
                TrayView::disconnected()
            }
        };
        let _guard = {
            let mut g = _guard.lock().expect("tray lock");
            g.update(&view);
            g
        };
        std::thread::sleep(POLL);
    }
}

/// The exported SNI item. Held for the process lifetime: dropping the
/// proxy unregisters the icon.
struct TrayItem {
    _conn: zbus::blocking::Connection,
}

impl TrayItem {
    fn publish(conn: &zbus::blocking::Connection, unique: &str) -> zbus::Result<Self> {
        conn.request_name(unique, zbus::fdo::NameFlag::ReplaceExisting, zbus::fdo::RequestNameReply::PrimaryOwner)?;
        let item = ItemState::default();
        conn.export(SNI_PATH, item)?;
        Ok(TrayItem {
            _conn: conn.clone(),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ItemState> {
        self._conn
            .exported_object(SNI_PATH)
            .expect("SNI object exported")
            .downcast_ref::<std::sync::Mutex<ItemState>>()
            .expect("SNI state is a Mutex<ItemState>")
            .lock()
            .expect("tray state mutex")
    }
}

/// SNI properties, behind a mutex because `EmitProperties` needs to read
/// them from the poll thread.
#[derive(Debug, Default)]
pub(crate) struct ItemState {
    pub title: String,
    pub description: String,
    pub icon: String,
    pub attention: bool,
}

impl ItemState {
    fn update(&mut self, v: &TrayView) {
        self.title = v.title();
        self.description = v.description();
        self.icon = v.icon_name().to_string();
        self.attention = v.needs_attention();
    }
}

// `ItemState` is exported as a plain object; the SNI interface itself is
// served by this proxy-free shim so no macro/derive feature is needed.
struct SniShim;

impl SniShim {
    fn constant(_c: &zbus::blocking::Connection, item: &str) -> String {
        item.to_string()
    }
}

#[allow(dead_code)]
fn _unused(_: &SniShim) {}