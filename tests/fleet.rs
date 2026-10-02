//! Fleet + zellij tests against fake `ssh`/`zellij` binaries on PATH.
//!
//! Single test: owns PATH and XDG_CONFIG_HOME for this binary.
use std::os::unix::fs::PermissionsExt;

const SNAPSHOT: &str = r#"{"agents":[{"pane":"%1","agent":"codex","state":"waiting","source":"hook","summary":"hi","session":"s","window":"w","command":"codex","updated_at":1,"state_since":1}],"updated_at":1}"#;

const FAKE_SSH: &str = r#"#!/bin/sh
while [ $# -gt 0 ]; do
  case "$1" in
    -o) shift 2;;
    -*) shift;;
    *) shift; break;;
  esac
done
CMD="$*"
case "$CMD" in
  "true") exit 0;;
  "command -v terhub") echo "/usr/bin/terhub";;
  "command -v tmux") echo "/usr/bin/tmux";;
  "terhub daemon status") echo "running (/tmp/x.sock)";;
  "terhub status --json") cat "$FAKE_SNAPSHOT";;
  "terhub send "*) echo "sent";;
  *) echo "unexpected: $CMD" >&2; exit 1;;
esac
"#;

const FAKE_ZELLIJ: &str = r#"#!/bin/sh
if [ "$1" = "list-sessions" ]; then printf 'main\nwork\n'; else exit 1; fi
"#;

fn write_exe(path: &std::path::Path, text: &str) {
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn fleet_and_zellij_against_fakes() {
    use terhub::fleet as f;
    let base = std::env::temp_dir().join(format!("terhub-fleet-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let bin = base.join("bin");
    let config = base.join("config");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&config).unwrap();
    write_exe(&bin.join("ssh"), FAKE_SSH);
    write_exe(&bin.join("zellij"), FAKE_ZELLIJ);
    let snap = base.join("snapshot.json");
    std::fs::write(&snap, SNAPSHOT).unwrap();
    // SAFETY: single test in this binary.
    unsafe {
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
        std::env::set_var("PATH", path);
        std::env::set_var("XDG_CONFIG_HOME", &config);
        std::env::set_var("FAKE_SNAPSHOT", &snap);
    }

    // Host CRUD.
    assert!(f::load_hosts().is_empty());
    f::add_host("dev", "fake-target", f::Mode::Control).unwrap();
    f::add_host("obs", "fake-target", f::Mode::Observe).unwrap();
    assert!(f::add_host("dev", "x", f::Mode::Observe).is_err());
    assert_eq!(f::load_hosts().len(), 2);

    // Fleet status parses the remote snapshot, in config order even
    // though hosts are queried concurrently.
    let statuses = f::fleet_status(&f::load_hosts());
    assert_eq!(statuses.len(), 2);
    assert_eq!(statuses[0].host.name, "dev");
    assert_eq!(statuses[1].host.name, "obs");
    for s in &statuses {
        let snap = s.result.as_ref().expect("host reachable");
        assert_eq!(snap.agents.len(), 1);
        assert_eq!(snap.agents[0].agent, "codex");
    }

    // Control gate + remote send.
    let hosts = f::load_hosts();
    let dev = hosts.iter().find(|h| h.name == "dev").unwrap();
    let obs = hosts.iter().find(|h| h.name == "obs").unwrap();
    assert_eq!(f::fleet_send(dev, "%1", "hi", true).unwrap().trim(), "sent");
    assert!(f::fleet_send(obs, "%1", "hi", true).is_err());

    // Doctor passes against the fake.
    let checks = f::doctor("fake-target");
    assert!(checks.iter().all(|c| c.ok), "{checks:?}");

    assert!(f::remove_host("dev").unwrap());
    assert!(!f::remove_host("dev").unwrap());

    // Zellij baseline parses the fake list.
    assert!(terhub::zellij::available());
    let sessions = terhub::zellij::list_sessions().unwrap();
    assert_eq!(sessions.len(), 2);
    assert_eq!(sessions[0].name, "main");

    // A hung remote command times out instead of hanging the caller.
    write_exe(&bin.join("ssh"), "#!/bin/sh\nsleep 30\n");
    let start = std::time::Instant::now();
    let err = f::ssh_run("fake-target", &["terhub", "status"], 1).unwrap_err();
    assert!(format!("{err:?}").contains("timed out"), "{err:?}");
    assert!(start.elapsed() < std::time::Duration::from_secs(10));

    let _ = std::fs::remove_dir_all(&base);
}
