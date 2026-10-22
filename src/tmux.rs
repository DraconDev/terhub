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
    /// True when tmux's `window_bell_flag` is on for this window —
    /// the pane's foreground program emitted a BEL since the last
    /// reset. CLIs ring on input-plea prompts (`y/N`, `Press enter`),
    /// so a flipped flag is high-precision evidence the pane needs
    /// the human. We read it once per tick and let the classifier
    /// downgrade back to false when the next capture clears the flag.
    pub bell: bool,
    /// Root process id of the pane (0 when unknown).
    pub pid: u32,
    /// starttime (stat field 22) of `pid` when it was listed (0 =
    /// unknown). Passed to pause/resume_group so a recycled pid is
    /// refused instead of stopping an unrelated process group.
    pub pid_start: u64,
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
            bell: false,
            pid: 0,
            pid_start: 0,
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
const PANE_FIELDS: usize = 14;
const SESSION_FIELDS: usize = 5;

const PANE_FORMAT: &str = "#{pane_id}\x1f#{window_id}\x1f#{window_name}\x1f#{window_active}\x1f#{pane_index}\x1f#{pane_current_command}\x1f#{pane_current_path}\x1f#{pane_active}\x1f#{pane_width}\x1f#{pane_height}\x1f#{session_name}\x1f#{window_activity}\x1f#{pane_pid}\x1f#{window_bell_flag}";

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
            // `window_bell_flag` is a 0/1 numeric, not a true/false
            // string — tmux's flag formatter returns 1/0 here, while
            // the other flags (active, window_active) are yes/no.
            // Match both shapes.
            if let Some(s) = parts.get(i) {
                let t = s.trim();
                if t == "1" {
                    return true;
                }
                if t == "0" {
                    return false;
                }
                !matches!(t, "0" | "" | "no")
            } else {
                false
            }
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
            bell: flag(13),
            pid_start: 0,
        });
    }
    // Identity pass: stamp each listed root pid with its current
    // starttime, so pause/resume can refuse a pid recycled between
    // this listing and the user's keypress. A second /proc walk is
    // cheaper than signalling the wrong process group.
    for pane in &mut panes {
        if pane.pid > 1 {
            pane.pid_start = crate::proc::proc_starttime(pane.pid).unwrap_or(0);
        }
    }
    panes
}

fn parse_sessions(out: &str) -> Vec<Session> {
    let mut sessions = Vec::new();
    for line in out.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split(SEP).collect();
        if parts.len() > SESSION_FIELDS {
            continue;
        }
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
fn label_commands(panes: Vec<Pane>) -> Vec<Pane> {
    if !panes
        .iter()
        .any(|p| p.pid != 0 && crate::proc::is_launcher_wrapper(&p.command))
    {
        return panes;
    }
    label_commands_with(panes, &crate::proc::Snapshot::capture())
}

/// Same as [`label_commands`] but reuses a snapshot the caller already
/// captured, so the daemon poll doesn't walk /proc twice per cycle.
fn label_commands_with(mut panes: Vec<Pane>, snap: &crate::proc::Snapshot) -> Vec<Pane> {
    for p in &mut panes {
        p.command = snap.display_command(p.pid, &p.command);
    }
    panes
}

/// An executable regular file: a non-executable `tmux` shadow earlier
/// on PATH must be skipped, not selected and then failed at spawn
/// with a confusing permission error.
fn is_executable(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.is_file()
        && p.metadata()
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

fn find_tmux_in(dirs: impl Iterator<Item = PathBuf>) -> Option<PathBuf> {
    dirs.map(|dir| dir.join("tmux")).find(|p| is_executable(p))
}

fn find_tmux() -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    find_tmux_in(env::split_paths(&path))
}

/// Environment variable selecting a private tmux socket for this client.
/// Unset (the default) means the operator's live server, exactly as
/// before. Set it for tests and agent fixtures so nothing we run can
/// touch the human's real sessions: a bare name is `tmux -L <name>`
/// (socket under `$TMUX_TMPDIR`), a value containing `/` is
/// `tmux -S <path>`. Killing such a private server is safe; killing
/// the default one destroys live sessions (see incident
/// 2026-10-09).
pub const SOCKET_ENV: &str = "TERHUB_TMUX_SOCKET";

/// tmux flags targeting the private socket `socket` selects.
fn socket_args(socket: &str) -> Vec<String> {
    if socket.contains('/') {
        vec!["-S".to_string(), socket.to_string()]
    } else {
        vec!["-L".to_string(), socket.to_string()]
    }
}

/// Bound for every tmux CLI call (see `run`).
const TMUX_CMD_TIMEOUT_SECS: u64 = 5;

pub struct Tmux {
    bin: PathBuf,
    socket: Option<String>,
}

impl Tmux {
    pub fn new() -> Result<Self> {
        find_tmux()
            .map(|bin| {
                let socket = env::var(SOCKET_ENV)
                    .ok()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                Tmux::with_socket(bin, socket)
            })
            .context("tmux not found on PATH")
    }

    pub fn available() -> bool {
        find_tmux().is_some()
    }

    /// Construct an explicit client. `socket` follows [`SOCKET_ENV`]:
    /// `None` or empty means the ambient default server.
    pub fn with_socket(bin: PathBuf, socket: Option<String>) -> Self {
        let socket = socket
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        Tmux { bin, socket }
    }

    /// Test handle with an explicit binary (a bogus path fails every call,
    /// which is how outage paths get exercised deterministically).
    pub fn test(bin: PathBuf) -> Self {
        Tmux::with_socket(bin, None)
    }

    fn run(&self, args: &[&str]) -> Result<String> {
        // Every tmux *observation* call is bounded: a wedged server (or
        // a hung socket) surfaces as a timeout error instead of
        // freezing the TUI or the daemon poll, which run these calls
        // on their hot paths. Same budget as the native probes; the
        // timeout error matches no empty-server string, so it keeps
        // the warning treatment instead of reading as "no sessions".
        //
        // The one exception is `display_popup`, which is deliberately
        // blocking: it runs an interactive picker inside the popup and
        // returns when the popup closes. It is a one-shot CLI path
        // (`terhub peek`), never the TUI or the daemon poll, and
        // bounding it would kill the user's picker mid-read.
        let mut cmd = Command::new(&self.bin);
        if let Some(s) = &self.socket {
            cmd.args(socket_args(s));
        }
        cmd.args(args);
        let what = format!("tmux {}", args.first().unwrap_or(&"?"));
        let out = crate::native::run_with_timeout(cmd, &what, TMUX_CMD_TIMEOUT_SECS)
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
                // No server yet (fresh private socket: "error connecting to
                // <socket> (No such file or directory)") means zero sessions,
                // not a failure — same as tmux's own "no server running".
                if m.contains("no server running")
                    || m.contains("no sessions")
                    || m.contains("error connecting to")
                {
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

    /// Open a window in `session` running `command`, printing the new pane
    /// id (`-P -F`) so callers can address it without a racy re-list.
    /// Empty session/name fails loudly; no command means a plain shell.
    pub fn new_window(
        &self,
        session: &str,
        name: &str,
        dir: Option<&str>,
        command: Option<&str>,
    ) -> Result<String> {
        let (session, name) = (session.trim(), name.trim());
        if session.is_empty() {
            bail!("session must not be empty");
        }
        if name.is_empty() {
            bail!("window name must not be empty");
        }
        let mut args = vec!["new-window", "-t", session, "-n", name];
        if let Some(d) = dir {
            args.extend(["-c", d]);
        }
        args.extend(["-P", "-F", "#{pane_id}"]);
        let mut owned;
        let slice: &[&str] = match command {
            Some(c) if !c.trim().is_empty() => {
                owned = args;
                owned.push(c);
                &owned
            }
            _ => &args,
        };
        Ok(self.run(slice)?.trim().to_string())
    }

    /// Split the active pane of `session` horizontally, returning the new
    /// pane's id. Runs on this client's socket like every other call, so
    /// tests and fixtures never reach the operator's live server.
    pub fn split_window(&self, session: &str) -> Result<String> {
        let session = session.trim();
        if session.is_empty() {
            bail!("session must not be empty");
        }
        Ok(self
            .run(&["split-window", "-t", session, "-P", "-F", "#{pane_id}"])?
            .trim()
            .to_string())
    }

    /// Kill whatever runs in `pane` and re-run `command` there
    /// (`respawn-pane -k`): the dead-pane recovery behind restart.
    pub fn respawn_pane(&self, pane: &str, command: &str) -> Result<()> {
        if command.trim().is_empty() {
            bail!("respawn command must not be empty");
        }
        self.run(&["respawn-pane", "-k", "-t", pane, command])
            .map(|_| ())
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

    /// Open a popup running `command` (a shell string, quoted by the
    /// caller) on *this client's* server. Goes through the same
    /// socket selection as every other call: spawning a bare `tmux`
    /// here would open the popup on the ambient default server even
    /// when the rest of the command read a private one.
    pub fn display_popup(&self, command: &str) -> Result<()> {
        let mut cmd = Command::new(&self.bin);
        if let Some(s) = &self.socket {
            cmd.args(socket_args(s));
        }
        let status = cmd
            .args(["display-popup", "-E", "-w", "80%", "-h", "80%", command])
            .status()
            .context("failed to run tmux")?;
        if status.success() {
            Ok(())
        } else {
            bail!("tmux display-popup failed")
        }
    }

    /// Attach arguments (without argv[0]), honoring inside-vs-outside tmux
    /// and this client's private socket (when one is selected).
    pub fn attach_args(&self, name: &str) -> Vec<String> {
        let verb = if env::var("TMUX").is_ok() {
            "switch-client"
        } else {
            "attach-session"
        };
        let mut args = Vec::new();
        if let Some(s) = &self.socket {
            args.extend(socket_args(s));
        }
        args.push(verb.to_string());
        args.push("-t".to_string());
        args.push(name.to_string());
        args
    }

    /// Attach replaces this process with a tmux client, which needs a real
    /// terminal. Callers that mutate server state first must check this
    /// upfront: a failed exec'd attach can't clean up (the image is gone).
    pub(crate) fn need_tty() -> Result<()> {
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
        let args = self.attach_args(name);
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

    /// Like [`list_all_panes`] but reuses an existing /proc snapshot for
    /// label resolution, so the daemon poll walks /proc once per cycle
    /// instead of twice. The snapshot must be the same one the poll
    /// will use to detect agents.
    pub fn list_all_panes_with_snap(&self, snap: &crate::proc::Snapshot) -> Result<Vec<Pane>> {
        let out = self.run(&["list-panes", "-a", "-F", PANE_FORMAT])?;
        Ok(label_commands_with(parse_panes(&out), snap))
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
    ///
    /// The exec is right for a TUI that is about to become the session
    /// and wrong for a long-lived background process (the tray): it
    /// replaces the process image, so the tray would be gone and the
    /// user's terminal taken over. Background callers use
    /// [`switch_client_to`](Self::switch_client_to) and
    /// [`select_pane_server_side`](Self::select_pane_server_side)
    /// instead.
    pub fn focus_pane(&self, pane: &str) -> Result<()> {
        if env::var("TMUX").is_ok() {
            self.switch_client_to(pane)
        } else {
            Self::need_tty()?;
            self.select_pane_server_side(pane)?;
            let session = self.session_of_pane(pane)?;
            self.attach_here(&session)
        }
    }

    /// Switch the tmux client to a pane.
    ///
    /// Only works from inside a tmux client — the caller *is* that
    /// client. Outside tmux the server has no current client to switch
    /// and answers "no current client"; that is not an error to paper
    /// over, it means the caller has to raise the window itself.
    pub fn switch_client_to(&self, pane: &str) -> Result<()> {
        self.run(&["switch-client", "-t", pane]).map(|_| ())
    }

    /// Select a pane server-side: the session's active window and pane
    /// move to it, and nothing is attached to the caller.
    ///
    /// This is the focus a background process can do — the session is
    /// ready to show that pane, so whoever looks at the session sees
    /// it. The caller is still responsible for bringing the window to
    /// the front.
    pub fn select_pane_server_side(&self, pane: &str) -> Result<()> {
        self.run(&["select-window", "-t", pane])?;
        self.run(&["select-pane", "-t", pane])?;
        Ok(())
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
    fn find_tmux_skips_non_executable_files() {
        // No env involved: explicit dirs keep parallel tests safe.
        let base = std::env::temp_dir().join(format!("terhub-tmux-exec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let shadow = base.join("shadow");
        let real = base.join("real");
        std::fs::create_dir_all(&shadow).unwrap();
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(shadow.join("tmux"), "not tmux\n").unwrap();
        std::fs::write(real.join("tmux"), "#!/bin/sh\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(shadow.join("tmux"), std::fs::Permissions::from_mode(0o644))
            .unwrap();
        std::fs::set_permissions(real.join("tmux"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        // Skips the shadow, finds the real one past it.
        assert_eq!(
            find_tmux_in([shadow.clone(), real.clone()].into_iter()),
            Some(real.join("tmux"))
        );
        // Nothing executable anywhere reads as absent.
        assert_eq!(find_tmux_in([shadow.clone()].into_iter()), None);
        assert_eq!(find_tmux_in([base.join("missing")].into_iter()), None);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn attach_verb_depends_on_tmux_env() {
        // No-socket handle: index 0 stays the verb itself.
        let t = Tmux::test("/nonexistent-tmux".into());
        // Save/restore to avoid leaking env changes into other tests.
        let saved = env::var("TMUX").ok();
        // SAFETY: test-only env mutation; no other threads touch TMUX here.
        unsafe {
            env::remove_var("TMUX");
        }
        assert_eq!(t.attach_args("x")[0], "attach-session");
        // SAFETY: same as above.
        unsafe {
            env::set_var("TMUX", "/tmp/fake,1,0");
        }
        assert_eq!(t.attach_args("x")[0], "switch-client");
        // SAFETY: same as above.
        unsafe {
            match saved {
                Some(v) => env::set_var("TMUX", v),
                None => env::remove_var("TMUX"),
            }
        }
    }

    #[test]
    fn display_popup_prepends_socket_flag() {
        // peek's popup must open on the same (possibly private)
        // server the rest of the command read — a bare `tmux` spawn
        // would land on the operator's ambient server.
        let dir = std::env::temp_dir().join(format!("terhub-tmux-popup-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("tmux");
        let log = dir.join("argv.txt");
        std::fs::write(
            &bin,
            format!("#!/bin/sh\necho \"$@\" > {}\n", log.display()),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let t = Tmux::with_socket(bin, Some("sock9".to_string()));
        assert!(t.display_popup("'/tmp/ter hub/terhub' peek --pick").is_ok());
        let argv = std::fs::read_to_string(&log).unwrap();
        assert_eq!(
            argv.trim(),
            "-L sock9 display-popup -E -w 80% -h 80% '/tmp/ter hub/terhub' peek --pick",
            "{argv}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn socket_args_uses_dash_l_for_names() {
        // A bare socket name targets `tmux -L <name>` (socket under
        // $TMUX_TMPDIR). This is the switch that keeps agent tests on a
        // private server instead of the operator's live one.
        assert_eq!(socket_args("terhub-test-42"), vec!["-L", "terhub-test-42"]);
    }

    #[test]
    fn socket_args_uses_dash_s_for_paths() {
        // A value containing '/' is an explicit socket path.
        assert_eq!(
            socket_args("/tmp/terhub-test.sock"),
            vec!["-S", "/tmp/terhub-test.sock"]
        );
    }

    #[test]
    fn empty_socket_means_default_server() {
        let t = Tmux::with_socket("/nonexistent-tmux".into(), Some("   ".to_string()));
        assert!(t.socket.is_none());
    }

    #[test]
    fn attach_args_prepend_socket_flag() {
        // Attach must reach the same private server as every other call,
        // so the socket flag leads the argument vector. The verb index
        // varies with $TMUX, so assert the prefix and the tail only.
        let t = Tmux::with_socket("/nonexistent-tmux".into(), Some("sock1".to_string()));
        let args = t.attach_args("dev");
        assert_eq!(&args[..2], ["-L", "sock1"]);
        assert_eq!(&args[args.len() - 2..], ["-t", "dev"]);
    }

    #[test]
    fn wedged_server_times_out_instead_of_blocking() {
        // A wedged tmux server must surface as a timeout error, never
        // freeze the caller: the TUI and the daemon poll run these
        // calls on their hot paths. Two-sided timing around the
        // deadline (load only stretches elapsed, so neither flakes).
        let dir = std::env::temp_dir().join(format!("terhub-tmux-wedge-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("tmux");
        std::fs::write(&bin, "#!/bin/sh\nsleep 30\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let t = Tmux::test(bin);
        let start = std::time::Instant::now();
        let err = t
            .run(&["list-sessions"])
            .expect_err("wedged tmux must time out");
        let elapsed = start.elapsed();
        assert!(
            format!("{err:#}").contains("timed out"),
            "unexpected error: {err:#}"
        );
        assert!(
            elapsed >= std::time::Duration::from_secs(super::TMUX_CMD_TIMEOUT_SECS),
            "returned before the deadline ({elapsed:?}): timeout path not taken"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(20),
            "wedged tmux blocked the caller ({elapsed:?})"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_prepends_socket_flag_to_every_command() {
        // End-to-end: a stub tmux echoes its argv, so we can see the
        // socket flag leading every invocation (default server gets none).
        let dir = std::env::temp_dir().join(format!("terhub-tmux-run-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("tmux");
        std::fs::write(&bin, "#!/bin/sh\necho \"$@\"\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let isolated = Tmux::with_socket(bin.clone(), Some("terhub-test-7".to_string()));
        assert_eq!(
            isolated.run(&["list-sessions"]).unwrap().trim(),
            "-L terhub-test-7 list-sessions"
        );
        let default = Tmux::test(bin);
        assert_eq!(
            default.run(&["list-sessions"]).unwrap().trim(),
            "list-sessions"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fresh_private_socket_means_zero_sessions_not_error() {
        // The private-server bootstrap case: tmux answers
        // "error connecting to <socket> (No such file or directory)" when
        // no server has started on that socket yet. That is "no sessions",
        // so list_sessions must not surface it as a failure.
        let dir = std::env::temp_dir().join(format!("terhub-tmux-noserver-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("tmux");
        std::fs::write(
            &bin,
            "#!/bin/sh\necho 'error connecting to /run/user/1000/tmux-1000/terhub-test (No such file or directory)' >&2\nexit 1\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let t = Tmux::with_socket(bin, Some("terhub-test".to_string()));
        assert!(t.list_sessions().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
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
        // closed (skip) rather than parse shifted. The current
        // PANE_FIELDS is 14 (incl. window_bell_flag), so a 15-field
        // line is the first to overflow.
        let line = "%9\x1f@3\x1fedit\x1f1\x1f0\x1fnvim\x1f/home/u\x1f1\x1f120\x1f40\x1fdev\x1f1790940000\x1f4242\x1f1\x1fEXTRA";
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
