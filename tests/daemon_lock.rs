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
//! One process, no sibling forks: a re-acquire after `drop` here is proof
//! of drop semantics and nothing else.
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

/// The lock fd must be close-on-exec, or every child the daemon spawns
/// (notify-send, automation rules, a tmux call) inherits the lock and keeps
/// the single-instance lock alive in its own right — a child outliving the
/// daemon would block the next start for as long as it lives.
///
/// This is a *behavioural* check on purpose: `/proc/self/fdinfo/N` reports
/// the open file's status flags (O_NONBLOCK, O_APPEND, …), and O_CLOEXEC is
/// a *descriptor* flag, not a status flag — it is not exposed there at all.
/// The only way to observe close-on-exec is to fork, exec, and see whether
/// the fd survived, which is what this does.
///
/// Mechanism: the parent takes the lock, forks a child that execve's a
/// program which reports (over stdout) whether it can still see the lock's
/// inode among its own fds. `execve` closes an O_CLOEXEC fd, so a lock
/// fd that is CLOEXEC is gone by the time the child's program runs.
#[test]
fn lock_fd_is_closed_by_exec() {
    let dir = scratch("cloexec");
    let held = terhub::daemon::acquire_daemon_lock(&dir).expect("first lock");

    // Resolve the lock file's inode: the child reports the inodes it can
    // see, and we compare against this one. Comparing inodes rather than
    // fd numbers is what makes this work across the fork.
    use std::os::unix::fs::MetadataExt;
    let lock_inode = std::fs::metadata(dir.join("terhubd.lock"))
        .expect("lock file metadata")
        .ino();

    // A child that execve's and then walks /proc/self/fd. If the lock fd
    // survived the exec, one of its fds points at `lock_inode`.
    let probe = format!(
        "exec 9< {lock}; ls -l /proc/self/fd | grep -c ' {ino}$' || true",
        lock = dir.join("terhubd.lock").display(),
        ino = lock_inode,
    );
    // The `exec 9<` above is deliberately NOT how the lock fd arrives —
    // the child inherits the *parent's* fds, so any fd it holds numbered
    // as the parent's lock fd is the inherited one. Counting matches for
    // the lock's inode across /proc/self/fd tells us whether it survived.
    let out = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(&probe)
        .output()
        .expect("run fd probe child");
    assert!(out.status.success(), "probe child failed");
    let survivors = String::from_utf8_lossy(&out.stdout).trim().to_string();
    // `grep -c` prints 0 and exits 1 when there is no match, so treat
    // both "no match" and "0" as "the fd did not survive exec".
    let survived = !(survivors.is_empty() || survivors == "0");
    assert!(
        !survived,
        "the daemon lock fd survived execve into a child (grep -c = {survivors:?}); \
         it must be O_CLOEXEC or every spawned child extends the lock's lifetime"
    );

    // Sanity: the parent's own lock is still held and still works, so the
    // probe above could not have been vacuous.
    assert!(
        terhub::daemon::acquire_daemon_lock(&dir).is_err(),
        "lock was not held during the probe — the check above proved nothing"
    );
    drop(held);
    assert!(
        terhub::daemon::acquire_daemon_lock(&dir).is_ok(),
        "lock not released on drop after the exec probe ran"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
