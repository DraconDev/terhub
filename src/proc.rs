//! Process-tree agent detection. Screen patterns guess; the process tree
//! knows: walk from the pane's root pid and match descendants against the
//! agent catalog. Anything unreadable degrades to "no agent" — never an
//! invented one.
use std::collections::HashMap;
use std::path::Path;

/// Exact binary names (lowercase). Matched against the process comm, its
/// argv[0] basename, and the basename of any path-like argv token, so
/// `node /bin/pi` and `python -m aider` resolve through their wrappers.
/// Session harnesses (muxa, tmux, …) are deliberately NOT listed: they are
/// transparent, so detection descends through them to the real agent — or
/// reports nothing when the harness holds no agent.
const AGENT_BINS: &[&str] = &[
    "claude",
    "codex",
    "gemini",
    "pi",
    "muse",
    "aider",
    "crush",
    "opencode",
    "droid",
    "cursor-agent",
    "cody",
    "goose",
    "aichat",
    "sgpt",
    "kilo",
    "qwen",
    "llm",
    "continue",
    "tabnine",
    "copilot",
];

/// Versioned wrapper stems (lowercase). Match a program basename that is
/// exactly the stem or the stem plus a version suffix (digit-first, or a
/// separator run containing a digit): `muse-bin-1.4.2` resolves,
/// `muse-bin-notes` (no digit) does not. A filename merely mentioning
/// the stem is not the program.
const VERSIONED_BINS: &[(&str, &str)] = &[("muse-bin", "muse"), ("codex-bin", "codex")];

/// Path-anchored needles (lowercase). Match inside a token when followed by
/// a boundary (end, `/`, or a non-name char): `/bin/pi` resolves,
/// `/bin/pip` and `/usr/bin/pi-hole` do not.
const PATH_NEEDLES: &[(&str, &str)] = &[("/.pi/agent", "pi"), ("/bin/pi", "pi")];

/// Bare-name needles (lowercase). Match a program identity (comm, argv[0],
/// or a path token's basename) exactly or as a separator-led prefix:
/// `claude-code` and `pi-agent-core-…` resolve, while bare argument
/// tokens and URLs never match — so `ssh claude-code` and
/// `curl …/claude-code` are not agents.
const NAME_NEEDLES: &[(&str, &str)] = &[("pi-agent", "pi"), ("claude-code", "claude")];

/// Lowercased basename for agent matching.
fn proc_base(s: &str) -> String {
    s.rsplit('/').next().unwrap_or("").trim().to_lowercase()
}

/// True when `rest` (a stem remainder) looks like a version suffix rather
/// than a different name: empty, digit-first, or a separator run holding
/// a digit.
fn is_version_suffix(rest: &str) -> bool {
    if rest.is_empty() {
        return true;
    }
    match rest.chars().next() {
        Some(c) if c.is_ascii_digit() => true,
        Some('-') | Some('.') | Some('_') => rest.contains(|c: char| c.is_ascii_digit()),
        _ => false,
    }
}

/// Substring-stage match on a program basename (comm, argv[0], or a path
/// token's basename).
fn match_substr_name(base: &str) -> Option<&'static str> {
    for (stem, agent) in VERSIONED_BINS {
        if base == *stem {
            return Some(agent);
        }
        if let Some(rest) = base.strip_prefix(stem)
            && is_version_suffix(rest)
        {
            return Some(agent);
        }
    }
    for (needle, agent) in NAME_NEEDLES {
        if base == *needle {
            return Some(agent);
        }
        if let Some(rest) = base.strip_prefix(needle)
            && matches!(rest.chars().next(), Some('-') | Some('.') | Some('_'))
        {
            return Some(agent);
        }
    }
    None
}

/// Substring-stage match on one full token: path needles with a trailing
/// boundary, plus basename matching for path-like tokens only (a bare
/// `muse-bin-notes.txt` argument is a filename, not a program).
fn match_substr_token(tok: &str) -> Option<&'static str> {
    for (needle, agent) in PATH_NEEDLES {
        let mut start = 0;
        while let Some(i) = tok[start..].find(needle) {
            let after = &tok[start + i + needle.len()..];
            let boundary = after.is_empty()
                || after.starts_with('/')
                || after.chars().next().is_some_and(|c| {
                    !(c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')
                });
            if boundary {
                return Some(agent);
            }
            start += i + 1;
        }
    }
    if tok.contains('/') {
        return match_substr_name(&proc_base(tok));
    }
    None
}

/// Launcher wrappers that spawn (not exec) their payload, so tmux reports
/// the wrapper — not the program — as the pane's foreground command.
/// Shells are deliberately absent: diving into a foreground shell's
/// children would surface background jobs, not the foreground program.
const CMD_WRAPPERS: &[&str] = &[
    "systemd-inhibit",
    "systemd-run",
    "timeout",
    "sudo",
    "doas",
    "rlwrap",
    "unbuffer",
    "expect",
    "script",
    "sg",
    "chrt",
    "ionice",
    "taskset",
    "numactl",
    "setarch",
    "nsenter",
];

/// True when `name` (a tmux-reported foreground command) is a launcher
/// wrapper worth unwrapping. Lets callers skip the /proc snapshot when
/// nothing on screen can benefit from it.
pub fn is_launcher_wrapper(name: &str) -> bool {
    CMD_WRAPPERS.contains(&name)
}

/// Auth-prompt companions (polkit, askpass) that ride along as extra
/// children of a wrapper: never the payload, ignored when descending.
const HELPER_PROCS: &[&str] = &["pkttyagent", "ssh-askpass", "systemd-ask-password"];

/// Terminal editors an agent summons via $EDITOR. Matched case-insensitively
/// against comm or argv[0]; plain user editing never matches without agent
/// context around it.
const EDITOR_NAMES: &[&str] = &["micro", "vim", "nvim", "vi", "nano", "emacs", "helix", "hx"];

pub fn is_editor_name(name: &str) -> bool {
    EDITOR_NAMES.contains(&name.to_lowercase().as_str())
}

/// argv[0] basename of a raw cmdline (`\0`- or space-separated).
fn argv0_base(cmdline: &str) -> &str {
    cmdline
        .split([' ', '\0', '\t'])
        .find(|t| !t.is_empty())
        .map(|a| a.rsplit('/').next().unwrap_or(""))
        .unwrap_or("")
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProcInfo {
    pid: u32,
    ppid: u32,
    comm: String,
    cmdline: String,
    /// utime + stime in clock ticks.
    ticks: u64,
    /// starttime in clock ticks since boot (stat field 22): pid identity
    /// across reuse — same pid with a different start is a new process.
    start: u64,
}

/// An agent process found under a pane root: the nearest match, whose
/// subtree CPU is tracked for stuck detection. Wrappers match too (their
/// cmdline embeds the child command), which is fine — the subtree sum
/// still captures the real agent's CPU.
///
/// Detection answers presence only (is an agent descended from this pane?),
/// never foreground intent; classification decides what it means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentHit {
    pub agent: String,
    pub pid: u32,
    /// starttime of the matched process: continuity checks compare this so
    /// a recycled pid never inherits the previous owner's still-clock.
    pub start: u64,
}

/// One point-in-time view of the process table, shared across all panes of
/// a daemon poll so /proc is walked once, not once per pane.
#[derive(Debug, Default)]
pub struct Snapshot {
    procs: HashMap<u32, ProcInfo>,
    kids: HashMap<u32, Vec<u32>>,
}

impl Snapshot {
    pub fn capture() -> Self {
        Self::capture_from(Path::new("/proc"))
    }

    fn capture_from(root: &Path) -> Self {
        let mut snap = Snapshot::default();
        let entries = match std::fs::read_dir(root) {
            Ok(e) => e,
            Err(_) => return snap,
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let pid: u32 = match name.to_string_lossy().parse() {
                Ok(p) => p,
                Err(_) => continue,
            };
            if let Some(info) = read_proc(&entry.path(), pid) {
                snap.kids.entry(info.ppid).or_default().push(pid);
                snap.procs.insert(pid, info);
            }
        }
        for kids in snap.kids.values_mut() {
            kids.sort_unstable();
        }
        snap
    }

    /// Nearest agent process at or below `root` (breadth-first, lowest pid
    /// wins ties). Wrappers (shells, systemd-inhibit, node) are transparent:
    /// every descendant is matched, not just direct children.
    pub fn detect_agent(&self, root: u32) -> Option<AgentHit> {
        if root == 0 || !self.procs.contains_key(&root) {
            return None;
        }
        let mut queue = std::collections::VecDeque::from([root]);
        let mut seen = std::collections::HashSet::from([root]);
        while let Some(pid) = queue.pop_front() {
            let info = &self.procs[&pid];
            if let Some(agent) = match_proc(&info.comm, &info.cmdline) {
                return Some(AgentHit {
                    agent: agent.to_string(),
                    pid,
                    start: info.start,
                });
            }
            if let Some(kids) = self.kids.get(&pid) {
                for kid in kids {
                    if seen.insert(*kid) {
                        queue.push_back(*kid);
                    }
                }
            }
        }
        None
    }

    /// True when `pid`'s subtree contains a terminal editor: the agent
    /// summoned its $EDITOR and is Waiting on the user. Scoped to this
    /// subtree only, so a background agent never claims a sibling editor.
    pub fn subtree_has_editor(&self, pid: u32) -> bool {
        if !self.procs.contains_key(&pid) {
            return false;
        }
        let mut queue = std::collections::VecDeque::from([pid]);
        let mut seen = std::collections::HashSet::from([pid]);
        while let Some(p) = queue.pop_front() {
            if is_editor_name(&self.procs[&p].comm) {
                return true;
            }
            if let Some(kids) = self.kids.get(&p) {
                for kid in kids {
                    if seen.insert(*kid) {
                        queue.push_back(*kid);
                    }
                }
            }
        }
        false
    }

    /// Display command for a pane: tmux's reported foreground command
    /// with launcher wrappers unwrapped to their payload. Descent starts
    /// at the reported process (found at or below `root`) and continues
    /// only through single-child wrappers, so ambiguity keeps tmux's word
    /// and foreground shells are never dived into. The payload names its
    /// agent when one matches down the chain, else its own argv[0].
    /// Anything unreadable keeps `reported` — never an invented command.
    pub fn display_command(&self, root: u32, reported: &str) -> String {
        if root == 0 || !self.procs.contains_key(&root) {
            return reported.to_string();
        }
        let mut queue = std::collections::VecDeque::from([root]);
        let mut seen = std::collections::HashSet::from([root]);
        let mut start = None;
        while let Some(pid) = queue.pop_front() {
            let info = &self.procs[&pid];
            if info.comm == reported || argv0_base(&info.cmdline) == reported {
                start = Some(pid);
                break;
            }
            if let Some(kids) = self.kids.get(&pid) {
                for kid in kids {
                    if seen.insert(*kid) {
                        queue.push_back(*kid);
                    }
                }
            }
        }
        let mut pid = match start {
            Some(p) => p,
            None => return reported.to_string(),
        };
        // Only a wrapper licenses the descent: anything else is already
        // the true foreground program.
        if !self.is_wrapper(pid) {
            return reported.to_string();
        }
        let mut chain = vec![pid];
        while chain.len() < 9 {
            // Exactly one real child: helpers (polkit, askpass) don't count.
            let kid = match self.kids.get(&pid) {
                Some(k) => {
                    let mut live = k
                        .iter()
                        .filter(|k| !chain.contains(*k) && !self.is_helper(**k));
                    match (live.next(), live.next()) {
                        (Some(&kid), None) => kid,
                        _ => break,
                    }
                }
                None => break,
            };
            chain.push(kid);
            pid = kid;
            if !self.is_wrapper(pid) {
                break;
            }
        }
        if chain.len() == 1 {
            // Childless or ambiguous wrapper: keep tmux's word.
            return reported.to_string();
        }
        for pid in &chain {
            if let Some(info) = self.procs.get(pid)
                && let Some(agent) = match_proc(&info.comm, &info.cmdline)
            {
                return agent.to_string();
            }
        }
        match self.procs.get(&pid) {
            Some(info) => {
                let base = argv0_base(&info.cmdline);
                if base.is_empty() {
                    info.comm.clone()
                } else {
                    base.to_string()
                }
            }
            None => reported.to_string(),
        }
    }

    fn is_wrapper(&self, pid: u32) -> bool {
        self.procs.get(&pid).is_some_and(|info| {
            CMD_WRAPPERS.contains(&info.comm.as_str())
                || CMD_WRAPPERS.contains(&argv0_base(&info.cmdline))
        })
    }

    fn is_helper(&self, pid: u32) -> bool {
        self.procs.get(&pid).is_some_and(|info| {
            HELPER_PROCS.contains(&info.comm.as_str())
                || HELPER_PROCS.contains(&argv0_base(&info.cmdline))
        })
    }

    /// Total CPU ticks of `pid` and all its descendants (0 when unknown).
    /// Compared across polls: an unchanged sum means nothing in the agent's
    /// tree did any work. Immune to pid reuse — the tree is re-walked from
    /// the pane root every poll, and a shrunk sum (restarted agent) resets
    /// the still-clock rather than tripping stuck.
    pub fn subtree_ticks(&self, pid: u32) -> u64 {
        let mut total = 0;
        let mut queue = vec![pid];
        let mut seen = std::collections::HashSet::from([pid]);
        while let Some(p) = queue.pop() {
            if let Some(info) = self.procs.get(&p) {
                total += info.ticks;
            }
            if let Some(kids) = self.kids.get(&p) {
                for kid in kids {
                    if seen.insert(*kid) {
                        queue.push(*kid);
                    }
                }
            }
        }
        total
    }
}

/// Canonical agent name for a process, or None. Exact stages first (comm,
/// argv[0], path-token basenames), then per-token substring needles with
/// boundaries — never a flat whole-cmdline contains, which cried agent
/// at `pip`, filenames, and URLs.
/// Canonical agent name for a hook-provided label: resolve through the
/// same pipeline as process matching (`claude-code` → `claude`,
/// `muse-bin-1.4.2` → `muse`), lowercasing anything unrecognized, so hook
/// records compare equal to proc/screen records downstream.
/// State letter from `/proc/<pid>/stat` (None when unreadable/gone).
pub fn proc_state(pid: u32) -> Option<char> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let tail = stat.rsplit_once(')')?.1;
    tail.split_whitespace().next()?.chars().next()
}

/// True when the process is currently job-stopped (T) or traced-stop (t).
pub fn is_stopped(pid: u32) -> bool {
    matches!(proc_state(pid), Some('T') | Some('t'))
}

/// Process group id from `/proc/<pid>/stat` (field 5).
fn proc_pgrp(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let tail = stat.rsplit_once(')')?.1;
    tail.split_whitespace().nth(2)?.parse().ok()
}

fn signal_tree(pid: u32, sig: libc::c_int, name: &str) -> anyhow::Result<()> {
    if pid <= 1 {
        anyhow::bail!("refusing to signal pid {pid}");
    }
    // tmux puts each pane root in its own process group, so a negative
    // pid stops the whole tree; a non-leader falls back to the lone pid
    // rather than spraying a foreign group.
    let target = if proc_pgrp(pid) == Some(pid) {
        -(pid as libc::pid_t)
    } else {
        pid as libc::pid_t
    };
    // SAFETY: kill() with a plain signal number has no memory effects.
    if unsafe { libc::kill(target, sig) } == 0 {
        return Ok(());
    }
    anyhow::bail!("{name} {pid}: {}", std::io::Error::last_os_error())
}

/// Freeze a pane's process tree (SIGSTOP); reversible via resume_group.
pub fn pause_group(pid: u32) -> anyhow::Result<()> {
    signal_tree(pid, libc::SIGSTOP, "pause")
}

/// Thaw a tree frozen by pause_group (SIGCONT).
pub fn resume_group(pid: u32) -> anyhow::Result<()> {
    signal_tree(pid, libc::SIGCONT, "resume")
}

pub fn canonical_agent(name: &str) -> String {
    match_proc("", name)
        .map(str::to_string)
        .unwrap_or_else(|| name.trim().to_lowercase())
}

fn match_proc(comm: &str, cmdline: &str) -> Option<&'static str> {
    let find_bin = |name: &str| AGENT_BINS.iter().find(|b| **b == name).copied();
    if let Some(agent) = find_bin(&proc_base(comm)) {
        return Some(agent);
    }
    let mut tokens = cmdline.split([' ', '\0', '\t']).filter(|t| !t.is_empty());
    let argv0 = tokens.next().unwrap_or("");
    if let Some(agent) = find_bin(&proc_base(argv0)) {
        return Some(agent);
    }
    let rest: Vec<&str> = tokens.collect();
    for tok in &rest {
        if !tok.contains('/') {
            continue;
        }
        if let Some(agent) = find_bin(&proc_base(tok)) {
            return Some(agent);
        }
    }
    if let Some(agent) = match_substr_name(&proc_base(comm)) {
        return Some(agent);
    }
    if let Some(agent) = match_substr_name(&proc_base(argv0)) {
        return Some(agent);
    }
    if !argv0.contains("://")
        && let Some(agent) = match_substr_token(argv0)
    {
        return Some(agent);
    }
    for tok in rest {
        // Remote identifiers aren't local programs.
        if tok.contains("://") {
            continue;
        }
        if let Some(agent) = match_substr_token(tok) {
            return Some(agent);
        }
    }
    None
}

fn read_proc(dir: &Path, pid: u32) -> Option<ProcInfo> {
    let comm = std::fs::read_to_string(dir.join("comm"))
        .ok()?
        .trim()
        .to_string();
    let raw = std::fs::read(dir.join("cmdline")).ok()?;
    let cmdline = String::from_utf8_lossy(&raw).into_owned();
    let stat = std::fs::read_to_string(dir.join("stat")).ok()?;
    // comm lives in parens and may itself contain parens: split at the last.
    let tail = stat.rsplit_once(')')?.1;
    let f: Vec<&str> = tail.split_whitespace().collect();
    // After comm: state(0) ppid(1) ... utime(11) stime(12) ... starttime(19).
    if f.len() < 20 {
        return None;
    }
    let num = |i: usize| f.get(i).and_then(|s| s.parse().ok()).unwrap_or(0);
    let ticks: u64 = num(11) + num(12);
    Some(ProcInfo {
        pid,
        ppid: num(1) as u32,
        comm,
        cmdline,
        ticks,
        start: num(19),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    /// Minimal /proc stand-in: (pid, ppid, comm, cmdline, utime, stime, start).
    /// `tag` keeps parallel tests out of each other's fixture dir.
    fn fake_proc(tag: &str, entries: &[(&str, &str, &str, &str, u64, u64, u64)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("terhub-proc-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        for (pid, ppid, comm, cmdline, utime, stime, start) in entries {
            let d = dir.join(pid);
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("comm"), format!("{comm}\n")).unwrap();
            fs::write(d.join("cmdline"), cmdline.replace(' ', "\0")).unwrap();
            // state ppid pgrp session tty tpgid flags minflt cminflt majflt
            // cmajflt utime stime cutime cstime pri nice threads itreal starttime
            fs::write(
                d.join("stat"),
                format!(
                    "{pid} ({comm}) S {ppid} 0 0 0 0 0 0 0 0 0 {utime} {stime} 0 0 0 0 0 0 {start}"
                ),
            )
            .unwrap();
        }
        dir
    }

    #[test]
    fn detects_pi_through_wrappers() {
        // The %192 shape: zsh → systemd-inhibit → node /bin/pi.
        let dir = fake_proc(
            "wrappers",
            &[
                ("100", "1", "zsh", "-zsh", 10, 5, 1000),
                (
                    "101",
                    "100",
                    "systemd-inhibit",
                    "systemd-inhibit --who=pi --mode=block node /bin/pi --approve",
                    0,
                    0,
                    1001,
                ),
                (
                    "102",
                    "101",
                    "node",
                    "node --import /home/u/.pi/agent/patches/x.mjs /home/u/.npm-global/bin/pi --approve",
                    400,
                    60,
                    1002,
                ),
            ],
        );
        let snap = Snapshot::capture_from(&dir);
        // Nearest match is the wrapper (its cmdline embeds /bin/pi); the
        // subtree sum still captures the real agent's CPU through it.
        let hit = snap.detect_agent(100).expect("no hit");
        assert_eq!((hit.agent.as_str(), hit.pid), ("pi", 101));
        assert_eq!(snap.subtree_ticks(hit.pid), 460);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn direct_binary_matches() {
        let dir = fake_proc(
            "direct",
            &[
                ("200", "1", "zsh", "-zsh", 0, 0, 2000),
                ("201", "200", "claude", "claude chat", 7, 3, 2001),
            ],
        );
        let snap = Snapshot::capture_from(&dir);
        let hit = snap.detect_agent(200).expect("no hit");
        assert_eq!((hit.agent.as_str(), hit.pid), ("claude", 201));
        assert_eq!(snap.subtree_ticks(201), 10);
        assert_eq!(snap.subtree_ticks(999), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn versioned_muse_wrapper_maps_to_muse() {
        // Muse Code's versioned binary is its own agent, not claude.
        let dir = fake_proc(
            "substr",
            &[(
                "300",
                "1",
                "muse-bin-1.4.2-R4684.1",
                "/home/u/.local/bin/muse-bin-1.4.2-R4684.1 --resume",
                1,
                1,
                3000,
            )],
        );
        let snap = Snapshot::capture_from(&dir);
        assert_eq!(
            snap.detect_agent(300).map(|h| h.agent),
            Some("muse".to_string())
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn bare_words_are_not_agents() {
        // `grep pi file`: "pi" is a bare token, not a path — must not match.
        let dir = fake_proc(
            "barewords",
            &[
                ("400", "1", "zsh", "-zsh", 0, 0, 4000),
                ("401", "400", "grep", "grep pi file", 0, 0, 4001),
                ("402", "400", "micro", "micro note.md", 0, 0, 4002),
            ],
        );
        let snap = Snapshot::capture_from(&dir);
        assert_eq!(snap.detect_agent(400), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    #[test]
    fn minimax_code_detects_as_agent() {
        let dir = fake_proc(
            "minimax",
            &[(
                "810",
                "1",
                "minimax-code",
                "minimax-code --yolo",
                0,
                0,
                8010,
            )],
        );
        let snap = Snapshot::capture_from(&dir);
        assert_eq!(
            snap.detect_agent(810).map(|h| h.agent),
            Some("minimax-code".to_string())
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn substr_needles_need_token_boundaries() {
        // /bin/pip is not /bin/pi.
        let dir = fake_proc(
            "notpip",
            &[("800", "1", "pip", "/bin/pip install foo", 0, 0, 8000)],
        );
        let snap = Snapshot::capture_from(&dir);
        assert_eq!(snap.detect_agent(800), None);
        let _ = fs::remove_dir_all(&dir);
        // A filename merely mentioning an agent binary is not the agent.
        let dir = fake_proc(
            "notnotes",
            &[("801", "1", "nvim", "nvim muse-bin-notes.txt", 0, 0, 8001)],
        );
        let snap = Snapshot::capture_from(&dir);
        assert_eq!(snap.detect_agent(801), None);
        let _ = fs::remove_dir_all(&dir);
        // A URL path segment is not a local program.
        let dir = fake_proc(
            "noturl",
            &[(
                "802",
                "1",
                "curl",
                "curl https://example.com/claude-code",
                0,
                0,
                8002,
            )],
        );
        let snap = Snapshot::capture_from(&dir);
        assert_eq!(snap.detect_agent(802), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn substr_needles_keep_true_wrappers() {
        // Versioned wrapper binaries still resolve.
        let dir = fake_proc(
            "stillver",
            &[(
                "803",
                "1",
                "muse-bin-1.4.2-R4684.1",
                "/home/u/.local/bin/muse-bin-1.4.2-R4684.1 --resume",
                1,
                1,
                8003,
            )],
        );
        let snap = Snapshot::capture_from(&dir);
        assert_eq!(
            snap.detect_agent(803).map(|h| h.agent),
            Some("muse".to_string())
        );
        let _ = fs::remove_dir_all(&dir);
        // pi patch-loader paths still resolve.
        let dir = fake_proc(
            "stillpatch",
            &[(
                "804",
                "1",
                "node",
                "node --import /home/u/.pi/agent/patches/x.mjs script.js",
                1,
                1,
                8004,
            )],
        );
        let snap = Snapshot::capture_from(&dir);
        assert_eq!(
            snap.detect_agent(804).map(|h| h.agent),
            Some("pi".to_string())
        );
        let _ = fs::remove_dir_all(&dir);
        // The claude-code binary itself still resolves.
        let dir = fake_proc(
            "stillcc",
            &[(
                "805",
                "1",
                "claude-code",
                "claude-code --resume",
                1,
                1,
                8005,
            )],
        );
        let snap = Snapshot::capture_from(&dir);
        assert_eq!(
            snap.detect_agent(805).map(|h| h.agent),
            Some("claude".to_string())
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn subtree_editor_distinguishes_child_from_sibling() {
        let dir = fake_proc(
            "edown",
            &[
                ("900", "1", "zsh", "-zsh", 0, 0, 9000),
                // Background agent with a sibling editor: not its $EDITOR.
                ("901", "900", "pi", "pi", 0, 0, 9001),
                ("902", "900", "micro", "micro notes.md", 0, 0, 9002),
                ("910", "1", "zsh", "-zsh", 0, 0, 9010),
                // Agent with its own $EDITOR child.
                ("911", "910", "pi", "pi", 0, 0, 9011),
                ("912", "911", "micro", "micro msg.md", 0, 0, 9012),
            ],
        );
        let snap = Snapshot::capture_from(&dir);
        assert!(!snap.subtree_has_editor(901));
        assert!(snap.subtree_has_editor(911));
        assert!(!snap.subtree_has_editor(999));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn hit_carries_process_starttime() {
        // Distinct starttimes so a pid-reuse mixup is observable.
        let dir = fake_proc(
            "start",
            &[
                ("920", "1", "zsh", "-zsh", 0, 0, 9200),
                ("921", "920", "pi", "pi", 30, 20, 9210),
            ],
        );
        let snap = Snapshot::capture_from(&dir);
        let hit = snap.detect_agent(920).expect("no hit");
        assert_eq!(hit.agent, "pi");
        assert_eq!(hit.pid, 921);
        assert_eq!(hit.start, 9210);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn canonical_agent_resolves_aliases() {
        assert_eq!(canonical_agent("claude-code"), "claude");
        assert_eq!(canonical_agent("muse-bin-1.4.2"), "muse");
        assert_eq!(canonical_agent("pi-agent-core-x"), "pi");
        assert_eq!(canonical_agent("Claude"), "claude");
        assert_eq!(canonical_agent("myagent"), "myagent");
        assert_eq!(canonical_agent(""), "");
    }

    #[test]
    fn nearest_agent_wins() {
        // Deep codex under the first child, shallow pi under the second:
        // breadth-first finds pi despite the higher pid.
        let dir = fake_proc(
            "nearest",
            &[
                ("600", "1", "zsh", "-zsh", 0, 0, 6000),
                ("601", "600", "zsh", "zsh", 0, 0, 6001),
                ("602", "601", "codex", "codex", 0, 0, 6002),
                ("603", "600", "pi", "pi", 0, 0, 6003),
            ],
        );
        let snap = Snapshot::capture_from(&dir);
        assert_eq!(
            snap.detect_agent(600).map(|h| (h.agent, h.pid)),
            Some(("pi".to_string(), 603))
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn harness_is_transparent() {
        // zsh → muxa watch → claude: the harness is not the agent.
        let dir = fake_proc(
            "harness",
            &[
                ("700", "1", "zsh", "-zsh", 0, 0, 7000),
                ("701", "700", "muxa", "muxa watch", 5, 5, 7001),
                ("702", "701", "claude", "claude", 3, 3, 7002),
            ],
        );
        let snap = Snapshot::capture_from(&dir);
        assert_eq!(
            snap.detect_agent(700).map(|h| (h.agent, h.pid)),
            Some(("claude".to_string(), 702))
        );
        let _ = fs::remove_dir_all(&dir);
        // Lone harness with no agent inside: nothing, not "muxa".
        let dir = fake_proc(
            "harness-lone",
            &[
                ("710", "1", "zsh", "-zsh", 0, 0, 7100),
                ("711", "710", "muxa", "muxa watch", 5, 5, 7101),
            ],
        );
        let snap = Snapshot::capture_from(&dir);
        assert_eq!(snap.detect_agent(710), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_or_missing_pids_match_nothing() {
        let dir = fake_proc("missing", &[("500", "1", "zsh", "-zsh", 0, 0, 5000)]);
        let snap = Snapshot::capture_from(&dir);
        assert_eq!(snap.detect_agent(0), None);
        assert_eq!(snap.detect_agent(999), None);
        assert_eq!(snap.detect_agent(500), None);
        let empty = Snapshot::capture_from(&dir.join("nope-missing"));
        assert_eq!(empty.detect_agent(500), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unwraps_inhibit_chain_to_agent() {
        // The screenshot shape: zsh → systemd-inhibit → node /bin/pi.
        let dir = fake_proc(
            "unwrap",
            &[
                ("100", "1", "zsh", "-zsh", 10, 5, 1000),
                (
                    "101",
                    "100",
                    "systemd-inhibit",
                    "systemd-inhibit --who=pi --mode=block node /bin/pi --approve",
                    0,
                    0,
                    1001,
                ),
                (
                    "102",
                    "101",
                    "node",
                    "node /home/u/.npm-global/bin/pi --approve",
                    400,
                    60,
                    1002,
                ),
            ],
        );
        let snap = Snapshot::capture_from(&dir);
        assert_eq!(snap.display_command(100, "systemd-inhibit"), "pi");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn display_command_keeps_shells_and_ambiguity() {
        let dir = fake_proc(
            "keep",
            &[
                // Foreground shell with a background job: never dive in.
                ("200", "1", "zsh", "-zsh", 0, 0, 2000),
                ("201", "200", "sleep", "sleep 100", 0, 0, 2001),
                // Wrapper with two children: ambiguous, keep tmux's word.
                ("300", "1", "systemd-inhibit", "systemd-inhibit", 0, 0, 3000),
                ("301", "300", "pi", "pi", 0, 0, 3001),
                ("302", "300", "codex", "codex", 0, 0, 3002),
            ],
        );
        let snap = Snapshot::capture_from(&dir);
        assert_eq!(snap.display_command(200, "zsh"), "zsh");
        assert_eq!(
            snap.display_command(300, "systemd-inhibit"),
            "systemd-inhibit"
        );
        assert_eq!(snap.display_command(999, "whatever"), "whatever");
        assert_eq!(snap.display_command(0, "zsh"), "zsh");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn display_command_shows_non_agent_payload() {
        // sudo → micro: no agent anywhere, argv[0] of the payload wins.
        let dir = fake_proc(
            "payload",
            &[
                ("400", "1", "zsh", "-zsh", 0, 0, 4000),
                ("401", "400", "sudo", "sudo micro note.md", 0, 0, 4001),
                ("402", "401", "micro", "micro note.md", 0, 0, 4002),
                ("500", "1", "timeout", "timeout 3600 pi", 0, 0, 5000),
                ("501", "500", "pi", "pi", 0, 0, 5001),
            ],
        );
        let snap = Snapshot::capture_from(&dir);
        assert_eq!(snap.display_command(400, "sudo"), "micro");
        assert_eq!(snap.display_command(500, "timeout"), "pi");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn helper_companions_do_not_block_descent() {
        // The live %77 shape: systemd-inhibit with a pkttyagent companion
        // next to the real payload. The auth prompt is never the payload.
        let dir = fake_proc(
            "helper",
            &[
                ("700", "1", "zsh", "-zsh", 0, 0, 7000),
                (
                    "701",
                    "700",
                    "systemd-inhibit",
                    "systemd-inhibit --mode=block pi",
                    0,
                    0,
                    7001,
                ),
                (
                    "702",
                    "701",
                    "pkttyagent",
                    "/run/current-system/sw/bin/pkttyagent --notify-fd 6 --fallback",
                    0,
                    0,
                    7002,
                ),
                ("703", "701", "pi", "pi", 0, 0, 7003),
            ],
        );
        let snap = Snapshot::capture_from(&dir);
        assert_eq!(snap.display_command(700, "systemd-inhibit"), "pi");
        let _ = fs::remove_dir_all(&dir);
    }
}
