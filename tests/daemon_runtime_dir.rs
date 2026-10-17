//! --runtime-dir flag beats TERHUB_RUNTIME_DIR: socket+pid appear in the
//! flag dir while the env dir stays empty.
//!
//! Regression guard for the terhubd startup refactor (the old code set
//! TERHUB_RUNTIME_DIR via `unsafe set_var` under a live multi-threaded
//! tokio runtime; the flag must keep working with the env write deleted).
//! The env and flag deliberately point at DIFFERENT dirs: with both aimed
//! at the same dir (as this test originally did) flag-vs-env resolution
//! is indistinguishable and the test cannot prove its own title.
//! Single test function: it owns TERHUB_RUNTIME_DIR for this test binary.
use std::time::Duration;

#[test]
fn runtime_dir_flag_wins_over_env() {
    let pid = std::process::id();
    let flag_dir = std::env::temp_dir().join(format!("terhubd-flag-{pid}"));
    let env_dir = std::env::temp_dir().join(format!("terhubd-env-{pid}"));
    let _ = std::fs::remove_dir_all(&flag_dir);
    let _ = std::fs::remove_dir_all(&env_dir);
    std::fs::create_dir_all(&flag_dir).unwrap();
    std::fs::create_dir_all(&env_dir).unwrap();
    // SAFETY: this test binary runs this single test; nothing else reads the var.
    unsafe {
        std::env::set_var("TERHUB_RUNTIME_DIR", &env_dir);
    }
    let bin = env!("CARGO_BIN_EXE_terhubd");
    let mut child = std::process::Command::new(bin)
        .arg("--runtime-dir")
        .arg(&flag_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn terhubd");
    let mut up = false;
    for _ in 0..100 {
        if flag_dir.join("terhubd.sock").exists() {
            up = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(up, "daemon did not bind in flag dir");
    let pid: u32 = std::fs::read_to_string(flag_dir.join("terhubd.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(pid, child.id());
    // The env dir must stay untouched: had the flag lost to the env,
    // the socket would be here instead.
    assert!(
        !env_dir.join("terhubd.sock").exists(),
        "daemon bound in env dir: --runtime-dir flag did not win"
    );
    assert!(
        !env_dir.join("terhubd.pid").exists(),
        "daemon wrote pid in env dir: --runtime-dir flag did not win"
    );
    // Graceful shutdown through the flag-dir socket: send_request
    // resolves via the env, so aim the env at the flag dir first.
    // SAFETY: same single-test ownership as above.
    unsafe {
        std::env::set_var("TERHUB_RUNTIME_DIR", &flag_dir);
    }
    let _ =
        terhub::daemon::send_request(&terhub::daemon::Request::Shutdown, Duration::from_secs(2));
    let mut exited = false;
    for _ in 0..50 {
        match child.try_wait() {
            Ok(Some(_)) => {
                exited = true;
                break;
            }
            _ => std::thread::sleep(Duration::from_millis(100)),
        }
    }
    if !exited {
        let _ = child.kill();
        let _ = child.wait();
        panic!("daemon would not stop");
    }
    let _ = std::fs::remove_dir_all(&flag_dir);
    let _ = std::fs::remove_dir_all(&env_dir);
}
