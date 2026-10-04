//! Fleet: observe (and optionally drive) agents on SSH-reachable hosts.
//!
//! No remote listener and no new ports: everything goes over `ssh` stdio to a
//! remote `terhub`. Observe mode only reads status; control mode additionally
//! allows sending input to remote panes.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Observe,
    Control,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "observe" | "read" | "ro" => Some(Mode::Observe),
            "control" | "write" | "rw" => Some(Mode::Control),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Mode::Observe => "observe",
            Mode::Control => "control",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Host {
    pub name: String,
    pub target: String,
    #[serde(default)]
    pub mode: Mode,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct FleetFile {
    #[serde(default)]
    host: Vec<Host>,
}

pub fn fleet_path() -> PathBuf {
    crate::config::config_path()
        .parent()
        .map(|p| p.join("fleet.toml"))
        .unwrap_or_else(|| PathBuf::from("fleet.toml"))
}

pub fn load_hosts() -> Vec<Host> {
    let text = match std::fs::read_to_string(fleet_path()) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    toml::from_str::<FleetFile>(&text)
        .map(|f| f.host)
        .unwrap_or_default()
}

pub fn save_hosts(hosts: &[Host]) -> Result<()> {
    let path = fleet_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = toml::to_string_pretty(&FleetFile {
        host: hosts.to_vec(),
    })?;
    std::fs::write(&path, text)?;
    Ok(())
}

pub fn add_host(name: &str, target: &str, mode: Mode) -> Result<()> {
    let name = name.trim();
    let target = target.trim();
    if name.is_empty() || target.is_empty() {
        bail!("name and target must not be empty");
    }
    let mut hosts = load_hosts();
    if hosts.iter().any(|h| h.name == name) {
        bail!("host '{name}' already exists (remove it first)");
    }
    hosts.push(Host {
        name: name.to_string(),
        target: target.to_string(),
        mode,
    });
    save_hosts(&hosts)
}

pub fn remove_host(name: &str) -> Result<bool> {
    let mut hosts = load_hosts();
    let before = hosts.len();
    hosts.retain(|h| h.name != name);
    if hosts.len() == before {
        return Ok(false);
    }
    save_hosts(&hosts)?;
    Ok(true)
}

/// Run a command over ssh. BatchMode so we never hang on a password prompt;
/// host keys must be pre-accepted. `timeout_secs` bounds the whole command:
/// ConnectTimeout alone only covers TCP connect, so a hung remote command is
/// killed past the deadline instead of hanging the caller forever.
pub fn ssh_run(target: &str, args: &[&str], timeout_secs: u64) -> Result<String> {
    let mut child = std::process::Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            &format!("ConnectTimeout={timeout_secs}"),
            target,
        ])
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to run ssh")?;
    let drained = match crate::automation::wait_drained(
        &mut child,
        std::time::Duration::from_secs(timeout_secs.max(1)),
    ) {
        Ok(d) => d,
        Err(crate::automation::DrainError::Timeout) => {
            bail!("ssh {target}: timed out after {timeout_secs}s")
        }
        Err(crate::automation::DrainError::WaitFailed(e)) => bail!("{e}"),
    };
    let status = drained.status;
    let stdout = drained.stdout;
    let stderr = drained.stderr;
    if !status.success() {
        let err = String::from_utf8_lossy(&stderr).trim().to_string();
        bail!(if err.is_empty() {
            format!("ssh exited {status}")
        } else {
            err
        });
    }
    Ok(String::from_utf8_lossy(&stdout).into_owned())
}

/// Single-quote a string for remote shell transport.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

pub struct HostStatus {
    pub host: Host,
    pub result: Result<crate::daemon::Snapshot, String>,
}

/// Query every host concurrently (one thread each); results come back in
/// config order so a slow host delays the answer but not the others.
pub fn fleet_status(hosts: &[Host]) -> Vec<HostStatus> {
    std::thread::scope(|s| {
        let handles: Vec<_> = hosts
            .iter()
            .map(|host| {
                s.spawn(|| {
                    let result = (|| -> Result<crate::daemon::Snapshot, String> {
                        let text = ssh_run(&host.target, &["terhub", "status", "--json"], 10)
                            .map_err(|e| format!("{e:?}"))?;
                        serde_json::from_str(&text).map_err(|e| format!("bad snapshot: {e}"))
                    })();
                    HostStatus {
                        host: host.clone(),
                        result,
                    }
                })
            })
            .collect();
        handles
            .into_iter()
            .zip(hosts.iter())
            .map(|(h, host)| {
                h.join().unwrap_or_else(|_| HostStatus {
                    host: host.clone(),
                    result: Err("status thread panicked".to_string()),
                })
            })
            .collect()
    })
}

/// Send input to a remote pane (control-mode hosts only).
pub fn fleet_send(host: &Host, pane: &str, text: &str, submit: bool) -> Result<String> {
    if host.mode != Mode::Control {
        bail!("host '{}' is observe-only", host.name);
    }
    let mut remote = format!("terhub send {} {}", sh_quote(pane), sh_quote(text));
    if !submit {
        remote.push_str(" --no-submit");
    }
    // One remote shell command so quoting survives the hop.
    ssh_run(&host.target, &[&remote], 15)
}

#[derive(Debug)]
pub struct DoctorCheck {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
}

/// Connectivity + remote prerequisites for one target.
pub fn doctor(target: &str) -> Vec<DoctorCheck> {
    let mut out = Vec::new();
    let ssh_ok = ssh_run(target, &["true"], 10).is_ok();
    out.push(DoctorCheck {
        name: "ssh",
        ok: ssh_ok,
        detail: if ssh_ok {
            "reachable".to_string()
        } else {
            "unreachable (check keys/host)".to_string()
        },
    });
    if !ssh_ok {
        for name in ["terhub", "daemon", "tmux"] {
            out.push(DoctorCheck {
                name,
                ok: false,
                detail: "skipped (no ssh)".to_string(),
            });
        }
        return out;
    }
    match ssh_run(target, &["command", "-v", "terhub"], 10) {
        Ok(path) => out.push(DoctorCheck {
            name: "terhub",
            ok: true,
            detail: path.trim().to_string(),
        }),
        Err(_) => out.push(DoctorCheck {
            name: "terhub",
            ok: false,
            detail: "not on remote PATH".to_string(),
        }),
    }
    match ssh_run(target, &["terhub", "daemon", "status"], 10) {
        Ok(_) => out.push(DoctorCheck {
            name: "daemon",
            ok: true,
            detail: "running".to_string(),
        }),
        Err(_) => out.push(DoctorCheck {
            name: "daemon",
            ok: false,
            detail: "not running".to_string(),
        }),
    }
    match ssh_run(target, &["command", "-v", "tmux"], 10) {
        Ok(path) => out.push(DoctorCheck {
            name: "tmux",
            ok: true,
            detail: path.trim().to_string(),
        }),
        Err(_) => out.push(DoctorCheck {
            name: "tmux",
            ok: false,
            detail: "not on remote PATH".to_string(),
        }),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_parse() {
        assert_eq!(Mode::parse("observe"), Some(Mode::Observe));
        assert_eq!(Mode::parse("RW"), Some(Mode::Control));
        assert_eq!(Mode::parse("bogus"), None);
        assert_eq!(Mode::default(), Mode::Observe);
    }

    #[test]
    fn shell_quoting() {
        assert_eq!(sh_quote("hi"), "'hi'");
        assert_eq!(sh_quote("don't"), "'don'\\''t'");
        assert_eq!(sh_quote("a b"), "'a b'");
    }

    #[test]
    fn control_gate() {
        let obs = Host {
            name: "o".to_string(),
            target: "t".to_string(),
            mode: Mode::Observe,
        };
        assert!(fleet_send(&obs, "%1", "x", true).is_err());
    }
}
