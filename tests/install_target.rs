//! Verifies that the `make build` install pipeline is in place and
//! runs the install step. The full behavioral check (does the
//! binary actually update?) lives in `scripts/check-install.sh`
//! and is run by `make check-install` / CI; this integration
//! test guards the wiring (Makefile, script, target names) so a
//! well-meaning refactor can't quietly drop the install step.

use std::path::Path;

const MAKEFILE: &str = "Makefile";
const SCRIPT: &str = "scripts/check-install.sh";

/// Extract the body of a Makefile target: the indented lines
/// following the `name:` header, until the first non-indented
/// line. Returns "" if the target isn't defined.
fn target_body(makefile: &str, name: &str) -> String {
    let header = format!("{name}:");
    let mut iter = makefile.lines();
    // Find the header.
    loop {
        match iter.next() {
            Some(l) if l.trim_start().starts_with(&header) => break,
            Some(_) => continue,
            None => return String::new(),
        }
    }
    // Collect indented (or blank) lines.
    let mut body = String::new();
    for l in iter {
        if l.is_empty() || l.starts_with([' ', '\t']) {
            body.push_str(l);
            body.push('\n');
        } else {
            break;
        }
    }
    body
}

#[test]
fn makefile_defines_build_target_that_installs() {
    let body = std::fs::read_to_string(MAKEFILE)
        .unwrap_or_else(|e| panic!("read {MAKEFILE}: {e}"));
    let build_block = target_body(&body, "build");
    assert!(
        !build_block.is_empty(),
        "{MAKEFILE} missing `build:` target"
    );
    // The build target must call both `cargo build` (so the
    // working tree is current) AND `cargo install` (so the
    // $PATH binary is too). Without the second, the stale
    // ~/.cargo/bin/terhub footgun returns.
    assert!(
        build_block.contains("cargo build"),
        "{MAKEFILE} `build` target must run `cargo build`"
    );
    assert!(
        build_block.contains("cargo install"),
        "{MAKEFILE} `build` target must run `cargo install` (without it the \
         stale-binary footgun returns)"
    );
}

#[test]
fn makefile_has_check_install_target_pointing_at_script() {
    let body = std::fs::read_to_string(MAKEFILE).unwrap();
    let check_block = target_body(&body, "check-install");
    assert!(
        !check_block.is_empty(),
        "{MAKEFILE} missing `check-install:` target"
    );
    assert!(
        check_block.contains("check-install.sh"),
        "{MAKEFILE} `check-install` target must invoke the script"
    );
}

#[test]
fn install_check_script_exists_and_is_executable() {
    let p = Path::new(SCRIPT);
    assert!(p.exists(), "{SCRIPT} missing");
    let meta = std::fs::metadata(p).expect("stat script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode();
        assert!(
            mode & 0o111 != 0,
            "{SCRIPT} must be executable (mode {mode:o})"
        );
    }
    let body = std::fs::read_to_string(p).unwrap();
    assert!(body.starts_with("#!"), "{SCRIPT} missing shebang");
}
