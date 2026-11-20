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
            let owner = if let Some(parent_id) = wsid.and_then(window_server::window_parent) {
                self.state.windows.tracked_window_id(parent_id).filter(|parent| {
                    parent.pid == dialog.pid && self.is_dialog_owner(*parent, space)
                })
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

    fn projects() -> (Reactor, SpaceId, Apps) {
        let mut reactor = test_reactor_with_workspace_settings(&VirtualWorkspaceSettings {
            default_workspace_count: 2,
            app_rules: vec![AppWorkspaceRule {
                title_regex: Some("^(Fuzzy Finder|Live Grep)$".into()),
                manage: Some(false),
                follow_parent: true,
                track_focus: true,
                ..Default::default()
            }],
            ..Default::default()
        });
        let space = SpaceId::new(1);
        let screen = CGRect::new(CGPoint::ZERO, CGSize::new(1200., 800.));
        let mut apps = Apps::new();
        apps.make_app_and_settle_on_screen(&mut reactor, screen, space, 1, make_windows(2));
        (reactor, space, apps)
    }

    fn open_picker(
        reactor: &mut Reactor,
        idx: u32,
        title: &str,
        parent: Option<WindowServerId>,
    ) -> WindowId {
        let mut info = make_window(idx as usize);
        info.ax_role = Some("AXWindow".into());
        info.title = title.into();
        window_server::set_window_parent_override(info.sys_id.unwrap(), Some(parent));
        let picker = WindowId::new(1, idx);
        reactor.handle_event(Event::WindowCreated(picker, info, None, None));
        picker
    }

    fn frame(reactor: &Reactor, wid: WindowId) -> CGRect {
        reactor.state.windows.window(wid).unwrap().frame_monotonic
    }

    fn shift(reactor: &mut Reactor, space: SpaceId, window: WindowId, dx: f64) {
        let mut target = frame(reactor, window);
        target.origin.x += dx;
        AnimationManager::instant_layout(reactor, space, &[(window, target)], None);
    }

    #[test]
    fn a_picker_parks_at_its_editors_edge_and_returns_to_its_offset() {
        let (mut reactor, space, _apps) = projects();
        let editor = WindowId::new(1, 1);
        let owner = reactor.test_window_server_id(editor);
        let picker = open_picker(&mut reactor, 3, "Fuzzy Finder", Some(owner));
        let screen = reactor.space_state.screen_by_space(space).unwrap().frame;
        let starting_editor = frame(&reactor, editor);
        let starting_picker = frame(&reactor, picker);

        for (dx, parking_x) in [
            (
                2.0 * screen.size.width,
                screen.origin.x + screen.size.width - 1.0,
            ),
            (
                -2.0 * screen.size.width,
                screen.origin.x - starting_picker.size.width + 1.0,
            ),
        ] {
            shift(&mut reactor, space, editor, dx);
            let mut parked = starting_picker;
            parked.origin.x = parking_x;
            assert_eq!(frame(&reactor, picker), parked);

            let return_dx = starting_editor.origin.x - frame(&reactor, editor).origin.x;
            shift(&mut reactor, space, editor, return_dx);
            assert_eq!(frame(&reactor, editor), starting_editor);
            assert_eq!(frame(&reactor, picker), starting_picker);
        }
        window_server::set_window_parent_override(WindowServerId::new(3), None);
    }

    #[test]
    fn picker_keeps_the_final_drag_position_when_the_move_arrives_after_mouse_up() {
        use crate::actor::drag::MouseButton;
        use crate::sys::event::MouseState;

        for animation_readback_first in [false, true] {
            let (mut reactor, space, _apps) = projects();
            let editor = WindowId::new(1, 1);
            let owner = reactor.test_window_server_id(editor);
            let picker = open_picker(&mut reactor, 3, "Fuzzy Finder", Some(owner));
            let mut dragged = frame(&reactor, picker);
            dragged.origin.x += 26.;
            reactor.handle_event(Event::WindowFrameChanged(
                picker,
                dragged,
                None,
                Requested(false),
                Some(MouseState::Down),
            ));
            reactor.handle_event(Event::MouseUp(MouseButton::Left));
            dragged.origin.x += 45.;
            if animation_readback_first {
                reactor.handle_event(Event::WindowFrameChanged(
                    picker,
                    dragged,
                    None,
                    Requested(true),
                    Some(MouseState::Up),
                ));
            }
            reactor.handle_event(Event::WindowFrameChanged(
                picker,
                dragged,
                None,
                Requested(false),
                Some(MouseState::Up),
            ));

            let editor_start = frame(&reactor, editor);
            shift(&mut reactor, space, editor, 120.);
            let editor_dx = frame(&reactor, editor).origin.x - editor_start.origin.x;
            assert!(editor_dx.abs() > 50.);
            let mut expected = dragged;
            expected.origin.x += editor_dx;
            assert_eq!(
                frame(&reactor, picker),
                expected,
                "readback first: {animation_readback_first}"
            );
            shift(&mut reactor, space, editor, -editor_dx);
            assert_eq!(frame(&reactor, editor), editor_start);
            assert_eq!(frame(&reactor, picker), dragged);
            window_server::set_window_parent_override(WindowServerId::new(3), None);
        }
    }

    #[test]
    fn two_projects_keep_separate_pickers_after_focus_and_close() {
        let (mut reactor, space, mut apps) = projects();
        apps.make_app_and_settle(&mut reactor, 2, make_windows(1));
        let first = WindowId::new(1, 1);
        let second = WindowId::new(1, 2);
        let other_app = WindowId::new(2, 1);
        reactor.handle_event(Event::WindowServerFocusChanged(first, space));
        let finder = open_picker(&mut reactor, 3, "Fuzzy Finder", None);
        reactor.handle_event(Event::WindowServerFocusChanged(second, space));
        reactor.handle_event(Event::WindowServerFocusChanged(other_app, space));
        let grep = open_picker(&mut reactor, 4, "Live Grep", None);
        let finder_frame = frame(&reactor, finder);
        let mut grep_frame = frame(&reactor, grep);
        let grep_offset = grep_frame.origin.x - frame(&reactor, second).origin.x;

        for (owner, dx, finder_dx, grep_dx) in [
            (other_app, 30., 0., 0.),
            (second, -70., 0., -70.),
            (first, 45., 45., -70.),
        ] {
            shift(&mut reactor, space, owner, dx);
            assert_eq!(
                frame(&reactor, finder).origin.x,
                finder_frame.origin.x + finder_dx
            );
            assert_eq!(frame(&reactor, grep).origin.x, grep_frame.origin.x + grep_dx);
        }

        reactor.handle_event(Event::WindowDestroyed(finder));
        reactor.handle_event(Event::WindowDestroyed(first));
        shift(&mut reactor, space, second, 80.);
        grep_frame.origin.x = frame(&reactor, second).origin.x + grep_offset;
        assert_eq!(frame(&reactor, grep), grep_frame);
        for idx in [3, 4] {
            window_server::set_window_parent_override(WindowServerId::new(idx), None);
        }
    }

    #[test]
    fn native_parent_discovered_after_picker_supersedes_focused_project() {
        let (mut reactor, space, _apps) = projects();
        let focused = WindowId::new(1, 1);
        reactor.handle_event(Event::WindowServerFocusChanged(focused, space));
        let owner_info = make_window(5);
        let dialog = open_picker(&mut reactor, 3, "Live Grep", owner_info.sys_id);
        let mut expected = frame(&reactor, dialog);
        shift(&mut reactor, space, focused, 75.);
        assert_eq!(frame(&reactor, dialog), expected, "wait for the named parent");

        let owner = WindowId::new(1, 5);
        reactor.handle_event(Event::WindowCreated(owner, owner_info, None, None));
        expected = frame(&reactor, dialog);
        shift(&mut reactor, space, owner, -35.);
        expected.origin.x -= 35.;
        assert_eq!(frame(&reactor, dialog), expected);
        window_server::set_window_parent_override(WindowServerId::new(3), None);
    }

    #[test]
    fn foreign_or_inactive_native_parent_does_not_bind_picker_to_active_editor() {
        let (mut reactor, space, mut apps) = projects();
        apps.make_app_and_settle(&mut reactor, 2, make_windows(1));
        let active = WindowId::new(1, 1);
        let hidden = WindowId::new(1, 2);
        let foreign = WindowId::new(2, 1);
        let workspace = reactor.test_workspace(space, 1);
        assert!(reactor.assign_test_window_to_workspace(space, hidden, workspace));
        reactor.handle_event(Event::WindowServerFocusChanged(active, space));

        for (idx, parent) in [(3, foreign), (4, hidden)] {
            let native = reactor.test_window_server_id(parent);
            let dialog = open_picker(&mut reactor, idx, "Live Grep", Some(native));
            let expected = frame(&reactor, dialog);
            shift(&mut reactor, space, active, 40.);
            shift(&mut reactor, space, parent, -35.);
            assert_eq!(frame(&reactor, dialog), expected);
            reactor.handle_event(Event::WindowDestroyed(dialog));
            window_server::set_window_parent_override(WindowServerId::new(idx), None);
        }
    }

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

    #[test]
    fn an_existing_picker_follows_its_new_native_parent_on_order_in() {
        let (mut reactor, space, _apps) = projects();
        let first = WindowId::new(1, 1);
        let second = WindowId::new(1, 2);
        reactor.handle_event(Event::WindowServerFocusChanged(first, space));
        let dialog = open_picker(&mut reactor, 3, "Live Grep", None);
        let native_dialog = reactor.test_window_server_id(dialog);

        for (previous, owner) in [(first, second), (second, first)] {
            let before = frame(&reactor, dialog);
            let native_owner = reactor.test_window_server_id(owner);
            window_server::set_window_parent_override(native_dialog, Some(Some(native_owner)));
            reactor.handle_event(Event::WindowServerUnhidden(native_dialog));
            shift(&mut reactor, space, previous, 40.);
            assert_eq!(
                frame(&reactor, dialog),
                before,
                "picker moved with its previous editor"
            );
            shift(&mut reactor, space, owner, 80.);
            assert_eq!(frame(&reactor, dialog).origin.x, before.origin.x + 80.);
        }
        window_server::set_window_parent_override(native_dialog, None);
    }
}
