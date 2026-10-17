//! GNU screen baseline backend: session list/attach parity via the screen CLI.
//! Pane-level observation needs screen's per-window APIs and stays future
//! work; for now this gives the TUI a way to see and rejoin screen
//! sessions on hosts where tmux isn't installed.
use anyhow::{Context, Result, bail};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SSession {
    pub name: String,
    /// True if this session is Attached (someone's already inside it).
    pub attached: bool,
}

fn screen_bin() -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join("screen"))
        .find(|p| p.is_file())
}

pub fn available() -> bool {
    screen_bin().is_some()
}

fn run(args: &[&str]) -> Result<String> {
    let bin = screen_bin().context("screen not found on PATH")?;
    let out = Command::new(&bin)
        .args(args)
        .output()
        .context("failed to run screen")?;
    // `screen -ls` exits non-zero when there are no sessions — that's the
    // normal empty-server case, not an error. Treat it as an empty list
    // by inspecting stdout; the other code paths check status.
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Parse `screen -ls` output. Example:
///   There are screens on:
///           12345.pts-0.host     (Detached)
///           12346.pts-1.host     (Attached)
///   2 Sockets in /run/screen/S-root.
/// Also tolerates the "No Sockets found" form.
pub fn parse_sessions(out: &str) -> Vec<SSession> {
    let mut out_v = Vec::new();
    for line in out.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with("There are") || l.starts_with("No Sockets") {
            continue;
        }
        // The session name is everything before the first whitespace, with
        // the leading PID dot stripped. A real session line is
        // `<pid>.<name>` with a numeric pid — dotted noise (`error.log
        // rotated`) has no numeric pid and must not become a phantom
        // session.
        let Some(head) = l.split_whitespace().next() else {
            continue;
        };
        let Some((pid, name)) = head.split_once('.') else {
            continue;
        };
        if pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) || name.is_empty() {
            continue;
        }
        let attached = l.contains("(Attached)");
        out_v.push(SSession {
            name: name.to_string(),
            attached,
        });
    }
    out_v
}

pub fn list_sessions() -> Result<Vec<SSession>> {
    // `screen -ls` is non-fatal on empty servers (status 1 + "No Sockets"),
    // so we just call it and parse whatever stdout we get. The bin must
    // exist; if it doesn't, surface that as an error.
    let _ = screen_bin().context("screen not found on PATH")?;
    let raw = run(&["-ls"])?;
    Ok(parse_sessions(&raw))
}

/// Replace this process with `screen -xD -r <name>` (multi-attach if it's
/// already in use; create if it doesn't exist). Only returns on failure.
/// Needs a real terminal.
pub fn attach_here(name: &str) -> Result<()> {
    use std::io::IsTerminal;
    use std::os::unix::process::CommandExt;
    if !std::io::stdout().is_terminal() {
        bail!("cannot attach: no terminal on stdout");
    }
    let bin = screen_bin().context("screen not found on PATH")?;
    // -D -r: reattach even if detached; -x: multi-attach if it's already
    // attached. Covers the common "join your own session from a second
    // terminal" case without surprise.
    Err(Command::new(&bin).args(["-xD", "-r", name]).exec().into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_short_list() {
        let out = "\
There are screens on:
\t12345.pts-0.host\t(Detached)
\t12346.pts-1.host\t(Attached)
2 Sockets in /run/screen/S-root.
";
        let s = parse_sessions(out);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].name, "pts-0.host");
        assert!(!s[0].attached);
        assert_eq!(s[1].name, "pts-1.host");
        assert!(s[1].attached);
    }

    #[test]
    fn parses_empty_server() {
        let out = "No Sockets found in /run/screen/S-root.\n";
        assert!(parse_sessions(out).is_empty());
    }

    #[test]
    fn rejects_dotted_noise_without_a_pid() {
        // A real session line is `<pid>.<name>` with a numeric pid;
        // dotted noise (`error.log rotated`) must not become a
        // phantom session named `log`.
        let out = "error.log rotated\n12345.pts-2.host\t(Detached)\nabc.def\t(Detached)\n";
        let s = parse_sessions(out);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].name, "pts-2.host");
    }

    #[test]
    fn ignores_garbage_lines() {
        let out = "weird stuff\n12345.pts-2.host\t(Detached)\nrandom\n";
        let s = parse_sessions(out);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].name, "pts-2.host");
    }
}
