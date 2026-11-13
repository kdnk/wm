// Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
use std::path::PathBuf;

use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::{
    Direction, DisplaySelector, LayoutMode, ResizeOrientation, RestoreScope, RestoreSource,
    WindowId, WorkspaceSelector,
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LayoutCommand {
    NextWindow,
    PrevWindow,
    MoveFocus(#[serde(rename = "direction")] Direction),
    Ascend,
    Descend,
    MoveNode(Direction),
    JoinWindow(Direction),
    ConsumeOrExpelWindow(Direction),
    ToggleStack,
    UnjoinWindows,
    ToggleFullscreen,
    ToggleFullscreenWithinGaps,
    ResizeWindowGrow(ResizeOrientation),
    ResizeWindowShrink(ResizeOrientation),
    ResizeWindowBy {
        amount: f64,
    },
    ScrollStrip {
        delta: f64,
    },
    SnapStrip,
    SwitchPresetColumnWidth {
        #[serde(default)]
        backwards: bool,
    },
    CenterSelection,
    NextWorkspace(Option<bool>),
    PrevWorkspace(Option<bool>),
    SwitchToWorkspace(usize),
    MoveWindowToWorkspace {
        workspace: WorkspaceSelector,
        follow: bool,
        window_id: Option<u32>,
    },
    CreateWorkspace,
    SwitchToLastWorkspace,
    SwapWindows(WindowId, WindowId),
    /// Move `window`'s tab to where `target`'s tab sits in their shared stack,
    /// shifting the tabs between them toward the vacated slot.
    MoveStackTab {
        window: WindowId,
        target: WindowId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReactorCommand {
    BindingMode(String),
    Debug,
    Serialize,
    SaveLayout {
        path: PathBuf,
    },
    SaveAndExit,
    RestoreLayout {
        path: PathBuf,
        scope: RestoreScope,
        #[serde(default)]
        source: RestoreSource,
    },
    SwitchSpace(Direction),
    ToggleSpaceActivated,
    FocusWindow {
        window_id: WindowId,
        window_server_id: Option<u32>,
    },
    ShowMissionControlAll,
    ShowMissionControlCurrent,
    DismissMissionControl,
    MoveMouseToDisplay(DisplaySelector),
    FocusDisplay(DisplaySelector),
    CloseWindow {
        window_server_id: Option<u32>,
    },
    MoveWindowToDisplay {
        selector: DisplaySelector,
        window_id: Option<u32>,
    },
    /// Move the active workspace to another display.
    ///
    /// Wm workspaces are display-local, so windows move into the destination
    /// display's workspace at the same ordinal and that workspace becomes active.
    MoveWorkspaceToDisplay {
        selector: DisplaySelector,
        /// Continue from the opposite edge when a directional selector has no
        /// display further in that direction.
        #[serde(default)]
        wrap_around: bool,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricsCommand {
    ShowTiming,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigCommand {
    SetAnimate(bool),
    SetAnimationDuration(f64),
    SetAnimationFps(f64),
    SetAnimationEasing(AnimationEasing),
    SetMouseFollowsFocus(bool),
    SetMouseHidesOnFocus(bool),
    SetFocusFollowsMouse(bool),
    SetOuterGaps {
        top: f64,
        left: f64,
        bottom: f64,
        right: f64,
    },
    SetInnerGaps {
        horizontal: f64,
        vertical: f64,
    },
    SetWorkspaceNames(Vec<String>),
    Set {
        key: String,
        value: Value,
    },
    GetConfig,
    SaveConfig,
    ReloadConfig,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnimationEasing {
    #[default]
    EaseInOut,
    Linear,
    EaseInSine,
    EaseOutSine,
    EaseInOutSine,
    EaseInQuad,
    EaseOutQuad,
    EaseInOutQuad,
    EaseInCubic,
    EaseOutCubic,
    EaseInOutCubic,
    EaseInQuart,
    EaseOutQuart,
    EaseInOutQuart,
    EaseInQuint,
    EaseOutQuint,
    EaseInOutQuint,
    EaseInExpo,
    EaseOutExpo,
    EaseInOutExpo,
    EaseInCirc,
    EaseOutCirc,
    EaseInOutCirc,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WmCommand {
    Layout(LayoutCommand),
    Metrics(MetricsCommand),
    Reactor(ReactorCommand),
    Config(ConfigCommand),
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum TypedWmCommand {
    Layout(LayoutCommand),
    Metrics(MetricsCommand),
    Reactor(ReactorCommand),
    Config(ConfigCommand),
}

#[derive(Deserialize)]
enum LegacyCommand {
    #[serde(alias = "reactor")]
    Reactor(LegacyReactorCommand),
    #[serde(alias = "config")]
    Config(ConfigCommand),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum LegacyReactorCommand {
    Layout(LayoutCommand),
    Metrics(MetricsCommand),
    Reactor(ReactorCommand),
}

impl From<TypedWmCommand> for WmCommand {
    fn from(command: TypedWmCommand) -> Self {
        match command {
            TypedWmCommand::Layout(command) => Self::Layout(command),
            TypedWmCommand::Metrics(command) => Self::Metrics(command),
            TypedWmCommand::Reactor(command) => Self::Reactor(command),
            TypedWmCommand::Config(command) => Self::Config(command),
        }
    }
}

impl<'de> Deserialize<'de> for WmCommand {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where D: Deserializer<'de> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum CommandInput {
            Typed(TypedWmCommand),
            LegacyJson(String),
        }

        match CommandInput::deserialize(deserializer)? {
            CommandInput::Typed(command) => Ok(command.into()),
            CommandInput::LegacyJson(command) => decode_legacy_command(&command),
        }
    }
}

fn decode_legacy_command<E>(command: &str) -> Result<WmCommand, E>
where E: DeError {
    match serde_json::from_str::<LegacyCommand>(command)
        .map_err(|error| E::custom(format!("invalid legacy command JSON: {error}")))?
    {
        LegacyCommand::Config(command) => Ok(WmCommand::Config(command)),
        LegacyCommand::Reactor(LegacyReactorCommand::Layout(command)) => {
            Ok(WmCommand::Layout(command))
        }
        LegacyCommand::Reactor(LegacyReactorCommand::Metrics(command)) => {
            Ok(WmCommand::Metrics(command))
        }
        LegacyCommand::Reactor(LegacyReactorCommand::Reactor(command)) => {
            Ok(WmCommand::Reactor(command))
        }
    }
}
