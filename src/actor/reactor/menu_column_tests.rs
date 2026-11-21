// Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
use objc2_core_foundation::CGSize;

use super::testing::*;
use super::*;
use crate::common::config::LayoutMode;
use crate::layout_engine::{ColumnId, ColumnPlacement, ColumnReorderRequest, LayoutSystem};

fn two_displays() -> (Reactor, SpaceId, SpaceId) {
    let mut reactor = test_reactor();
    let left = SpaceId::new(1);
    let right = SpaceId::new(2);
    let size = CGSize::new(1200.0, 800.0);
    reactor.handle_event(space_state_event(
        vec![
            CGRect::new(CGPoint::ZERO, size),
            CGRect::new(CGPoint::new(1200.0, 0.0), size),
        ],
        vec![Some(left), Some(right)],
    ));
    reactor.space_state.command_space = Some(left);
    (reactor, left, right)
}

#[test]
fn menu_column_drop_on_unfocused_display_arranges_only_its_space_without_focus_effects() {
    let (mut reactor, left, right) = two_displays();
    let engine = &mut reactor.layout_manager.layout_engine;
    let workspace = engine.workspaces().active_workspace(right).unwrap();
    assert_eq!(engine.active_layout_mode_at(right), LayoutMode::Scrolling);
    for index in 1..=3 {
        let _ = engine.handle_event(
            &mut reactor.state.windows,
            layout::LayoutEvent::WindowAdded(right, WindowId::new(2, index)),
        );
    }
    let _ = engine.handle_event(
        &mut reactor.state.windows,
        layout::LayoutEvent::WindowAdded(left, WindowId::new(1, 1)),
    );
    let _ = engine.handle_event(
        &mut reactor.state.windows,
        layout::LayoutEvent::WindowFocused(left, WindowId::new(1, 1)),
    );
    let ws = &engine.workspaces()[workspace];
    let tree = ws.layout_system.container_tree(ws.active_layout().unwrap());
    let members = |column: &wm_protocol::ContainerTreeNode| {
        column
            .children
            .iter()
            .map(|window| {
                let window = window.window_id.unwrap();
                WindowId::new(window.pid, window.idx)
            })
            .collect()
    };
    let mut expected = ws.layout_system.all_windows_in_layout(ws.active_layout().unwrap());
    expected.rotate_left(1);
    let request = ColumnReorderRequest {
        space: right,
        workspace,
        source: ColumnId(tree.children[0].node_id),
        target: ColumnId(tree.children[2].node_id),
        source_members: members(&tree.children[0]),
        target_members: members(&tree.children[2]),
        placement: ColumnPlacement::After,
    };
    let focus = engine.focused_window();
    assert_eq!(focus, Some(WindowId::new(1, 1)));
    assert_eq!(engine.workspaces().workspace_space(workspace), Some(right));
    assert_eq!(engine.workspaces().active_workspace(right), Some(workspace));
    assert!(reactor.is_space_active(right));
    let outcome = reactor.dispatch_workflow(Event::MenuBarReorderColumn(request)).unwrap();
    assert_eq!(outcome.arrange.space_scope, Some(right));
    assert_eq!(outcome.arrange.passes, 1);
    assert!(outcome.layout_responses.is_empty());
    assert!(outcome.raise_requests.is_empty());
    assert!(outcome.make_key_windows.is_empty());
    assert!(outcome.mouse_warps.is_empty());
    assert!(outcome.post_arrange_mouse_warp.is_none());
    assert!(outcome.display_switch_warp.is_none());
    assert_eq!(reactor.layout_manager.layout_engine.focused_window(), focus);
    assert_eq!(reactor.space_state.command_space, Some(left));
    let ws = &reactor.layout_manager.layout_engine.workspaces()[workspace];
    assert_eq!(
        ws.layout_system.all_windows_in_layout(ws.active_layout().unwrap()),
        expected
    );
}

#[test]
fn menu_layout_command_uses_origin_display_and_rejects_hidden_native_space() {
    let (mut reactor, left, right) = two_displays();
    let left_workspace = reactor.layout_manager.layout_engine.workspaces().active_workspace(left);
    let right_workspace = reactor.layout_manager.layout_engine.workspaces().active_workspace(right);
    let outcome = reactor
        .dispatch_workflow(Event::MenuBarLayoutCommand {
            space: right,
            command: layout::LayoutCommand::NextWorkspace(None),
        })
        .unwrap();
    assert_eq!(outcome.arrange.space_scope, Some(right));
    assert_eq!(
        reactor.layout_manager.layout_engine.workspaces().active_workspace(left),
        left_workspace
    );
    assert_ne!(
        reactor.layout_manager.layout_engine.workspaces().active_workspace(right),
        right_workspace
    );
    let outcome = reactor
        .dispatch_workflow(Event::MenuBarLayoutCommand {
            space: SpaceId::new(999),
            command: layout::LayoutCommand::NextWorkspace(None),
        })
        .unwrap();
    assert_eq!(outcome.arrange.passes, 0);
}

fn active_menu_windows(_mode: LayoutMode, animated: bool) -> (Apps, Reactor, SpaceId) {
    let (mut apps, mut reactor) = test_context();
    let space = SpaceId::new(1);
    reactor.config.settings.layout.scrolling.column_width_ratio = 1.0;
    reactor.config.settings.layout.scrolling.animate = Some(animated);
    reactor.config.settings.layout.scrolling.preserve_window_sizes = false;
    reactor
        .layout_manager
        .layout_engine
        .set_layout_settings(&reactor.config.settings.layout);
    reactor.handle_event(space_state_event(
        vec![CGRect::new(CGPoint::ZERO, CGSize::new(1000.0, 1000.0))],
        vec![Some(space)],
    ));
    let focused = WindowId::new(1, 2);
    reactor.handle_events(apps.make_app_with_opts(1, make_windows(3), Some(focused), true, true));
    reactor.handle_event(Event::ApplicationGloballyActivated(1));
    reactor.handle_event(Event::WindowServerFocusChanged(focused, space));
    apps.simulate_until_quiet(&mut reactor);
    (apps, reactor, space)
}

#[test]
fn menu_column_icon_click_warps_to_revealed_window_and_repeated_click_still_warps() {
    for mode in [LayoutMode::Scrolling] {
        for animated in [false, true] {
            for enabled in [false, true] {
                let (_apps, mut reactor, _space) = active_menu_windows(mode, animated);
                let target = WindowId::new(1, 1);
                if mode == LayoutMode::Scrolling {
                    assert!(reactor.window_center_on_known_screen(target).is_none());
                }
                reactor.config.settings.mouse_follows_focus = enabled;
                let (animation_tx, _animation_rx) = super::animation::AnimationSender::channel();
                reactor.animation_tx = Some(animation_tx);
                let (input_tx, mut input_rx) = crate::actor::channel();
                reactor.communication_manager.input_tx = Some(input_tx);
                let (raise_tx, mut raise_rx) = crate::actor::channel();
                reactor.communication_manager.raise_manager_tx = raise_tx;
                for _ in 0..2 {
                    reactor.handle_event(Event::MenuBarFocusWindow(target));
                    let warps: Vec<_> = std::iter::from_fn(|| input_rx.try_recv().ok())
                        .filter_map(|(_, request)| match request {
                            input::Request::Warp(point) => Some(point),
                            input::Request::FocusPointer(focus) => Some(focus.visible.mid()),
                            _ => None,
                        })
                        .collect();
                    let center = if mode == LayoutMode::Scrolling {
                        CGPoint::new(450.0, 500.0)
                    } else {
                        reactor.state.windows.window(target).unwrap().frame_monotonic.mid()
                    };
                    assert_eq!(
                        warps,
                        vec![center],
                        "{mode:?}, animated={animated}, enabled={enabled}"
                    );
                    let raised: Vec<_> = std::iter::from_fn(|| raise_rx.try_recv().ok())
                        .filter_map(|(_, event)| match event {
                            raise_manager::Event::RaiseRequest(request) => request.focus_window,
                            _ => None,
                        })
                        .collect();
                    assert!(!raised.is_empty());
                    assert!(raised.iter().all(|&(window, _)| window == target));
                    assert_eq!(
                        reactor.layout_manager.layout_engine.focused_window(),
                        Some(target)
                    );
                }
            }
        }
    }
}

#[test]
fn explicit_focus_transition_ignores_passing_windows_until_motion_finishes() {
    let target = WindowId::new(1, 1);
    for mouse_follows_focus in [true, false] {
        for (request, destination) in [
            (Event::MenuBarFocusWindow(target), target),
            (
                Event::Command(Command::Reactor(ReactorCommand::FocusWindow {
                    window_id: target.into(),
                    window_server_id: None,
                })),
                target,
            ),
            (
                Event::Command(Command::Layout(layout::LayoutCommand::MoveFocus(
                    Direction::Left,
                ))),
                WindowId::new(1, 3),
            ),
        ] {
            assert_explicit_focus_preserved_during_motion(
                request,
                destination,
                mouse_follows_focus,
            );
        }
    }
}

fn assert_explicit_focus_preserved_during_motion(
    request: Event,
    target: WindowId,
    mouse_follows_focus: bool,
) {
    let (_apps, mut reactor, _space) = active_menu_windows(LayoutMode::Scrolling, true);
    reactor.config.settings.mouse_follows_focus = mouse_follows_focus;
    let passing = WindowId::new(1, 2);
    let (animation_tx, animation_rx) = super::animation::AnimationSender::channel();
    reactor.animation_tx = Some(animation_tx);
    let (input_tx, _input_rx) = crate::actor::channel();
    reactor.communication_manager.input_tx = Some(input_tx);
    let (raise_tx, mut raise_rx) = crate::actor::channel();
    reactor.communication_manager.raise_manager_tx = raise_tx;

    let request_name = format!("{request:?}");
    reactor.handle_event(request);
    assert_eq!(
        reactor.layout_manager.layout_engine.focused_window(),
        Some(target)
    );
    let mut camera = animation_rx
        .commands
        .try_iter()
        .find_map(|message| match message {
            super::animation::Message::Camera(camera) => Some(camera),
            _ => None,
        })
        .expect("explicit focus must animate the viewport");
    while raise_rx.try_recv().is_ok() {}
    reactor.handle_loop_event(Event::MouseMoved(reactor.test_window_server_id(passing)));
    assert_eq!(
        reactor.layout_manager.layout_engine.focused_window(),
        Some(target),
        "motion must not replace the destination ({request_name}, mouse_follows_focus={mouse_follows_focus})"
    );
    assert!(
        raise_rx.try_recv().is_err(),
        "passing windows must not request focus"
    );

    camera.sample(Instant::now() + Duration::from_secs(1));
    reactor.handle_loop_event(Event::CameraFinished);
    reactor.handle_loop_event(Event::MouseMoved(reactor.test_window_server_id(passing)));
    assert_eq!(
        reactor.layout_manager.layout_engine.focused_window(),
        Some(passing),
        "fresh motion must resume hover after the presentation completes"
    );
}

#[test]
fn menu_focus_transition_rejects_motion_queued_before_completion() {
    let (_apps, mut reactor, _space) = active_menu_windows(LayoutMode::Scrolling, true);
    let target = WindowId::new(1, 1);
    let passing = WindowId::new(1, 2);
    let (animation_tx, animation_rx) = super::animation::AnimationSender::channel();
    reactor.animation_tx = Some(animation_tx);
    let (input_tx, _input_rx) = crate::actor::channel();
    reactor.communication_manager.input_tx = Some(input_tx);
    let (raise_tx, mut raise_rx) = crate::actor::channel();
    reactor.communication_manager.raise_manager_tx = raise_tx;
    reactor.handle_event(Event::MenuBarFocusWindow(target));
    let mut camera = animation_rx
        .commands
        .try_iter()
        .find_map(|message| match message {
            super::animation::Message::Camera(camera) => Some(camera),
            _ => None,
        })
        .unwrap();
    while raise_rx.try_recv().is_ok() {}
    let publisher = MouseFocusPublisher::default();
    let (sender, mut receiver) = crate::actor::channel();
    publisher
        .publish(&sender, crate::sys::event::hid_event_timestamp_now())
        .unwrap();
    let reactor = Rc::new(RefCell::new(reactor));
    Reactor::handle_thread_event(&reactor, receiver.try_recv().unwrap().1);
    assert_eq!(
        publisher.take_latest(),
        None,
        "motion during travel must be consumed"
    );

    publisher
        .publish(&sender, crate::sys::event::hid_event_timestamp_now())
        .unwrap();
    camera.sample(Instant::now() + Duration::from_secs(1));
    Reactor::handle_thread_event(&reactor, Event::CameraFinished);
    window_server::set_window_under_cursor_override(Some(Some(
        reactor.borrow().test_window_server_id(passing),
    )));
    Reactor::handle_thread_event(&reactor, receiver.try_recv().unwrap().1);
    assert_eq!(
        reactor.borrow().layout_manager.layout_engine.focused_window(),
        Some(target)
    );
    assert!(raise_rx.try_recv().is_err());

    let after_completion = reactor.borrow().mouse_focus_warp_requested_at.unwrap() + 1;
    publisher.publish(&sender, after_completion).unwrap();
    Reactor::handle_thread_event(&reactor, receiver.try_recv().unwrap().1);
    window_server::set_window_under_cursor_override(None);
    assert_eq!(
        reactor.borrow().layout_manager.layout_engine.focused_window(),
        Some(passing)
    );
}

#[test]
fn menu_focus_transition_survives_retarget_and_releases_on_cancellation() {
    let (_apps, mut reactor, space) = active_menu_windows(LayoutMode::Scrolling, true);
    let target = WindowId::new(1, 1);
    let retarget = WindowId::new(1, 3);
    let passing = WindowId::new(1, 2);
    let (animation_tx, animation_rx) = super::animation::AnimationSender::channel();
    reactor.animation_tx = Some(animation_tx);
    let (input_tx, mut input_rx) = crate::actor::channel();
    reactor.communication_manager.input_tx = Some(input_tx);
    let (raise_tx, mut raise_rx) = crate::actor::channel();
    reactor.communication_manager.raise_manager_tx = raise_tx;
    let old_hover = reactor.mouse_raise_token.clone();
    reactor.handle_event(Event::MenuBarFocusWindow(target));
    assert!(
        old_hover.is_cancelled(),
        "focus travel must revoke older hover work"
    );
    let mut old_camera = animation_rx
        .commands
        .try_iter()
        .find_map(|message| match message {
            super::animation::Message::Camera(camera) => Some(camera),
            _ => None,
        })
        .unwrap();
    reactor.handle_event(Event::MenuBarFocusWindow(retarget));
    old_camera.sample(Instant::now() + Duration::from_secs(1));
    reactor.handle_loop_event(Event::CameraFinished);
    while raise_rx.try_recv().is_ok() {}
    reactor.handle_loop_event(Event::MouseMoved(reactor.test_window_server_id(passing)));
    assert_eq!(
        reactor.layout_manager.layout_engine.focused_window(),
        Some(retarget)
    );
    assert!(
        raise_rx.try_recv().is_err(),
        "old completion cannot unlock a replacement camera"
    );
    assert!(reactor.presentations_hold_hover());
    assert!(
        std::iter::from_fn(|| input_rx.try_recv().ok()).any(|(_, request)| {
            matches!(request, input::Request::SetFocusFollowsMouseEnabled(false))
        }),
        "focus travel must pause hover sampling"
    );

    reactor.stop_camera(space);
    assert!(!reactor.presentations_hold_hover());
    assert!(
        std::iter::from_fn(|| input_rx.try_recv().ok()).any(|(_, request)| {
            matches!(request, input::Request::SetFocusFollowsMouseEnabled(true))
        }),
        "cancelling the presentation must restore hover sampling"
    );
    reactor.handle_loop_event(Event::MouseMoved(reactor.test_window_server_id(passing)));
    assert_eq!(
        reactor.layout_manager.layout_engine.focused_window(),
        Some(passing)
    );
}

#[test]
fn menu_focus_transition_has_no_cooldown_when_animation_is_disabled() {
    for reduce_motion in [false, true] {
        let (_apps, mut reactor, _space) =
            active_menu_windows(LayoutMode::Scrolling, reduce_motion);
        reactor.reduce_motion = reduce_motion;
        let target = WindowId::new(1, 1);
        let passing = WindowId::new(1, 2);
        let (animation_tx, animation_rx) = super::animation::AnimationSender::channel();
        reactor.animation_tx = Some(animation_tx);
        let (input_tx, _input_rx) = crate::actor::channel();
        reactor.communication_manager.input_tx = Some(input_tx);
        reactor.handle_event(Event::MenuBarFocusWindow(target));
        let mut camera = animation_rx
            .commands
            .try_iter()
            .find_map(|message| match message {
                super::animation::Message::Camera(camera) => Some(camera),
                _ => None,
            })
            .unwrap();
        camera.sample(Instant::now());
        reactor.handle_loop_event(Event::CameraFinished);
        assert!(!reactor.presentations_hold_hover());
        reactor.handle_loop_event(Event::MouseMoved(reactor.test_window_server_id(passing)));
        assert_eq!(
            reactor.layout_manager.layout_engine.focused_window(),
            Some(passing)
        );
    }
}

#[test]
fn pointer_driven_focus_does_not_hold_hover_during_its_animation() {
    let (_apps, mut reactor, _space) = active_menu_windows(LayoutMode::Scrolling, true);
    let target = WindowId::new(1, 1);
    reactor.config.settings.mouse_follows_focus = true;
    let (animation_tx, _animation_rx) = super::animation::AnimationSender::channel();
    reactor.animation_tx = Some(animation_tx);
    let (input_tx, mut input_rx) = crate::actor::channel();
    reactor.communication_manager.input_tx = Some(input_tx);
    reactor.handle_loop_event(Event::MouseMoved(reactor.test_window_server_id(target)));
    assert!(!reactor.presentations_hold_hover());
    assert!(
        !std::iter::from_fn(|| input_rx.try_recv().ok()).any(|(_, request)| {
            matches!(request, input::Request::SetFocusFollowsMouseEnabled(false))
        }),
        "ordinary pointer focus must remain responsive"
    );
}

#[test]
fn menu_column_stale_icon_click_does_not_focus_or_warp() {
    for hidden_workspace in [false, true] {
        let (_apps, mut reactor, _space) = active_menu_windows(LayoutMode::Scrolling, false);
        let target = if hidden_workspace {
            reactor.handle_test_layout_command(layout::LayoutCommand::SwitchToWorkspace(1));
            WindowId::new(1, 1)
        } else {
            WindowId::new(99, 99)
        };
        let focus = reactor.layout_manager.layout_engine.focused_window();
        let (input_tx, mut input_rx) = crate::actor::channel();
        reactor.communication_manager.input_tx = Some(input_tx);
        let (raise_tx, mut raise_rx) = crate::actor::channel();
        reactor.communication_manager.raise_manager_tx = raise_tx;
        reactor.handle_event(Event::MenuBarFocusWindow(target));
        assert!(input_rx.try_recv().is_err());
        assert!(raise_rx.try_recv().is_err());
        assert_eq!(reactor.layout_manager.layout_engine.focused_window(), focus);
    }
}

#[test]
fn menu_column_actual_drop_keeps_pointer_and_focused_window() {
    let (_apps, mut reactor, space) = active_menu_windows(LayoutMode::Scrolling, false);
    let engine = &reactor.layout_manager.layout_engine;
    let workspace = engine.workspaces().active_workspace(space).unwrap();
    let ws = &engine.workspaces()[workspace];
    let layout = ws.active_layout().unwrap();
    let tree = ws.layout_system.container_tree(layout);
    let members = |column: &wm_protocol::ContainerTreeNode| {
        column
            .children
            .iter()
            .map(|node| {
                let window = node.window_id.unwrap();
                WindowId::new(window.pid, window.idx)
            })
            .collect()
    };
    let mut expected = ws.layout_system.all_windows_in_layout(layout);
    expected.rotate_left(1);
    let request = ColumnReorderRequest {
        space,
        workspace,
        source: ColumnId(tree.children[0].node_id),
        target: ColumnId(tree.children[2].node_id),
        source_members: members(&tree.children[0]),
        target_members: members(&tree.children[2]),
        placement: ColumnPlacement::After,
    };
    let focus = engine.focused_window();
    let (input_tx, mut input_rx) = crate::actor::channel();
    reactor.communication_manager.input_tx = Some(input_tx);
    let (raise_tx, mut raise_rx) = crate::actor::channel();
    reactor.communication_manager.raise_manager_tx = raise_tx;
    reactor.handle_event(Event::MenuBarReorderColumn(request));
    assert!(
        !std::iter::from_fn(|| input_rx.try_recv().ok()).any(|(_, request)| {
            matches!(
                request,
                input::Request::Warp(_) | input::Request::WarpRelative { .. }
            )
        })
    );
    assert!(!std::iter::from_fn(|| raise_rx.try_recv().ok()).any(|(_, event)| {
        matches!(event, raise_manager::Event::RaiseRequest(request) if request.focus_window.is_some())
    }));
    let engine = &reactor.layout_manager.layout_engine;
    assert_eq!(engine.focused_window(), focus);
    let ws = &engine.workspaces()[workspace];
    assert_eq!(
        ws.layout_system.all_windows_in_layout(ws.active_layout().unwrap()),
        expected
    );
}
