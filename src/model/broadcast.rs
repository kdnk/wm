// Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
use slotmap::Key;
/// The actor broadcast channel uses the protocol event directly. This keeps
/// the server and client on one event definition instead of maintaining a
/// runtime copy that must be translated before IPC.
pub use wm_protocol::WmEvent as BroadcastEvent;

use crate::actor::app::WindowId;
use crate::model::virtual_workspace::VirtualWorkspaceId;

pub type BroadcastSender = crate::actor::Sender<BroadcastEvent>;
pub type BroadcastReceiver = crate::actor::Receiver<BroadcastEvent>;

pub fn protocol_workspace_id(id: VirtualWorkspaceId) -> wm_protocol::WorkspaceId {
    let value = id.data().as_ffi();
    wm_protocol::WorkspaceId {
        idx: value as u32,
        version: (value >> 32) as u32,
    }
}

pub fn protocol_window_id(id: WindowId) -> wm_protocol::WindowId { id.into() }
