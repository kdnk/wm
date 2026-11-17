// Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
pub mod engine;
mod floating;
pub(crate) mod graph;
pub mod systems;
pub mod utils;
pub(crate) mod workspaces;

pub(crate) use engine::WindowLayoutInfo;
pub use engine::{
    EventResponse, LayoutCommand, LayoutEngine, LayoutEvent, LayoutEventOutcome, ResolvedWindow,
    RestoreReport, RestoreRequest, RestoreScope, RestoreSource,
};
pub(crate) use floating::FloatingManager;
pub use graph::{Direction, LayoutKind, Orientation, ResizeOrientation};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HorizontalResizeAnchor {
    #[default]
    Left,
    Right,
}

impl HorizontalResizeAnchor {
    pub fn from_pointer(
        screen: Option<objc2_core_foundation::CGRect>,
        pointer: Option<objc2_core_foundation::CGPoint>,
    ) -> Self {
        if let (Some(screen), Some(pointer)) = (screen, pointer)
            && pointer.x.is_finite()
            && pointer.y.is_finite()
            && pointer.x >= screen.origin.x
            && pointer.x < screen.max().x
            && pointer.y >= screen.origin.y
            && pointer.y < screen.max().y
            && pointer.x >= screen.mid().x
        {
            Self::Right
        } else {
            Self::Left
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct ColumnId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnPlacement {
    Before,
    After,
}

#[derive(Debug, Clone)]
pub struct ColumnReorderRequest {
    pub space: crate::sys::screen::SpaceId,
    pub workspace: VirtualWorkspaceId,
    pub source: ColumnId,
    pub target: ColumnId,
    pub placement: ColumnPlacement,
    pub source_members: Vec<crate::actor::app::WindowId>,
    pub target_members: Vec<crate::actor::app::WindowId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowDropAction {
    Swap,
    Stack,
    Insert(Direction),
    Move(Direction),
    ConsumeOrExpel(Direction),
}

impl From<crate::common::config::MouseDropAction> for WindowDropAction {
    fn from(action: crate::common::config::MouseDropAction) -> Self {
        match action {
            crate::common::config::MouseDropAction::Swap => Self::Swap,
            crate::common::config::MouseDropAction::Stack => Self::Stack,
        }
    }
}

/// The user's destination and placement; source membership and classification are authoritative.
#[derive(Debug, Clone)]
pub struct WorkspaceDropRequest {
    pub window: crate::actor::app::WindowId,
    pub workspace: VirtualWorkspaceId,
    pub target: Option<(crate::actor::app::WindowId, WindowDropAction)>,
    pub frame: Option<objc2_core_foundation::CGRect>,
}

pub(crate) struct WindowDropRequest {
    pub source: crate::actor::app::WindowId,
    pub target: crate::actor::app::WindowId,
    pub space: crate::sys::screen::SpaceId,
    pub action: WindowDropAction,
}
pub(crate) use systems::LayoutId;
pub use systems::{LayoutSystem, LayoutSystemKind, ScrollingLayoutSystem};

pub use crate::model::virtual_workspace::{VirtualWorkspaceId, WorkspaceStore};

#[cfg(test)]
mod drop_preview_tests;

#[cfg(test)]
mod resize_anchor_tests {
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};

    use super::HorizontalResizeAnchor;

    #[test]
    fn pointer_anchor_uses_target_display_coordinates_and_defaults_without_input() {
        let screen = CGRect::new(CGPoint::new(-1500.0, 80.0), CGSize::new(1000.0, 800.0));
        for (pointer, expected) in [
            (Some(CGPoint::new(-1499.0, 100.0)), HorizontalResizeAnchor::Left),
            (Some(CGPoint::new(-1000.0, 100.0)), HorizontalResizeAnchor::Right),
            (Some(CGPoint::new(-501.0, 100.0)), HorizontalResizeAnchor::Right),
            (Some(CGPoint::new(-500.0, 100.0)), HorizontalResizeAnchor::Left),
            (Some(CGPoint::new(500.0, 100.0)), HorizontalResizeAnchor::Left),
            (Some(CGPoint::new(-700.0, 50.0)), HorizontalResizeAnchor::Left),
            (Some(CGPoint::new(f64::NAN, 100.0)), HorizontalResizeAnchor::Left),
            (
                Some(CGPoint::new(-700.0, f64::INFINITY)),
                HorizontalResizeAnchor::Left,
            ),
            (None, HorizontalResizeAnchor::Left),
        ] {
            assert_eq!(
                HorizontalResizeAnchor::from_pointer(Some(screen), pointer),
                expected
            );
        }
        assert_eq!(
            HorizontalResizeAnchor::from_pointer(None, Some(screen.mid())),
            HorizontalResizeAnchor::Left
        );
    }
}
