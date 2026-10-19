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

    /// A user prompt (kind "prompt"): what the human asked, capped.
    pub fn prompt(t: u64, pane: &str, agent: &str, prompt: &str) -> Self {
        Entry {
            t,
            kind: "prompt".to_string(),
            pane: pane.to_string(),
            agent: agent.to_string(),
            from: None,
            to: None,
            session: String::new(),
            window: String::new(),
            summary: crate::agent::cap_chars(prompt.trim(), LEDGER_PROMPT_CAP),
            event: None,
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

/// Exclusive lock on the ledger, taken by every writer and by the
/// prune.
///
/// The prune rewrites the file by rename. An append that lands between
/// the prune's read and its rename is written through a descriptor on
/// the *old* inode, which the rename unlinks: the entry is written
/// successfully and then silently discarded, so it never appears in a
/// report even though the TUI showed it. The lock makes the two
/// mutually exclusive.
///
/// A sidecar file, not the ledger itself: the prune replaces the
/// ledger's inode, so a lock held on the ledger would not protect the
/// file that replaces it.
struct LedgerLock {
    file: std::fs::File,
}

impl LedgerLock {
    fn acquire(path: &std::path::Path) -> Result<Self> {
        use std::os::unix::io::AsRawFd;
        let lock_path = path.with_extension("lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&lock_path)?;
        // SAFETY: flock on a live fd, exclusive and blocking. The lock
        // is released when the fd is closed, which the Drop below does.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(LedgerLock { file })
    }
}

impl Drop for LedgerLock {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        // SAFETY: the fd is still open here — the file is dropped after
        // this runs, and the unlock only touches the descriptor.
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

/// Make the data directory private to its owner.
///
/// The ledger holds prompts, summaries and project paths, and the
/// daemon's own stderr log is already 0600 for exactly that reason.
/// A directory that other local accounts can list is a directory they
/// can read.
fn ensure_private_dir(dir: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir)?;
    let want = std::fs::Permissions::from_mode(0o700);
    // Only touch the mode when it is wrong: this runs on the append
    // path, and a needless chmod syscall is not worth it.
    let current = std::fs::metadata(dir).map(|m| m.permissions().mode() & 0o777);
    if current != Some(0o700) {
        std::fs::set_permissions(dir, want)?;
    }
    Ok(())
}

/// Give an existing ledger the same private mode.
///
/// `OpenOptions::mode` only applies when the file is *created*, so a
/// ledger that predates this fix keeps its old permissions until
/// something corrects them.
pub fn ensure_ledger_mode(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path)
        && meta.permissions().mode() & 0o777 != 0o600
    {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
}

pub fn append_entry(path: &std::path::Path, entry: &Entry) -> Result<()> {
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }
    use std::io::Write;
    let line = serde_json::to_string(entry)?;
    // The lock is taken after the serialization so a serialization
    // failure does not hold it, and before the open so the append
    // cannot land inside the prune's read-rename window.
    let _lock = LedgerLock::acquire(path)?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    writeln!(file, "{line}")?;
    Ok(())
}

/// Stream the ledger one line at a time.
///
/// A line torn mid–UTF-8 character (a crash/ENOSPC inside
/// `append_entry`'s one write, splitting a multi-byte char in a
/// summary) must not void the whole file: `read_to_string` fails on
/// *any* invalid byte, which used to make every query report "no
/// activity" and — worse — emptied `read_rule_fires`, resetting
/// automation cooldowns/caps. Lossy per-line decoding turns the torn
/// line into an unparseable line, which the existing paths already
/// skip (reads) or quarantine (prune), leaving every valid line
/// intact.
///
/// The bytes are read in chunks rather than slurped: a 34 MB ledger
/// costs one 64 KB buffer instead of the whole file plus a `Vec<String>`
/// holding a copy of every line.
fn for_each_line(path: &std::path::Path, mut f: impl FnMut(&str)) -> std::io::Result<()> {
    use std::io::BufRead;
    let file = std::fs::File::open(path)?;
    let mut reader = std::io::BufReader::with_capacity(64 * 1024, file);
    let mut buf: Vec<u8> = Vec::with_capacity(512);
    loop {
        buf.clear();
        let n = reader.read_until(b'\n', &mut buf)?;
        if n == 0 {
            break;
        }
        f(&String::from_utf8_lossy(&buf));
    }
    Ok(())
}

/// The timestamp at the head of a ledger line, when the line has the
/// standard shape.
///
/// `serde_json` writes the fields in declaration order and `t` is
/// first, so the timestamp can be read without deserializing the
/// line. That is what makes a windowed read cheap: the lines outside
/// the window are skipped without ever being parsed.
fn entry_time(line: &str) -> Option<u64> {
    let rest = line.strip_prefix("{\"t\":")?;
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// Read entries, keeping only those at or after `since` (unix seconds).
///
/// `since = None` reads everything. The window is applied *before* the
/// line is parsed, so a one-hour query no longer costs the same as an
/// all-time one: on a 34 MB ledger the all-time read was 5.9s and the
/// one-hour read 3.5s, because the parse happened either way.
pub fn read_entries_since(path: &std::path::Path, since: Option<u64>) -> Vec<Entry> {
    read_entries_result(path, since).unwrap_or_default()
}

/// The read that can fail, for the callers that must not treat a
/// failure as "nothing is there".
///
/// The distinction is the whole point: a missing file means "no
/// history yet", while a read error means "the history is unreadable".
/// Collapsing the second into the first empties the automation
/// guardrail state and lets a rule fire again immediately.
fn read_entries_result(path: &std::path::Path, since: Option<u64>) -> Result<Vec<Entry>> {
    let mut out = Vec::new();
    for_each_line(path, |raw| {
        let line = raw.trim();
        if line.is_empty() {
            return;
        }
        if let Some(since) = since
            && let Some(t) = entry_time(line)
            && t < since
        {
            return;
        }
        if let Ok(e) = serde_json::from_str::<Entry>(line) {
            out.push(e);
        }
    })?;
    Ok(out)
}

pub fn read_entries(path: &std::path::Path) -> Vec<Entry> {
    read_entries_since(path, None)
}

/// Past rule fires as (rule, pane, unixtime) triples for guardrails.
///
/// `Ok` distinguishes "no file yet" from "the file could not be read":
/// the difference matters, because a read error must not read as
/// "nothing has fired" — that empties the guardrail state and lets a
/// rule fire again immediately.
pub fn read_rule_fires_checked(path: &std::path::Path) -> Result<Vec<(String, String, u64)>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    Ok(read_entries_result(path, None)?
        .into_iter()
        .filter(|e| e.kind == "rule")
        .filter_map(|e| e.event.clone().map(|r| (r, e.pane.clone(), e.t)))
        .collect())
}

/// Past rule fires, treating an unreadable ledger as "no fires".
///
/// Prefer [`read_rule_fires_checked`] anywhere the difference is
/// load-bearing.
pub fn read_rule_fires(path: &std::path::Path) -> Vec<(String, String, u64)> {
    read_rule_fires_checked(path).unwrap_or_default()
}

/// Default ceiling on the ledger's size, in bytes.
///
/// The 90-day window bounds the ledger by *age*; this bounds it by
/// size. Without the second bound a busy machine reaches hundreds of
/// megabytes inside the retention window (this ledger grew ~1.9
/// MB/day), and every query pays to parse all of it.
pub const LEDGER_MAX_BYTES: usize = 64 * 1024 * 1024;

/// Drop entries older than `keep_days`; returns surviving count.
/// Prompt text kept per ledger line (the record keeps a shorter cut).
pub const LEDGER_PROMPT_CAP: usize = 500;

/// Drop entries older than `keep_days`; returns surviving count.
///
/// A serialization *or write* failure is returned as an error instead
/// of the in-memory count: reporting success while the old ledger
/// stays on disk would lie about what was pruned — and the write
/// failure is the likelier one (read-only dir, full disk).
pub fn prune(path: &std::path::Path, now: u64, keep_days: u64) -> Result<usize, String> {
    prune_bounded(path, now, keep_days, LEDGER_MAX_BYTES)
}

/// [`prune`] with an explicit byte ceiling.
///
/// The oldest ordinary entries are dropped first — the file is
/// append-only, so its order is chronological. Quarantined (torn)
/// lines are kept whatever happens: the user may still be working on
/// recovering the data, so they are not what the budget is spent on.
pub fn prune_bounded(
    path: &std::path::Path,
    now: u64,
    keep_days: u64,
    max_bytes: usize,
) -> Result<usize, String> {
    let cutoff = now.saturating_sub(keep_days * 86400);
    // The lock makes this read-rewrite atomic with respect to appends.
    // Without it, an append that lands between the read and the rename
    // is written to the old inode and then silently discarded.
    let _lock = LedgerLock::acquire(path)
        .map_err(|e| format!("prune ledger lock failed: {e}"))?;
    // A ledger that does not exist yet has nothing to prune.
    if !path.exists() {
        return Ok(0);
    }
    let mut kept: Vec<Entry> = Vec::new();
    let mut quarantined: Vec<Entry> = Vec::new();
    // Per-line lossy decode (see for_each_line): a UTF-8-torn line
    // reaches the quarantine arm below instead of voiding the file.
    for_each_line(path, |raw| {
        let l = raw.trim();
        if l.is_empty() {
            return;
        }
        match serde_json::from_str::<Entry>(l) {
            Ok(e) if e.kind == "__corrupt__" => {
                // A prior quarantine: keep it (the user may still be
                // working on recovering the data).
                quarantined.push(e);
            }
            Ok(e) if e.t >= cutoff => kept.push(e),
            Ok(_) => {} // expired, drop
            Err(reason) => {
                // Torn / unparseable line: tag it __corrupt__ and
                // preserve the raw head + the parse error in `summary`
                // so `history status` can show both. The tagged entry
                // is a normal, parseable Entry, so future prunes and
                // reads treat it like any other quarantined line; the
                // query code (build_spans, stats, recent_prompts) all
                // filter by `kind`, so quarantines never leak into
                // reports.
                quarantined.push(Entry {
                    t: 0,
                    kind: "__corrupt__".to_string(),
                    pane: String::new(),
                    agent: String::new(),
                    from: None,
                    to: None,
                    session: String::new(),
                    window: String::new(),
                    summary: corrupt_summary(l, &reason),
                    event: None,
                });
            }
        }
    })
    .map_err(|e| format!("prune ledger read failed: {e}"))?;
    let kept_lines: Vec<String> = kept
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("prune ledger serialization failed: {e}"))?;
    let quarantined_lines: Vec<String> = quarantined
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("prune ledger serialization failed: {e}"))?;
    // Spend the budget on the ordinary entries, oldest first.
    let mut kept_bytes: usize = kept_lines.iter().map(|l| l.len() + 1).sum();
    let quarantined_bytes: usize = quarantined_lines.iter().map(|l| l.len() + 1).sum();
    let mut start = 0usize;
    while kept_bytes + quarantined_bytes > max_bytes && start < kept_lines.len() {
        kept_bytes -= kept_lines[start].len() + 1;
        start += 1;
    }
    let kept_lines = &kept_lines[start..];
    let count = kept_lines.len();
    let mut body = String::with_capacity(kept_bytes + quarantined_bytes);
    for l in kept_lines.iter().chain(quarantined_lines.iter()) {
        body.push_str(l);
        body.push('\n');
    }
    // Atomic write so a crash mid-prune can't truncate the ledger.
    // The failure is propagated: a prune that could not write has not
    // pruned anything, and saying otherwise hides the ledger growing
    // without bound.
    crate::config::atomic_write(path, &body)
        .map_err(|e| format!("prune ledger write failed: {e}"))?;
    Ok(count)
}

/// Recent prompts, newest first, optionally scoped to one pane.
pub fn recent_prompts(entries: &[Entry], pane: Option<&str>, limit: usize) -> Vec<Entry> {
    let mut out: Vec<Entry> = entries
        .iter()
        .filter(|e| e.kind == "prompt" && pane.is_none_or(|p| e.pane == p))
        .cloned()
        .collect();
    out.sort_by_key(|e| std::cmp::Reverse(e.t));
    out.truncate(limit);
    out
}

fn corrupt_summary(raw: &str, reason: &serde_json::Error) -> String {
    // Cap the head so a single pathological line can't grow without
    // bound; the parse error is small and is always included.
    const HEAD_CAP: usize = 200;
    let head: String = raw.chars().take(HEAD_CAP).collect();
    if raw.chars().count() > HEAD_CAP {
        format!("{head}…  ({reason})")
    } else {
        format!("{head}  ({reason})")
    }
}

/// Number of quarantined (torn) lines currently in the ledger. The
/// `history status` / CLI surfaces this so the user can see and act on
/// a crash that left a torn line behind.
pub fn corrupt_count(path: &std::path::Path) -> usize {
    read_entries(path)
        .iter()
        .filter(|e| e.kind == "__corrupt__")
        .count()
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
    // Suffix-strip, never byte-split: split_at(len-1) panics on a
    // trailing multibyte char ("5é"). An empty head fails the u64 parse.
    for (suffix, mult) in [("m", 60), ("h", 3600), ("d", 86400)] {
        if let Some(num) = s.strip_suffix(suffix)
            && let Ok(n) = num.parse::<u64>()
        {
            return Ok(Some(now.saturating_sub(n.saturating_mul(mult))));
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
    /// Finished-and-acknowledged: the agent ran, hit a verdicted footer,
    /// and stayed quiet. Separate from `idle` (which means "never ran
    /// / never finished") so the timeline can show how productive a
    /// day was without confusing the two.
    pub done: u64,
}

impl StatsRow {
    pub fn total(&self) -> u64 {
        self.work + self.wait + self.err + self.idle + self.done
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
            AgentState::Done => row.done += dur,
            AgentState::Idle => row.idle += dur,
            AgentState::Unknown => {}
        }
    }
    let mut out: Vec<StatsRow> = rows.into_values().collect();
    out.sort_by(|a, b| a.key.cmp(&b.key));
    out
}

// ---------------------------------------------------------------------------
// Foreground time (human focus ledger)
// ---------------------------------------------------------------------------

/// One pane holding the human's focus, from a `focus` event to the next
/// focus/detach (or `now` for the open tail).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeSpan {
    pub pane: String,
    pub start: u64,
    pub end: u64,
}

pub fn build_foreground(entries: &[Entry], now: u64) -> Vec<ForeSpan> {
    let mut human: Vec<&Entry> = entries.iter().filter(|e| e.kind == "human").collect();
    human.sort_by_key(|e| e.t);
    let mut spans = Vec::new();
    let mut current: Option<(&str, u64)> = None;
    let close = |cur: &mut Option<(&str, u64)>, end: u64, spans: &mut Vec<ForeSpan>| {
        if let Some((pane, start)) = cur.take()
            && end > start
        {
            spans.push(ForeSpan {
                pane: pane.to_string(),
                start,
                end,
            });
        }
    };
    for e in human {
        match e.event.as_deref() {
            Some("focus") if !e.pane.is_empty() => {
                close(&mut current, e.t, &mut spans);
                current = Some((e.pane.as_str(), e.t));
            }
            _ => close(&mut current, e.t, &mut spans),
        }
    }
    close(&mut current, now, &mut spans);
    spans
}

/// Focused seconds per pane, most-focused first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ForeTotal {
    pub pane: String,
    pub secs: u64,
}

pub fn foreground_totals(spans: &[ForeSpan], since: Option<u64>) -> Vec<ForeTotal> {
    let mut acc: HashMap<&str, u64> = HashMap::new();
    for s in spans {
        let start = since.map(|c| s.start.max(c)).unwrap_or(s.start);
        if start >= s.end {
            continue;
        }
        *acc.entry(s.pane.as_str()).or_default() += s.end - start;
    }
    let mut out: Vec<ForeTotal> = acc
        .into_iter()
        .map(|(pane, secs)| ForeTotal {
            pane: pane.to_string(),
            secs,
        })
        .collect();
    out.sort_by(|a, b| b.secs.cmp(&a.secs).then(a.pane.cmp(&b.pane)));
    out
}

// ---------------------------------------------------------------------------
// Report (all breakdowns at once, three renderings)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize)]
pub struct ReportData {
    pub by_day: Vec<StatsRow>,
    pub by_agent: Vec<StatsRow>,
    pub by_session: Vec<StatsRow>,
    pub foreground: Vec<ForeTotal>,
}

impl ReportData {
    pub fn is_empty(&self) -> bool {
        self.by_day.iter().all(|r| r.total() == 0)
            && self.by_agent.iter().all(|r| r.total() == 0)
            && self.by_session.iter().all(|r| r.total() == 0)
            && self.foreground.iter().all(|f| f.secs == 0)
    }
}

pub fn report_json(data: &ReportData) -> String {
    serde_json::to_string_pretty(data).unwrap_or_else(|_| "{}".to_string())
}

pub fn report_markdown(data: &ReportData, since_label: &str) -> String {
    let mut md = format!("# terhub report (since {since_label})\n");
    for (name, rows) in [
        ("by day", &data.by_day),
        ("by agent", &data.by_agent),
        ("by session", &data.by_session),
    ] {
        md.push_str(&format!("\n## {name}\n\n| key | work | wait | err | idle | done | total |\n| --- | --- | --- | --- | --- | --- | --- |\n"));
        for r in rows {
            md.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} |\n",
                r.key,
                fmt_dur(r.work),
                fmt_dur(r.wait),
                fmt_dur(r.err),
                fmt_dur(r.idle),
                fmt_dur(r.done),
                fmt_dur(r.total()),
            ));
        }
    }
    md.push_str("\n## foreground\n\n| pane | focused |\n| --- | --- |\n");
    for f in &data.foreground {
        md.push_str(&format!("| {} | {} |\n", f.pane, fmt_dur(f.secs)));
    }
    md
}

pub fn fmt_dur(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 2 * 86400 {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d{}h", secs / 86400, (secs % 86400) / 3600)
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
        // Done: quiet green check — finished, not waiting, never
        // alarming. The history view doesn't have access to the
        // user's palette, so it falls back to the same light-green
        // its working state uses.
        AgentState::Done => Style::default()
            .fg(Color::LightGreen)
            .add_modifier(Modifier::BOLD),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TimelineSort {
    #[default]
    Time,
    /// Attention states first (by start), then the rest chronologically.
    Waiting,
}

/// Order spans for the timeline. Waiting sort surfaces stuck/error/waiting
/// spans above the work log without dropping anything.
pub fn sort_spans(spans: &mut [Span], sort: TimelineSort) {
    match sort {
        TimelineSort::Time => spans.sort_by_key(|s| (s.start, s.pane.clone())),
        TimelineSort::Waiting => spans.sort_by_key(|s| {
            let attention = matches!(
                s.state,
                AgentState::Waiting | AgentState::Error | AgentState::Stuck | AgentState::Limited
            );
            (!attention, s.start, s.pane.clone())
        }),
    }
}

/// Heat level of one hour: work-ish minutes map to block density.
fn heat_char(mins: u64) -> char {
    match mins {
        0 => '·',
        1..=15 => '░',
        16..=30 => '▒',
        31..=45 => '▓',
        _ => '█',
    }
}

/// Rendered line count of the heatmap: header + one row per workish
/// day (+ the empty note when there are none). Mirrors render_heatmap.
pub fn heatmap_line_count(spans: &[Span]) -> usize {
    use std::collections::HashSet;
    let mut days = HashSet::new();
    for s in spans {
        let workish = matches!(
            s.state,
            AgentState::Working
                | AgentState::Waiting
                | AgentState::Error
                | AgentState::Stuck
                | AgentState::Limited
        );
        if !workish || s.end <= s.start {
            continue;
        }
        // Hour-walk mirrors render_heatmap so multi-day spans count each row.
        let mut t = s.start;
        while t < s.end {
            days.insert(day_of(t));
            t = (t / 3600 + 1) * 3600;
        }
    }
    if days.is_empty() { 2 } else { 1 + days.len() }
}

/// Day × hour activity grid: one row per day, 24 hour cells. Only
/// work-ish states (working/waiting/error/stuck/limited) heat a cell;
/// idle and unknown stay cold.
pub fn render_heatmap(
    frame: &mut ratatui::Frame,
    spans: &[Span],
    title: &str,
    scroll: usize,
    area: ratatui::layout::Rect,
    pal: &crate::ui::Palette,
) {
    use ratatui::text::{Line, Span as TSpan};
    use ratatui::widgets::{Block, Paragraph};

    let mut days: std::collections::BTreeMap<i64, [u64; 24]> = std::collections::BTreeMap::new();
    for s in spans {
        let workish = matches!(
            s.state,
            AgentState::Working
                | AgentState::Waiting
                | AgentState::Error
                | AgentState::Stuck
                | AgentState::Limited
        );
        if !workish || s.end <= s.start {
            continue;
        }
        let mut t = s.start;
        while t < s.end {
            let hour = t / 3600;
            let hour_end = (hour + 1) * 3600;
            let take = (s.end.min(hour_end) - t).min(3600);
            let day_cells = days.entry(day_of(t)).or_insert([0; 24]);
            day_cells[(hour % 24) as usize] += take / 60 + u64::from(take % 60 > 0);
            t = hour_end;
        }
    }
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(TSpan::raw(
        "            00 02 04 06 08 10 12 14 16 18 20 22",
    )));
    for (day, cells) in &days {
        let row: String = cells.iter().map(|m| heat_char(*m)).collect();
        let total: u64 = cells.iter().sum();
        lines.push(Line::from(TSpan::raw(format!(
            "{} {} {}",
            ymd_label(*day),
            row,
            fmt_dur(total * 60)
        ))));
    }
    if days.is_empty() {
        lines.push(Line::raw("no activity in range"));
    }
    let total = lines.len();
    let visible: Vec<Line> = lines.into_iter().skip(scroll).collect();
    let block = Block::bordered()
        .title(Line::from(format!(" {title} ({total} lines) ")).style(pal.title))
        .border_style(pal.border);
    let base = if total <= 2 && days.is_empty() {
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

    #[test]
    fn since_rejects_unicode_without_panic() {
        let now = 1_000_000;
        // Multibyte input must error, never panic on a char boundary.
        assert!(parse_since("5é", now).is_err());
        assert!(parse_since("é", now).is_err());
        assert!(parse_since("", now).is_err());
        assert!(parse_since("m", now).is_err());
        assert_eq!(parse_since("90m", now).unwrap(), Some(now - 5400));
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
    fn prompt_entries_cap_and_recap_newest_first() {
        let long = "p".repeat(900);
        let e = Entry::prompt(100, "%1", "pi", &long);
        assert_eq!(e.kind, "prompt");
        assert_eq!(e.summary.chars().count(), 500);
        let entries = vec![
            Entry::prompt(100, "%1", "pi", "first"),
            entry(150, "%1", AgentState::Working),
            Entry::prompt(200, "%2", "pi", "second"),
            Entry::prompt(300, "%1", "pi", "third"),
        ];
        let all = recent_prompts(&entries, None, 10);
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].summary, "third");
        assert_eq!(all[2].summary, "first");
        let one = recent_prompts(&entries, Some("%1"), 10);
        assert_eq!(one.len(), 2);
        let capped = recent_prompts(&entries, None, 2);
        assert_eq!(capped.len(), 2);
        assert_eq!(capped[0].summary, "third");
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
    fn prune_quarantines_torn_lines_and_keeps_them() {
        // A torn line (truncated crash) must not be silently dropped:
        // it is tagged __corrupt__, the raw head is preserved in
        // `summary`, and a subsequent prune keeps the quarantine so the
        // user can see and recover it.
        let dir = std::env::temp_dir().join(format!("terhub-prune-q-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("activity.jsonl");
        let good = Entry::prompt(100, "%1", "pi", "keep me");
        append_entry(&path, &good).unwrap();
        append_raw(
            &path,
            "{\"t\":200,\"kind\":\"prompt\",\"pane\":\"%2\",\"summ",
        ); // torn
        // The torn line is NOT parsed by read_entries (filter_map ok),
        // so it does not appear in queries. corrupt_count is 0 because
        // it was never quarantined yet.
        assert_eq!(read_entries(&path).len(), 1);
        assert_eq!(corrupt_count(&path), 0);
        // Prune quarantines the torn line (tag __corrupt__), keeps the
        // good one (t=100 within 90d of now=200+90*86400), and survives
        // a second prune.
        let kept = prune(&path, 100 + 90 * 86400, 90).unwrap();
        assert_eq!(kept, 1, "prune kept the good entry");
        let entries = read_entries(&path);
        assert_eq!(
            entries.iter().filter(|e| e.kind != "__corrupt__").count(),
            1
        );
        assert_eq!(corrupt_count(&path), 1, "torn line quarantined");
        let q = entries.iter().find(|e| e.kind == "__corrupt__").unwrap();
        assert!(
            q.summary.contains("summ"),
            "raw head preserved: {}",
            q.summary
        );
        // build_spans / stats skip __corrupt__.
        let spans = build_spans(&entries, 100 + 90 * 86400);
        assert!(spans.is_empty(), "quarantines must not become spans");
        // A second prune keeps the quarantine (it doesn't re-drop it).
        prune(&path, 100 + 90 * 86400 + 86400, 90).unwrap();
        assert_eq!(corrupt_count(&path), 1, "quarantine survives a re-prune");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn utf8_torn_line_does_not_void_the_ledger() {
        // A crash mid-append that splits a multi-byte character used
        // to make read_to_string fail for the *whole file*: valid
        // entries vanished from every query and rule-fire history
        // (automation cooldowns/caps) reset. Per-line lossy decoding
        // must keep the valid line and let prune quarantine the
        // torn one like any other corrupt line.
        let dir = std::env::temp_dir().join(format!("terhub-prune-utf8-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("activity.jsonl");
        let good = Entry::prompt(100, "%1", "pi", "keep me ▸ done");
        append_entry(&path, &good).unwrap();
        // Second line torn inside a 3-byte sequence (\xe2\x82 is a
        // truncated \xe2\x82\xac).
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(b"{\"t\":200,\"kind\":\"prompt\",\"pane\":\"%2\",\"summary\":\"\xe2\x82")
                .unwrap();
        }
        // The valid entry still reads; the torn line is skipped.
        let entries = read_entries(&path);
        assert_eq!(entries.len(), 1, "valid entry lost: {entries:?}");
        assert_eq!(entries[0].pane, "%1");
        // Rule-fire history rides read_entries too: it must not reset.
        assert_eq!(read_rule_fires(&path).len(), 0);
        // Prune quarantines the torn line instead of returning 0.
        let kept = prune(&path, 100 + 90 * 86400, 90).unwrap();
        assert_eq!(kept, 1, "prune kept the good entry");
        assert_eq!(corrupt_count(&path), 1, "torn line quarantined");
        assert_eq!(read_entries(&path).len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn append_raw(path: &std::path::Path, line: &str) {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        writeln!(f, "{line}").unwrap();
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
        assert_eq!(prune(&path, 100 + 91 * 86400, 90).unwrap(), 0);
        assert!(read_entries(&path).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_reported_count_matches_disk() {
        // The count prune reports must describe what is on disk, not
        // just what was in memory: a swallowed serialization failure
        // used to return the kept count while the old ledger stayed.
        let dir = std::env::temp_dir().join(format!("terhub-prune-count-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("activity.jsonl");
        append_entry(&path, &Entry::prompt(100, "%1", "pi", "one")).unwrap();
        append_entry(&path, &Entry::prompt(200, "%1", "pi", "two")).unwrap();
        append_entry(&path, &Entry::prompt(300, "%1", "pi", "three")).unwrap();
        // now is just past 90 days after the first entry: the last
        // two stay, the first drops.
        let kept = prune(&path, 100 + 90 * 86400 + 1, 90).unwrap();
        assert_eq!(kept, 2);
        let back = read_entries(&path);
        assert_eq!(
            back.iter().filter(|e| e.kind != "__corrupt__").count(),
            kept,
            "disk disagrees with the reported count"
        );
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

    #[test]
    fn foreground_spans_track_focus_changes() {
        let entries = vec![
            Entry::human_focus(100, Some("%1")),
            Entry::human_focus(200, Some("%2")),
            Entry::human_focus(300, None),
            Entry::human_focus(400, Some("%1")),
        ];
        let spans = build_foreground(&entries, 500);
        assert_eq!(spans.len(), 3);
        assert_eq!(
            (spans[0].pane.as_str(), spans[0].start, spans[0].end),
            ("%1", 100, 200)
        );
        assert_eq!(
            (spans[1].pane.as_str(), spans[1].start, spans[1].end),
            ("%2", 200, 300)
        );
        assert_eq!(
            (spans[2].pane.as_str(), spans[2].start, spans[2].end),
            ("%1", 400, 500)
        );
        // Totals attribute per pane, most-focused first.
        let totals = foreground_totals(&spans, None);
        assert_eq!(
            totals,
            vec![
                ForeTotal {
                    pane: "%1".to_string(),
                    secs: 200
                },
                ForeTotal {
                    pane: "%2".to_string(),
                    secs: 100
                },
            ]
        );
        // A stray detach with nothing focused invents no time.
        let spans = build_foreground(&[Entry::human_focus(50, None)], 500);
        assert!(spans.is_empty());
    }

    fn sample_report() -> ReportData {
        let entries = vec![
            entry(100, "%1", AgentState::Working),
            entry(200, "%1", AgentState::Waiting),
        ];
        let spans = build_spans(&entries, 400);
        ReportData {
            by_day: stats(&spans, None, GroupBy::Day),
            by_agent: stats(&spans, None, GroupBy::Agent),
            by_session: stats(&spans, None, GroupBy::Session),
            foreground: vec![ForeTotal {
                pane: "%1".to_string(),
                secs: 250,
            }],
        }
    }

    #[test]
    fn report_json_round_trips() {
        let v: serde_json::Value = serde_json::from_str(&report_json(&sample_report())).unwrap();
        assert_eq!(v["by_agent"][0]["key"], "claude");
        assert_eq!(v["by_agent"][0]["work"], 100);
        assert_eq!(v["foreground"][0]["pane"], "%1");
        assert_eq!(v["foreground"][0]["secs"], 250);
    }

    #[test]
    fn report_markdown_total_reconciles_with_visible_columns() {
        // total() sums work+wait+err+idle+done, so the table must show
        // every one of those columns: a dropped column makes the total
        // disagree with the visible cells. Second-scale values keep
        // fmt_dur exact ("Ns"), so the sum is checkable.
        let row = StatsRow {
            key: "claude".to_string(),
            work: 10,
            wait: 20,
            err: 0,
            idle: 5,
            done: 20,
        };
        let data = ReportData {
            by_day: vec![row.clone()],
            by_agent: vec![],
            by_session: vec![],
            foreground: vec![],
        };
        let md = report_markdown(&data, "today");
        let line = md
            .lines()
            .find(|l| l.starts_with("| claude "))
            .expect("missing row:\n{md}")
            .to_string();
        let cells: Vec<&str> = line
            .split('|')
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .collect();
        // key + work + wait + err + idle + done + total.
        assert_eq!(
            cells.len(),
            7,
            "row must show every column total() sums:\n{line}"
        );
        let secs = |c: &str| c.strip_suffix('s').unwrap().parse::<u64>().unwrap();
        let visible: u64 = cells[1..6].iter().map(|c| secs(c)).sum();
        assert_eq!(
            visible,
            secs(cells[6]),
            "total must equal the visible columns:\n{line}"
        );
    }

    #[test]
    fn report_markdown_renders_tables() {
        let md = report_markdown(&sample_report(), "today");
        assert!(md.contains("## by agent"), "missing section:\n{md}");
        assert!(md.contains("claude"), "missing key:\n{md}");
        assert!(md.contains("## foreground"), "missing foreground:\n{md}");
        assert!(md.contains("%1"), "missing pane:\n{md}");
    }

    #[test]
    fn timeline_sort_waiting_leads_with_attention() {
        let entries = vec![
            entry(100, "%1", AgentState::Working),
            entry(200, "%1", AgentState::Waiting),
            entry(150, "%2", AgentState::Error),
        ];
        let mut spans = build_spans(&entries, 300);
        sort_spans(&mut spans, TimelineSort::Waiting);
        let states: Vec<AgentState> = spans.iter().map(|s| s.state).collect();
        assert_eq!(states[0], AgentState::Error);
        assert_eq!(states[1], AgentState::Waiting);
        assert_eq!(states[2], AgentState::Working);
        let mut spans = build_spans(&entries, 300);
        sort_spans(&mut spans, TimelineSort::Time);
        assert!(spans.windows(2).all(|w| w[0].start <= w[1].start));
    }

    #[test]
    fn heatmap_renders_day_rows_with_blocks() {
        use ratatui::{Terminal, backend::TestBackend};
        let entries = vec![
            entry(3600, "%1", AgentState::Working),
            entry(7200, "%1", AgentState::Idle),
        ];
        let spans = build_spans(&entries, 10800);
        let backend = TestBackend::new(100, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let pal = crate::ui::palette("dracula");
                render_heatmap(f, &spans, "timeline", 0, f.area(), &pal)
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
        assert!(text.contains("1970-01-01"), "missing day row:\n{text}");
        assert!(text.contains('█'), "missing full block:\n{text}");
    }
}
