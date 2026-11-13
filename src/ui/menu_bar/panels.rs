use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, Instant};

use cgs::menu::StatusItem;
use objc2::MainThreadOnly;
use objc2::rc::Retained;
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSPanel, NSRunningApplication, NSScreen, NSView,
    NSWindowCollectionBehavior, NSWindowStyleMask, NSWorkspace,
};
use objc2_core_foundation::{CFArray, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_graphics::{
    CGWindowListCopyWindowInfo, CGWindowListOption, kCGNullWindowID, kCGWindowBounds,
    kCGWindowLayer, kCGWindowOwnerPID,
};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize};

use super::window_list::{
    StripPresentation, WindowListView, WorkspaceWindows, viewport_width, workspace_windows,
};
use super::{
    BuiltStatusMenu, MenuActionHandler, MenuShortcuts, add_action_item, build_static_menu,
    layout_library_files_in, parse_layout_mode, rebuild_restore_menu, set_menu_item_checked,
    set_menu_item_hotkey, workspace_menu_title,
};
use crate::actor::menu_bar::DisplayWorkspaces;
use crate::actor::wm_controller::WmCommand;
use crate::common::config::MenuBarSettings;
use crate::sys::hotkey::Hotkey;
use crate::sys::screen::{NSScreenExt, ScreenId, SpaceId};

struct Controls {
    built: BuiltStatusMenu,
    handler: Rc<MenuActionHandler>,
}

impl Controls {
    fn new(
        display: &DisplayWorkspaces,
        parent: &MenuActionHandler,
        hotkeys: &[(Hotkey, WmCommand)],
        folder: &Path,
        mtm: MainThreadMarker,
    ) -> Self {
        let handler = parent.scoped(display.space);
        handler.set_layout_folder(folder.to_path_buf());
        let built = build_static_menu(mtm, &handler);
        let shortcuts = MenuShortcuts::from_hotkeys(hotkeys);
        set_menu_item_hotkey(&built.next_workspace_item, shortcuts.next_workspace.as_ref());
        set_menu_item_hotkey(&built.prev_workspace_item, shortcuts.prev_workspace.as_ref());
        set_menu_item_hotkey(&built.tiling_item, shortcuts.toggle_space_activation.as_ref());
        set_menu_item_hotkey(&built.reload_item, shortcuts.reload_config.as_ref());
        set_menu_item_hotkey(&built.quit_item, shortcuts.quit_rift.as_ref());
        for workspace in &display.workspaces {
            let item = add_action_item(
                &built.workspace_submenu,
                mtm,
                &handler,
                &workspace_menu_title(workspace),
                MenuActionHandler::on_switch_workspace,
            );
            item.setTag(workspace.index as isize);
            set_menu_item_checked(&item, workspace.is_active);
            set_menu_item_hotkey(
                &item,
                shortcuts
                    .switch_workspace_by_index
                    .get(&workspace.index)
                    .or_else(|| shortcuts.switch_workspace_by_name.get(&workspace.name)),
            );
        }
        built.workspace_item.setEnabled(!display.workspaces.is_empty());
        set_menu_item_checked(&built.tiling_item, display.space_is_activated);
        let active = display
            .workspaces
            .iter()
            .find(|workspace| workspace.is_active)
            .and_then(|workspace| parse_layout_mode(&workspace.layout_mode));
        for (mode, item) in &built.layout_items {
            set_menu_item_checked(item, active == Some(*mode));
        }
        let files = layout_library_files_in(folder);
        handler.set_layout_files(files.iter().map(|(_, path)| path.clone()).collect());
        rebuild_restore_menu(
            mtm,
            &handler,
            &built.restore_workspace_menu,
            &files,
            MenuActionHandler::on_restore_master_file_workspace,
            MenuActionHandler::on_restore_library_workspace,
            MenuActionHandler::on_restore_workspace,
        );
        rebuild_restore_menu(
            mtm,
            &handler,
            &built.restore_space_menu,
            &files,
            MenuActionHandler::on_restore_master_file_space,
            MenuActionHandler::on_restore_library_space,
            MenuActionHandler::on_restore_space,
        );
        Self { built, handler }
    }
}

struct Panel {
    window: Retained<NSPanel>,
    root: Retained<NSView>,
    strip: WindowListView,
}
impl Panel {
    fn new(mtm: MainThreadMarker) -> Self {
        let window = NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(mtm),
            NSRect::ZERO,
            NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
            NSBackingStoreType::Buffered,
            false,
        );
        unsafe {
            window.setReleasedWhenClosed(false);
        }
        window.setOpaque(false);
        window.setBackgroundColor(Some(&NSColor::clearColor()));
        window.setHasShadow(false);
        window.setHidesOnDeactivate(false);
        window.setBecomesKeyOnlyIfNeeded(true);
        window.setFloatingPanel(true);
        window.setMovable(false);
        window.setExcludedFromWindowsMenu(true);
        window.setLevel(25);
        window.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::IgnoresCycle
                | NSWindowCollectionBehavior::FullScreenAuxiliary,
        );
        let root = NSView::new(mtm);
        window.setContentView(Some(&root));
        Self {
            window,
            root,
            strip: WindowListView::new(mtm),
        }
    }

    fn project(
        &mut self,
        workspaces: &[WorkspaceWindows],
        grouped: bool,
        presentation: StripPresentation,
        controls: &Controls,
    ) {
        unsafe {
            self.root.setMenu(Some(controls.built.menu.ns_menu()));
        }
        self.strip
            .update(workspaces, grouped, presentation, &self.root, &controls.handler);
        self.strip.set_context_menu(controls.built.menu.ns_menu());
    }

    fn show(&self, frame: NSRect, mode: bool) {
        self.root.setFrame(NSRect::new(NSPoint::ZERO, frame.size));
        self.strip.position_after(
            (frame.size.width - self.strip.width()).max(0.0),
            frame.size.height,
        );
        self.strip.set_mode(mode);
        self.window.setFrame_display(frame, true);
        self.window.orderFrontRegardless();
    }

    fn hide(&self) {
        self.strip.stop_motion();
        super::drag::cancel_window(Some(&self.window));
        self.window.orderOut(None);
    }
}
impl Drop for Panel {
    fn drop(&mut self) { self.hide(); }
}

struct DisplayPresentation {
    space: SpaceId,
    controls: Controls,
    signature: Vec<(String, usize, String, String, bool)>,
    activated: bool,
    generation: u64,
    panel: Panel,
}

const MENU_WIDTH_RETRY_GRACE: Duration = Duration::from_millis(250);

struct MenuWidthSnapshot<A> {
    application: A,
    width: f64,
    first_unavailable_at: Option<Instant>,
}

struct MenuWidthSnapshots<A> {
    entries: HashMap<i32, MenuWidthSnapshot<A>>,
}

impl<A> Default for MenuWidthSnapshots<A> {
    fn default() -> Self { Self { entries: HashMap::new() } }
}

impl<A: Clone + PartialEq> MenuWidthSnapshots<A> {
    fn clear(&mut self) { self.entries.clear(); }

    fn resolve(
        &mut self,
        pid: i32,
        application: &A,
        measured: Option<f64>,
        now: Instant,
    ) -> Option<f64> {
        tracing::debug!(pid, measured_width = ?measured,
            cached_width = ?self.entries.get(&pid).map(|snapshot| snapshot.width),
            unavailable_age_ms = ?self.entries.get(&pid).and_then(|snapshot| snapshot.first_unavailable_at).map(|first| now.duration_since(first).as_millis()),
            identity_matches = self.entries.get(&pid).is_some_and(|snapshot| snapshot.application == *application),
            "Rift menu width observed");
        let resolved = if let Some(width) = measured {
            self.entries.insert(pid, MenuWidthSnapshot {
                application: application.clone(),
                width,
                first_unavailable_at: None,
            });
            Some(width)
        } else {
            self.entries
                .get_mut(&pid)
                .filter(|snapshot| snapshot.application == *application)
                .and_then(|snapshot| {
                    let first = snapshot.first_unavailable_at.get_or_insert(now);
                    (now.duration_since(*first) < MENU_WIDTH_RETRY_GRACE).then_some(snapshot.width)
                })
        };
        tracing::debug!(pid, resolved_width = ?resolved, "Rift menu width resolved");
        resolved
    }
}

#[derive(Default)]
pub(super) struct DisplayPanels {
    displays: HashMap<ScreenId, DisplayPresentation>,
    generation: u64,
    menu_widths: MenuWidthSnapshots<Retained<NSRunningApplication>>,
}
impl DisplayPanels {
    pub(super) fn invalidate_controls(&mut self) { self.generation += 1; }

    pub(super) fn is_empty(&self) -> bool { self.displays.is_empty() }

    pub(super) fn clear(&mut self) {
        self.displays.clear();
        self.menu_widths.clear();
    }

    pub(super) fn update(
        &mut self,
        displays: &[DisplayWorkspaces],
        settings: &MenuBarSettings,
        mode: bool,
        parent: &Rc<MenuActionHandler>,
        hotkeys: &[(Hotkey, WmCommand)],
        folder: &Path,
        item: &StatusItem,
        mtm: MainThreadMarker,
    ) {
        self.displays.retain(|id, _| {
            let keep = displays.iter().any(|display| display.screen_id == *id);
            if !keep { tracing::debug!(screen = ?id, display_count = displays.len(), "Rift display panels removed"); }
            keep
        });
        let unlocked = super::notch::session_unlocked();
        if !unlocked || displays.is_empty() {
            self.menu_widths.clear();
        } else {
            self.menu_widths
                .entries
                .retain(|_, snapshot| !snapshot.application.isTerminated());
        }
        if super::drag::busy() {
            for display in displays {
                for workspace in &display.workspaces {
                    let columns = workspace
                        .columns
                        .iter()
                        .map(|column| super::drag::ColumnIdentity {
                            id: column.id,
                            members: column.members.clone(),
                        })
                        .collect::<Vec<_>>();
                    super::drag::invalidate_if_changed(
                        display.space,
                        workspace.key,
                        if workspace.is_active { &columns } else { &[] },
                    );
                }
                if let Some(entry) = self.displays.get(&display.screen_id) {
                    if unlocked && entry.space == display.space {
                        entry.panel.strip.set_mode(mode);
                    } else {
                        entry.panel.hide();
                    }
                }
            }
            return;
        }

        let native = item.ns_status_item();
        let retiring_native = native.isVisible() || native.length() != 0.0;
        if native.isVisible() {
            native.setVisible(false);
        }
        if native.length() != 0.0 {
            native.setLength(0.0);
        }
        let screens = NSScreen::screens(mtm);
        let windows = native_menu_windows(&screens);
        let generation = self.generation;
        let foreground_application = if unlocked && !displays.is_empty() {
            NSWorkspace::sharedWorkspace().frontmostApplication()
        } else {
            None
        };
        let native_menu_width = if unlocked && !displays.is_empty() {
            foreground_application.and_then(|application| {
                let pid = application.processIdentifier();
                if application.isTerminated() {
                    tracing::debug!(pid, "Rift foreground application terminated");
                    self.menu_widths.entries.remove(&pid);
                    return None;
                }
                let measured = super::notch::menu_width_for_pid(pid);
                if application.isTerminated() {
                    tracing::debug!(pid, "Rift foreground application terminated");
                    self.menu_widths.entries.remove(&pid);
                    return None;
                }
                self.menu_widths.resolve(pid, &application, measured, Instant::now())
            })
        } else {
            None
        };
        for display in displays {
            let Some(screen) =
                screens.iter().find(|screen| screen.get_number() == Ok(display.screen_id))
            else {
                continue;
            };
            let signature = display
                .workspaces
                .iter()
                .map(|w| {
                    (
                        w.id.clone(),
                        w.index,
                        w.name.clone(),
                        w.layout_mode.clone(),
                        w.is_active,
                    )
                })
                .collect::<Vec<_>>();
            let entry =
                self.displays.entry(display.screen_id).or_insert_with(|| DisplayPresentation {
                    space: display.space,
                    controls: Controls::new(display, parent, hotkeys, folder, mtm),
                    signature: signature.clone(),
                    activated: display.space_is_activated,
                    generation,
                    panel: Panel::new(mtm),
                });
            if !super::drag::busy()
                && !entry.controls.handler.tracking.get()
                && (entry.space != display.space
                    || entry.signature != signature
                    || entry.activated != display.space_is_activated
                    || entry.generation != generation)
            {
                entry.controls = Controls::new(display, parent, hotkeys, folder, mtm);
                entry.space = display.space;
                entry.signature = signature;
                entry.activated = display.space_is_activated;
                entry.generation = generation;
            }
            let workspaces = workspace_windows(display, settings);
            let notch = super::notch::area_for(display, &screen);
            let frame = screen.frame();
            let visible = screen.visibleFrame();
            let band_height =
                frame.origin.y + frame.size.height - visible.origin.y - visible.size.height;
            let eligible = unlocked && !retiring_native && band_height >= 20.0;
            let domains = [
                right_menu_domain(
                    frame,
                    visible,
                    notch.as_ref().map(|_| screen.auxiliaryTopRightArea()),
                    native_menu_width,
                ),
                notch.and_then(|area| {
                    native_menu_width
                        .and_then(|width| super::notch::left_menu_domain(area.left, width))
                }),
            ];
            let intervals = domains.map(|domain| {
                domain.filter(|_| eligible).and_then(|domain| {
                    windows.as_ref().and_then(|windows| {
                        free_menu_interval(frame, domain, windows, super::own_pid())
                    })
                })
            });
            let choice = choose_placement(
                intervals,
                viewport_width(display, settings, StripPresentation::Combined),
                viewport_width(display, settings, StripPresentation::WorkspaceOverview),
            );
            if let Some((presentation, frame)) = choice {
                entry.panel.project(
                    &workspaces,
                    settings.group_app_icons,
                    presentation,
                    &entry.controls,
                );
                entry.panel.show(frame, mode);
            } else {
                entry.panel.hide();
            }
        }
    }
}

fn right_menu_domain(
    screen: NSRect,
    visible: NSRect,
    notch: Option<NSRect>,
    native_menu_width: Option<f64>,
) -> Option<NSRect> {
    if let Some(area) = notch {
        return Some(area);
    }
    let menu_width = native_menu_width?;
    let band = screen.origin.y + screen.size.height - visible.origin.y - visible.size.height;
    if !menu_width.is_finite()
        || menu_width < 0.0
        || menu_width + 8.0 >= screen.size.width
        || !band.is_finite()
        || band < 20.0
    {
        return None;
    }
    Some(NSRect::new(
        NSPoint::new(
            screen.origin.x + menu_width + 8.0,
            screen.origin.y + screen.size.height - band,
        ),
        NSSize::new(screen.size.width - menu_width - 8.0, band),
    ))
}

fn choose_placement(
    intervals: [Option<NSRect>; 2],
    combined_width: f64,
    overview_width: f64,
) -> Option<(StripPresentation, NSRect)> {
    [
        (StripPresentation::Combined, combined_width),
        (StripPresentation::WorkspaceOverview, overview_width),
    ]
    .into_iter()
    .find_map(|(presentation, width)| {
        intervals
            .into_iter()
            .flatten()
            .find_map(|interval| fit_right(interval, width).map(|frame| (presentation, frame)))
    })
}

#[derive(Clone, Copy)]
struct NativeMenuWindow {
    frame: NSRect,
    pid: i32,
    layer: i64,
}

fn native_menu_windows(
    screens: &objc2_foundation::NSArray<NSScreen>,
) -> Option<Vec<NativeMenuWindow>> {
    let top = screens.firstObject()?.frame().origin.y + screens.firstObject()?.frame().size.height;
    let windows = CGWindowListCopyWindowInfo(
        CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements,
        kCGNullWindowID,
    )?;
    let windows: CFRetained<CFArray<CFDictionary<CFString, CFType>>> =
        unsafe { CFRetained::cast_unchecked(windows) };
    let number = |dict: &CFDictionary<CFString, CFType>, key: &CFString| {
        dict.get(key)?.downcast::<CFNumber>().ok()?.as_f64()
    };
    Some(
        windows
            .iter()
            .filter_map(|window| {
                let layer = number(&window, unsafe { kCGWindowLayer })? as i64;
                if layer != 25 {
                    return None;
                }
                let pid = number(&window, unsafe { kCGWindowOwnerPID })? as i32;
                let bounds =
                    window.get(unsafe { kCGWindowBounds })?.downcast::<CFDictionary>().ok()?;
                let bounds: CFRetained<CFDictionary<CFString, CFType>> =
                    unsafe { CFRetained::cast_unchecked(bounds) };
                let x = number(&bounds, &CFString::from_static_str("X"))?;
                let y = number(&bounds, &CFString::from_static_str("Y"))?;
                let width = number(&bounds, &CFString::from_static_str("Width"))?;
                let height = number(&bounds, &CFString::from_static_str("Height"))?;
                [x, y, width, height].iter().all(|v| v.is_finite()).then_some(NativeMenuWindow {
                    frame: NSRect::new(
                        NSPoint::new(x, top - y - height),
                        NSSize::new(width, height),
                    ),
                    pid,
                    layer,
                })
            })
            .collect(),
    )
}

fn free_menu_interval(
    screen: NSRect,
    domain: NSRect,
    windows: &[NativeMenuWindow],
    own_pid: i32,
) -> Option<NSRect> {
    let band_top = domain.origin.y + domain.size.height;
    let mut measured = false;
    let occupied = windows
        .iter()
        .filter(|window| {
            let rect = window.frame;
            window.pid != own_pid
                && window.layer == 25
                && (rect.size.height - domain.size.height).abs() <= 2.0
                && (rect.origin.y - domain.origin.y).abs() <= 2.0
                && rect.origin.x >= screen.origin.x - 2.0
                && rect.origin.x + rect.size.width <= screen.origin.x + screen.size.width + 2.0
                && rect.size.width > 0.0
        })
        .filter_map(|window| {
            measured = true;
            let rect = window.frame;
            (rect.origin.x + rect.size.width > domain.origin.x).then_some(rect.origin.x)
        })
        .reduce(f64::min);
    let domain_right = domain.origin.x + domain.size.width;
    let right = occupied.unwrap_or(domain_right).min(domain_right) - 8.0;
    (measured && right > domain.origin.x).then_some(NSRect::new(
        domain.origin,
        NSSize::new(right - domain.origin.x, band_top - domain.origin.y),
    ))
}

fn fit_right(interval: NSRect, width: f64) -> Option<NSRect> {
    (width.is_finite() && width > 0.0 && width <= interval.size.width).then_some(NSRect::new(
        NSPoint::new(
            interval.origin.x + interval.size.width - width,
            interval.origin.y + (interval.size.height - super::window_list::CAPSULE_HEIGHT) / 2.0,
        ),
        NSSize::new(width, super::window_list::CAPSULE_HEIGHT),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn menu_bar_single_strip_prefers_full_right_then_full_left_before_compacting() {
        let right = NSRect::new(NSPoint::new(1600.0, -64.0), NSSize::new(400.0, 64.0));
        let left = NSRect::new(NSPoint::new(1108.0, -64.0), NSSize::new(484.0, 64.0));
        let (presentation, frame) =
            choose_placement([Some(right), Some(left)], 300.0, 100.0).unwrap();
        assert!(presentation == StripPresentation::Combined);
        assert_eq!(frame.origin, NSPoint::new(1700.0, -45.0));
        assert_eq!(frame.size, NSSize::new(300.0, 26.0));
        let (presentation, frame) =
            choose_placement([Some(right), Some(left)], 484.0, 100.0).unwrap();
        assert!(presentation == StripPresentation::Combined);
        assert_eq!(frame.origin.x, 1108.0);
        assert_eq!(frame.origin.x + frame.size.width, 1592.0);
    }

    #[test]
    fn menu_bar_single_strip_compacts_right_then_left_and_hides_without_a_fit() {
        let right = NSRect::new(NSPoint::new(1600.0, -64.0), NSSize::new(100.0, 64.0));
        let left = NSRect::new(NSPoint::new(1108.0, -64.0), NSSize::new(200.0, 64.0));
        let (presentation, frame) =
            choose_placement([Some(right), Some(left)], 300.0, 100.0).unwrap();
        assert!(presentation == StripPresentation::WorkspaceOverview);
        assert_eq!(frame.origin.x, 1600.0);
        let (presentation, frame) =
            choose_placement([Some(right), Some(left)], 300.0, 101.0).unwrap();
        assert!(presentation == StripPresentation::WorkspaceOverview);
        assert_eq!(frame.origin.x, 1207.0);
        assert!(choose_placement([Some(right), Some(left)], 300.0, 201.0).is_none());
        assert!(choose_placement([None, None], 100.0, 50.0).is_none());
        assert!(choose_placement([Some(right), None], f64::NAN, 101.0).is_none());
    }

    #[test]
    fn menu_bar_left_fallback_preserves_menu_notch_and_native_item_gutters() {
        let screen = NSRect::new(NSPoint::new(400.0, -1169.0), NSSize::new(2400.0, 1169.0));
        let left = NSRect::new(NSPoint::new(400.0, -64.0), NSSize::new(1200.0, 64.0));
        let domain = super::super::notch::left_menu_domain(left, 700.0).unwrap();
        let mut windows = [NativeMenuWindow {
            frame: NSRect::new(NSPoint::new(2100.0, -64.0), NSSize::new(38.0, 64.0)),
            pid: 702,
            layer: 25,
        }];
        let interval = free_menu_interval(screen, domain, &windows, 99).unwrap();
        let (_, frame) = choose_placement([None, Some(interval)], 300.0, 100.0).unwrap();
        assert_eq!(frame.origin.x + frame.size.width, 1592.0);
        assert!(frame.origin.x >= 1108.0);
        windows[0].frame.origin.x = 1500.0;
        let interval = free_menu_interval(screen, domain, &windows, 99).unwrap();
        let (_, frame) = choose_placement([None, Some(interval)], 300.0, 100.0).unwrap();
        assert_eq!(frame.origin.x + frame.size.width, 1492.0);
        windows[0].frame.origin.x = 1100.0;
        assert!(free_menu_interval(screen, domain, &windows, 99).is_none());
        assert!(free_menu_interval(screen, domain, &[], 99).is_none());
    }

    #[test]
    fn menu_bar_recent_menu_widths_survive_alternating_app_measurement_failures() {
        let start = Instant::now();
        let mut widths = MenuWidthSnapshots::default();
        assert_eq!(widths.resolve(10, &1, Some(374.0), start), Some(374.0));
        assert_eq!(
            widths.resolve(20, &2, Some(628.0), start + Duration::from_millis(100)),
            Some(628.0)
        );
        assert_eq!(
            widths.resolve(10, &1, None, start + Duration::from_millis(200)),
            Some(374.0)
        );
        assert_eq!(
            widths.resolve(20, &2, None, start + Duration::from_millis(300)),
            Some(628.0)
        );
        assert_eq!(
            widths.resolve(30, &3, None, start + Duration::from_millis(350)),
            None
        );
        assert_eq!(
            widths.resolve(10, &99, None, start + Duration::from_millis(400)),
            None
        );
    }

    #[test]
    fn menu_bar_recent_menu_width_known_app_return_starts_failure_grace() {
        let start = Instant::now();
        let mut widths = MenuWidthSnapshots::default();
        widths.resolve(20, &2, Some(628.0), start);
        widths.resolve(10, &1, Some(374.0), start + Duration::from_millis(400));
        assert_eq!(
            widths.resolve(20, &2, None, start + Duration::from_millis(525)),
            Some(628.0)
        );
        assert_eq!(
            widths.resolve(20, &2, None, start + Duration::from_millis(615)),
            Some(628.0)
        );
        assert_eq!(
            widths.resolve(20, &2, Some(628.0), start + Duration::from_millis(780)),
            Some(628.0)
        );
    }

    #[test]
    fn menu_bar_recent_menu_width_failures_do_not_extend_expiry() {
        let start = Instant::now();
        let mut widths = MenuWidthSnapshots::default();
        widths.resolve(10, &1, Some(374.0), start);
        for millis in [250, 400, 499] {
            assert_eq!(
                widths.resolve(10, &1, None, start + Duration::from_millis(millis)),
                Some(374.0)
            );
        }
        assert_eq!(
            widths.resolve(10, &1, None, start + Duration::from_millis(500)),
            None
        );
        assert_eq!(
            widths.resolve(10, &1, None, start + Duration::from_millis(550)),
            None
        );
        assert_eq!(
            widths.resolve(10, &1, Some(410.0), start + Duration::from_millis(600)),
            Some(410.0)
        );
        assert_eq!(
            widths.resolve(10, &1, None, start + Duration::from_millis(1000)),
            Some(410.0)
        );
        assert_eq!(
            widths.resolve(10, &1, None, start + Duration::from_millis(1249)),
            Some(410.0)
        );
        assert_eq!(
            widths.resolve(10, &1, None, start + Duration::from_millis(1250)),
            None
        );
    }

    #[test]
    fn menu_bar_recent_menu_width_app_switches_do_not_reset_expired_failure_grace() {
        let start = Instant::now();
        let mut widths = MenuWidthSnapshots::default();
        widths.resolve(10, &1, Some(374.0), start);
        widths.resolve(20, &2, Some(628.0), start + Duration::from_millis(100));
        assert_eq!(
            widths.resolve(10, &1, None, start + Duration::from_millis(525)),
            Some(374.0)
        );
        widths.resolve(20, &2, Some(628.0), start + Duration::from_millis(600));
        assert_eq!(
            widths.resolve(10, &1, None, start + Duration::from_millis(774)),
            Some(374.0)
        );
        assert_eq!(
            widths.resolve(20, &2, None, start + Duration::from_millis(775)),
            Some(628.0)
        );
        assert_eq!(
            widths.resolve(10, &1, None, start + Duration::from_millis(775)),
            None
        );
        widths.resolve(20, &2, Some(628.0), start + Duration::from_millis(800));
        assert_eq!(
            widths.resolve(10, &1, None, start + Duration::from_millis(900)),
            None
        );
        widths.resolve(10, &1, Some(410.0), start + Duration::from_millis(1000));
        assert_eq!(
            widths.resolve(10, &1, None, start + Duration::from_millis(1500)),
            Some(410.0)
        );
    }

    #[test]
    fn menu_bar_recent_menu_width_fresh_no_fit_overrides_fallback_and_clear_forgets_apps() {
        let start = Instant::now();
        let mut widths = MenuWidthSnapshots::default();
        let left = NSRect::new(NSPoint::ZERO, NSSize::new(899.0, 43.0));
        let measured = widths.resolve(10, &1, Some(374.0), start).unwrap();
        assert!(
            super::super::notch::left_menu_domain(left, measured)
                .and_then(|domain| fit_right(domain, 190.0))
                .is_some()
        );
        assert_eq!(
            widths.resolve(10, &1, None, start + Duration::from_millis(50)),
            Some(374.0)
        );
        assert_eq!(
            widths.resolve(10, &1, None, start + Duration::from_millis(300)),
            None
        );
        let measured =
            widths.resolve(10, &1, Some(890.0), start + Duration::from_millis(400)).unwrap();
        assert!(
            super::super::notch::left_menu_domain(left, measured)
                .and_then(|domain| fit_right(domain, 38.0))
                .is_none()
        );
        widths.resolve(20, &2, Some(628.0), start + Duration::from_millis(450));
        widths.clear();
        assert_eq!(
            widths.resolve(10, &1, None, start + Duration::from_millis(500)),
            None
        );
        assert_eq!(
            widths.resolve(20, &2, None, start + Duration::from_millis(500)),
            None
        );
    }

    #[test]
    fn menu_bar_external_domain_uses_native_menu_prefix_without_a_managed_main_window() {
        let screen = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(2560.0, 1440.0));
        let visible = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(2560.0, 1409.0));
        let measured_native_menu = 420.0;
        let domain = right_menu_domain(screen, visible, None, Some(measured_native_menu)).unwrap();
        assert_eq!(domain.origin, NSPoint::new(428.0, 1409.0));
        let microphone = NativeMenuWindow {
            frame: NSRect::new(NSPoint::new(2018.0, 1410.0), NSSize::new(48.0, 30.0)),
            pid: 702,
            layer: 25,
        };
        let panel = fit_right(
            free_menu_interval(screen, domain, &[microphone], 99).unwrap(),
            258.0,
        )
        .unwrap();
        assert_eq!(panel.origin.x + panel.size.width, 2010.0);
        assert!(panel.origin.x > measured_native_menu);
        assert!(right_menu_domain(screen, visible, None, None).is_none());
    }

    #[test]
    fn menu_bar_panels_use_each_displays_free_interval_before_native_items() {
        let external = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(2560.0, 1440.0));
        let builtin = NSRect::new(NSPoint::new(416.0, -1169.0), NSSize::new(1800.0, 1169.0));
        let external_domain = NSRect::new(NSPoint::new(600.0, 1409.0), NSSize::new(1960.0, 31.0));
        let builtin_domain = NSRect::new(NSPoint::new(1426.0, -38.0), NSSize::new(790.0, 38.0));
        let mut windows = vec![
            NativeMenuWindow {
                frame: NSRect::new(NSPoint::new(2061.0, 1410.0), NSSize::new(38.0, 30.0)),
                pid: 702,
                layer: 25,
            },
            NativeMenuWindow {
                frame: NSRect::new(NSPoint::new(1719.0, -39.0), NSSize::new(38.0, 39.0)),
                pid: 702,
                layer: 25,
            },
            NativeMenuWindow {
                frame: NSRect::new(NSPoint::new(1500.0, -32.0), NSSize::new(114.0, 26.0)),
                pid: 99,
                layer: 25,
            },
            NativeMenuWindow {
                frame: builtin_domain,
                pid: 401,
                layer: 24,
            },
        ];
        let ext = fit_right(
            free_menu_interval(external, external_domain, &windows, 99).unwrap(),
            258.0,
        )
        .unwrap();
        let internal = fit_right(
            free_menu_interval(builtin, builtin_domain, &windows, 99).unwrap(),
            114.0,
        )
        .unwrap();
        assert_eq!(ext.origin.x + ext.size.width, 2053.0);
        assert_eq!(internal.origin.x + internal.size.width, 1711.0);
        assert!(internal.origin.x + internal.size.width < windows[1].frame.origin.x);
        let same = internal;
        let wider = fit_right(
            free_menu_interval(external, external_domain, &windows, 99).unwrap(),
            500.0,
        )
        .unwrap();
        assert_eq!(wider.origin.x + wider.size.width, ext.origin.x + ext.size.width);
        assert_eq!(
            fit_right(
                free_menu_interval(builtin, builtin_domain, &windows, 99).unwrap(),
                114.0
            )
            .unwrap(),
            same
        );
        windows[1].frame.origin.x = 1660.0;
        let moved = fit_right(
            free_menu_interval(builtin, builtin_domain, &windows, 99).unwrap(),
            114.0,
        )
        .unwrap();
        assert_eq!(moved.origin.x + moved.size.width, 1652.0);
        assert!(
            fit_right(
                free_menu_interval(builtin, builtin_domain, &windows, 99).unwrap(),
                700.0
            )
            .is_none()
        );
        assert!(free_menu_interval(builtin, builtin_domain, &[], 99).is_none());
    }
    #[test]
    fn menu_bar_placement_ignores_status_items_wholly_before_the_domain() {
        let screen = NSRect::new(NSPoint::ZERO, NSSize::new(1800.0, 1169.0));
        let right = NSRect::new(NSPoint::new(1010.0, 1131.0), NSSize::new(790.0, 38.0));
        let left = NSRect::new(NSPoint::new(400.0, 1131.0), NSSize::new(400.0, 38.0));
        let mut windows = vec![NativeMenuWindow {
            frame: NSRect::new(NSPoint::new(650.0, 1131.0), NSSize::new(38.0, 38.0)),
            pid: 702,
            layer: 25,
        }];
        let interval = free_menu_interval(screen, right, &windows, 99).unwrap();
        assert_eq!(interval.origin, right.origin);
        assert_eq!(interval.size, NSSize::new(782.0, 38.0));
        windows.push(NativeMenuWindow {
            frame: NSRect::new(NSPoint::new(1650.0, 1131.0), NSSize::new(38.0, 38.0)),
            pid: 702,
            layer: 25,
        });
        let interval = free_menu_interval(screen, right, &windows, 99).unwrap();
        assert_eq!(interval.size, NSSize::new(632.0, 38.0));
        let (presentation, frame) = choose_placement(
            [
                Some(interval),
                free_menu_interval(screen, left, &windows, 99),
            ],
            300.0,
            100.0,
        )
        .unwrap();
        assert!(presentation == StripPresentation::Combined);
        assert_eq!(frame.origin.x, 1342.0);
        assert_eq!(frame.origin.x + frame.size.width, 1642.0);
    }

    #[test]
    fn menu_bar_placement_preserves_native_boundary_collisions_and_measurement_requirements() {
        let screen = NSRect::new(NSPoint::ZERO, NSSize::new(1800.0, 1169.0));
        let domain = NSRect::new(NSPoint::new(1010.0, 1131.0), NSSize::new(790.0, 38.0));
        let mut windows = [NativeMenuWindow {
            frame: NSRect::new(NSPoint::new(1000.0, 1131.0), NSSize::new(38.0, 38.0)),
            pid: 702,
            layer: 25,
        }];
        assert!(
            free_menu_interval(screen, domain, &windows, 99).is_none(),
            "a native item straddling the domain start still blocks placement"
        );
        windows[0].frame.origin.x = 972.0;
        let interval = free_menu_interval(screen, domain, &windows, 99).unwrap();
        assert_eq!(
            interval.size,
            NSSize::new(782.0, 38.0),
            "an item ending at the domain start cannot occupy its interior"
        );
        windows[0].frame.origin.x = 1762.0;
        let interval = free_menu_interval(screen, domain, &windows, 99).unwrap();
        assert_eq!(
            interval.origin.x + interval.size.width,
            1754.0,
            "items at the screen edge retain the native-item gutter"
        );
        assert!(free_menu_interval(screen, domain, &[], 99).is_none());
        windows[0].pid = 99;
        assert!(free_menu_interval(screen, domain, &windows, 99).is_none());
        windows[0].pid = 702;
        windows[0].layer = 24;
        assert!(free_menu_interval(screen, domain, &windows, 99).is_none());
    }
}
