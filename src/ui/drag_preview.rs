// Modified by kdnk for wm on 2026-11-15. Original attribution is preserved in LICENSE.
use std::sync::LazyLock;

use cgs::compositor::{
    CgsWindow, CgsWindowError, WindowSurface, WindowTransaction, backdrop_blur,
    with_disabled_actions,
};
use objc2::rc::Retained;
use objc2_app_kit::NSWorkspace;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::CGColor;
use objc2_foundation::{NSNumber, NSString, NSValue};
use objc2_quartz_core::{CABasicAnimation, CALayer, CAMediaTiming, CAMediaTimingFunction};

use crate::actor::drag::{DropTarget, preview_frame};
use crate::layout_engine::WindowDropAction;
use crate::sys::geometry::SameAs;

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

#[derive(Debug, Clone, Copy)]
struct PreviewGeometry {
    carrier: CGRect,
    from: CGRect,
    to: CGRect,
}

impl PreviewGeometry {
    fn between(from: CGRect, to: CGRect) -> Self {
        let origin = CGPoint::new(from.origin.x.min(to.origin.x), from.origin.y.min(to.origin.y));
        let carrier = CGRect::new(
            origin,
            CGSize::new(
                (from.origin.x + from.size.width).max(to.origin.x + to.size.width) - origin.x,
                (from.origin.y + from.size.height).max(to.origin.y + to.size.height) - origin.y,
            ),
        );
        let local = |frame: CGRect| {
            CGRect::new(
                CGPoint::new(frame.origin.x - origin.x, frame.origin.y - origin.y),
                frame.size,
            )
        };
        Self {
            carrier,
            from: local(from),
            to: local(to),
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
}

// Reactor moves to its thread before creating these exclusively owned layers.
unsafe impl Send for DragPreview {}

impl DragPreview {
    pub fn new(target: DropTarget) -> Result<Self, CgsWindowError> {
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
        })
    }

    pub fn show(&mut self, target: DropTarget) -> Result<(), CgsWindowError> {
        self.show_with_appearance(target, PreviewAppearance::current())
    }

    fn show_with_appearance(
        &mut self,
        target: DropTarget,
        appearance: PreviewAppearance,
    ) -> Result<(), CgsWindowError> {
        let frame = preview_frame(target);
        let style = Self::style(target);
        let relative_window = target.intent.window.idx.get();
        let tracking = self.visibility == Visibility::Tracking;
        let frame_changed = !self.frame.same_as(frame);
        let style_changed = self.style != style;
        let ordering_changed = !tracking || self.relative_window != Some(relative_window);
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
            let fading = unsafe { self.stage.presentationLayer() }
                .is_some_and(|layer| layer.opacity() > 0.0);
            let from = if (tracking || fading) && duration > 0.0 {
                self.presented_frame()
            } else {
                frame
            };
            let geometry = PreviewGeometry::between(from, frame);
            let transaction = WindowTransaction::new()?;
            transaction.set_rounded_frame(&self.window, geometry.carrier, 0.0)?;
            self.surface.set_bounds_in(&transaction, Self::bounds(geometry.carrier.size));
            with_disabled_actions(|| self.stage.setBounds(Self::bounds(geometry.carrier.size)));
            self.set_geometry(geometry.from, geometry.to, duration);
            transaction.commit();
            self.carrier = geometry.carrier;
            self.frame = frame;
        }
        if ordering_changed {
            self.window.order_above(Some(relative_window))?;
            self.relative_window = Some(relative_window);
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
        self.visibility = visibility;
        self.surface.flush();
    }

    fn presented_frame(&self) -> CGRect {
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

    fn set_geometry(&self, from: CGRect, to: CGRect, duration: f64) {
        animate_frame(&self.shape, from, to, duration);
        let from_bounds = Self::bounds(from.size);
        let to_bounds = Self::bounds(to.size);
        if let Some(backdrop) = &self.backdrop {
            animate_frame(backdrop, from_bounds, to_bounds, duration);
        }
        animate_frame(&self.tint, from_bounds, to_bounds, duration);
        animate_frame(
            &self.card,
            Self::card_frame(from.size),
            Self::card_frame(to.size),
            duration,
        );
    }

    fn style(target: DropTarget) -> PreviewStyle {
        if target.intent.action == WindowDropAction::Stack {
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

fn animate_frame(layer: &CALayer, from: CGRect, to: CGRect, duration: f64) {
    let anchor = layer.anchorPoint();
    let position = |frame: CGRect| {
        CGPoint::new(
            frame.origin.x + frame.size.width * anchor.x,
            frame.origin.y + frame.size.height * anchor.y,
        )
    };
    with_disabled_actions(|| layer.setFrame(to));
    let position_key = NSString::from_str("preview.position");
    let bounds_key = NSString::from_str("preview.bounds");
    if duration == 0.0 || from.same_as(to) {
        layer.removeAnimationForKey(&position_key);
        layer.removeAnimationForKey(&bounds_key);
        return;
    }
    let movement = CABasicAnimation::animationWithKeyPath(Some(&NSString::from_str("position")));
    let resize = CABasicAnimation::animationWithKeyPath(Some(&NSString::from_str("bounds")));
    unsafe {
        movement.setFromValue(Some(&NSValue::valueWithPoint(position(from))));
        movement.setToValue(Some(&NSValue::valueWithPoint(position(to))));
        resize.setFromValue(Some(&NSValue::valueWithRect(DragPreview::bounds(from.size))));
        resize.setToValue(Some(&NSValue::valueWithRect(DragPreview::bounds(to.size))));
    }
    for animation in [&movement, &resize] {
        animation.setDuration(duration);
        animation.setTimingFunction(Some(&CAMediaTimingFunction::functionWithControlPoints(
            0.2, 0.8, 0.2, 1.0,
        )));
    }
    layer.addAnimation_forKey(&movement, Some(&position_key));
    layer.addAnimation_forKey(&resize, Some(&bounds_key));
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
        let geometry = PreviewGeometry::between(from, to);
        assert_eq!(geometry.carrier, rect(20.0, 10.0, 780.0, 430.0));
        assert_eq!(geometry.from, rect(0.0, 30.0, 100.0, 400.0));
        assert_eq!(geometry.to, rect(280.0, 0.0, 500.0, 200.0));
        for (local, global) in [(geometry.from, from), (geometry.to, to)] {
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
        let forward = PreviewGeometry::between(from, to);
        let reverse = PreviewGeometry::between(to, from);
        assert_eq!(forward.carrier, reverse.carrier);
        assert_eq!(forward.from, reverse.to);
        assert_eq!(forward.to, reverse.from);
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
