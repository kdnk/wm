// Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
use tracing::trace;

use crate::actor::reactor::events::EventOutcome;
use crate::actor::reactor::managers::{DragManager, LayoutManager};
use crate::actor::reactor::{LayoutEvent, Reactor};
use crate::model::WmState;
use crate::sys::screen::SpaceId;

pub fn build_drag_scene(
    state: &WmState,
    layout: &LayoutManager,
    source: crate::actor::app::WindowId,
    space: SpaceId,
) -> crate::actor::drag::DragScene {
    crate::actor::drag::DragScene {
        action_override: layout.layout_engine.drop_action_override(space),
        source_neighbors: layout.layout_engine.source_move_neighbors(space, source),
        targets: layout
            .layout_engine
            .drop_scene_windows(space, source)
            .into_iter()
            .filter(|window| *window != source)
            .filter_map(|window| {
                Some(crate::actor::drag::DragSceneTarget {
                    window,
                    space,
                    frame: state.windows.window(window)?.frame_monotonic,
                })
            })
            .collect(),
    }
}

#[derive(Debug, Clone)]
pub struct MouseUpPayload {
    pub button: crate::actor::drag::MouseButton,
    pub final_space: Option<SpaceId>,
}

pub fn handle_cancel(drag: &mut DragManager) -> EventOutcome {
    let cancelled = drag.actor.cancel();
    drag.release_preview();
    drag.externally_controlled_window = None;
    let Some(cancelled) = cancelled else {
        return EventOutcome::no_change();
    };
    let source = cancelled.source;
    match (cancelled.kind, source.tiled) {
        (crate::actor::drag::DragKind::ModifierMove, true) => EventOutcome::layout_changed(false),
        (crate::actor::drag::DragKind::ModifierMove, false)
            if source.last_frame != source.origin_frame =>
        {
            EventOutcome::no_change().with_pre_layout_window_frame_write(
                source.window,
                source.origin_frame,
                true,
            )
        }
        _ => EventOutcome::no_change(),
    }
}

pub fn handle_mouse_up(
    reactor: &mut Reactor,
    payload: MouseUpPayload,
) -> anyhow::Result<EventOutcome> {
    let state = &mut reactor.state;
    let layout = &mut reactor.layout_manager;
    let drag = &mut reactor.drag_manager;
    let mut outcome = EventOutcome::layout_changed(false);
    let Some(commit) = drag.actor.finish(payload.button) else {
        return Ok(outcome);
    };
    let window = commit.source.window;
    drag.externally_controlled_window = None;
    let mut needs_layout = commit.source.tiled;

    if let Some(target) = commit.target
        && payload.final_space == Some(target.intent.space)
        && state.windows.contains_window(window)
        && state.windows.contains_window(target.intent.window)
    {
        trace!(source=?window, target=?target.intent.window, action=?target.intent.action, "performing window drop");
        if layout.layout_engine.apply_window_drop(crate::layout_engine::WindowDropRequest {
            source: window,
            target: target.intent.window,
            space: target.intent.space,
            action: target.intent.action,
        }) {
            needs_layout = true;
            outcome.drop_haptic = drag.haptics_enabled;
            outcome.finish_drop_preview = true;
        }
    }

    if !outcome.finish_drop_preview {
        drag.release_preview();
    }

    let transfer_pending = state
        .windows
        .record(window)
        .is_some_and(|record| record.pending_transfer().is_some());
    let source = state
        .windows
        .workspace_for_window(window)
        .and_then(|workspace| layout.layout_engine.workspaces().workspace_space(workspace))
        .or(commit.source.origin_space);
    let native_on_other_display = payload.final_space.is_some_and(|space| {
        state.windows.native_space(window).is_some_and(|native| native != space)
    });
    let needs_placement = commit.source.origin_frame != commit.source.last_frame
        && (source != payload.final_space || transfer_pending || native_on_other_display);
    if commit.source.origin_space != payload.final_space || needs_placement {
        let Some(space) = payload.final_space.filter(|_| state.windows.is_admitted(window)) else {
            if source.is_some() {
                outcome = outcome.with_layout_event(LayoutEvent::WindowRemoved(window));
            }
            return Ok(outcome.with_arrange_passes(1));
        };
        let Some(screen) = reactor.space_state.screen_by_space(space) else {
            return Ok(outcome.with_arrange_passes(1));
        };
        let engine = &mut layout.layout_engine;
        if engine.workspaces().active_workspace(space).is_none() {
            return Ok(outcome.with_arrange_passes(1));
        }
        if let Some(source) = source.filter(|&source| source != space) {
            if !engine
                .move_window_to_space(&mut state.windows, source, space, screen.frame.size, window)
                .changed
            {
                return Ok(outcome.with_arrange_passes(1));
            }
        } else if source.is_none() {
            let _ =
                engine.handle_event(&mut state.windows, LayoutEvent::WindowAdded(space, window));
            if engine
                .workspaces()
                .workspace_for_window(&state.windows, space, window)
                .is_none()
            {
                return Ok(outcome.with_arrange_passes(1));
            }
        }
        state.windows.cancel_window_transfer(window);
        let placement = if engine.is_window_floating(window) {
            state.windows.clear_floating_display_geometry(window);
            super::command::request_window_transfer(
                state,
                layout,
                window,
                space,
                Some(commit.source.last_frame),
                true,
            )
        } else {
            reactor.request_window_transfer(window, space, None)
        };
        outcome.absorb(placement);
        reactor.request_space_snapshot();
        return Ok(outcome.with_arrange_passes(1));
    }
    if let Some(space) = payload.final_space
        && layout.layout_engine.is_window_floating(window)
    {
        state.windows.clear_floating_display_geometry(window);
        if let Some(workspace) = layout
            .layout_engine
            .workspaces()
            .workspace_for_window(&state.windows, space, window)
            .or_else(|| layout.layout_engine.workspaces().active_workspace(space))
        {
            layout.layout_engine.store_floating_position(
                workspace,
                window,
                commit.source.last_frame,
            );
        }
    }

    Ok(outcome.with_arrange_passes(u8::from(needs_layout)))
}

#[cfg(test)]
mod tests {
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};

    use super::*;
    use crate::actor::app::WindowId;
    use crate::actor::drag::{DragMotion, DragScene, DragSceneTarget, DragSource, MouseButton};
    use crate::actor::reactor::testing::{make_windows, test_context};

    fn drop_outcome(target_exists: bool) -> EventOutcome {
        let (mut apps, mut reactor) = test_context();
        let screen = CGRect::new(CGPoint::ZERO, CGSize::new(1000.0, 1000.0));
        let space = SpaceId::new(1);
        apps.make_app_and_settle_on_screen(&mut reactor, screen, space, 1, make_windows(2));
        let source = WindowId::new(1, 1);
        let target = WindowId::new(1, 2);
        let source_frame = reactor.state.windows.window(source).unwrap().frame_monotonic;
        let target_frame = reactor.state.windows.window(target).unwrap().frame_monotonic;
        let target = if target_exists {
            target
        } else {
            WindowId::new(99, 1)
        };
        reactor.drag_manager.haptics_enabled = false;
        reactor.drag_manager.actor.begin_native(
            DragSource {
                window: source,
                origin_frame: source_frame,
                last_frame: source_frame,
                origin_space: Some(space),
                current_space: Some(space),
                tiled: true,
            },
            DragScene {
                targets: vec![DragSceneTarget {
                    window: target,
                    space,
                    frame: target_frame,
                }],
                ..DragScene::default()
            },
        );
        reactor.drag_manager.actor.motion(DragMotion {
            point: CGPoint::new(
                target_frame.origin.x + target_frame.size.width / 2.0,
                target_frame.origin.y + target_frame.size.height / 2.0,
            ),
        });
        let intent = reactor.drag_manager.actor.intent().unwrap();
        reactor.drag_manager.actor.set_preview(intent, Some(target_frame));
        assert_eq!(
            reactor.drag_manager.actor.target().unwrap().intent.window,
            target
        );
        handle_mouse_up(&mut reactor, MouseUpPayload {
            button: MouseButton::Left,
            final_space: Some(space),
        })
        .unwrap()
    }

    #[test]
    fn accepted_drop_requests_preview_handoff_even_without_haptics() {
        let outcome = drop_outcome(true);
        assert!(outcome.finish_drop_preview);
        assert!(!outcome.drop_haptic);
        assert_eq!(outcome.arrange.passes, 1);
    }

    #[test]
    fn vanished_drop_target_does_not_request_a_success_preview() {
        assert!(!drop_outcome(false).finish_drop_preview);
    }
}
