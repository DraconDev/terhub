//! find_tmux must skip a non-executable PATH shadow instead of
//! selecting it and failing confusingly at spawn.
//!
//! Single test: owns PATH and TERHUB_TMUX_SOCKET for this binary.
use std::os::unix::fs::PermissionsExt;

#[test]
fn find_tmux_skips_non_executable_shadow() {
    use terhub::tmux::Tmux;
    let base =
        std::env::temp_dir().join(format!("terhub-tmux-path-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let shadow = base.join("shadow");
    std::fs::create_dir_all(&shadow).unwrap();
    // A regular file named tmux, earlier on PATH, but not executable.
    std::fs::write(shadow.join("tmux"), "this is not tmux\n").unwrap();
    std::fs::set_permissions(
        shadow.join("tmux"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    // The real tmux, resolved before PATH is narrowed.
    let real_dir = std::env::var_os("PATH")
        .map(|p| {
            std::env::split_paths(&p)
                .find(|d| {
                    d.join("tmux").is_file()
                        && std::fs::metadata(d.join("tmux"))
                            .map(|m| m.permissions().mode() & 0o111 != 0)
                            .unwrap_or(false)
                })
                .expect("real tmux required on PATH for this suite (CI installs it)")
        })
        .expect("PATH must be set");
    // SAFETY: single test in this binary.
    unsafe {
        std::env::set_var(
            "TERHUB_TMUX_SOCKET",
            format!("terhub-test-path-{}", std::process::id()),
        );
        std::env::set_var("PATH", &shadow);
    }

    // Shadow only: selecting it would fail at spawn with permission
    // denied, so it must read as absent instead.
    assert!(
        !Tmux::available(),
        "non-executable tmux shadow must not count as available"
    );
    assert!(
        Tmux::new().is_err(),
        "non-executable tmux shadow must not construct"
    );

    // Shadow first, real tmux later: the lookup must skip past the
    // shadow. A private socket with no server answers Ok(empty) when
    // the real binary runs; the shadow would fail at spawn instead.
    // SAFETY: same single-test ownership.
    unsafe {
        std::env::set_var(
            "PATH",
            format!("{}:{}", shadow.display(), real_dir.display()),
        );
    }
    let sessions = Tmux::new()
        .expect("real tmux past the shadow must construct")
        .list_sessions()
        .expect("real tmux past the shadow must run");
    assert!(sessions.is_empty());
    let _ = std::fs::remove_dir_all(&base);
}
