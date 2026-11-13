// Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
use std::path::Path;

use anyhow::{Context, ensure};
use wm::common::config::Config;

fn main() -> anyhow::Result<()> {
    let mut arguments = std::env::args_os().skip(1);
    let path = arguments.next().context("usage: config-check CONFIG.toml")?;
    ensure!(arguments.next().is_none(), "usage: config-check CONFIG.toml");
    let config = Config::read(Path::new(&path))?;
    let issues = config.validate();
    ensure!(
        issues.is_empty(),
        "Invalid Wm configuration:\n{}",
        issues.join("\n")
    );
    println!("PASS: complete Wm configuration is valid");
    Ok(())
}
