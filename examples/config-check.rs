use std::path::Path;

use anyhow::{Context, ensure};
use rift_wm::common::config::Config;

fn main() -> anyhow::Result<()> {
    let mut arguments = std::env::args_os().skip(1);
    let path = arguments.next().context("usage: config-check CONFIG.toml")?;
    ensure!(arguments.next().is_none(), "usage: config-check CONFIG.toml");
    let config = Config::read(Path::new(&path))?;
    let issues = config.validate();
    ensure!(
        issues.is_empty(),
        "Invalid Rift configuration:\n{}",
        issues.join("\n")
    );
    println!("PASS: complete Rift configuration is valid");
    Ok(())
}
