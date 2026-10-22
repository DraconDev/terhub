//! Capturing many pane screens at once.
//!
//! The daemon polls every pane on a timer, and each poll needs one
//! `tmux capture-pane` per pane: a separate child process, bounded at
//! `TMUX_CMD_TIMEOUT_SECS`. Run one after another, a poll costs
//! N × timeout in the worst case, and one wedged tmux stalls every
//! other pane's capture behind it.
//!
//! [`capture_screens`] runs the captures through a bounded worker pool
//! instead: the wall-clock cost drops to ceil(N/FANOUT) × timeout while
//! the thread and child counts stay flat no matter how many panes are
//! open.

use crate::tmux::{Pane, Tmux};
use std::collections::HashMap;

/// Worker count for the per-pane screen captures.
///
/// One worker runs one `tmux capture-pane` child at a time, so this
/// caps threads AND children together — the same reasoning as the
/// fleet's SSH fan-out. Eight keeps a poll responsive without
/// spraying a dozen concurrent tmux children at a machine that may
/// already be busy.
pub const CAPTURE_FANOUT: usize = 8;

/// How many lines of scrollback a screen capture keeps.
const CAPTURE_LINES: u32 = 30;

/// Capture the screens of the panes named by `wanted`, concurrently.
///
/// `wanted` indexes into `panes`: the caller decides which panes are
/// worth a capture at all (a pane nobody has claimed yet does not need
/// one). The result is keyed by pane id; a pane whose capture failed is
/// simply absent, which is what the serial version's `.ok()` produced
/// too — a failed capture reads as "no screen", never as an empty one.
///
/// A capture that fails for one pane does not affect the others: each
/// worker owns its own child, and a failed capture is dropped rather
/// than propagated.
pub fn capture_screens(
    tmux: &Tmux,
    panes: &[Pane],
    wanted: &[usize],
) -> HashMap<String, String> {
    let mut out: HashMap<String, String> = HashMap::new();
    if wanted.is_empty() {
        return out;
    }
    // One pane: the pool would cost more than it saves.
    if wanted.len() == 1 {
        let pane = &panes[wanted[0]];
        if let Ok(screen) = tmux.capture_pane(&pane.id, CAPTURE_LINES) {
            out.insert(pane.id.clone(), screen);
        }
        return out;
    }
    // One slot per capture, so results come back in request order and
    // no two workers write the same place.
    let slots: Vec<std::sync::Mutex<Option<(String, String)>>> =
        (0..wanted.len()).map(|_| std::sync::Mutex::new(None)).collect();
    let next = std::sync::Mutex::new(0usize);
    std::thread::scope(|s| {
        for _ in 0..wanted.len().min(CAPTURE_FANOUT) {
            s.spawn(|| {
                loop {
                    let i = {
                        // Tolerate a poisoned index (a dead sibling):
                        // carry on from its counter rather than cascading.
                        let mut n = next.lock().unwrap_or_else(|e| e.into_inner());
                        if *n >= wanted.len() {
                            return;
                        }
                        let i = *n;
                        *n += 1;
                        i
                    };
                    let pane = &panes[wanted[i]];
                    let Ok(screen) = tmux.capture_pane(&pane.id, CAPTURE_LINES) else {
                        // A failed capture is dropped, exactly like the
                        // serial `.ok()` — the pane reads as "no screen".
                        continue;
                    };
                    let mut slot = slots[i].lock().unwrap_or_else(|e| e.into_inner());
                    *slot = Some((pane.id.clone(), screen));
                }
            });
        }
    });
    for slot in slots {
        if let Some((id, screen)) = slot.into_inner().unwrap_or_else(|e| e.into_inner()) {
            out.insert(id, screen);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// A fake tmux whose `capture-pane` blocks until `expected` distinct
    /// panes have been captured.
    ///
    /// This is the concurrency proof: run the captures one after
    /// another and the first one waits for a marker that only the
    /// second could create — so it times out and the capture is lost.
    /// Run them together and every marker appears and all captures
    /// succeed.
    ///
    /// The wait is bounded well under the tmux client's own 5s command
    /// timeout: a capture that overran it would be killed, and the
    /// test would pass for the wrong reason (a lost capture rather
    /// than a lost concurrency).
    fn fake_tmux_barrier(dir: &Path, expected: usize) -> Tmux {
        use std::os::unix::fs::PermissionsExt;
        let markers = dir.join("markers");
        std::fs::create_dir_all(&markers).unwrap();
        let script = format!(
            "#!/bin/sh\ncase \"$1\" in\n  capture-pane)\n    pane=\"$4\"\n    touch '{m}'/\"$pane\"\n    i=0\n    while [ \"$(ls '{m}' | wc -l)\" -lt {n} ]; do\n      i=$((i+1))\n      if [ $i -gt 100 ]; then exit 1; fi\n      sleep 0.01\n    done\n    printf 'screen for %s' \"$pane\"\n    ;;\n  *) printf '' ;;\nesac\n",
            m = markers.display(),
            n = expected,
        );
        let path = dir.join("tmux");
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        Tmux::test(path)
    }

    fn pane(id: &str) -> Pane {
        Pane::test(id, "w", "sleep 1")
    }

    #[test]
    fn captures_run_together_not_one_after_another() {
        let dir = std::env::temp_dir().join(format!("terhub-cap-pool-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let panes: Vec<Pane> = (0..4).map(|i| pane(&format!("%{i}"))).collect();
        let tmux = fake_tmux_barrier(&dir, panes.len());
        let wanted: Vec<usize> = (0..panes.len()).collect();

        let screens = capture_screens(&tmux, &panes, &wanted);

        // Every pane came back: the barrier could only be cleared if
        // all four captures were in flight at once.
        assert_eq!(screens.len(), panes.len(), "{screens:?}");
        for p in &panes {
            assert_eq!(
                screens.get(&p.id).map(String::as_str),
                Some(format!("screen for {}", p.id).as_str())
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_capture_leaves_the_others_intact() {
        let dir = std::env::temp_dir().join(format!("terhub-cap-fail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        use std::os::unix::fs::PermissionsExt;
        // The fake fails for one pane and succeeds for the other: the
        // failure must not take the pool down with it.
        let script = "#!/bin/sh\ncase \"$1\" in\n  capture-pane)\n    if [ \"$4\" = \"%0\" ]; then exit 1; fi\n    printf 'screen for %s' \"$4\"\n    ;;\n  *) printf '' ;;\nesac\n";
        let path = dir.join("tmux");
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let tmux = Tmux::test(path);
        let panes = vec![pane("%0"), pane("%1")];
        let wanted: Vec<usize> = (0..panes.len()).collect();

        let screens = capture_screens(&tmux, &panes, &wanted);

        assert!(!screens.contains_key("%0"), "a failed capture must not appear");
        assert_eq!(screens.get("%1").map(String::as_str), Some("screen for %1"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_request_captures_nothing() {
        let tmux = Tmux::test("/nonexistent-tmux-binary".into());
        let panes = vec![pane("%0")];
        assert!(capture_screens(&tmux, &panes, &[]).is_empty());
    }
}
