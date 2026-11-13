// Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
use wm::common::config::{config_file, restore_file};

#[test]
fn default_paths_use_the_wm_namespace() {
    assert!(config_file().ends_with(".config/wm/config.toml"));
    assert!(restore_file().ends_with(".wm/layout.ron"));
}
