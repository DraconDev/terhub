//! Config: theme + keybindings as TOML. Not vim, customizable.
use anyhow::{Context, Result};
use crossterm::event::KeyCode;
use serde::Deserialize;
use std::collections::HashMap;
use std::env;
use std::path::PathBuf;

pub const DEFAULT_THEME: &str = "dark";
pub const DEFAULT_DENSITY: &str = "comfortable";

/// Built-in theme names, in live-cycle order.
pub const THEMES: &[&str] = &["dark", "light", "dracula", "nord", "gruvbox", "solarized"];

pub fn is_known_theme(name: &str) -> bool {
    THEMES.contains(&name)
}

/// Table density: `comfortable` shows the summary column with airy spacing,
/// `compact` drops it and tightens columns.
pub const DENSITIES: &[&str] = &["comfortable", "compact"];

pub fn is_known_density(name: &str) -> bool {
    DENSITIES.contains(&name)
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
}

impl Default for AppConfig {
    fn default() -> Self {
        AppConfig {
            keys: default_keys(),
            theme: DEFAULT_THEME.to_string(),
            density: DEFAULT_DENSITY.to_string(),
            highlight_attached: true,
            notifications_enabled: true,
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
    /// Python default "textual-dark").
    pub fn palette(&self) -> &str {
        if is_known_theme(&self.theme) {
            &self.theme
        } else {
            "dark"
        }
    }

    /// Effective density; unknown values fall back to comfortable.
    pub fn density_name(&self) -> &str {
        if is_known_density(&self.density) {
            &self.density
        } else {
            "comfortable"
        }
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
        f if f.len() > 1 && f.starts_with('f') => f[1..]
            .parse::<u8>()
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

#[derive(Debug, Deserialize, Default)]
struct FileConfig {
    #[serde(default)]
    ui: UiSection,
    #[serde(default)]
    keys: HashMap<String, String>,
    #[serde(default)]
    notifications: NotificationsSection,
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
}

impl Default for UiSection {
    fn default() -> Self {
        UiSection {
            theme: default_theme(),
            density: default_density(),
            highlight_attached: true,
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

/// User screen-pattern manifests live next to the config file.
pub fn manifests_dir() -> PathBuf {
    config_path()
        .parent()
        .map(|p| p.join("manifests"))
        .unwrap_or_else(|| PathBuf::from("manifests"))
}

pub fn config_path() -> PathBuf {
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
) -> anyhow::Result<()> {
    persist_ui_settings_to(&config_path(), theme, density, notifications, highlight)
}

fn persist_ui_settings_to(
    path: &PathBuf,
    theme: &str,
    density: &str,
    notifications: bool,
    highlight: bool,
) -> anyhow::Result<()> {
    let text = std::fs::read_to_string(path).unwrap_or_else(|_| default_config_text());
    let mut out: Vec<String> = Vec::new();
    let mut section = String::new();
    let mut seen = [false; 4]; // theme, density, highlight, enabled
    let (mut ui_at, mut notif_at): (Option<usize>, Option<usize>) = (None, None);
    for line in text.lines() {
        // Refresh our own stale generated header while we're here.
        if line == "# terhub config: keys are single-key bindings, theme is dark or light." {
            out.push("# terhub config: keys are single-key bindings.".to_string());
            out.push(
                "# theme: dark, light, dracula, nord, gruvbox, solarized. density: comfortable, compact."
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
            let mut msg = e.to_string().replace('\n', " ");
            msg.truncate(160);
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
    cfg.notifications_enabled = file.notifications.enabled;
    (cfg, problems)
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
    out
}

pub fn ensure_config() -> Result<PathBuf> {
    let path = config_path();
    if !path.exists() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create config dir {}", parent.display()))?;
        }
        let text = "# terhub config: keys are single-key bindings.\n# theme: dark, light, dracula, nord, gruvbox, solarized. density: comfortable, compact.\n"
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
    fn letter_match_is_case_insensitive() {
        assert!(key_matches(&KeySpec::Char('n'), KeyCode::Char('N')));
        assert!(key_matches(&KeySpec::Char('q'), KeyCode::Char('q')));
        assert!(!key_matches(&KeySpec::Char('n'), KeyCode::Char('x')));
        assert!(!key_matches(&KeySpec::None, KeyCode::Char('n')));
    }

    #[test]
    fn partial_config_merges_over_defaults() {
        let dir = std::env::temp_dir().join(format!("terhub-test-{}", std::process::id()));
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
    fn persist_touches_only_ui_keys() {
        let dir = std::env::temp_dir().join(format!("terhub-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("persist.toml");
        std::fs::write(
            &path,
            "# terhub config: keys are single-key bindings, theme is dark or light.\n# my fancy config\n[ui]\ntheme = \"dark\"\n# density = \"compact\"\n\n[keys]\nnew = \"c\"\n",
        )
        .unwrap();
        persist_ui_settings_to(&path, "nord", "compact", false, false).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# my fancy config"), "{text}");
        assert!(
            text.contains("# theme: dark, light, dracula, nord, gruvbox, solarized."),
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
        let dir = std::env::temp_dir().join(format!("terhub-test-{}", std::process::id()));
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
