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
    /// Display name for rows and tabs (empty = show `name`). Routing
    /// always uses `name`, so labels are free-form and renamable.
    #[serde(default)]
    pub label: String,
}

impl Host {
    /// What the user sees: the label when set, else the routing name.
    pub fn display_name(&self) -> &str {
        if self.label.trim().is_empty() {
            &self.name
        } else {
            self.label.trim()
        }
    }
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

pub fn load_hosts() -> Result<Vec<Host>> {
    let path = fleet_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    // A corrupt file errors loudly: callers must refuse to overwrite it.
    // (Flattening a parse error to empty here once let add_host wipe
    // every host in the file.)
    toml::from_str::<FleetFile>(&text)
        .map(|f| f.host)
        .with_context(|| format!("parse {}", path.display()))
}

pub fn save_hosts(hosts: &[Host]) -> Result<()> {
    let path = fleet_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Preserve any non-`host` top-level keys a user added to
    // fleet.toml by re-using the existing file as a Value template
    // and only updating the `host` entry. Comments inside the host
    // array are still lost (the `toml` crate is not lossless on
    // round-trip; preserving them needs `toml_edit`); the contract
    // we keep is "don't destroy top-level data the user put here".
    let mut root: toml::Value = match std::fs::read_to_string(&path) {
        Ok(t) => toml::from_str(&t).unwrap_or_else(|_| toml::Value::Table(toml::map::Map::new())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            toml::Value::Table(toml::map::Map::new())
        }
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let arr: toml::Value = toml::Value::Array(
        hosts
            .iter()
            .cloned()
            .map(toml::Value::try_from)
            .collect::<Result<_, _>>()?,
    );
    root.as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("fleet.toml root is not a table"))?
        .insert("host".to_string(), arr);
    let text = toml::to_string_pretty(&root)?;
    crate::config::atomic_write(&path, &text)?;
    Ok(())
}

/// Host-name rules: non-empty, and no colon (remote ids are
/// `host:%N`, so a colon would corrupt the split).
pub fn validate_host_name(name: &str) -> Result<()> {
    if name.trim().is_empty() {
        bail!("host name must not be empty");
    }
    if name.contains(':') {
        bail!("host name must not contain ':'");
    }
    // Native terminal rows (`konsole:…`, `kitty:…`, `wezterm:…`) share the
    // non-% namespace; a host named the same would misroute jumps.
    if crate::native::BACKENDS.contains(&name.trim().to_lowercase().as_str()) {
        bail!("host name '{name}' is reserved for native terminal rows");
    }
    Ok(())
}

pub fn add_host(name: &str, target: &str, mode: Mode, label: &str) -> Result<()> {
    let name = name.trim();
    let target = target.trim();
    validate_host_name(name)?;
    if target.is_empty() {
        bail!("target must not be empty");
    }
    let mut hosts = load_hosts()?;
    if hosts.iter().any(|h| h.name == name) {
        bail!("host '{name}' already exists (remove it first)");
    }
    hosts.push(Host {
        name: name.to_string(),
        target: target.to_string(),
        mode,
        label: label.trim().to_string(),
    });
    save_hosts(&hosts)
}

/// Rename a host's display label (empty clears back to the name).
/// Returns false when no host carries `name`.
pub fn set_label(name: &str, label: &str) -> Result<bool> {
    let mut hosts = load_hosts()?;
    let Some(h) = hosts.iter_mut().find(|h| h.name == name) else {
        return Ok(false);
    };
    h.label = label.trim().to_string();
    save_hosts(&hosts)?;
    Ok(true)
}

pub fn remove_host(name: &str) -> Result<bool> {
    let mut hosts = load_hosts()?;
    let before = hosts.len();
    hosts.retain(|h| h.name != name);
    if hosts.len() == before {
        return Ok(false);
    }
    save_hosts(&hosts)?;
    Ok(true)
}

/// Run a command over ssh. BatchMode so we never hang on a password prompt;
/// host keys must be pre-accepted. `--` ends ssh option parsing so a
/// dash-leading target can never be mistaken for an ssh flag.
/// `timeout_secs` bounds the whole command:
/// ConnectTimeout alone only covers TCP connect, so a hung remote command is
/// killed past the deadline instead of hanging the caller forever.
pub fn ssh_run(target: &str, args: &[&str], timeout_secs: u64) -> Result<String> {
    let mut child = std::process::Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            &format!("ConnectTimeout={timeout_secs}"),
            "--",
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

/// Max concurrent ssh children during a fleet-wide status query. A
/// large fleet (hundreds of hosts) would otherwise spawn one thread
/// and one `ssh` per host at once, exhausting fds and pinning a
/// core. 16 keeps latency low while bounding the burst.
const FLEET_FANOUT: usize = 16;

/// Query every host concurrently, capped at [`FLEET_FANOUT`] in flight.
/// Results come back in config order so a slow host delays the answer
/// but not the others.
pub fn fleet_status(hosts: &[Host]) -> Vec<HostStatus> {
    std::thread::scope(|s| {
        let sem = std::sync::Arc::new(std::sync::Semaphore::new(FLEET_FANOUT));
        let handles: Vec<_> = hosts
            .iter()
            .map(|host| {
                let sem = std::sync::Arc::clone(&sem);
                s.spawn(move || {
                    // Acquire a permit before opening an ssh child so
                    // the cap is real, not just a thread cap.
                    let _permit = sem.acquire().expect("semaphore closed");
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

/// Turn a remote snapshot into table rows: ids are `host:%N`, sessions
/// `label ▸ session` (never matching a local tab, so remotes show under
/// All only). Agents are re-keyed by remote id for badges and dots.
pub fn snapshot_rows(
    host: &Host,
    snap: &crate::daemon::Snapshot,
) -> (Vec<crate::tmux::Pane>, Vec<crate::agent::AgentRecord>) {
    let mut rows = Vec::new();
    let mut agents = Vec::new();
    for a in &snap.agents {
        let id = format!("{}:{}", host.name, a.pane);
        rows.push(crate::tmux::Pane {
            id: id.clone(),
            session: format!("{} \u{25b8} {}", host.display_name(), a.session),
            window_id: String::new(),
            window_name: a.window.clone(),
            window_active: false,
            index: 0,
            command: a.command.clone(),
            path: String::new(),
            active: false,
            width: 0,
            height: 0,
            activity: a.updated_at,
            pid: 0,
        });
        let mut rec = a.clone();
        rec.pane = id;
        // Handles are local-only: a remote `@pi-web` must never shadow or
        // resolve as the local one.
        rec.handle.clear();
        agents.push(rec);
    }
    (rows, agents)
}

/// Styled scrollback from a remote pane (the preview path for remote rows).
///
/// One remote shell command string, not an argv vector: ssh joins its
/// post-`--` arguments with spaces and runs them via the remote
/// shell, so the pane id (which can come from an untrusted remote
/// `status --json` snapshot, or from the user) must be single-quoted
/// to keep `;` / `` ` `` / `$()` from executing remotely.
pub fn remote_capture(target: &str, pane: &str, lines: u32) -> Result<String> {
    let cmd = format!(
        "tmux capture-pane -p -e -t {} -S -{lines}",
        sh_quote(pane)
    );
    ssh_run(target, &[&cmd], 10)
}

/// argv for an interactive remote attach: `-t` for the tty, `--` so a
/// dash-leading target can't become an ssh flag. No BatchMode — auth
/// may legitimately prompt. No session means the remote most-recent.
///
/// The remote command is built as a single shell-quoted string so
/// the session name (which can come from `fleet list` over a
/// compromised remote, or the CLI) cannot inject into the remote
/// shell.
pub fn attach_remote_argv(target: &str, session: Option<&str>) -> Vec<String> {
    let remote = match session {
        Some(s) => format!("tmux attach-session -t {}", sh_quote(s)),
        None => "tmux attach-session".to_string(),
    };
    vec!["-t".to_string(), "--".to_string(), target.to_string(), remote]
}

/// Replace this process with `ssh -t` into the remote session (the TUI
/// exits: ssh owns the terminal from here). Only returns on failure.
pub fn attach_remote(target: &str, session: Option<&str>) -> Result<()> {
    use std::os::unix::process::CommandExt;
    crate::tmux::Tmux::need_tty()?;
    let args = attach_remote_argv(target, session);
    Err(std::process::Command::new("ssh").args(&args).exec().into())
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
    fn host_names_reject_backend_reservations() {
        assert!(validate_host_name("dev").is_ok());
        for reserved in ["konsole", "kitty", "wezterm", "Konsole", " KITTY "] {
            assert!(
                validate_host_name(reserved).is_err(),
                "{reserved} must be reserved"
            );
        }
        assert!(validate_host_name("has:colon").is_err());
        assert!(validate_host_name("  ").is_err());
    }

    #[test]
    fn shell_quoting() {
        assert_eq!(sh_quote("hi"), "'hi'");
        assert_eq!(sh_quote("don't"), "'don'\\''t'");
        assert_eq!(sh_quote("a b"), "'a b'");
    }

    #[test]
    fn snapshot_rows_prefix_remote_identity() {
        use crate::agent::{AgentRecord, AgentState, Source};
        let host = Host {
            name: "dev".to_string(),
            target: "dev.example".to_string(),
            mode: Mode::Observe,
            label: String::new(),
        };
        let snap = crate::daemon::Snapshot {
            agents: vec![AgentRecord {
                pane: "%1".to_string(),
                agent: "pi".to_string(),
                state: AgentState::Waiting,
                source: Source::Hook,
                summary: "Approve?".to_string(),
                session: "web".to_string(),
                window: "srv".to_string(),
                command: "pi".to_string(),
                updated_at: 1700,
                state_since: 1600,
                cpu_ticks: 0,
                cpu_still_since: 0,
                agent_start: 0,
                last_turn_secs: 0,
                screen_hash: 0,
                quiet_since: 0,
                last_prompt_at: 0,
                last_prompt: String::new(),
                handle: "@pi-web".to_string(),
                model: String::new(),
                ctx_pct: None,
                reg_pid: 0,
            }],
            updated_at: 1700,
        };
        let (rows, agents) = snapshot_rows(&host, &snap);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "dev:%1");
        assert_eq!(rows[0].session, "dev \u{25b8} web");
        assert_eq!(rows[0].command, "pi");
        assert_eq!(rows[0].activity, 1700);
        assert_eq!(rows[0].pid, 0);
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].pane, "dev:%1");
        assert_eq!(agents[0].summary, "Approve?");
        assert!(agents[0].handle.is_empty(), "remote handle leaked");
    }

    #[test]
    fn display_name_prefers_label() {
        let mut host = Host {
            name: "dev".to_string(),
            target: "t".to_string(),
            mode: Mode::Observe,
            label: String::new(),
        };
        assert_eq!(host.display_name(), "dev");
        host.label = "Dev Box".to_string();
        assert_eq!(host.display_name(), "Dev Box");
        // Whitespace-only labels fall back to the name.
        host.label = "   ".to_string();
        assert_eq!(host.display_name(), "dev");
    }

    #[test]
    fn snapshot_rows_use_label_for_session() {
        use crate::agent::{AgentRecord, AgentState, Source};
        let host = Host {
            name: "dev".to_string(),
            target: "dev.example".to_string(),
            mode: Mode::Observe,
            label: "Dev Box".to_string(),
        };
        let snap = crate::daemon::Snapshot {
            agents: vec![AgentRecord {
                pane: "%1".to_string(),
                agent: "pi".to_string(),
                state: AgentState::Waiting,
                source: Source::Hook,
                summary: String::new(),
                session: "web".to_string(),
                window: "srv".to_string(),
                command: "pi".to_string(),
                updated_at: 1700,
                state_since: 1600,
                cpu_ticks: 0,
                cpu_still_since: 0,
                agent_start: 0,
                last_turn_secs: 0,
                screen_hash: 0,
                quiet_since: 0,
                last_prompt_at: 0,
                last_prompt: String::new(),
                handle: String::new(),
                model: String::new(),
                ctx_pct: None,
                reg_pid: 0,
            }],
            updated_at: 1700,
        };
        let (rows, agents) = snapshot_rows(&host, &snap);
        assert_eq!(rows[0].session, "Dev Box \u{25b8} web");
        // Routing ids stay on the name: labels are display-only.
        assert_eq!(rows[0].id, "dev:%1");
        assert_eq!(agents[0].pane, "dev:%1");
    }

    #[test]
    fn host_names_reject_colons() {
        // A colon in the name would corrupt `host:%N` remote ids.
        assert!(validate_host_name("bad:name").is_err());
        assert!(validate_host_name("  ").is_err());
        assert!(validate_host_name("good-name").is_ok());
    }

    #[test]
    fn attach_remote_argv_orders_ssh_flags() {
        // The remote command is a single shell-quoted string so the
        // session name cannot inject into the remote shell; ssh gets
        // one argv element for it.
        let argv = attach_remote_argv("dev.example", Some("web"));
        assert_eq!(
            argv,
            vec![
                "-t",
                "--",
                "dev.example",
                "tmux attach-session -t 'web'",
            ]
        );
        assert!(!argv.iter().any(|a| a.contains("BatchMode")));
        let argv = attach_remote_argv("dev.example", None);
        assert_eq!(
            argv,
            vec!["-t", "--", "dev.example", "tmux attach-session"]
        );
    }

    #[test]
    fn control_gate() {
        let obs = Host {
            name: "o".to_string(),
            target: "t".to_string(),
            mode: Mode::Observe,
            label: String::new(),
        };
        assert!(fleet_send(&obs, "%1", "x", true).is_err());
    }
}
