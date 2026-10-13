//! Single-instance lock drop semantics, isolated on purpose.
//!
//! `daemon_lock_excludes_second_holder` in src/daemon.rs covers lock
//! *exclusion*. This binary covers *release on drop* — and it lives here
//! rather than beside that test because of what a fork does to an flock.
//!
//! flock is per open-file-description, and `fork` duplicates the
//! description rather than the lock: a child forked while a lock is held
//! keeps that lock alive past the parent's `drop()`, until the child exits
//! (or execve's an O_CLOEXEC-closed image). src/daemon.rs's own test suite
//! forks children constantly (the fake tmux script, the fake muse binary,
//! `sleep 30` reg_pid probes), so an in-process "released on drop"
//! assertion there cannot distinguish "the holder dropped the lock" from
//! "some sibling's child is inside its fork→execve window" — which is
//! exactly the flake the falsification round reproduced (3/6, 3/5, 2/5).
//!
//! One test, one process, no forks: a re-acquire after `drop` here is
//! proof of drop semantics and nothing else.
use std::path::PathBuf;

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("terhub-lock-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn lock_is_released_when_the_holder_drops_it() {
    let dir = scratch("drop");
    let first = terhub::daemon::acquire_daemon_lock(&dir).expect("first lock");
    // Still held: a second open-file-description in this same process is
    // refused while the first lives.
    assert!(
        terhub::daemon::acquire_daemon_lock(&dir).is_err(),
        "second holder was admitted while the first lock was still alive"
    );
    drop(first);
    // Released: a fresh attempt now succeeds. No fork happens anywhere in
    // this binary, so nothing else can be holding a duplicate OFD.
    assert!(
        terhub::daemon::acquire_daemon_lock(&dir).is_ok(),
        "lock not released on drop"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn lock_file_is_opened_close_on_exec() {
    // The daemon spawns children (notify-send, automation rules, tmux
    // calls). Without O_CLOEXEC each inherits the lock fd and keeps the
    // single-instance lock alive in its own right — a child outliving the
    // daemon would block the next start. Verify the flag is actually set,
    // via /proc, rather than trusting the custom_flags call.
    use std::os::unix::io::AsRawFd;
    let dir = scratch("cloexec");
    let held = terhub::daemon::acquire_daemon_lock(&dir).expect("lock");
    let fd = held.as_raw_fd();
    let flags_path = format!("/proc/self/fdinfo/{fd}");
    let info = std::fs::read_to_string(&flags_path).expect("fdinfo for lock fd");
    let cloexec = info
        .lines()
        .find_map(|l| l.strip_prefix("flags:"))
        .and_then(|f| u32::from_str_radix(f.trim().trim_start_matches("0"), 8).ok())
        .map(|flags| flags & libc::O_CLOEXEC != 0);
    assert_eq!(
        cloexec,
        Some(true),
        "daemon lock fd must be O_CLOEXEC so spawned children cannot extend its lifetime:\n{info}"
    );
    drop(held);
    let _ = std::fs::remove_dir_all(&dir);
}
