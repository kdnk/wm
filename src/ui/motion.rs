// Modified by kdnk for wm on 2026-11-15. Original attribution is preserved in LICENSE.
use objc2_core_foundation::{CGPoint, CGRect, CGSize};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct RectVelocity {
    pub origin: CGPoint,
    pub size: CGSize,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct RectSample {
    pub frame: CGRect,
    pub velocity: RectVelocity,
}

impl RectSample {
    pub fn stationary(frame: CGRect) -> Self {
        Self {
            frame,
            velocity: RectVelocity::default(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RectSpring {
    initial: RectSample,
    target: CGRect,
    omega: f64,
}

impl RectSpring {
    pub fn new(initial: RectSample, target: CGRect, response: f64) -> Self {
        Self {
            initial,
            target,
            omega: 12.0 / response,
        }
    }

    pub fn target(self) -> CGRect { self.target }

    fn component(self, from: f64, to: f64, velocity: f64, time: f64) -> (f64, f64) {
        if time == 0.0 {
            return (from, velocity);
        }
        let decay = (-self.omega * time).exp();
        if decay == 0.0 {
            return (to, 0.0);
        }
        let displacement = from - to;
        let b = velocity + self.omega * displacement;
        (
            to + (displacement + b * time) * decay,
            (velocity - self.omega * b * time) * decay,
        )
    }

    pub fn sample(self, elapsed: f64) -> RectSample {
        let time = elapsed.max(0.0);
        let from = self.initial.frame;
        let velocity = self.initial.velocity;
        let (x, vx) = self.component(from.origin.x, self.target.origin.x, velocity.origin.x, time);
        let (y, vy) = self.component(from.origin.y, self.target.origin.y, velocity.origin.y, time);
        let (width, vw) = self.component(
            from.size.width,
            self.target.size.width,
            velocity.size.width,
            time,
        );
        let (height, vh) = self.component(
            from.size.height,
            self.target.size.height,
            velocity.size.height,
            time,
        );
        RectSample {
            frame: CGRect::new(CGPoint::new(x, y), CGSize::new(width.max(1.0), height.max(1.0))),
            velocity: RectVelocity {
                origin: CGPoint::new(vx, vy),
                size: CGSize::new(
                    if width < 1.0 { 0.0 } else { vw },
                    if height < 1.0 { 0.0 } else { vh },
                ),
            },
        }
    }

    pub fn settled(self, sample: RectSample, scale: f64) -> bool {
        let tolerance = 0.25 / scale;
        let edges = |frame: CGRect| {
            [
                frame.origin.x,
                frame.origin.y,
                frame.origin.x + frame.size.width,
                frame.origin.y + frame.size.height,
            ]
        };
        let velocity = sample.velocity;
        edges(sample.frame)
            .into_iter()
            .zip(edges(self.target))
            .zip(edges(CGRect::new(velocity.origin, velocity.size)))
            .all(|((current, target), speed)| {
                (current - target).abs() <= tolerance && speed.abs() / self.omega <= tolerance
            })
    }

    pub fn envelope(self) -> CGRect {
        let range = |from: f64, to: f64, velocity: f64| {
            let mut minimum = from.min(to);
            let mut maximum = from.max(to);
            let b = velocity + self.omega * (from - to);
            let time = velocity / (self.omega * b);
            if time.is_finite() && time > 0.0 {
                let value = self.component(from, to, velocity, time).0;
                minimum = minimum.min(value);
                maximum = maximum.max(value);
            }
            (minimum, maximum)
        };
        let from = self.initial.frame;
        let velocity = self.initial.velocity;
        let (left, maximum_x) = range(from.origin.x, self.target.origin.x, velocity.origin.x);
        let (top, maximum_y) = range(from.origin.y, self.target.origin.y, velocity.origin.y);
        let (_, right) = range(
            from.origin.x + from.size.width,
            self.target.origin.x + self.target.size.width,
            velocity.origin.x + velocity.size.width,
        );
        let (_, bottom) = range(
            from.origin.y + from.size.height,
            self.target.origin.y + self.target.size.height,
            velocity.origin.y + velocity.size.height,
        );
        CGRect::new(
            CGPoint::new(left, top),
            CGSize::new(
                right.max(maximum_x + 1.0) - left,
                bottom.max(maximum_y + 1.0) - top,
            ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, width: f64, height: f64) -> CGRect {
        CGRect::new(CGPoint::new(x, y), CGSize::new(width, height))
    }

    fn assert_near(actual: RectSample, expected: RectSample) {
        let components = |sample: RectSample| {
            [
                sample.frame.origin.x,
                sample.frame.origin.y,
                sample.frame.size.width,
                sample.frame.size.height,
                sample.velocity.origin.x,
                sample.velocity.origin.y,
                sample.velocity.size.width,
                sample.velocity.size.height,
            ]
        };
        for (actual, expected) in components(actual).into_iter().zip(components(expected)) {
            assert!((actual - expected).abs() < 1e-9, "{actual} != {expected}");
        }
    }

    #[test]
    fn retarget_keeps_position_and_every_velocity_component() {
        let first = RectSpring::new(
            RectSample::stationary(rect(0.0, 0.0, 24.0, 22.0)),
            rect(80.0, 40.0, 60.0, 50.0),
            0.24,
        );
        let published = first.sample(0.04);
        let reversed = RectSpring::new(published, rect(0.0, 40.0, 24.0, 22.0), 0.24);
        assert_eq!(reversed.sample(0.0), published);
        assert!(reversed.sample(0.00001).frame.origin.x > published.frame.origin.x);
        assert!(reversed.sample(0.00001).frame.size.width > published.frame.size.width);
        assert!(reversed.sample(0.08).velocity.origin.x < 0.0);
    }

    #[test]
    fn retargeting_an_unchanged_destination_keeps_the_trajectory() {
        let target = rect(80.0, 40.0, 60.0, 50.0);
        let first =
            RectSpring::new(RectSample::stationary(rect(0.0, 0.0, 24.0, 22.0)), target, 0.24);
        let redirected = RectSpring::new(first.sample(0.037), target, 0.24);
        for time in [0.0, 0.003, 0.02, 0.08, 0.3] {
            assert_near(redirected.sample(time), first.sample(time + 0.037));
        }
    }

    #[test]
    fn large_moves_wait_until_all_edges_and_velocities_settle() {
        let target = rect(6000.0, 0.0, 100.0, 100.0);
        let spring = RectSpring::new(
            RectSample::stationary(rect(0.0, 0.0, 100.0, 100.0)),
            target,
            0.24,
        );
        assert!(!spring.settled(spring.sample(0.24), 2.0));
        assert!(spring.settled(spring.sample(0.4), 2.0));
        assert_eq!(spring.sample(1000.0), RectSample::stationary(target));
        let overlapping_edges = RectSample {
            frame: rect(0.1, 0.0, 100.1, 100.0),
            velocity: RectVelocity::default(),
        };
        let still = RectSpring::new(
            RectSample::stationary(rect(0.0, 0.0, 100.0, 100.0)),
            rect(0.0, 0.0, 100.0, 100.0),
            0.24,
        );
        assert!(!still.settled(overlapping_edges, 2.0));
        let moving = RectSample {
            frame: still.target(),
            velocity: RectVelocity {
                origin: CGPoint::new(10.0, 0.0),
                size: CGSize::ZERO,
            },
        };
        assert!(!still.settled(moving, 2.0));
    }

    #[test]
    fn sampling_depends_on_elapsed_time_and_handles_long_pauses() {
        let spring = RectSpring::new(
            RectSample::stationary(rect(0.0, 0.0, 24.0, 22.0)),
            rect(80.0, 0.0, 24.0, 22.0),
            0.24,
        );
        let direct = spring.sample(0.1);
        for step in 0..100 {
            let _ = spring.sample(step as f64 / 1000.0);
        }
        assert_eq!(spring.sample(0.1), direct);
        assert_eq!(
            spring.sample(f64::INFINITY),
            RectSample::stationary(spring.target())
        );
    }

    #[test]
    fn envelope_contains_reversals_and_clamped_sizes() {
        for initial in [
            RectSample {
                frame: rect(40.0, 20.0, 24.0, 22.0),
                velocity: RectVelocity {
                    origin: CGPoint::new(800.0, -600.0),
                    size: CGSize::new(200.0, -100.0),
                },
            },
            RectSample {
                frame: rect(40.0, 20.0, 2.0, 2.0),
                velocity: RectVelocity {
                    origin: CGPoint::new(-100.0, 100.0),
                    size: CGSize::new(-800.0, -900.0),
                },
            },
        ] {
            let spring = RectSpring::new(initial, rect(0.0, 0.0, 24.0, 22.0), 0.24);
            let envelope = spring.envelope();
            for step in 0..10000 {
                let sample = spring.sample(step as f64 / 10000.0);
                assert!(sample.frame.size.width >= 1.0 && sample.frame.size.height >= 1.0);
                if sample.frame.size.width == 1.0 {
                    assert_eq!(sample.velocity.size.width, 0.0);
                }
                if sample.frame.size.height == 1.0 {
                    assert_eq!(sample.velocity.size.height, 0.0);
                }
                assert!(sample.frame.origin.x >= envelope.origin.x - 1e-9);
                assert!(sample.frame.origin.y >= envelope.origin.y - 1e-9);
                assert!(
                    sample.frame.origin.x + sample.frame.size.width
                        <= envelope.origin.x + envelope.size.width + 1e-9
                );
                assert!(
                    sample.frame.origin.y + sample.frame.size.height
                        <= envelope.origin.y + envelope.size.height + 1e-9
                );
            }
        }
    }
}
