//! Verifies that the `make build` install pipeline is in place and
//! runs the install step. The full behavioral check (does the
//! binary actually update?) lives in `scripts/check-install.sh`
//! and is run by `make check-install` / CI; this integration
//! test guards the wiring (Makefile, script, target names) so a
//! well-meaning refactor can't quietly drop the install step.

use std::path::Path;

const MAKEFILE: &str = "Makefile";
const SCRIPT: &str = "scripts/check-install.sh";

#[test]
fn makefile_defines_build_target_that_installs() {
    let body = std::fs::read_to_string(MAKEFILE)
        .unwrap_or_else(|e| panic!("read {MAKEFILE}: {e}"));
    // The build target must call both `cargo build` (so the
    // working tree is current) AND `cargo install` (so the
    // $PATH binary is too). Without the second, the stale
    // ~/.cargo/bin/terhub footgun returns.
    assert!(
        body.lines().any(|l| l.trim_start().starts_with("build:")),
        "{MAKEFILE} missing `build:` target"
    );
    let build_block = body
        .lines()
        .skip_while(|l| !l.trim_start().starts_with("build:"))
        .take_while(|l| !l.trim_start().starts_with(".") && !l.trim_start().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
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
    let check_block = body
        .lines()
        .skip_while(|l| !l.trim_start().starts_with("check-install:"))
        .take_while(|l| !l.trim_start().starts_with(".") && !l.trim_start().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
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
