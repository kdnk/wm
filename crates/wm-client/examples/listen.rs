// Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
use std::error::Error;

use wm_client::{EventKind, WmMachClient};

fn main() -> Result<(), Box<dyn Error>> {
    // Pass a specific event on the command line, or listen to every event by default.
    let event = match std::env::args().nth(1).as_deref() {
        Some("workspace_changed") => EventKind::WorkspaceChanged,
        Some("windows_changed") => EventKind::WindowsChanged,
        Some("window_title_changed") => EventKind::WindowTitleChanged,
        Some("focused_window_changed") => EventKind::FocusedWindowChanged,
        Some("stacks_changed") => EventKind::StacksChanged,
        Some("layout_changed") => EventKind::LayoutChanged,
        Some("selection_changed") => EventKind::SelectionChanged,
        Some("*") | None => EventKind::All,
        Some(other) => return Err(format!("unknown event kind: {other}").into()),
    };
    let client = WmMachClient::connect()?;
    let subscription = client.subscribe(event)?;

    eprintln!("Listening for Wm event '{event}'. Press Ctrl-C to stop.");
    loop {
        let event = subscription.recv_event()?;
        println!("{}", serde_json::to_string_pretty(&event)?);
    }
}
