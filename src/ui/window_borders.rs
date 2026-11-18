// Modified by kdnk for wm on 2026-11-19. Original attribution is preserved in LICENSE.
use cgs::compositor::{
    CgsWindow, CgsWindowError, WindowSurface, WindowTransaction, with_disabled_actions,
};
use objc2::rc::Retained;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGColor, CGColorSpace, kCGColorSpaceSRGB};
use objc2_quartz_core::CALayer;

use crate::common::collections::hash_map::Entry;
use crate::common::collections::{HashMap, HashSet};
use crate::common::config::WindowBorderSettings;
use crate::sys::screen::SpaceId;
use crate::sys::skylight::SLSWindowTags;
use crate::sys::window_server::{self, WindowIterator, WindowServerId};

#[derive(Default)]
pub struct WindowBorders {
    borders: HashMap<WindowServerId, Border>,
}

impl WindowBorders {
    pub fn sync(
        &mut self,
        targets: &[(WindowServerId, CGRect)],
        focused: Option<WindowServerId>,
        settings: &WindowBorderSettings,
        reorder: bool,
    ) {
        if !settings.enabled {
            self.borders.clear();
            return;
        }

        let mut visible = HashSet::default();
        for &(target, frame) in targets {
            let color = if focused == Some(target) {
                settings.active_color
            } else {
                settings.inactive_color
            };
            if color >> 24 == 0 || frame.size.width <= 0.0 || frame.size.height <= 0.0 {
                continue;
            }
            visible.insert(target);
            let style = BorderStyle::new(frame, settings.width, color);
            let result = match self.borders.entry(target) {
                Entry::Vacant(entry) => Border::new(target, style).map(|border| {
                    entry.insert(border);
                }),
                Entry::Occupied(mut entry) => entry.get_mut().update(target, style, reorder),
            };
            if let Err(error) = result {
                visible.remove(&target);
                tracing::debug!(?target, %error, "could not display window border");
            }
        }
        self.borders.retain(|target, _| visible.contains(target));
    }
}

#[derive(Clone, Copy, PartialEq)]
struct BorderStyle {
    frame: CGRect,
    width: f64,
    color: u32,
}

impl BorderStyle {
    fn new(frame: CGRect, width: f64, color: u32) -> Self {
        Self {
            frame: CGRect::new(
                CGPoint::new(frame.origin.x - width, frame.origin.y - width),
                CGSize::new(frame.size.width + width * 2.0, frame.size.height + width * 2.0),
            ),
            width,
            color,
        }
    }

    fn paint(self, layer: &CALayer, inner_radius: f64) {
        let [alpha, red, green, blue] =
            self.color.to_be_bytes().map(|value| f64::from(value) / 255.0);
        let color = CGColor::new_srgb(red, green, blue, alpha);
        layer.setBorderColor(Some(&color));
        layer.setBorderWidth(self.width);
        layer.setCornerRadius(inner_radius + self.width);
    }
}

struct Border {
    // WindowSurface detaches from its window during drop.
    surface: WindowSurface,
    window: CgsWindow,
    layer: Retained<CALayer>,
    style: BorderStyle,
    inner_radius: f64,
    space: Option<SpaceId>,
}

impl Border {
    fn new(target: WindowServerId, style: BorderStyle) -> Result<Self, CgsWindowError> {
        let bounds = CGRect::new(CGPoint::ZERO, style.frame.size);
        // Managed CGS windows can interleave with application windows at the same level.
        let window = CgsWindow::new(bounds)?;
        window.set_resolution(2.0)?;
        window.set_opacity(false)?;
        window.clear_backing(bounds.size)?;
        let tx = WindowTransaction::new()?;
        tx.set_rounded_frame(&window, style.frame, 0.0)?;
        tx.commit();
        window.disable_shadow();
        let inner_radius = WindowIterator::new(&[target])
            .and_then(|query| query.advance()?.corner_radius())
            .unwrap_or(9.0);
        let layer = CALayer::layer();
        with_disabled_actions(|| {
            layer.setAnchorPoint(CGPoint::ZERO);
            layer.setPosition(CGPoint::ZERO);
            layer.setBounds(bounds);
            layer.setContentsScale(2.0);
            layer.setOpaque(false);
            style.paint(&layer, inner_radius);
        });
        let surface = WindowSurface::new(window.id(), bounds, &layer)?;
        if let Some(srgb) = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB })) {
            surface.set_color_space(&srgb)?;
        }
        surface.flush();
        let mut border = Self {
            surface,
            window,
            layer,
            style,
            inner_radius,
            space: None,
        };
        border.order_above(target)?;
        Ok(border)
    }

    fn order_above(&mut self, target: WindowServerId) -> Result<(), CgsWindowError> {
        let sticky = SLSWindowTags::ON_ALL_WORKSPACES.bits();
        if window_server::window_tags(target).is_some_and(|tags| tags & sticky != 0) {
            self.window.set_tags(sticky)?;
            self.space = None;
        } else {
            self.window.clear_tags(sticky)?;
            if let Some(space) = window_server::window_space(target)
                && self.space != Some(space)
            {
                window_server::move_window_to_space(WindowServerId(self.window.id()), space)
                    .map_err(CgsWindowError::Property)?;
                self.space = Some(space);
            }
        }
        if let Some(level) = window_server::window_level(target.as_u32()) {
            self.window.set_level(level as i32)?;
            self.window.set_sublevel(window_server::window_sub_level(target.as_u32()))?;
        }
        self.window.order_above(Some(target.as_u32()))
    }

    fn update(
        &mut self,
        target: WindowServerId,
        style: BorderStyle,
        reorder: bool,
    ) -> Result<(), CgsWindowError> {
        if style.frame != self.style.frame {
            let tx = WindowTransaction::new()?;
            if style.frame.size == self.style.frame.size {
                tx.move_window(&self.window, style.frame.origin);
            } else {
                tx.set_rounded_frame(&self.window, style.frame, 0.0)?;
                let bounds = CGRect::new(CGPoint::ZERO, style.frame.size);
                self.surface.set_bounds_in(&tx, bounds);
                with_disabled_actions(|| self.layer.setBounds(bounds));
                self.surface.flush();
            }
            tx.commit();
        }
        if style.color != self.style.color || style.width != self.style.width {
            with_disabled_actions(|| style.paint(&self.layer, self.inner_radius));
            self.surface.flush();
        }
        self.style = style;
        if reorder {
            self.order_above(target)?;
        }
        Ok(())
    }
}
