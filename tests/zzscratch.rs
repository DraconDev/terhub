//! TEMPORARY scratch verification (deleted after the run).
use std::path::Path;

#[test]
fn claude_status_only_checks_matcher_zero() {
    // User config: a first matcher with no "hooks" key at all, plus a
    // dedicated terhub matcher further down the array.
    let dir = std::env::temp_dir().join(format!("terhub-scratch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("settings.json");
    std::fs::write(
        &path,
        r#"{"hooks": {"Stop": [{"matcher": "Foo"}]}, "other": 1}"#,
    )
    .unwrap();
    let report = terhub::hooks::claude_install(&path).unwrap();
    println!("install report: {report:?}");
    let text = std::fs::read_to_string(&path).unwrap();
    println!("--- after install ---\n{text}");
    let status = terhub::hooks::claude_status(&path);
    println!("status events: {status:?}");
    // Second run must be a no-op.
    let report2 = terhub::hooks::claude_install(&path).unwrap();
    println!("second install report: {report2:?}");
    let text2 = std::fs::read_to_string(&path).unwrap();
    println!(
        "marker count after 2nd install: {}",
        text2.matches(terhub::hooks::MARKER).count()
    );
    assert!(false, "inspect output above");
}

#[test]
fn prune_drops_corrupt_line_forever_and_is_not_atomic() {
    let dir = std::env::temp_dir().join(format!("terhub-scratch2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("activity.jsonl");
    let good = terhub::history::Entry::prompt(100, "%1", "pi", "keep me");
    append(&path, &good);
    // A torn line (crash mid-append).
    append_raw(&path, "{\"t\":200,\"kind\":\"prompt\",\"pane\":\"%2\",\"summ");
    println!("--- before prune ---\n{}", std::fs::read_to_string(&path).unwrap());
    let kept = terhub::history::prune(&path, 10_000, 365);
    println!("kept: {kept}");
    println!("--- after prune ---\n{}", std::fs::read_to_string(&path).unwrap());
    println!(
        "entries readable now: {}",
        terhub::history::read_entries(&path).len()
    );
    assert!(false, "inspect output above");
}

fn append(path: &Path, e: &terhub::history::Entry) {
    terhub::history::append_entry(path, e).unwrap();
}
fn append_raw(path: &Path, line: &str) {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    writeln!(f, "{line}").unwrap();
}
