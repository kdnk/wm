// Modified by kdnk for wm on 2026-11-15. Original attribution is preserved in LICENSE.
use std::sync::LazyLock;

use cgs::compositor::{
    CgsWindow, CgsWindowError, WindowSurface, WindowTransaction, backdrop_blur,
    with_disabled_actions,
};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::NSWorkspace;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::CGColor;
use objc2_foundation::{NSArray, NSNumber, NSString, NSValue};
use objc2_quartz_core::{
    CABasicAnimation, CACurrentMediaTime, CAKeyframeAnimation, CALayer, CAMediaTiming,
    kCAAnimationLinear,
};

use crate::actor::drag::{PreviewAnchor, PreviewTarget, preview_frame};
use crate::sys::geometry::SameAs;
use crate::sys::window_server::WindowServerId;
use crate::ui::motion::{RectSample, RectSpring};

static FILL: LazyLock<Retained<CGColor>> =
    LazyLock::new(|| CGColor::new_generic_rgb(0.13, 0.62, 1.0, 0.18).into());
static BORDER: LazyLock<Retained<CGColor>> =
    LazyLock::new(|| CGColor::new_generic_rgb(0.13, 0.62, 1.0, 0.95).into());
static STACK_FILL: LazyLock<Retained<CGColor>> =
    LazyLock::new(|| CGColor::new_generic_rgb(0.34, 0.64, 1.0, 0.16).into());
static OPAQUE_FILL: LazyLock<Retained<CGColor>> =
    LazyLock::new(|| CGColor::new_generic_rgb(0.81, 0.91, 1.0, 1.0).into());
static CONTRAST_FILL: LazyLock<Retained<CGColor>> =
    LazyLock::new(|| CGColor::new_generic_rgb(0.72, 0.86, 1.0, 0.90).into());
static CONTRAST_BORDER: LazyLock<Retained<CGColor>> =
    LazyLock::new(|| CGColor::new_generic_rgb(0.02, 0.25, 0.52, 1.0).into());
static OPAQUE_STACK_FILL: LazyLock<Retained<CGColor>> =
    LazyLock::new(|| CGColor::new_generic_rgb(0.56, 0.77, 1.0, 1.0).into());

const PREVIEW_CORNER_RADIUS: f64 = 12.0;
const PREVIEW_BLUR_RADIUS: f64 = 6.0;
const MOTION_DURATION: f64 = 0.12;
const EXIT_DURATION: f64 = 0.09;
const KEYFRAME_RATE: f64 = 240.0;

fn keyframe_time(index: usize) -> f64 {
    match index {
        0 => 0.0,
        1 => 0.25 / KEYFRAME_RATE,
        _ => (index - 1) as f64 / KEYFRAME_RATE,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PreviewAppearance {
    increase_contrast: bool,
    reduce_transparency: bool,
    reduce_motion: bool,
}

impl PreviewAppearance {
    fn current() -> Self {
        let workspace = NSWorkspace::sharedWorkspace();
        Self {
            increase_contrast: workspace.accessibilityDisplayShouldIncreaseContrast(),
            reduce_transparency: workspace.accessibilityDisplayShouldReduceTransparency(),
            reduce_motion: workspace.accessibilityDisplayShouldReduceMotion(),
        }
    }

    fn apply(&self, shape: &CALayer, tint: &CALayer, card: &CALayer, backdrop: Option<&CALayer>) {
        with_disabled_actions(|| {
            let border = if self.increase_contrast {
                &*CONTRAST_BORDER
            } else {
                &*BORDER
            };
            shape.setBorderColor(Some(border));
            shape.setBorderWidth(if self.increase_contrast { 3.0 } else { 2.0 });
            card.setBorderColor(Some(border));
            card.setBorderWidth(if self.increase_contrast { 2.0 } else { 1.0 });
            tint.setBackgroundColor(Some(if self.reduce_transparency {
                &OPAQUE_FILL
            } else if self.increase_contrast {
                &CONTRAST_FILL
            } else {
                &FILL
            }));
            card.setBackgroundColor(Some(if self.reduce_transparency || self.increase_contrast {
                &OPAQUE_STACK_FILL
            } else {
                &STACK_FILL
            }));
            if let Some(backdrop) = backdrop {
                backdrop.setHidden(self.reduce_transparency);
            }
        });
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreviewStyle {
    Tile,
    Stack,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Visibility {
    Hidden,
    Tracking,
    Committed,
}

#[derive(Debug)]
struct PreviewGeometry {
    carrier: CGRect,
    frames: Vec<CGRect>,
    spring: Option<RectSpring>,
}

impl PreviewGeometry {
    fn new(initial: RectSample, to: CGRect, animate: bool, scale: f64) -> Self {
        if !animate
            || initial.frame.size.width < 1.0
            || initial.frame.size.height < 1.0
            || to.size.width < 1.0
            || to.size.height < 1.0
        {
            return Self {
                carrier: to,
                frames: vec![CGRect::new(CGPoint::ZERO, to.size)],
                spring: None,
            };
        }
        let spring = RectSpring::new(initial, to, MOTION_DURATION);
        if spring.settled(initial, scale) {
            return Self::new(initial, to, false, scale);
        }
        let carrier = spring.envelope();
        let local = |frame: CGRect| {
            CGRect::new(
                CGPoint::new(
                    frame.origin.x - carrier.origin.x,
                    frame.origin.y - carrier.origin.y,
                ),
                frame.size,
            )
        };
        let mut frames = vec![local(initial.frame)];
        loop {
            let sample = spring.sample(keyframe_time(frames.len()));
            let done = spring.settled(sample, scale);
            frames.push(local(if done { spring.target() } else { sample.frame }));
            if done {
                break;
            }
        }
        Self {
            carrier,
            frames,
            spring: Some(spring),
        }
    }

    fn duration(&self) -> f64 { keyframe_time(self.frames.len().saturating_sub(1)) }
}

#[derive(Debug, Clone, Copy)]
struct PreviewMotion {
    spring: RectSpring,
    started: f64,
    duration: f64,
}

impl PreviewMotion {
    fn presented(self, frame: CGRect, now: f64) -> RectSample {
        let elapsed = (now - self.started).max(0.0);
        if elapsed >= self.duration {
            return RectSample::stationary(frame);
        }
        RectSample {
            frame,
            velocity: self.spring.sample(elapsed).velocity,
        }
    }
}

pub struct DragPreview {
    // Detach the surface before releasing its WindowServer window.
    surface: WindowSurface,
    window: CgsWindow,
    stage: Retained<CALayer>,
    shape: Retained<CALayer>,
    backdrop: Option<Retained<CALayer>>,
    tint: Retained<CALayer>,
    card: Retained<CALayer>,
    carrier: CGRect,
    frame: CGRect,
    style: PreviewStyle,
    appearance: PreviewAppearance,
    relative_window: Option<u32>,
    visibility: Visibility,
    motion: Option<PreviewMotion>,
}

// Reactor moves to its thread before creating these exclusively owned layers.
unsafe impl Send for DragPreview {}

impl DragPreview {
    pub fn new(target: impl Into<PreviewTarget>) -> Result<Self, CgsWindowError> {
        let target = target.into();
        let appearance = PreviewAppearance::current();
        let frame = preview_frame(target);
        let style = Self::style(target);
        let bounds = Self::bounds(frame.size);
        let window = CgsWindow::new_compositor(frame, 0.0)?;
        window.set_alpha(1.0)?;
        window.set_tags(1 << 3)?;

        let stage = CALayer::layer();
        let shape = CALayer::layer();
        let backdrop = backdrop_blur(PREVIEW_BLUR_RADIUS);
        let tint = CALayer::layer();
        let card = CALayer::layer();
        with_disabled_actions(|| {
            stage.setBounds(bounds);
            stage.setPosition(CGPoint::ZERO);
            stage.setAnchorPoint(CGPoint::ZERO);
            stage.setContentsScale(2.0);
            stage.setOpaque(false);
            stage.setGeometryFlipped(true);
            stage.setOpacity(0.0);
            stage.addSublayer(&shape);

            shape.setAnchorPoint(CGPoint::ZERO);
            shape.setFrame(bounds);
            shape.setContentsScale(2.0);
            shape.setMasksToBounds(true);
            shape.setCornerRadius(PREVIEW_CORNER_RADIUS);
            if let Some(backdrop) = &backdrop {
                backdrop.setFrame(bounds);
                shape.addSublayer(backdrop);
            }
            tint.setFrame(bounds);
            shape.addSublayer(&tint);
            card.setOpacity(if style == PreviewStyle::Stack {
                1.0
            } else {
                0.0
            });
            card.setFrame(Self::card_frame(frame.size));
            card.setCornerRadius(8.0);
            shape.addSublayer(&card);
        });
        appearance.apply(&shape, &tint, &card, backdrop.as_deref());

        let surface = WindowSurface::new(window.id(), bounds, &stage)?;
        surface.flush();
        Ok(Self {
            surface,
            window,
            stage,
            shape,
            backdrop,
            tint,
            card,
            carrier: frame,
            frame,
            style,
            appearance,
            relative_window: None,
            visibility: Visibility::Hidden,
            motion: None,
        })
    }

    pub fn window_id(&self) -> WindowServerId { WindowServerId::new(self.window.id()) }

    pub fn show(&mut self, target: impl Into<PreviewTarget>) -> Result<(), CgsWindowError> {
        self.show_with_appearance(target, PreviewAppearance::current())
    }

    fn show_with_appearance(
        &mut self,
        target: impl Into<PreviewTarget>,
        appearance: PreviewAppearance,
    ) -> Result<(), CgsWindowError> {
        let target = target.into();
        let frame = preview_frame(target);
        let style = Self::style(target);
        let relative_window = match target.anchor {
            PreviewAnchor::Window(window) => Some(window.idx.get()),
            PreviewAnchor::Display(_) => None,
        };
        let tracking = self.visibility == Visibility::Tracking;
        let frame_changed = !self.frame.same_as(frame);
        let style_changed = self.style != style;
        let ordering_changed = !tracking || self.relative_window != relative_window;
        let appearance_changed = self.appearance != appearance;
        let settle_motion = appearance.reduce_motion && !self.appearance.reduce_motion;
        if !frame_changed && !style_changed && !ordering_changed && !appearance_changed {
            return Ok(());
        }
        if appearance_changed {
            appearance.apply(&self.shape, &self.tint, &self.card, self.backdrop.as_deref());
            self.appearance = appearance;
        }
        let duration = if appearance.reduce_motion {
            0.0
        } else {
            MOTION_DURATION
        };
        if frame_changed || !tracking || settle_motion {
            let presented = self.presented_frame();
            let started = CACurrentMediaTime();
            let fading = unsafe { self.stage.presentationLayer() }
                .is_some_and(|layer| layer.opacity() > 0.0);
            let from = if (tracking || fading) && duration > 0.0 {
                presented
            } else {
                frame
            };
            let initial = if tracking {
                self.motion.map_or_else(
                    || RectSample::stationary(from),
                    |motion| motion.presented(from, started),
                )
            } else {
                RectSample::stationary(from)
            };
            let geometry =
                PreviewGeometry::new(initial, frame, duration > 0.0, self.shape.contentsScale());
            let prepared = self.prepare_geometry(&geometry);
            let transaction = WindowTransaction::new()?;
            transaction.set_rounded_frame(&self.window, geometry.carrier, 0.0)?;
            self.surface.set_bounds_in(&transaction, Self::bounds(geometry.carrier.size));
            with_disabled_actions(|| self.stage.setBounds(Self::bounds(geometry.carrier.size)));
            for frame in prepared {
                frame.apply(started);
            }
            transaction.commit();
            self.motion = geometry.spring.map(|spring| PreviewMotion {
                spring,
                started,
                duration: geometry.duration(),
            });
            self.carrier = geometry.carrier;
            self.frame = frame;
        }
        if ordering_changed {
            self.window.order_above(relative_window)?;
            self.relative_window = relative_window;
        }
        if style_changed || settle_motion {
            fade(
                &self.card,
                if style == PreviewStyle::Stack {
                    1.0
                } else {
                    0.0
                },
                duration,
            );
            self.style = style;
        }
        if !tracking || settle_motion {
            fade(&self.stage, 1.0, duration);
        }
        self.visibility = Visibility::Tracking;
        self.surface.flush();
        Ok(())
    }

    pub fn finish(&mut self) { self.fade_out(Visibility::Committed); }

    pub fn hide(&mut self) {
        if self.visibility == Visibility::Tracking {
            self.fade_out(Visibility::Hidden);
        }
    }

    fn fade_out(&mut self, visibility: Visibility) {
        if self.visibility != Visibility::Tracking {
            return;
        }
        let duration = if NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion() {
            0.0
        } else {
            EXIT_DURATION
        };
        fade(&self.stage, 0.0, duration);
        self.motion = None;
        self.visibility = visibility;
        self.surface.flush();
    }

    fn presented_frame(&self) -> CGRect {
        self.surface.flush();
        let local = unsafe { self.shape.presentationLayer() }
            .map_or_else(|| self.shape.frame(), |layer| layer.frame());
        CGRect::new(
            CGPoint::new(
                local.origin.x + self.carrier.origin.x,
                local.origin.y + self.carrier.origin.y,
            ),
            local.size,
        )
    }

    fn prepare_geometry<'a>(&'a self, geometry: &PreviewGeometry) -> Vec<PreparedFrame<'a>> {
        let duration = geometry.duration();
        let mut prepared = vec![PreparedFrame::new(&self.shape, &geometry.frames, duration)];
        let bounds =
            geometry.frames.iter().map(|frame| Self::bounds(frame.size)).collect::<Vec<_>>();
        if let Some(backdrop) = &self.backdrop {
            prepared.push(PreparedFrame::new(backdrop, &bounds, duration));
        }
        prepared.push(PreparedFrame::new(&self.tint, &bounds, duration));
        let cards = geometry
            .frames
            .iter()
            .map(|frame| Self::card_frame(frame.size))
            .collect::<Vec<_>>();
        prepared.push(PreparedFrame::new(&self.card, &cards, duration));
        prepared
    }

    fn style(target: PreviewTarget) -> PreviewStyle {
        if target.stacked {
            PreviewStyle::Stack
        } else {
            PreviewStyle::Tile
        }
    }

    fn bounds(size: CGSize) -> CGRect { CGRect::new(CGPoint::ZERO, size) }

    fn card_frame(size: CGSize) -> CGRect {
        const INSET: f64 = 9.0;
        CGRect::new(
            CGPoint::new(INSET, INSET),
            CGSize::new(
                (size.width - INSET * 2.0).max(0.0),
                (size.height - INSET * 2.0).max(0.0),
            ),
        )
    }
}

struct PreparedFrame<'a> {
    layer: &'a CALayer,
    target: CGRect,
    animations: Option<(Retained<CAKeyframeAnimation>, Retained<CAKeyframeAnimation>)>,
}

impl<'a> PreparedFrame<'a> {
    fn new(layer: &'a CALayer, frames: &[CGRect], duration: f64) -> Self {
        let target = *frames.last().unwrap();
        let animations = if duration == 0.0 {
            None
        } else {
            let anchor = layer.anchorPoint();
            let positions = frames
                .iter()
                .map(|frame| unsafe {
                    NSValue::valueWithPoint(CGPoint::new(
                        frame.origin.x + frame.size.width * anchor.x,
                        frame.origin.y + frame.size.height * anchor.y,
                    ))
                })
                .collect::<Vec<_>>();
            let bounds = frames
                .iter()
                .map(|frame| unsafe { NSValue::valueWithRect(DragPreview::bounds(frame.size)) })
                .collect::<Vec<_>>();
            let times = NSArray::from_retained_slice(
                &(0..frames.len())
                    .map(|index| NSNumber::new_f64(keyframe_time(index) / duration))
                    .collect::<Vec<_>>(),
            );
            let movement =
                CAKeyframeAnimation::animationWithKeyPath(Some(&NSString::from_str("position")));
            let resize =
                CAKeyframeAnimation::animationWithKeyPath(Some(&NSString::from_str("bounds")));
            unsafe {
                movement.setValues(Some(&NSArray::from_slice(
                    &positions.iter().map(|value| &**value as &AnyObject).collect::<Vec<_>>(),
                )));
                resize.setValues(Some(&NSArray::from_slice(
                    &bounds.iter().map(|value| &**value as &AnyObject).collect::<Vec<_>>(),
                )));
                movement.setCalculationMode(kCAAnimationLinear);
                resize.setCalculationMode(kCAAnimationLinear);
            }
            movement.setKeyTimes(Some(&times));
            resize.setKeyTimes(Some(&times));
            movement.setDuration(duration);
            resize.setDuration(duration);
            Some((movement, resize))
        };
        Self { layer, target, animations }
    }

    fn apply(self, started: f64) {
        with_disabled_actions(|| self.layer.setFrame(self.target));
        let position_key = NSString::from_str("preview.position");
        let bounds_key = NSString::from_str("preview.bounds");
        if let Some((movement, resize)) = self.animations {
            let local_time = self.layer.convertTime_fromLayer(started, None);
            movement.setBeginTime(local_time);
            resize.setBeginTime(local_time);
            self.layer.addAnimation_forKey(&movement, Some(&position_key));
            self.layer.addAnimation_forKey(&resize, Some(&bounds_key));
        } else {
            self.layer.removeAnimationForKey(&position_key);
            self.layer.removeAnimationForKey(&bounds_key);
        }
    }
}

fn fade(layer: &CALayer, to: f32, duration: f64) {
    let from =
        unsafe { layer.presentationLayer() }.map_or(layer.opacity(), |layer| layer.opacity());
    let key = NSString::from_str("preview.opacity");
    with_disabled_actions(|| layer.setOpacity(to));
    if duration == 0.0 || from == to {
        layer.removeAnimationForKey(&key);
        return;
    }
    let animation = CABasicAnimation::animationWithKeyPath(Some(&NSString::from_str("opacity")));
    unsafe {
        animation.setFromValue(Some(&NSNumber::new_f32(from)));
        animation.setToValue(Some(&NSNumber::new_f32(to)));
    }
    animation.setDuration(duration);
    layer.addAnimation_forKey(&animation, Some(&key));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, width: f64, height: f64) -> CGRect {
        CGRect::new(CGPoint::new(x, y), CGSize::new(width, height))
    }

    #[test]
    fn carrier_contains_both_shapes_without_moving_the_presented_start() {
        let from = rect(20.0, 40.0, 100.0, 400.0);
        let to = rect(300.0, 10.0, 500.0, 200.0);
        let geometry = PreviewGeometry::new(RectSample::stationary(from), to, true, 2.0);
        assert_eq!(geometry.carrier, rect(20.0, 10.0, 780.0, 430.0));
        assert_eq!(geometry.frames[0], rect(0.0, 30.0, 100.0, 400.0));
        assert_eq!(*geometry.frames.last().unwrap(), rect(280.0, 0.0, 500.0, 200.0));
        for (local, global) in [
            (geometry.frames[0], from),
            (*geometry.frames.last().unwrap(), to),
        ] {
            assert_eq!(local.origin.x + geometry.carrier.origin.x, global.origin.x);
            assert_eq!(local.origin.y + geometry.carrier.origin.y, global.origin.y);
            assert!(local.origin.x >= 0.0 && local.origin.y >= 0.0);
            assert!(local.origin.x + local.size.width <= geometry.carrier.size.width);
            assert!(local.origin.y + local.size.height <= geometry.carrier.size.height);
        }
    }

    #[test]
    fn carrier_handles_negative_display_coordinates_and_reverse_motion() {
        let from = rect(-1500.0, -900.0, 200.0, 600.0);
        let to = rect(-1800.0, -500.0, 700.0, 100.0);
        let forward = PreviewGeometry::new(RectSample::stationary(from), to, true, 2.0);
        let reverse = PreviewGeometry::new(RectSample::stationary(to), from, true, 2.0);
        assert_eq!(forward.carrier, reverse.carrier);
        assert_eq!(forward.frames.first(), reverse.frames.last());
        assert_eq!(forward.frames.last(), reverse.frames.first());
    }

    #[test]
    fn reversing_preview_retains_velocity_inside_the_full_carrier() {
        let original = rect(20.0, 40.0, 100.0, 200.0);
        let target = rect(500.0, 100.0, 250.0, 300.0);
        let first = RectSpring::new(RectSample::stationary(original), target, MOTION_DURATION);
        let published = first.sample(0.025);
        let motion = PreviewMotion {
            spring: first,
            started: 10.0,
            duration: 0.2,
        };
        let initial = motion.presented(published.frame, 10.025);
        assert!((initial.velocity.origin.x - published.velocity.origin.x).abs() < 1e-8);
        assert_eq!(initial.frame, published.frame);
        let reversed = PreviewGeometry::new(initial, original, true, 2.0);
        assert!(reversed.frames[1].origin.x > reversed.frames[0].origin.x);
        assert!(
            reversed.carrier.origin.x + reversed.carrier.size.width
                > published.frame.origin.x + published.frame.size.width
        );
        for local in &reversed.frames {
            assert!(local.origin.x >= -1e-9 && local.origin.y >= -1e-9);
            assert!(local.origin.x + local.size.width <= reversed.carrier.size.width + 1e-9);
            assert!(local.origin.y + local.size.height <= reversed.carrier.size.height + 1e-9);
        }
        let last = reversed.frames.last().unwrap();
        assert_eq!(last.origin.x + reversed.carrier.origin.x, original.origin.x);
        assert_eq!(last.origin.y + reversed.carrier.origin.y, original.origin.y);
        assert_eq!(last.size, original.size);
        assert!(
            reversed
                .spring
                .unwrap()
                .settled(reversed.spring.unwrap().sample(reversed.duration()), 2.0)
        );
        assert_eq!(motion.presented(target, 10.3), RectSample::stationary(target));
    }

    #[test]
    fn immediate_and_degenerate_previews_have_no_motion_or_stale_velocity() {
        let initial = RectSpring::new(
            RectSample::stationary(rect(0.0, 0.0, 100.0, 100.0)),
            rect(300.0, 40.0, 200.0, 200.0),
            MOTION_DURATION,
        )
        .sample(0.02);
        for target in [
            rect(100.0, 200.0, 200.0, 200.0),
            rect(100.0, 200.0, 0.0, 0.0),
        ] {
            let immediate = PreviewGeometry::new(initial, target, false, 2.0);
            assert_eq!(immediate.carrier, target);
            assert_eq!(immediate.frames, vec![CGRect::new(CGPoint::ZERO, target.size)]);
            assert_eq!(immediate.duration(), 0.0);
            assert!(immediate.spring.is_none());
        }
        let degenerate = PreviewGeometry::new(initial, rect(100.0, 200.0, 0.0, 0.0), true, 2.0);
        assert_eq!(degenerate.duration(), 0.0);
        assert!(degenerate.spring.is_none());
    }

    #[test]
    fn layer_geometry_shares_timing_and_immediate_updates_remove_keyframes() {
        let geometry = PreviewGeometry::new(
            RectSample::stationary(rect(0.0, 0.0, 100.0, 100.0)),
            rect(300.0, 40.0, 200.0, 200.0),
            true,
            2.0,
        );
        let layer = CALayer::layer();
        let prepared = PreparedFrame::new(&layer, &geometry.frames, geometry.duration());
        let (movement, resize) = prepared.animations.as_ref().unwrap();
        assert_eq!(movement.duration(), resize.duration());
        assert_eq!(movement.values().unwrap().count(), geometry.frames.len());
        assert_eq!(resize.values().unwrap().count(), geometry.frames.len());
        let now = CACurrentMediaTime();
        prepared.apply(now);
        let position =
            unsafe { layer.animationForKey(&NSString::from_str("preview.position")) }.unwrap();
        let bounds =
            unsafe { layer.animationForKey(&NSString::from_str("preview.bounds")) }.unwrap();
        assert_eq!(position.beginTime(), bounds.beginTime());
        assert_eq!(layer.frame(), *geometry.frames.last().unwrap());
        let target = rect(40.0, 30.0, 80.0, 60.0);
        PreparedFrame::new(&layer, &[target], 0.0).apply(now);
        assert_eq!(layer.frame(), target);
        assert!(
            unsafe { layer.animationForKey(&NSString::from_str("preview.position")) }.is_none()
        );
        assert!(unsafe { layer.animationForKey(&NSString::from_str("preview.bounds")) }.is_none());
    }

    #[test]
    fn accessibility_appearance_updates_native_layers_without_changing_geometry() {
        let shape = CALayer::layer();
        let tint = CALayer::layer();
        let card = CALayer::layer();
        let backdrop = CALayer::layer();
        let frame = rect(20.0, 30.0, 500.0, 300.0);
        with_disabled_actions(|| shape.setFrame(frame));
        for increase_contrast in [false, true] {
            for reduce_transparency in [false, true] {
                let appearance = PreviewAppearance {
                    increase_contrast,
                    reduce_transparency,
                    reduce_motion: false,
                };
                appearance.apply(&shape, &tint, &card, Some(&backdrop));
                assert_eq!(shape.frame(), frame);
                assert_eq!(backdrop.isHidden(), reduce_transparency);
                if reduce_transparency {
                    assert_eq!(CGColor::alpha(tint.backgroundColor().as_deref()), 1.0);
                    assert_eq!(CGColor::alpha(card.backgroundColor().as_deref()), 1.0);
                }
                if increase_contrast {
                    assert_eq!(CGColor::alpha(shape.borderColor().as_deref()), 1.0);
                    assert!(shape.borderWidth() > 2.0);
                } else {
                    assert_eq!(shape.borderWidth(), 2.0);
                }
            }
        }
        PreviewAppearance {
            increase_contrast: false,
            reduce_transparency: false,
            reduce_motion: false,
        }
        .apply(&shape, &tint, &card, Some(&backdrop));
        assert!(!backdrop.isHidden());
        assert!(CGColor::alpha(tint.backgroundColor().as_deref()) < 1.0);
        assert_eq!(shape.borderWidth(), 2.0);
        assert_eq!(card.borderWidth(), 1.0);
    }
}
