// Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
use std::ptr::NonNull;
use std::time::{Duration, Instant};

use objc2_app_kit::NSScreen;
use objc2_application_services::{AXError, AXUIElement as RawAXUIElement, AXValue, AXValueType};
use objc2_core_foundation::{
    CFBoolean, CFDictionary, CFRetained, CFString, CFType, CGPoint, CGSize,
};
use objc2_foundation::{NSPoint, NSRect, NSSize};

use crate::actor::menu_bar::DisplayWorkspaces;
use crate::sys::axuielement::{AXUIElement, Result as AXResult};

pub(super) struct NotchArea {
    pub left: NSRect,
}

pub(super) fn area_for(display: &DisplayWorkspaces, screen: &NSScreen) -> Option<NotchArea> {
    if !objc2::available!(macos = 12.0) || !display.is_builtin {
        return None;
    }
    let left = screen.auxiliaryTopLeftArea();
    let right = screen.auxiliaryTopRightArea();
    (left.size.width > 0.0 && right.size.width > 0.0 && left.size.height >= 22.0)
        .then_some(NotchArea { left })
}

pub(super) fn session_unlocked() -> bool {
    let Some(session) = objc2_core_graphics::CGSessionCopyCurrentDictionary() else {
        return false;
    };
    let session: CFRetained<CFDictionary<CFString, CFType>> =
        unsafe { CFRetained::cast_unchecked(session) };
    let key = CFString::from_static_str("CGSSessionScreenIsLocked");
    !session
        .get(&key)
        .and_then(|value| value.downcast::<CFBoolean>().ok())
        .is_some_and(|value| value.value())
}

const MENU_READ_BUDGET: Duration = Duration::from_millis(75);

struct MenuRead {
    pid: i32,
    started: Instant,
}

impl MenuRead {
    fn remaining_at(&self, now: Instant) -> Option<Duration> {
        MENU_READ_BUDGET
            .checked_sub(now.duration_since(self.started))
            .filter(|remaining| !remaining.is_zero())
    }

    fn read<T>(
        &self,
        element: &AXUIElement,
        attribute: &'static str,
        operation: impl FnOnce() -> AXResult<T>,
    ) -> Option<T> {
        let now = Instant::now();
        let Some(remaining) = self.remaining_at(now) else {
            tracing::debug!(
                pid = self.pid,
                attribute,
                stage = "before_rpc",
                elapsed_ms = now.duration_since(self.started).as_millis(),
                "Wm menu read deadline expired"
            );
            return None;
        };
        let status = unsafe { element.retained().set_messaging_timeout(remaining.as_secs_f32()) };
        if status != AXError::Success {
            tracing::debug!(pid = self.pid, attribute, stage = "set_timeout", error = ?status,
                elapsed_ms = self.started.elapsed().as_millis(), "Wm menu read failed");
            return None;
        }
        let result = operation();
        let finished = Instant::now();
        if self.remaining_at(finished).is_none() {
            tracing::debug!(
                pid = self.pid,
                attribute,
                stage = "after_rpc",
                elapsed_ms = finished.duration_since(self.started).as_millis(),
                "Wm menu read deadline expired"
            );
            return None;
        }
        match result {
            Ok(value) => Some(value),
            Err(error) => {
                tracing::debug!(pid = self.pid, attribute, stage = "rpc", error = ?error,
                    elapsed_ms = finished.duration_since(self.started).as_millis(), "Wm menu read failed");
                None
            }
        }
    }

    fn element_attribute(&self, element: &AXUIElement, name: &'static str) -> Option<AXUIElement> {
        let value = self
            .read(element, name, || element.attribute(name))??
            .downcast::<RawAXUIElement>()
            .ok()?;
        Some(unsafe { AXUIElement::from_get_rule(CFRetained::as_ptr(&value).as_ptr()) })
    }

    fn item_frame(&self, element: &AXUIElement) -> Option<NSRect> {
        if let Some(frame) = self.read(element, "AXFrame", || element.frame()) {
            return Some(frame);
        }
        let position = self
            .read(element, "AXPosition", || element.attribute("AXPosition"))??
            .downcast::<AXValue>()
            .ok()?;
        let size = self
            .read(element, "AXSize", || element.attribute("AXSize"))??
            .downcast::<AXValue>()
            .ok()?;
        let mut point = CGPoint::ZERO;
        let mut dimensions = CGSize::ZERO;
        if !unsafe { position.value(AXValueType::CGPoint, NonNull::from(&mut point).cast()) }
            || !unsafe { size.value(AXValueType::CGSize, NonNull::from(&mut dimensions).cast()) }
        {
            return None;
        }
        Some(NSRect::new(point, dimensions))
    }
}

pub(super) fn menu_width_for_pid(pid: i32) -> Option<f64> {
    let read = MenuRead { pid, started: Instant::now() };
    let element = AXUIElement::application(pid);
    let menu = read.element_attribute(&element, "AXMenuBar")?;
    let origin = read.item_frame(&menu)?.origin.x;
    let children = read.read(&menu, "AXChildren", || menu.children())?;
    if children.is_empty() {
        tracing::debug!(
            pid,
            stage = "empty_children",
            elapsed_ms = read.started.elapsed().as_millis(),
            "Wm menu read failed"
        );
        return None;
    }
    children
        .into_iter()
        .map(|child| {
            read.item_frame(&child).map(|frame| frame.origin.x + frame.size.width - origin)
        })
        .collect::<Option<Vec<_>>>()?
        .into_iter()
        .reduce(f64::max)
}

pub(super) fn left_menu_domain(left: NSRect, menu_width: f64) -> Option<NSRect> {
    if !menu_width.is_finite() || menu_width < 0.0 || menu_width + 8.0 >= left.size.width {
        return None;
    }
    Some(NSRect::new(
        NSPoint::new(left.origin.x + menu_width + 8.0, left.origin.y),
        NSSize::new(left.size.width - menu_width - 8.0, left.size.height),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn menu_bar_menu_read_uses_remaining_operation_budget_instead_of_a_short_rpc_cap() {
        let started = Instant::now();
        let read = MenuRead { pid: 42, started };
        assert_eq!(read.remaining_at(started), Some(Duration::from_millis(75)));
        assert_eq!(
            read.remaining_at(started + Duration::from_millis(20)),
            Some(Duration::from_millis(55))
        );
        assert_eq!(
            read.remaining_at(started + Duration::from_millis(45)),
            Some(Duration::from_millis(30))
        );
    }

    #[test]
    fn menu_bar_menu_read_fallback_rpcs_share_one_deadline() {
        let started = Instant::now();
        let read = MenuRead { pid: 42, started };
        let frame_allowance = read.remaining_at(started + Duration::from_millis(20)).unwrap();
        let position_allowance = read.remaining_at(started + Duration::from_millis(50)).unwrap();
        let size_allowance = read.remaining_at(started + Duration::from_millis(74)).unwrap();
        assert_eq!(frame_allowance, Duration::from_millis(55));
        assert_eq!(position_allowance, Duration::from_millis(25));
        assert_eq!(size_allowance, Duration::from_millis(1));
        assert_eq!(read.remaining_at(started + MENU_READ_BUDGET), None);
        assert_eq!(read.remaining_at(started + Duration::from_millis(100)), None);
    }

    #[test]
    fn menu_bar_notch_domain_reserves_the_foreground_menu_prefix() {
        let left = NSRect::new(NSPoint::new(400.0, -64.0), NSSize::new(1200.0, 64.0));
        let domain = left_menu_domain(left, 700.0).unwrap();
        assert_eq!(domain.origin, NSPoint::new(1108.0, -64.0));
        assert_eq!(domain.size, NSSize::new(492.0, 64.0));
        assert!(left_menu_domain(left, f64::NAN).is_none());
        assert!(left_menu_domain(left, -1.0).is_none());
        assert!(left_menu_domain(left, 1200.0).is_none());
    }
}
