//! Config: theme + keybindings as TOML. Not vim, customizable.
use anyhow::{Context, Result};
use crossterm::event::KeyCode;
use ratatui::style::{Color, Modifier, Style};
use serde::Deserialize;
use std::collections::HashMap;
use std::env;
use std::path::PathBuf;

pub const DEFAULT_THEME: &str = "dark";
pub const DEFAULT_DENSITY: &str = "comfortable";
pub const DEFAULT_IDLE_FLAG_SECS: u64 = 21600;

/// Built-in theme names, in live-cycle order.
pub const THEMES: &[&str] = &[
    "dark",
    "light",
    "dracula",
    "nord",
    "gruvbox",
    "solarized",
    "tokyonight",
    "catppuccin",
    "onedark",
    "monokai",
];

pub fn is_known_theme(name: &str) -> bool {
    THEMES.iter().any(|t| t.eq_ignore_ascii_case(name))
}

/// Table density: `comfortable` shows the summary column with airy spacing,
/// `compact` drops it and tightens columns.
pub const DENSITIES: &[&str] = &["comfortable", "compact"];

pub fn is_known_density(name: &str) -> bool {
    DENSITIES.iter().any(|d| d.eq_ignore_ascii_case(name))
}

fn default_keys() -> HashMap<String, String> {
    [
        ("new", "n"),
        ("kill", "x"),
        ("rename", "r"),
        ("attach", "enter"),
        ("detach", "d"),
        ("quit", "q"),
        ("help", "?"),
        ("refresh", "f5"),
        ("attend", "a"),
        ("preview", "p"),
        ("theme", "t"),
        ("density", "v"),
        ("settings", "s"),
        ("sort", "o"),
        ("filter", "f"),
        ("mark", "space"),
        ("unmark", "u"),
        ("hide_shells", "h"),
        ("spawn", "w"),
        ("restart", "e"),
        ("pause", "z"),
        ("hosts", "m"),
        ("bulk", "b"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub keys: HashMap<String, String>,
    pub theme: String,
    pub density: String,
    pub highlight_attached: bool,
    pub notifications_enabled: bool,
    /// Daemon tunables (restart the daemon to apply): poll cadence, stuck
    /// threshold (`0` disables stuck detection), evidence staleness.
    pub poll_secs: u64,
    pub stuck_secs: u64,
    pub stale_secs: u64,
    /// Frozen-screen → Idle threshold (`0` disables). A live agent whose
    /// screen has not changed this long finished; without it, finished
    /// agents read `working` forever.
    pub quiet_idle_secs: u64,
    /// Validated `[[command]]` entries (invalid ones become problems).
    pub commands: Vec<CustomCommand>,
    /// Validated `[[profile]]` spawn entries; builtins when none given.
    pub profiles: Vec<SpawnProfile>,
    /// Validated `[[template]]` message snippets; builtins when none given.
    pub templates: Vec<MessageTemplate>,
    /// Order session tabs by recent activity instead of tmux order.
    pub mru_tabs: bool,
    /// Hash-color Agent cells so the same agent reads as a group.
    pub color_agents: bool,
    /// Style specs the agent hash picks from; empty = theme default.
    pub agent_palette: Vec<String>,
    /// Per-agent style-spec overrides, keyed by lowercase agent name.
    pub agent_styles: HashMap<String, String>,
    /// Tint Idle when working + quiet past `stuck_secs` (the stuck smell).
    pub idle_warn: bool,
    /// Style spec for that tint.
    pub idle_warn_style: String,
    /// Tint Idle when idle + quiet past this (a day-scale idle stops
    /// looking fine; `0` disables, like `stuck_secs`).
    pub idle_flag_secs: u64,
}

impl Default for AppConfig {
    fn default() -> Self {
        AppConfig {
            keys: default_keys(),
            theme: DEFAULT_THEME.to_string(),
            density: DEFAULT_DENSITY.to_string(),
            highlight_attached: true,
            notifications_enabled: true,
            poll_secs: crate::daemon::POLL_SECS,
            stuck_secs: crate::daemon::STUCK_SECS,
            stale_secs: crate::daemon::HOOK_STALE_SECS,
            quiet_idle_secs: crate::daemon::QUIET_IDLE_SECS,
            commands: Vec::new(),
            profiles: builtin_profiles(),
            templates: builtin_templates(),
            mru_tabs: false,
            color_agents: true,
            agent_palette: Vec::new(),
            agent_styles: HashMap::new(),
            idle_warn: true,
            idle_warn_style: "bold yellow".to_string(),
            idle_flag_secs: DEFAULT_IDLE_FLAG_SECS,
        }
    }
}

impl AppConfig {
    pub fn key(&self, action: &str) -> &str {
        self.keys.get(action).map(String::as_str).unwrap_or("")
    }

    pub fn spec(&self, action: &str) -> KeySpec {
        parse_key(self.key(action))
    }

    /// Theme palette name; unknown values fall back to dark (incl. the old
    /// Python default "textual-dark"). Returns the canonical lowercase
    /// name so "Dark" keeps working downstream of this call.
    pub fn palette(&self) -> &str {
        THEMES
            .iter()
            .find(|t| t.eq_ignore_ascii_case(&self.theme))
            .copied()
            .unwrap_or("dark")
    }

    /// Effective density; unknown values fall back to comfortable.
    /// Canonical lowercase, like [`Self::palette`].
    pub fn density_name(&self) -> &str {
        DENSITIES
            .iter()
            .find(|d| d.eq_ignore_ascii_case(&self.density))
            .copied()
            .unwrap_or("comfortable")
    }

    pub fn compact(&self) -> bool {
        self.density_name() == "compact"
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeySpec {
    Char(char),
    Code(KeyCode),
    None,
}

/// Parse a config key name: single chars ("n", "?"), names ("enter", "esc",
/// "space", "tab", "up"...), function keys ("f5"), or the legacy
/// "question_mark" alias.
pub fn parse_key(s: &str) -> KeySpec {
    let s = s.trim();
    if s.is_empty() {
        return KeySpec::None;
    }
    if s.chars().count() == 1 {
        return KeySpec::Char(s.chars().next().unwrap_or('\0'));
    }
    match s.to_lowercase().as_str() {
        "enter" | "return" => KeySpec::Code(KeyCode::Enter),
        "esc" | "escape" => KeySpec::Code(KeyCode::Esc),
        "tab" => KeySpec::Code(KeyCode::Tab),
        "backspace" => KeySpec::Code(KeyCode::Backspace),
        "delete" | "del" => KeySpec::Code(KeyCode::Delete),
        "insert" | "ins" => KeySpec::Code(KeyCode::Insert),
        "space" => KeySpec::Char(' '),
        "up" => KeySpec::Code(KeyCode::Up),
        "down" => KeySpec::Code(KeyCode::Down),
        "left" => KeySpec::Code(KeyCode::Left),
        "right" => KeySpec::Code(KeyCode::Right),
        "home" => KeySpec::Code(KeyCode::Home),
        "end" => KeySpec::Code(KeyCode::End),
        "pageup" | "pgup" => KeySpec::Code(KeyCode::PageUp),
        "pagedown" | "pgdn" => KeySpec::Code(KeyCode::PageDown),
        "question_mark" => KeySpec::Char('?'),
        // Real function keys run F1–F24; f0/f99 are typos, not keys.
        f if f.len() > 1 && f.starts_with('f') => f[1..]
            .parse::<u8>()
            .ok()
            .filter(|n| (1..=24).contains(n))
            .map(KeyCode::F)
            .map(KeySpec::Code)
            .unwrap_or(KeySpec::None),
        _ => KeySpec::None,
    }
}

pub fn key_matches(spec: &KeySpec, code: KeyCode) -> bool {
    match (spec, code) {
        (KeySpec::Char(a), KeyCode::Char(b)) => {
            if a.is_ascii_alphabetic() && b.is_ascii_alphabetic() {
                a.eq_ignore_ascii_case(&b)
            } else {
                a == &b
            }
        }
        (KeySpec::Code(a), b) => a == &b,
        _ => false,
    }
}

/// Collision check mirroring [`key_matches`]: alphabetic chars collide
/// case-insensitively (a custom `G` steals builtin `g` at runtime), so an
/// exact `KeySpec` equality here would bless bindings that can never fire.
fn spec_conflicts(a: &KeySpec, b: &KeySpec) -> bool {
    match (a, b) {
        (KeySpec::Char(x), KeySpec::Char(y)) => {
            if x.is_ascii_alphabetic() && y.is_ascii_alphabetic() {
                x.eq_ignore_ascii_case(y)
            } else {
                x == y
            }
        }
        _ => a == b,
    }
}

/// One token of a style spec: a modifier, a foreground color, an explicit
/// reset, or something unknown (ignored by the parser, flagged by `--check`).
enum SpecToken {
    Mod(Modifier),
    Fg(Color),
    Reset,
    Unknown,
}

fn spec_token(tok: &str) -> SpecToken {
    let t = tok.to_lowercase();
    match t.as_str() {
        "bold" => SpecToken::Mod(Modifier::BOLD),
        "dim" => SpecToken::Mod(Modifier::DIM),
        "italic" => SpecToken::Mod(Modifier::ITALIC),
        "underlined" | "underline" => SpecToken::Mod(Modifier::UNDERLINED),
        "reverse" | "reversed" | "invert" => SpecToken::Mod(Modifier::REVERSED),
        "crossed" | "crossed-out" | "strikethrough" => SpecToken::Mod(Modifier::CROSSED_OUT),
        "blink" => SpecToken::Mod(Modifier::SLOW_BLINK),
        "none" | "default" | "plain" => SpecToken::Reset,
        _ => match parse_color_name(&t) {
            Some(c) => SpecToken::Fg(c),
            None => SpecToken::Unknown,
        },
    }
}

/// Parse a style spec (`"bold yellow"`, `"red"`, `"#ff8800"`): optional
/// modifiers plus a foreground color, case-insensitive. Unknown tokens are
/// ignored so a typo degrades to plainer text instead of breaking the UI;
/// `config --check` names them.
pub fn parse_style_spec(s: &str) -> Style {
    let mut st = Style::default();
    for tok in s.split_whitespace() {
        match spec_token(tok) {
            SpecToken::Mod(m) => st = st.add_modifier(m),
            SpecToken::Fg(c) => st = st.fg(c),
            SpecToken::Reset => st = Style::default(),
            SpecToken::Unknown => {}
        }
    }
    st
}

/// True when every token of the spec is known (empty means default style).
pub fn valid_style_spec(s: &str) -> bool {
    s.split_whitespace()
        .all(|tok| !matches!(spec_token(tok), SpecToken::Unknown))
}

fn parse_color_name(t: &str) -> Option<Color> {
    if let Some(hex) = t.strip_prefix('#') {
        return parse_hex_color(hex);
    }
    let (bright, base) = match t
        .strip_prefix("bright-")
        .or_else(|| t.strip_prefix("light-"))
    {
        Some(b) => (true, b),
        None => (false, t),
    };
    let normal = match base {
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "gray" | "grey" => Color::Gray,
        "white" => Color::White,
        _ => return None,
    };
    Some(if bright {
        match normal {
            Color::Black => Color::DarkGray,
            Color::Red => Color::LightRed,
            Color::Green => Color::LightGreen,
            Color::Yellow => Color::LightYellow,
            Color::Blue => Color::LightBlue,
            Color::Magenta => Color::LightMagenta,
            Color::Cyan => Color::LightCyan,
            Color::Gray | Color::White => Color::White,
            _ => normal,
        }
    } else {
        normal
    })
}

fn parse_hex_color(hex: &str) -> Option<Color> {
    let bytes = hex.as_bytes();
    let (r, g, b) = match bytes.len() {
        3 => (
            hex_val(bytes[0]).map(|v| v * 17)?,
            hex_val(bytes[1]).map(|v| v * 17)?,
            hex_val(bytes[2]).map(|v| v * 17)?,
        ),
        6 => (
            hex_byte(bytes[0], bytes[1])?,
            hex_byte(bytes[2], bytes[3])?,
            hex_byte(bytes[4], bytes[5])?,
        ),
        _ => return None,
    };
    Some(Color::Rgb(r, g, b))
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

fn hex_byte(hi: u8, lo: u8) -> Option<u8> {
    Some(hex_val(hi)? * 16 + hex_val(lo)?)
}

#[derive(Debug, Deserialize, Default)]
struct FileConfig {
    #[serde(default)]
    ui: UiSection,
    #[serde(default)]
    keys: HashMap<String, String>,
    #[serde(default)]
    notifications: NotificationsSection,
    #[serde(default)]
    daemon: DaemonSection,
    #[serde(default)]
    command: Vec<CommandFile>,
    #[serde(default)]
    profile: Vec<ProfileFile>,
    #[serde(default)]
    template: Vec<TemplateFile>,
    #[serde(default)]
    style: StyleSection,
}

/// One `[[command]]` entry: a user shell command on the selected pane.
#[derive(Debug, Deserialize, Default)]
struct CommandFile {
    #[serde(default)]
    name: String,
    #[serde(default)]
    key: String,
    #[serde(default)]
    run: String,
}

/// Validated custom command. `{pane} {session} {window} {dir} {cmd}
/// {agent} {state}` expand against the selected row.
#[derive(Debug, Clone)]
pub struct CustomCommand {
    pub name: String,
    pub key: String,
    pub spec: KeySpec,
    pub run: String,
}

pub const COMMAND_VARS: &[&str] = &["pane", "session", "window", "dir", "cmd", "agent", "state"];

/// One `[[profile]]` entry: a named agent program for spawn-with-prompt.
#[derive(Debug, Deserialize, Default)]
struct ProfileFile {
    #[serde(default)]
    name: String,
    #[serde(default)]
    program: String,
    #[serde(default)]
    dir: Option<String>,
}

/// Validated spawn profile. `dir` overrides the start directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnProfile {
    pub name: String,
    pub program: String,
    pub dir: Option<String>,
}

/// Builtin agents offered when the user defines no `[[profile]]`.
pub fn builtin_profiles() -> Vec<SpawnProfile> {
    ["claude", "codex", "pi", "gemini", "muse"]
        .into_iter()
        .map(|a| SpawnProfile {
            name: a.to_string(),
            program: a.to_string(),
            dir: None,
        })
        .collect()
}

/// One `[[template]]` entry: a named reusable message for bulk prompt
/// and `send --template`.
#[derive(Debug, Deserialize, Default)]
struct TemplateFile {
    #[serde(default)]
    name: String,
    #[serde(default)]
    text: String,
}

/// Validated message template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageTemplate {
    pub name: String,
    pub text: String,
}

/// Builtin messages offered when the user defines no `[[template]]`.
pub fn builtin_templates() -> Vec<MessageTemplate> {
    [
        ("continue", "continue"),
        (
            "status",
            "Summarize your current status in two lines: what is done, what is next.",
        ),
        (
            "review",
            "Review your own work for mistakes and fix what you find.",
        ),
    ]
    .into_iter()
    .map(|(name, text)| MessageTemplate {
        name: name.to_string(),
        text: text.to_string(),
    })
    .collect()
}

#[derive(Debug, Deserialize)]
struct NotificationsSection {
    #[serde(default = "default_true")]
    enabled: bool,
}

impl Default for NotificationsSection {
    fn default() -> Self {
        NotificationsSection { enabled: true }
    }
}

#[derive(Debug, Deserialize)]
struct UiSection {
    #[serde(default = "default_theme")]
    theme: String,
    #[serde(default = "default_density")]
    density: String,
    #[serde(default = "default_true")]
    highlight_attached: bool,
    #[serde(default)]
    mru_tabs: bool,
}

impl Default for UiSection {
    fn default() -> Self {
        UiSection {
            theme: default_theme(),
            density: default_density(),
            highlight_attached: true,
            mru_tabs: false,
        }
    }
}

fn default_theme() -> String {
    DEFAULT_THEME.to_string()
}

fn default_density() -> String {
    DEFAULT_DENSITY.to_string()
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize)]
struct DaemonSection {
    #[serde(default = "default_poll_secs")]
    poll_secs: u64,
    #[serde(default = "default_stuck_secs")]
    stuck_secs: u64,
    #[serde(default = "default_stale_secs")]
    stale_secs: u64,
    #[serde(default = "default_quiet_idle_secs")]
    quiet_idle_secs: u64,
}

impl Default for DaemonSection {
    fn default() -> Self {
        DaemonSection {
            poll_secs: default_poll_secs(),
            stuck_secs: default_stuck_secs(),
            stale_secs: default_stale_secs(),
            quiet_idle_secs: default_quiet_idle_secs(),
        }
    }
}

fn default_poll_secs() -> u64 {
    crate::daemon::POLL_SECS
}

fn default_stuck_secs() -> u64 {
    crate::daemon::STUCK_SECS
}

fn default_stale_secs() -> u64 {
    crate::daemon::HOOK_STALE_SECS
}

fn default_quiet_idle_secs() -> u64 {
    crate::daemon::QUIET_IDLE_SECS
}

#[derive(Debug, Deserialize)]
struct StyleSection {
    #[serde(default = "default_true")]
    color_agents: bool,
    #[serde(default)]
    agent_palette: Vec<String>,
    #[serde(default)]
    agents: HashMap<String, String>,
    #[serde(default = "default_true")]
    idle_warn: bool,
    #[serde(default = "default_idle_warn_style")]
    idle_warn_style: String,
    #[serde(default = "default_idle_flag_secs")]
    idle_flag_secs: u64,
}

impl Default for StyleSection {
    fn default() -> Self {
        StyleSection {
            color_agents: true,
            agent_palette: Vec::new(),
            agents: HashMap::new(),
            idle_warn: true,
            idle_warn_style: default_idle_warn_style(),
            idle_flag_secs: default_idle_flag_secs(),
        }
    }
}

fn default_idle_warn_style() -> String {
    "bold yellow".to_string()
}

fn default_idle_flag_secs() -> u64 {
    DEFAULT_IDLE_FLAG_SECS
}

/// User screen-pattern manifests live next to the config file.
pub fn manifests_dir() -> PathBuf {
    config_path()
        .parent()
        .map(|p| p.join("manifests"))
        .unwrap_or_else(|| PathBuf::from("manifests"))
}

pub fn config_path() -> PathBuf {
    // Test seam (and power-user escape hatch): point terhub at any file.
    if let Ok(p) = env::var("TERHUB_CONFIG")
        && !p.is_empty()
    {
        return PathBuf::from(p);
    }
    let base = env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .ok()
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| {
            env::var("HOME")
                .ok()
                .filter(|h| !h.is_empty())
                .map(|h| PathBuf::from(h).join(".config"))
        })
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("terhub").join("config.toml")
}

pub fn load_config(path: Option<PathBuf>) -> AppConfig {
    load_config_report(path).0
}

/// Persist UI settings back to the config file with surgical line edits:
/// only the four known keys are touched, everything else (comments, key
/// bindings, manifests) is preserved byte-for-byte.
pub fn persist_ui_settings(
    theme: &str,
    density: &str,
    notifications: bool,
    highlight: bool,
    mru_tabs: bool,
) -> anyhow::Result<()> {
    persist_ui_settings_to(
        &config_path(),
        theme,
        density,
        notifications,
        highlight,
        mru_tabs,
    )
}

fn persist_ui_settings_to(
    path: &PathBuf,
    theme: &str,
    density: &str,
    notifications: bool,
    highlight: bool,
    mru_tabs: bool,
) -> anyhow::Result<()> {
    let text = std::fs::read_to_string(path).unwrap_or_else(|_| default_config_text());
    let mut out: Vec<String> = Vec::new();
    let mut section = String::new();
    let mut seen = [false; 5]; // theme, density, highlight, enabled, mru_tabs
    let (mut ui_at, mut notif_at): (Option<usize>, Option<usize>) = (None, None);
    for line in text.lines() {
        // Refresh our own stale generated header while we're here.
        if line == "# terhub config: keys are single-key bindings, theme is dark or light." {
            out.push("# terhub config: keys are single-key bindings.".to_string());
            out.push(
                "# theme: dark, light, dracula, nord, gruvbox, solarized, tokyonight, catppuccin, onedark, monokai. density: comfortable, compact."
                    .to_string(),
            );
            continue;
        }
        let t = line.trim();
        if t.starts_with('[') && t.ends_with(']') {
            section = t.to_string();
            if section == "[ui]" {
                ui_at = Some(out.len());
            }
            if section == "[notifications]" {
                notif_at = Some(out.len());
            }
            out.push(line.to_string());
            continue;
        }
        if section == "[ui]" && is_key_line(line, "theme") {
            out.push(format!("theme = \"{theme}\""));
            seen[0] = true;
        } else if section == "[ui]" && is_key_line(line, "density") {
            out.push(format!("density = \"{density}\""));
            seen[1] = true;
        } else if section == "[ui]" && is_key_line(line, "highlight_attached") {
            out.push(format!("highlight_attached = {highlight}"));
            seen[2] = true;
        } else if section == "[ui]" && is_key_line(line, "mru_tabs") {
            out.push(format!("mru_tabs = {mru_tabs}"));
            seen[4] = true;
        } else if section == "[notifications]" && is_key_line(line, "enabled") {
            out.push(format!("enabled = {notifications}"));
            seen[3] = true;
        } else {
            out.push(line.to_string());
        }
    }
    // Insert whatever the file lacked directly under its section header
    // (appending at EOF would land it in the wrong section).
    let mut missing_ui = Vec::new();
    if !seen[0] {
        missing_ui.push(format!("theme = \"{theme}\""));
    }
    if !seen[1] {
        missing_ui.push(format!("density = \"{density}\""));
    }
    if !seen[2] {
        missing_ui.push(format!("highlight_attached = {highlight}"));
    }
    if !seen[4] {
        missing_ui.push(format!("mru_tabs = {mru_tabs}"));
    }
    let mut ui_inserted = 0;
    if !missing_ui.is_empty() {
        match ui_at {
            Some(i) => {
                for (k, line) in missing_ui.into_iter().enumerate() {
                    out.insert(i + 1 + k, line);
                    ui_inserted += 1;
                }
            }
            None => {
                out.push("[ui]".to_string());
                out.extend(missing_ui);
            }
        }
    }
    if !seen[3] {
        let line = format!("enabled = {notifications}");
        match notif_at {
            Some(i) => {
                let shift = if ui_at.is_some_and(|u| u < i) {
                    ui_inserted
                } else {
                    0
                };
                out.insert(i + 1 + shift, line);
            }
            None => {
                out.push("[notifications]".to_string());
                out.push(line);
            }
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, out.join("\n") + "\n")?;
    Ok(())
}

/// True when the line assigns `key` (ignores comments and lookalikes).
fn is_key_line(line: &str, key: &str) -> bool {
    let t = line.trim_start();
    t.strip_prefix(key)
        .is_some_and(|rest| rest.trim_start().starts_with('='))
        && !t.starts_with('#')
}

/// Byte-cap a message, flooring to a char boundary. `String::truncate`
/// panics mid-multibyte-char, which would turn a config error report
/// into a crash.
fn truncate_msg(s: &str, max: usize) -> String {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Load config plus human-readable problems (syntax errors, unknown keys or
/// theme). A missing file is fine (defaults); a broken file must say so
/// instead of silently behaving like defaults.
pub fn load_config_report(path: Option<PathBuf>) -> (AppConfig, Vec<String>) {
    let mut cfg = AppConfig::default();
    let mut problems = Vec::new();
    let path = path.unwrap_or_else(config_path);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return (cfg, problems),
    };
    let file: FileConfig = match toml::from_str(&text) {
        Ok(f) => f,
        Err(e) => {
            let msg = truncate_msg(&e.to_string().replace('\n', " "), 160);
            problems.push(format!("{}: {msg}", path.display()));
            return (cfg, problems);
        }
    };
    for (k, v) in file.keys {
        if !cfg.keys.contains_key(&k) {
            problems.push(format!("unknown key '{k}' (known: {})", known_keys()));
        }
        cfg.keys.insert(k, v);
    }
    // Builtin values are validated like custom ones: a typo/empty/f99
    // would otherwise parse to KeySpec::None and silently deaden the
    // action. Bad values revert to their default (poll_secs precedent).
    // Sorted actions keep problem order deterministic.
    let defaults = default_keys();
    let mut actions: Vec<String> = cfg.keys.keys().cloned().collect();
    actions.sort();
    for action in &actions {
        let value = cfg.keys[action].clone();
        if parse_key(&value) == KeySpec::None
            && let Some(def) = defaults.get(action)
        {
            problems.push(format!(
                "key '{action}' has bad value '{value}' (using default '{def}')"
            ));
            cfg.keys.insert(action.clone(), def.clone());
        }
    }
    // Builtin-vs-builtin collisions after rebinding: first-match dispatch
    // would silently shadow one of the pair.
    let specs: Vec<(String, KeySpec)> = actions
        .iter()
        .filter(|a| defaults.contains_key(a.as_str()))
        .map(|a| (a.clone(), parse_key(&cfg.keys[a])))
        .collect();
    for (i, (a, sa)) in specs.iter().enumerate() {
        for (b, sb) in specs.iter().skip(i + 1) {
            if *sa != KeySpec::None && spec_conflicts(sa, sb) {
                problems.push(format!("keys '{a}' and '{b}' both use '{}'", cfg.keys[a]));
            }
        }
    }
    cfg.theme = file.ui.theme;
    if !is_known_theme(&cfg.theme) {
        problems.push(format!(
            "unknown theme '{}' (known: {})",
            cfg.theme,
            THEMES.join(", ")
        ));
    }
    cfg.density = file.ui.density;
    if !is_known_density(&cfg.density) {
        problems.push(format!(
            "unknown density '{}' (known: {})",
            cfg.density,
            DENSITIES.join(", ")
        ));
    }
    cfg.highlight_attached = file.ui.highlight_attached;
    cfg.mru_tabs = file.ui.mru_tabs;
    cfg.notifications_enabled = file.notifications.enabled;
    // Zero poll/stale would wedge or blind the daemon; stuck_secs = 0 is a
    // legitimate "off switch" and stays.
    cfg.poll_secs = file.daemon.poll_secs;
    if cfg.poll_secs == 0 {
        problems.push(format!(
            "poll_secs must be >= 1 (using default {})",
            crate::daemon::POLL_SECS
        ));
        cfg.poll_secs = crate::daemon::POLL_SECS;
    }
    cfg.stuck_secs = file.daemon.stuck_secs;
    cfg.quiet_idle_secs = file.daemon.quiet_idle_secs;
    cfg.stale_secs = file.daemon.stale_secs;
    if cfg.stale_secs == 0 {
        problems.push(format!(
            "stale_secs must be >= 1 (using default {})",
            crate::daemon::HOOK_STALE_SECS
        ));
        cfg.stale_secs = crate::daemon::HOOK_STALE_SECS;
    }
    // Row styling: bad specs degrade to plain text at render time; --check
    // names them so typos don't silently vanish.
    cfg.color_agents = file.style.color_agents;
    cfg.agent_palette = file.style.agent_palette;
    for (i, spec) in cfg.agent_palette.iter().enumerate() {
        if !valid_style_spec(spec) {
            problems.push(format!("style agent_palette[{i}] has bad style '{spec}'"));
        }
    }
    cfg.agent_styles = file
        .style
        .agents
        .into_iter()
        .map(|(k, v)| (k.to_lowercase(), v))
        .collect();
    for (agent, spec) in &cfg.agent_styles {
        if !valid_style_spec(spec) {
            problems.push(format!("style agent '{agent}' has bad style '{spec}'"));
        }
    }
    cfg.idle_warn = file.style.idle_warn;
    cfg.idle_warn_style = file.style.idle_warn_style;
    cfg.idle_flag_secs = file.style.idle_flag_secs;
    if !valid_style_spec(&cfg.idle_warn_style) {
        problems.push(format!(
            "style idle_warn_style has bad style '{}'",
            cfg.idle_warn_style
        ));
    }
    // Custom commands: built-ins win every key collision (checked against
    // effective bindings, so user rebinding is respected).
    let builtin: Vec<(String, KeySpec)> = default_keys()
        .into_keys()
        .map(|a| {
            let spec = cfg.spec(&a);
            (a, spec)
        })
        .collect();
    let mut taken: Vec<KeySpec> = Vec::new();
    for (i, c) in file.command.iter().enumerate() {
        let tag = if c.name.is_empty() {
            format!("command #{i}")
        } else {
            format!("command '{}'", c.name)
        };
        if c.name.is_empty() {
            problems.push(format!("{tag} has no name"));
            continue;
        }
        if c.run.is_empty() {
            problems.push(format!("{tag} has no run template"));
            continue;
        }
        let spec = parse_key(&c.key);
        if spec == KeySpec::None {
            problems.push(format!("{tag} has bad key '{}'", c.key));
            continue;
        }
        if let Some((action, _)) = builtin.iter().find(|(_, s)| spec_conflicts(s, &spec)) {
            problems.push(format!(
                "{tag} key '{}' collides with built-in '{action}'",
                c.key
            ));
            continue;
        }
        if taken.iter().any(|t| spec_conflicts(t, &spec)) {
            problems.push(format!("{tag} key '{}' is already used", c.key));
            continue;
        }
        if let Some(var) = first_unknown_var(&c.run) {
            problems.push(format!(
                "{tag} has unknown {{{var}}} (known: {})",
                COMMAND_VARS.join(", ")
            ));
            continue;
        }
        taken.push(spec.clone());
        cfg.commands.push(CustomCommand {
            name: c.name.clone(),
            key: c.key.clone(),
            spec,
            run: c.run.clone(),
        });
    }
    // Spawn profiles: user entries replace the builtins wholesale (an
    // explicit list means "these are my agents"); invalid ones become
    // problems and the survivors still apply.
    if !file.profile.is_empty() {
        let mut profiles = Vec::new();
        for (i, p) in file.profile.iter().enumerate() {
            let tag = if p.name.is_empty() {
                format!("profile #{i}")
            } else {
                format!("profile '{}'", p.name)
            };
            if p.name.is_empty() {
                problems.push(format!("{tag} has no name"));
                continue;
            }
            if p.program.trim().is_empty() {
                problems.push(format!("{tag} has no program"));
                continue;
            }
            if profiles.iter().any(|q: &SpawnProfile| q.name == p.name) {
                problems.push(format!("{tag} is a duplicate name"));
                continue;
            }
            profiles.push(SpawnProfile {
                name: p.name.clone(),
                program: p.program.clone(),
                dir: p.dir.clone(),
            });
        }
        cfg.profiles = profiles;
    }
    // Message templates: same wholesale rule as profiles.
    if !file.template.is_empty() {
        let mut templates = Vec::new();
        for (i, t) in file.template.iter().enumerate() {
            let tag = if t.name.is_empty() {
                format!("template #{i}")
            } else {
                format!("template '{}'", t.name)
            };
            if t.name.is_empty() {
                problems.push(format!("{tag} has no name"));
                continue;
            }
            if t.text.trim().is_empty() {
                problems.push(format!("{tag} has no text"));
                continue;
            }
            if templates.iter().any(|q: &MessageTemplate| q.name == t.name) {
                problems.push(format!("{tag} is a duplicate name"));
                continue;
            }
            templates.push(MessageTemplate {
                name: t.name.clone(),
                text: t.text.clone(),
            });
        }
        cfg.templates = templates;
    }
    (cfg, problems)
}

/// First `{var}` in the template that isn't a known expansion, if any.
/// `{{` escapes a literal brace.
fn first_unknown_var(run: &str) -> Option<String> {
    let chars: Vec<char> = run.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '{' {
            if chars.get(i + 1) == Some(&'{') {
                i += 2;
                continue;
            }
            if let Some(end) = chars[i..].iter().position(|c| *c == '}') {
                let var: String = chars[i + 1..i + end].iter().collect();
                if !COMMAND_VARS.contains(&var.as_str()) {
                    return Some(var);
                }
                i += end + 1;
                continue;
            }
        }
        i += 1;
    }
    None
}

fn known_keys() -> String {
    let mut keys: Vec<String> = default_keys().into_keys().collect();
    keys.sort();
    keys.join(", ")
}

pub fn default_config_text() -> String {
    let defaults = default_keys();
    let mut keys: Vec<(&str, &str)> = defaults
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    keys.sort();
    let mut out = String::from(
        "[ui]\ntheme = \"dark\"\ndensity = \"comfortable\"\nhighlight_attached = true\n\n[keys]\n",
    );
    for (k, v) in keys {
        out.push_str(&format!("{k} = \"{v}\"\n"));
    }
    out.push_str("\n[notifications]\nenabled = true\n");
    out.push_str(&format!(
        "\n[daemon]\n# Restart the daemon to apply. stuck_secs = 0 disables stuck detection.\n# quiet_idle_secs = 0 disables frozen-screen → idle.\npoll_secs = {}\nstuck_secs = {}\nstale_secs = {}\nquiet_idle_secs = {}\n",
        crate::daemon::POLL_SECS,
        crate::daemon::STUCK_SECS,
        crate::daemon::HOOK_STALE_SECS,
        crate::daemon::QUIET_IDLE_SECS
    ));
    out.push_str(
        "\n# Custom pane commands (key must not collide with a built-in):\n# [[command]]\n# name = \"git status\"\n# key = \"G\"\n# run = \"git -C {dir} status -sb\"\n",
    );
    out.push_str(
        "\n# Reusable messages for bulk prompt (Tab) and send --template (yours replace the builtins):\n# [[template]]\n# name = \"continue\"\n# text = \"continue\"\n",
    );
    out.push_str(
        "\n# Row styling (specs: modifiers + a color, e.g. \"bold yellow\", \"red\", \"#ff8800\"):\n# [style]\n# color_agents = true\n# agent_palette = []\n# idle_warn = true\n# idle_warn_style = \"bold yellow\"\n# idle_flag_secs = 21600\n# [style.agents]\n# pi = \"cyan\"\n",
    );
    out
}

pub fn ensure_config() -> Result<PathBuf> {
    let path = config_path();
    if !path.exists() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create config dir {}", parent.display()))?;
        }
        let text = "# terhub config: keys are single-key bindings.\n# theme: dark, light, dracula, nord, gruvbox, solarized, tokyonight, catppuccin, onedark, monokai. density: comfortable, compact.\n"
            .to_string()
            + &default_config_text();
        std::fs::write(&path, text)
            .with_context(|| format!("failed to write {}", path.display()))?;
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unique scratch dir per call: lib tests run in parallel threads and
    /// must never share one `config.toml` (write/read races flake).
    fn test_dir() -> std::path::PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::env::temp_dir().join(format!("terhub-test-{}-{n}", std::process::id()))
    }

    #[test]
    fn truncate_msg_floors_to_char_boundary() {
        let s = format!("{}é{}", "a".repeat(159), "b".repeat(10));
        // Byte 160 is the second byte of é: a naive truncate panics.
        let out = truncate_msg(&s, 160);
        assert_eq!(out, "a".repeat(159));
        assert!(out.len() <= 160);
        assert_eq!(truncate_msg("hello", 160), "hello");
    }

    #[test]
    fn parse_common_keys() {
        assert_eq!(parse_key("n"), KeySpec::Char('n'));
        assert_eq!(parse_key("?"), KeySpec::Char('?'));
        assert_eq!(parse_key("question_mark"), KeySpec::Char('?'));
        assert_eq!(parse_key("enter"), KeySpec::Code(KeyCode::Enter));
        assert_eq!(parse_key("ESC"), KeySpec::Code(KeyCode::Esc));
        assert_eq!(parse_key("f5"), KeySpec::Code(KeyCode::F(5)));
        assert_eq!(parse_key("f12"), KeySpec::Code(KeyCode::F(12)));
        assert_eq!(parse_key(""), KeySpec::None);
        assert_eq!(parse_key("nope-not-a-key"), KeySpec::None);
    }

    #[test]
    fn fkey_range_rejects_absurd() {
        assert_eq!(parse_key("f0"), KeySpec::None);
        assert_eq!(parse_key("f99"), KeySpec::None);
        assert_eq!(parse_key("f25"), KeySpec::None);
        assert_eq!(parse_key("f1"), KeySpec::Code(KeyCode::F(1)));
        assert_eq!(parse_key("f24"), KeySpec::Code(KeyCode::F(24)));
    }

    #[test]
    fn letter_match_is_case_insensitive() {
        assert!(key_matches(&KeySpec::Char('n'), KeyCode::Char('N')));
        assert!(key_matches(&KeySpec::Char('q'), KeyCode::Char('q')));
        assert!(!key_matches(&KeySpec::Char('n'), KeyCode::Char('x')));
        assert!(!key_matches(&KeySpec::None, KeyCode::Char('n')));
    }

    #[test]
    fn style_spec_parses_modifiers_colors_hex() {
        let st = parse_style_spec("bold yellow");
        assert_eq!(st.fg, Some(Color::Yellow));
        assert!(st.add_modifier.contains(Modifier::BOLD));
        assert_eq!(parse_style_spec("red").fg, Some(Color::Red));
        assert_eq!(parse_style_spec("BOLD BLUE").fg, Some(Color::Blue));
        assert_eq!(parse_style_spec("bright-cyan").fg, Some(Color::LightCyan));
        assert_eq!(parse_style_spec("light-black").fg, Some(Color::DarkGray));
        assert_eq!(parse_style_spec("grey").fg, Some(Color::Gray));
        assert_eq!(
            parse_style_spec("#ff8800").fg,
            Some(Color::Rgb(0xff, 0x88, 0x00))
        );
        assert_eq!(
            parse_style_spec("#f80").fg,
            Some(Color::Rgb(0xff, 0x88, 0x00))
        );
        // Unknown tokens degrade, never break.
        assert_eq!(parse_style_spec("nope"), Style::default());
        assert_eq!(parse_style_spec(""), Style::default());
        let st = parse_style_spec("bold nope red");
        assert_eq!(st.fg, Some(Color::Red));
        assert!(st.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn style_spec_validity_flags_unknown_tokens() {
        assert!(valid_style_spec(""));
        assert!(valid_style_spec("bold yellow"));
        assert!(valid_style_spec("#abc123"));
        assert!(valid_style_spec("none"));
        assert!(!valid_style_spec("nope"));
        assert!(!valid_style_spec("bold nope"));
        assert!(!valid_style_spec("#xyz"));
        assert!(!valid_style_spec("#12345"));
    }

    #[test]
    fn partial_config_merges_over_defaults() {
        let dir = test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "[keys]\nnew = \"c\"\n").unwrap();
        let cfg = load_config(Some(path.clone()));
        assert_eq!(cfg.key("new"), "c");
        assert_eq!(cfg.key("kill"), "x");
        assert_eq!(cfg.theme, "dark");
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn daemon_section_parses_with_defaults() {
        let dir = test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("daemon.toml");
        std::fs::write(&path, "[daemon]\nstuck_secs = 60\n").unwrap();
        let (cfg, problems) = load_config_report(Some(path.clone()));
        assert!(problems.is_empty());
        assert_eq!(cfg.stuck_secs, 60);
        assert_eq!(cfg.poll_secs, crate::daemon::POLL_SECS);
        assert_eq!(cfg.stale_secs, crate::daemon::HOOK_STALE_SECS);
        assert_eq!(cfg.quiet_idle_secs, crate::daemon::QUIET_IDLE_SECS);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn style_section_parses_idle_flag_with_default() {
        let dir = test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("style.toml");
        std::fs::write(&path, "[style]\nidle_flag_secs = 60\n").unwrap();
        let (cfg, problems) = load_config_report(Some(path.clone()));
        assert!(problems.is_empty());
        assert_eq!(cfg.idle_flag_secs, 60);
        assert!(cfg.idle_warn);
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, "[style]\n").unwrap();
        let (cfg, _) = load_config_report(Some(path.clone()));
        assert_eq!(cfg.idle_flag_secs, DEFAULT_IDLE_FLAG_SECS);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn zero_poll_and_stale_fall_back_with_problems() {
        let dir = test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("daemon-zero.toml");
        std::fs::write(
            &path,
            "[daemon]\npoll_secs = 0\nstuck_secs = 0\nstale_secs = 0\n",
        )
        .unwrap();
        let (cfg, problems) = load_config_report(Some(path.clone()));
        assert_eq!(problems.len(), 2);
        assert_eq!(cfg.poll_secs, crate::daemon::POLL_SECS);
        assert_eq!(cfg.stale_secs, crate::daemon::HOOK_STALE_SECS);
        // stuck_secs = 0 is the documented off switch, not an error.
        assert_eq!(cfg.stuck_secs, 0);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn custom_commands_validate() {
        let dir = test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("commands.toml");
        std::fs::write(
            &path,
            "[[command]]\nname = \"gs\"\nkey = \"G\"\nrun = \"git -C {dir} status\"\n\n\
             [[command]]\nname = \"bad\"\nkey = \"n\"\nrun = \"echo x\"\n\n\
             [[command]]\nname = \"bogus\"\nkey = \"Y\"\nrun = \"echo {bogus}\"\n\n\
             [[command]]\nname = \"\"\nkey = \"E\"\nrun = \"echo x\"\n",
        )
        .unwrap();
        let (cfg, problems) = load_config_report(Some(path.clone()));
        assert_eq!(cfg.commands.len(), 1);
        assert_eq!(cfg.commands[0].name, "gs");
        assert_eq!(problems.len(), 3);
        assert!(problems.iter().any(|p| p.contains("collides")));
        assert!(problems.iter().any(|p| p.contains("{bogus}")));
        assert!(problems.iter().any(|p| p.contains("no name")));
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn message_templates_validate_with_builtins() {
        let dir = test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("templates.toml");
        std::fs::write(
            &path,
            "[[template]]\nname = \"go\"\ntext = \"continue\"\n\n\
             [[template]]\nname = \"\"\ntext = \"x\"\n\n\
             [[template]]\nname = \"empty\"\n\n\
             [[template]]\nname = \"go\"\ntext = \"again\"\n",
        )
        .unwrap();
        let (cfg, problems) = load_config_report(Some(path.clone()));
        assert_eq!(cfg.templates.len(), 1);
        assert_eq!(cfg.templates[0].name, "go");
        assert_eq!(cfg.templates[0].text, "continue");
        assert_eq!(problems.len(), 3);
        assert!(problems.iter().any(|p| p.contains("no name")));
        assert!(problems.iter().any(|p| p.contains("no text")));
        assert!(problems.iter().any(|p| p.contains("duplicate")));
        std::fs::remove_file(path).ok();
        // No [[template]] at all: builtins keep bulk prompt usable.
        let (cfg, _) = load_config_report(Some(dir.join("missing.toml")));
        assert!(!cfg.templates.is_empty());
        assert!(cfg.templates.iter().any(|t| t.name == "continue"));
    }

    #[test]
    fn spawn_profiles_validate_with_builtins() {
        // Valid entries survive; nameless, program-less, and duplicate
        // names become problems. Empty file still offers builtins.
        let dir = test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("profiles.toml");
        std::fs::write(
            &path,
            "[[profile]]\nname = \"pi-fast\"\nprogram = \"pi --model fast\"\ndir = \"/tmp\"\n\n\
             [[profile]]\nname = \"\"\nprogram = \"pi\"\n\n\
             [[profile]]\nname = \"noprogram\"\n\n\
             [[profile]]\nname = \"pi-fast\"\nprogram = \"pi\"\n",
        )
        .unwrap();
        let (cfg, problems) = load_config_report(Some(path.clone()));
        assert_eq!(cfg.profiles.len(), 1);
        assert_eq!(cfg.profiles[0].name, "pi-fast");
        assert_eq!(cfg.profiles[0].program, "pi --model fast");
        assert_eq!(cfg.profiles[0].dir.as_deref(), Some("/tmp"));
        assert_eq!(problems.len(), 3);
        assert!(problems.iter().any(|p| p.contains("no name")));
        assert!(problems.iter().any(|p| p.contains("no program")));
        assert!(problems.iter().any(|p| p.contains("duplicate")));
        std::fs::remove_file(path).ok();
        // No [[profile]] at all: builtin agents keep spawn usable.
        let empty = dir.join("empty.toml");
        std::fs::write(&empty, "# nothing\n").unwrap();
        let (cfg, _) = load_config_report(Some(empty.clone()));
        assert!(cfg.profiles.len() >= 3);
        assert!(cfg.profiles.iter().any(|p| p.name == "claude"));
        std::fs::remove_file(empty).ok();
    }

    #[test]
    fn spawn_keys_have_defaults() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.key("spawn"), "w");
        assert_eq!(cfg.key("restart"), "e");
        assert_eq!(cfg.key("pause"), "z");
    }

    #[test]
    fn mru_tabs_defaults_off_and_parses() {
        assert!(!AppConfig::default().mru_tabs);
        let dir = test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mru.toml");
        std::fs::write(&path, "[ui]\nmru_tabs = true\n").unwrap();
        let (cfg, problems) = load_config_report(Some(path.clone()));
        assert!(problems.is_empty(), "{problems:?}");
        assert!(cfg.mru_tabs);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn bad_builtin_key_value_warns_and_reverts() {
        let dir = test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("badkeys.toml");
        std::fs::write(&path, "[keys]\nattach = \"f99\"\nquit = \"\"\n").unwrap();
        let (cfg, problems) = load_config_report(Some(path.clone()));
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(
            problems
                .iter()
                .any(|p| p.contains("attach") && p.contains("f99"))
        );
        assert!(problems.iter().any(|p| p.contains("quit")));
        // Reverted to defaults instead of deadening the actions.
        assert_eq!(cfg.key("attach"), "enter");
        assert_eq!(cfg.key("quit"), "q");
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn builtin_key_collision_warns() {
        let dir = test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("keyclash.toml");
        // kill defaults to x; rebinding quit onto it shadows one of them.
        std::fs::write(&path, "[keys]\nquit = \"x\"\n").unwrap();
        let (_, problems) = load_config_report(Some(path.clone()));
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].contains("quit") && problems[0].contains("kill"),
            "{problems:?}"
        );
        // Case-insensitive: X steals x at runtime too.
        std::fs::write(&path, "[keys]\nquit = \"X\"\n").unwrap();
        let (_, problems) = load_config_report(Some(path.clone()));
        assert_eq!(problems.len(), 1, "{problems:?}");
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn unknown_theme_falls_back_to_dark() {
        let cfg = AppConfig {
            theme: "textual-dark".to_string(),
            ..AppConfig::default()
        };
        assert_eq!(cfg.palette(), "dark");
        let cfg = AppConfig {
            theme: "light".to_string(),
            ..AppConfig::default()
        };
        assert_eq!(cfg.palette(), "light");
    }

    #[test]
    fn theme_and_density_match_case_insensitively() {
        let dir = test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("case.toml");
        std::fs::write(&path, "[ui]\ntheme = \"Dark\"\ndensity = \"Compact\"\n").unwrap();
        let (cfg, problems) = load_config_report(Some(path.clone()));
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(cfg.palette(), "dark");
        assert!(cfg.compact());
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn persist_touches_only_ui_keys() {
        let dir = test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("persist.toml");
        std::fs::write(
            &path,
            "# terhub config: keys are single-key bindings, theme is dark or light.\n# my fancy config\n[ui]\ntheme = \"dark\"\n# density = \"compact\"\n\n[keys]\nnew = \"c\"\n",
        )
        .unwrap();
        persist_ui_settings_to(&path, "nord", "compact", false, false, false).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# my fancy config"), "{text}");
        assert!(
            text.contains("# theme: dark, light, dracula, nord, gruvbox, solarized, tokyonight, catppuccin, onedark, monokai."),
            "{text}"
        );
        assert!(!text.contains("theme is dark or light"), "{text}");
        assert!(text.contains("theme = \"nord\""), "{text}");
        assert!(text.contains("# density = \"compact\""), "{text}");
        assert!(text.contains("\ndensity = \"compact\"\n"), "{text}");
        assert!(text.contains("new = \"c\""), "{text}");
        assert!(text.contains("[notifications]\nenabled = false"), "{text}");
        // And it loads back.
        let (cfg, problems) = load_config_report(Some(path.clone()));
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(cfg.theme, "nord");
        assert!(cfg.compact());
        assert!(!cfg.notifications_enabled);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn report_names_syntax_and_unknowns() {
        let dir = test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let bad = dir.join("bad.toml");
        std::fs::write(&bad, "[keys\nnew = ").unwrap();
        let (_, problems) = load_config_report(Some(bad.clone()));
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("bad.toml"), "{problems:?}");
        let odd = dir.join("odd.toml");
        std::fs::write(
            &odd,
            "[ui]\ntheme = \"neon\"\n[keys]\nfrobnicate = \"z\"\nnew = \"c\"\n",
        )
        .unwrap();
        let (cfg, problems) = load_config_report(Some(odd.clone()));
        assert_eq!(cfg.key("new"), "c");
        assert_eq!(cfg.palette(), "dark");
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("frobnicate")));
        assert!(problems.iter().any(|p| p.contains("neon")));
        std::fs::remove_file(bad).ok();
        std::fs::remove_file(odd).ok();
    }
}
