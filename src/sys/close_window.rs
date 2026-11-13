// Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use objc2::rc::autoreleasepool;
use objc2_app_kit::NSWorkspace;
use objc2_application_services::AXIsProcessTrusted;
use wm_client::WmMachClient;
use wm_protocol::{ReactorCommand, WindowId, WmCommand, WmRequest, WmResponse, WorkspaceData};

use super::axuielement::AXUIElement;

const QUERY_TIMEOUT: Duration = Duration::from_millis(200);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(1);

struct CloseFocusPlan {
    workspace: String,
    source: WindowId,
    target: WindowId,
    target_server_id: u32,
}

#[derive(Debug, PartialEq)]
enum Observation {
    Waiting,
    Focus,
    Cancel,
}

impl CloseFocusPlan {
    fn new(workspaces: &[WorkspaceData], pid: i32, server_id: u32) -> Option<Self> {
        let workspace = workspaces.iter().find(|workspace| {
            workspace.is_active
                && workspace.windows.iter().any(|window| {
                    window.is_focused
                        && window.id.pid == pid
                        && window.window_server_id == Some(server_id)
                })
        })?;
        if workspace.layout_mode != "scrolling" {
            return None;
        }
        let mut tiled: Vec<_> = workspace
            .windows
            .iter()
            .filter_map(|window| {
                (!window.is_floating)
                    .then_some(window.layout_position)
                    .flatten()
                    .map(|position| ((position.column, position.row), window))
            })
            .collect();
        tiled.sort_by_key(|(position, _)| *position);
        let source = tiled.iter().position(|(_, window)| {
            window.id.pid == pid && window.window_server_id == Some(server_id)
        })?;
        let neighbor = if source + 1 < tiled.len() {
            source + 1
        } else {
            source.checked_sub(1)?
        };
        let target = tiled[neighbor].1;
        Some(Self {
            workspace: workspace.id.clone(),
            source: tiled[source].1.id,
            target: target.id,
            target_server_id: target.window_server_id?,
        })
    }

    fn observe(&self, workspaces: &[WorkspaceData]) -> Observation {
        let Some(workspace) = workspaces.iter().find(|workspace| {
            workspace.id == self.workspace
                && workspace.is_active
                && workspace.layout_mode == "scrolling"
        }) else {
            return Observation::Cancel;
        };
        if !workspace.windows.iter().any(|window| {
            window.id == self.target
                && window.window_server_id == Some(self.target_server_id)
                && !window.is_floating
        }) {
            return Observation::Cancel;
        }
        if workspace.windows.iter().any(|window| window.id == self.source) {
            return Observation::Waiting;
        }
        if workspaces
            .iter()
            .any(|workspace| workspace.windows.iter().any(|window| window.id == self.source))
        {
            return Observation::Cancel;
        }
        Observation::Focus
    }
}

fn workspaces(client: &WmMachClient, timeout: Duration) -> Option<Vec<WorkspaceData>> {
    client
        .send_typed_request_with_timeout(&WmRequest::GetWorkspaces { space_id: None }, timeout)
        .ok()?
        .into_result()
        .ok()
}

fn focus_after_close(client: &WmMachClient, plan: CloseFocusPlan) {
    let deadline = Instant::now() + CLOSE_TIMEOUT;
    while Instant::now() < deadline {
        let timeout = QUERY_TIMEOUT.min(deadline.saturating_duration_since(Instant::now()));
        let Some(workspaces) = workspaces(client, timeout) else {
            return;
        };
        match plan.observe(&workspaces) {
            Observation::Waiting => {
                thread::sleep(
                    Duration::from_millis(25)
                        .min(deadline.saturating_duration_since(Instant::now())),
                );
            }
            Observation::Cancel => return,
            Observation::Focus => {
                if Instant::now() < deadline {
                    let request = WmRequest::ExecuteCommand {
                        command: WmCommand::Reactor(ReactorCommand::FocusWindow {
                            window_id: plan.target,
                            window_server_id: Some(plan.target_server_id),
                        }),
                    };
                    let _: Result<WmResponse, _> = client.send_typed_request_with_timeout(
                        &request,
                        QUERY_TIMEOUT.min(deadline.saturating_duration_since(Instant::now())),
                    );
                }
                return;
            }
        }
    }
}

pub fn close_focused_window() -> anyhow::Result<()> {
    autoreleasepool(|_| {
        if !unsafe { AXIsProcessTrusted() } {
            bail!("Accessibility permission is required");
        }
        let application = NSWorkspace::sharedWorkspace()
            .frontmostApplication()
            .context("no application is focused")?;
        let pid = application.processIdentifier();
        let window = AXUIElement::application(pid)
            .focused_window()
            .context("cannot read the focused window")?;
        let client = WmMachClient;
        let plan = window.window_server_id().ok().and_then(|server_id| {
            CloseFocusPlan::new(&workspaces(&client, QUERY_TIMEOUT)?, pid, server_id)
        });
        window.close_window().context("cannot press the window close button")?;
        if let Some(plan) = plan {
            focus_after_close(&client, plan);
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wm_protocol::{WindowData, WindowLayoutPosition};

    use super::*;

    fn tile(id: u32, column: usize, row: usize, focused: bool) -> WindowData {
        serde_json::from_value(json!({
            "id": { "pid": 123, "idx": id },
            "title": "fixture", "frame": { "origin": { "x": 0, "y": 0 }, "size": { "width": 100, "height": 100 } },
            "is_floating": false, "is_focused": focused, "window_server_id": id,
            "bundle_id": null, "app_name": null,
            "layout_position": { "column": column, "row": row }
        })).unwrap()
    }

    fn workspace(windows: Vec<WindowData>) -> WorkspaceData {
        WorkspaceData {
            id: "workspace-1".into(),
            index: 0,
            name: "01".into(),
            layout_mode: "scrolling".into(),
            is_active: true,
            display_uuid: None,
            is_focused: true,
            window_count: windows.len(),
            windows,
        }
    }

    #[test]
    fn chooses_logical_next_then_previous_neighbor() {
        let windows = vec![
            tile(30, 2, 0, false),
            tile(10, 0, 0, false),
            tile(20, 1, 0, true),
        ];
        let plan = CloseFocusPlan::new(&[workspace(windows)], 123, 20).unwrap();
        assert_eq!(plan.target, WindowId { pid: 123, idx: 30 });

        let first = workspace(vec![tile(10, 0, 0, true), tile(20, 1, 0, false)]);
        assert_eq!(CloseFocusPlan::new(&[first], 123, 10).unwrap().target.idx, 20);
        let last = workspace(vec![tile(10, 0, 0, false), tile(20, 1, 0, true)]);
        assert_eq!(CloseFocusPlan::new(&[last], 123, 20).unwrap().target.idx, 10);
        let stacked = workspace(vec![
            tile(30, 2, 0, false),
            tile(20, 0, 1, true),
            tile(10, 0, 0, false),
        ]);
        assert_eq!(CloseFocusPlan::new(&[stacked], 123, 20).unwrap().target.idx, 30);
    }

    #[test]
    fn waits_for_confirmed_removal_and_cancels_when_context_changes() {
        let before = workspace(vec![
            tile(10, 0, 0, false),
            tile(20, 1, 0, true),
            tile(30, 2, 0, false),
        ]);
        let plan = CloseFocusPlan::new(&[before.clone()], 123, 20).unwrap();
        assert_eq!(plan.observe(&[before]), Observation::Waiting);
        let after = workspace(vec![tile(10, 0, 0, false), tile(30, 2, 0, false)]);
        assert_eq!(plan.observe(&[after.clone()]), Observation::Focus);
        let mut inactive = after.clone();
        inactive.is_active = false;
        assert_eq!(plan.observe(&[inactive]), Observation::Cancel);
        let mut moved = workspace(vec![tile(20, 0, 0, false)]);
        moved.id = "workspace-2".into();
        moved.is_active = false;
        assert_eq!(plan.observe(&[after.clone(), moved]), Observation::Cancel);
        assert_eq!(
            plan.observe(&[workspace(vec![tile(10, 0, 0, false)])]),
            Observation::Cancel
        );
        let mut floating = after.clone();
        floating.windows[1].is_floating = true;
        assert_eq!(plan.observe(&[floating]), Observation::Cancel);
        let mut replaced = after.clone();
        replaced.windows[1].window_server_id = Some(300);
        assert_eq!(plan.observe(&[replaced]), Observation::Cancel);
        let mut traditional = after;
        traditional.layout_mode = "bsp".into();
        assert_eq!(plan.observe(&[traditional]), Observation::Cancel);
    }

    #[test]
    fn native_close_without_retargeting_for_unmanaged_contexts() {
        assert!(CloseFocusPlan::new(&[workspace(vec![tile(20, 0, 0, true)])], 123, 20).is_none());
        let before = workspace(vec![tile(10, 0, 0, false), tile(20, 1, 0, true)]);
        assert!(CloseFocusPlan::new(&[before.clone()], 999, 20).is_none());
        assert!(CloseFocusPlan::new(&[before.clone()], 123, 999).is_none());
        let mut floating = before.clone();
        floating.windows[1].is_floating = true;
        assert!(CloseFocusPlan::new(&[floating], 123, 20).is_none());
        let mut inactive = before.clone();
        inactive.is_active = false;
        assert!(CloseFocusPlan::new(&[inactive], 123, 20).is_none());
        let mut traditional = before.clone();
        traditional.layout_mode = "bsp".into();
        assert!(CloseFocusPlan::new(&[traditional], 123, 20).is_none());
        let mut no_position = before;
        no_position.windows[1].layout_position = None;
        assert!(CloseFocusPlan::new(&[no_position], 123, 20).is_none());
    }

    #[test]
    fn orders_stacked_rows_before_later_columns() {
        let mut next_row = tile(20, 0, 0, false);
        next_row.layout_position = Some(WindowLayoutPosition { column: 0, row: 1 });
        let before = workspace(vec![tile(30, 1, 0, false), next_row, tile(10, 0, 0, true)]);
        assert_eq!(CloseFocusPlan::new(&[before], 123, 10).unwrap().target.idx, 20);
    }
}
