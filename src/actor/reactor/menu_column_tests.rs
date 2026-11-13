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
    let response = engine.handle_virtual_workspace_command(
        &mut reactor.state.windows,
        right,
        &layout::LayoutCommand::SetWorkspaceLayout {
            workspace: None,
            mode: LayoutMode::Scrolling,
        },
    );
    assert!(response.changed);
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

fn active_menu_windows(mode: LayoutMode, animated: bool) -> (Apps, Reactor, SpaceId) {
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
    reactor.handle_test_layout_command(layout::LayoutCommand::SetWorkspaceLayout {
        workspace: None,
        mode,
    });
    let focused = WindowId::new(1, 2);
    reactor.handle_events(apps.make_app_with_opts(1, make_windows(3), Some(focused), true, true));
    reactor.handle_event(Event::ApplicationGloballyActivated(1));
    reactor.send_layout_event(layout::LayoutEvent::WindowFocused(space, focused));
    apps.simulate_until_quiet(&mut reactor);
    (apps, reactor, space)
}

#[test]
fn menu_column_icon_click_warps_to_revealed_window_and_repeated_click_still_warps() {
    for mode in [LayoutMode::Traditional, LayoutMode::Scrolling] {
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
