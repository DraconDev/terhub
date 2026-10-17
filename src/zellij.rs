//! Zellij baseline backend: session list/attach parity via the zellij CLI.
//! Pane-level observation needs the plugin API and stays future work.
use anyhow::{Context, Result, bail};
use std::process::Command;

/// Bound for every zellij CLI call (see `run`).
const ZELLIJ_CMD_TIMEOUT_SECS: u64 = 5;

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
    run_with_bin(&bin, args)
}

/// Test seam with an explicit binary (mirrors [`crate::tmux::Tmux::test`]:
/// a wedged fake proves the timeout without touching PATH).
fn run_with_bin(bin: &std::path::Path, args: &[&str]) -> Result<String> {
    // Bounded like the tmux backend: a wedged zellij server surfaces
    // as a timeout error instead of freezing the caller. The timeout
    // error matches no empty-server string, so it still errors loudly
    // instead of reading as "no sessions".
    let mut cmd = Command::new(bin);
    cmd.args(args);
    let what = format!("zellij {}", args.first().unwrap_or(&"?"));
    let out = crate::native::run_with_timeout(cmd, &what, ZELLIJ_CMD_TIMEOUT_SECS)
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
    fn wedged_server_times_out_instead_of_blocking() {
        // Same contract as the tmux backend: a wedged zellij server
        // must time out, never block the caller. The fake binary is
        // injected via PATH (save/restore: no other lib test resolves
        // the zellij binary, and PATH is restored before any assert
        // can panic).
        let dir = std::env::temp_dir().join(format!("terhub-zellij-wedge-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("zellij"), "#!/bin/sh\nsleep 30\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir.join("zellij"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        // Prepend (never replace): the fake script itself needs `sleep`
        // from the real PATH.
        let old_path = std::env::var_os("PATH");
        let mut prepended = dir.as_os_str().as_encoded_bytes().to_vec();
        prepended.push(b':');
        if let Some(ref p) = old_path {
            prepended.extend_from_slice(p.as_encoded_bytes());
        }
        use std::os::unix::ffi::OsStrExt;
        let prepended_path = OsStrExt::from_bytes(&prepended).to_os_string();
        unsafe {
            std::env::set_var("PATH", prepended_path);
        }
        let start = std::time::Instant::now();
        let err = run(&["list-sessions", "--short"]).expect_err("wedged zellij must time out");
        let elapsed = start.elapsed();
        match old_path {
            Some(p) => unsafe {
                std::env::set_var("PATH", p);
            },
            None => unsafe {
                std::env::remove_var("PATH");
            },
        }
        assert!(
            format!("{err:#}").contains("timed out"),
            "unexpected error: {err:#}"
        );
        assert!(
            elapsed >= std::time::Duration::from_secs(super::ZELLIJ_CMD_TIMEOUT_SECS),
            "returned before the deadline ({elapsed:?}): timeout path not taken"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(20),
            "wedged zellij blocked the caller ({elapsed:?})"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_server_reads_as_no_sessions() {
        assert!(is_empty_server(&anyhow::anyhow!("No sessions found")));
        assert!(is_empty_server(&anyhow::anyhow!(
            "error: no session running"
        )));
        // Real failures (and a missing binary) must still error, not
        // masquerade as an empty list.
        assert!(!is_empty_server(&anyhow::anyhow!("boom")));
        assert!(!is_empty_server(&anyhow::anyhow!(
            "zellij not found on PATH"
        )));
    }
}
