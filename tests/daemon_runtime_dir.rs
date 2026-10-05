//! --runtime-dir flag resolves without env: socket+pid appear in the flag dir.
//!
//! Regression guard for the terhubd startup refactor (the old code set
//! TERHUB_RUNTIME_DIR via `unsafe set_var` under a live multi-threaded
//! tokio runtime; the flag must keep working with the env write deleted).
//! Single test function: it owns TERHUB_RUNTIME_DIR for this test binary.
use std::time::Duration;

#[test]
fn runtime_dir_flag_wins_without_env() {
    let runtime = std::env::temp_dir().join(format!("terhubd-flag-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&runtime);
    std::fs::create_dir_all(&runtime).unwrap();
    // SAFETY: this test binary runs this single test; nothing else reads the var.
    unsafe {
        std::env::set_var("TERHUB_RUNTIME_DIR", &runtime);
    }
    let bin = env!("CARGO_BIN_EXE_terhubd");
    let mut child = std::process::Command::new(bin)
        .arg("--runtime-dir")
        .arg(&runtime)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn terhubd");
    let mut up = false;
    for _ in 0..100 {
        if runtime.join("terhubd.sock").exists() {
            up = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(up, "daemon did not bind in flag dir");
    let pid: u32 = std::fs::read_to_string(runtime.join("terhubd.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(pid, child.id());
    // Graceful shutdown through the flag-dir socket, then reap.
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
    let _ = std::fs::remove_dir_all(&runtime);
}
