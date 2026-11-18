// Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
use serde::{Deserialize, Serialize};
pub use wm_protocol::{DisplaySelector, ReactorCommand};

use crate::actor::app::{AppInfo, AppThreadHandle, WindowId, pid_t};
use crate::common::log::MetricsCommand;
use crate::layout_engine::{LayoutCommand, WindowLayoutInfo};
use crate::model::WindowStore;
use crate::sys::app::WindowInfo;
use crate::sys::screen::SpaceId;

/// All mutable domain state is owned by the reactor thread.
///
/// Workspace topology is still carried by the layout coordinator during this
/// migration, but window identity, native-space observations, and workspace
/// assignments have one explicit owner here. Cross-store operations receive
/// this store by reference instead of retaining an alias to it.
#[derive(Debug, Default)]
pub struct WmState {
    pub windows: WindowStore,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Requested(pub bool);

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(untagged)]
pub enum Command {
    Layout(LayoutCommand),
    Metrics(MetricsCommand),
    Reactor(ReactorCommand),
}

#[derive(Debug, Clone)]
pub enum MissionControlState {
    Inactive,
    Active,
    Transitioning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuState {
    Closed,
    Open(pid_t),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceSwitchState {
    Inactive,
    Active,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceSwitchOrigin {
    Manual,
    Auto,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StaleCleanupState {
    Enabled,
    Suppressed,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RefocusState {
    None,
    Pending(SpaceId),
}

#[derive(Debug)]
pub(crate) struct AppState {
    #[allow(unused)]
    pub(crate) info: AppInfo,
    pub(crate) handle: AppThreadHandle,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct UnmanagedWindowOptions {
    pub(crate) track_focus: bool,
    pub(crate) follow_parent: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct WindowState {
    pub(crate) info: WindowInfo,
    /// The last known frame of the window. Always includes the last write.
    ///
    /// This value only updates monotonically with respect to writes; in other
    /// words, we only accept reads when we know they come after the last write.
    pub(crate) frame_monotonic: CGRect,
    /// Wm/macOS heuristic result, kept separately from an explicit app-rule
    /// override so every discovered window can remain tracked.
    pub(crate) is_manageable: bool,
    pub(crate) native_transient: bool,
    pub(crate) manage_override: Option<bool>,
    pub(crate) unmanaged: UnmanagedWindowOptions,
    /// Some apps temporarily give a tiled editor AXDialog while its child is open.
    pub(crate) has_following_dialog: bool,
}

impl From<WindowInfo> for WindowState {
    fn from(info: WindowInfo) -> WindowState {
        WindowState {
            frame_monotonic: info.frame,
            info,
            is_manageable: false,
            native_transient: false,
            manage_override: None,
            unmanaged: UnmanagedWindowOptions::default(),
            has_following_dialog: false,
        }
    }
}

impl WindowState {
    pub(crate) fn layout_info(&self, wid: WindowId) -> WindowLayoutInfo {
        WindowLayoutInfo {
            window_id: wid,
            title: Some(self.info.title.clone()),
            ax_role: self.info.ax_role.clone(),
            ax_subrole: self.info.ax_subrole.clone(),
            is_resizable: self.info.is_resizable,
            current_size: self.frame_monotonic.size,
            min_size: self.info.min_size,
            max_size: self.info.max_size,
        }
    }

    /// The single admission policy used by every layout-facing caller.
    pub(crate) fn is_admitted(&self) -> bool {
        self.is_admitted_with_override(self.manage_override)
    }

    pub(crate) fn is_admitted_with_override(&self, manage_override: Option<bool>) -> bool {
        if self.info.is_minimized || manage_override == Some(false) {
            return false;
        }
        let is_ax_dialog = self.info.ax_role.as_deref() == Some("AXWindow")
            && self.info.ax_subrole.as_deref() == Some("AXDialog");
        if self.has_following_dialog && is_ax_dialog && !self.native_transient {
            return true;
        }
        let explicitly_managed_dialog = manage_override == Some(true)
            && is_ax_dialog
            && self.info.is_root
            && self.info.is_resizable
            && self.info.is_modal == Some(false);
        (!self.native_transient
            || (manage_override == Some(true)
                && self.info.ax_role.as_deref() == Some("AXWindow")
                && self.info.ax_subrole.as_deref() == Some("AXStandardWindow")))
            && (!crate::sys::app::is_ax_transient(&self.info) || explicitly_managed_dialog)
            && manage_override.unwrap_or(self.is_manageable)
    }

    fn is_unmanaged_ax_window(&self) -> bool {
        self.manage_override == Some(false)
            && !self.info.is_minimized
            && self.info.ax_role.as_deref() == Some("AXWindow")
    }

    pub(crate) fn tracks_unmanaged_focus(&self) -> bool {
        self.unmanaged.track_focus && self.is_unmanaged_ax_window()
    }

    pub(crate) fn follows_unmanaged_parent(&self) -> bool {
        self.unmanaged.follow_parent && self.is_unmanaged_ax_window()
    }

    pub(crate) fn can_reconcile_admission(&self) -> bool { !self.info.is_minimized }
}

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ReactorError {
    #[error("App communication failed: {0}")]
    AppCommunicationFailed(#[from] tokio::sync::mpsc::error::SendError<crate::actor::app::Request>),
    #[error("Stack line communication failed: {0}")]
    StackLineCommunicationFailed(
        #[from] tokio::sync::mpsc::error::TrySendError<crate::actor::stack_line::Event>,
    ),
    #[error("Raise manager communication failed: {0}")]
    RaiseManagerCommunicationFailed(
        #[from] tokio::sync::mpsc::error::SendError<crate::actor::raise_manager::Event>,
    ),
}

#[cfg(test)]
mod tests {
    use wm_protocol::{RestoreScope, RestoreSource};

    use super::*;

    #[test]
    fn followed_editor_keeps_admission_during_temporary_dialog_role() {
        let mut info =
            crate::actor::reactor::testing::make_window_info(CGRect::ZERO, None, "editor", None);
        info.ax_role = Some("AXWindow".into());
        info.ax_subrole = Some("AXDialog".into());
        let mut editor = WindowState::from(info);
        assert!(!editor.is_admitted());
        editor.has_following_dialog = true;
        assert!(editor.is_admitted());
        assert!(!editor.is_admitted_with_override(Some(false)));
        editor.info.is_minimized = true;
        assert!(!editor.is_admitted());
        editor.info.is_minimized = false;
        editor.native_transient = true;
        assert!(!editor.is_admitted());
        editor.native_transient = false;
        editor.info.ax_subrole = Some("AXSheet".into());
        assert!(!editor.is_admitted());
    }

    #[test]
    fn legacy_restore_command_defaults_to_portable_source_policy() {
        let mut serialized = serde_json::to_value(ReactorCommand::RestoreLayout {
            path: "layout.ron".into(),
            scope: RestoreScope::Workspace,
            source: RestoreSource::CurrentSpace,
        })
        .unwrap();
        serialized
            .get_mut("restore_layout")
            .and_then(serde_json::Value::as_object_mut)
            .unwrap()
            .remove("source");

        let restored: ReactorCommand = serde_json::from_value(serialized).unwrap();

        assert_eq!(restored, ReactorCommand::RestoreLayout {
            path: "layout.ron".into(),
            scope: RestoreScope::Workspace,
            source: RestoreSource::SavedActiveSpace,
        });
    }
}
use objc2_core_foundation::CGRect;
