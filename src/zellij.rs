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

pub fn list_sessions() -> Result<Vec<ZSession>> {
    Ok(parse_sessions(&run(&["list-sessions", "--short"])?))
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
}
