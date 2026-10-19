//! The temp-dir sweeper: scratch space whose owner is gone is removed,
//! scratch space whose owner is alive is left alone.
//!
//! The suites create scratch paths under the system temp dir named
//! `terhub-<what>-<pid>…`. A binary that ends — or is killed — leaves
//! them behind, and they accumulate into hundreds of directories.
//!
//! Single test: owns the sandbox for this binary.
mod common;

use std::path::PathBuf;

/// A pid that has certainly exited: a child we waited for.
fn dead_pid() -> u32 {
    let mut child = std::process::Command::new("true")
        .spawn()
        .expect("spawn true");
    let status = child.wait().expect("wait true");
    assert!(status.success());
    child.id()
}

fn scratch(tag: &str, pid: u32) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("terhub-sweeptest-{tag}-{pid}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

#[test]
fn sweep_removes_dead_owner_and_keeps_live_owner() {
    let _sandbox = common::Sandbox::new("sweep");

    let dead = scratch("dead", dead_pid());
    let live = scratch("live", std::process::id());
    // A path with no pid in its name is a fixed global scratch dir
    // (the kwin helper's), not a run's leftovers: never removed.
    let nameless = std::env::temp_dir().join("terhub-sweeptest-nopid");
    let _ = std::fs::remove_dir_all(&nameless);
    std::fs::create_dir_all(&nameless).expect("create nameless scratch dir");

    common::sweep_stale_tempdirs();

    assert!(
        !dead.exists(),
        "scratch dir of a dead owner survived the sweep: {}",
        dead.display()
    );
    assert!(
        live.exists(),
        "sweep removed scratch space belonging to a live process: {}",
        live.display()
    );
    assert!(
        nameless.exists(),
        "sweep removed a scratch path with no owner pid in its name: {}",
        nameless.display()
    );

    let _ = std::fs::remove_dir_all(&live);
    let _ = std::fs::remove_dir_all(&nameless);
}
