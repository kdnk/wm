// Modified by kdnk for wm on 2026-11-19. Original attribution is preserved in LICENSE.
use objc2_core_foundation::CGRect;
use tokio::sync::watch;

use crate::actor::window_notify;
use crate::common::collections::BTreeMap;
use crate::common::config::WindowBorderSettings;
use crate::model::hidden_window_placement::HiddenWindowPlacement;
use crate::sys::geometry::CGRectExt;
use crate::sys::screen::SpaceId;
use crate::sys::window_server::{WindowIterator, WindowServerId};
use crate::ui::window_borders::{BorderTarget, WindowBorders};

#[derive(Clone, Copy, PartialEq)]
pub(super) struct BorderWindow {
    pub(super) space: SpaceId,
    pub(super) screen: CGRect,
    pub(super) dialog_parent: Option<WindowServerId>,
}

#[derive(Default, Clone)]
struct Scene {
    windows: BTreeMap<WindowServerId, BorderWindow>,
    focused: Option<WindowServerId>,
    settings: WindowBorderSettings,
    order_revision: u64,
}

#[derive(Clone)]
pub struct Publisher(watch::Sender<Scene>);

pub struct Observer(watch::Receiver<Scene>);

pub fn channel() -> (Publisher, Observer) {
    let (tx, rx) = watch::channel(Scene::default());
    (Publisher(tx), Observer(rx))
}

impl Publisher {
    pub(super) fn set_windows(
        &self,
        settings: WindowBorderSettings,
        windows: BTreeMap<WindowServerId, BorderWindow>,
    ) {
        self.0.send_if_modified(|scene| {
            if scene.settings == settings && scene.windows == windows {
                return false;
            }
            if windows
                .iter()
                .any(|(id, display)| scene.windows.get(id).is_some_and(|old| old != display))
            {
                scene.order_revision = scene.order_revision.wrapping_add(1);
            }
            scene.settings = settings;
            scene.windows = windows;
            true
        });
    }

    pub(super) fn set_focus(&self, focused: Option<WindowServerId>) {
        self.0.send_if_modified(|scene| {
            if scene.focused == focused {
                return false;
            }
            scene.focused = focused;
            scene.order_revision = scene.order_revision.wrapping_add(1);
            !scene.windows.is_empty()
        });
    }

    pub(super) fn window_changed(&self, id: WindowServerId, reorder: bool) {
        self.0.send_if_modified(|scene| {
            if !scene.windows.contains_key(&id) {
                return scene.windows.values().any(|window| window.dialog_parent.is_some());
            }
            if reorder {
                scene.order_revision = scene.order_revision.wrapping_add(1);
            }
            true
        });
    }
}

impl Observer {
    pub async fn run(mut self, notifications: window_notify::Sender) {
        let mut renderer = WindowBorders::default();
        let mut subscribed = false;
        let mut last_order = 0;
        while self.0.changed().await.is_ok() {
            let scene = self.0.borrow_and_update().clone();
            if scene.settings.enabled != subscribed {
                notifications.send(window_notify::Request::WindowBorders(scene.settings.enabled));
                subscribed = scene.settings.enabled;
            }
            let ids: Vec<_> = scene.windows.keys().copied().collect();
            let mut targets = Vec::with_capacity(ids.len());
            if let Some(query) = WindowIterator::new(&ids) {
                while let Some(window) = query.advance() {
                    let id = WindowServerId::new(window.window_id());
                    let frame = window.bounds();
                    if let Some(target) = scene.windows.get(&id)
                        && window.alpha() > 0.0
                    {
                        let visible = border_on_screen(target.screen, frame);
                        if !visible && target.dialog_parent.is_none() {
                            continue;
                        }
                        targets.push(BorderTarget {
                            window: id,
                            frame,
                            dialog_parent: target.dialog_parent,
                            visible,
                        });
                    }
                }
            }
            targets.sort_by_key(|target| target.dialog_parent.is_none());
            renderer.sync(
                &targets,
                scene.focused,
                &scene.settings,
                scene.order_revision != last_order,
            );
            last_order = scene.order_revision;
        }
    }
}

fn border_on_screen(screen: CGRect, frame: CGRect) -> bool {
    let overlap = CGRectExt::intersection(&screen, &frame);
    overlap.size.width > HiddenWindowPlacement::REVEAL_PX
        && overlap.size.height > HiddenWindowPlacement::REVEAL_PX
}

#[cfg(test)]
mod tests {
    use objc2_core_foundation::{CGPoint, CGSize};

    use super::*;

    #[test]
    fn border_wakes_only_for_targets_and_keeps_native_focus_before_discovery() {
        let (publisher, mut observer) = channel();
        let id = WindowServerId(7);
        publisher.set_focus(Some(id));
        publisher.set_windows(
            WindowBorderSettings {
                enabled: true,
                ..Default::default()
            },
            BTreeMap::from([(id, BorderWindow {
                space: SpaceId::new(1),
                screen: CGRect::ZERO,
                dialog_parent: None,
            })]),
        );
        assert_eq!(observer.0.borrow_and_update().focused, Some(id));
        publisher.window_changed(WindowServerId(8), true);
        assert!(!observer.0.has_changed().unwrap());
        publisher.window_changed(id, false);
        assert!(observer.0.has_changed().unwrap());
        assert_eq!(observer.0.borrow_and_update().order_revision, 1);
        publisher.window_changed(id, true);
        assert_eq!(observer.0.borrow_and_update().order_revision, 2);
        publisher.set_windows(
            WindowBorderSettings {
                enabled: true,
                ..Default::default()
            },
            BTreeMap::from([(id, BorderWindow {
                space: SpaceId::new(2),
                screen: CGRect::ZERO,
                dialog_parent: None,
            })]),
        );
        assert_eq!(observer.0.borrow_and_update().order_revision, 3);
        publisher.set_focus(None);
        assert_eq!(observer.0.borrow().focused, None);
    }

    #[test]
    fn border_skips_one_point_parked_windows() {
        let screen = CGRect::new(CGPoint::ZERO, CGSize::new(1440.0, 800.0));
        let frame = |x| CGRect::new(CGPoint::new(x, 50.0), CGSize::new(300.0, 200.0));
        assert!(!border_on_screen(screen, frame(1439.0)));
        assert!(border_on_screen(screen, frame(1420.0)));
    }

    #[test]
    fn a_dialog_border_checks_occlusion_when_an_untracked_window_changes() {
        let (publisher, mut observer) = channel();
        publisher.set_windows(
            WindowBorderSettings {
                enabled: true,
                ..Default::default()
            },
            BTreeMap::from([(WindowServerId(7), BorderWindow {
                space: SpaceId::new(1),
                screen: CGRect::ZERO,
                dialog_parent: Some(WindowServerId(8)),
            })]),
        );
        let order_revision = observer.0.borrow_and_update().order_revision;
        publisher.window_changed(WindowServerId(9), true);
        assert!(observer.0.has_changed().unwrap());
        assert_eq!(observer.0.borrow_and_update().order_revision, order_revision);
    }
}
