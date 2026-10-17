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
    assert!(f::load_hosts().unwrap().is_empty());
    f::add_host("dev", "fake-target", f::Mode::Control, "").unwrap();
    f::add_host("obs", "fake-target", f::Mode::Observe, "").unwrap();
    assert!(f::add_host("dev", "x", f::Mode::Observe, "").is_err());
    assert_eq!(f::load_hosts().unwrap().len(), 2);

    // Fleet status parses the remote snapshot, in config order even
    // though hosts are queried concurrently.
    let statuses = f::fleet_status(&f::load_hosts().unwrap());
    assert_eq!(statuses.len(), 2);
    assert_eq!(statuses[0].host.name, "dev");
    assert_eq!(statuses[1].host.name, "obs");
    for s in &statuses {
        let snap = s.result.as_ref().expect("host reachable");
        assert_eq!(snap.agents.len(), 1);
        assert_eq!(snap.agents[0].agent, "codex");
    }

    // Control gate + remote send.
    let hosts = f::load_hosts().unwrap();
    let dev = hosts.iter().find(|h| h.name == "dev").unwrap();
    let obs = hosts.iter().find(|h| h.name == "obs").unwrap();
    assert_eq!(f::fleet_send(dev, "%1", "hi", true).unwrap().trim(), "sent");
    assert!(f::fleet_send(obs, "%1", "hi", true).is_err());

    // Doctor passes against the fake.
    let checks = f::doctor("fake-target");
    assert!(checks.iter().all(|c| c.ok), "{checks:?}");

    assert!(f::remove_host("dev").unwrap());
    assert!(!f::remove_host("dev").unwrap());

    // Corrupt file: reads fail loudly and add/remove refuse to touch it.
    let fleet_file = f::fleet_path();
    std::fs::write(&fleet_file, "host = [unclosed\n").unwrap();
    assert!(f::load_hosts().is_err());
    assert!(f::add_host("new", "t", f::Mode::Observe, "").is_err());
    assert!(f::remove_host("obs").is_err());
    assert_eq!(
        std::fs::read_to_string(&fleet_file).unwrap(),
        "host = [unclosed\n",
        "corrupt file must survive, byte-identical"
    );

    // Duplicate host names in a hand-edited file: name is the
    // routing identity, so the load must fail loudly (mirroring
    // add_host) instead of shadowing the second target.
    std::fs::write(
        &fleet_file,
        "[[host]]\nname = \"dev\"\ntarget = \"a\"\n\n[[host]]\nname = \"dev\"\ntarget = \"b\"\n",
    )
    .unwrap();
    let err = f::load_hosts().unwrap_err();
    assert!(
        format!("{err}").contains("duplicate host name 'dev'"),
        "{err}"
    );

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
    // Two-sided timing around the 1s deadline: the lower bound proves
    // the timeout path waited (an instant "timed out" for the wrong
    // reason still passes the error assert above), the upper bound is
    // hang protection. Load only stretches elapsed, so neither flakes.
    let elapsed = start.elapsed();
    assert!(
        elapsed >= std::time::Duration::from_secs(1),
        "ssh timeout returned before its 1s deadline ({elapsed:?})"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "ssh timeout overran ({elapsed:?})"
    );

    // Batch G (finding 46): a dash-leading target must reach ssh as the
    // hostname behind a `--` separator, never as an ssh option.
    write_exe(&bin.join("ssh"), "#!/bin/sh\nprintf '%s\\n' \"$@\"\n");
    let out = f::ssh_run("-oProxyCommand=evil", &["echo", "hi"], 5).unwrap();
    let argv: Vec<&str> = out.lines().collect();
    let sep = argv.iter().position(|a| *a == "--").expect("-- separator");
    assert_eq!(
        argv[sep + 1],
        "-oProxyCommand=evil",
        "target must follow --: {argv:?}"
    );

    // Batch 3: remote capture builds ONE shell command string and
    // single-quotes the pane id, so a `;`/`$()`/backtick in the pane
    // id (e.g. from a compromised remote `status --json`) cannot
    // execute remotely. The fake ssh echoes each of its argv on its
    // own line; the remote command is the trailing element.
    let out = f::remote_capture("fake-target", "%3", 60).unwrap();
    let argv: Vec<&str> = out.lines().collect();
    let cmd = argv.last().expect("remote command");
    for want in ["tmux", "capture-pane", "-p", "-e", "-t", "%3", "-S", "-60"] {
        assert!(cmd.contains(want), "missing {want}: {cmd}");
    }
    assert!(cmd.contains("'%3'"), "pane must be single-quoted: {cmd}");

    // Same with a hostile pane id: the malicious suffix is wrapped in
    // single quotes and cannot reach the remote shell.
    let evil = "%3; touch /tmp/pwned";
    let out = f::remote_capture("fake-target", evil, 10).unwrap();
    let cmd = out.lines().last().expect("remote command");
    assert!(
        cmd.contains("'%3; touch /tmp/pwned'"),
        "evil pane must be single-quoted whole: {cmd}"
    );
    // The FAKE_SSH joined argv with spaces; the only shell metachars
    // it saw are inside the single-quoted string, which the remote
    // shell will treat as a single token (the whole `%3; touch ...`
    // is one pane-id literal). Verify the evil payload sits inside
    // the single-quoted pane token, not bare in the command.
    let q = cmd.find("'%3; touch /tmp/pwned'");
    assert!(q.is_some(), "evil payload must be inside quotes: {cmd}");

    // Batch 3 (continued): attach_remote_argv also single-quotes the
    // session name.
    let argv = f::attach_remote_argv("t", Some("s; rm"));
    let last = argv.last().expect("remote cmd");
    assert_eq!(argv[0], "-t");
    assert_eq!(argv[1], "--");
    assert_eq!(argv[2], "t");
    assert!(
        last.contains("'s; rm'"),
        "session must be single-quoted: {last}"
    );
    let argv = f::attach_remote_argv("t", None);
    assert_eq!(argv.last().unwrap(), "tmux attach-session");

    // Batch 3: fleet fetch turns a remote snapshot into table rows.
    write_exe(&bin.join("ssh"), FAKE_SSH);
    // The corrupt-file phase poisoned the registry; start that file over.
    std::fs::write(f::fleet_path(), "").unwrap();
    f::add_host("dev", "fake-target", f::Mode::Observe, "").unwrap();
    let mut app = terhub::app::App::new(terhub::config::AppConfig::default());
    app.fetch_fleet();
    assert_eq!(app.fleet_rows.len(), 1);
    assert_eq!(app.fleet_rows[0].id, "dev:%1");
    assert_eq!(app.fleet_agents.len(), 1);
    assert_eq!(app.fleet_agents[0].agent, "codex");
    assert_eq!(
        app.remote_target("dev:%1"),
        Some(("dev".to_string(), "s".to_string()))
    );
    assert!(app.fleet_errors.is_empty());

    let _ = std::fs::remove_dir_all(&base);
}
