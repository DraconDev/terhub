use std::path::PathBuf;
use terhub::config::{AppConfig, default_config_text, load_config};

#[test]
fn missing_file_yields_defaults() {
    let cfg = load_config(Some(PathBuf::from("/nonexistent-terhub-config.toml")));
    assert_eq!(cfg.key("new"), "n");
    assert_eq!(cfg.key("quit"), "q");
    assert_eq!(cfg.theme, "dark");
}

#[test]
fn default_text_roundtrips() {
    let dir = std::env::temp_dir().join(format!("terhub-cfg-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.toml");
    std::fs::write(&path, default_config_text()).unwrap();
    let cfg = load_config(Some(path.clone()));
    let fresh = AppConfig::default();
    assert_eq!(cfg.key("kill"), fresh.key("kill"));
    assert_eq!(cfg.theme, fresh.theme);
    std::fs::remove_file(path).ok();
}

#[test]
fn corrupt_file_yields_defaults() {
    let dir = std::env::temp_dir().join(format!("terhub-cfg-bad-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.toml");
    std::fs::write(&path, "[[[ not toml").unwrap();
    let cfg = load_config(Some(path.clone()));
    assert_eq!(cfg.key("new"), "n");
    std::fs::remove_file(path).ok();
}

#[test]
fn style_section_parses_and_validates() {
    use terhub::config::load_config_report;
    let dir = std::env::temp_dir().join(format!("terhub-cfg-style-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.toml");
    std::fs::write(
        &path,
        "[style]\ncolor_agents = false\nagent_palette = [\"red\", \"nope\"]\nidle_warn_style = \"bold red\"\n[style.agents]\nPi = \"cyan\"\n",
    )
    .unwrap();
    let (cfg, problems) = load_config_report(Some(path.clone()));
    assert!(!cfg.color_agents);
    assert_eq!(cfg.agent_palette, vec!["red".to_string(), "nope".to_string()]);
    assert_eq!(cfg.agent_styles.get("pi").map(String::as_str), Some("cyan"));
    assert_eq!(cfg.idle_warn_style, "bold red");
    assert!(
        problems.iter().any(|p| p.contains("agent_palette[1]")),
        "missing problem: {problems:?}"
    );
    std::fs::remove_file(path).ok();
}
