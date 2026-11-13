// Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, DefinedClass, MainThreadOnly, Message, define_class, msg_send};
use objc2_app_kit::{
    NSAppearanceCustomization, NSButton, NSColor, NSDragOperation, NSDraggingContext,
    NSDraggingDestination, NSDraggingInfo, NSDraggingItem, NSDraggingSession, NSDraggingSource,
    NSEvent, NSPasteboardItem, NSView, NSWindow, NSWorkspace,
};
use objc2_foundation::{
    MainThreadMarker, NSArray, NSNumber, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString,
};
use objc2_quartz_core::{CABasicAnimation, CAMediaTiming, CAMediaTimingFunction, CATransaction};

use crate::actor::app::WindowId;
use crate::actor::reactor::{Event, Sender};
use crate::layout_engine::{ColumnId, ColumnPlacement, ColumnReorderRequest, VirtualWorkspaceId};
use crate::sys::screen::SpaceId;

const TYPE: &str = "app.wm.menu-column";
const MARKER_MOTION: &str = "wm-insertion-position";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ColumnIdentity {
    pub id: ColumnId,
    pub members: Vec<WindowId>,
}
struct Control {
    button: Retained<NSButton>,
    identity: ColumnIdentity,
}
struct Press {
    index: usize,
    origin: NSPoint,
}
struct DropTarget {
    identity: ColumnIdentity,
    placement: ColumnPlacement,
    marker_x: f64,
}
struct Feedback {
    source: Retained<NSButton>,
    alpha: f64,
    marker: Option<Retained<NSView>>,
}
impl Feedback {
    fn lift(source: &NSButton) -> Self {
        let alpha = source.alphaValue();
        source.setAlphaValue(alpha * 0.3);
        Self {
            source: source.retain(),
            alpha,
            marker: None,
        }
    }

    fn restore(self) {
        self.source.setAlphaValue(self.alpha);
        if let Some(marker) = self.marker {
            hide_marker(&marker);
        }
    }
}
struct Gesture {
    view: Retained<DragContent>,
    source: ColumnIdentity,
    token: String,
    native: bool,
    valid: bool,
    feedback: Option<Feedback>,
    _window: Option<Retained<NSWindow>>,
}
thread_local! { static GESTURE: RefCell<Option<Gesture>> = const { RefCell::new(None) }; static SERIAL: Cell<u64> = const { Cell::new(0) }; }

pub(super) fn busy() -> bool { GESTURE.with(|gesture| gesture.borrow().is_some()) }
pub(super) fn cancel_window(window: Option<&NSWindow>) {
    let (cancelled, feedback) = GESTURE.with(|gesture| {
        let mut gesture = gesture.borrow_mut();
        let Some(active) =
            gesture.as_mut().filter(|active| active.view.window().as_deref() == window)
        else {
            return (None, None);
        };
        active.valid = false;
        let feedback = active.feedback.take();
        let cancelled = if active.native { None } else { gesture.take() };
        (cancelled, feedback)
    });
    if let Some(feedback) = feedback {
        feedback.restore();
    }
    if let Some(active) = cancelled {
        active.view.ivars().press.borrow_mut().take();
    }
}
pub(super) fn invalidate_if_changed(
    space: SpaceId,
    workspace: VirtualWorkspaceId,
    columns: &[ColumnIdentity],
) {
    let feedback = GESTURE.with(|active| {
        let mut active = active.borrow_mut();
        let gesture = active.as_mut()?;
        let state = gesture.view.ivars();
        if state.space == space && state.workspace == workspace {
            let unchanged = columns.contains(&gesture.source)
                && state
                    .controls
                    .borrow()
                    .iter()
                    .all(|control| columns.contains(&control.identity));
            if !unchanged {
                gesture.valid = false;
                return gesture.feedback.take();
            }
        }
        None
    });
    if let Some(feedback) = feedback {
        feedback.restore();
    }
}

fn hide_marker(marker: &NSView) {
    marker.setHidden(true);
    if let Some(layer) = marker.layer() {
        layer.removeAnimationForKey(&NSString::from_str(MARKER_MOTION));
    }
}

pub(super) struct State {
    space: SpaceId,
    workspace: VirtualWorkspaceId,
    reactor: Sender,
    controls: RefCell<Vec<Control>>,
    press: RefCell<Option<Press>>,
    marker: Retained<NSView>,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[ivars = State]
    pub(super) struct DragContent;
    unsafe impl NSObjectProtocol for DragContent {}
    impl DragContent {
        #[unsafe(method(acceptsFirstMouse:))]
        fn first_mouse(&self, _: Option<&NSEvent>) -> bool { true }
        #[unsafe(method(mouseDownCanMoveWindow))]
        fn moves_window(&self) -> bool { false }
        #[unsafe(method(hitTest:))]
        fn hit(&self, point: NSPoint) -> *mut NSView {
            let hit: *mut NSView = unsafe { msg_send![super(self), hitTest: point] };
            if self.ivars().controls.borrow().iter().any(|control| std::ptr::eq(hit, &*control.button as *const NSButton as *const NSView)) {
                (self as *const Self).cast_mut().cast()
            } else { hit }
        }
        #[unsafe(method(mouseDown:))]
        fn down(&self, event: &NSEvent) {
            if busy() { return; }
            let point = self.convertPoint_fromView(event.locationInWindow(), None);
            let index = self.ivars().controls.borrow().iter().position(|control| contains(control.button.frame(),point));
            let Some(index) = index else { return };
            let source = self.ivars().controls.borrow()[index].identity.clone();
            let token = SERIAL.with(|serial| { let next=serial.get()+1; serial.set(next); format!("{}:{next}",std::process::id()) });
            *self.ivars().press.borrow_mut() = Some(Press { index, origin: point });
            GESTURE.with(|gesture| *gesture.borrow_mut() = Some(Gesture { view: self.retain(), source, token, native: false, valid: true, feedback: None, _window: self.window() }));
        }
        #[unsafe(method(mouseDragged:))]
        fn dragged(&self, event: &NSEvent) {
            let point = self.convertPoint_fromView(event.locationInWindow(), None);
            let Some((index,origin)) = self.ivars().press.borrow().as_ref().map(|press|(press.index,press.origin)) else { return; };
            if (point.x-origin.x).hypot(point.y-origin.y) < 4.0 { return; }
            let Some(token) = GESTURE.with(|gesture| {
                gesture.borrow().as_ref().filter(|active|!active.native && active.valid).map(|active|active.token.clone())
            }) else { return; };
            let button = self.ivars().controls.borrow()[index].button.clone();
            let Some(image) = button.image() else { cancel_window(self.window().as_deref()); return; };
            let writer = NSPasteboardItem::new();
            writer.setString_forType(&NSString::from_str(&token), &NSString::from_str(TYPE));
            let item = NSDraggingItem::initWithPasteboardWriter(NSDraggingItem::alloc(), ProtocolObject::from_ref(&*writer));
            let size = image.size();
            let frame = button.frame();
            let rect = NSRect::new(NSPoint::new(frame.origin.x+(frame.size.width-size.width)/2.0, frame.origin.y+(frame.size.height-18.0)/2.0+5.0),NSSize::new(size.width,18.0));
            unsafe { item.setDraggingFrame_contents(rect, Some(&image)); }
            let feedback = Feedback::lift(&button);
            GESTURE.with(|gesture| {
                if let Some(active) = gesture.borrow_mut().as_mut() {
                    active.native = true;
                    active.feedback = Some(feedback);
                }
            });
            self.beginDraggingSessionWithItems_event_source(&NSArray::from_slice(&[&*item]), event, ProtocolObject::from_ref(self));
        }
        #[unsafe(method(mouseUp:))]
        fn up(&self, event: &NSEvent) {
            let native = GESTURE.with(|gesture| gesture.borrow().as_ref().is_some_and(|gesture| gesture.native));
            if native { return; }
            let point = self.convertPoint_fromView(event.locationInWindow(),None);
            let press = self.ivars().press.borrow_mut().take();
            let valid = GESTURE.with(|gesture| gesture.borrow_mut().take().is_some_and(|gesture| gesture.valid));
            if let Some(press) = press.filter(|_|valid) {
                let controls=self.ivars().controls.borrow();
                if contains(controls[press.index].button.frame(),point) { unsafe { controls[press.index].button.performClick(None); } }
            }
        }
    }
    unsafe impl NSDraggingSource for DragContent {
        #[unsafe(method(draggingSession:sourceOperationMaskForDraggingContext:))]
        fn operations(&self, _: &NSDraggingSession, context: NSDraggingContext) -> NSDragOperation {
            if context == NSDraggingContext::WithinApplication { NSDragOperation::Move } else { NSDragOperation::None }
        }
        #[unsafe(method(ignoreModifierKeysForDraggingSession:))]
        fn ignore_modifiers(&self, _: &NSDraggingSession) -> bool { true }
        #[unsafe(method(draggingSession:endedAtPoint:operation:))]
        fn ended(&self, _: &NSDraggingSession, _: NSPoint, _: NSDragOperation) {
            let _keep_alive = self.retain();
            self.finish();
        }
    }
    unsafe impl NSDraggingDestination for DragContent {
        #[unsafe(method(draggingEntered:))]
        fn entered(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> NSDragOperation { self.draggingUpdated(sender) }
        #[unsafe(method(draggingUpdated:))]
        fn updated(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> NSDragOperation {
            let target=self.target(sender);
            self.show_target(target.as_ref(), !NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion());
            if target.is_some() { NSDragOperation::Move } else { NSDragOperation::None }
        }
        #[unsafe(method(draggingExited:))]
        fn exited(&self, _: Option<&ProtocolObject<dyn NSDraggingInfo>>) { hide_marker(&self.ivars().marker); }
        #[unsafe(method(prepareForDragOperation:))]
        fn prepare(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> bool { self.target(sender).is_some() }
        #[unsafe(method(performDragOperation:))]
        fn perform(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> bool {
            self.perform_drop(sender)
        }
    }
);

fn contains(rect: NSRect, point: NSPoint) -> bool {
    point.x >= rect.origin.x
        && point.x <= rect.origin.x + rect.size.width
        && point.y >= rect.origin.y
        && point.y <= rect.origin.y + rect.size.height
}
impl DragContent {
    pub(super) fn new(
        mtm: MainThreadMarker,
        space: SpaceId,
        workspace: VirtualWorkspaceId,
        reactor: Sender,
    ) -> Retained<Self> {
        let marker = NSView::new(mtm);
        marker.setWantsLayer(true);
        if let Some(layer) = marker.layer() {
            layer.setCornerRadius(1.0);
        }
        marker.setHidden(true);
        let this: Retained<Self> = unsafe {
            msg_send![
                super(Self::alloc(mtm).set_ivars(State {
                    space,
                    workspace,
                    reactor,
                    controls: RefCell::new(Vec::new()),
                    press: RefCell::new(None),
                    marker: marker.clone()
                })),
                init
            ]
        };
        this.addSubview(&marker);
        this.registerForDraggedTypes(&NSArray::from_slice(&[&*NSString::from_str(TYPE)]));
        this
    }

    pub(super) fn add_column(&self, button: &NSButton, identity: ColumnIdentity) {
        self.ivars().controls.borrow_mut().push(Control {
            button: button.retain(),
            identity,
        });
        self.addSubview(&self.ivars().marker);
    }

    fn perform_drop(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> bool {
        let Some(target) = self.target(sender) else {
            return false;
        };
        let source =
            GESTURE.with(|gesture| gesture.borrow().as_ref().map(|gesture| gesture.source.clone()));
        let Some(source) = source else { return false };
        self.ivars().reactor.send(Event::MenuBarReorderColumn(ColumnReorderRequest {
            space: self.ivars().space,
            workspace: self.ivars().workspace,
            source: source.id,
            target: target.identity.id,
            source_members: source.members,
            target_members: target.identity.members,
            placement: target.placement,
        }));
        let feedback = GESTURE.with(|gesture| {
            let mut gesture = gesture.borrow_mut();
            let active = gesture.as_mut()?;
            active.valid = false;
            active.feedback.take()
        });
        if let Some(feedback) = feedback {
            feedback.restore();
        }
        true
    }

    fn finish(&self) {
        self.ivars().press.borrow_mut().take();
        let active = GESTURE.with(|gesture| gesture.borrow_mut().take());
        if let Some(feedback) = active.and_then(|gesture| gesture.feedback) {
            feedback.restore();
        }
        hide_marker(&self.ivars().marker);
    }

    fn show_target(&self, target: Option<&DropTarget>, animate: bool) {
        let marker = &self.ivars().marker;
        let Some(target) = target else {
            hide_marker(marker);
            return;
        };
        let previous = GESTURE.with(|gesture| {
            let mut gesture = gesture.borrow_mut();
            gesture.as_mut()?.feedback.as_mut()?.marker.replace(marker.clone())
        });
        if let Some(previous) = previous.filter(|previous| !std::ptr::eq(&**previous, &**marker)) {
            hide_marker(&previous);
        }
        let layer = marker.layer().unwrap();
        let height = 14.0_f64.min(self.bounds().size.height);
        let frame = NSRect::new(
            NSPoint::new(target.marker_x - 1.0, (self.bounds().size.height - height) / 2.0),
            NSSize::new(2.0, height),
        );
        let was_visible = !marker.isHidden();
        let changed = marker.frame() != frame;
        let from = unsafe { layer.presentationLayer() }
            .map_or(layer.position().x, |layer| layer.position().x);
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        self.effectiveAppearance()
            .performAsCurrentDrawingAppearance(&block2::RcBlock::new(|| {
                let alpha = if NSWorkspace::sharedWorkspace()
                    .accessibilityDisplayShouldIncreaseContrast()
                {
                    1.0
                } else {
                    0.65
                };
                layer.setBackgroundColor(Some(
                    &NSColor::labelColor().colorWithAlphaComponent(alpha).CGColor(),
                ));
            }));
        marker.setFrame(frame);
        marker.setHidden(false);
        CATransaction::commit();
        let key = NSString::from_str(MARKER_MOTION);
        if !animate || !was_visible {
            layer.removeAnimationForKey(&key);
        } else if changed {
            let motion =
                CABasicAnimation::animationWithKeyPath(Some(&NSString::from_str("position.x")));
            unsafe {
                motion.setFromValue(Some(&NSNumber::new_f64(from)));
                motion.setToValue(Some(&NSNumber::new_f64(layer.position().x)));
            }
            motion.setDuration(0.12);
            motion.setTimingFunction(Some(&CAMediaTimingFunction::functionWithControlPoints(
                0.2, 0.8, 0.2, 1.0,
            )));
            layer.addAnimation_forKey(&motion, Some(&key));
        }
    }

    fn target(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> Option<DropTarget> {
        let token = sender
            .draggingPasteboard()
            .stringForType(&NSString::from_str(TYPE))?
            .to_string();
        let source = GESTURE.with(|gesture| {
            let gesture = gesture.borrow();
            let active = gesture.as_ref()?;
            let state = active.view.ivars();
            (active.native
                && active.valid
                && token == active.token
                && state.space == self.ivars().space
                && state.workspace == self.ivars().workspace)
                .then(|| active.source.clone())
        })?;
        let point = self.convertPoint_fromView(sender.draggingLocation(), None);
        if !contains(self.bounds(), point) {
            return None;
        }
        let controls = self.ivars().controls.borrow();
        if self.subviews().iter().any(|view| {
            view.downcast_ref::<NSButton>().is_some_and(|button| {
                contains(button.frame(), point)
                    && !controls.iter().any(|control| std::ptr::eq(&*control.button, button))
            })
        }) {
            return None;
        }
        let source_index = controls.iter().position(|control| control.identity == source)?;
        let frames: Vec<_> = controls.iter().map(|control| control.button.frame()).collect();
        let (index, placement, marker_x) = insertion_target(&frames, source_index, point.x)?;
        Some(DropTarget {
            identity: controls[index].identity.clone(),
            placement,
            marker_x,
        })
    }
}

fn insertion_target(
    frames: &[NSRect],
    source: usize,
    x: f64,
) -> Option<(usize, ColumnPlacement, f64)> {
    let slot = frames
        .iter()
        .position(|frame| x <= frame.origin.x + frame.size.width / 2.0)
        .unwrap_or(frames.len());
    if slot == source || slot == source + 1 {
        return None;
    }
    let (index, placement) = if slot < frames.len() {
        (slot, ColumnPlacement::Before)
    } else {
        (frames.len().checked_sub(1)?, ColumnPlacement::After)
    };
    let marker_x = if slot == 0 {
        frames[0].origin.x
    } else if slot == frames.len() {
        frames[slot - 1].origin.x + frames[slot - 1].size.width
    } else {
        (frames[slot - 1].origin.x + frames[slot - 1].size.width + frames[slot].origin.x) / 2.0
    };
    Some((index, placement, marker_x))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_bar_insertion_slots_skip_both_source_boundaries() {
        let frames = [
            NSRect::new(NSPoint::new(4.0, 0.0), NSSize::new(26.0, 24.0)),
            NSRect::new(NSPoint::new(28.0, 0.0), NSSize::new(42.0, 24.0)),
            NSRect::new(NSPoint::new(68.0, 0.0), NSSize::new(26.0, 24.0)),
        ];
        let slots = [4.0, 28.0, 68.0, 94.0];
        for source in 0..frames.len() {
            for (slot, x) in slots.into_iter().enumerate() {
                let actual = insertion_target(&frames, source, x);
                if slot == source || slot == source + 1 {
                    assert!(
                        actual.is_none(),
                        "source {source}, slot {slot} must preserve order"
                    );
                } else {
                    let expected = match slot {
                        0 => (0, ColumnPlacement::Before, 4.0),
                        1 => (1, ColumnPlacement::Before, 29.0),
                        2 => (2, ColumnPlacement::Before, 69.0),
                        3 => (2, ColumnPlacement::After, 94.0),
                        _ => unreachable!(),
                    };
                    assert_eq!(actual, Some(expected));
                }
            }
        }
        assert!(insertion_target(&[], 0, 0.0).is_none());
    }
}
