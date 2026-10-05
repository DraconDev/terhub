//! Zellij baseline backend: session list/attach parity via the zellij CLI.
//! Pane-level observation needs the plugin API and stays future work.
use anyhow::{Context, Result, bail};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZSession {
    pub name: String,
}

fn zellij_bin() -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join("zellij"))
        .find(|p| p.is_file())
}

pub fn available() -> bool {
    zellij_bin().is_some()
}

fn run(args: &[&str]) -> Result<String> {
    let bin = zellij_bin().context("zellij not found on PATH")?;
    let out = Command::new(&bin)
        .args(args)
        .output()
        .context("failed to run zellij")?;
    if !out.status.success() {
        let mut msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if msg.is_empty() {
            msg = String::from_utf8_lossy(&out.stdout).trim().to_string();
        }
        if msg.is_empty() {
            msg = "zellij failed".to_string();
        }
        bail!("{msg}");
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Parse `zellij list-sessions --short` (one bare name per line).
pub fn parse_sessions(out: &str) -> Vec<ZSession> {
    out.lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .map(|name| ZSession { name })
        .collect()
}

/// True when a list failure just means "no sessions" (same shape as the
/// tmux empty-server check): that reads as an empty list, exit 0 — not an
/// error. Anything else (a missing binary, real failures) still errors.
fn is_empty_server(e: &anyhow::Error) -> bool {
    e.to_string().to_lowercase().contains("no session")
}

pub fn list_sessions() -> Result<Vec<ZSession>> {
    match run(&["list-sessions", "--short"]) {
        Ok(out) => Ok(parse_sessions(&out)),
        Err(e) if is_empty_server(&e) => Ok(vec![]),
        Err(e) => Err(e),
    }
}

/// Replace this process with `zellij attach`. Only returns on failure.
/// Like tmux attach, this needs a real terminal: fail cleanly without one.
pub fn attach_here(name: &str) -> Result<()> {
    use std::io::IsTerminal;
    use std::os::unix::process::CommandExt;
    if !std::io::stdout().is_terminal() {
        bail!("cannot attach: no terminal on stdout");
    }
    let bin = zellij_bin().context("zellij not found on PATH")?;
    Err(Command::new(&bin).args(["attach", name]).exec().into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_short_list() {
        let out = "main\nwork\n";
        assert_eq!(
            parse_sessions(out),
            vec![
                ZSession {
                    name: "main".to_string()
                },
                ZSession {
                    name: "work".to_string()
                },
            ]
        );
        assert!(parse_sessions("").is_empty());
        assert!(parse_sessions("   \n").is_empty());
    }

    #[test]
    fn empty_server_reads_as_no_sessions() {
        assert!(is_empty_server(&anyhow::anyhow!("No sessions found")));
        assert!(is_empty_server(&anyhow::anyhow!("error: no session running")));
        // Real failures (and a missing binary) must still error, not
        // masquerade as an empty list.
        assert!(!is_empty_server(&anyhow::anyhow!("boom")));
        assert!(!is_empty_server(&anyhow::anyhow!("zellij not found on PATH")));
    }
}
