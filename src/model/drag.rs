// Modified by kdnk for wm on 2026-11-21. Original attribution is preserved in LICENSE.
use objc2_core_foundation::{CGPoint, CGRect};

use crate::actor::app::WindowId;
use crate::layout_engine::WindowDropAction;
use crate::sys::screen::SpaceId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragKind {
    NativeMove,
    NativeResize,
    ModifierMove,
}

#[derive(Debug, Clone, Copy)]
pub struct DragSceneTarget {
    pub window: WindowId,
    pub space: SpaceId,
    pub frame: CGRect,
}

#[derive(Debug, Clone, Default)]
pub struct DragScene {
    /// Layout-wide semantic override. Stack layouts use Swap for every zone.
    pub action_override: Option<WindowDropAction>,
    /// Whether each source-tile edge points toward a logical tiled neighbor.
    pub source_neighbors: [bool; 4],
    pub targets: Vec<DragSceneTarget>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropZone {
    Center,
    West,
    East,
    North,
    South,
}

#[derive(Debug, Clone, Copy)]
pub struct DropIntent {
    pub window: WindowId,
    pub space: SpaceId,
    pub frame: CGRect,
    pub zone: DropZone,
    pub action: WindowDropAction,
}

#[derive(Debug, Clone, Copy)]
pub struct DropTarget {
    pub intent: DropIntent,
    pub preview_area: CGRect,
}

#[derive(Debug, Clone, Copy)]
pub struct PreviewTarget {
    pub anchor: PreviewAnchor,
    pub preview_area: CGRect,
    pub stacked: bool,
}

#[derive(Debug, Clone, Copy)]
pub enum PreviewAnchor {
    Window(WindowId),
    Display(SpaceId),
}

impl From<DropTarget> for PreviewTarget {
    fn from(target: DropTarget) -> Self {
        Self {
            anchor: PreviewAnchor::Window(target.intent.window),
            preview_area: target.preview_area,
            stacked: target.intent.action == WindowDropAction::Stack,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct TransferPreview {
    pub space: SpaceId,
    pub frame: Option<CGRect>,
}

#[derive(Debug, Clone, Copy)]
pub struct DragSource {
    pub window: WindowId,
    pub origin_frame: CGRect,
    pub last_frame: CGRect,
    pub origin_space: Option<SpaceId>,
    pub current_space: Option<SpaceId>,
    pub tiled: bool,
}

#[derive(Debug, Clone)]
pub struct DragCommit {
    pub source: DragSource,
    pub target: Option<DropTarget>,
    pub pointer: CGPoint,
    pub kind: DragKind,
}

#[derive(Debug, Clone, Copy)]
pub struct DragCancel {
    pub source: DragSource,
    pub kind: DragKind,
}
