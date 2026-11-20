// Modified by kdnk for wm on 2026-11-20. Original attribution is preserved in LICENSE.
use crate::common::collections::HashMap;
use crate::sys::app::{WindowInfo, is_ax_transient, pid_t};
use crate::sys::screen::SpaceId;
use crate::sys::skylight::SLSWindowTags;
use crate::sys::window_server::{NativeTransient, WindowServerId, window_tags};

#[derive(Default)]
pub(super) struct TransientFocusGuard {
    pub windows: HashMap<WindowServerId, Transient>,
}

pub(super) struct Transient {
    pub pid: pid_t,
    pub space: SpaceId,
    pub visible: bool,
    // AX recovery may supplement native evidence, but must not erase it.
    pub native: Option<NativeTransient>,
}

impl TransientFocusGuard {
    pub fn native(
        &mut self,
        id: WindowServerId,
        pid: pid_t,
        space: SpaceId,
        transient: Option<NativeTransient>,
    ) {
        if transient.is_some() {
            self.windows.insert(id, Transient {
                pid,
                space,
                visible: true,
                native: transient,
            });
        } else {
            self.windows.remove(&id);
        }
    }

    pub fn ax(&mut self, id: WindowServerId, pid: pid_t, info: &WindowInfo, space: SpaceId) {
        if self.windows.get(&id).is_some_and(|t| t.native.is_some() && t.pid == pid) {
            return;
        }
        if is_ax_transient(info)
            && !is_zoom_reactions_panel(id, info)
            && matches!(
                info.ax_role.as_deref(),
                Some("AXWindow" | "AXSheet" | "AXPopover" | "AXMenu")
            )
        {
            self.windows.insert(id, Transient {
                pid,
                space,
                visible: !info.is_minimized,
                native: None,
            });
        } else {
            self.windows.remove(&id);
        }
    }

    pub fn visibility(&mut self, id: WindowServerId, visible: bool) {
        if let Some(t) = self.windows.get_mut(&id) {
            t.visible = visible;
        }
    }

    pub fn depart(&mut self, id: WindowServerId, departed: SpaceId, resolved: Option<SpaceId>) {
        if let Some(t) = self.windows.get_mut(&id) {
            if let Some(space) = resolved.filter(|space| *space != departed) {
                t.space = space;
                t.visible = true;
            } else if t.space == departed {
                t.visible = false;
            }
        }
    }
}

fn is_zoom_reactions_panel(id: WindowServerId, info: &WindowInfo) -> bool {
    info.bundle_id.as_deref() == Some("us.zoom.xos")
        && info.ax_role.as_deref() == Some("AXWindow")
        && info.ax_subrole.as_deref() == Some("AXSystemDialog")
        && info.is_modal == Some(false)
        && !info.is_root
        && window_tags(id).map(SLSWindowTags::from_bits_retain).is_some_and(|tags| {
            tags.contains(
                SLSWindowTags::ATTACHED
                    | SLSWindowTags::FLOATING
                    | SLSWindowTags::ON_ALL_WORKSPACES
                    | SLSWindowTags::PREVENTS_ACTIVATION
                    | SLSWindowTags::IGNORES_CYCLE
                    | SLSWindowTags::DEFERS_ORDERING,
            ) && !tags.intersects(SLSWindowTags::MODAL | SLSWindowTags::DOCUMENT)
        })
}
