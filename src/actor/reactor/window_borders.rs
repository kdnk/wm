// Modified by kdnk for wm on 2026-11-19. Original attribution is preserved in LICENSE.
use super::{BTreeMap, Reactor, WindowServerId};
use crate::actor::window_borders::BorderWindow;

impl Reactor {
    pub(super) fn publish_window_borders(&self) {
        let Some(tx) = &self.borders_tx else { return };
        let settings = self.config.settings.ui.window_borders;
        let windows =
            if settings.enabled && !self.refreshes_blocked() && !self.is_mission_control_active() {
                self.border_windows()
            } else {
                BTreeMap::new()
            };
        tx.set_windows(settings, windows);
    }

    fn border_windows(&self) -> BTreeMap<WindowServerId, BorderWindow> {
        let store = &self.state.windows;
        let workspaces = self.layout_manager.layout_engine.workspaces();
        store
            .iter_windows()
            .filter_map(|(wid, window)| {
                let id = window.info.sys_id?;
                if !store.is_window_visible(id)
                    || window.info.is_minimized
                    || store.is_window_native_fullscreen_suspended(wid)
                    || !(window.is_admitted()
                        || (window.info.is_standard
                            && window.info.is_root
                            && !window.native_transient))
                {
                    return None;
                }
                let space = store
                    .native_space(wid)
                    .or_else(|| self.space_state.active_window_spaces.get(&id).copied())?;
                if self.space_state.fullscreen_spaces.contains(&space)
                    || store
                        .workspace_for_window(wid)
                        .is_some_and(|ws| workspaces.active_workspace(space) != Some(ws))
                {
                    return None;
                }
                let dialog_parent = self
                    .dialog_parents
                    .get(&wid)
                    .and_then(|parent| store.window(parent.window)?.info.sys_id);
                Some((id, BorderWindow {
                    space,
                    screen: self.space_state.screen_by_space(space)?.frame,
                    dialog_parent,
                }))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};

    use super::*;
    use crate::actor::app::WindowId;
    use crate::actor::reactor::testing::{space_state_event, test_reactor_with_workspace_count};
    use crate::sys::screen::SpaceId;

    #[test]
    fn border_inventory_follows_native_visibility_and_workspace_membership() {
        let mut reactor = test_reactor_with_workspace_count(2);
        let space = SpaceId::new(41);
        let screen = CGRect::new(CGPoint::ZERO, CGSize::new(1000.0, 800.0));
        reactor.handle_event(space_state_event(vec![screen], vec![Some(space)]));
        let spaces = reactor.test_workspace_ids(space);
        for index in 1..=6 {
            reactor.add_test_window(
                WindowId::new(10, index),
                WindowServerId(index),
                Some(space),
                screen,
            );
        }
        let store = &mut reactor.state.windows;
        store.assign_window_to_workspace(WindowId::new(10, 1), spaces[0]);
        store.assign_window_to_workspace(WindowId::new(10, 2), spaces[1]);
        store.window_mut(WindowId::new(10, 3)).unwrap().manage_override = Some(false);
        store.mark_window_hidden(WindowServerId(4));
        store.window_mut(WindowId::new(10, 5)).unwrap().info.is_minimized = true;
        store.window_mut(WindowId::new(10, 6)).unwrap().native_transient = true;
        assert_eq!(reactor.border_windows().keys().copied().collect::<Vec<_>>(), [
            WindowServerId(1),
            WindowServerId(3)
        ]);
        reactor.set_test_active_workspace(space, spaces[1]);
        assert_eq!(reactor.border_windows().keys().copied().collect::<Vec<_>>(), [
            WindowServerId(2),
            WindowServerId(3)
        ]);
        reactor.space_state.fullscreen_spaces.insert(space);
        assert!(reactor.border_windows().is_empty());
    }
}
