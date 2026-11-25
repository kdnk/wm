// Modified by kdnk for wm on 2026-11-26. Original attribution is preserved in LICENSE.
use std::path::PathBuf;

use clap::{Args, Subcommand, ValueEnum};
use serde_json::Value;
use wm_protocol::{
    AnimationEasing, ConfigCommand, Direction, DisplaySelector, LayoutCommand, MetricsCommand,
    ReactorCommand, ResizeOrientation, RestoreScope, RestoreSource, WmCommand, WmRequest,
    WorkspaceSelector,
};

use super::parse_window_id;
use crate::common::config::restore_file;
use crate::layout_engine::LayoutEngine;

#[derive(Args)]
pub(super) struct ExecuteArgs {
    #[command(subcommand)]
    command: ExecuteCommands,
}

impl ExecuteArgs {
    pub(super) fn into_request(self) -> Result<WmRequest, String> {
        let command = match self.command {
            ExecuteCommands::BindingMode { mode } => {
                WmCommand::Reactor(ReactorCommand::BindingMode(mode))
            }
            ExecuteCommands::Window { window_cmd } => map_window_command(window_cmd)?,
            ExecuteCommands::Workspace { workspace_cmd } => {
                WmCommand::Layout(map_workspace_command(workspace_cmd))
            }
            ExecuteCommands::Layout { layout_cmd } => {
                WmCommand::Layout(map_layout_command(layout_cmd)?)
            }
            ExecuteCommands::Config { config_cmd } => return map_config_request(config_cmd),
            ExecuteCommands::MissionControl { mission_cmd } => {
                WmCommand::Reactor(map_mission_control_command(mission_cmd))
            }
            ExecuteCommands::Display { display_cmd } => {
                WmCommand::Reactor(map_display_command(display_cmd)?)
            }
            ExecuteCommands::Space { space_cmd } => {
                WmCommand::Reactor(map_space_command(space_cmd)?)
            }
            ExecuteCommands::SaveAndExit => WmCommand::Reactor(ReactorCommand::SaveAndExit),
            ExecuteCommands::SaveLayout { file } => {
                WmCommand::Reactor(ReactorCommand::SaveLayout {
                    path: wire_path(file.into_path()?)?,
                })
            }
            ExecuteCommands::LoadLayout { file, scope } => {
                let source = if file.master {
                    RestoreSource::CurrentSpace
                } else {
                    RestoreSource::SavedActiveSpace
                };
                let path = file.into_path()?;
                LayoutEngine::validate_file(path.clone()).map_err(|error| {
                    format!("could not load layout file at {}: {error}", path.display())
                })?;
                let scope = match scope {
                    CliRestoreScope::Workspace => RestoreScope::Workspace,
                    CliRestoreScope::Space => RestoreScope::Space,
                    CliRestoreScope::Displays => RestoreScope::Displays,
                };
                WmCommand::Reactor(ReactorCommand::RestoreLayout {
                    path: wire_path(path)?,
                    scope,
                    source,
                })
            }
            ExecuteCommands::Debug => WmCommand::Reactor(ReactorCommand::Debug),
            ExecuteCommands::Serialize => WmCommand::Reactor(ReactorCommand::Serialize),
            #[allow(deprecated)]
            ExecuteCommands::ToggleSpaceActivated => {
                eprintln!("this command is deprecated, use wm execute space toggle-activated");
                WmCommand::Reactor(ReactorCommand::ToggleSpaceActivated)
            }
            ExecuteCommands::ShowTiming => WmCommand::Metrics(MetricsCommand::ShowTiming),
        };
        Ok(WmRequest::ExecuteCommand { command })
    }
}

#[derive(Subcommand)]
enum ExecuteCommands {
    /// Switch to a configured binding mode (use default to reset)
    BindingMode { mode: String },
    /// Window management commands
    Window {
        #[command(subcommand)]
        window_cmd: WindowCommands,
    },
    /// Virtual workspace commands
    Workspace {
        #[command(subcommand)]
        workspace_cmd: WorkspaceCommands,
    },
    /// Layout commands
    Layout {
        #[command(subcommand)]
        layout_cmd: LayoutCommands,
    },
    /// Configuration management commands
    Config {
        #[command(subcommand)]
        config_cmd: ConfigCommands,
    },
    /// Mission control commands
    MissionControl {
        #[command(subcommand)]
        mission_cmd: MissionControlCommands,
    },
    /// Display/mouse commands
    Display {
        #[command(subcommand)]
        display_cmd: DisplayCommands,
    },
    /// macOS space commands (Mission Control spaces, not virtual workspaces)
    Space {
        #[command(subcommand)]
        space_cmd: SpaceCommands,
    },
    /// Save the master file and exit Wm
    SaveAndExit,
    /// Save Wm's current layout state without exiting
    ///
    /// Use --master instead of PATH to update Wm's master file.
    SaveLayout {
        #[command(flatten)]
        file: LayoutFileSelection,
    },
    /// Restore a layout file to the current workspace, macOS Space, or every display
    ///
    /// Use --master instead of PATH to load Wm's master file.
    LoadLayout {
        #[command(flatten)]
        file: LayoutFileSelection,
        /// Restore one workspace, the current macOS Space, or the current Space of every display.
        #[arg(long, value_enum, default_value_t = CliRestoreScope::Workspace)]
        scope: CliRestoreScope,
    },
    /// Print layout tree debugging output in the running wm instance
    Debug,
    /// Serialize and print runtime state
    Serialize,
    /// this command is deprecated, use `wm execute space toggle-activated`
    #[deprecated]
    ToggleSpaceActivated,
    /// Show timing metrics
    ShowTiming,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliRestoreScope {
    Workspace,
    Space,
    Displays,
}

#[derive(Debug, Clone, Args)]
#[group(required = true, multiple = false)]
struct LayoutFileSelection {
    /// Layout file path.
    #[arg(value_name = "PATH")]
    path: Option<PathBuf>,
    /// Use Wm's master file (~/.wm/layout.ron).
    #[arg(long)]
    master: bool,
}

impl LayoutFileSelection {
    fn into_path(self) -> Result<PathBuf, String> {
        if self.master {
            return Ok(restore_file());
        }
        let path = self.path.expect("clap requires either PATH or --master");
        if path.is_absolute() {
            Ok(path)
        } else {
            std::env::current_dir()
                .map(|current_dir| current_dir.join(path))
                .map_err(|error| format!("could not resolve layout path: {error}"))
        }
    }
}

fn wire_path(path: PathBuf) -> Result<PathBuf, String> {
    if path.to_str().is_none() {
        return Err("path contains invalid UTF-8 characters".into());
    }
    Ok(path)
}

#[derive(Subcommand)]
enum WindowCommands {
    /// Focus the next window
    Next,
    /// Focus the previous window
    Prev,
    /// Focus a window by direction or by a specific window ID
    Focus {
        /// Direction to focus (left, right, up, down)
        direction: Option<String>,
        /// Wm window ID as JSON (`{"pid":123,"idx":456}`) or debug text
        #[arg(long, conflicts_with = "direction")]
        window_id: Option<String>,
        /// Optional macOS window server ID for the target window
        #[arg(long, requires = "window_id")]
        window_server_id: Option<String>,
    },
    /// Toggle fullscreen mode (fills the whole screen, ignores outer gaps)
    ToggleFullscreen,
    /// Toggle fullscreen within configured outer gaps (respects outer gaps / fills tiling area)
    ToggleFullscreenWithinGaps,
    /// Grow the current window size (increments by ~5%).
    ResizeGrow {
        /// Axis to resize; smart chooses the nearest applicable split.
        #[arg(long, value_enum, default_value_t = CliResizeOrientation::Horizontal)]
        orientation: CliResizeOrientation,
    },
    /// Shrink the current window size (decrements by ~5%).
    ResizeShrink {
        /// Axis to resize; smart chooses the nearest applicable split.
        #[arg(long, value_enum, default_value_t = CliResizeOrientation::Horizontal)]
        orientation: CliResizeOrientation,
    },
    /// Resize the selected window by a fractional amount.
    /// - Pass a signed floating value: positive to grow, negative to shrink.
    /// - The value is a fraction of the current size (e.g. `0.05` = 5%).
    /// Examples:
    ///   wm execute window resize-by --amount 0.05    # grow by 5%
    ///   wm execute window resize-by --amount -0.10   # shrink by 10%
    ResizeBy {
        #[arg(long, allow_negative_numbers = true)]
        amount: f64,
    },
    /// Close a window as if Command-W was pressed
    Close {
        /// Optional window server ID; defaults to the focused window
        #[arg(long, visible_alias = "window-server-id")]
        window_id: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliResizeOrientation {
    Horizontal,
    Vertical,
    Smart,
}

impl From<CliResizeOrientation> for ResizeOrientation {
    fn from(value: CliResizeOrientation) -> Self {
        match value {
            CliResizeOrientation::Horizontal => Self::Horizontal,
            CliResizeOrientation::Vertical => Self::Vertical,
            CliResizeOrientation::Smart => Self::Smart,
        }
    }
}

fn map_window_command(cmd: WindowCommands) -> Result<WmCommand, String> {
    Ok(match cmd {
        WindowCommands::Next => WmCommand::Layout(LayoutCommand::NextWindow),
        WindowCommands::Prev => WmCommand::Layout(LayoutCommand::PrevWindow),
        WindowCommands::Focus {
            direction,
            window_id,
            window_server_id,
        } => match (direction, window_id) {
            (Some(direction), None) => {
                WmCommand::Layout(LayoutCommand::MoveFocus(parse_focus_direction(&direction)?))
            }
            (None, Some(window_id)) => WmCommand::Reactor(ReactorCommand::FocusWindow {
                window_id: parse_window_id(&window_id)?,
                window_server_id: window_server_id
                    .as_deref()
                    .map(parse_window_server_id)
                    .transpose()?,
            }),
            (None, None) => {
                return Err("window focus requires a direction or --window-id".to_string());
            }
            (Some(_), Some(_)) => {
                return Err(
                    "window focus accepts either a direction or --window-id, not both".to_string(),
                );
            }
        },
        WindowCommands::ToggleFullscreen => WmCommand::Layout(LayoutCommand::ToggleFullscreen),
        WindowCommands::ToggleFullscreenWithinGaps => {
            WmCommand::Layout(LayoutCommand::ToggleFullscreenWithinGaps)
        }
        WindowCommands::ResizeGrow { orientation } => {
            WmCommand::Layout(LayoutCommand::ResizeWindowGrow(orientation.into()))
        }
        WindowCommands::ResizeShrink { orientation } => {
            WmCommand::Layout(LayoutCommand::ResizeWindowShrink(orientation.into()))
        }
        WindowCommands::ResizeBy { amount } => {
            WmCommand::Layout(LayoutCommand::ResizeWindowBy { amount: finite_number(amount)? })
        }
        WindowCommands::Close { window_id } => WmCommand::Reactor(ReactorCommand::CloseWindow {
            window_server_id: window_id.as_deref().map(parse_window_server_id).transpose()?,
        }),
    })
}

fn parse_window_server_id(input: &str) -> Result<u32, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("window_server_id cannot be empty".to_string());
    }

    if trimmed.starts_with("0x") {
        u32::from_str_radix(trimmed.trim_start_matches("0x"), 16)
            .map_err(|_| format!("Invalid hexadecimal window server id: {}", trimmed))
    } else {
        trimmed.parse().map_err(|_| format!("Invalid window server id: {}", trimmed))
    }
}

#[derive(Subcommand)]
enum WorkspaceCommands {
    /// Switch to next workspace
    Next { skip_empty: Option<bool> },
    /// Switch to previous workspace
    Prev { skip_empty: Option<bool> },
    /// Switch to specific workspace
    Switch { workspace_id: usize },
    /// Move current window to workspace
    MoveWindow {
        workspace_id: usize,
        /// Switch to the destination workspace after moving the window.
        #[arg(long)]
        follow: bool,
        window_id: Option<u32>,
    },
    /// Create a new workspace
    Create,
    /// Switch to the last workspace
    Last,
}

fn map_workspace_command(cmd: WorkspaceCommands) -> LayoutCommand {
    match cmd {
        WorkspaceCommands::Next { skip_empty } => LayoutCommand::NextWorkspace(skip_empty),
        WorkspaceCommands::Prev { skip_empty } => LayoutCommand::PrevWorkspace(skip_empty),
        WorkspaceCommands::Switch { workspace_id } => {
            LayoutCommand::SwitchToWorkspace(workspace_id)
        }
        WorkspaceCommands::MoveWindow {
            workspace_id,
            follow,
            window_id,
        } => LayoutCommand::MoveWindowToWorkspace {
            workspace: WorkspaceSelector::Index(workspace_id),
            follow,
            window_id,
        },
        WorkspaceCommands::Create => LayoutCommand::CreateWorkspace,
        WorkspaceCommands::Last => LayoutCommand::SwitchToLastWorkspace,
    }
}

#[derive(Subcommand)]
enum LayoutCommands {
    /// Move selection up the tree
    Ascend,
    /// Move selection down the tree
    Descend,
    /// Move the selected node in a direction
    MoveNode { direction: String },
    /// Join the selected window with neighbor in a direction
    JoinWindow { direction: String },
    /// Join with a neighbor, or unjoin when the selected window is already joined
    ConsumeOrExpelWindow { direction: String },
    /// Toggle stacked state for the selected container
    ToggleStack,
    /// Unjoin previously joined windows
    Unjoin,
    /// Swap two windows by window id (`WindowId { pid: ..., idx: ... }`)
    SwapWindows { a: String, b: String },
    /// Scroll the strip by a normalized delta (scrolling layout only)
    ScrollStrip {
        #[arg(allow_negative_numbers = true)]
        delta: f64,
    },
    /// Snap the strip to the nearest column boundary (scrolling layout only)
    SnapStrip,
    /// Switch to the next preset column width (scrolling layout only).
    SwitchPresetColumnWidth {
        #[arg(long)]
        backwards: bool,
    },
    /// Toggle centering of the selected column in scrolling layout.
    /// If invoked again on the same selection, centering is removed.
    CenterSelection,
}

fn map_layout_command(cmd: LayoutCommands) -> Result<LayoutCommand, String> {
    Ok(match cmd {
        LayoutCommands::Ascend => LayoutCommand::Ascend,
        LayoutCommands::Descend => LayoutCommand::Descend,
        LayoutCommands::MoveNode { direction } => LayoutCommand::MoveNode(direction.into()),
        LayoutCommands::JoinWindow { direction } => LayoutCommand::JoinWindow(direction.into()),
        LayoutCommands::ConsumeOrExpelWindow { direction } => {
            LayoutCommand::ConsumeOrExpelWindow(direction.into())
        }
        LayoutCommands::ToggleStack => LayoutCommand::ToggleStack,
        LayoutCommands::Unjoin => LayoutCommand::UnjoinWindows,
        LayoutCommands::SwapWindows { a, b } => {
            LayoutCommand::SwapWindows(parse_window_id(&a)?, parse_window_id(&b)?)
        }
        LayoutCommands::ScrollStrip { delta } => {
            LayoutCommand::ScrollStrip { delta: finite_number(delta)? }
        }
        LayoutCommands::SnapStrip => LayoutCommand::SnapStrip,
        LayoutCommands::SwitchPresetColumnWidth { backwards } => {
            LayoutCommand::SwitchPresetColumnWidth { backwards }
        }
        LayoutCommands::CenterSelection => LayoutCommand::CenterSelection,
    })
}

#[derive(Subcommand)]
enum ConfigCommands {
    /// Update animation settings
    SetAnimate { value: String },
    /// Set the animation duration in seconds
    SetAnimationDuration { value: f64 },
    /// Set the animation frame rate
    SetAnimationFps { value: f64 },
    /// Set the animation easing curve
    SetAnimationEasing {
        /// Curve name, such as linear, ease_in_out, or ease_in_out_sine
        value: String,
    },

    /// Update mouse settings
    SetMouseFollowsFocus {
        #[arg(action = clap::ArgAction::Set)]
        value: bool,
    },
    /// Show or hide the pointer after focus changes
    SetMouseHidesOnFocus {
        #[arg(action = clap::ArgAction::Set)]
        value: bool,
    },
    /// Enable or disable focusing windows under the pointer
    SetFocusFollowsMouse {
        #[arg(action = clap::ArgAction::Set)]
        value: bool,
    },

    /// Set the outer gap on each screen edge
    SetOuterGaps {
        top: f64,
        left: f64,
        bottom: f64,
        right: f64,
    },
    /// Set the horizontal and vertical gaps between windows
    SetInnerGaps { horizontal: f64, vertical: f64 },

    /// Update workspace settings
    SetWorkspaceNames { names: Vec<String> },

    /// Set a configuration value by its dot-separated key
    ///
    /// Example: wm execute config set settings.animate true
    Set {
        /// Dot-separated key path (e.g. settings.animate or settings.layout.gaps.outer.top)
        key: String,
        /// Value should be valid JSON (true, 1, "string", {"a":1}), but if it's not valid JSON
        /// it will be treated as a string.
        value: String,
    },

    /// Get current config
    Get,

    /// Save current config to file
    Save,

    /// Reload config from file
    Reload,
}

fn map_config_request(cmd: ConfigCommands) -> Result<WmRequest, String> {
    let command = match cmd {
        ConfigCommands::SetAnimate { value } => {
            let bool_value = match value.to_lowercase().as_str() {
                "true" | "on" => true,
                "false" | "off" => false,
                _ => return Err(format!("Invalid boolean value: {}. Use true/false", value)),
            };
            ConfigCommand::SetAnimate(bool_value)
        }
        ConfigCommands::SetAnimationDuration { value } => {
            ConfigCommand::SetAnimationDuration(finite_number(value)?)
        }
        ConfigCommands::SetAnimationFps { value } => {
            ConfigCommand::SetAnimationFps(finite_number(value)?)
        }
        ConfigCommands::SetAnimationEasing { value } => {
            let easing = serde_json::from_value::<AnimationEasing>(Value::String(value))
                .map_err(|error| format!("Invalid animation easing: {error}"))?;
            ConfigCommand::SetAnimationEasing(easing)
        }
        ConfigCommands::SetMouseFollowsFocus { value } => {
            ConfigCommand::SetMouseFollowsFocus(value)
        }
        ConfigCommands::SetMouseHidesOnFocus { value } => {
            ConfigCommand::SetMouseHidesOnFocus(value)
        }
        ConfigCommands::SetFocusFollowsMouse { value } => {
            ConfigCommand::SetFocusFollowsMouse(value)
        }
        ConfigCommands::SetOuterGaps { top, left, bottom, right } => ConfigCommand::SetOuterGaps {
            top: finite_number(top)?,
            left: finite_number(left)?,
            bottom: finite_number(bottom)?,
            right: finite_number(right)?,
        },
        ConfigCommands::SetInnerGaps { horizontal, vertical } => ConfigCommand::SetInnerGaps {
            horizontal: finite_number(horizontal)?,
            vertical: finite_number(vertical)?,
        },
        ConfigCommands::SetWorkspaceNames { names } => ConfigCommand::SetWorkspaceNames(names),
        ConfigCommands::Set { key, value } => {
            let value = serde_json::from_str(&value).unwrap_or_else(|_| Value::String(value));
            ConfigCommand::Set { key, value }
        }
        ConfigCommands::Get => return Ok(WmRequest::GetConfig),
        ConfigCommands::Save => ConfigCommand::SaveConfig,
        ConfigCommands::Reload => ConfigCommand::ReloadConfig,
    };
    Ok(WmRequest::ExecuteCommand {
        command: WmCommand::Config(command),
    })
}

#[derive(Subcommand)]
enum MissionControlCommands {
    /// Show all workspaces in mission control
    ShowAll,
    /// Show current workspace in mission control
    ShowCurrent,
    /// Dismiss mission control
    Dismiss,
}

fn map_mission_control_command(cmd: MissionControlCommands) -> ReactorCommand {
    match cmd {
        MissionControlCommands::ShowAll => ReactorCommand::ShowMissionControlAll,
        MissionControlCommands::ShowCurrent => ReactorCommand::ShowMissionControlCurrent,
        MissionControlCommands::Dismiss => ReactorCommand::DismissMissionControl,
    }
}

#[derive(Subcommand)]
enum DisplayCommands {
    /// Focus a display by direction, index, or UUID.
    Focus {
        #[command(flatten)]
        display: DisplaySelectionArgs,
    },
    /// Move the mouse cursor to a display by direction, index, or UUID.
    MoveMouse {
        #[command(flatten)]
        display: DisplaySelectionArgs,
    },
    /// Move mouse cursor to a display by index (0-based)
    MoveMouseToIndex {
        /// Display index (0-based)
        index: usize,
    },
    /// Move mouse cursor to a display by UUID
    MoveMouseToUuid {
        /// Display UUID
        uuid: String,
    },
    /// Move a window to a display by direction, index, or UUID.
    MoveWindow {
        #[command(flatten)]
        display: DisplaySelectionArgs,
        /// Optional window id (window idx); defaults to the focused window if omitted.
        #[arg(long)]
        window_id: Option<u32>,
    },
    /// Move the active workspace to a display by direction, index, or UUID.
    MoveWorkspace {
        #[command(flatten)]
        display: DisplaySelectionArgs,
        /// Continue from the opposite edge for directional selectors.
        #[arg(long)]
        wrap_around: bool,
    },
}

#[derive(Args)]
#[group(required = true, multiple = false)]
struct DisplaySelectionArgs {
    /// Direction relative to the current display (left, right, up, down).
    #[arg(long)]
    direction: Option<String>,
    /// Display index (0-based).
    #[arg(long)]
    index: Option<usize>,
    /// Display UUID.
    #[arg(long)]
    uuid: Option<String>,
}

impl DisplaySelectionArgs {
    fn into_selector(self) -> Result<DisplaySelector, String> {
        if let Some(direction) = self.direction {
            Ok(DisplaySelector::Direction(parse_focus_direction(&direction)?))
        } else if let Some(index) = self.index {
            Ok(DisplaySelector::Index(index))
        } else if let Some(uuid) = self.uuid {
            Ok(DisplaySelector::Uuid(uuid))
        } else {
            Err(
                "display selection requires exactly one of --direction, --index, or --uuid"
                    .to_string(),
            )
        }
    }
}

fn map_display_command(cmd: DisplayCommands) -> Result<ReactorCommand, String> {
    Ok(match cmd {
        DisplayCommands::Focus { display } => {
            ReactorCommand::FocusDisplay(display.into_selector()?)
        }
        DisplayCommands::MoveMouse { display } => {
            ReactorCommand::MoveMouseToDisplay(display.into_selector()?)
        }
        DisplayCommands::MoveMouseToIndex { index } => {
            ReactorCommand::MoveMouseToDisplay(DisplaySelector::Index(index))
        }
        DisplayCommands::MoveMouseToUuid { uuid } => {
            ReactorCommand::MoveMouseToDisplay(DisplaySelector::Uuid(uuid))
        }
        DisplayCommands::MoveWindow { display, window_id } => ReactorCommand::MoveWindowToDisplay {
            selector: display.into_selector()?,
            window_id,
        },
        DisplayCommands::MoveWorkspace { display, wrap_around } => {
            ReactorCommand::MoveWorkspaceToDisplay {
                selector: display.into_selector()?,
                wrap_around,
            }
        }
    })
}

#[derive(Subcommand)]
enum SpaceCommands {
    /// Toggle whether wm manages the current macOS space
    ToggleActivated,
    /// Switch to an adjacent macOS space (Mission Control spaces, not virtual workspaces)
    Switch {
        /// Direction to switch (left, right, up, down)
        direction: String,
    },
}

fn map_space_command(cmd: SpaceCommands) -> Result<ReactorCommand, String> {
    Ok(match cmd {
        SpaceCommands::ToggleActivated => ReactorCommand::ToggleSpaceActivated,
        SpaceCommands::Switch { direction } => {
            ReactorCommand::SwitchSpace(parse_focus_direction(&direction)?)
        }
    })
}

fn parse_focus_direction(value: &str) -> Result<Direction, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "left" => Ok(Direction::Left),
        "right" => Ok(Direction::Right),
        "up" => Ok(Direction::Up),
        "down" => Ok(Direction::Down),
        other => Err(format!(
            "Invalid focus direction '{}'; must be left, right, up, or down",
            other
        )),
    }
}

fn finite_number(value: f64) -> Result<f64, String> {
    if !value.is_finite() {
        return Err("invalid type: null, expected f64".into());
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;
    use crate::cli::{Cli, Commands};

    fn execute_request(args: &[&str]) -> WmRequest {
        let cli =
            Cli::try_parse_from(["wm", "execute"].into_iter().chain(args.iter().copied())).unwrap();
        let Some(Commands::Execute(execute)) = cli.command else {
            panic!("expected an execute command");
        };
        execute.into_request().unwrap()
    }

    #[test]
    fn negative_resize_and_scroll_arguments_keep_their_sign() {
        assert_eq!(
            execute_request(&["window", "resize-by", "--amount", "-0.10"]),
            WmRequest::ExecuteCommand {
                command: WmCommand::Layout(LayoutCommand::ResizeWindowBy { amount: -0.1 }),
            }
        );
        assert_eq!(
            execute_request(&["layout", "scroll-strip", "-0.25"]),
            WmRequest::ExecuteCommand {
                command: WmCommand::Layout(LayoutCommand::ScrollStrip { delta: -0.25 }),
            }
        );
    }

    #[test]
    fn execute_requests_use_typed_protocol_commands() {
        let request = execute_request(&["window", "next"]);

        assert_eq!(
            serde_json::to_value(request).unwrap(),
            serde_json::json!({
                "execute_command": { "command": { "layout": "next_window" } }
            })
        );
    }

    #[test]
    fn execute_config_get_sends_a_query() {
        assert_eq!(execute_request(&["config", "get"]), WmRequest::GetConfig);
    }

    #[test]
    fn config_commands_are_not_embedded_as_json_strings() {
        let request = execute_request(&["config", "set-animate", "true"]);

        assert_eq!(
            serde_json::to_value(request).unwrap(),
            serde_json::json!({
                "execute_command": { "command": { "config": { "set_animate": true } } }
            })
        );
    }
}
