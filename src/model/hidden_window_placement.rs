// Modified by kdnk for wm on 2026-11-15. Original attribution is preserved in LICENSE.
use objc2_core_foundation::{CGPoint, CGRect, CGSize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HideCorner {
    BottomLeft,
    #[default]
    BottomRight,
}

impl HideCorner {
    pub fn opposite(self) -> Self {
        match self {
            Self::BottomLeft => Self::BottomRight,
            Self::BottomRight => Self::BottomLeft,
        }
    }
}

/// Physical scrolling constraint, prepared once from the display topology.
/// Free edges retain one point; edges facing another display contain the window.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ScrollingDisplayBounds {
    screen: CGRect,
    left: bool,
    right: bool,
}

impl ScrollingDisplayBounds {
    pub(crate) fn for_screen(screen: CGRect, screens: &[CGRect]) -> Option<Self> {
        let mut bounds = Self {
            screen,
            left: false,
            right: false,
        };
        for other in screens.iter().filter(|other| **other != screen) {
            if other.max().y <= screen.origin.y || other.origin.y >= screen.max().y {
                continue;
            }
            bounds.left |= other.origin.x < screen.origin.x;
            bounds.right |= other.max().x > screen.max().x;
        }
        Some(bounds)
    }

    pub(crate) fn resize_origin(self, frame: CGRect) -> Option<CGPoint> {
        (frame.origin.x < self.screen.origin.x
            || (self.right && frame.max().x > self.screen.max().x))
            .then_some(CGPoint::new(
                if self.right {
                    self.screen.origin.x
                } else {
                    self.screen.max().x - HiddenWindowPlacement::REVEAL_PX
                },
                self.apply(frame).origin.y,
            ))
    }

    pub(crate) fn for_motion(mut self, previous: CGRect) -> Self {
        let bottom = self.screen.max().y.max(previous.max().y);
        self.screen.origin.y = self.screen.origin.y.min(previous.origin.y);
        self.screen.size.height = bottom - self.screen.origin.y;
        self
    }

    pub(crate) fn apply(self, mut frame: CGRect) -> CGRect {
        let left = if self.left {
            self.screen.origin.x
        } else {
            self.screen.origin.x - frame.size.width + HiddenWindowPlacement::REVEAL_PX
        };
        let right = if self.right {
            (self.screen.max().x - frame.size.width).max(self.screen.origin.x)
        } else {
            self.screen.max().x - HiddenWindowPlacement::REVEAL_PX
        };
        frame.origin.x = frame.origin.x.clamp(left.min(right), right);
        frame.origin.y = frame.origin.y.clamp(
            self.screen.origin.y,
            (self.screen.max().y - frame.size.height).max(self.screen.origin.y),
        );
        frame
    }

    pub(crate) fn dialog_x(self, owner: CGRect, width: f64, offset_x: f64) -> f64 {
        let reveal = HiddenWindowPlacement::REVEAL_PX;
        if !self.left && owner.max().x <= self.screen.origin.x + reveal {
            self.screen.origin.x - width + reveal
        } else if !self.right && owner.origin.x >= self.screen.max().x - reveal {
            self.screen.max().x - reveal
        } else {
            owner.origin.x + offset_x
        }
    }
}

/// Pure geometry used to place inactive-workspace windows just offscreen.
pub struct HiddenWindowPlacement;

impl HiddenWindowPlacement {
    pub(crate) const REVEAL_PX: f64 = 1.0;
    const VISIBLE_THRESHOLD_PX: f64 = 3.0;

    fn rect_for_corner(screen: CGRect, window: CGRect, corner: HideCorner) -> CGRect {
        let x = match corner {
            HideCorner::BottomLeft => screen.origin.x - window.size.width + Self::REVEAL_PX,
            HideCorner::BottomRight => screen.max().x - Self::REVEAL_PX,
        };
        CGRect::new(CGPoint::new(x, screen.max().y - Self::REVEAL_PX), window.size)
    }

    fn intersection_area(a: CGRect, b: CGRect) -> f64 {
        let width = (a.max().x.min(b.max().x) - a.origin.x.max(b.origin.x)).max(0.0);
        let height = (a.max().y.min(b.max().y) - a.origin.y.max(b.origin.y)).max(0.0);
        width * height
    }

    pub fn calculate(
        screen: CGRect,
        window: CGRect,
        preferred_corner: HideCorner,
        other_screens: &[CGRect],
    ) -> CGRect {
        let overlap = |candidate: &CGRect| {
            other_screens
                .iter()
                .filter(|other| **other != screen)
                .map(|other| Self::intersection_area(*candidate, *other))
                .sum::<f64>()
        };
        // A bottom corner lands on any display stacked below; parking just past the
        // side edge, top-aligned and no taller than the display, avoids that.
        let corners = [preferred_corner, preferred_corner.opposite()];
        let bottom = corners.map(|corner| Self::rect_for_corner(screen, window, corner));
        let side = bottom.map(|rect| {
            CGRect::new(
                CGPoint::new(rect.origin.x, screen.origin.y),
                CGSize::new(window.size.width, window.size.height.min(screen.size.height)),
            )
        });
        bottom
            .into_iter()
            .chain(side)
            // Score each candidate once; a comparator otherwise rescans all
            // displays for the current best candidate on every comparison.
            .map(|candidate| (candidate, overlap(&candidate)))
            .min_by(|(_, a), (_, b)| a.total_cmp(b))
            .map(|(candidate, _)| candidate)
            .unwrap_or(window)
    }

    pub fn is_hidden(screen: CGRect, window: CGRect, other_screens: &[CGRect]) -> bool {
        // Most parked windows expose at most a pixel. Check that inexpensive
        // case before evaluating parking candidates against every other display.
        let visible_width =
            (window.max().x.min(screen.max().x) - window.origin.x.max(screen.origin.x)).max(0.0);
        let visible_height =
            (window.max().y.min(screen.max().y) - window.origin.y.max(screen.origin.y)).max(0.0);
        visible_width <= Self::VISIBLE_THRESHOLD_PX
            || visible_height <= Self::VISIBLE_THRESHOLD_PX
            || [HideCorner::BottomLeft, HideCorner::BottomRight]
                .into_iter()
                .any(|corner| Self::calculate(screen, window, corner, other_screens) == window)
    }
}

#[cfg(test)]
mod tests {
    use objc2_core_foundation::{CGPoint, CGSize};

    use super::*;

    fn rect(x: f64, y: f64, width: f64, height: f64) -> CGRect {
        CGRect::new(CGPoint::new(x, y), CGSize::new(width, height))
    }

    #[test]
    fn scrolling_retains_its_display_across_topologies_and_rounding() {
        let screen = rect(-1200.5, -900.5, 1000.0, 800.0);
        for neighbor in [
            rect(-1200.5, -1900.5, 1500.0, 1000.0),
            rect(-1200.5, -100.5, 1500.0, 1000.0),
            rect(-2200.5, -900.5, 1000.0, 800.0),
            rect(-200.5, -900.5, 1000.0, 800.0),
            rect(-250.5, -150.5, 1000.0, 800.0),
        ] {
            let bounds = ScrollingDisplayBounds::for_screen(screen, &[neighbor]).unwrap();
            for scale in [1.0_f64, 2.0] {
                for x in [-3000.25, -1250.25, -200.25, 1000.25] {
                    let frame =
                        bounds.apply(rect((x * scale).round() / scale, -900.5, 499.5, 700.0));
                    let own = HiddenWindowPlacement::intersection_area(screen, frame);
                    assert!(own > 0.0);
                    assert!(HiddenWindowPlacement::intersection_area(neighbor, frame) <= own);
                }
            }
        }
    }

    #[test]
    fn scrolling_motion_contracts_vertical_overflow_without_relaxing_neighbors() {
        let screen = rect(0., 0., 1000., 800.);
        let bounds =
            ScrollingDisplayBounds::for_screen(screen, &[screen, rect(1000., 0., 1000., 800.)])
                .unwrap();
        for direction in [-1., 1.] {
            let mut previous = rect(700., direction * 30., 400., 800.);
            for expected in [30., 20., 10., 0.] {
                let requested = rect(700., direction * expected, 400., 800.);
                let motion = bounds.for_motion(previous);
                let frame = motion.apply(requested);
                assert_eq!(frame.origin.y, direction * expected);
                assert_eq!(frame.origin.x, 600.);
                let more_overflow = motion.apply(rect(700., direction * 100., 400., 800.));
                assert_eq!(more_overflow.origin.y, previous.origin.y);
                previous = frame;
            }
            assert_eq!(
                bounds.apply(rect(100., direction * 30., 400., 800.)).origin.y,
                0.
            );
        }
        let inside = rect(100., 0., 400., 800.);
        assert_eq!(
            bounds.for_motion(inside).apply(rect(100., 30., 400., 800.)),
            inside
        );
    }

    #[test]
    fn anchors_to_requested_corner() {
        let hidden = HiddenWindowPlacement::calculate(
            rect(0.0, 0.0, 1000.0, 800.0),
            rect(10.0, 20.0, 200.0, 100.0),
            HideCorner::BottomRight,
            &[],
        );
        assert_eq!(hidden, rect(999.0, 799.0, 200.0, 100.0));
    }

    #[test]
    fn avoids_an_adjacent_monitor() {
        let screen = rect(0.0, 0.0, 1000.0, 800.0);
        let hidden = HiddenWindowPlacement::calculate(
            screen,
            rect(0.0, 0.0, 200.0, 100.0),
            HideCorner::BottomRight,
            &[rect(1000.0, 0.0, 1000.0, 800.0)],
        );
        assert_eq!(hidden.origin.x, -199.0);
    }

    #[test]
    fn parks_beside_the_display_when_another_display_sits_below() {
        let upper = rect(81.0, -1049.0, 1920.0, 1049.0);
        let lower = rect(0.0, 40.0, 2056.0, 1289.0);
        let window = rect(16.0, 100.0, 1007.0, 1219.0);
        let hidden =
            HiddenWindowPlacement::calculate(upper, window, HideCorner::BottomRight, &[lower]);
        assert_eq!(hidden, rect(2000.0, -1049.0, 1007.0, 1049.0));
        assert!(HiddenWindowPlacement::is_hidden(upper, hidden, &[lower]));
    }
}
