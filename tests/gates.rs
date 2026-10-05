// Batch G (finding 45): deny configuration must exist and gate CI.
use std::fs;

#[test]
fn deny_config_exists_with_strict_bans() {
    let text =
        fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/deny.toml")).expect("deny.toml");
    assert!(
        text.contains("[advisories]"),
        "must configure advisories gate"
    );
    assert!(text.contains("[bans]"), "must configure license/code bans");
    assert!(text.contains("[licenses]"), "must configure licenses gate");
}

#[test]
fn ci_workflow_runs_gates() {
    let text = fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/.github/workflows/ci.yml"
    ))
    .expect("ci.yml");
    for gate in ["fmt --check", "clippy", "cargo test", "cargo-deny-action"] {
        assert!(text.contains(gate), "CI must run {gate}");
    }
}
