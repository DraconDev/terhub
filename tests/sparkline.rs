//! End-to-end tests for the needs-you vs done cascade plus activity
//! sparklines. Each test exercises a real piece of public surface
//! (AgentState + the TUI render + the classifier + the proc reader)
//! so regressions in the cascade are loud at the suite boundary.
//!
//! Kept here (not as a lib test mod) so the cascade contract gets
//! its own bin name in the test report — exactly one more suite
//! than the count before this work shipped, so a CI run can spot
//! "the cascade suite vanished" at a glance.

use terhub::agent::{AgentState, CompiledManifest, Manifest};
use terhub::proc::parked_in_tty_read;
use terhub::ui::sparkline_glyphs;

fn make_manifest(done_pat: &str) -> CompiledManifest {
    CompiledManifest::compile(&Manifest {
        agent: "x".to_string(),
        commands: vec!["x".to_string()],
        working: vec!["loading".to_string()],
        waiting: vec!["^❯".to_string()],
        error: vec![],
        limited: vec![],
        done: vec![done_pat.to_string()],
        idle: vec![],
        idle_fallback: vec![],
        waiting_unless: vec![],
        busy: vec![],
        phase: vec![],
    })
    .unwrap()
}

#[test]
fn done_state_is_quiet_not_attention() {
    // The Done FYI row's whole point: a finished agent never raises
    // the red triangle. A regression here would put every quiet
    // finished pane in the tray count.
    assert!(!AgentState::Done.needs_attention());
}

#[test]
fn done_pattern_promotes_matching_tail() {
    let m = vec![make_manifest("all tasks passed")];
    let hit = terhub::agent::classify_screen(
        &m,
        "x",
        "Summary\nall tasks passed · 0 failed\n",
        false,
        false,
    )
    .unwrap();
    assert_eq!(hit.state, AgentState::Done);
}

#[test]
fn bell_flag_promotes_to_waiting() {
    let m = vec![make_manifest("all tasks passed")];
    let quiet = "loading 0.1\n";
    // Without bell: a screen showing only the "loading" footer
    // reads as Working (the manifest's working pattern matches).
    let plain = terhub::agent::classify_screen(&m, "x", quiet, false, false).unwrap();
    assert_eq!(plain.state, AgentState::Working);
    // With bell: the same screen upgrades to Waiting, summary
    // surfaces `🔔` so the row chip can render the reason.
    let ringed = terhub::agent::classify_screen(&m, "x", quiet, true, false).unwrap();
    assert_eq!(ringed.state, AgentState::Waiting);
    assert!(ringed.summary.contains("🔔"));
}

#[test]
fn parked_alone_does_not_promote() {
    // The contract: parked kernel state + no prompt-shape line =
    // stays Working. A parked `nvim` or `fzf` is not a needs-you
    // reason. Only parked + a real waiting line in the tail
    // upgrades the verdict.
    let m = vec![make_manifest("all tasks passed")];
    let prompt = "loading 0.1\n❯ do you want to proceed?\n";
    let with_prompt = terhub::agent::classify_screen(&m, "x", prompt, false, true).unwrap();
    assert_eq!(
        with_prompt.state,
        AgentState::Waiting,
        "parked + prompt must read Waiting"
    );
    let no_prompt = terhub::agent::classify_screen(&m, "x", "loading 0.1\n", false, true).unwrap();
    assert_eq!(
        no_prompt.state,
        AgentState::Working,
        "parked alone must not promote to Waiting"
    );
}

#[test]
fn sparkline_uses_log_scale_so_a_burst_still_tall() {
    // A burst of 4096 bytes should render the full-height glyph
    // (█) at the matching bucket; quiet neighbors stay small
    // (▁). Without log scaling, the single busy tick and the
    // quiet ones would render as the same bar — defeating the
    // "just went quiet" signal the sparkline is for.
    let mut buckets = [0u32; 6];
    buckets[3] = 4096; // one tall bar
    let out = sparkline_glyphs(&buckets);
    let chars: Vec<char> = out.chars().collect();
    assert_eq!(chars.len(), 6);
    assert!(
        chars[3] as u32 >= '▆' as u32,
        "4096-byte burst should render tall, got {out:?}"
    );
    assert!(
        chars[0] as u32 <= '▁' as u32,
        "quiet bucket should render short, got {out:?}"
    );
}

#[test]
fn proc_reader_responds_safely_to_missing_pid() {
    // The /proc read happens every tick on every pane. A stale
    // pid (exited, recycled) must NOT panic and must NOT report
    // parked — false positives here would promote every recycled
    // shell to needs-you.
    let bogus_pid = u32::MAX;
    assert!(!parked_in_tty_read(bogus_pid));
}
