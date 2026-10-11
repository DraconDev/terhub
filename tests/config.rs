

/// REGRESSION (auditor finding): the surgical `[ui]` writer once gave
/// the `saved_filters` and `sort` arms the same bookkeeping slot, so a
/// patch carrying both keys silently dropped one of the two lines.
/// Exercise every starting layout.
#[test]
fn combined_saved_filters_and_sort_patch_writes_both_lines() {
    use terhub::config::SavedFilter;

    const SF: &str = "saved_filters = [{name = \"work\", query = \"vim\"}]";

    fn persist(tag: &str, patch: &terhub::config::ViewChanges, start: &str) -> String {
        let path = std::env::temp_dir()
            .join(format!("terhub-combined-{tag}-{}.toml", std::process::id()));
        std::fs::write(&path, start).unwrap();
        terhub::config::persist_ui_settings_at(&path, patch).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).ok();
        text
    }

    // `ViewChanges` is the change flag set; `ui_persist_changes` folds it
    // into the `UiPersist` patch the surgical writer consumes.
    let changes = terhub::config::ViewChanges {
        saved_filters: true,
        sort: true,
        ..Default::default()
    };
    let cfg = AppConfig {
        saved_filters: vec![SavedFilter {
            name: "work".to_string(),
            query: "vim".to_string(),
        }],
        sort: Some("Idle".to_string()),
        ..AppConfig::default()
    };
    let patch = cfg.ui_persist_changes(&changes);

    // A: the file already has a `sort` line and no `saved_filters`.
    let a = persist(
        "a",
        &patch,
        "[ui]\ntheme = \"dark\"\nfilter = \"!zsh\"\nsort = \"State\"\n",
    );
    assert!(a.contains(SF), "A: saved_filters line lost:\n{a}");
    assert!(a.contains("sort = \"Idle\""), "A: sort line lost:\n{a}");

    // B: the mirror - a `saved_filters` line and no `sort`.
    let b = persist(
        "b",
        &patch,
        "[ui]\ntheme = \"dark\"\nsaved_filters = [{name = \"old\", query = \"old\"}]\n",
    );
    assert!(b.contains(SF), "B: saved_filters line lost:\n{b}");
    assert!(b.contains("sort = \"Idle\""), "B: sort line lost:\n{b}");

    // C: neither line exists - the append pass must add both.
    let c = persist("c", &patch, "[ui]\ntheme = \"dark\"\n");
    assert!(
        c.contains(SF) && c.contains("sort = \"Idle\""),
        "C: append pass lost a key:\n{c}"
    );

    // D: both already exist with stale values - both must be rewritten.
    let d = persist(
        "d",
        &patch,
        "[ui]\nsort = \"Pane\"\nsaved_filters = [{name = \"old\", query = \"old\"}]\n",
    );
    assert!(
        d.contains(SF) && d.contains("sort = \"Idle\""),
        "D: an in-place rewrite lost a key:\n{d}"
    );

    // Every layout reparses with both halves intact.
    for (tag, text) in [("a", &a), ("b", &b), ("c", &c), ("d", &d)] {
        let path = std::env::temp_dir()
            .join(format!("terhub-combined-parse-{tag}-{}.toml", std::process::id()));
        std::fs::write(&path, text).unwrap();
        let (cfg, problems) = terhub::config::load_config_report(Some(path.clone()));
        assert!(problems.is_empty(), "{tag}: parse problems {problems:?}");
        assert_eq!(cfg.saved_filters.len(), 1, "{tag}: saved filters gone");
        assert_eq!(cfg.saved_filters[0].query, "vim", "{tag}: query wrong");
        std::fs::remove_file(&path).ok();
    }
}
