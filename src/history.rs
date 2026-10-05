//! History: append-only activity ledger, stats, and timeline.
//!
//! The daemon logs every agent state transition plus tmux session open/close
//! as JSON lines. Stats and timeline derive from those entries. Day
//! boundaries are UTC (documented limitation; no tz database vendored).
use crate::agent::{AgentRecord, AgentState};
use crate::attend::Transition;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

pub fn data_dir() -> PathBuf {
    if let Ok(d) = std::env::var("TERHUB_DATA_DIR")
        && !d.is_empty()
    {
        return PathBuf::from(d);
    }
    if let Ok(d) = std::env::var("XDG_DATA_HOME")
        && !d.is_empty()
    {
        return PathBuf::from(d).join("terhub");
    }
    if let Ok(h) = std::env::var("HOME")
        && !h.is_empty()
    {
        return PathBuf::from(h).join(".local/share/terhub");
    }
    std::env::temp_dir().join("terhub-data")
}

pub fn ledger_path() -> PathBuf {
    data_dir().join("activity.jsonl")
}

// ---------------------------------------------------------------------------
// Entries
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub t: u64,
    /// "agent", "session", "rule", or "human".
    pub kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pane: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<AgentState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<AgentState>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub session: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub window: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub summary: String,
    /// Session entries only: "open" or "close".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
}

impl Entry {
    pub fn transition(t: u64, tr: &Transition) -> Self {
        Entry {
            t,
            kind: "agent".to_string(),
            pane: tr.pane.clone(),
            agent: tr.agent.clone(),
            from: Some(tr.from),
            to: Some(tr.to),
            session: tr.session.clone(),
            window: tr.window.clone(),
            summary: tr.summary.clone(),
            event: None,
        }
    }

    /// A rule firing: kind "rule", event = rule name.
    pub fn rule_fire(rule: &str, rec: &AgentRecord, desc: &str, t: u64) -> Self {
        Entry {
            t,
            kind: "rule".to_string(),
            pane: rec.pane.clone(),
            agent: rec.agent.clone(),
            from: None,
            to: None,
            session: rec.session.clone(),
            window: rec.window.clone(),
            summary: desc.to_string(),
            event: Some(rule.to_string()),
        }
    }

    /// Human focus change: event "focus" with the pane, or "detach".
    pub fn human_focus(t: u64, pane: Option<&str>) -> Self {
        Entry {
            t,
            kind: "human".to_string(),
            pane: pane.unwrap_or("").to_string(),
            agent: String::new(),
            from: None,
            to: None,
            session: String::new(),
            window: String::new(),
            summary: String::new(),
            event: Some(if pane.is_some() {
                "focus".to_string()
            } else {
                "detach".to_string()
            }),
        }
    }

    pub fn session_event(t: u64, session: &str, open: bool) -> Self {
        Entry {
            t,
            kind: "session".to_string(),
            pane: String::new(),
            agent: String::new(),
            from: None,
            to: None,
            session: session.to_string(),
            window: String::new(),
            summary: String::new(),
            event: Some(if open {
                "open".to_string()
            } else {
                "close".to_string()
            }),
        }
    }
}

pub fn append_entry(path: &std::path::Path, entry: &Entry) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{}", serde_json::to_string(entry)?)?;
    Ok(())
}

pub fn read_entries(path: &std::path::Path) -> Vec<Entry> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    text.lines()
        .filter_map(|l| serde_json::from_str(l.trim()).ok())
        .collect()
}

/// Past rule fires as (rule, pane, unixtime) triples for guardrails.
pub fn read_rule_fires(path: &std::path::Path) -> Vec<(String, String, u64)> {
    read_entries(path)
        .into_iter()
        .filter(|e| e.kind == "rule")
        .filter_map(|e| e.event.clone().map(|r| (r, e.pane.clone(), e.t)))
        .collect()
}

/// Drop entries older than `keep_days`; returns surviving count.
pub fn prune(path: &std::path::Path, now: u64, keep_days: u64) -> usize {
    let cutoff = now.saturating_sub(keep_days * 86400);
    let kept: Vec<Entry> = read_entries(path)
        .into_iter()
        .filter(|e| e.t >= cutoff)
        .collect();
    if let Ok(text) = kept
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()
    {
        let _ = std::fs::write(
            path,
            text.join("\n") + if text.is_empty() { "" } else { "\n" },
        );
    }
    kept.len()
}

// ---------------------------------------------------------------------------
// Dates (UTC, Howard Hinnant's algorithms)
// ---------------------------------------------------------------------------

pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = ((m + 9) % 12) as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub fn day_of(t: u64) -> i64 {
    (t / 86400) as i64
}

pub fn ymd_label(days: i64) -> String {
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

pub fn hm_label(t: u64) -> String {
    format!("{:02}:{:02}", (t % 86400) / 3600, (t % 3600) / 60)
}

fn utc_midnight(now: u64) -> u64 {
    now - now % 86400
}

/// Parse `--since`: today|yesterday|week|month|all, `<n>m|h|d`, or YYYY-MM-DD.
/// `Ok(None)` means all of history.
pub fn parse_since(s: &str, now: u64) -> Result<Option<u64>> {
    let s = s.trim().to_lowercase();
    if s == "all" {
        return Ok(None);
    }
    if s == "today" {
        return Ok(Some(utc_midnight(now)));
    }
    if s == "yesterday" {
        return Ok(Some(utc_midnight(now).saturating_sub(86400)));
    }
    if s == "week" {
        return Ok(Some(now.saturating_sub(7 * 86400)));
    }
    if s == "month" {
        return Ok(Some(now.saturating_sub(30 * 86400)));
    }
    if s.len() > 1 {
        let (num, unit) = s.split_at(s.len() - 1);
        if let Ok(n) = num.parse::<u64>() {
            let mult = match unit {
                "m" => Some(60),
                "h" => Some(3600),
                "d" => Some(86400),
                _ => None,
            };
            if let Some(mult) = mult {
                return Ok(Some(now.saturating_sub(n.saturating_mul(mult))));
            }
        }
    }
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() == 3
        && let (Ok(y), Ok(m), Ok(d)) = (
            parts[0].parse::<i64>(),
            parts[1].parse::<u32>(),
            parts[2].parse::<u32>(),
        )
        && (1..=12).contains(&m)
        && (1..=31).contains(&d)
    {
        // Round-trip: days_from_civil normalizes 02-31 into March, so reject
        // dates that don't survive the trip.
        let days = days_from_civil(y, m, d);
        if days >= 0 && civil_from_days(days) == (y, m, d) {
            return Ok(Some(days as u64 * 86400));
        }
    }
    anyhow::bail!("bad --since '{s}' (today|yesterday|week|month|all|24h|7d|YYYY-MM-DD)")
}

// ---------------------------------------------------------------------------
// Spans + stats
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct Span {
    pub pane: String,
    pub agent: String,
    pub session: String,
    pub window: String,
    pub state: AgentState,
    pub start: u64,
    pub end: u64,
    pub summary: String,
}

/// Turn per-pane transition entries into state spans ending at `now`.
/// A span starts when its state is entered; the unknown past before the first
/// entry is never fabricated.
pub fn build_spans(entries: &[Entry], now: u64) -> Vec<Span> {
    let mut by_pane: HashMap<&str, Vec<&Entry>> = HashMap::new();
    for e in entries {
        if e.kind != "agent" || e.to.is_none() {
            continue;
        }
        by_pane.entry(e.pane.as_str()).or_default().push(e);
    }
    let mut spans = Vec::new();
    for entries in by_pane.values() {
        let mut sorted = entries.clone();
        sorted.sort_by_key(|e| e.t);
        for (i, e) in sorted.iter().enumerate() {
            let to = match e.to {
                Some(s) => s,
                None => continue,
            };
            let end = sorted.get(i + 1).map(|n| n.t).unwrap_or(now);
            if end < e.t {
                continue;
            }
            spans.push(Span {
                pane: e.pane.clone(),
                agent: e.agent.clone(),
                session: e.session.clone(),
                window: e.window.clone(),
                state: to,
                start: e.t,
                end,
                summary: e.summary.clone(),
            });
        }
    }
    spans.sort_by_key(|s| (s.start, s.pane.clone()));
    spans
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupBy {
    Day,
    Agent,
    Session,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct StatsRow {
    pub key: String,
    pub work: u64,
    pub wait: u64,
    pub err: u64,
    pub idle: u64,
}

impl StatsRow {
    pub fn total(&self) -> u64 {
        self.work + self.wait + self.err + self.idle
    }
}

/// Aggregate spans into per-key rows. Unknown states are skipped; spans are
/// clipped to `since` and attributed to the group of their (clipped) start.
pub fn stats(spans: &[Span], since: Option<u64>, group: GroupBy) -> Vec<StatsRow> {
    let mut rows: HashMap<String, StatsRow> = HashMap::new();
    for s in spans {
        let start = since.map(|c| s.start.max(c)).unwrap_or(s.start);
        if start >= s.end {
            continue;
        }
        let dur = s.end - start;
        let key = match group {
            GroupBy::Day => ymd_label(day_of(start)),
            GroupBy::Agent => {
                if s.agent.is_empty() {
                    "(unknown)".to_string()
                } else {
                    s.agent.clone()
                }
            }
            GroupBy::Session => {
                if s.session.is_empty() {
                    "(unknown)".to_string()
                } else {
                    s.session.clone()
                }
            }
        };
        let row = rows.entry(key.clone()).or_insert(StatsRow {
            key,
            ..StatsRow::default()
        });
        match s.state {
            AgentState::Working => row.work += dur,
            AgentState::Waiting => row.wait += dur,
            AgentState::Error | AgentState::Stuck | AgentState::Limited => row.err += dur,
            AgentState::Idle => row.idle += dur,
            AgentState::Unknown => {}
        }
    }
    let mut out: Vec<StatsRow> = rows.into_values().collect();
    out.sort_by(|a, b| a.key.cmp(&b.key));
    out
}

pub fn fmt_dur(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    }
}

// ---------------------------------------------------------------------------
// Timeline view
// ---------------------------------------------------------------------------

pub struct TimelineView {
    pub spans: Vec<Span>,
    pub scroll: usize,
}

impl TimelineView {
    pub fn line_count(&self) -> usize {
        let mut days = std::collections::HashSet::new();
        for s in &self.spans {
            days.insert(day_of(s.start));
        }
        self.spans.len() + days.len()
    }
}

fn state_style(state: AgentState) -> ratatui::style::Style {
    use ratatui::style::{Color, Modifier, Style};
    match state {
        AgentState::Waiting | AgentState::Error | AgentState::Stuck | AgentState::Limited => {
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        }
        AgentState::Working => Style::default().fg(Color::Green),
        AgentState::Idle => Style::default().fg(Color::DarkGray),
        AgentState::Unknown => Style::default().fg(Color::DarkGray),
    }
}

pub fn render_timeline(
    frame: &mut ratatui::Frame,
    view: &TimelineView,
    title: &str,
    area: ratatui::layout::Rect,
    pal: &crate::ui::Palette,
) {
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line, Span as TSpan};
    use ratatui::widgets::{Block, Paragraph};

    let mut lines: Vec<Line> = Vec::new();
    let mut last_day = i64::MIN;
    for s in &view.spans {
        let day = day_of(s.start);
        if day != last_day {
            last_day = day;
            lines.push(Line::from(TSpan::styled(
                format!("── {} ──", ymd_label(day)),
                Style::default().add_modifier(Modifier::BOLD),
            )));
        }
        let range = format!(
            "{}–{} {:>4}",
            hm_label(s.start),
            hm_label(s.end),
            fmt_dur(s.end - s.start)
        );
        let mut spans = vec![TSpan::raw(format!(
            "{range}  {:<8} {:<5} ",
            s.agent, s.pane
        ))];
        spans.push(TSpan::styled(
            format!("{:<7}", s.state.label()),
            state_style(s.state),
        ));
        spans.push(TSpan::raw(format!(" {}:{}", s.session, s.window)));
        if !s.summary.is_empty() {
            spans.push(TSpan::raw(format!(" {}", s.summary)));
        }
        lines.push(Line::from(spans));
    }
    if lines.is_empty() {
        lines.push(Line::raw("no activity in range"));
    }
    let total = lines.len();
    let visible: Vec<Line> = lines.into_iter().skip(view.scroll).collect();
    let block = Block::bordered()
        .title(Line::from(format!(" {title} ({total} lines) ")).style(pal.title))
        .border_style(pal.border);
    let base = if total == 1 && view.spans.is_empty() {
        pal.dim
    } else {
        pal.status
    };
    let styled: Vec<Line> = visible
        .into_iter()
        .map(|mut l| {
            l.style = base;
            l
        })
        .collect();
    frame.render_widget(Paragraph::new(styled).block(block), area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_round_trip() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        for (y, m, d) in [(2026, 10, 1), (2000, 2, 29), (1999, 12, 31), (2038, 1, 19)] {
            let days = days_from_civil(y, m, d);
            assert_eq!(civil_from_days(days), (y, m, d));
        }
        assert_eq!(ymd_label(days_from_civil(2026, 10, 1)), "2026-10-01");
        assert_eq!(hm_label(21 * 3600 + 47 * 60), "21:47");
    }

    #[test]
    fn since_parsing() {
        // 2026-10-01T12:00:00Z
        let now = days_from_civil(2026, 10, 1) as u64 * 86400 + 12 * 3600;
        let midnight = days_from_civil(2026, 10, 1) as u64 * 86400;
        assert_eq!(parse_since("all", now).unwrap(), None);
        assert_eq!(parse_since("today", now).unwrap(), Some(midnight));
        assert_eq!(
            parse_since("yesterday", now).unwrap(),
            Some(midnight - 86400)
        );
        assert_eq!(parse_since("24h", now).unwrap(), Some(now - 86400));
        assert_eq!(parse_since("7d", now).unwrap(), Some(now - 7 * 86400));
        assert_eq!(
            parse_since("2026-09-30", now).unwrap(),
            Some(midnight - 86400)
        );
        assert!(parse_since("bogus", now).is_err());
        assert!(parse_since("2026-13-01", now).is_err());
        // Impossible days are rejected, not normalized into the next month.
        assert!(parse_since("2026-02-31", now).is_err());
        assert!(parse_since("2026-02-30", now).is_err());
        assert!(parse_since("2027-02-29", now).is_err());
        assert!(parse_since("2028-02-29", now).is_ok());
    }

    fn entry(t: u64, pane: &str, to: AgentState) -> Entry {
        Entry {
            t,
            kind: "agent".to_string(),
            pane: pane.to_string(),
            agent: "claude".to_string(),
            from: None,
            to: Some(to),
            session: "s".to_string(),
            window: "w".to_string(),
            summary: String::new(),
            event: None,
        }
    }

    #[test]
    fn fmt_dur_uses_days_past_two_days() {
        assert_eq!(fmt_dur(45), "45s");
        assert_eq!(fmt_dur(90), "1m");
        assert_eq!(fmt_dur(5 * 3600 + 600), "5h10m");
        // A 56-hour "turn" reads like a broken clock; days stay honest.
        assert_eq!(fmt_dur(56 * 3600 + 36 * 60), "2d8h");
        assert_eq!(fmt_dur(2 * 86400), "2d0h");
        assert_eq!(fmt_dur(47 * 3600), "47h0m");
    }

    #[test]
    fn spans_cover_transitions_to_now() {
        let entries = vec![
            entry(100, "%1", AgentState::Working),
            entry(200, "%1", AgentState::Waiting),
            entry(150, "%2", AgentState::Error),
        ];
        let spans = build_spans(&entries, 300);
        assert_eq!(spans.len(), 3);
        let w: Vec<&Span> = spans.iter().filter(|s| s.pane == "%1").collect();
        assert_eq!((w[0].start, w[0].end), (100, 200));
        assert_eq!(w[0].state, AgentState::Working);
        assert_eq!((w[1].start, w[1].end), (200, 300));
    }

    #[test]
    fn stats_group_and_clip() {
        let entries = vec![
            entry(100, "%1", AgentState::Working),
            entry(200, "%1", AgentState::Waiting),
        ];
        let spans = build_spans(&entries, 400);
        let rows = stats(&spans, None, GroupBy::Agent);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].work, 100);
        assert_eq!(rows[0].wait, 200);
        // Clipped at 250: only the waiting tail counts.
        let rows = stats(&spans, Some(250), GroupBy::Agent);
        assert_eq!(rows[0].work, 0);
        assert_eq!(rows[0].wait, 150);
    }

    #[test]
    fn human_focus_entries() {
        let e = Entry::human_focus(100, Some("%3"));
        assert_eq!(e.kind, "human");
        assert_eq!(e.event.as_deref(), Some("focus"));
        assert_eq!(e.pane, "%3");
        let e = Entry::human_focus(100, None);
        assert_eq!(e.event.as_deref(), Some("detach"));
    }

    #[test]
    fn ledger_round_trip() {
        let dir = std::env::temp_dir().join(format!("terhub-ledger-{}", std::process::id()));
        let path = dir.join("activity.jsonl");
        let e = entry(100, "%1", AgentState::Working);
        append_entry(&path, &e).unwrap();
        append_entry(&path, &Entry::session_event(120, "s", true)).unwrap();
        let back = read_entries(&path);
        assert_eq!(back.len(), 2);
        assert_eq!(back[1].event.as_deref(), Some("open"));
        // Prune keeps only recent.
        assert_eq!(prune(&path, 100 + 91 * 86400, 90), 0);
        assert!(read_entries(&path).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn timeline_renders_spans() {
        use ratatui::{Terminal, backend::TestBackend};
        let entries = vec![
            entry(100, "%1", AgentState::Working),
            entry(200, "%1", AgentState::Waiting),
        ];
        let view = TimelineView {
            spans: build_spans(&entries, 300),
            scroll: 0,
        };
        let backend = TestBackend::new(100, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let pal = crate::ui::palette("dracula");
                render_timeline(f, &view, "timeline", f.area(), &pal)
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                text.push_str(buf[(x, y)].symbol());
            }
            text.push('\n');
        }
        assert!(text.contains("working"), "missing working:\n{text}");
        assert!(text.contains("waiting"), "missing waiting:\n{text}");
        assert!(text.contains("1970-01-01"), "missing day header:\n{text}");
    }
}
