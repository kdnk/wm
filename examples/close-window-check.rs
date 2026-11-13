// Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
use anyhow::{Context, ensure};
use wm::sys::axuielement::AXUIElement;

fn main() -> anyhow::Result<()> {
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    ensure!(
        arguments.len() == 2,
        "usage: close-window-check FIXTURE_PID UNIQUE_TITLE"
    );
    let pid = arguments[0].parse::<i32>().context("invalid fixture PID")?;
    ensure!(pid > 0, "fixture PID must be positive");
    let window = AXUIElement::application(pid).focused_window()?;
    ensure!(window.title()? == arguments[1], "fixture title does not match");
    window.close_window().context("cannot press the fixture close button")
}
