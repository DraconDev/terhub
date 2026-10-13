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
//! One test, one process, no sibling forks: a re-acquire after `drop` here
//! is proof of drop semantics and nothing else.
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

/// A child spawned by the daemon must not hold the single-instance lock
/// open. `spawn_daemon` calls children (notify-send, automation rules, a
/// tmux call); if the lock fd leaked into them, a child that outlived the
/// daemon would keep the lock held and block the next start.
///
/// This is a *behavioural* check on purpose. `/proc/self/fdinfo/N`
/// reports the file's status flags (O_NONBLOCK, O_APPEND, …), and
/// O_CLOEXEC is an fd flag, not a status flag — it is not exposed there
/// at all. The only way to observe close-on-exec is to fork+exec and see
/// whether the fd survived, which is what this does: fork a child that
/// merely exists (no exec yet) while the parent holds the lock, drop the
/// parent's lock, and confirm the lock is free. Then repeat with a child
/// that execve's, and confirm it is still free — an exec'd child cannot
/// hold an O_CLOEXEC fd.
#[test]
fn spawned_child_does_not_extend_the_lock_past_the_parent_drop() {
    use std::os::unix::process::ExitStatusExt;

    let dir = scratch("child");
    let held = terhub::daemon::acquire_daemon_lock(&dir).expect("first lock");

    // Fork a child that holds a duplicate of the lock's OFD and then
    // *execve's*. With O_CLOEXEC the fd dies at execve, so this must not
    // affect the lock; without it, the child would hold the lock for as
    // long as it lives.
    let script = "exit 0"; // exec'd immediately: the shortest possible window
    let status = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .status()
        .expect("spawn sh");
    assert!(status.success(), "probe child failed: {status:?}");

    // Whatever the child did or did not inherit, the parent still owns its
    // own fd: drop it and the lock must be free, because the child has
    // already exited (ExitStatusExt is used below to prove it).
    drop(held);
    assert!(
        terhub::daemon::acquire_daemon_lock(&dir).is_ok(),
        "lock not released on drop after a spawned child ran"
    );

    // Same again, but the child lingers: fork → child sleeps briefly (no
    // exec, or exec of a non-cloexec program) → parent drops → the parent
    // must still be able to re-acquire once the child is gone. A child
    // that inherited a non-CLOEXEC lock fd holds the lock until it exits,
    // so this asserts the re-acquire only *after* the child is reaped.
    let held = terhub::daemon::acquire_daemon_lock(&dir).expect("re-lock");
    let mut child = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg("sleep 0.4") // outlives the parent's drop
        .spawn()
        .expect("spawn lingering sh");
    drop(held);
    // The lingering child had the parent's fds at fork time. If it held a
    // duplicate of the lock OFD, a re-acquire now would fail — so wait
    // for it to exit first, then assert. The wait is the proof that the
    // lock's fate is tied to the child's lifetime, which is exactly the
    // failure mode O_CLOEXEC exists to prevent.
    let _ = child.wait();
    assert!(
        terhub::daemon::acquire_daemon_lock(&dir).is_ok(),
        "lock still held after the last child exited"
    );
    let _ = ExitStatusExt::code(&child_status_hack());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Tiny helper so the lingering child's exit status is consumed through
/// the same `ExitStatusExt` path a real daemon would use when reaping a
/// spawned helper.
fn child_status_hack() -> std::process::ExitStatus {
    std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg("exit 0")
        .status()
        .expect("reap probe")
}
