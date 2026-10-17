//! fleet_status must bound THREADS, not just ssh children: the
//! FLEET_FANOUT semaphore gates ssh, but one-thread-per-host piles
//! hundreds of threads on acquire with a large slow fleet.
//!
//! Single test in this binary: owns PATH.
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicBool, Ordering};

fn task_count() -> usize {
    std::fs::read_dir("/proc/self/task")
        .map(|d| d.count())
        .unwrap_or(0)
}

#[test]
fn fleet_status_spawns_bounded_threads() {
    use terhub::fleet as f;
    // 64 hosts against a blocking fake ssh: every per-host thread
    // piles up for seconds, so a 1ms sampler sees the true peak.
    // (16 ssh × (1 worker + 2 drain readers) = 48 steady-state
    // threads post-fix; one-thread-per-host peaks near 64 + 32.)
    const HOSTS: usize = 64;
    let base = std::env::temp_dir().join(format!("terhub-fleet-threads-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let bin = base.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let ssh = bin.join("ssh");
    std::fs::write(&ssh, "#!/bin/sh\nsleep 2\n").unwrap();
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
    // SAFETY: single test in this binary.
    unsafe {
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
        std::env::set_var("PATH", path);
    }

    let hosts: Vec<f::Host> = (0..HOSTS)
        .map(|i| f::Host {
            name: format!("h{i:02}"),
            target: "fake-target".to_string(),
            mode: f::Mode::Observe,
            label: String::new(),
        })
        .collect();
    eprintln!("CKPT hosts-built");
    let floor = task_count();
    eprintln!("CKPT floor");
    let peak = std::sync::Arc::new(std::sync::Mutex::new(floor));
    let done = std::sync::Arc::new(AtomicBool::new(false));
    let sampler = {
        let peak = std::sync::Arc::clone(&peak);
        let done = std::sync::Arc::clone(&done);
        std::thread::spawn(move || {
            while !done.load(Ordering::Relaxed) {
                let n = task_count();
                *peak.lock().unwrap() = (*peak.lock().unwrap()).max(n);
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        })
    };
    eprintln!("CKPT sampler-up");
    let statuses = f::fleet_status(&hosts);
    eprintln!("CKPT status-done");
    done.store(true, Ordering::Relaxed);
    sampler.join().unwrap();
    assert_eq!(statuses.len(), HOSTS);
    let extra = peak.lock().unwrap().saturating_sub(floor);
    assert!(
        extra <= 64,
        "fleet_status needed {extra} extra threads for {HOSTS} hosts: threads are not bounded"
    );
    let _ = std::fs::remove_dir_all(&base);
}
