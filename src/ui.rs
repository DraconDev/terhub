//! ratatui rendering for the hub. `render` returns the screen areas occupied by
//! the session rows and pane rows so the event loop can map mouse clicks.
use crate::app::{App, Focus, Mode, SortKey, is_quiet_shell};
use crate::tmux::Pane;
use ansi_to_tui::IntoText;
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Cell, Clear, Paragraph, Row, Table, Wrap},
};
use std::collections::HashMap;

pub struct Palette {
    pub border: Style,
    pub title: Style,
    pub header: Style,
    pub selected: Style,
    pub attached: Style,
    pub status: Style,
    pub dim: Style,
    pub focus: Style,
    /// Done-row accent: a quiet green check for finished tasks. The
    /// State column for `AgentState::Done` reads in this colour, so
    /// a finished pane is a one-glance "all good" without competing
    /// for the bright red reserved for things needing action.
    pub done: Style,
}

pub fn palette(theme: &str) -> Palette {
    let rgb = Color::Rgb;
    match theme {
        "rose-pine" => Palette {
            border: Style::default().fg(rgb(0x6E, 0x6A, 0x86)),
            title: Style::default()
                .fg(rgb(0x9C, 0xCF, 0xD8))
                .add_modifier(Modifier::BOLD),
            header: Style::default()
                .fg(rgb(0xF6, 0xC1, 0x77))
                .add_modifier(Modifier::BOLD),
            selected: Style::default()
                .fg(rgb(0x1F, 0x1D, 0x2E))
                .bg(rgb(0x9C, 0xCF, 0xD8)),
            attached: Style::default().fg(rgb(0x31, 0x74, 0x8F)),
            status: Style::default().fg(rgb(0xE0, 0xDE, 0xF4)),
            dim: Style::default().fg(rgb(0x90, 0x8C, 0xAA)),
            focus: Style::default().fg(rgb(0xEB, 0xBC, 0xBA)),
            done: Style::default().fg(rgb(0x9C, 0xC1, 0x9A)),
        },
        "everforest" => Palette {
            border: Style::default().fg(rgb(0x5D, 0x65, 0x5D)),
            title: Style::default()
                .fg(rgb(0xA7, 0xC0, 0x80))
                .add_modifier(Modifier::BOLD),
            header: Style::default()
                .fg(rgb(0xDB, 0xBC, 0x7F))
                .add_modifier(Modifier::BOLD),
            selected: Style::default()
                .fg(rgb(0x2D, 0x35, 0x3B))
                .bg(rgb(0xA7, 0xC0, 0x80)),
            attached: Style::default().fg(rgb(0x7F, 0xBB, 0xB3)),
            status: Style::default().fg(rgb(0xD3, 0xC6, 0xAA)),
            dim: Style::default().fg(rgb(0x85, 0x92, 0x89)),
            focus: Style::default().fg(rgb(0xD6, 0x99, 0xB6)),
            done: Style::default().fg(rgb(0x9C, 0xC1, 0x9A)),
        },
        "kanagawa" => Palette {
            border: Style::default().fg(rgb(0x54, 0x54, 0x46)),
            title: Style::default()
                .fg(rgb(0x7F, 0xB4, 0xCA))
                .add_modifier(Modifier::BOLD),
            header: Style::default()
                .fg(rgb(0xC8, 0xB3, 0x6C))
                .add_modifier(Modifier::BOLD),
            selected: Style::default()
                .fg(rgb(0xD6, 0xD1, 0xC7))
                .bg(rgb(0x22, 0x33, 0x3D)),
            attached: Style::default().fg(rgb(0x87, 0xA9, 0x87)),
            status: Style::default().fg(rgb(0xC5, 0xC9, 0xC5)),
            dim: Style::default().fg(rgb(0x62, 0x5E, 0x5A)),
            focus: Style::default().fg(rgb(0x95, 0x73, 0xA7)),
            done: Style::default().fg(rgb(0x9C, 0xC1, 0x9A)),
        },
        "ayu" => Palette {
            border: Style::default().fg(rgb(0x33, 0x44, 0x59)),
            title: Style::default()
                .fg(rgb(0x59, 0xC2, 0xFF))
                .add_modifier(Modifier::BOLD),
            header: Style::default()
                .fg(rgb(0xFF, 0xB4, 0x54))
                .add_modifier(Modifier::BOLD),
            selected: Style::default()
                .fg(rgb(0x0F, 0x14, 0x19))
                .bg(rgb(0x95, 0xE6, 0xCB)),
            attached: Style::default().fg(rgb(0xAA, 0xD9, 0x4C)),
            status: Style::default().fg(rgb(0xB3, 0xB1, 0xAD)),
            dim: Style::default().fg(rgb(0x3E, 0x4B, 0x59)),
            focus: Style::default().fg(rgb(0xD2, 0xA6, 0xFF)),
            done: Style::default().fg(rgb(0x9C, 0xC1, 0x9A)),
        },
        "nightfly" => Palette {
            border: Style::default().fg(rgb(0x1D, 0x3B, 0x53)),
            title: Style::default()
                .fg(rgb(0x7A, 0xA6, 0xDA))
                .add_modifier(Modifier::BOLD),
            header: Style::default()
                .fg(rgb(0xFF, 0xCC, 0x00))
                .add_modifier(Modifier::BOLD),
            selected: Style::default()
                .fg(rgb(0x01, 0x16, 0x27))
                .bg(rgb(0x5F, 0xB4, 0xD4)),
            attached: Style::default().fg(rgb(0xA1, 0xCD, 0x76)),
            status: Style::default().fg(rgb(0xB2, 0xB2, 0xB2)),
            dim: Style::default().fg(rgb(0x5C, 0x66, 0x70)),
            focus: Style::default().fg(rgb(0xC7, 0x92, 0xEA)),
            done: Style::default().fg(rgb(0x9C, 0xC1, 0x9A)),
        },
        "palenight" => Palette {
            border: Style::default().fg(rgb(0x3A, 0x3F, 0x5B)),
            title: Style::default()
                .fg(rgb(0x82, 0xAA, 0xFF))
                .add_modifier(Modifier::BOLD),
            header: Style::default()
                .fg(rgb(0xFF, 0xCB, 0x6B))
                .add_modifier(Modifier::BOLD),
            selected: Style::default()
                .fg(rgb(0x29, 0x2D, 0x3E))
                .bg(rgb(0x86, 0xE1, 0xFC)),
            attached: Style::default().fg(rgb(0xC3, 0xE8, 0x8D)),
            status: Style::default().fg(rgb(0xBF, 0xCA, 0xE0)),
            dim: Style::default().fg(rgb(0x67, 0x6E, 0x95)),
            focus: Style::default().fg(rgb(0xC7, 0x92, 0xEA)),
            done: Style::default().fg(rgb(0x9C, 0xC1, 0x9A)),
        },
        "github" => Palette {
            border: Style::default().fg(rgb(0x30, 0x35, 0x3D)),
            title: Style::default()
                .fg(rgb(0x58, 0xA6, 0xFF))
                .add_modifier(Modifier::BOLD),
            header: Style::default()
                .fg(rgb(0xD2, 0x99, 0x22))
                .add_modifier(Modifier::BOLD),
            selected: Style::default()
                .fg(rgb(0x0D, 0x11, 0x17))
                .bg(rgb(0x3F, 0xB9, 0x50)),
            attached: Style::default().fg(rgb(0x3F, 0xB9, 0x50)),
            status: Style::default().fg(rgb(0xC9, 0xD1, 0xD9)),
            dim: Style::default().fg(rgb(0x8B, 0x94, 0x9E)),
            focus: Style::default().fg(rgb(0xBC, 0x8C, 0xFF)),
            done: Style::default().fg(rgb(0x9C, 0xC1, 0x9A)),
        },
        "github-light" => Palette {
            border: Style::default().fg(rgb(0xE1, 0xE4, 0xE8)),
            title: Style::default()
                .fg(rgb(0x03, 0x66, 0xD6))
                .add_modifier(Modifier::BOLD),
            header: Style::default()
                .fg(rgb(0xDB, 0xAB, 0x09))
                .add_modifier(Modifier::BOLD),
            selected: Style::default()
                .fg(rgb(0xFF, 0xFF, 0xFF))
                .bg(rgb(0x03, 0x66, 0xD6)),
            attached: Style::default().fg(rgb(0x28, 0xA7, 0x45)),
            status: Style::default().fg(rgb(0x24, 0x29, 0x2E)),
            dim: Style::default().fg(rgb(0x6A, 0x73, 0x7D)),
            focus: Style::default().fg(rgb(0x6F, 0x42, 0xC1)),
            done: Style::default().fg(rgb(0x9C, 0xC1, 0x9A)),
        },
        "onehalf" => Palette {
            border: Style::default().fg(rgb(0x5C, 0x63, 0x70)),
            title: Style::default()
                .fg(rgb(0x61, 0xAF, 0xEF))
                .add_modifier(Modifier::BOLD),
            header: Style::default()
                .fg(rgb(0xE5, 0xC0, 0x7B))
                .add_modifier(Modifier::BOLD),
            selected: Style::default()
                .fg(rgb(0xDC, 0xDE, 0xE0))
                .bg(rgb(0x3E, 0x44, 0x52)),
            attached: Style::default().fg(rgb(0x98, 0xC3, 0x79)),
            status: Style::default().fg(rgb(0xDC, 0xDE, 0xE0)),
            dim: Style::default().fg(rgb(0x5C, 0x63, 0x70)),
            focus: Style::default().fg(rgb(0xC6, 0x78, 0xDD)),
            done: Style::default().fg(rgb(0x9C, 0xC1, 0x9A)),
        },
        "onehalf-light" => Palette {
            border: Style::default().fg(rgb(0xE0, 0xE0, 0xE0)),
            title: Style::default()
                .fg(rgb(0x40, 0x78, 0xF2))
                .add_modifier(Modifier::BOLD),
            header: Style::default()
                .fg(rgb(0xF4, 0xB7, 0x3E))
                .add_modifier(Modifier::BOLD),
            selected: Style::default()
                .fg(rgb(0xFF, 0xFF, 0xFF))
                .bg(rgb(0x40, 0x78, 0xF2)),
            attached: Style::default().fg(rgb(0x98, 0xC3, 0x79)),
            status: Style::default().fg(rgb(0x38, 0x3A, 0x42)),
            dim: Style::default().fg(rgb(0xA3, 0xA3, 0xA3)),
            focus: Style::default().fg(rgb(0xA6, 0x26, 0xA4)),
            done: Style::default().fg(rgb(0x9C, 0xC1, 0x9A)),
        },
        "light" => Palette {
            border: Style::default().fg(Color::Blue),
            title: Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::BOLD),
            header: Style::default()
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
            selected: Style::default().fg(Color::Black).bg(Color::Cyan),
            attached: Style::default().fg(Color::Green),
            status: Style::default().fg(Color::Black),
            dim: Style::default().fg(Color::DarkGray),
            focus: Style::default().fg(Color::Magenta),
            done: Style::default().fg(Color::LightGreen),
        },
        "dracula" => Palette {
            border: Style::default().fg(rgb(0x62, 0x72, 0xA4)),
            title: Style::default()
                .fg(rgb(0x8B, 0xE9, 0xFD))
                .add_modifier(Modifier::BOLD),
            header: Style::default()
                .fg(rgb(0xF1, 0xFA, 0x8C))
                .add_modifier(Modifier::BOLD),
            selected: Style::default()
                .fg(rgb(0xF8, 0xF8, 0xF2))
                .bg(rgb(0x44, 0x47, 0x5A)),
            attached: Style::default().fg(rgb(0x50, 0xFA, 0x7B)),
            status: Style::default().fg(rgb(0xF8, 0xF8, 0xF2)),
            dim: Style::default().fg(rgb(0x62, 0x72, 0xA4)),
            focus: Style::default().fg(rgb(0xFF, 0x79, 0xC6)),
            done: Style::default().fg(rgb(0x9C, 0xC1, 0x9A)),
        },
        "nord" => Palette {
            border: Style::default().fg(rgb(0x4C, 0x56, 0x6A)),
            title: Style::default()
                .fg(rgb(0x88, 0xC0, 0xD0))
                .add_modifier(Modifier::BOLD),
            header: Style::default()
                .fg(rgb(0xEB, 0xCB, 0x8B))
                .add_modifier(Modifier::BOLD),
            selected: Style::default()
                .fg(rgb(0xEC, 0xEF, 0xF4))
                .bg(rgb(0x43, 0x4C, 0x5E)),
            attached: Style::default().fg(rgb(0xA3, 0xBE, 0x8C)),
            status: Style::default().fg(rgb(0xE5, 0xE9, 0xF0)),
            dim: Style::default().fg(rgb(0x4C, 0x56, 0x6A)),
            focus: Style::default().fg(rgb(0xD0, 0x87, 0x70)),
            done: Style::default().fg(rgb(0x9C, 0xC1, 0x9A)),
        },
        "gruvbox" => Palette {
            border: Style::default().fg(rgb(0x92, 0x83, 0x74)),
            title: Style::default()
                .fg(rgb(0x83, 0xA5, 0x98))
                .add_modifier(Modifier::BOLD),
            header: Style::default()
                .fg(rgb(0xFA, 0xBD, 0x2F))
                .add_modifier(Modifier::BOLD),
            selected: Style::default()
                .fg(rgb(0x28, 0x28, 0x28))
                .bg(rgb(0xFA, 0xBD, 0x2F)),
            attached: Style::default().fg(rgb(0xB8, 0xBB, 0x26)),
            status: Style::default().fg(rgb(0xEB, 0xDB, 0xB2)),
            dim: Style::default().fg(rgb(0x92, 0x83, 0x74)),
            focus: Style::default().fg(rgb(0xFE, 0x80, 0x19)),
            done: Style::default().fg(rgb(0x9C, 0xC1, 0x9A)),
        },
        "solarized" => Palette {
            border: Style::default().fg(rgb(0x58, 0x6E, 0x75)),
            title: Style::default()
                .fg(rgb(0x26, 0x8B, 0xD2))
                .add_modifier(Modifier::BOLD),
            header: Style::default()
                .fg(rgb(0xB5, 0x89, 0x00))
                .add_modifier(Modifier::BOLD),
            selected: Style::default()
                .fg(rgb(0x00, 0x2B, 0x36))
                .bg(rgb(0x2A, 0xA1, 0x98)),
            attached: Style::default().fg(rgb(0x85, 0x99, 0x00)),
            status: Style::default().fg(rgb(0x83, 0x94, 0x96)),
            dim: Style::default().fg(rgb(0x58, 0x6E, 0x75)),
            focus: Style::default().fg(rgb(0xCB, 0x4B, 0x16)),
            done: Style::default().fg(rgb(0x9C, 0xC1, 0x9A)),
        },
        "tokyonight" => Palette {
            border: Style::default().fg(rgb(0x56, 0x5F, 0x89)),
            title: Style::default()
                .fg(rgb(0x7D, 0xCF, 0xFF))
                .add_modifier(Modifier::BOLD),
            header: Style::default()
                .fg(rgb(0xE0, 0xAF, 0x68))
                .add_modifier(Modifier::BOLD),
            selected: Style::default()
                .fg(rgb(0xC0, 0xCA, 0xF5))
                .bg(rgb(0x33, 0x46, 0x7C)),
            attached: Style::default().fg(rgb(0x9E, 0xCE, 0x6A)),
            status: Style::default().fg(rgb(0xC0, 0xCA, 0xF5)),
            dim: Style::default().fg(rgb(0x56, 0x5F, 0x89)),
            focus: Style::default().fg(rgb(0xBB, 0x9A, 0xF7)),
            done: Style::default().fg(rgb(0x9C, 0xC1, 0x9A)),
        },
        "catppuccin" => Palette {
            border: Style::default().fg(rgb(0x6C, 0x70, 0x86)),
            title: Style::default()
                .fg(rgb(0x74, 0xC7, 0xEC))
                .add_modifier(Modifier::BOLD),
            header: Style::default()
                .fg(rgb(0xF9, 0xE2, 0xAF))
                .add_modifier(Modifier::BOLD),
            selected: Style::default()
                .fg(rgb(0xCD, 0xD6, 0xF4))
                .bg(rgb(0x45, 0x47, 0x5B)),
            attached: Style::default().fg(rgb(0xA6, 0xE3, 0xA1)),
            status: Style::default().fg(rgb(0xCD, 0xD6, 0xF4)),
            dim: Style::default().fg(rgb(0x6C, 0x70, 0x86)),
            focus: Style::default().fg(rgb(0xCB, 0xA6, 0xF7)),
            done: Style::default().fg(rgb(0x9C, 0xC1, 0x9A)),
        },
        "onedark" => Palette {
            border: Style::default().fg(rgb(0x5C, 0x63, 0x70)),
            title: Style::default()
                .fg(rgb(0x56, 0xB6, 0xC2))
                .add_modifier(Modifier::BOLD),
            header: Style::default()
                .fg(rgb(0xE5, 0xC0, 0x7B))
                .add_modifier(Modifier::BOLD),
            selected: Style::default()
                .fg(rgb(0xAB, 0xB2, 0xBF))
                .bg(rgb(0x3E, 0x44, 0x51)),
            attached: Style::default().fg(rgb(0x98, 0xC3, 0x79)),
            status: Style::default().fg(rgb(0xAB, 0xB2, 0xBF)),
            dim: Style::default().fg(rgb(0x5C, 0x63, 0x70)),
            focus: Style::default().fg(rgb(0xC6, 0x78, 0xDD)),
            done: Style::default().fg(rgb(0x9C, 0xC1, 0x9A)),
        },
        "monokai" => Palette {
            border: Style::default().fg(rgb(0x75, 0x71, 0x5E)),
            title: Style::default()
                .fg(rgb(0x66, 0xD9, 0xEF))
                .add_modifier(Modifier::BOLD),
            header: Style::default()
                .fg(rgb(0xE6, 0xDB, 0x74))
                .add_modifier(Modifier::BOLD),
            selected: Style::default()
                .fg(rgb(0xF8, 0xF8, 0xF2))
                .bg(rgb(0x49, 0x48, 0x3E)),
            attached: Style::default().fg(rgb(0xA6, 0xE2, 0x2E)),
            status: Style::default().fg(rgb(0xF8, 0xF8, 0xF2)),
            dim: Style::default().fg(rgb(0x75, 0x71, 0x5E)),
            focus: Style::default().fg(rgb(0xF9, 0x26, 0x72)),
            done: Style::default().fg(rgb(0x9C, 0xC1, 0x9A)),
        },
        _ => Palette {
            // The default dark style: explicit RGBs with one job each
            // (a previous revision reused terminal ANSI names, leaving
            // title==border, header==focus and attached==working).
            // Border recedes; title is identity blue; header is
            // structural amber; selection is an unmissable blue block;
            // attached green means live; status is neutral light grey;
            // dim stays readable on black; focus purple is reserved for
            // you-are-here so it never collides with the header.
            border: Style::default().fg(rgb(0x6E, 0x76, 0x81)),
            title: Style::default()
                .fg(rgb(0x79, 0xC0, 0xFF))
                .add_modifier(Modifier::BOLD),
            header: Style::default()
                .fg(rgb(0xFF, 0xA6, 0x57))
                .add_modifier(Modifier::BOLD),
            selected: Style::default()
                .fg(rgb(0xFF, 0xFF, 0xFF))
                .bg(rgb(0x1F, 0x6F, 0xEB)),
            attached: Style::default().fg(rgb(0x7E, 0xE7, 0x87)),
            status: Style::default().fg(rgb(0xC9, 0xD1, 0xD9)),
            dim: Style::default().fg(rgb(0x9A, 0xA5, 0xB1)),
            focus: Style::default().fg(rgb(0xD2, 0xA8, 0xFF)),
            done: Style::default().fg(rgb(0x9C, 0xC1, 0x9A)),
        },
    }
}

/// One table header cell: click area plus the sort it selects (`None`
/// for the unsortable mark column).
#[derive(Debug, Clone, Copy)]
pub struct HeaderHit {
    pub area: Rect,
    pub key: Option<SortKey>,
}

#[derive(Debug, Default, Clone)]
pub struct ClickAreas {
    pub sessions: Rect,
    pub panes: Rect,
    /// Clickable session segments within `sessions`, in row order.
    pub segments: Vec<Rect>,
    /// Settings content rows area (empty rect when closed).
    pub settings: Rect,
    /// Settings sidebar rows, one per section (empty when closed).
    pub settings_side: Vec<Rect>,
    /// Table header cells for click-to-sort (empty when covered).
    pub headers: Vec<HeaderHit>,
}

pub fn render(frame: &mut Frame, app: &mut App) -> ClickAreas {
    app.expire_status();
    let pal = palette(app.config.palette());
    let area = frame.area();
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(area);

    // Fullscreen preview: nothing else is visible or clickable.
    if app.show_preview && app.mode == Mode::Normal {
        if app.selected_pane().is_none() {
            app.show_preview = false;
        } else {
            render_preview(frame, app, &pal, area);
            return ClickAreas::default();
        }
    }

    // The selector bar manages its own horizontal scroll; the table body is
    // the area minus top/bottom borders minus the header row.
    let (sessions_area, segments) = render_selector(frame, app, &pal, chunks[0]);
    app.ensure_visible(chunks[1].height.saturating_sub(3) as usize);
    let (panes_area, header_hits) = render_table(frame, app, &pal, chunks[1]);

    let (status, style) = match &app.status {
        Some(s) => (s.clone(), pal.status),
        None if app.sessions.is_empty() => (
            format!(
                "no sessions — press {} to create one",
                app.config.key_label("new")
            ),
            pal.dim,
        ),
        None => (
            format!(
                "left-click = jump · right-click = preview · ctrl+click session = attach · tab = switch list · ←/→ sessions · ↑/↓ move · click header = sort · {} = filter · {} = settings",
                app.config.key_label("filter"),
                app.config.key_label("settings")
            ),
            pal.dim,
        ),
    };
    frame.render_widget(Paragraph::new(status).style(style), chunks[2]);

    let (settings, settings_side) = if app.mode == Mode::Settings {
        render_settings(frame, app, &pal, area)
    } else {
        if app.mode != Mode::Normal {
            render_modal(frame, app, &pal, area);
        }
        (Rect::default(), Vec::new())
    };
    ClickAreas {
        sessions: sessions_area,
        panes: panes_area,
        segments,
        settings,
        settings_side,
        headers: header_hits,
    }
}

fn truncate(s: &str, n: usize) -> String {
    let mut out: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        out.push('…');
    }
    out
}

/// The record/share blur: replace a column's text with a solid fill of
/// the same width so the table layout (and every non-sensitive column)
/// stays intact while the value is gone. The fill is a `█` of the
/// target width; the *style* is the caller's business, the helper only
/// owns the string. When the value is a dash placeholder ("—") or is
/// empty, the cell already says "nothing here" — leave it, swapping a
/// dash for blocks just moves the eye without hiding anything.
fn blur_cell(value: &str) -> String {
    if value.is_empty() || value.chars().all(|c| c == '—' || c.is_whitespace()) {
        return value.to_string();
    }
    "█".repeat(value.chars().count())
}

/// The chip's label at full length, or `None` when nothing is filtered:
/// every filter that is hiding rows, so an active filter is never silent.
///
/// Kept separate from the styled rendering so the caller can measure the
/// label *before* it is truncated — it has to know how wide the chip
/// wants to be to decide whether the title line has room for it.
fn filter_chip_label(app: &App) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    // The standing exclusion (`[ui] filter`) only ever hides.
    if !app.exclude.is_empty() {
        parts.push(format!("filter: {}", truncate(&app.exclude, 24)));
    }
    // A saved filter says its name; a hand-typed query says the query.
    match app.active_saved_filter.as_deref() {
        Some(name) => parts.push(format!("filter: {}", truncate(name, 24))),
        None if !app.filter.is_empty() => {
            parts.push(format!("search: {}", truncate(&app.filter, 24)));
        }
        None => {}
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join(" · "))
}

/// Build the chip from its label, capped at `max_label` cells so the
/// caller can never be handed something wider than it can draw.
fn filter_chip(label: &str, pal: &Palette, max_label: usize) -> Line<'static> {
    Line::from(vec![
        // The block: a filled square in the title ink, so the chip reads
        // as one object rather than more title text.
        Span::styled("◼", pal.title),
        Span::styled(format!(" {}", truncate(label, max_label)), pal.title),
    ])
}

/// Render a 6-bucket sparkline bucket array as 6 block characters.
/// Heights are log-mapped: a single tall bar is the signal the user
/// wants (a busy tick next to quiet ones), not arithmetic — equal
/// bars feel like a flatline even when one tick had 50x the bytes.
/// The lowest step (`▁`) is still visible above the row baseline, so
/// a "quiet" pane shows a flat ridge instead of nothing.
pub fn sparkline_glyphs(buckets: &[u32; 6]) -> String {
    // Find the max so we can scale. Empty ring (all zeros) → flat
    // ridge: a uniform 0 means a fresh pane with no samples.
    let max = buckets.iter().copied().max().unwrap_or(0);
    const STEPS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    if max == 0 {
        return "▁▁▁▁▁▁".to_string();
    }
    // Log scale: a 4096-byte tick shows full height; a 1-byte tick
    // shows the smallest step. log2(4096) = 12, so step index =
    // round(log2(bucket + 1) * 7 / 12). Bounded to 7 (the last step).
    let mut out = String::with_capacity(12);
    for &b in buckets {
        let step = if b == 0 {
            0
        } else {
            // log2(b + 1) * 7 / 12, clamped to 7
            let log_bits = 32 - b.saturating_add(1).leading_zeros() - 1;
            ((log_bits as u64 * 7 + 6) / 12).min(7) as usize
        };
        out.push(STEPS[step]);
    }
    out
}

/// Preview text with embedded ANSI colors. tmux `-e` captures carry SGR
/// styling; unparseable input falls back to stripped plain text so raw
/// escapes never leak onto the screen.
///
/// Lines whose trailing SGR tail leaves a background open are padded with
/// that background out to `width`: tmux trims erase-painted EOL cells but
/// leaves the transition codes behind, so without the pad the fill renders
/// as a ragged bar instead of reaching the pane edge like the real pane.
pub fn preview_text(raw: &str, width: usize) -> ratatui::text::Text<'static> {
    let mut text = match raw.as_bytes().into_text() {
        Ok(text) => text,
        Err(_) => return ratatui::text::Text::from(strip_ansi(raw)),
    };
    if width == 0 {
        return text;
    }
    for (line, raw_line) in text.lines.iter_mut().zip(raw.lines()) {
        let Some(bg) = trailing_bg(raw_line) else {
            continue;
        };
        let w = line.width();
        if w < width {
            line.spans
                .push(Span::styled(" ".repeat(width - w), Style::default().bg(bg)));
        }
    }
    text
}

/// Background left open by a line's trailing SGR-only tail (`Some`), or
/// `None` when the tail closes the background (`0`/`49`) or there is no
/// SGR tail at all. Only the suffix matters: a reset earlier in the tail
/// is superseded by a later bg set, matching terminal state semantics.
fn trailing_bg(line: &str) -> Option<Color> {
    let bytes = line.as_bytes();
    let mut end = bytes.len();
    let mut codes: Vec<&str> = Vec::new();
    loop {
        if end == 0 || bytes[end - 1] != b'm' {
            break;
        }
        let mut i = end - 1;
        while i > 0 && (bytes[i - 1].is_ascii_digit() || bytes[i - 1] == b';') {
            i -= 1;
        }
        if i < 2 || bytes[i - 2] != 0x1b || bytes[i - 1] != b'[' {
            break;
        }
        codes.push(&line[i..end - 1]);
        end = i - 2;
    }
    codes.reverse();
    let mut bg: Option<Color> = None;
    for params in codes {
        let nums: Vec<u16> = if params.is_empty() {
            vec![0]
        } else {
            params.split(';').map(|p| p.parse().unwrap_or(0)).collect()
        };
        let mut j = 0;
        while j < nums.len() {
            match nums[j] {
                0 => bg = None,
                49 => bg = None,
                40..=47 => bg = Some(ansi_bg(nums[j] - 40)),
                100..=107 => bg = Some(ansi_bg(nums[j] - 100 + 8)),
                48 if j + 4 < nums.len() && nums[j + 1] == 2 => {
                    bg = Some(Color::Rgb(
                        nums[j + 2].min(255) as u8,
                        nums[j + 3].min(255) as u8,
                        nums[j + 4].min(255) as u8,
                    ));
                    j += 4;
                }
                48 if j + 2 < nums.len() && nums[j + 1] == 5 => {
                    bg = Some(Color::Indexed(nums[j + 2].min(255) as u8));
                    j += 2;
                }
                _ => {}
            }
            j += 1;
        }
    }
    bg
}

fn ansi_bg(n: u16) -> Color {
    match n {
        0 => Color::Black,
        1 => Color::Red,
        2 => Color::Green,
        3 => Color::Yellow,
        4 => Color::Blue,
        5 => Color::Magenta,
        6 => Color::Cyan,
        7 => Color::Gray,
        8 => Color::DarkGray,
        9 => Color::LightRed,
        10 => Color::LightGreen,
        11 => Color::LightYellow,
        12 => Color::LightBlue,
        13 => Color::LightMagenta,
        14 => Color::LightCyan,
        _ => Color::White,
    }
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next();
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            Some('(') | Some(')') => {
                chars.next();
                chars.next();
            }
            Some(_) => {
                chars.next();
            }
            None => {}
        }
    }
    out
}

/// Mouse-hover row style: bold only, no background — safe on every theme
/// (a bg would swallow dim cells on dark themes) and distinct from the
/// bg-based selection.
fn hover_style() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

/// Panes per window, keyed by tmux window id: drives the conditional
/// `:[N]` suffix and `*` mark — both are noise in single-pane windows.
fn window_pane_counts(panes: &[Pane]) -> HashMap<&str, usize> {
    let mut counts = HashMap::new();
    for p in panes {
        *counts.entry(p.window_id.as_str()).or_insert(0) += 1;
    }
    counts
}

/// Bare name in single-pane windows; `name:[N]` where panes share one.
pub fn window_label(window_name: &str, index: u32, panes_in_window: usize) -> String {
    if panes_in_window > 1 {
        format!("{window_name}:[{index}]")
    } else {
        window_name.to_string()
    }
}

/// Display id for the Pane column. The full ids stay namespaced
/// (`konsole:<pid>:<sess>`, `host:%N`) so actions address the right row,
/// but the namespace is already named by the Session column: showing the
/// raw id would need an 18-wide column, and clipping it collapses every
/// Konsole tab onto the same `konsol`. The backend-local id is unique
/// within its session, which is what this column needs. tmux ids (`%N`)
/// never contain `:` and pass through unchanged.
pub fn pane_cell_id(id: &str) -> String {
    if id.starts_with('%') {
        return id.to_string();
    }
    if let Some((_, local)) = crate::native::split_native_id(id) {
        return local.to_string();
    }
    // Fleet rows: `host:%N`. The host is in the Session column
    // (`label ▸ session`); keep the pane part.
    match id.rsplit_once(':') {
        Some((_, pane)) if pane.starts_with('%') => pane.to_string(),
        _ => id.to_string(),
    }
}

/// `!` needs you; `+` is space-marked for bulk ops; `*` is the focused
/// pane, shown only where a window holds several panes and focus is
/// actually ambiguous. Attention always wins over the mark.
/// The mark column, most urgent first: a pane that still needs you
/// keeps its `!` even after a visit (the tray still counts it), then the
/// manual `+` and the focus `*`; `\u{2713}` is only for rows with nothing
/// louder to say — it answers "have I looked at everything yet".
pub fn row_mark(
    is_needy: bool,
    marked: bool,
    active: bool,
    panes_in_window: usize,
    attended: bool,
) -> &'static str {
    if is_needy {
        "!"
    } else if marked {
        "+"
    } else if active && panes_in_window > 1 {
        "*"
    } else if attended {
        "\u{2713}"
    } else {
        " "
    }
}

/// One selector segment's parts: (marker, name, counts, attached, dot).
/// The dot is the session's worst live state; idle/unknown-only tabs
/// stay dotless so the bar reads as an exception list, not wallpaper.
fn selector_segments(
    app: &App,
) -> Vec<(
    String,
    String,
    String,
    bool,
    Option<crate::agent::AgentState>,
)> {
    use crate::agent::AgentState;
    let dot = |worst: Option<AgentState>| match worst {
        Some(
            AgentState::Waiting
            | AgentState::Stuck
            | AgentState::Error
            | AgentState::Limited
            | AgentState::Working,
        ) => worst,
        _ => None,
    };
    let counts = app.session_counts();
    let mut rows = Vec::with_capacity(app.sessions.len() + 1);
    let total_needy = app.attention_count();
    let all_counts = if total_needy > 0 {
        format!("({}·{}!)", app.panes.len(), total_needy)
    } else {
        format!("({})", app.panes.len())
    };
    rows.push((
        "".to_string(),
        "All".to_string(),
        all_counts,
        false,
        dot(app.session_worst(None)),
    ));
    for s in &app.sessions {
        let (panes, needy) = counts.get(&s.name).copied().unwrap_or((0, 0));
        let count_text = if needy > 0 {
            format!("({panes}·{needy}!)")
        } else {
            format!("({panes})")
        };
        let mark = if s.attached { "●" } else { "○" };
        rows.push((
            mark.to_string(),
            s.name.clone(),
            count_text,
            s.attached,
            dot(app.session_worst(Some(&s.name))),
        ));
    }
    rows
}

fn seg_text(mark: &str, name: &str, counts: &str) -> String {
    if mark.is_empty() {
        format!("{name} {counts}")
    } else {
        format!("{mark} {name} {counts}")
    }
}

/// Single-line session selector. Scrolls horizontally (`session_scroll` is
/// the first visible segment) so long session lists collapse to `‹ … ›`.
/// Returns the bar area plus one click rect per visible segment, in order.
fn render_selector(
    frame: &mut Frame,
    app: &mut App,
    pal: &Palette,
    area: Rect,
) -> (Rect, Vec<Rect>) {
    let segs = selector_segments(app);
    let width = area.width as usize;
    // Record/share mode blurs the session names in the bar too — the tab
    // is identity, not state. The counts and the dots stay: "how many
    // and which state" is exactly what the recording should show. It is
    // computed up front so the width closure below can measure the
    // blurred label (a `█` per char) instead of the live name, keeping
    // the scroll-window math honest.
    let blur = app.config.blur_cols;
    // "› " focus flag, " │ " separators, "‹ " / " ›" overflow markers.
    const FLAG: usize = 2;
    const SEP: usize = 3;
    let seg_width = |i: usize| {
        let (mark, name, counts, _, dot) = &segs[i];
        // Truncate first, then blur the truncated name: the fill must
        // match the width the live tab would take (20 chars, ellipsis
        // when longer), so the scroll window math is identical whether
        // or not the toggle is on. Blurring the raw name would overfit
        // wide tabs into the overflow and drop the scroll behaviour.
        let name_trunc = truncate(name, 20);
        let name_shown = if blur {
            blur_cell(&name_trunc)
        } else {
            name_trunc
        };
        seg_text(mark, &name_shown, counts).chars().count() + if dot.is_some() { 2 } else { 0 }
    };
    // Slide the window until the selection fits on screen.
    if app.selected < app.session_scroll {
        app.session_scroll = app.selected;
    }
    loop {
        // Flag + room for " ›", plus "‹ " once scrolled.
        let mut used = FLAG + 2 + if app.session_scroll > 0 { 2 } else { 0 };
        let mut end = app.session_scroll;
        while end < segs.len() && used + seg_width(end) + SEP <= width {
            used += seg_width(end) + SEP;
            end += 1;
        }
        if app.selected < end || app.session_scroll >= app.selected {
            break;
        }
        app.session_scroll += 1;
    }
    let start = app.session_scroll.min(segs.len().saturating_sub(1));
    app.session_scroll = start;

    let mut spans = vec![Span::styled(
        "› ",
        if app.focus == Focus::Sessions {
            pal.focus
        } else {
            pal.dim
        },
    )];
    let mut rects = Vec::new();
    let mut x = area.x + FLAG as u16;
    if start > 0 {
        spans.push(Span::styled("‹ ", pal.dim));
        x += 2;
    }
    let mut used = (x - area.x) as usize + 2; // room for a possible " ›"
    for (offset, i) in (start..segs.len()).enumerate() {
        let (mark, name, counts, attached, dot) = &segs[i];
        // Same truncate-then-blur contract as the width closure above,
        // so a tab's rendered width equals the width the scroll window
        // reserved for it — the bar never reflows on the toggle.
        let name_trunc = truncate(name, 20);
        let name_display = if blur {
            blur_cell(&name_trunc)
        } else {
            name_trunc
        };
        let text = seg_text(mark, &name_display, counts);
        let w = text.chars().count() + if dot.is_some() { 2 } else { 0 };
        if used + w + SEP > width {
            break;
        }
        if offset > 0 {
            spans.push(Span::styled(" │ ", pal.dim));
            x += SEP as u16;
        }
        let needy = counts.contains('!');
        let base = if i == app.selected {
            pal.selected
        } else if *attached && app.config.highlight_attached {
            pal.attached
        } else {
            Style::default()
        };
        let overlay = if needy {
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        } else {
            pal.dim
        };
        // Selected keeps its background; the overlay only sets fg/flags.
        spans.push(Span::styled(text, base.patch(overlay)));
        if let Some(worst) = dot {
            spans.push(Span::styled(" ●", base.patch(state_style(*worst, pal))));
        }
        rects.push(Rect {
            x,
            y: area.y,
            width: w as u16,
            height: 1,
        });
        x += w as u16;
        used += w + SEP;
    }
    if start + rects.len() < segs.len() {
        spans.push(Span::styled(" ›", pal.dim));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
    (
        Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: 1,
        },
        rects,
    )
}

fn state_style(state: crate::agent::AgentState, pal: &Palette) -> Style {
    use crate::agent::AgentState;
    match state {
        AgentState::Waiting | AgentState::Error | AgentState::Stuck | AgentState::Limited => {
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        }
        // Done: quiet dim-green check — finished, not waiting, not
        // alarming. Shares the working hue so a finished task still
        // reads as "complete and good" instead of "fallen silent".
        AgentState::Done => Style::default()
            .fg(pal.done.fg.unwrap_or(Color::Green))
            .add_modifier(Modifier::BOLD),
        AgentState::Working => Style::default().fg(Color::Green),
        AgentState::Idle | AgentState::Unknown => pal.dim,
    }
}

/// Agent cell text style: per-agent override wins, else the name hashes
/// into the palette (custom or theme default) so the same agent reads as
/// a group; unnamed or disabled falls back to `dim`.
fn agent_text_style(name: &str, cfg: &crate::config::AppConfig, dim: Style) -> Style {
    if name.is_empty() || !cfg.color_agents {
        return dim;
    }
    if let Some(spec) = cfg.agent_styles.get(&name.to_lowercase()) {
        return crate::config::parse_style_spec(spec);
    }
    let palette = agent_palette(cfg);
    let idx = crate::fnv1a(&name.to_lowercase()) as usize % palette.len();
    crate::config::parse_style_spec(palette[idx])
}

/// Palette the agent hash picks from: custom list, else brights on dark
/// themes (only `light` needs the dark set).
fn agent_palette(cfg: &crate::config::AppConfig) -> Vec<&str> {
    if !cfg.agent_palette.is_empty() {
        return cfg.agent_palette.iter().map(String::as_str).collect();
    }
    if cfg.palette() == "light" {
        vec!["red", "green", "blue", "magenta", "cyan", "black"]
    } else {
        vec![
            "bright-cyan",
            "bright-magenta",
            "bright-green",
            "bright-yellow",
            "bright-blue",
            "bright-red",
        ]
    }
}

/// Idle cell: duration text plus warn tints — working with no visible
/// change past the stuck threshold (the stuck smell), and idle past
/// `idle_flag_secs` (a day-scale idle stops looking fine). Waiting is
/// quiet by nature, so only working and idle warn.
fn idle_cell(
    rec: Option<&crate::agent::AgentRecord>,
    activity: u64,
    now: u64,
    cfg: &crate::config::AppConfig,
) -> (String, bool) {
    let (text, age) = match rec {
        Some(r) if r.quiet_since > 0 => {
            let age = now.saturating_sub(r.quiet_since);
            (crate::history::fmt_dur(age), Some(age))
        }
        _ if activity == 0 => ("—".to_string(), None),
        _ => {
            let age = now.saturating_sub(activity);
            (crate::history::fmt_dur(age), Some(age))
        }
    };
    let warn = cfg.idle_warn
        && age.is_some_and(|a| {
            let state = rec.map(|r| r.state);
            (matches!(state, Some(crate::agent::AgentState::Working))
                && cfg.stuck_secs > 0
                && a > cfg.stuck_secs)
                || (matches!(state, Some(crate::agent::AgentState::Idle))
                    && cfg.idle_flag_secs > 0
                    && a > cfg.idle_flag_secs)
        });
    (text, warn)
}

fn render_table(frame: &mut Frame, app: &App, pal: &Palette, area: Rect) -> (Rect, Vec<HeaderHit>) {
    let compact = app.config.compact();
    // Filtered to one session: its column would repeat on every row.
    let show_session = app.side_session().is_none();
    let needy = app.visible_needy();
    let idle = app.visible_idle();
    let mut title = format!(" terhub — {} pane(s)", app.filtered.len());
    if !app.agents_live {
        title.push_str(" · daemon down");
    }
    if let Some(s) = app.side_session() {
        title.push_str(&format!(" · {} only", truncate(&s.name, 16)));
    }
    if needy > 0 {
        title.push_str(&format!(" · {needy} need you"));
    }
    if idle > 0 {
        title.push_str(&format!(" · {idle} idle"));
    }
    if !app.marked.is_empty() {
        title.push_str(&format!(" · {} marked", app.marked.len()));
    }
    if app.hidden_quiet > 0 {
        title.push_str(&format!(" · hiding {}", app.hidden_quiet));
    }
    if app.agents_live
        && !app
            .agents
            .values()
            .any(|r| r.source == crate::agent::Source::Hook)
    {
        title.push_str(" · no hooks yet (GUIDE: Hooks)");
    }
    title.push_str(&format!(
        " · {} · {} ",
        app.config.palette(),
        app.config.density_name(),
    ));
    // The active sort column can be hidden (Session while filtered to one
    // session, Summary in compact mode): say so here instead of dropping
    // the indicator silently. (The trailing space above separates this.)
    let sort_visible = (app.sort_key != SortKey::Session || show_session)
        && (app.sort_key != SortKey::Summary || !compact);
    if !sort_visible {
        title.push_str(&format!(
            "· sorted by {:?} {}",
            app.sort_key,
            if app.sort_asc { '▲' } else { '▼' }
        ));
    }

    let now = crate::agent::now_unix();
    let warn_style = crate::config::parse_style_spec(&app.config.idle_warn_style);
    // The filter chip rides the title line when there is room for both.
    // On a narrow terminal the title alone fills the border, so the chip
    // drops onto its own header line above the table: a chip clipped
    // mid-label is exactly the "I forgot I was filtering" this
    // indicator exists to prevent. So measure how wide the chip *wants*
    // to be before choosing where it goes, and give the label exactly
    // the room that choice leaves.
    let inner_w = area.width.saturating_sub(2) as usize;
    // Room left on the title line: the border width minus the title text
    // minus the separator ratatui inserts between two titles.
    let inline_room = inner_w
        .saturating_sub(title.chars().count())
        .saturating_sub(2);
    let own_row = filter_chip_label(app).is_some_and(|label| {
        // block + space + label
        inline_room < label.chars().count() + 2
    });
    if own_row && let Some(label) = &filter_chip_label(app) {
        frame.render_widget(
            filter_chip(label, pal, inner_w.saturating_sub(2)),
            Rect {
                x: area.x,
                y: area.y,
                width: area.width,
                height: 1,
            },
        );
    }
    // The table shrinks by the chip's row when the chip sits on its own
    // line; everything below keys off `area`, so rebind it once here.
    let area = if own_row {
        Rect {
            x: area.x,
            y: area.y + 1,
            width: area.width,
            height: area.height.saturating_sub(1),
        }
    } else {
        area
    };
    let visible = area.height.saturating_sub(3) as usize;
    // Window sizes come from the full pane list: stable under filtering,
    // so a suffix never flickers when a sibling is filtered out.
    let counts = window_pane_counts(&app.panes);
    let rows: Vec<Row> = app
        .filtered
        .iter()
        .enumerate()
        .skip(app.pane_scroll)
        .take(visible)
        .map(|(i, p)| {
            let rec = app.agents.get(&p.id);
            let is_needy = rec.is_some_and(|r| r.state.needs_attention());
            let siblings = counts.get(p.window_id.as_str()).copied().unwrap_or(1);
            let mark = row_mark(
                is_needy,
                app.marked.contains(&p.id),
                p.active,
                siblings,
                app.is_attended(&p.id),
            );
            let mark_style = if is_needy {
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
            } else {
                pal.dim
            };
            let (agent, agent_style) = match rec {
                Some(r) => {
                    let mut st = agent_text_style(&r.agent, &app.config, pal.dim);
                    if is_needy {
                        st = st.add_modifier(Modifier::BOLD);
                    }
                    (r.agent.clone(), st)
                }
                None => ("—".to_string(), pal.dim),
            };
            let handle = match rec {
                Some(r) if !r.handle.is_empty() => truncate(&r.handle, 14),
                _ => "—".to_string(),
            };
            // No daemon record: no agent, so no agent-reported state.
            // Unclaimed panes that have been quiet past the dormancy
            // threshold show as `idle` so the user can see dormancy
            // without the daemon; panes with unknown or recent activity
            // stay as the plain `—` (no agent, no judgement). The Agent
            // and Src columns still read `—` so the row clearly has no
            // agent — this is "dormant shell", not "finished agent".
            let (state, state_style) = match rec {
                Some(r) => (r.state.label().to_string(), state_style(r.state, pal)),
                None if is_quiet_shell(None, p.activity, now) => ("idle".to_string(), pal.dim),
                None => ("—".to_string(), pal.dim),
            };
            let age = match rec {
                // A zero timestamp means "never recorded", not the epoch:
                // without the guard this renders as 20737d. (Same shape
                // as the `turn` guard below.)
                Some(r) if r.state_since > 0 => crate::history::fmt_dur(r.waiting_secs(now)),
                _ => "—".to_string(),
            };
            let turn = match rec {
                Some(r) if r.last_turn_secs > 0 => crate::history::fmt_dur(r.last_turn_secs),
                _ => "—".to_string(),
            };
            // Claimed panes use the visible-quiet clock: tmux activity
            // counts every repaint, so animated agent screens would read
            // busy forever. Unclaimed panes fall back to window activity.
            let (act, act_warn) = idle_cell(rec, p.activity, now, &app.config);
            let window = window_label(&p.window_name, p.index, siblings);
            let dir = p.path.rsplit('/').next().unwrap_or("");
            let dir = if dir.is_empty() { "—" } else { dir };
            // hook = agent-reported fact, scr = screen guess: how much to
            // trust the state.
            let (src, src_style) = match rec {
                Some(r) if r.source == crate::agent::Source::Hook => {
                    ("hook".to_string(), pal.attached)
                }
                Some(r) if r.source == crate::agent::Source::Proc => {
                    ("proc".to_string(), pal.attached)
                }
                Some(r) if r.source == crate::agent::Source::Manual => {
                    ("man".to_string(), pal.attached)
                }
                Some(_) => ("scr".to_string(), pal.dim),
                None => ("—".to_string(), pal.dim),
            };
            // The observer's own row wears the attached accent on its id
            // instead of another dim entry.
            let id_style = if app.own_pane.as_deref() == Some(p.id.as_str()) {
                pal.attached
            } else {
                pal.dim
            };
            let mut cells = vec![
                Cell::from(mark).style(mark_style),
                Cell::from(pane_cell_id(&p.id)).style(id_style),
            ];
            // Record/share mode: replace the identity-revealing values
            // with a solid fill of the same width. The fill keeps the
            // table layout and every non-sensitive column intact; only
            // the text that would reveal a repo, session, command, or
            // prompt is gone. State, Src, the time columns, and the
            // mark column stay readable — that is the point.
            let blur = app.config.blur_cols;
            let session_cell = if show_session {
                let s = truncate(&p.session, 18);
                if blur { blur_cell(&s) } else { s }
            } else {
                String::new()
            };
            let window_cell = if blur {
                blur_cell(&truncate(&window, 20))
            } else {
                truncate(&window, 20)
            };
            let cmd_cell = if blur {
                blur_cell(&truncate(&p.command, if compact { 16 } else { 32 }))
            } else {
                truncate(&p.command, if compact { 16 } else { 32 })
            };
            let dir_cell = if blur {
                blur_cell(&truncate(dir, 24))
            } else {
                truncate(dir, 24)
            };
            let agent_cell = if blur {
                blur_cell(&agent)
            } else {
                agent.clone()
            };
            let handle_cell = if blur {
                blur_cell(&handle)
            } else {
                handle.clone()
            };
            if show_session {
                // Sessions are top-level categories: the title hue
                // without its bold, so the family ties without shouting.
                // A blurred session is a solid fill in the same hue —
                // the column shape survives, only the name is gone.
                cells.push(
                    Cell::from(session_cell)
                        .style(Style::default().fg(pal.title.fg.unwrap_or(Color::Reset))),
                );
            }
            cells.extend([
                // Window is sub-structure: recede it so session/cmd read first.
                Cell::from(window_cell).style(pal.dim),
                // Cmd is primary content (what runs): full status ink so
                // it reads the same on every terminal instead of
                // inheriting whatever the terminal's default fg is.
                Cell::from(cmd_cell)
                    .style(Style::default().fg(pal.status.fg.unwrap_or(Color::Reset))),
                Cell::from(dir_cell).style(if dir.contains("(deleted)") {
                    // A vanished cwd reads struck-through, still legible.
                    pal.dim.add_modifier(Modifier::CROSSED_OUT)
                } else {
                    pal.dim
                }),
                Cell::from(agent_cell).style(agent_style),
                // Handle groups with its agent: same hash colour, so the
                // eye lands the pair as one owner instead of two greys.
                Cell::from(handle_cell).style(
                    rec.filter(|r| !r.handle.is_empty())
                        .map(|r| agent_text_style(&r.agent, &app.config, pal.dim))
                        .unwrap_or(pal.dim),
                ),
                Cell::from(state).style(state_style),
                Cell::from(src).style(src_style),
                Cell::from(age).style(pal.dim),
                Cell::from(turn).style(pal.dim),
                Cell::from(act).style(if act_warn { warn_style } else { pal.dim }),
            ]);
            // Optional activity sparkline: 6 wide cells, rightmost
            // freshest, height rises with bytes/tick. Only when the
            // user opts in via `[ui] activity_sparkline = true` — a
            // wide-terminal feature, so default off.
            if app.config.activity_sparkline {
                cells.push(
                    Cell::from(sparkline_glyphs(&app.sparkline_buckets(&p.id))).style(pal.dim),
                );
            }
            if !compact {
                let summary = rec.map(|r| r.summary.as_str()).unwrap_or("");
                // When the row's state is Idle and the summary is the
                // "turn just ended" event the agent reports together
                // with that transition ("response finished"), the raw
                // text reads as a contradiction: the user sees idle
                // beside a string that names a working-flavored reason.
                // The two are actually consistent — the agent went
                // quiet after that action — but the column relationship
                // is invisible. Prefix a `↘` so the transition is
                // visible at a glance: "this summary is the cause of
                // the current state, not a description of the moment".
                // Skip if already marked (idempotent across re-renders)
                // and skip on any other state — the marker is
                // specifically about the Working→Idle drop.
                let summary =
                    if matches!(rec.map(|r| r.state), Some(crate::agent::AgentState::Idle))
                        && summary == "response finished"
                        && !summary.starts_with('\u{2198}')
                    {
                        format!("\u{2198} {summary}")
                    } else {
                        summary.to_string()
                    };
                let summary = if blur { blur_cell(&summary) } else { summary };
                cells.push(Cell::from(truncate(&summary, 60)).style(pal.dim));
            }
            let mut row = Row::new(cells);
            if i == app.pane_selected {
                row = row.style(pal.selected);
            } else if Some(i) == app.hover {
                row = row.style(hover_style());
            }
            row
        })
        .collect();
    let mut headers = vec![String::new(), "Pane".to_string()];
    let mut keys: Vec<Option<SortKey>> = vec![None, Some(SortKey::Pane)];
    // The Pane column grows only when namespaced ids need it: a bare
    // `%N` fits 6, while `konsole:<pid>:<sess>` shows its local part
    // (`1535082:6`). tmux-only servers keep the 6-wide column, so the
    // wider rows never steal width from Summary.
    let pane_w = app
        .filtered
        .iter()
        .map(|p| pane_cell_id(&p.id).chars().count())
        .max()
        .unwrap_or(3)
        .clamp(6, 12);
    let mut widths = vec![Constraint::Length(1), Constraint::Length(pane_w as u16)];
    if show_session {
        headers.push("Session".to_string());
        keys.push(Some(SortKey::Session));
        widths.push(Constraint::Percentage(9));
    }
    headers.extend(
        [
            "Window", "Cmd", "Dir", "Agent", "Handle", "State", "Src", "For", "Turn", "Idle",
        ]
        .map(str::to_string),
    );
    keys.extend(
        [
            SortKey::Window,
            SortKey::Cmd,
            SortKey::Dir,
            SortKey::Agent,
            SortKey::Handle,
            SortKey::State,
            SortKey::Src,
            SortKey::For,
            SortKey::Turn,
            SortKey::Idle,
        ]
        .map(Some),
    );
    // Window/Cmd/Dir/Agent = 43%; with Session 9% + Summary 16% the
    // percentage columns total 68%, so with the fixed Handle column
    // (12) the 195-wide table still fits versioned binaries uncut and
    // only Summary absorbs the deficit at narrower widths.
    widths.extend([
        Constraint::Percentage(9),
        Constraint::Percentage(13),
        Constraint::Percentage(13),
        Constraint::Percentage(8),
        Constraint::Length(12),
        Constraint::Length(8),
        Constraint::Length(4),
        Constraint::Length(6),
        Constraint::Length(6),
        Constraint::Length(6),
    ]);
    if !compact {
        headers.push("Summary".to_string());
        keys.push(Some(SortKey::Summary));
        widths.push(Constraint::Percentage(16));
    }
    if app.config.activity_sparkline {
        // Sparkline column has no sort key — the data is a small
        // rolling window, not an orderable property of the pane.
        headers.push("Activity".to_string());
        keys.push(None);
        widths.push(Constraint::Length(7));
    }
    if let Some(i) = keys.iter().position(|k| *k == Some(app.sort_key)) {
        headers[i].push(if app.sort_asc { '▲' } else { '▼' });
    }
    let spacing = if compact { 0 } else { 1 };
    // Header hit cells: the same Layout split Table uses for its columns.
    let hits: Vec<HeaderHit> = Layout::horizontal(widths.clone())
        .spacing(spacing)
        .split(Rect {
            x: area.x + 1,
            y: area.y + 1,
            width: area.width.saturating_sub(2),
            height: 1,
        })
        .iter()
        .zip(keys.iter())
        .map(|(r, k)| HeaderHit { area: *r, key: *k })
        .collect();
    let header = Row::new(headers).style(pal.header).height(1);
    let border = if app.focus == Focus::Panes {
        pal.focus
    } else {
        pal.border
    };
    let mut block = Block::bordered()
        .title(Line::from(title).style(pal.title))
        .border_style(border);
    if !own_row && let Some(label) = filter_chip_label(app) {
        block = block.title(filter_chip(&label, pal, inline_room));
    }
    let table = Table::new(rows, widths)
        .header(header)
        .block(block)
        .column_spacing(spacing);
    frame.render_widget(table, area);

    // Rows live below the top border + header row, inside the side borders.
    let rows_rect = Rect {
        x: area.x + 1,
        y: area.y + 2,
        width: area.width.saturating_sub(2),
        height: (app
            .filtered
            .len()
            .saturating_sub(app.pane_scroll)
            .min(visible)) as u16,
    };
    (rows_rect, hits)
}

fn render_preview(frame: &mut Frame, app: &mut App, pal: &Palette, area: Rect) {
    let Some(p) = app.selected_pane() else {
        return;
    };
    frame.render_widget(Clear, area);
    let rec = app.agents.get(&p.id);
    let agent = rec
        .map(|r| r.agent.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("—");
    let state = rec.map(|r| r.state.label()).unwrap_or("—");
    let now = crate::agent::now_unix();
    let (idle, _) = idle_cell(rec, p.activity, now, &app.config);
    let mut title = format!(
        " {} · {}:{} · {agent} {state} {idle}",
        p.id, p.session, p.window_name,
    );
    // Model/ctx strip: hook-reported facts about what the agent runs.
    if let Some(r) = rec {
        if !r.model.is_empty() {
            title.push_str(&format!(" · {}", r.model));
        }
        if let Some(ctx) = r.ctx_pct {
            title.push_str(&format!(" · ctx {ctx}%"));
        }
    }
    if let Some(pct) = app.preview_cpu_pct {
        title.push_str(&format!(" · cpu {pct:.0}%"));
    }
    if let Some(g) = &app.preview_git_line {
        title.push_str(&format!(" · {g}"));
    }
    title.push_str(&format!(
        " — left-click/{} jump · right-click/esc close ",
        app.config.key_label("attach")
    ));
    let block = Block::bordered()
        .title(Line::from(title).style(pal.title))
        .border_style(pal.focus);
    let inner = block.inner(area);
    app.preview_visible = inner.height as usize;
    let total = app.preview_line_count();
    frame.render_widget(block, area);
    let offset = app.preview_scroll.min(app.preview_max_scroll());
    app.preview_scroll = offset;
    frame.render_widget(
        Paragraph::new(preview_text(
            &app.preview,
            app.preview_width.unwrap_or(inner.width as usize),
        ))
        .wrap(Wrap { trim: false })
        .scroll((offset as u16, 0))
        .style(pal.status),
        inner,
    );
    if total > inner.height as usize && inner.height > 0 {
        let pos = format!(" {offset}/{total} ");
        let x = inner
            .x
            .saturating_add(inner.width.saturating_sub(pos.len() as u16 + 1));
        let y = inner.y.saturating_add(inner.height.saturating_sub(1));
        frame.render_widget(
            Paragraph::new(Line::from(pos).style(pal.dim)).alignment(Alignment::Right),
            Rect::new(x, y, inner.width.min(16), 1),
        );
    }
}

/// Sample mini-table for the Theme/Density sections: fixed rows in the
/// previewed theme + density, so styles can be judged on an empty
/// setup. Mirrors the main table's text styling (title, header, a
/// selected row, an attached agent, a dim idle row) without touching
/// live data. The summary cell follows the previewed density, so
/// comfortable vs compact reads in the sample too.
fn settings_preview_lines(theme: &str, comfortable: bool) -> Vec<Line<'_>> {
    let pal = palette(theme);
    let lines = vec![
        Line::from(Span::styled(
            format!(" terhub — 3 pane(s) · {theme} "),
            pal.title,
        )),
        Line::from(Span::styled(" Pane  Agent   State    Idle", pal.header)),
        Line::from(Span::styled(
            if comfortable {
                "%1    pi      working  3m    turn started"
            } else {
                "%1    pi      working  3m"
            },
            pal.selected,
        )),
        Line::from(vec![
            Span::raw("%2    "),
            Span::styled("claude  ", pal.attached),
            Span::raw("waiting  12m"),
            Span::styled(if comfortable { "  approve deploy?" } else { "" }, pal.dim),
        ]),
        Line::from(Span::styled("%3    —       idle     2h", pal.dim)),
    ];
    lines
}

fn render_settings(
    frame: &mut Frame,
    app: &mut App,
    pal: &Palette,
    area: Rect,
) -> (Rect, Vec<Rect>) {
    use crate::app::SettingRow;
    let rows = app.settings_rows();
    let sections = app.settings_sections();
    let sec = app.settings_section.min(sections.len().saturating_sub(1));
    let (sec_name, sec_rows) = sections
        .get(sec)
        .map(|(n, r)| (*n, r.clone()))
        .unwrap_or(("", Vec::new()));
    // Selected item's global row, for scroll-into-view (only when the
    // selection lives in the displayed section).
    let mut ordinal = 0;
    let mut sel_row = 0;
    let mut sel_here = false;
    for (i, r) in rows.iter().enumerate() {
        if matches!(r, SettingRow::Item { .. }) {
            if ordinal == app.settings_selected {
                sel_row = i;
                sel_here = sec_rows.contains(&i);
            }
            ordinal += 1;
        }
    }
    // Content window into this section's rows: the scroll is a global
    // row; the offset inside the section is what the pane shows.
    let sec_start = sec_rows.first().copied().unwrap_or(0);
    // Full-page layout: sidebar for sections, rest for content.
    let side_w: u16 = sections
        .iter()
        .map(|(n, _)| n.chars().count() as u16)
        .max()
        .unwrap_or(8)
        + 4;
    let cols = Layout::horizontal([Constraint::Length(side_w), Constraint::Min(0)]).split(area);
    let visible = cols[1].height.saturating_sub(2) as usize;
    let mut offset = app.settings_scroll.saturating_sub(sec_start);
    if sel_here {
        let pos = sel_row.saturating_sub(sec_start);
        if pos < offset {
            offset = pos;
        } else if visible > 0 && pos >= offset + visible {
            offset = pos + 1 - visible;
        }
        app.settings_scroll = sec_start + offset;
    }
    let max_off = sec_rows.len().saturating_sub(visible);
    offset = offset.min(max_off);

    let mut title = format!(" settings — {sec_name} — enter apply · esc close · ←/→ section ");
    if offset > 0 {
        title.push('▲');
    }
    if offset + visible < sec_rows.len() {
        title.push('▼');
    }
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .title(Line::from(title).style(pal.title))
        .border_style(pal.focus);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    // Sidebar: every section, active marked. Content starts right of it.
    let side = Rect {
        x: inner.x,
        y: inner.y,
        width: side_w.saturating_sub(1),
        height: inner.height,
    };
    let content = Rect {
        x: inner.x + side_w,
        y: inner.y,
        width: inner.width.saturating_sub(side_w),
        height: inner.height,
    };
    let side_lines: Vec<Line> = sections
        .iter()
        .enumerate()
        .map(|(i, (name, _))| {
            if i == sec {
                Line::from(vec![
                    Span::styled("› ", pal.focus),
                    Span::styled((*name).to_string(), pal.focus),
                ])
            } else {
                Line::from(vec![Span::styled(format!("  {name}"), pal.dim)])
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(side_lines), side);
    // Divider between sidebar and content.
    for y in inner.y..inner.y + inner.height {
        frame.render_widget(
            Paragraph::new(Span::styled("│", pal.border)),
            Rect {
                x: inner.x + side_w - 1,
                y,
                width: 1,
                height: 1,
            },
        );
    }
    // Live style preview beside Theme/Density: a fixed options column
    // plus a sample box rendered in the hovered theme + density, so an
    // empty setup can still judge styles. Other sections keep the full
    // width; narrow terminals skip the preview entirely.
    let want_preview = matches!(sec_name, "Theme" | "Density");
    let (opts, prev) = if want_preview && content.width >= 72 {
        let parts = Layout::horizontal([Constraint::Length(38), Constraint::Min(0)]).split(content);
        (parts[0], parts[1])
    } else {
        (content, Rect::default())
    };
    let show_preview = prev.width > 0;
    let lines: Vec<Line> = sec_rows
        .iter()
        .skip(offset)
        .take(visible)
        .map(|&i| {
            let r = &rows[i];
            match r {
                SettingRow::Header(name) => {
                    Line::from(Span::styled(format!(" {name}"), pal.header))
                }
                SettingRow::Item { label, active, .. } => {
                    let sel = i == sel_row;
                    let mut spans = vec![Span::styled(
                        if sel { "› " } else { "  " },
                        if sel { pal.focus } else { pal.dim },
                    )];
                    spans.push(Span::raw(label.clone()));
                    if *active {
                        spans.push(Span::styled(" ●", pal.attached));
                    }
                    let mut line = Line::from(spans);
                    if sel {
                        line.style = pal.selected;
                    }
                    line
                }
                SettingRow::Legend {
                    triangle,
                    color,
                    name,
                    meaning,
                } => {
                    // The legend mirrors the panel: a coloured ▼ in the
                    // same fill the tray renders, then the state name in
                    // bold, then a one-line meaning. Theme-driven, so what
                    // you read here is what you see in the taskbar.
                    let spans = vec![
                        Span::raw("  "),
                        Span::styled(
                            format!("{triangle} "),
                            crate::config::parse_style_spec(color)
                                .fg
                                .unwrap_or(ratatui::style::Color::Reset),
                        ),
                        Span::styled(
                            format!("{name} "),
                            Style::default().add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(format!("— {meaning}"), pal.dim),
                    ];
                    Line::from(spans)
                }
            }
        })
        .collect();
    let shown = lines.len();
    frame.render_widget(Paragraph::new(lines), opts);
    if show_preview {
        let (pv_theme, pv_comfy) = app.settings_preview_style();
        let density = if pv_comfy { "comfortable" } else { "compact" };
        let pblock = Block::bordered()
            .title(Line::from(format!("preview: {pv_theme} · {density}")).style(pal.title))
            .border_style(pal.border);
        let pinner = pblock.inner(prev);
        frame.render_widget(pblock, prev);
        frame.render_widget(
            Paragraph::new(settings_preview_lines(&pv_theme, pv_comfy)),
            pinner,
        );
    }
    // Sidebar hit zones (one row each) for mouse section jumps.
    let side_hits: Vec<Rect> = sections
        .iter()
        .enumerate()
        .map(|(i, _)| Rect {
            x: side.x,
            y: side.y + i as u16,
            width: side.width,
            height: 1,
        })
        .collect();
    (
        Rect {
            x: opts.x,
            y: opts.y,
            width: opts.width,
            height: shown as u16,
        },
        side_hits,
    )
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    }
}

/// Fleet hosts popup: registry rows with per-host agent counts and
/// fetch errors, plus the cache age. Tall box with its own scroll.
fn render_hosts(frame: &mut Frame, app: &App, pal: &Palette, area: Rect) {
    let w = area.width.saturating_sub(8).max(40);
    let h = area.height.saturating_sub(6).max(8);
    let popup = centered(area, w, h);
    frame.render_widget(Clear, popup);
    let age = match app.fleet_fetched_at {
        Some(t) => {
            let s = t.elapsed().as_secs();
            if s < 90 {
                format!("fetched {s}s ago")
            } else {
                format!("fetched {}m ago", s / 60)
            }
        }
        None => "never fetched".to_string(),
    };
    let block = Block::bordered()
        .title(Line::from(format!(" Fleet hosts ({age}) ")).style(pal.title))
        .border_style(pal.border);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let mut rows: Vec<Line> = Vec::new();
    if app.fleet_hosts.is_empty() {
        rows.push(Line::from("no hosts — press a to add one").style(pal.dim));
    }
    for (i, host) in app.fleet_hosts.iter().enumerate() {
        let n = app
            .fleet_agents
            .iter()
            .filter(|a| {
                app.fleet_index
                    .get(&a.pane)
                    .is_some_and(|(h, _)| *h == host.name)
            })
            .count();
        let err = app.fleet_errors.iter().find(|(h, _)| *h == host.name);
        let mark = if i == app.hosts_selected {
            "› "
        } else {
            "  "
        };
        let mut text = format!(
            "{mark}{} {} [{}] ({n} agents)",
            host.display_name(),
            host.target,
            host.mode.label()
        );
        if let Some((_, e)) = err {
            text.push_str(&format!(" ✗ {}", e.lines().next().unwrap_or("")));
        }
        let style = if i == app.hosts_selected {
            pal.selected
        } else if err.is_some() {
            Style::default().fg(Color::Red)
        } else {
            pal.status
        };
        rows.push(Line::from(text).style(style));
    }
    rows.push(Line::from("a=add x=remove r=fetch enter=jump esc=close").style(pal.dim));
    let shown: Vec<Line> = rows
        .into_iter()
        .skip(app.hosts_scroll)
        .take(inner.height as usize)
        .collect();
    frame.render_widget(Paragraph::new(shown), inner);
}

fn render_modal(frame: &mut Frame, app: &App, pal: &Palette, area: Rect) {
    if matches!(app.mode, Mode::Hosts) {
        render_hosts(frame, app, pal, area);
        return;
    }
    // Scrollable command output gets its own tall popup, not the 60x5 box.
    if let Mode::CommandOutput {
        name,
        lines,
        scroll,
    } = &app.mode
    {
        let w = area.width.saturating_sub(8).max(40);
        let h = area.height.saturating_sub(6).max(8);
        let popup = centered(area, w, h);
        frame.render_widget(Clear, popup);
        let block = Block::bordered()
            .title(Line::from(format!(" {name} (Esc closes) ")).style(pal.title))
            .border_style(pal.border);
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        let shown: Vec<Line> = lines
            .iter()
            .skip(*scroll)
            .take(inner.height as usize)
            .map(|l| Line::from(l.as_str()))
            .collect();
        frame.render_widget(Paragraph::new(shown).style(pal.status), inner);
        return;
    }
    let (heading, body, prefix) = match &app.mode {
        Mode::PromptNew => (
            " New session ".to_string(),
            "Name: ".to_string() + &app.input,
            Some("Name: ".len()),
        ),
        Mode::PromptSavedFilter { index } => {
            // Naming a saved filter: single line, like every other
            // prompt. The label says which of the two operations
            // (add vs rename) is in flight.
            let label = match index {
                Some(_) => "Rename: ",
                None => "Filter name: ",
            };
            (
                " Saved filter ".to_string(),
                format!("{label}{}", app.input),
                Some(label.len()),
            )
        }
        Mode::PromptRename(old) => (
            format!(" Rename '{old}' "),
            "Name: ".to_string() + &app.input,
            Some("Name: ".len()),
        ),
        Mode::PromptFilter => {
            // The standing editor wears exclusion chrome so it never
            // reads as a search: `Hide:` plus a pre-filled `!`. The
            // live prompt is a plain Text: with no pre-fill. Both
            // are single-line — the new standing model lets the user
            // type space-separated tokens (each becomes an exclude
            // in the NOT-OR sense), but the prompt itself is one
            // line.
            if app.editing_standing {
                (
                    " Exclude filter ".to_string(),
                    "Hide: ".to_string() + &app.input,
                    Some("Hide: ".len()),
                )
            } else {
                (
                    " Filter ".to_string(),
                    "Text: ".to_string() + &app.input,
                    Some("Text: ".len()),
                )
            }
        }
        Mode::PromptSpawn { profile } => {
            let prof = app
                .config
                .profiles
                .get(*profile)
                .map(|p| format!("{} · {}", p.name, p.program))
                .unwrap_or_else(|| "none".to_string());
            (
                " Spawn agent ".to_string(),
                format!("Prompt: {}\nProfile: {prof} (Tab cycles)", app.input),
                Some("Prompt: ".len()),
            )
        }
        Mode::PromptBulk { targets, template } => {
            let name = app
                .config
                .templates
                .get(*template)
                .map(|t| t.name.as_str())
                .unwrap_or("none");
            (
                format!(" Prompt {} pane(s) ", targets.len()),
                format!(
                    "Text: {}\nTo: {}\nTemplate: {name} (Tab cycles, Enter sends + submits)",
                    app.input,
                    truncate(&targets.join(" "), 52),
                ),
                Some("Text: ".len()),
            )
        }
        Mode::ConfirmKill(name) => (
            format!(" Kill '{name}'? "),
            "This closes all its windows. Enter=yes Esc=no".to_string(),
            None,
        ),
        Mode::ConfirmKillPane(id) => (
            format!(" Kill pane '{id}'? "),
            "This closes that pane only. Enter=yes Esc=no".to_string(),
            None,
        ),
        Mode::ConfirmRestartPane { id, command } => (
            format!(" Restart pane '{id}'? "),
            format!("This kills and re-runs {command}. Enter=yes Esc=no"),
            None,
        ),
        Mode::ConfirmKillMarked(ids) => (
            format!(" Kill {} marked pane(s)? ", ids.len()),
            format!("{} Enter=yes Esc=no", truncate(&ids.join(" "), 52)),
            None,
        ),
        Mode::PromptHostAdd => (
            " Add host ".to_string(),
            "host target [mode]: ".to_string() + &app.input,
            Some("host target [mode]: ".len()),
        ),
        // Handled by the tall-popup branch above.
        Mode::CommandOutput { .. } => return,
        Mode::Hosts => return,
        Mode::Normal | Mode::Settings => return,
    };
    let popup = centered(area, 60, 5);
    frame.render_widget(Clear, popup);
    let block = Block::bordered()
        .title(Line::from(heading).style(pal.title))
        .border_style(pal.border);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    // Wrapped: a long id list (or rename input) folds inside the modal
    // instead of clipping the Enter/Esc hint off the edge.
    frame.render_widget(
        Paragraph::new(body)
            .style(pal.status)
            .wrap(Wrap { trim: false }),
        inner,
    );
    if let Some(plen) = prefix {
        let cx = inner.x + (plen as u16) + (app.cursor as u16);
        let cx = cx.min(inner.x + inner.width.saturating_sub(1));
        // Two-line prompt: the active line is always the top row
        // (the parked line goes below it). The popup is 6 tall
        // (inner 4 rows), so both lines fit without clipping
        // the bottom border.
        frame.set_cursor_position(ratatui::layout::Position::new(cx, inner.y));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_theme_has_its_own_palette() {
        use crate::config::THEMES;
        let dark_title = palette("dark").title.fg;
        for theme in THEMES {
            // A missing arm silently falls into the dark palette.
            if *theme != "dark" {
                assert_ne!(
                    palette(theme).title.fg,
                    dark_title,
                    "{theme} has no palette arm"
                );
            }
        }
        // Pin the new palettes' title colors (canonical hexes).
        assert_eq!(
            palette("tokyonight").title.fg,
            Some(Color::Rgb(0x7D, 0xCF, 0xFF))
        );
        assert_eq!(
            palette("catppuccin").title.fg,
            Some(Color::Rgb(0x74, 0xC7, 0xEC))
        );
        assert_eq!(
            palette("onedark").title.fg,
            Some(Color::Rgb(0x56, 0xB6, 0xC2))
        );
        assert_eq!(
            palette("monokai").title.fg,
            Some(Color::Rgb(0x66, 0xD9, 0xEF))
        );
    }

    #[test]
    fn dark_palette_roles_are_semantically_distinct() {
        // The default dark style regressed into terminal ANSI names
        // with title==border, header==focus and attached==working.
        // Every chrome role now owns a distinct hue so states read
        // by meaning, not by squinting.
        let p = palette("dark");
        let roles = [
            ("border", p.border.fg),
            ("title", p.title.fg),
            ("header", p.header.fg),
            ("selected-bg", p.selected.bg),
            ("attached", p.attached.fg),
            ("status", p.status.fg),
            ("dim", p.dim.fg),
            ("focus", p.focus.fg),
        ];
        for (i, (an, a)) in roles.iter().enumerate() {
            for (bn, b) in roles.iter().skip(i + 1) {
                assert_ne!(
                    a, b,
                    "dark roles {an} and {bn} share a colour — samey again"
                );
            }
        }
        // Pin the canonical hexes so a drive-by edit can't silently
        // re-muddy them.
        assert_eq!(p.title.fg, Some(Color::Rgb(0x79, 0xC0, 0xFF)));
        assert_eq!(p.focus.fg, Some(Color::Rgb(0xD2, 0xA8, 0xFF)));
        assert_eq!(p.dim.fg, Some(Color::Rgb(0x9A, 0xA5, 0xB1)));
    }

    #[test]
    fn window_label_hides_lone_suffix() {
        assert_eq!(window_label("chat", 1, 1), "chat");
        assert_eq!(window_label("chat", 1, 2), "chat:[1]");
        assert_eq!(window_label("chat", 2, 2), "chat:[2]");
    }

    #[test]
    fn pane_cell_id_keeps_tmux_and_strips_namespaces() {
        // tmux ids pass through untouched.
        assert_eq!(pane_cell_id("%12"), "%12");
        // Native ids drop the backend the Session column already names;
        // a 6-wide column would otherwise clip all four Konsole tabs to
        // the same "konsol".
        assert_eq!(pane_cell_id("konsole:1535082:6"), "1535082:6");
        assert_eq!(pane_cell_id("kitty:7"), "7");
        assert_eq!(pane_cell_id("wezterm:0"), "0");
        // Fleet rows: host is in the Session column, keep the pane.
        assert_eq!(pane_cell_id("dev:%1"), "%1");
        // Anything unrecognised shows as-is rather than mangled.
        assert_eq!(pane_cell_id("odd"), "odd");
    }

    #[test]
    fn agent_text_style_groups_names_and_honors_overrides() {
        use crate::config::AppConfig;
        let cfg = AppConfig::default();
        let dim = Style::default().fg(Color::DarkGray);
        // Same name, same style (case-insensitive); unnamed/disabled → dim.
        assert_eq!(
            agent_text_style("pi", &cfg, dim),
            agent_text_style("PI", &cfg, dim)
        );
        assert_ne!(agent_text_style("pi", &cfg, dim), dim);
        assert_eq!(agent_text_style("", &cfg, dim), dim);
        let off = AppConfig {
            color_agents: false,
            ..AppConfig::default()
        };
        assert_eq!(agent_text_style("pi", &off, dim), dim);
        // Override wins over the hash.
        let mut custom = AppConfig::default();
        custom
            .agent_styles
            .insert("pi".to_string(), "red".to_string());
        assert_eq!(agent_text_style("pi", &custom, dim).fg, Some(Color::Red));
        // Custom single-entry palette pins every agent to it.
        let pal = AppConfig {
            agent_palette: vec!["#00ff00".to_string()],
            ..AppConfig::default()
        };
        assert_eq!(
            agent_text_style("pi", &pal, dim).fg,
            Some(Color::Rgb(0, 0xff, 0))
        );
        assert_eq!(
            agent_text_style("muse", &pal, dim).fg,
            Some(Color::Rgb(0, 0xff, 0))
        );
    }

    #[test]
    fn idle_cell_warns_only_on_stale_working() {
        use crate::agent::{AgentRecord, AgentState, Source};
        use crate::config::AppConfig;
        let cfg = AppConfig::default();
        let now = 10_000;
        let rec = |state: AgentState, quiet_since: u64| AgentRecord {
            pane: "%1".to_string(),
            agent: "pi".to_string(),
            state,
            source: Source::Proc,
            summary: String::new(),
            session: String::new(),
            window: String::new(),
            command: String::new(),
            updated_at: now,
            state_since: now - 100,
            cpu_ticks: 0,
            cpu_still_since: 0,
            agent_start: 0,
            last_turn_secs: 0,
            screen_hash: 1,
            quiet_since,
            last_prompt_at: 0,
            last_prompt: String::new(),
            handle: String::new(),
            model: String::new(),
            ctx_pct: None,
            reg_pid: 0,
            bell: false,
            bell_reason: String::new(),
        };
        // Working + quiet past stuck_secs (600) → warn.
        let r = rec(AgentState::Working, now - 3600);
        assert_eq!(
            idle_cell(Some(&r), 0, now, &cfg),
            ("1h0m".to_string(), true)
        );
        // Fresh work → calm.
        let r = rec(AgentState::Working, now - 5);
        assert_eq!(idle_cell(Some(&r), 0, now, &cfg), ("5s".to_string(), false));
        // Waiting is quiet by nature → calm even when old.
        let r = rec(AgentState::Waiting, now - 3600);
        assert!(!idle_cell(Some(&r), 0, now, &cfg).1);
        // No quiet clock → window activity age, still warns when stale.
        let r = rec(AgentState::Working, 0);
        assert_eq!(
            idle_cell(Some(&r), now - 3600, now, &cfg),
            ("1h0m".to_string(), true)
        );
        // Unclaimed panes never warn.
        assert_eq!(
            idle_cell(None, now - 3600, now, &cfg),
            ("1h0m".to_string(), false)
        );
        assert_eq!(idle_cell(None, 0, now, &cfg), ("—".to_string(), false));
        // Off switches: flag or stuck_secs = 0.
        let off = AppConfig {
            idle_warn: false,
            ..AppConfig::default()
        };
        let r = rec(AgentState::Working, now - 3600);
        assert!(!idle_cell(Some(&r), 0, now, &off).1);
        let nostuck = AppConfig {
            stuck_secs: 0,
            ..AppConfig::default()
        };
        assert!(!idle_cell(Some(&r), 0, now, &nostuck).1);
    }

    #[test]
    fn row_mark_rules() {
        assert_eq!(row_mark(true, false, false, 1, false), "!");
        assert_eq!(row_mark(true, true, true, 2, true), "!");
        assert_eq!(row_mark(false, true, false, 1, true), "+");
        assert_eq!(row_mark(false, true, true, 2, false), "+");
        assert_eq!(row_mark(false, false, true, 2, true), "*");
        assert_eq!(row_mark(false, false, false, 2, true), "\u{2713}");
        assert_eq!(row_mark(false, false, false, 2, false), " ");
        assert_eq!(row_mark(false, false, false, 1, true), "\u{2713}");
        // Focused but alone in its window: nothing to disambiguate.
        assert_eq!(row_mark(false, false, true, 1, false), " ");
        assert_eq!(row_mark(false, false, true, 1, true), "\u{2713}");
    }

    #[test]
    fn preview_text_keeps_ansi_colors() {
        let text = preview_text("\u{1b}[31mred\u{1b}[0m plain", 80);
        assert_eq!(text.lines.len(), 1);
        let spans = &text.lines[0].spans;
        assert_eq!(spans[0].content.as_ref(), "red");
        assert_eq!(spans[0].style.fg, Some(Color::Red));
        assert!(spans.iter().any(|s| s.content.as_ref() == " plain"));
    }

    #[test]
    fn preview_text_passes_plain_through() {
        let text = preview_text("just text\ntwo lines", 80);
        assert_eq!(text.lines.len(), 2);
        assert_eq!(text.lines[0].spans[0].content.as_ref(), "just text");
    }

    #[test]
    fn preview_eol_fill_extends_trailing_bg() {
        // tmux `-e` orphan tail: trimmed erase-painted EOL cells.
        let raw = "\u{1b}[48;2;37;65;49mhi\u{1b}[0m\u{1b}[48;2;37;65;49m";
        let text = preview_text(raw, 20);
        let line = &text.lines[0];
        assert_eq!(line.width(), 20);
        let pad = line.spans.last().unwrap();
        assert_eq!(pad.content.as_ref(), " ".repeat(18));
        assert_eq!(pad.style.bg, Some(Color::Rgb(37, 65, 49)));
    }

    #[test]
    fn preview_eol_fill_closed_tail_stays_ragged() {
        // `49m` closes the run: content is complete, no fill.
        let raw = "\u{1b}[48;2;37;65;49mhi\u{1b}[49m";
        let text = preview_text(raw, 20);
        assert_eq!(text.lines[0].width(), 2);
    }

    #[test]
    fn preview_eol_fill_blank_line_stays_dark() {
        let text = preview_text("a\n\nb", 20);
        assert_eq!(text.lines.len(), 3);
        assert_eq!(text.lines[1].width(), 0);
        assert!(text.lines[1].spans.iter().all(|s| s.style.bg.is_none()));
    }

    #[test]
    fn trailing_bg_reads_final_state() {
        assert_eq!(
            trailing_bg("x\u{1b}[0m\u{1b}[48;2;1;2;3m"),
            Some(Color::Rgb(1, 2, 3))
        );
        assert_eq!(trailing_bg("x\u{1b}[48;2;1;2;3m\u{1b}[49m"), None);
        assert_eq!(trailing_bg("x\u{1b}[41m"), Some(Color::Red));
        assert_eq!(trailing_bg("x\u{1b}[48;5;200m"), Some(Color::Indexed(200)));
        assert_eq!(trailing_bg("plain"), None);
        assert_eq!(trailing_bg(""), None);
        // Non-SGR tail breaks the run: no fill.
        assert_eq!(trailing_bg("x\u{1b}[48;2;1;2;3m "), None);
    }

    #[test]
    fn strip_ansi_removes_sequences() {
        assert_eq!(strip_ansi("\u{1b}[31mred\u{1b}[0m"), "red");
        assert_eq!(strip_ansi("\u{1b}[?25lhide"), "hide");
        assert_eq!(strip_ansi("\u{1b}(Bplain"), "plain");
        assert_eq!(strip_ansi("no escapes"), "no escapes");
    }

    #[test]
    fn counts_key_on_window_id() {
        let mut a = Pane::test("%1", "w", "pi");
        a.window_id = "@1".to_string();
        let mut b = Pane::test("%2", "w", "pi");
        b.window_id = "@1".to_string();
        // Same name, other window: must not merge.
        let mut c = Pane::test("%3", "w", "pi");
        c.window_id = "@2".to_string();
        let panes = [a, b, c];
        let counts = window_pane_counts(&panes);
        assert_eq!(counts.get("@1"), Some(&2));
        assert_eq!(counts.get("@2"), Some(&1));
    }

    /// The state cell renders `idle` for unclaimed panes that have been
    /// quiet past [`QUIET_SHELL_SECS`] (the same threshold the `h` toggle
    /// uses to hide them), and `—` otherwise. This test pins the gate so
    /// "we are not currently showing idles" — unclaimed dormancy reading
    /// as blank — stays fixed.
    #[test]
    fn unclaimed_quiet_pane_is_idle() {
        use crate::app::QUIET_SHELL_SECS;
        let now = 10_000_u64;
        // Activity right at the threshold: qualifies (>=).
        assert!(is_quiet_shell(None, now - QUIET_SHELL_SECS, now));
        // Comfortably past the threshold: qualifies.
        assert!(is_quiet_shell(None, now - QUIET_SHELL_SECS - 1, now));
        // One second short: does not qualify (the threshold is inclusive).
        assert!(!is_quiet_shell(None, now - QUIET_SHELL_SECS + 1, now));
        // Unknown activity (0): never qualifies — unknowns stay `—`.
        assert!(!is_quiet_shell(None, 0, now));
        // Activity from before unix epoch / weird clocks: saturates to 0
        // and stays non-quiet, so a glitchy pane doesn't read as idle.
        assert!(!is_quiet_shell(None, u64::MAX, now));
    }
}
