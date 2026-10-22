//! Config: theme + keybindings as TOML. Not vim, customizable.
use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::style::{Color, Modifier, Style};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
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
    "rose-pine",
    "everforest",
    "kanagawa",
    "ayu",
    "nightfly",
    "palenight",
    "github",
    "github-light",
    "onehalf",
    "onehalf-light",
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
        // Optional hard-clear for the filter query. Unbound by default
        // (empty string); Esc in the filter prompt always restores the
        // pre-edit query, so this only matters when a user wants a
        // dedicated "wipe the filter" key. Bind e.g. to "ctrl+backspace".
        ("filter_clear", ""),
        // Cycle through saved (named) filters; wraps past the last one
        // back to the first. Empty list is a no-op with a status hint.
        ("saved_filter", "ctrl+f"),
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

/// A named saved filter: a label the user picks from the settings
/// page (or cycles with the `saved_filter` key) plus the include query
/// it applies to the table. Both are single-line; the query is stored
/// exactly as typed (multi-token, space separated).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedFilter {
    pub name: String,
    pub query: String,
}

impl SavedFilter {
    /// True when a row carrying this label would apply the query
    /// unchanged — what lets the table header name it as active.
    pub fn matches_query(&self, query: &str) -> bool {
        self.query == query
    }
}

/// Escape a string for a TOML basic-string literal (the form used by
/// the surgical `[ui]` writer). Names and queries are single-line by
/// construction, but they can carry quotes, backslashes and control
/// characters, and TOML rejects a raw backslash or an unterminated
/// string.
pub fn toml_basic_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out
}
#[derive(Debug, Clone)]
pub struct AppConfig {
    pub keys: HashMap<String, String>,
    pub theme: String,
    pub density: String,
    /// Standing table filter, persisted to `[ui] filter`. Stored with
    /// a leading `!` so the on-disk form reads as "exclude" at a
    /// glance (e.g. `filter = "!zsh"`). `app::App::new` strips a
    /// single leading `!` so `app.exclude` is bare tokens under the
    /// multi-token model; the on-disk format is the source of
    /// truth.
    pub filter: String,
    /// Named saved filters, persisted to `[ui] saved_filters` as an
    /// array of inline tables
    /// (`saved_filters = [{name = "work", query = "npm cargo"}]`).
    /// Additive and defaulted: a config without the key loads empty,
    /// and the standing `filter` line keeps its meaning untouched.
    pub saved_filters: Vec<SavedFilter>,
    /// Table sort column name (`Idle` default; see `SortKey::parse_name`).
    pub sort: String,
    /// Sort descending (`true` default = newest/longest first).
    pub sort_desc: bool,
    /// Hide quiet shells on startup (`h` toggles live).
    pub hide_shells: bool,
    pub highlight_attached: bool,
    pub notifications_enabled: bool,
    /// Blur the identity-revealing table columns (Session, Window, Cmd,
    /// Dir, Agent, Handle, Summary) so a recorded screen or shared
    /// screenshot hides what the user is working on while every state
    /// and count column stays readable. Default off: it is a recording
    /// aid, not an everyday view. Persisted to `[ui] blur_cols`.
    pub blur_cols: bool,
    /// Daemon tunables (restart the daemon to apply): poll cadence, stuck
    /// threshold (`0` disables stuck detection), evidence staleness.
    pub poll_secs: u64,
    pub stuck_secs: u64,
    pub stale_secs: u64,
    /// Frozen-screen → Idle threshold (`0` disables). A live agent whose
    /// screen has not changed this long finished; without it, finished
    /// agents read `working` forever.
    pub quiet_idle_secs: u64,
    /// Untouched-waiting → Idle threshold (`0` disables). Longer than
    /// `quiet_idle_secs` on purpose: a fresh question must keep flagging
    /// even when you step away briefly.
    pub waiting_idle_secs: u64,
    /// Stale-completion → Idle threshold (`0` disables). A finished agent
    /// that has stopped producing output is waiting for its next prompt,
    /// so it reads idle once its completion banner is old news.
    pub done_idle_secs: u64,
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
    /// Show a 6-wide per-pane activity sparkline column (default off;
    /// wide terminals opt in). Per-row output-rate history: the rightmost
    /// cell is the freshest sample, bars rise with bytes/tick. In-memory
    /// only — restarts reset. Aggregates 10 samples into one bar from a
    /// 60-sample ring, so the visible window is ~60 ticks at 3s each,
    /// or ~3 minutes of activity at a glance.
    pub activity_sparkline: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        AppConfig {
            keys: default_keys(),
            theme: DEFAULT_THEME.to_string(),
            density: DEFAULT_DENSITY.to_string(),
            filter: String::new(),
            saved_filters: Vec::new(),
            sort: "Idle".to_string(),
            sort_desc: true,
            hide_shells: false,
            highlight_attached: true,
            notifications_enabled: true,
            poll_secs: crate::daemon::POLL_SECS,
            stuck_secs: crate::daemon::STUCK_SECS,
            stale_secs: crate::daemon::HOOK_STALE_SECS,
            quiet_idle_secs: crate::daemon::QUIET_IDLE_SECS,
            waiting_idle_secs: crate::daemon::WAITING_IDLE_SECS,
            done_idle_secs: crate::daemon::DONE_IDLE_SECS,
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
            activity_sparkline: false,
            blur_cols: false,
        }
    }
}

impl AppConfig {
    pub fn key(&self, action: &str) -> &str {
        self.keys.get(action).map(String::as_str).unwrap_or("")
    }

    /// Human-readable label for a binding: the raw config value when it
    /// has no modifiers ("n", "f5", "enter"), the canonical "ctrl+s"
    /// spelling when it does. Intentionally unbound actions (an empty
    /// value) read as "—" so help lines stay readable.
    pub fn key_label(&self, action: &str) -> String {
        match self.spec(action).label() {
            s if s.is_empty() => "—".to_string(),
            s => s,
        }
    }

    pub fn spec(&self, action: &str) -> KeyBinding {
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
/// Modifier-aware key spec: the key itself plus the modifiers that must be
/// held for it to fire.
pub enum KeySpec {
    /// A character key ("n", "?", "g"...); the char is the key.
    Char(char),
    /// A named/special key (enter, esc, f5...); holds the code.
    Code(KeyCode),
    /// No binding for this action.
    None,
}

impl KeySpec {
    /// The modifier set this binding was declared with. `None` values and
    /// every pre-modifier-era default parse to `KeyModifiers::NONE`, which
    /// means "fires with any modifiers" (back-compat): a bare binding
    /// still works under a stray ctrl/shift, exactly as before.
    pub fn mods(&self) -> KeyModifiers {
        KeyModifiers::NONE
    }
}

/// One parsed keybinding: a modifier set plus a key spec (None = unbound).
/// Plain config values ("n", "enter", "f5") parse to a spec with
/// `KeyModifiers::NONE` — which keeps the historic "fires under any
/// modifiers" behaviour so bare-key binding stays reachable. Modifier
/// values ("ctrl+s", "alt+x", "shift+g") carry their modifiers and fire
/// only on that exact combo, so they never shadow the bare key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyBinding {
    pub mods: KeyModifiers,
    pub spec: KeySpec,
}

impl KeyBinding {
    /// Display form for help text, footers and the config template.
    /// No modifiers -> plain key name ("n", "enter", "f5", "g");
    /// with modifiers -> "ctrl+s", "shift+g", "ctrl+shift+enter".
    /// Unbound actions return the raw config value when it is non-empty
    /// (a failed parse still shows what the user asked for, matching how
    /// the pre-modifier help lines read), and the empty string when the
    /// action is intentionally unbound (filter_clear default).
    pub fn label(&self) -> String {
        let key = match &self.spec {
            KeySpec::None => {
                // Intentionally unbound action: the empty label is
                // rendered as "—" by key_label at display call sites,
                // but label() itself stays empty so a bare binding
                // check (label empty AND spec None) still works.
                String::new()
            }
            KeySpec::Char(c) => c.to_string(),
            KeySpec::Code(k) => match k {
                KeyCode::Char(c) => c.to_string(),
                KeyCode::Enter => "enter".into(),
                KeyCode::Esc => "esc".into(),
                KeyCode::Tab => "tab".into(),
                KeyCode::BackTab => "backtab".into(),
                KeyCode::Backspace => "backspace".into(),
                KeyCode::Delete => "delete".into(),
                KeyCode::Insert => "insert".into(),
                KeyCode::Up => "up".into(),
                KeyCode::Down => "down".into(),
                KeyCode::Left => "left".into(),
                KeyCode::Right => "right".into(),
                KeyCode::Home => "home".into(),
                KeyCode::End => "end".into(),
                KeyCode::PageUp => "pageup".into(),
                KeyCode::PageDown => "pagedown".into(),
                KeyCode::F(n) => format!("f{n}"),
                other => format!("{other:?}"),
            },
        };
        if self.mods == KeyModifiers::NONE {
            return key;
        }
        let mut parts: Vec<String> = Vec::new();
        if self.mods.intersects(KeyModifiers::CONTROL) {
            parts.push("ctrl".into());
        }
        if self.mods.intersects(KeyModifiers::ALT) {
            parts.push("alt".into());
        }
        if self.mods.intersects(KeyModifiers::SHIFT) {
            parts.push("shift".into());
        }
        if self.mods.intersects(KeyModifiers::SUPER) {
            parts.push("super".into());
        }
        if parts.is_empty() {
            key
        } else {
            parts.push(key);
            parts.join("+")
        }
    }

    /// Fire test for a physical key event. Bare bindings (the whole
    /// pre-modifier era, e.g. `s`) fire only on an unmodified press:
    /// a ctrl+s press has no bare binding to match, so it reaches the
    /// swallow guard in [`crate::app::App::on_key`] unless some binding
    /// explicitly asks for ctrl+s. Modifier-qualified bindings require
    /// an exact modifier match: "ctrl+s" fires only on ctrl+s, never on
    /// bare s or ctrl+shift+s.
    pub fn matches(&self, code: &KeyCode, mods: KeyModifiers) -> bool {
        if self.mods != KeyModifiers::NONE && mods != self.mods {
            return false;
        }
        // A bare binding never wants modifier keys: the press's modifier
        // set must be empty (the dispatch guard guarantees this is the
        // only path bare bindings are reached through anyway, but the
        // check keeps this method total and unit-testable). One
        // exception: a bare *uppercase* letter binding (the shipped
        // template's `key = "G"`) is physically pressed as
        // Char('G') + SHIFT — shift is how the letter is typed, not a
        // chord modifier. Only the exact shifted char qualifies, so a
        // bare lowercase binding stays unshifted-only.
        if self.mods == KeyModifiers::NONE && mods != KeyModifiers::NONE {
            let shifted_letter = mods == KeyModifiers::SHIFT
                && matches!(&self.spec, KeySpec::Char(c) if c.is_ascii_uppercase());
            if !shifted_letter {
                return false;
            }
        }
        match (&self.spec, code) {
            (KeySpec::None, _) => false,
            (KeySpec::Char(a), KeyCode::Char(b)) => {
                if a.is_ascii_alphabetic() && b.is_ascii_alphabetic() {
                    a.eq_ignore_ascii_case(b)
                } else {
                    a == b
                }
            }
            (KeySpec::Code(a), b) => a == b,
            _ => false,
        }
    }

    /// Collision test for config validation: two bindings collide when a
    /// single physical press can fire both. Different modifier sets can
    /// never fire on the same press, so only identical-modifier pairs
    /// (including the NONE era of bare keys) need to be compared — exactly
    /// the collision semantics bare keys always had.
    pub fn conflicts(&self, other: &KeyBinding) -> bool {
        if self.mods != other.mods || self.spec == KeySpec::None {
            return false;
        }
        match (&self.spec, &other.spec) {
            (KeySpec::Char(x), KeySpec::Char(y)) => {
                if x.is_ascii_alphabetic() && y.is_ascii_alphabetic() {
                    x.eq_ignore_ascii_case(y)
                } else {
                    x == y
                }
            }
            (KeySpec::Code(x), KeySpec::Code(y)) => x == y,
            _ => false,
        }
    }
}

/// Parse a config key name: single chars ("n", "?"), names ("enter", "esc",
/// "space", "tab", "up"...), function keys ("f5"), or the legacy
/// "question_mark" alias.
pub fn parse_key(s: &str) -> KeyBinding {
    let s = s.trim();
    let (mods, rest) = split_modifiers(s);
    let rest = rest.trim();
    if rest.is_empty() {
        return KeyBinding {
            mods,
            spec: KeySpec::None,
        };
    }
    if rest.chars().count() == 1 {
        return KeyBinding {
            mods,
            spec: KeySpec::Char(rest.chars().next().unwrap_or('\0')),
        };
    }
    let spec = match rest.to_lowercase().as_str() {
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
    };
    KeyBinding { mods, spec }
}

/// Split a config key value into (modifiers, key). Accepted prefixes are
/// `ctrl`, `alt`, `shift`, `super` (and the spellings `control`,
/// `option`, `meta`, `cmd`), comma- or whitespace-separated, e.g.
/// "ctrl+s", "ctrl shift enter", "alt+f5". A value with no recognised
/// prefix keeps today's semantics: KeyModifiers::NONE = fires with any
/// modifiers. Unknown prefixes are not modifiers: "foo+n" falls through
/// to the whole-string key lookup and parses to None, exactly like an
/// unknown key name always did.
fn split_modifiers(s: &str) -> (KeyModifiers, &str) {
    let known: &[&str] = &[
        "ctrl", "control", "alt", "option", "shift", "super", "meta", "cmd",
    ];
    if !s.contains('+') {
        // No '+' separator: a bare value, no modifiers. (Whitespace-only
        // combinations like "ctrl s" are not modifier syntax; they parse
        // to a bad key, as always.)
        return (KeyModifiers::NONE, s);
    }
    let mut mods = KeyModifiers::NONE;
    let mut rest: Option<&str> = None;
    for part in s.split('+') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let l = part.to_lowercase();
        if known.contains(&l.as_str()) {
            match l.as_str() {
                "ctrl" | "control" => mods |= KeyModifiers::CONTROL,
                "alt" | "option" => mods |= KeyModifiers::ALT,
                "shift" => mods |= KeyModifiers::SHIFT,
                "super" | "meta" | "cmd" => mods |= KeyModifiers::SUPER,
                _ => unreachable!(),
            }
        } else {
            // The key part: must be last, and there may be only one of
            // them. Multiple keys ("ctrl+s+t") are not a binding — report
            // the whole string back so the caller's normal "unknown key"
            // path handles it with its usual diagnostics.
            if rest.is_some() {
                return (KeyModifiers::NONE, s);
            }
            rest = Some(part);
        }
    }
    match rest {
        Some(r) => (mods, r),
        None => (KeyModifiers::NONE, s),
    }
}

/// Modifier-aware fire test: `true` when `binding` fires on a physical
/// press of `code` held with `mods`.
///
/// Bare bindings (`KeyBinding.mods == NONE`, every pre-modifier-era
/// default) keep today's semantics: the modifiers held are irrelevant
/// (stray ctrl while firing a bare `s` still works, exactly as before;
/// only crossterm-level ctrl+C is reserved). Modifier-qualified
/// bindings require an *exact* modifier match, so "ctrl+s" fires only on
/// ctrl+s, never on a bare s or on ctrl+shift+s.
#[must_use]
pub fn key_matches(binding: &KeyBinding, code: &KeyCode, mods: KeyModifiers) -> bool {
    binding.matches(code, mods)
}

/// Modifier-aware collision test: two bindings collide when a single
/// physical press can fire both. Different modifier sets can never fire
/// on the same press, so only identical-modifier pairs are compared
/// (including the NONE era of bare keys, which is exactly the collision
/// semantics bare keys always had).
#[must_use]
pub fn spec_conflicts(a: &KeyBinding, b: &KeyBinding) -> bool {
    a.conflicts(b)
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

/// Unknown fields are collected, not fatal: a stale or misplaced key
/// (a removed binding, a section renamed) must degrade that one field to
/// its default and be reported — never silently discard the whole file.
/// The report names the field and its section so the fix is one edit.
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
    #[serde(flatten)]
    unknown: BTreeMap<String, toml::Value>,
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
    #[serde(flatten)]
    unknown: BTreeMap<String, toml::Value>,
}

/// Validated custom command. `{pane} {session} {window} {dir} {cmd}
/// {agent} {state}` expand against the selected row.
#[derive(Debug, Clone)]
pub struct CustomCommand {
    pub name: String,
    pub key: String,
    pub spec: KeyBinding,
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
    #[serde(flatten)]
    unknown: BTreeMap<String, toml::Value>,
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
    #[serde(flatten)]
    unknown: BTreeMap<String, toml::Value>,
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
    #[serde(flatten)]
    unknown: BTreeMap<String, toml::Value>,
}

impl Default for NotificationsSection {
    fn default() -> Self {
        NotificationsSection {
            enabled: true,
            unknown: BTreeMap::new(),
        }
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
    /// Standing table filter (`filter: ""` shows everything). The
    /// on-disk form keeps a leading `!` so the file reads as
    /// "exclude" (e.g. `filter = "!zsh"`). `app::App::new` strips
    /// one leading `!` so the in-memory field is bare tokens.
    #[serde(default)]
    filter: String,
    /// Named saved filters (`[ui] saved_filters`). Additive and
    /// defaulted, so a config written before this key existed still
    /// loads (empty list) and keeps its `filter` line untouched.
    #[serde(default)]
    saved_filters: Vec<SavedFilter>,
    /// Table sort column name (`Idle` default; unknown names warn and
    /// fall back).
    #[serde(default = "default_sort")]
    sort: String,
    /// Sort descending (default true = newest/longest first).
    #[serde(default = "default_sort_desc")]
    sort_desc: bool,
    /// Hide quiet shells on startup (`h` toggles live).
    #[serde(default)]
    hide_shells: bool,
    /// Show a 6-wide per-pane activity sparkline column (default off;
    /// wide terminals opt in). Rightmost cell is the freshest sample,
    /// bars rise with bytes/tick; restarts reset the in-memory ring.
    #[serde(default)]
    activity_sparkline: bool,
    /// Blur the identity-revealing table columns (Session, Window, Cmd,
    /// Dir, Agent, Handle, Summary) for recordings and shared shots.
    /// Default off. See `AppConfig::blur_cols`.
    #[serde(default)]
    blur_cols: bool,
    #[serde(flatten)]
    unknown: BTreeMap<String, toml::Value>,
}

impl Default for UiSection {
    fn default() -> Self {
        UiSection {
            theme: default_theme(),
            density: default_density(),
            highlight_attached: true,
            mru_tabs: false,
            filter: String::new(),
            saved_filters: Vec::new(),
            sort: default_sort(),
            sort_desc: default_sort_desc(),
            hide_shells: false,
            activity_sparkline: false,
            blur_cols: false,
            unknown: BTreeMap::new(),
        }
    }
}

fn default_sort() -> String {
    "Idle".to_string()
}

fn default_sort_desc() -> bool {
    true
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
    #[serde(default = "default_waiting_idle_secs")]
    waiting_idle_secs: u64,
    #[serde(default = "default_done_idle_secs")]
    done_idle_secs: u64,
    #[serde(flatten)]
    unknown: BTreeMap<String, toml::Value>,
}

impl Default for DaemonSection {
    fn default() -> Self {
        DaemonSection {
            poll_secs: default_poll_secs(),
            stuck_secs: default_stuck_secs(),
            stale_secs: default_stale_secs(),
            quiet_idle_secs: default_quiet_idle_secs(),
            waiting_idle_secs: default_waiting_idle_secs(),
            done_idle_secs: default_done_idle_secs(),
            unknown: BTreeMap::new(),
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

fn default_waiting_idle_secs() -> u64 {
    crate::daemon::WAITING_IDLE_SECS
}

fn default_done_idle_secs() -> u64 {
    crate::daemon::DONE_IDLE_SECS
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
    #[serde(flatten)]
    unknown: BTreeMap<String, toml::Value>,
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
            unknown: BTreeMap::new(),
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

/// Per-thread override for [`config_path`].
///
/// `TERHUB_CONFIG` is process-global: a test that points it at a
/// scratch file changes what *every* thread in the process resolves,
/// so two tests that each redirect it race, and one test's redirect
/// can hijack another test's read. A per-thread override gives each
/// caller its own config path with no shared state at all.
///
/// The env var stays the fallback, and the power-user escape hatch: a
/// thread with no override — a daemon thread, a spawned child — still
/// resolves the ambient config.
thread_local! {
    static CONFIG_PATH_OVERRIDE: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Point *this thread's* config resolution at `path`, returning the
/// previous override so the caller can restore it.
///
/// `None` clears the override and hands config resolution back to the
/// ambient (`TERHUB_CONFIG` / XDG) resolution.
pub(crate) fn override_config_path(path: Option<PathBuf>) -> Option<PathBuf> {
    CONFIG_PATH_OVERRIDE.with(|slot| slot.borrow_mut().replace(path))
}

pub fn config_path() -> PathBuf {
    // Per-thread override first: it is caller-scoped state that must
    // not leak across threads (see `override_config_path`).
    if let Some(p) = CONFIG_PATH_OVERRIDE.with(|slot| slot.borrow().clone()) {
        return p;
    }
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

/// The `[ui]`/`[notifications]` values one surgical config write
/// carries. Bundled so the persist functions don't grow a parameter
/// per remembered view setting. `None` means "leave whatever the file
/// says" — the write skips the line entirely. That is what keeps one
/// terhub instance from reverting another's saved view: an instance
/// only persists the keys it actually changed, so a stale instance
/// toggling a theme can no longer rewrite a newer instance's filter.
#[derive(Default)]
pub struct UiPersist<'a> {
    pub theme: Option<&'a str>,
    pub density: Option<&'a str>,
    pub notifications: Option<bool>,
    pub highlight: Option<bool>,
    pub mru_tabs: Option<bool>,
    pub filter: Option<&'a str>,
    /// Named saved filters as a TOML array-of-inline-tables line
    /// (`saved_filters = [{name = "work", query = "npm cargo"}]`).
    /// `None` leaves whatever the file says in place.
    pub saved_filters: Option<&'a [SavedFilter]>,
    pub sort: Option<&'a str>,
    pub sort_desc: Option<bool>,
    pub hide_shells: Option<bool>,
    /// `[ui] blur_cols` — record/share mode. `None` leaves the file's
    /// value alone; a `Some(bool)` is a deliberate flip of the toggle.
    pub blur_cols: Option<bool>,
}

/// Which view keys one instance has changed since its last successful
/// write. Untouched keys stay untouchable: another instance or a hand
/// edit owns them.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ViewChanges {
    pub theme: bool,
    pub density: bool,
    pub notifications: bool,
    pub highlight: bool,
    pub mru_tabs: bool,
    pub filter: bool,
    pub saved_filters: bool,
    pub sort: bool,
    pub sort_desc: bool,
    pub hide_shells: bool,
    pub blur_cols: bool,
}

impl ViewChanges {
    /// True when nothing was touched: the caller can skip the write.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

impl AppConfig {
    /// Snapshot every persistable view setting (full sync: a fresh
    /// config, or a caller that owns the whole `[ui]` section).
    pub fn ui_persist(&self) -> UiPersist<'_> {
        UiPersist {
            theme: Some(&self.theme),
            density: Some(&self.density),
            notifications: Some(self.notifications_enabled),
            highlight: Some(self.highlight_attached),
            mru_tabs: Some(self.mru_tabs),
            filter: Some(&self.filter),
            saved_filters: Some(&self.saved_filters),
            sort: Some(&self.sort),
            sort_desc: Some(self.sort_desc),
            hide_shells: Some(self.hide_shells),
            blur_cols: Some(self.blur_cols),
        }
    }

    /// Snapshot only the keys `c` marks as changed. Keys left false
    /// come back `None`, and the surgical writer skips them — so the
    /// file keeps whatever another instance (or the user's hand edit)
    /// put there.
    pub fn ui_persist_changes(&self, c: &ViewChanges) -> UiPersist<'_> {
        UiPersist {
            theme: c.theme.then_some(&*self.theme),
            density: c.density.then_some(&*self.density),
            notifications: c.notifications.then_some(self.notifications_enabled),
            highlight: c.highlight.then_some(self.highlight_attached),
            mru_tabs: c.mru_tabs.then_some(self.mru_tabs),
            filter: c.filter.then_some(&*self.filter),
            saved_filters: c.saved_filters.then_some(&*self.saved_filters),
            sort: c.sort.then_some(&*self.sort),
            sort_desc: c.sort_desc.then_some(self.sort_desc),
            hide_shells: c.hide_shells.then_some(self.hide_shells),
            blur_cols: c.blur_cols.then_some(self.blur_cols),
        }
    }
}

/// Serialize saved filters as a TOML array of inline tables:
/// `{name = "work", query = "npm cargo"}, …`. An empty list encodes
/// as `[]`, which the loader reads back as no saved filters — the
/// line is the single source of truth, so a delete of the last filter
/// clears the key in the file instead of leaving a stale entry.
fn encode_saved_filters(filters: &[SavedFilter]) -> String {
    let mut out = String::new();
    for (i, f) in filters.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&format!(
            "{{name = \"{}\", query = \"{}\"}}",
            toml_basic_string(&f.name),
            toml_basic_string(&f.query)
        ));
    }
    out
}

/// Persist UI settings back to the config file with surgical line edits:
/// only the known `[ui]`/`[notifications]` keys are touched, everything
/// else (comments, key bindings, manifests) is preserved byte-for-byte.
pub fn persist_ui_settings(v: &UiPersist) -> anyhow::Result<()> {
    persist_ui_settings_to(&config_path(), v)
}

/// [`persist_ui_settings`] against an explicit path. The in-crate
/// callers (and the tests) resolve `config_path()` themselves, so the
/// seam keeps the env lookup out of the write.
pub fn persist_ui_settings_at(path: &std::path::Path, v: &UiPersist) -> anyhow::Result<()> {
    persist_ui_settings_to(&path.to_path_buf(), v)
}

fn persist_ui_settings_to(path: &PathBuf, v: &UiPersist) -> anyhow::Result<()> {
    let raw = std::fs::read_to_string(path).unwrap_or_else(|_| default_config_text());
    let text = strip_bom(&raw);
    // Scan the file once for string state. A line inside a multi-line
    // string is *content*, not config: a `FOO=1` line in a user's
    // command template is not a duplicate key, and a `[section]` line
    // in one is not a section header. The writer scans lines, so it
    // has to know where the string bodies are — or it rewrites the
    // user's script behind their back.
    let mut scan = Scan::Plain;
    let mut scanned: Vec<(&str, bool)> = Vec::new();
    for line in text.lines() {
        let inside = in_multi_line(scan);
        let (next, _comment) = scan_toml_line(scan, line);
        scan = next;
        scanned.push((line, inside));
    }
    // Self-heal first. A key assigned twice in one section makes the
    // whole file unparseable (TOML: "Cannot overwrite a value"), and a
    // config that won't parse is answered with defaults — which
    // silently forgets every part of the standing view, filter
    // included. The first assignment wins; later duplicates are
    // dropped before the rewrite pass, so persisting repairs the file
    // instead of carrying the corruption forward.
    let lines: Vec<(&str, bool)> = {
        use std::collections::HashSet;
        let mut seen: HashSet<(String, String)> = HashSet::new();
        let mut section = String::new();
        scanned
            .into_iter()
            .filter(|(line, inside)| {
                // Inside a multi-line string the line is content: it
                // is never a header and never a duplicate key.
                if *inside {
                    return true;
                }
                let t = line.trim_start();
                // Comment-only lines are never headers (see the main
                // pass below): `# [ui]` must not open the ui section.
                if let Some(rest) = t.strip_prefix('[')
                    && let Some(end) = rest.find(']')
                {
                    let next = format!("[{}]", &rest[..end]);
                    if next != section {
                        section = next;
                        seen.retain(|(s, _)| s == &section);
                    }
                    return true;
                }
                match assigned_key(line) {
                    Some(k) if !section.is_empty() => {
                        seen.insert((section.clone(), k.to_lowercase()))
                    }
                    _ => true,
                }
            })
            .collect()
    };
    let mut out: Vec<String> = Vec::new();
    let mut section = String::new();
    // One slot per surgically-writable `[ui]` key. These are *named*
    // so the key↔slot mapping cannot drift: an earlier version of
    // `saved_filters` was inserted at the `sort` slot, and a patch
    // carrying both keys silently dropped one of the two lines.
    let mut seen = [false; SLOT_UI + 1];
    // The `[ui]` and `[notifications]` section positions, recorded
    // while scanning so missing keys can be inserted in place.
    let (mut ui_at, mut notif_at): (Option<usize>, Option<usize>) = (None, None);
    for (line, inside) in lines.iter().copied() {
        // Content of a multi-line string: kept byte-for-byte. It is
        // never a header, never a key and never a comment.
        if inside {
            out.push(line.to_string());
            continue;
        }
        // Refresh our own stale generated header while we're here.
        if line == "# terhub config: keys are single-key bindings, theme is dark or light." {
            out.push("# terhub config: keys are single-key bindings.".to_string());
            out.push(
                "# theme: dark, light, dracula, nord, gruvbox, solarized, tokyonight, catppuccin, onedark, monokai, rose-pine, everforest, kanagawa, ayu, nightfly, palenight, github, github-light, onehalf, onehalf-light. density: comfortable, compact."
                    .to_string(),
            );
            continue;
        }
        // A section header is `[name]` possibly followed by a `# comment`;
        // a comment-only line that happens to contain `[` (e.g.
        // `# [old section]`) must not be treated as a header — so the
        // `[` test runs on the trimmed line itself, with no `#`
        // stripping. We locate the first `[` and the first `]` after
        // it, then ignore the rest.
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix('[')
            && let Some(end) = rest.find(']')
        {
            let name = &rest[..end];
            section = format!("[{name}]");
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
            match v.theme {
                Some(t) => {
                    out.push(format!("theme = \"{t}\"{}", trailing_comment(line)));
                    seen[THEME] = true;
                }
                None => out.push(line.to_string()),
            }
        } else if section == "[ui]" && is_key_line(line, "density") {
            match v.density {
                Some(d) => {
                    out.push(format!("density = \"{d}\"{}", trailing_comment(line)));
                    seen[DENSITY] = true;
                }
                None => out.push(line.to_string()),
            }
        } else if section == "[ui]" && is_key_line(line, "highlight_attached") {
            match v.highlight {
                Some(h) => {
                    out.push(format!(
                        "highlight_attached = {h}{}",
                        trailing_comment(line)
                    ));
                    seen[HIGHLIGHT] = true;
                }
                None => out.push(line.to_string()),
            }
        } else if section == "[ui]" && is_key_line(line, "mru_tabs") {
            match v.mru_tabs {
                Some(m) => {
                    out.push(format!("mru_tabs = {m}{}", trailing_comment(line)));
                    seen[MRU_TABS] = true;
                }
                None => out.push(line.to_string()),
            }
        } else if section == "[ui]" && is_key_line(line, "filter") {
            match v.filter {
                // Quotes escape the one char TOML needs escaped; a filter
                // is plain search text, never a path, so backslash and
                // quote are the only risks.
                Some(f) => {
                    let esc = f.replace('\\', "\\\\").replace('"', "\\\"");
                    out.push(format!("filter = \"{esc}\"{}", trailing_comment(line)));
                    seen[FILTER] = true;
                }
                None => out.push(line.to_string()),
            }
        } else if section == "[ui]" && is_key_line(line, "saved_filters") {
            match v.saved_filters {
                Some(f) => {
                    out.push(format!(
                        "saved_filters = [{}]{}",
                        encode_saved_filters(f),
                        trailing_comment(line)
                    ));
                    seen[SAVED_FILTERS] = true;
                }
                None => out.push(line.to_string()),
            }
        } else if section == "[ui]" && is_key_line(line, "sort") {
            match v.sort {
                Some(s) => {
                    let esc = s.replace('\\', "\\\\").replace('"', "\\\"");
                    out.push(format!("sort = \"{esc}\"{}", trailing_comment(line)));
                    seen[SORT] = true;
                }
                None => out.push(line.to_string()),
            }
        } else if section == "[ui]" && is_key_line(line, "sort_desc") {
            match v.sort_desc {
                Some(d) => {
                    out.push(format!("sort_desc = {d}{}", trailing_comment(line)));
                    seen[SORT_DESC] = true;
                }
                None => out.push(line.to_string()),
            }
        } else if section == "[ui]" && is_key_line(line, "hide_shells") {
            match v.hide_shells {
                Some(h) => {
                    out.push(format!("hide_shells = {h}{}", trailing_comment(line)));
                    seen[HIDE_SHELLS] = true;
                }
                None => out.push(line.to_string()),
            }
        } else if section == "[ui]" && is_key_line(line, "blur_cols") {
            match v.blur_cols {
                Some(b) => {
                    out.push(format!("blur_cols = {b}{}", trailing_comment(line)));
                    seen[BLUR_COLS] = true;
                }
                None => out.push(line.to_string()),
            }
        } else if section == "[notifications]" && is_key_line(line, "enabled") {
            match v.notifications {
                Some(e) => {
                    out.push(format!("enabled = {e}{}", trailing_comment(line)));
                    seen[NOTIF_ENABLED] = true;
                }
                None => out.push(line.to_string()),
            }
        } else {
            out.push(line.to_string());
        }
    }
    // Insert whatever the file lacked directly under its section header
    // (appending at EOF would land it in the wrong section).
    let mut missing_ui = Vec::new();
    if !seen[THEME]
        && let Some(t) = v.theme
    {
        missing_ui.push(format!("theme = \"{t}\""));
    }
    if !seen[DENSITY]
        && let Some(d) = v.density
    {
        missing_ui.push(format!("density = \"{d}\""));
    }
    if !seen[HIGHLIGHT]
        && let Some(h) = v.highlight
    {
        missing_ui.push(format!("highlight_attached = {h}"));
    }
    if !seen[MRU_TABS]
        && let Some(m) = v.mru_tabs
    {
        missing_ui.push(format!("mru_tabs = {m}"));
    }
    if !seen[FILTER]
        && let Some(f) = v.filter
    {
        let esc = f.replace('\\', "\\\\").replace('"', "\\\"");
        missing_ui.push(format!("filter = \"{esc}\""));
    }
    if !seen[SAVED_FILTERS]
        && let Some(f) = v.saved_filters
    {
        missing_ui.push(format!("saved_filters = [{}]", encode_saved_filters(f)));
    }
    if !seen[SORT]
        && let Some(s) = v.sort
    {
        let esc = s.replace('\\', "\\\\").replace('"', "\\\"");
        missing_ui.push(format!("sort = \"{esc}\""));
    }
    if !seen[SORT_DESC]
        && let Some(d) = v.sort_desc
    {
        missing_ui.push(format!("sort_desc = {d}"));
    }
    if !seen[HIDE_SHELLS]
        && let Some(h) = v.hide_shells
    {
        missing_ui.push(format!("hide_shells = {h}"));
    }
    if !seen[BLUR_COLS]
        && let Some(b) = v.blur_cols
    {
        missing_ui.push(format!("blur_cols = {b}"));
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
    if !seen[NOTIF_ENABLED]
        && let Some(e) = v.notifications
    {
        match notif_at {
            Some(i) => {
                let shift = if ui_at.is_some_and(|u| u < i) {
                    ui_inserted
                } else {
                    0
                };
                out.insert(i + 1 + shift, format!("enabled = {e}"));
            }
            None => {
                out.push("[notifications]".to_string());
                out.push(format!("enabled = {e}"));
            }
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    atomic_write(path, &(out.join("\n") + "\n"))?;
    Ok(())
}

/// Index of each surgically-writable `[ui]` key inside the `seen`
/// bookkeeping array. Named because the array is positional: inserting
/// a key without shifting its neighbours silently breaks the append
/// pass (the bug that dropped a `sort` line when a `saved_filters`
/// patch carried both).
mod slot {
    pub const THEME: usize = 0;
    pub const DENSITY: usize = 1;
    pub const HIGHLIGHT: usize = 2;
    /// `[notifications] enabled` — a different section from the `[ui]`
    /// keys, hence its own index.
    pub const NOTIF_ENABLED: usize = 3;
    pub const MRU_TABS: usize = 4;
    pub const FILTER: usize = 5;
    pub const SAVED_FILTERS: usize = 6;
    pub const SORT: usize = 7;
    pub const SORT_DESC: usize = 8;
    pub const HIDE_SHELLS: usize = 9;
    pub const BLUR_COLS: usize = 10;
    /// Highest `[ui]` slot; the array spans `0..=SLOT_UI`.
    pub const SLOT_UI: usize = BLUR_COLS;
}
use slot::*;

/// Write `text` to `path` atomically: write to a unique tmp file in
/// the same directory, fsync, rename. A crash mid-write leaves the
/// original file intact (and the tmp is orphaned in the same dir;
/// [`sweep_orphan_tmps`] prunes those).
///
/// The target's mode is preserved. `File::create` would reset it to
/// 0644, which silently undoes a deliberate `chmod 600` on every
/// rewrite — and these files are rewritten on a timer (the ledger
/// hourly, the config on every save). A file that does not exist yet
/// is created 0600: everything written here holds prompts, summaries
/// or config, and nothing needs it to be world-readable.
pub(crate) fn atomic_write(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "config path has no parent directory",
        )
    })?;
    // Owner-only directory: the files written here hold the user's
    // prompts, filters and key bindings. The umask default would
    // leave a fresh directory readable by every local account.
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
    }
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let fname = path
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or("config");
    let tmp = parent.join(format!(".{fname}.tmp.{pid}.{nonce}"));
    // Keep the target's current mode; 0600 for a file that is new.
    let mode = std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o777)
        .unwrap_or(0o600);
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            // create_new, not create: a predictable name must not be
            // able to redirect the write through a symlink.
            .create_new(true)
            .mode(mode)
            .open(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    // Ignore rename failure cleanup best-effort: on rename failure
    // (e.g. cross-device), fall back to a non-atomic write so the
    // caller still gets persistence.
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        std::fs::write(path, text)?;
        // The fallback write reset the mode too: put it back.
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
        return Err(e);
    }
    Ok(())
}

/// Remove the orphaned atomic-write temporaries in `dir`; returns how
/// many were removed.
///
/// [`atomic_write`] creates `.{name}.tmp.{pid}.{nonce}` and renames it
/// over the target. A crash between the two leaves the tmp behind, and
/// on the ledger — tens of megabytes of history — that is megabytes
/// per crash, forever: nothing else ever collects them.
///
/// The owner is named in the file name, so a tmp whose pid is gone
/// cannot be in flight. A tmp whose pid is alive belongs to a live
/// writer (a concurrent save in this process, or another daemon on the
/// same directory) and is left alone — deleting it would turn a
/// harmless crash leftover into a lost write.
pub fn sweep_orphan_tmps(dir: &std::path::Path) -> usize {
    // A writer that died a moment ago may still be in its rename
    // window on another thread; give it a grace period.
    const GRACE_SECS: u64 = 60;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        // `.{name}.tmp.{pid}.{nonce}` and `{name}.tmp.{pid}.{nonce}.{tag}`
        // both carry the pid right after the last ".tmp.".
        let Some((_, tail)) = name.rsplit_once(".tmp.") else {
            continue;
        };
        let Some((pid, _)) = tail.split_once('.') else {
            continue;
        };
        let Ok(pid) = pid.parse::<i32>() else {
            continue;
        };
        if pid <= 0 || process_alive(pid) {
            continue;
        }
        // Old enough that no live writer owns it any more.
        let age = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| now.saturating_sub(d.as_secs()))
            .unwrap_or(0);
        if age < GRACE_SECS {
            continue;
        }
        if std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Whether a pid still names a live process.
///
/// `kill(pid, 0)` is the portable test: it succeeds for a process we
/// may signal, fails with EPERM for one we may not (still alive), and
/// fails with ESRCH for one that is gone.
fn process_alive(pid: i32) -> bool {
    // SAFETY: kill with signal 0 performs error checking only and
    // sends no signal.
    let rc = unsafe { libc::kill(pid, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Where a TOML line scanner is inside the file.
///
/// TOML strings come in four shapes and the writer has to tell them
/// apart: a `#` inside a quoted value is part of the value, not a
/// comment, and the lines between the delimiters of a multi-line
/// string are *content*, not config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scan {
    /// Outside any string.
    Plain,
    /// Inside a single-line basic string (`"…"`).
    Basic,
    /// Inside a single-line literal string (`'…'`).
    Literal,
    /// Inside a multi-line basic string (`"""…"""`).
    BasicMulti,
    /// Inside a multi-line literal string (`'''…'''`).
    LiteralMulti,
}

/// Whether the scanner is inside a multi-line string, where a line is
/// content rather than config.
fn in_multi_line(state: Scan) -> bool {
    matches!(state, Scan::BasicMulti | Scan::LiteralMulti)
}

/// Scan one line of TOML.
///
/// Returns the string state after the line and the byte offset of the
/// comment on it (if any). A comment starts at a `#` that is at the
/// start of the line or preceded by whitespace and is not inside a
/// string — so `filter = "npm #rust"` keeps its `#` as part of the
/// value.
fn scan_toml_line(mut state: Scan, line: &str) -> (Scan, Option<usize>) {
    let b = line.as_bytes();
    let mut i = 0usize;
    while i < b.len() {
        match state {
            Scan::Plain => {
                if b[i..].starts_with(b"\"\"\"") {
                    state = Scan::BasicMulti;
                    i += 3;
                } else if b[i..].starts_with(b"'''") {
                    state = Scan::LiteralMulti;
                    i += 3;
                } else if b[i] == b'"' {
                    state = Scan::Basic;
                    i += 1;
                } else if b[i] == b'\'' {
                    state = Scan::Literal;
                    i += 1;
                } else if b[i] == b'#' && (i == 0 || b[i - 1].is_ascii_whitespace()) {
                    return (state, Some(i));
                } else {
                    i += 1;
                }
            }
            Scan::Basic => {
                if b[i] == b'\\' {
                    // Escaped character: skip it and the next byte.
                    i += 2;
                } else if b[i] == b'"' {
                    state = Scan::Plain;
                    i += 1;
                } else {
                    i += 1;
                }
            }
            Scan::Literal => {
                if b[i] == b'\'' {
                    state = Scan::Plain;
                    i += 1;
                } else {
                    i += 1;
                }
            }
            Scan::BasicMulti => {
                if b[i..].starts_with(b"\"\"\"") {
                    state = Scan::Plain;
                    i += 3;
                } else if b[i] == b'\\' {
                    i += 2;
                } else {
                    i += 1;
                }
            }
            Scan::LiteralMulti => {
                if b[i..].starts_with(b"'''") {
                    state = Scan::Plain;
                    i += 3;
                } else {
                    i += 1;
                }
            }
        }
    }
    (state, None)
}

/// Whether `line` assigns `key`.
///
/// TOML allows a quoted key (`"theme" = "dark"`), and the writer has
/// to see those too: a quoted key it cannot match is a key it
/// believes is missing, so it inserts its own assignment beside it —
/// and the file then defines the key twice, which TOML rejects, and
/// the loader answers with defaults for the *whole* config.
fn is_key_line(line: &str, key: &str) -> bool {
    let t = line.trim_start();
    if t.starts_with('#') {
        return false;
    }
    let Some((k, _rest)) = t.split_once('=') else {
        return false;
    };
    let k = k.trim();
    if k == key {
        return true;
    }
    // A quoted key: the quotes are syntax, not part of the name.
    let bytes = k.as_bytes();
    if bytes.len() >= 2 {
        let q = bytes[0];
        if (q == b'"' || q == b'\'') && bytes[bytes.len() - 1] == q {
            return &k[1..k.len() - 1] == key;
        }
    }
    false
}

/// The assigned key on a TOML assignment line, or `None` for blank
/// lines, comments and section headers. Used by the dedupe pass.
fn assigned_key(line: &str) -> Option<&str> {
    let t = line.trim_start();
    if t.starts_with('#') {
        return None;
    }
    let (k, _rest) = t.split_once('=')?;
    let k = k.trim();
    if k.is_empty()
        || !k
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        return None;
    }
    Some(k)
}

/// Drop a leading UTF-8 byte-order mark if present. Some editors and
/// Windows tools save files with a U+FEFF BOM; TOML has no concept of
/// it, and Rust's `str::trim_start` does not strip it. The config
/// writer must drop it explicitly — otherwise the line is not seen as
/// a `[ui]` header and a fresh `[ui]` block gets appended at EOF,
/// corrupting the file into something the reader rejects.
fn strip_bom(text: &str) -> &str {
    text.strip_prefix('\u{feff}').unwrap_or(text)
}

/// Return the trailing `# ...` comment on `line` (including the leading
/// whitespace + `#`), or `""` if there is none. Used to preserve user
/// comments when rewriting a key during UI persist.
fn trailing_comment(line: &str) -> String {
    // A `#` is a comment only outside a quoted value (TOML): at the
    // start of the line, or preceded by whitespace. The scanner knows
    // about strings, so a `#` inside `filter = "npm #rust"` stays part
    // of the value instead of being spliced onto the rewritten line as
    // a comment tail.
    let (_state, at) = scan_toml_line(Scan::Plain, line);
    at.map(|i| line[i..].to_string()).unwrap_or_default()
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
    // Unknown fields in the typed sections are reported, not fatal: the
    // rest of the file still applies. Names the field and its section so
    // the fix is one line edit.
    for k in file.unknown.keys() {
        problems.push(format!("ignored unknown field '{k}' (not a known section)"));
    }
    for k in file.ui.unknown.keys() {
        problems.push(format!("ignored unknown field '{k}' in [ui]"));
    }
    for k in file.notifications.unknown.keys() {
        problems.push(format!("ignored unknown field '{k}' in [notifications]"));
    }
    for k in file.daemon.unknown.keys() {
        problems.push(format!("ignored unknown field '{k}' in [daemon]"));
    }
    for k in file.style.unknown.keys() {
        problems.push(format!("ignored unknown field '{k}' in [style]"));
    }
    for c in &file.command {
        for k in c.unknown.keys() {
            problems.push(format!("ignored unknown field '{k}' in [[command]]"));
        }
    }
    for p in &file.profile {
        for k in p.unknown.keys() {
            problems.push(format!("ignored unknown field '{k}' in [[profile]]"));
        }
    }
    for t in &file.template {
        for k in t.unknown.keys() {
            problems.push(format!("ignored unknown field '{k}' in [[template]]"));
        }
    }
    // Builtin values are validated like custom ones: a typo/f99 would
    // otherwise parse to KeySpec::None and silently deaden the action.
    // Bad values revert to their default (poll_secs precedent). An empty
    // value is legal when the default is empty: it means "intentionally
    // unbound" (e.g. filter_clear, which has no default binding).
    // Sorted actions keep problem order deterministic.
    let defaults = default_keys();
    let mut actions: Vec<String> = cfg.keys.keys().cloned().collect();
    actions.sort();
    for action in &actions {
        let value = cfg.keys[action].clone();
        let parsed = parse_key(&value);
        let default_val = defaults.get(action);
        if parsed.spec == KeySpec::None {
            match default_val {
                Some(def) if !def.is_empty() => {
                    // Typo/empty with a real default: revert and report.
                    problems.push(format!(
                        "key '{action}' has bad value '{value}' (using default '{def}')"
                    ));
                    cfg.keys.insert(action.clone(), def.clone());
                }
                // Empty default: unbound is the legal resting state;
                // nothing to revert to, nothing to report.
                _ => {}
            }
        }
    }
    // Builtin-vs-builtin collisions after rebinding: first-match dispatch
    // would silently shadow one of the pair. Modifier-qualified bindings
    // never collide with a bare key (different physical presses), so the
    // collision check compares modifier sets before keys.
    let specs: Vec<(String, KeyBinding)> = actions
        .iter()
        .filter(|a| defaults.contains_key(a.as_str()))
        .map(|a| (a.clone(), parse_key(&cfg.keys[a])))
        .collect();
    for (i, (a, sa)) in specs.iter().enumerate() {
        for (b, sb) in specs.iter().skip(i + 1) {
            if sa.spec != KeySpec::None && sa.conflicts(sb) {
                problems.push(format!("keys '{a}' and '{b}' both use '{}'", sa.label()));
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
    // `app::App::new` strips a single leading `!` from `cfg.filter`
    // so old configs (`filter = "!zsh"`) land in `app.exclude` as
    // bare tokens under the new model. The on-disk format is kept
    // here verbatim.
    cfg.filter = file.ui.filter;
    // Saved filters load verbatim; entries with an empty name or query
    // are dropped here so a hand-edited config can't leave an unnamed
    // row on the settings page. Re-encoding is the writer's job.
    cfg.saved_filters = file
        .ui
        .saved_filters
        .into_iter()
        .filter(|f| !f.name.trim().is_empty() && !f.query.trim().is_empty())
        .collect();
    cfg.sort = file.ui.sort;
    if crate::app::SortKey::parse_name(&cfg.sort).is_none() {
        problems.push(format!(
            "unknown sort '{}' (known: Pane, Session, Window, Cmd, Dir, Agent, Handle, State, Src, For, Turn, Idle, Summary)",
            cfg.sort
        ));
        cfg.sort = "Idle".to_string();
    }
    cfg.sort_desc = file.ui.sort_desc;
    cfg.hide_shells = file.ui.hide_shells;
    cfg.activity_sparkline = file.ui.activity_sparkline;
    cfg.blur_cols = file.ui.blur_cols;
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
    cfg.waiting_idle_secs = file.daemon.waiting_idle_secs;
    cfg.done_idle_secs = file.daemon.done_idle_secs;
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
    let builtin: Vec<(String, KeyBinding)> = default_keys()
        .into_keys()
        .map(|a| {
            let spec = cfg.spec(&a);
            (a, spec)
        })
        .collect();
    let mut taken: Vec<KeyBinding> = Vec::new();
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
        if spec.spec == KeySpec::None {
            problems.push(format!("{tag} has bad key '{}'", c.key));
            continue;
        }
        if let Some((action, _)) = builtin.iter().find(|(_, s)| spec.conflicts(s)) {
            problems.push(format!(
                "{tag} key '{}' collides with built-in '{action}'",
                c.key
            ));
            continue;
        }
        if taken.iter().any(|t| spec.conflicts(t)) {
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
        "[ui]\ntheme = \"dark\"\ndensity = \"comfortable\"\nhighlight_attached = true\nblur_cols = false\n\n[keys]\n",
    );
    for (k, v) in keys {
        if v.is_empty() {
            // Intentionally unbound action (filter_clear): show a
            // commented example so the user can opt in without
            // re-binding a live key.
            out.push_str(&format!(
                "# {k} = \"\"  # unbound by default; try \"ctrl+backspace\"\n"
            ));
        } else {
            out.push_str(&format!("{k} = \"{v}\"\n"));
        }
    }
    out.push_str("\n[notifications]\nenabled = true\n");
    out.push_str(&format!(
        "\n[daemon]\n# Restart the daemon to apply. stuck_secs = 0 disables stuck detection.\n# quiet_idle_secs = 0 disables frozen-screen → idle;
# waiting_idle_secs = 0 keeps an untouched waiting prompt flagged.
# done_idle_secs = 0 keeps a finished agent reading `done` forever.\npoll_secs = {}\nstuck_secs = {}\nstale_secs = {}\nquiet_idle_secs = {}\nwaiting_idle_secs = {}\ndone_idle_secs = {}\n",
        crate::daemon::POLL_SECS,
        crate::daemon::STUCK_SECS,
        crate::daemon::HOOK_STALE_SECS,
        crate::daemon::QUIET_IDLE_SECS,
        crate::daemon::WAITING_IDLE_SECS,
        crate::daemon::DONE_IDLE_SECS
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
        let text = "# terhub config: keys are single-key bindings.\n# theme: dark, light, dracula, nord, gruvbox, solarized, tokyonight, catppuccin, onedark, monokai, rose-pine, everforest, kanagawa, ayu, nightfly, palenight, github, github-light, onehalf, onehalf-light. density: comfortable, compact.\n"
            .to_string()
            + &default_config_text();
        atomic_write(&path, &text)
            .with_context(|| format!("failed to write {}", path.display()))?;
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    /// A pid that is guaranteed not to be running: spawn a child, reap
    /// it, and use the (now free) pid.
    fn dead_pid() -> i32 {
        let child = std::process::Command::new("true").spawn().expect("spawn");
        let pid = child.id() as i32;
        let mut child = child;
        let _ = child.wait();
        pid
    }

    /// Age a file so the sweep's grace period has passed.
    fn age_file(path: &std::path::Path, secs: u64) {
        let when = std::time::SystemTime::now() - std::time::Duration::from_secs(secs);
        let file = std::fs::File::options()
            .write(true)
            .open(path)
            .expect("open");
        let times = std::fs::FileTimes::new().set_modified(when);
        file.set_times(times).expect("set mtime");
    }

    #[test]
    fn sweep_removes_a_dead_owners_tmp_and_keeps_a_live_one() {
        // The ledger's atomic write leaves `.{name}.tmp.{pid}.{nonce}`
        // behind when a crash lands between the write and the rename.
        // On a 33 MB ledger that is megabytes per crash, forever.
        let dir = std::env::temp_dir().join(format!("terhub-tmpsweep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dead = dead_pid();
        let orphan = dir.join(format!(".activity.jsonl.tmp.{dead}.111"));
        let live = dir.join(format!(".activity.jsonl.tmp.{}.222", std::process::id()));
        let fresh = dir.join(format!(".activity.jsonl.tmp.{}.333", dead));
        let other = dir.join("activity.jsonl");
        for f in [&orphan, &live, &fresh, &other] {
            std::fs::write(f, b"x").unwrap();
        }
        age_file(&orphan, 7200);
        age_file(&live, 7200);
        // `fresh` keeps its current mtime: a writer that died a moment
        // ago may still be inside its rename window.

        let removed = sweep_orphan_tmps(&dir);
        assert_eq!(removed, 1, "only the dead owner's tmp is collected");
        assert!(!orphan.exists(), "the orphan is gone");
        assert!(live.exists(), "a live writer's tmp is never touched");
        assert!(fresh.exists(), "a tmp inside the grace period is kept");
        assert!(other.exists(), "the ledger itself is not a tmp");
        let _ = std::fs::remove_dir_all(&dir);
    }

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
        use crossterm::event::KeyModifiers;
        let bare = KeyModifiers::NONE;
        assert_eq!(
            parse_key("n"),
            KeyBinding {
                mods: bare,
                spec: KeySpec::Char('n')
            }
        );
        assert_eq!(
            parse_key("?"),
            KeyBinding {
                mods: bare,
                spec: KeySpec::Char('?')
            }
        );
        assert_eq!(
            parse_key("question_mark"),
            KeyBinding {
                mods: bare,
                spec: KeySpec::Char('?')
            }
        );
        assert_eq!(
            parse_key("enter"),
            KeyBinding {
                mods: bare,
                spec: KeySpec::Code(KeyCode::Enter)
            }
        );
        assert_eq!(
            parse_key("ESC"),
            KeyBinding {
                mods: bare,
                spec: KeySpec::Code(KeyCode::Esc)
            }
        );
        assert_eq!(
            parse_key("f5"),
            KeyBinding {
                mods: bare,
                spec: KeySpec::Code(KeyCode::F(5))
            }
        );
        assert_eq!(
            parse_key("f12"),
            KeyBinding {
                mods: bare,
                spec: KeySpec::Code(KeyCode::F(12))
            }
        );
        assert_eq!(
            parse_key(""),
            KeyBinding {
                mods: bare,
                spec: KeySpec::None
            }
        );
        assert_eq!(
            parse_key("nope-not-a-key"),
            KeyBinding {
                mods: bare,
                spec: KeySpec::None
            }
        );
    }

    #[test]
    fn parse_modifiers_keys() {
        use crossterm::event::KeyModifiers;
        assert_eq!(
            parse_key("ctrl+s"),
            KeyBinding {
                mods: KeyModifiers::CONTROL,
                spec: KeySpec::Char('s'),
            }
        );
        assert_eq!(
            parse_key("Ctrl+S"),
            KeyBinding {
                mods: KeyModifiers::CONTROL,
                spec: KeySpec::Char('S'),
            }
        );
        assert_eq!(
            parse_key("alt+x"),
            KeyBinding {
                mods: KeyModifiers::ALT,
                spec: KeySpec::Char('x'),
            }
        );
        assert_eq!(
            parse_key("shift+g"),
            KeyBinding {
                mods: KeyModifiers::SHIFT,
                spec: KeySpec::Char('g'),
            }
        );
        assert_eq!(
            parse_key("ctrl+shift+enter"),
            KeyBinding {
                mods: KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                spec: KeySpec::Code(KeyCode::Enter),
            }
        );
        assert_eq!(
            parse_key("super+f5"),
            KeyBinding {
                mods: KeyModifiers::SUPER,
                spec: KeySpec::Code(KeyCode::F(5)),
            }
        );
        // Unknown modifiers fall through to the unknown-key path: bad value
        // reported, default restored (see load_config_report tests).
        assert_eq!(
            parse_key("foo+s"),
            KeyBinding {
                mods: KeyModifiers::NONE,
                spec: KeySpec::None
            }
        );
        // Multiple keys in a combo are not a binding.
        assert_eq!(
            parse_key("ctrl+s+t"),
            KeyBinding {
                mods: KeyModifiers::NONE,
                spec: KeySpec::None
            }
        );
    }

    #[test]
    fn modifier_key_match_and_conflict() {
        use crossterm::event::KeyModifiers;
        let ctrl_s = parse_key("ctrl+s");
        assert_eq!(ctrl_s.mods, KeyModifiers::CONTROL);
        // ctrl+s fires on ctrl+s only.
        assert!(ctrl_s.matches(&KeyCode::Char('s'), KeyModifiers::CONTROL));
        assert!(!ctrl_s.matches(&KeyCode::Char('s'), KeyModifiers::NONE));
        assert!(!ctrl_s.matches(
            &KeyCode::Char('s'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT
        ));
        // A bare s does NOT fire when ctrl is held: a bare binding's
        // KeyBinding.mods is NONE, so with press modifiers = CONTROL the
        // press has no matching binding and is swallowed by the on_key
        // guard (typing safety). Bare-key binding stays reachable by
        // pressing without modifiers.
        let bare_s = parse_key("s");
        assert!(bare_s.matches(&KeyCode::Char('s'), KeyModifiers::NONE));
        assert!(!bare_s.matches(&KeyCode::Char('s'), KeyModifiers::CONTROL));
        // Collision: ctrl+s vs ctrl+s collide; ctrl+s vs s do not.
        assert!(ctrl_s.conflicts(&ctrl_s.clone()));
        assert!(!ctrl_s.conflicts(&bare_s));
        assert!(bare_s.conflicts(&parse_key("S")));
    }

    #[test]
    fn bare_uppercase_binding_fires_on_shifted_press() {
        use crossterm::event::KeyModifiers;
        // The shipped template recommends `key = "G"`: a real press
        // arrives as Char('G') + SHIFT, so the bare binding must
        // accept that (and only that) modifier set.
        let g = parse_key("G");
        assert!(g.matches(&KeyCode::Char('G'), KeyModifiers::SHIFT));
        assert!(!g.matches(&KeyCode::Char('G'), KeyModifiers::CONTROL));
        assert!(!g.matches(&KeyCode::Char('G'), KeyModifiers::SHIFT | KeyModifiers::ALT));
        // A bare lowercase binding stays unshifted-only.
        assert!(!parse_key("g").matches(&KeyCode::Char('G'), KeyModifiers::SHIFT));
    }

    #[test]
    fn key_binding_label() {
        use crossterm::event::KeyModifiers;
        assert_eq!(parse_key("n").label(), "n");
        assert_eq!(parse_key("enter").label(), "enter");
        assert_eq!(parse_key("f5").label(), "f5");
        assert_eq!(parse_key("ctrl+s").label(), "ctrl+s");
        assert_eq!(parse_key("ctrl+shift+enter").label(), "ctrl+shift+enter");
        assert_eq!(parse_key("super+alt+x").label(), "alt+super+x");
        assert_eq!(
            parse_key("").label(),
            KeyBinding {
                mods: KeyModifiers::NONE,
                spec: KeySpec::None
            }
            .label()
        );
    }

    #[test]
    fn fkey_range_rejects_absurd() {
        let bare = crossterm::event::KeyModifiers::NONE;
        assert_eq!(
            parse_key("f0"),
            KeyBinding {
                mods: bare,
                spec: KeySpec::None
            }
        );
        assert_eq!(
            parse_key("f99"),
            KeyBinding {
                mods: bare,
                spec: KeySpec::None
            }
        );
        assert_eq!(
            parse_key("f25"),
            KeyBinding {
                mods: bare,
                spec: KeySpec::None
            }
        );
        assert_eq!(
            parse_key("f1"),
            KeyBinding {
                mods: bare,
                spec: KeySpec::Code(KeyCode::F(1))
            }
        );
        assert_eq!(
            parse_key("f24"),
            KeyBinding {
                mods: bare,
                spec: KeySpec::Code(KeyCode::F(24))
            }
        );
    }

    #[test]
    fn letter_match_is_case_insensitive() {
        use crossterm::event::KeyModifiers;
        assert!(parse_key("n").matches(&KeyCode::Char('N'), KeyModifiers::NONE));
        assert!(parse_key("q").matches(&KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(!parse_key("n").matches(&KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(!parse_key("").matches(&KeyCode::Char('n'), KeyModifiers::NONE));
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
        assert_eq!(cfg.waiting_idle_secs, crate::daemon::WAITING_IDLE_SECS);
        assert_eq!(cfg.done_idle_secs, crate::daemon::DONE_IDLE_SECS);
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
    fn persist_skips_keys_the_patch_leaves_none() {
        // The clobber guard at the writer level: a `None` key is not
        // rewritten and not re-inserted, so an instance that changed
        // one setting cannot touch the others. This is what keeps a
        // stale instance from reverting a newer one's saved filter.
        let dir = test_dir();
        let path = dir.join("none.toml");
        std::fs::create_dir_all(&dir).unwrap();
        let before = "# terhub config: keys are single-key bindings.\n# hand-written\n[ui]\ntheme = \"dark\"\nfilter = \"!zsh\"\nsort = \"Idle\"\n\n[keys]\nnew = \"c\"\n";
        std::fs::write(&path, before).unwrap();
        persist_ui_settings_to(
            &path,
            &UiPersist {
                theme: Some("nord"),
                // Everything else None: leave the file's value alone.
                density: None,
                notifications: None,
                highlight: None,
                mru_tabs: None,
                filter: None,
                saved_filters: None,
                sort: None,
                sort_desc: None,
                hide_shells: None,
                blur_cols: None,
            },
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("theme = \"nord\""), "{text}");
        // Untouched lines survive byte-for-byte, including the filter
        // the user saved from a newer instance.
        assert!(text.contains("filter = \"!zsh\""), "{text}");
        assert!(text.contains("sort = \"Idle\""), "{text}");
        assert!(text.contains("# hand-written"), "{text}");
        // And a None key is never inserted as a duplicate.
        assert_eq!(text.matches("filter = ").count(), 1, "{text}");
        assert_eq!(text.matches("theme = ").count(), 1, "{text}");
        // Still parses (no duplicate keys).
        let (_cfg, problems) = load_config_report(Some(path.clone()));
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn persist_keeps_a_multiline_string_intact() {
        // A user's command template is a legal TOML multi-line string.
        // Its body is content, not config: the `FOO=1`/`FOO=2` lines are
        // not duplicate keys and the `[ui]` line is not a section
        // header. The writer used to read them as config and deleted
        // the second FOO on every save -- rewriting the user's script.
        let dir = test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("multiline.toml");
        let body = "[ui]\ntheme = \"dark\"\n\n[[command]]\nname = \"deploy\"\nkey = \"D\"\nrun = \"\"\"\nset -e\nFOO=1\nFOO=2\n[ui]\n\"\"\"\n";
        std::fs::write(&path, body).unwrap();
        persist_ui_settings_to(
            &path,
            &UiPersist {
                theme: Some("nord"),
                density: None,
                notifications: None,
                highlight: None,
                mru_tabs: None,
                filter: None,
                saved_filters: None,
                sort: None,
                sort_desc: None,
                hide_shells: None,
                blur_cols: None,
            },
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("FOO=1"), "the template lost a line:\n{text}");
        assert!(text.contains("FOO=2"), "the template lost a line:\n{text}");
        assert!(
            text.contains("[ui]\n\"\"\""),
            "the template's [ui] line was eaten as a section header:\n{text}"
        );
        // The theme still landed, exactly once.
        assert_eq!(text.matches("theme = ").count(), 1, "{text}");
        assert!(text.contains("theme = \"nord\""), "{text}");
        toml::from_str::<FileConfig>(&text).expect("persist must leave loadable TOML");
    }

    #[test]
    fn persist_recognises_a_quoted_ui_key() {
        // `"theme" = "dark"` is a legal TOML key. The writer used to
        // match only bare keys, so it believed the key was missing and
        // inserted its own assignment beside it -- the file then
        // defined the key twice, TOML rejected the whole file, and the
        // loader answered with defaults for *every* setting.
        let dir = test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("quoted.toml");
        std::fs::write(&path, "[ui]\n\"theme\" = \"dark\"\n").unwrap();
        persist_ui_settings_to(
            &path,
            &UiPersist {
                theme: Some("nord"),
                density: None,
                notifications: None,
                highlight: None,
                mru_tabs: None,
                filter: None,
                saved_filters: None,
                sort: None,
                sort_desc: None,
                hide_shells: None,
                blur_cols: None,
            },
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text.matches("theme").count(),
            1,
            "the key was written twice:\n{text}"
        );
        assert!(text.contains("nord"), "the new value did not land:\n{text}");
        toml::from_str::<FileConfig>(&text).expect("persist must leave loadable TOML");
        // The file still parses as a whole config -- that is the part
        // that used to break: a duplicated key made TOML reject the
        // file and the loader answered with defaults for everything.
        let cfg = toml::from_str::<FileConfig>(&text).unwrap();
        assert_eq!(cfg.ui.theme, "nord");
    }

    #[test]
    fn a_hash_inside_a_value_is_not_a_comment() {
        // `filter = "npm #rust"` -- the `#` is part of the value. The
        // writer spliced the tail onto the rewritten line, turning the
        // user's value into `filter = "npm #rust"#rust"` and breaking
        // its promise to preserve everything else byte-for-byte.
        let dir = test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hash.toml");
        std::fs::write(&path, "[ui]\nfilter = \"npm #rust\"\n").unwrap();
        persist_ui_settings_to(
            &path,
            &UiPersist {
                theme: None,
                density: None,
                notifications: None,
                highlight: None,
                mru_tabs: None,
                filter: Some("npm #rust"),
                saved_filters: None,
                sort: None,
                sort_desc: None,
                hide_shells: None,
                blur_cols: None,
            },
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("filter = \"npm #rust\"\n"),
            "the value was mangled:\n{text}"
        );
        assert!(!text.contains("#rust\"#"), "the value was mangled:\n{text}");
        toml::from_str::<FileConfig>(&text).expect("persist must leave loadable TOML");
    }

    #[test]
    fn persist_repairs_a_config_with_duplicate_keys() {
        // The forgetting bug this file actually hit: a key assigned
        // twice in one section makes the whole file unparseable, and an
        // unparseable config is answered with defaults — so every part
        // of the standing view (filter included) is silently forgotten
        // on every start. A persist must repair it, not carry it on.
        let dir = test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dupe.toml");
        std::fs::write(
            &path,
            "[ui]\ntheme = \"dark\"\nfilter = \"!bash\"\nsort = \"Idle\"\nfilter = \"!zsh\"\n\n[keys]\nnew = \"c\"\n",
        )
        .unwrap();
        // Before the write, the file is unparseable.
        assert!(
            toml::from_str::<FileConfig>(&std::fs::read_to_string(&path).unwrap()).is_err(),
            "fixture should be broken TOML"
        );
        persist_ui_settings_to(
            &path,
            &UiPersist {
                theme: Some("nord"),
                density: None,
                notifications: None,
                highlight: None,
                mru_tabs: None,
                filter: Some("!zsh"),
                saved_filters: None,
                sort: None,
                sort_desc: None,
                hide_shells: None,
                blur_cols: None,
            },
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        // First assignment wins for the keys we don't touch, the file
        // parses again, and the value we wrote landed exactly once.
        assert_eq!(text.matches("filter = ").count(), 1, "{text}");
        assert_eq!(text.matches("theme = ").count(), 1, "{text}");
        assert!(text.contains("filter = \"!zsh\""), "{text}");
        assert!(text.contains("theme = \"nord\""), "{text}");
        toml::from_str::<FileConfig>(&text).expect("persist must leave loadable TOML");
        // Comments and other sections survive the repair.
        assert!(text.contains("[keys]"), "{text}");
    }

    /// Shapes a real config.toml can take. A persist must never make
    /// an already-valid file unparseable, and must never lose the user's
    /// values in the sections/keys it was not asked to change.
    #[test]
    fn persist_survives_real_world_config_shapes() {
        // (name, original text, expected theme, expected filter, has keys section)
        let cases: &[(&str, &str, &str, &str, bool)] = &[
            (
                "crlf",
                "[ui]\r\ntheme = \"dark\"\r\nfilter = \"!zsh\"\r\n\r\n[keys]\r\nnew = \"c\"\r\n",
                "dark",
                "!zsh",
                true,
            ),
            (
                "commented header",
                "# lead\n[ui] # theme settings\ntheme = \"dark\"\n",
                "dark",
                "",
                false,
            ),
            (
                "indented keys",
                "[ui]\n  theme = \"dark\"\n\t filter = \"!zsh\"\n",
                "dark",
                "!zsh",
                false,
            ),
            (
                "no spaces around eq",
                "[ui]\ntheme=\"dark\"\n",
                "dark",
                "",
                false,
            ),
            ("single quotes", "[ui]\ntheme = 'dark'\n", "dark", "", false),
            (
                "trailing whitespace",
                "[ui]\ntheme = \"dark\"   \n",
                "dark",
                "",
                false,
            ),
            ("no ui section", "[keys]\nnew = \"c\"\n", "dark", "", true),
            (
                "empty ui section",
                "[ui]\n\n[style]\nidle_flag_secs = 300\n",
                "dark",
                "",
                false,
            ),
            ("bom", "\u{feff}[ui]\ntheme = \"dark\"\n", "dark", "", false),
        ];
        for (name, original, want_theme, want_filter, has_keys) in cases {
            let dir = test_dir();
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join(format!("shape-{name}.toml"));
            std::fs::write(&path, original).unwrap();
            // A no-op patch: nothing to change, so the file must come
            // back still parseable with the same values — the persist
            // path must not rewrite blindly.
            persist_ui_settings_to(&path, &UiPersist::default()).unwrap();
            let text = std::fs::read_to_string(&path).unwrap();
            let (cfg, problems) = load_config_report(Some(path.clone()));
            assert!(
                problems.is_empty(),
                "{name}: no-op persist reported a problem: {problems:?}\n{text}"
            );
            assert_eq!(
                (cfg.theme.as_str(), cfg.filter.as_str()),
                (*want_theme, *want_filter),
                "{name}: a no-op persist changed stored values\n{text}"
            );
            if *has_keys {
                assert!(text.contains("new = \"c\""), "{name}: lost [keys]\n{text}");
            }
            if text.contains("[style]") {
                assert!(
                    text.contains("idle_flag_secs = 300"),
                    "{name}: lost [style]\n{text}"
                );
            }
            // A no-op persist must not silently change byte order marks;
            // the file has none in the input, and must have none out.
            assert!(
                !text.starts_with('\u{feff}'),
                "{name}: a no-op persist left a BOM at the start\n{text}"
            );
            std::fs::remove_file(path).ok();
        }
        // Second half of the contract: with a real patch, every value
        // must land and the file must still parse — a shape can survive
        // a no-op yet corrupt when a key has to be inserted or rewritten.
        for (name, original, _, _, _) in cases {
            let dir = test_dir();
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join(format!("shape-write-{name}.toml"));
            std::fs::write(&path, original).unwrap();
            persist_ui_settings_to(
                &path,
                &UiPersist {
                    theme: Some("gruvbox"),
                    density: Some("compact"),
                    notifications: Some(false),
                    highlight: Some(true),
                    mru_tabs: Some(true),
                    filter: Some("!noping"),
                    saved_filters: None,
                    sort: Some("State"),
                    sort_desc: Some(false),
                    hide_shells: Some(true),
                    blur_cols: Some(false),
                },
            )
            .unwrap();
            let text = std::fs::read_to_string(&path).unwrap();
            let (cfg, problems) = load_config_report(Some(path.clone()));
            assert!(
                problems.is_empty(),
                "{name}: patch persist reported a problem: {problems:?}\n{text}"
            );
            assert_eq!(cfg.theme, "gruvbox", "{name}\n{text}");
            assert_eq!(cfg.filter, "!noping", "{name}\n{text}");
            assert_eq!(cfg.sort, "State", "{name}\n{text}");
            assert!(!cfg.sort_desc, "{name}\n{text}");
            assert!(cfg.hide_shells, "{name}\n{text}");
            assert!(cfg.compact(), "{name}\n{text}");
            // No key may be duplicated by the write.
            for key in [
                "theme",
                "filter",
                "sort",
                "sort_desc",
                "hide_shells",
                "density",
            ] {
                assert_eq!(
                    text.matches(&format!("{key} = ")).count(),
                    1,
                    "{name}: key {key} written more than once\n{text}"
                );
            }
            // And a persisted file must not carry a BOM into a future
            // load that doesn't strip one (terhub's own reader does, but
            // other tools and the toml crate's parsers do not).
            assert!(
                !text.starts_with('\u{feff}'),
                "{name}: persist left a leading BOM in {path:?}\n{text}"
            );
            std::fs::remove_file(path).ok();
        }
    }

    #[test]
    fn persist_ignores_commented_out_section_header() {
        // `# [ui]` is a comment, not a header: the theme line below
        // it is top-level in real TOML. Persist must create/use a
        // real [ui] section instead of rewriting the top-level line
        // in place and reporting success while loads ignore it.
        let dir = test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("commented-header.toml");
        std::fs::write(&path, "# [ui]\ntheme = \"nord\"\n").unwrap();
        persist_ui_settings_to(
            &path,
            &UiPersist {
                theme: Some("gruvbox"),
                ..UiPersist::default()
            },
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let (cfg, problems) = load_config_report(Some(path.clone()));
        // The pre-existing top-level line is (correctly) reported as
        // misplaced, but the persisted value must land in [ui].
        assert!(
            problems.iter().all(|p| p.contains("'theme'")),
            "{problems:?}\n{text}"
        );
        assert_eq!(cfg.theme, "gruvbox", "{text}");
        // Exactly one real [ui] header, and the comment line survives.
        assert_eq!(
            text.lines().filter(|l| l.trim() == "[ui]").count(),
            1,
            "{text}"
        );
        assert!(text.contains("# [ui]"), "{text}");
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
        persist_ui_settings_to(
            &path,
            &UiPersist {
                theme: Some("nord"),
                density: Some("compact"),
                notifications: Some(false),
                highlight: Some(false),
                mru_tabs: Some(false),
                filter: Some("!zsh"),
                saved_filters: None,
                sort: Some("State"),
                sort_desc: Some(true),
                hide_shells: Some(true),
                blur_cols: Some(true),
            },
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# my fancy config"), "{text}");
        assert!(
            text.contains("# theme: dark, light, dracula, nord, gruvbox, solarized, tokyonight, catppuccin, onedark, monokai, rose-pine, everforest, kanagawa, ayu, nightfly, palenight, github, github-light, onehalf, onehalf-light."),
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
        assert_eq!(cfg.filter, "!zsh");
        assert_eq!(cfg.sort, "State");
        assert!(cfg.sort_desc);
        assert!(cfg.hide_shells);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn unknown_sort_warns_and_falls_back() {
        let dir = test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("badsort.toml");
        std::fs::write(&path, "[ui]\nsort = \"frobnicate\"\n").unwrap();
        let (cfg, problems) = load_config_report(Some(path.clone()));
        assert_eq!(cfg.sort, "Idle");
        assert!(cfg.sort_desc);
        assert!(
            problems.iter().any(|p| p.contains("'frobnicate'")),
            "{problems:?}"
        );
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

    #[test]
    fn unknown_section_field_is_reported_not_fatal() {
        // A stale/misplaced key (e.g. a removed binding left under the
        // wrong section) must degrade that one field and be named —
        // never discard the whole file and silently fall back to
        // defaults.
        let dir = test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let odd = dir.join("misplaced.toml");
        std::fs::write(
            &odd,
            "[ui]\ntheme = \"dracula\"\n[keys]\nnew = \"c\"\n[style]\nfilter = \"f\"\nsettings = \"s\"\n",
        )
        .unwrap();
        let (cfg, problems) = load_config_report(Some(odd.clone()));
        // The valid sections still apply.
        assert_eq!(cfg.palette(), "dracula");
        assert_eq!(cfg.key("new"), "c");
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("'filter' in [style]")));
        assert!(problems.iter().any(|p| p.contains("'settings' in [style]")));
        std::fs::remove_file(odd).ok();
    }
}
