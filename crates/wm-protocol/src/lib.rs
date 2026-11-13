// Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
//! Shared, platform-neutral protocol types for Wm.
//!
//! The server owns the runtime model and the client owns the Mach transport,
//! but both use these types at the wire boundary. JSON encoding remains an
//! implementation detail of the transport crates.

extern crate self as wm_protocol;

pub const DEFAULT_SERVICE_NAME: &str = "git.kdnk.wm";

mod commands;
mod events;
mod layout;
mod queries;
mod selectors;
mod transport;

pub use commands::{
    AnimationEasing, ConfigCommand, LayoutCommand,
    MetricsCommand, ReactorCommand, WmCommand,
};
pub use events::{EventKind, StackInfo, WmEvent, WorkspaceId};
pub use layout::{
    Direction, DirectionalDistance, LayoutKind, LayoutMode, Orientation, ResizeOrientation,
};
pub use queries::{
    ApplicationData, ContainerNodeType, ContainerTreeNode, DisplayData, LayoutStateData, Point,
    Rect, Size, WindowData, WindowId, WindowLayoutPosition, WorkspaceData, WorkspaceLayoutData,
};
pub use selectors::{DisplaySelector, RestoreScope, RestoreSource, WorkspaceSelector};
pub use transport::{JsonWmResponse, WindowClaimFlags, WmRequest, WmResponse};
