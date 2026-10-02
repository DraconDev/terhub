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
