//! Backend: tmux session management via the tmux CLI.
use anyhow::{Context, Result, bail};
use std::env;
use std::path::PathBuf;
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub name: String,
    pub windows: u32,
    pub attached: bool,
    pub created: String,
    pub activity: String,
}

impl Session {
    #[cfg(test)]
    pub fn test(name: &str) -> Self {
        Session {
            name: name.to_string(),
            windows: 1,
            attached: false,
            created: String::new(),
            activity: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pane {
    pub id: String,
    pub session: String,
    pub window_id: String,
    pub window_name: String,
    pub window_active: bool,
    pub index: u32,
    pub command: String,
    pub path: String,
    pub active: bool,
    pub width: u32,
    pub height: u32,
    /// Last activity in the pane's window, epoch seconds (tmux tracks
    /// activity per window, not per pane; 0 when unknown).
    pub activity: u64,
    /// Root process id of the pane (0 when unknown).
    pub pid: u32,
}

impl Pane {
    #[cfg(test)]
    pub fn test(id: &str, window: &str, command: &str) -> Self {
        Pane {
            id: id.to_string(),
            session: "test".to_string(),
            window_id: "@0".to_string(),
            window_name: window.to_string(),
            window_active: true,
            index: 0,
            command: command.to_string(),
            path: "/tmp".to_string(),
            active: true,
            width: 80,
            height: 24,
            activity: 0,
            pid: 0,
        }
    }
}

/// Field separator for tmux `-F` formats: ASCII unit separator. A tab
/// separator collides with tabs inside window names, paths, and session
/// names, shifting every later field; 0x1f passes through tmux untouched
/// and never appears in names.
const SEP: char = '\x1f';

/// Field counts of the two formats below: more parts than this means a
/// separator smuggled inside a field, and the line is skipped.
const PANE_FIELDS: usize = 13;
const SESSION_FIELDS: usize = 5;

const PANE_FORMAT: &str = "#{pane_id}\x1f#{window_id}\x1f#{window_name}\x1f#{window_active}\x1f#{pane_index}\x1f#{pane_current_command}\x1f#{pane_current_path}\x1f#{pane_active}\x1f#{pane_width}\x1f#{pane_height}\x1f#{session_name}\x1f#{window_activity}\x1f#{pane_pid}";

fn parse_panes(out: &str) -> Vec<Pane> {
    let mut panes = Vec::new();
    for line in out.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split(SEP).collect();
        // Overflow fails closed: a shifted pid would walk the wrong
        // process tree. Short lines still parse leniently (older tmux).
        if parts.len() > PANE_FIELDS {
            continue;
        }
        let get = |i: usize| parts.get(i).unwrap_or(&"").to_string();
        let flag = |i: usize| {
            !matches!(
                parts.get(i).map(|s| s.trim()),
                None | Some("0") | Some("") | Some("no")
            )
        };
        let num = |i: usize| -> u64 { parts.get(i).and_then(|s| s.parse().ok()).unwrap_or(0) };
        panes.push(Pane {
            id: get(0),
            window_id: get(1),
            window_name: get(2),
            window_active: flag(3),
            index: num(4) as u32,
            command: get(5),
            path: get(6),
            active: flag(7),
            width: num(8) as u32,
            height: num(9) as u32,
            session: get(10),
            activity: num(11),
            pid: num(12) as u32,
        });
    }
    panes
}

fn parse_sessions(out: &str) -> Vec<Session> {
    let mut sessions = Vec::new();
    for line in out.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split('\t').collect();
        let get = |i: usize| parts.get(i).unwrap_or(&"").to_string();
        let windows = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        let attached = !matches!(
            parts.get(2).map(|s| s.trim()),
            None | Some("0") | Some("") | Some("no")
        );
        sessions.push(Session {
            name: get(0),
            windows,
            attached,
            created: get(3),
            activity: get(4),
        });
    }
    sessions
}

/// Unwrap launcher wrappers (`systemd-inhibit`, `sudo`, …) to the payload
/// program, so the Cmd column names what runs instead of what launched it.
/// The /proc snapshot is skipped entirely when no pane reports a wrapper.
fn label_commands(mut panes: Vec<Pane>) -> Vec<Pane> {
    if !panes
        .iter()
        .any(|p| p.pid != 0 && crate::proc::is_launcher_wrapper(&p.command))
    {
        return panes;
    }
    let snap = crate::proc::Snapshot::capture();
    for p in &mut panes {
        p.command = snap.display_command(p.pid, &p.command);
    }
    panes
}

fn find_tmux() -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|dir| dir.join("tmux"))
        .find(|p| p.is_file())
}

pub struct Tmux {
    bin: PathBuf,
}

impl Tmux {
    pub fn new() -> Result<Self> {
        find_tmux()
            .map(|bin| Tmux { bin })
            .context("tmux not found on PATH")
    }

    pub fn available() -> bool {
        find_tmux().is_some()
    }

    fn run(&self, args: &[&str]) -> Result<String> {
        let out = Command::new(&self.bin)
            .args(args)
            .output()
            .context("failed to run tmux")?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            let mut msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
            if msg.is_empty() {
                msg = String::from_utf8_lossy(&out.stdout).trim().to_string();
            }
            if msg.is_empty() {
                msg = "tmux failed".to_string();
            }
            bail!("{msg}");
        }
    }

    pub fn list_sessions(&self) -> Result<Vec<Session>> {
        let out = match self.run(&[
            "list-sessions",
            "-F",
            "#{session_name}\x1f#{session_windows}\x1f#{session_attached}\x1f#{t:session_created}\x1f#{t:session_activity}",
        ]) {
            Ok(o) => o,
            Err(e) => {
                let m = e.to_string().to_lowercase();
                if m.contains("no server running") || m.contains("no sessions") {
                    return Ok(vec![]);
                }
                return Err(e);
            }
        };
        Ok(parse_sessions(&out))
    }

    pub fn session_exists(&self, name: &str) -> bool {
        self.run(&["has-session", "-t", name]).is_ok()
    }

    pub fn new_session(
        &self,
        name: &str,
        start_dir: Option<&str>,
        window_name: Option<&str>,
    ) -> Result<Session> {
        let name = name.trim();
        if name.is_empty() {
            bail!("session name must not be empty");
        }
        let mut args = vec!["new-session", "-d", "-s", name];
        if let Some(d) = start_dir {
            args.extend(["-c", d]);
        }
        if let Some(w) = window_name {
            args.extend(["-n", w]);
        }
        self.run(&args)?;
        Ok(self
            .list_sessions()?
            .into_iter()
            .find(|s| s.name == name)
            .unwrap_or(Session {
                name: name.to_string(),
                windows: 1,
                attached: false,
                created: String::new(),
                activity: String::new(),
            }))
    }

    pub fn kill_session(&self, name: &str) -> Result<()> {
        self.run(&["kill-session", "-t", name]).map(|_| ())
    }

    /// Type literal text into a pane, optionally submitting with Enter.
    pub fn send_keys(&self, pane: &str, text: &str, submit: bool) -> Result<()> {
        self.run(&["send-keys", "-l", "-t", pane, "--", text])?;
        if submit {
            self.run(&["send-keys", "-t", pane, "Enter"])?;
        }
        Ok(())
    }

    /// Interrupt whatever runs in a pane (Ctrl-C).
    pub fn interrupt(&self, pane: &str) -> Result<()> {
        self.run(&["send-keys", "-t", pane, "C-c"]).map(|_| ())
    }

    pub fn rename_session(&self, old: &str, new: &str) -> Result<()> {
        if new.trim().is_empty() {
            bail!("new name must not be empty");
        }
        self.run(&["rename-session", "-t", old, new]).map(|_| ())
    }

    /// Attach arguments (without argv[0]), honoring inside-vs-outside tmux.
    pub fn attach_args(name: &str) -> Vec<String> {
        let verb = if env::var("TMUX").is_ok() {
            "switch-client"
        } else {
            "attach-session"
        };
        vec![verb.to_string(), "-t".to_string(), name.to_string()]
    }

    /// Attach replaces this process with a tmux client, which needs a real
    /// terminal. Callers that mutate server state first must check this
    /// upfront: a failed exec'd attach can't clean up (the image is gone).
    fn need_tty() -> Result<()> {
        use std::io::IsTerminal;
        if std::io::stdout().is_terminal() {
            Ok(())
        } else {
            bail!("cannot attach: no terminal on stdout")
        }
    }

    /// Replace this process with the attach command (call after TUI exits).
    /// Only returns on failure.
    pub fn attach_here(&self, name: &str) -> Result<()> {
        use std::os::unix::process::CommandExt;
        Self::need_tty()?;
        let args = Self::attach_args(name);
        Err(Command::new(&self.bin).args(&args).exec().into())
    }

    /// All panes in a session, across its windows.
    pub fn list_panes(&self, session: &str) -> Result<Vec<Pane>> {
        let out = self.run(&["list-panes", "-s", "-t", session, "-F", PANE_FORMAT])?;
        Ok(label_commands(parse_panes(&out)))
    }

    /// All panes on the server, across sessions (for the daemon poll).
    pub fn list_all_panes(&self) -> Result<Vec<Pane>> {
        let out = self.run(&["list-panes", "-a", "-F", PANE_FORMAT])?;
        Ok(label_commands(parse_panes(&out)))
    }

    /// Last `lines` of a pane's visible content (no escape sequences).
    pub fn capture_pane(&self, pane: &str, lines: u32) -> Result<String> {
        let start = format!("-{lines}");
        self.run(&["capture-pane", "-p", "-t", pane, "-S", &start])
    }

    /// Capture with escape sequences preserved, for color preview display.
    /// Never feed this to the screen classifier: SGR garbage would pollute
    /// state matching and summaries. Preview rendering only.
    pub fn capture_pane_styled(&self, pane: &str, lines: u32) -> Result<String> {
        let start = format!("-{lines}");
        self.run(&["capture-pane", "-p", "-e", "-t", pane, "-S", &start])
    }

    pub fn kill_pane(&self, pane: &str) -> Result<()> {
        self.run(&["kill-pane", "-t", pane]).map(|_| ())
    }

    pub fn swap_panes(&self, a: &str, b: &str) -> Result<()> {
        self.run(&["swap-pane", "-s", a, "-t", b]).map(|_| ())
    }

    /// Foreground pane of the active client, if any client is attached.
    pub fn foreground_pane(&self) -> Option<String> {
        self.display("", "#{pane_id}")
            .map(|s| s.trim().to_string())
            .ok()
            .filter(|s| !s.is_empty())
    }

    /// Raw `display-message` query against any target (empty = current).
    pub fn display(&self, target: &str, format: &str) -> Result<String> {
        let mut args = vec!["display-message", "-p"];
        if !target.is_empty() {
            args.extend(["-t", target]);
        }
        args.push(format);
        self.run(&args)
    }

    pub fn session_of_pane(&self, pane: &str) -> Result<String> {
        Ok(self.display(pane, "#{session_name}")?.trim().to_string())
    }

    /// Jump the client to a pane. Inside tmux this switches the client in
    /// place; outside tmux it selects the pane server-side and execs attach.
    /// Only returns (Ok) when no exec was needed.
    pub fn focus_pane(&self, pane: &str) -> Result<()> {
        if env::var("TMUX").is_ok() {
            self.run(&["switch-client", "-t", pane]).map(|_| ())
        } else {
            Self::need_tty()?;
            self.run(&["select-window", "-t", pane])?;
            self.run(&["select-pane", "-t", pane])?;
            let session = self.session_of_pane(pane)?;
            self.attach_here(&session)
        }
    }

    /// Move our own client to another session without leaving the process
    /// (only meaningful inside tmux; the TUI stays open behind the jump).
    pub fn switch_session(&self, name: &str) -> Result<()> {
        self.run(&["switch-client", "-t", name]).map(|_| ())
    }

    /// Detach the current client (only meaningful inside tmux).
    pub fn detach_client(&self) -> Result<()> {
        self.run(&["detach-client"]).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attach_verb_depends_on_tmux_env() {
        // Save/restore to avoid leaking env changes into other tests.
        let saved = env::var("TMUX").ok();
        // SAFETY: test-only env mutation; no other threads touch TMUX here.
        unsafe {
            env::remove_var("TMUX");
        }
        assert_eq!(Tmux::attach_args("x")[0], "attach-session");
        // SAFETY: same as above.
        unsafe {
            env::set_var("TMUX", "/tmp/fake,1,0");
        }
        assert_eq!(Tmux::attach_args("x")[0], "switch-client");
        // SAFETY: same as above.
        unsafe {
            match saved {
                Some(v) => env::set_var("TMUX", v),
                None => env::remove_var("TMUX"),
            }
        }
    }

    #[test]
    fn tab_in_window_name_does_not_shift_fields() {
        // Window name carries a real tab: fields must still land correctly.
        let line = "%9\x1f@3\x1fedit\tnotes\x1f1\x1f0\x1fnvim\x1f/home/u\x1f1\x1f120\x1f40\x1fdev\x1f1790940000\x1f4242";
        let panes = parse_panes(line);
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].window_name, "edit\tnotes");
        assert_eq!(panes[0].session, "dev");
        assert_eq!(panes[0].activity, 1790940000);
        assert_eq!(panes[0].pid, 4242);
    }

    #[test]
    fn overflow_fields_skip_line() {
        // More parts than expected means a separator inside a field: fail
        // closed (skip) rather than parse shifted.
        let line = "%9\x1f@3\x1fedit\x1f1\x1f0\x1fnvim\x1f/home/u\x1f1\x1f120\x1f40\x1fdev\x1f1790940000\x1f4242\x1fEXTRA";
        assert!(parse_panes(line).is_empty());
    }

    #[test]
    fn session_tab_in_name_does_not_shift() {
        let line = "my\tsess\x1f3\x1f1\x1f1749000000\x1f1790940000";
        let sessions = parse_sessions(line);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].name, "my\tsess");
        assert_eq!(sessions[0].windows, 3);
        assert!(sessions[0].attached);
    }

    #[test]
    fn parse_panes_reads_window_activity() {
        let line = "%9\x1f@3\x1fedit\x1f1\x1f0\x1fnvim\x1f/home/u\x1f1\x1f120\x1f40\x1fdev\x1f1790940000\x1f4242";
        let panes = parse_panes(line);
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].session, "dev");
        assert_eq!(panes[0].activity, 1790940000);
        assert_eq!(panes[0].pid, 4242);
        // Short lines (older tmux without the field) still parse.
        let legacy = "%9\x1f@3\x1fedit\x1f1\x1f0\x1fnvim\x1f/home/u\x1f1\x1f120\x1f40\x1fdev";
        assert_eq!(parse_panes(legacy)[0].activity, 0);
        assert_eq!(parse_panes(legacy)[0].pid, 0);
    }
}
