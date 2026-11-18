// Modified by kdnk for wm on 2026-11-19. Original attribution is preserved in LICENSE.
use super::*;

#[derive(Clone, Copy, Debug)]
pub(super) struct DialogParent {
    pub window: WindowId,
    pub offset_x: f64,
}

impl Reactor {
    pub(super) fn can_move_dialog_with_parent(&self, dialog: WindowId) -> bool {
        self.state.windows.window(dialog).is_some_and(|w| w.follows_unmanaged_parent())
            && !self.state.windows.is_claimed(dialog)
            && !self.state.windows.is_temporarily_unavailable(dialog)
            && self.state.windows.can_place_window(dialog)
    }

    pub(super) fn is_dialog_owner(&self, wid: WindowId, space: SpaceId) -> bool {
        let Some(window) = self.state.windows.window(wid) else {
            return false;
        };
        let modal_blocked = window.info.ax_role.as_deref() == Some("AXWindow")
            && window.info.ax_subrole.as_deref() == Some("AXDialog")
            && window.manage_override != Some(false)
            && !window.info.is_minimized;
        (self.state.windows.is_admitted(wid) || modal_blocked)
            && self.state.windows.can_place_window(wid)
            && self.state.windows.can_reconcile_admission(wid)
            && !self.layout_manager.layout_engine.is_window_floating(wid)
            && self.state.windows.workspace_for_window(wid).is_some_and(|workspace| {
                self.layout_manager.layout_engine.workspaces().active_workspace(space)
                    == Some(workspace)
            })
    }

    pub(super) fn refresh_dialog_parents(&mut self) {
        self.last_focused_editor
            .retain(|_, wid| self.state.windows.contains_window(*wid));
        self.dialog_parents.retain(|dialog, _| {
            self.state.windows.window(*dialog).is_some_and(|window| {
                window.manage_override == Some(false) && window.unmanaged.follow_parent
            })
        });
        let new_dialogs: Vec<_> = self
            .state
            .windows
            .iter_windows()
            .filter_map(|(wid, window)| {
                (window.follows_unmanaged_parent() && !self.dialog_parents.contains_key(&wid))
                    .then_some((wid, window.info.sys_id, window.frame_monotonic))
            })
            .collect();
        for (dialog, wsid, frame) in new_dialogs {
            let Some(space) = self.best_space_for_window(&frame, wsid) else {
                continue;
            };
            let native = wsid
                .and_then(window_server::window_parent)
                .and_then(|id| self.state.windows.tracked_window_id(id));
            let owner = if let Some(parent) = native {
                (parent.pid == dialog.pid && self.is_dialog_owner(parent, space)).then_some(parent)
            } else {
                self.last_focused_editor
                    .get(&dialog.pid)
                    .copied()
                    .or_else(|| {
                        self.layout_manager
                            .layout_engine
                            .focused_window()
                            .filter(|w| w.pid == dialog.pid)
                    })
                    .filter(|owner| self.is_dialog_owner(*owner, space))
                    .or_else(|| {
                        let mut editors = self
                            .state
                            .windows
                            .window_ids_for_pid(dialog.pid)
                            .filter(|wid| self.is_dialog_owner(*wid, space));
                        let first = editors.next()?;
                        editors.next().is_none().then_some(first)
                    })
            };
            if let Some(owner) = owner
                && let Some(parent) = self.state.windows.window(owner)
            {
                self.dialog_parents.insert(dialog, DialogParent {
                    window: owner,
                    offset_x: frame.origin.x - parent.frame_monotonic.origin.x,
                });
            }
        }
        let active_owners: HashSet<_> = self
            .dialog_parents
            .iter()
            .filter(|(dialog, parent)| {
                self.can_move_dialog_with_parent(**dialog)
                    && self.state.windows.can_reconcile_admission(parent.window)
                    && self.state.windows.can_place_window(parent.window)
            })
            .map(|(_, parent)| parent.window)
            .collect();
        let changes: Vec<_> = self
            .state
            .windows
            .iter_windows()
            .filter_map(|(wid, state)| {
                let active = active_owners.contains(&wid);
                (active != state.has_following_dialog).then_some((wid, active))
            })
            .collect();
        for (wid, active) in changes {
            let was_admitted = self.state.windows.is_admitted(wid);
            self.state.windows.window_mut(wid).unwrap().has_following_dialog = active;
            let admitted = self.state.windows.is_admitted(wid);
            if admitted != was_admitted
                && let Some(space) = self.assigned_space_for_window_id(wid)
            {
                let event = if admitted {
                    LayoutEvent::WindowAdded(space, wid)
                } else {
                    self.request_window_inventory(wid.pid);
                    LayoutEvent::WindowSuspended(wid)
                };
                self.send_layout_event(event);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};

    use super::*;
    use crate::actor::reactor::animation::AnimationManager;
    use crate::actor::reactor::testing::{
        Apps, make_window, make_windows, space_state_event, test_reactor_with_workspace_settings,
    };
    use crate::common::config::{AppWorkspaceRule, VirtualWorkspaceSettings};

    #[test]
    fn dialog_follows_its_native_or_opening_editor_in_both_directions() {
        for native_parent in [false, true] {
            let mut reactor = test_reactor_with_workspace_settings(&VirtualWorkspaceSettings {
                app_rules: vec![
                    AppWorkspaceRule {
                        title_regex: Some("^Picker".into()),
                        manage: Some(false),
                        follow_parent: true,
                        track_focus: true,
                        ..Default::default()
                    },
                    AppWorkspaceRule {
                        title_regex: Some("^Unrelated".into()),
                        manage: Some(false),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            });
            let mut apps = Apps::new();
            let space = SpaceId::new(1);
            let screen = CGRect::new(CGPoint::ZERO, CGSize::new(1000., 800.));
            reactor.handle_event(space_state_event(vec![screen], vec![Some(space)]));
            apps.make_app_and_settle(&mut reactor, 1, make_windows(2));
            let first = WindowId::new(1, 1);
            let second = WindowId::new(1, 2);
            let dialog = WindowId::new(1, 3);
            let unrelated = WindowId::new(1, 4);
            reactor.handle_event(Event::WindowServerFocusChanged(second, space));
            let mut info = make_window(3);
            info.ax_role = Some("AXWindow".into());
            info.title = "Picker".into();
            let id = info.sys_id.unwrap();
            let owner = if native_parent { first } else { second };
            let blocked = reactor.state.windows.window_mut(owner).unwrap();
            blocked.info.ax_role = Some("AXWindow".into());
            blocked.info.ax_subrole = Some("AXDialog".into());
            blocked.info.is_standard = false;
            blocked.is_manageable = false;
            reactor.send_layout_event(LayoutEvent::WindowSuspended(owner));
            assert!(!reactor.state.windows.is_admitted(owner));
            let parent = native_parent.then(|| reactor.test_window_server_id(first));
            window_server::set_window_parent_override(id, Some(parent));
            reactor.handle_event(Event::WindowCreated(dialog, info.clone(), None, None));
            let mut other = make_window(4);
            other.title = "Unrelated".into();
            other.ax_role = Some("AXWindow".into());
            reactor.handle_event(Event::WindowCreated(unrelated, other.clone(), None, None));
            assert!(
                reactor.state.windows.is_admitted(owner),
                "modal editor keeps its tile"
            );
            let neighbor = if native_parent { second } else { first };
            assert_eq!(reactor.dialog_parents[&dialog].window, owner);
            reactor.handle_event(Event::WindowServerFocusChanged(neighbor, space));
            reactor.refresh_dialog_parents();
            let offset = reactor.dialog_parents[&dialog].offset_x;
            for (step, dx) in [120., -220., 100.].into_iter().enumerate() {
                let mut target = reactor.state.windows.window(owner).unwrap().frame_monotonic;
                if step == 1 {
                    // macOS accepted a different dialog x at the display edge.
                    reactor.state.windows.window_mut(dialog).unwrap().frame_monotonic.origin.x +=
                        219.;
                }
                let mut neighbor_frame =
                    reactor.state.windows.window(neighbor).unwrap().frame_monotonic;
                neighbor_frame.origin.x -= dx;
                target.origin.x += dx;
                AnimationManager::instant_layout(
                    &mut reactor,
                    space,
                    &[(owner, target), (neighbor, neighbor_frame)],
                    None,
                );
                let after = reactor.state.windows.window(dialog).unwrap().frame_monotonic;
                assert_eq!(after.origin.x, target.origin.x + offset);
                assert_eq!(after.origin.y, info.frame.origin.y);
                assert_eq!(after.size, info.frame.size);
                assert_eq!(
                    reactor.state.windows.window(unrelated).unwrap().frame_monotonic,
                    other.frame
                );
                assert!(reactor.state.windows.workspace_for_window(dialog).is_none());
            }
            reactor.handle_event(Event::WindowMinimized(dialog));
            assert!(!reactor.state.windows.window(owner).unwrap().has_following_dialog);
            reactor.handle_event(Event::WindowDeminiaturized(dialog));
            reactor.handle_event(Event::WindowDestroyed(owner));
            assert_eq!(reactor.dialog_parents[&dialog].window, owner);
            reactor.handle_event(Event::WindowDestroyed(dialog));
            assert!(!reactor.dialog_parents.contains_key(&dialog));
            window_server::set_window_parent_override(id, None);
        }
    }
}
