// Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
use std::process::Command;

use wm::common::config::{Config, config_file, restore_file};
use wm::layout_engine::LayoutEngine;
use wm::model::window_store::WindowStore;

#[test]
fn default_paths_use_the_wm_namespace() {
    assert!(config_file().ends_with(".config/wm/config.toml"));
    assert!(restore_file().ends_with(".wm/layout.ron"));
}

#[test]
fn validate_checks_only_the_saved_layout() {
    let home = tempfile::tempdir().unwrap();
    let config_path = home.path().join("config.toml");
    std::fs::write(&config_path, "invalid config").unwrap();
    let snapshot = home.path().join(".wm/layout.ron");
    let validate = || {
        Command::new(env!("CARGO_BIN_EXE_wm"))
            .env("HOME", home.path())
            .args(["--validate", "--config"])
            .arg(&config_path)
            .output()
            .unwrap()
    };

    let missing = validate();
    assert_eq!(missing.status.code(), Some(1), "{missing:?}");
    assert!(String::from_utf8_lossy(&missing.stderr).contains(&*snapshot.to_string_lossy()));
    assert!(!snapshot.exists());

    std::fs::create_dir(snapshot.parent().unwrap()).unwrap();
    std::fs::write(&snapshot, "invalid layout").unwrap();
    let invalid = validate();
    assert_eq!(invalid.status.code(), Some(1), "{invalid:?}");
    assert!(String::from_utf8_lossy(&invalid.stderr).contains(&*snapshot.to_string_lossy()));
    assert_eq!(std::fs::read_to_string(&snapshot).unwrap(), "invalid layout");

    let config = Config::default();
    let layout = LayoutEngine::new(&config.virtual_workspaces, &config.settings.layout, None);
    layout
        .save_current_layout(snapshot.clone(), &WindowStore::default(), None)
        .unwrap();
    let valid = validate();
    assert!(valid.status.success(), "{valid:?}");
    assert!(String::from_utf8_lossy(&valid.stdout).contains(&*snapshot.to_string_lossy()));
}
