// Batch G (finding 45): deny configuration must exist and gate CI.
// Pass-3 hardening: pin the strict *values*, not just the section
// headers — flipping `yanked = "warn"` or `wildcards = "allow"` used
// to leave every assertion green while `cargo deny check` happily
// enforced the weakened file.
use std::fs;

fn deny_toml() -> String {
    fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/deny.toml")).expect("deny.toml")
}

fn ci_yml() -> String {
    fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/.github/workflows/ci.yml"
    ))
    .expect("ci.yml")
}

#[test]
fn deny_config_exists_with_strict_bans() {
    let text = deny_toml();
    assert!(
        text.contains("[advisories]"),
        "must configure advisories gate"
    );
    assert!(text.contains("[bans]"), "must configure license/code bans");
    assert!(text.contains("[licenses]"), "must configure licenses gate");
    // The strict values themselves: weakening any of these must fail.
    for strict in [
        "yanked = \"deny\"",
        "wildcards = \"deny\"",
        "unknown-registry = \"deny\"",
        "unknown-git = \"deny\"",
    ] {
        assert!(text.contains(strict), "deny.toml must pin {strict}");
    }
    // The license allow-list is an allow-list: it must name the
    // approved licenses and never open the gate to everything.
    assert!(
        text.contains("allow = ["),
        "licenses must use an allow list"
    );
    for lic in ["MIT", "Apache-2.0"] {
        assert!(
            text.contains(&format!("\"{lic}\"")),
            "license allow-list must include {lic}"
        );
    }
    assert!(
        !text.contains("\"*\""),
        "license allow-list must not wildcard-allow every license"
    );
}

#[test]
fn ci_workflow_runs_gates() {
    let text = ci_yml();
    for gate in ["fmt --check", "clippy", "cargo test", "cargo-deny-action"] {
        assert!(text.contains(gate), "CI must run {gate}");
    }
    // Cold-runner wiring: dependencies are fetched before any
    // --offline gate, and tmux exists for the suites that need it.
    let fetch_at = text.find("cargo fetch").expect("CI must fetch deps");
    // NB: search for the step's run command — "clippy" alone also
    // matches the toolchain's `components:` line, which precedes it.
    let clippy_at = text.find("cargo clippy").expect("CI must run clippy");
    assert!(
        fetch_at < clippy_at,
        "cargo fetch must precede the offline clippy gate"
    );
    assert!(text.contains("tmux"), "CI must install tmux for the suites");
    // The stale-binary behavioural gate actually runs in CI (the
    // tests/install_target.rs header claim).
    assert!(
        text.contains("check-install"),
        "CI must run the check-install gate"
    );
}
