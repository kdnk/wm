use objc2_core_foundation::{CGPoint, CGRect, CGSize};

use super::*;
use crate::actor::app::WindowId;
use crate::common::collections::HashMap;
use crate::common::config::GapSettings;
use crate::layout_engine::systems::WindowLayoutConstraints;

const ACTIONS: [WindowDropAction; 6] = [
    WindowDropAction::Swap,
    WindowDropAction::Stack,
    WindowDropAction::Insert(Direction::Left),
    WindowDropAction::Insert(Direction::Right),
    WindowDropAction::Insert(Direction::Up),
    WindowDropAction::Insert(Direction::Down),
];

fn w(idx: u32) -> WindowId { WindowId::new(1, idx) }

fn screen() -> CGRect { CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(1200.0, 800.0)) }

fn calculated_source<S: LayoutSystem>(system: &S, layout: LayoutId, source: WindowId) -> CGRect {
    system
        .calculate_layout(
            layout,
            screen(),
            0.0,
            &HashMap::<WindowId, WindowLayoutConstraints>::default(),
            &GapSettings::default(),
            Default::default(),
        )
        .into_iter()
        .find_map(|(window, frame)| (window == source).then_some(frame))
        .expect("source frame after successful drop")
}

fn verify_actions(
    baseline: &LayoutSystemKind,
    layout: LayoutId,
    source: WindowId,
    target: WindowId,
) {
    let before = format!("{baseline:?}");
    for action in ACTIONS {
        let mut preview_copy = baseline.preview_clone().unwrap();
        let mut committed = baseline.preview_clone().unwrap();
        let preview_available = preview_copy.apply_window_drop(layout, source, target, action);
        assert_eq!(
            preview_available,
            committed.apply_window_drop(layout, source, target, action),
            "availability differs for {action:?}"
        );
        if !preview_available {
            continue;
        }
        assert_eq!(
            calculated_source(&preview_copy, layout, source),
            calculated_source(&committed, layout, source),
            "preview differs for {action:?}"
        );
    }
    assert_eq!(format!("{baseline:?}"), before, "preview mutated live layout");
}

fn populate(system: &mut LayoutSystemKind) -> LayoutId {
    let layout = system.create_layout();
    for window in [w(1), w(2), w(3), w(4)] {
        system.add_window_after_selection(layout, window);
    }
    layout
}

#[test]
fn previews_match_committed_frames_for_every_layout_and_action() {
    for mut system in [
        LayoutSystemKind::Scrolling(ScrollingLayoutSystem::default()),
    ] {
        let layout = populate(&mut system);
        verify_actions(&system, layout, w(4), w(2));
    }
}





#[test]
fn move_drop_matches_keyboard_move_without_mutating_preview_source() {
    for mut system in [
        LayoutSystemKind::Scrolling(ScrollingLayoutSystem::default()),
    ] {
        let layout = populate(&mut system);
        let before = format!("{system:?}");
        let mut preview = system.preview_clone().unwrap();
        let mut keyboard = system.preview_clone().unwrap();
        let moved =
            preview.apply_window_drop(layout, w(2), w(2), WindowDropAction::Move(Direction::Right));
        assert!(keyboard.select_window(layout, w(2)));
        assert_eq!(moved, keyboard.move_selection(layout, Direction::Right));
        assert_eq!(format!("{preview:?}"), format!("{keyboard:?}"));
        assert_eq!(format!("{system:?}"), before);
    }
}







#[test]
fn same_slot_edges_use_the_configured_center_action() {
    use crate::common::config::MouseDropAction;
    use crate::layout_engine::engine::DropPreview;

    let same_slot = DropPreview {
        frame: screen(),
        same_slot: true,
    };
    let insert = WindowDropAction::Insert(Direction::Right);
    assert_eq!(
        same_slot.action(insert, MouseDropAction::Swap),
        WindowDropAction::Swap
    );
    assert_eq!(
        same_slot.action(insert, MouseDropAction::Stack),
        WindowDropAction::Stack
    );
    assert_eq!(
        DropPreview {
            frame: screen(),
            same_slot: false
        }
        .action(insert, MouseDropAction::Swap),
        insert
    );
}
