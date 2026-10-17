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

/// Third-party actions must pin an immutable commit SHA, not a
/// floating major tag: a moved `v4`/`v2`/`stable` tag silently swaps
/// the code CI executes. The human-readable tag stays as a trailing
/// comment; the SHA governs. Every `uses:` line must therefore carry
/// a 40-hex SHA after the `@`.
#[test]
fn ci_actions_pin_shas_not_floating_tags() {
    let text = ci_yml();
    let mut unpinned = Vec::new();
    for line in text.lines() {
        let code = line.split('#').next().unwrap_or("").trim();
        let Some(at) = code.find("uses:") else {
            continue;
        };
        let after = code[at..].split('@').nth(1).unwrap_or("");
        let sha: String = after.chars().take_while(|c| !c.is_whitespace()).collect();
        let pinned = sha.len() == 40 && sha.chars().all(|c| c.is_ascii_hexdigit());
        if !pinned {
            unpinned.push(code.to_string());
        }
    }
    assert!(
        unpinned.is_empty(),
        "CI actions must pin a 40-hex SHA, floating tags found: {unpinned:?}"
    );
}

/// Substring pins stay green when a gate step is semantically
/// neutralized: `continue-on-error: true` swallows its failure and
/// `if: false` skips it entirely, while `fmt --check`, `clippy`,
/// `cargo test` and `cargo-deny-action` all still appear in the file.
/// The gates job must therefore carry neither knob — if a conditional
/// or best-effort step is ever genuinely needed, this test forces the
/// conversation instead of silently blessing it.
#[test]
fn ci_gate_steps_cannot_be_neutralized() {
    let text = ci_yml();
    // Even `continue-on-error: false` is banned: it is a one-word
    // flip from silencing a gate, and the gates job has no step
    // whose failure is acceptable.
    assert!(
        !text.contains("continue-on-error"),
        "CI must not use continue-on-error (a failing gate must fail the job)"
    );
    // Step/job-level `if:` skips the step with every substring pin
    // intact. Scan code lines only — comments may discuss `if:` freely.
    let conditional: Vec<&str> = text
        .lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter(|code| code.starts_with("if:") || code.starts_with("if :"))
        .collect();
    assert!(
        conditional.is_empty(),
        "CI gate steps must be unconditional, found: {conditional:?}"
    );
}

/// `make test` / `make clippy` must run the same scope as the CI
/// gates they stand in for locally: a `--lib --tests` subset lets a
/// local green pass while CI (all-targets, everything) fails. Pin
/// the exact CI command strings inside the Makefile recipes so
/// weakening either side breaks this test.
#[test]
fn make_targets_mirror_ci_gate_scope() {
    let make =
        fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Makefile")).expect("Makefile");
    let ci = ci_yml();
    for cmd in [
        "cargo test --offline",
        "cargo clippy --offline --all-targets -- -D warnings",
    ] {
        assert!(ci.contains(cmd), "CI must run `{cmd}`");
        assert!(
            make.contains(cmd),
            "Makefile must run the CI command `{cmd}` (local green must equal CI green)"
        );
    }
}
