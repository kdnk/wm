// Modified by kdnk for wm on 2026-11-26. Original attribution is preserved in LICENSE.
use clap::{Args, Subcommand};
use wm_protocol::WmRequest;

use super::parse_window_id;

#[derive(Args)]
pub(super) struct QueryArgs {
    #[command(subcommand)]
    command: QueryCommands,
}

impl QueryArgs {
    pub(super) fn into_request(self) -> Result<WmRequest, String> {
        match self.command {
            QueryCommands::Workspaces {
                scope: SpaceScopeArgs { space_id, display },
            } => match display {
                Some(display_uuid) => Ok(WmRequest::GetWorkspacesForDisplay { display_uuid }),
                None => Ok(WmRequest::GetWorkspaces { space_id }),
            },
            QueryCommands::Windows {
                scope: SpaceScopeArgs { space_id, display },
            } => match display {
                Some(display_uuid) => Ok(WmRequest::GetWindowsForDisplay { display_uuid }),
                None => Ok(WmRequest::GetWindows { space_id }),
            },
            QueryCommands::Displays => Ok(WmRequest::GetDisplays),
            QueryCommands::Window { window_id } => {
                let window_id = parse_window_id(&window_id)?;
                Ok(WmRequest::GetWindowInfo { window_id })
            }
            QueryCommands::Applications => Ok(WmRequest::GetApplications),
            QueryCommands::Layout {
                scope: SpaceScopeArgs { space_id, display },
                workspace_id,
            } => match display {
                Some(display_uuid) => {
                    Ok(WmRequest::GetLayoutStateForDisplay { display_uuid, workspace_id })
                }
                None => Ok(WmRequest::GetLayoutState { space_id, workspace_id }),
            },
            QueryCommands::WorkspaceLayout {
                scope: SpaceScopeArgs { space_id, display },
                workspace_id,
            } => match display {
                Some(display_uuid) => {
                    Ok(WmRequest::GetWorkspaceLayoutsForDisplay { display_uuid, workspace_id })
                }
                None => Ok(WmRequest::GetWorkspaceLayouts { space_id, workspace_id }),
            },
            QueryCommands::BindingMode => Ok(WmRequest::GetBindingMode),
            QueryCommands::Metrics => Ok(WmRequest::GetMetrics),
        }
    }
}

#[derive(Args)]
#[group(multiple = false)]
struct SpaceScopeArgs {
    /// macOS space ID; defaults to the active display space
    #[arg(long)]
    space_id: Option<u64>,
    /// Display UUID; uses the display's current macOS space
    #[arg(long, value_name = "UUID")]
    display: Option<String>,
}

#[derive(Subcommand)]
enum QueryCommands {
    /// List virtual workspaces (optionally for a specific MacOS space)
    Workspaces {
        #[command(flatten)]
        scope: SpaceScopeArgs,
    },
    /// List windows (optionally filtered by space)
    Windows {
        #[command(flatten)]
        scope: SpaceScopeArgs,
    },
    /// List connected displays
    Displays,
    /// Get information about a specific window
    Window { window_id: String },
    /// List running applications
    Applications,
    /// Get layout state and normalized container tree for a space
    Layout {
        #[command(flatten)]
        scope: SpaceScopeArgs,
        /// Virtual workspace index; defaults to the active workspace
        #[arg(long)]
        workspace_id: Option<usize>,
    },
    /// Get workspace layout-engine mode(s)
    WorkspaceLayout {
        #[command(flatten)]
        scope: SpaceScopeArgs,
        #[arg(long)]
        workspace_id: Option<usize>,
    },
    /// Get the active binding mode name
    BindingMode,
    /// Get performance metrics
    Metrics,
}
