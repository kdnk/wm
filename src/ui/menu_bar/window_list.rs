// Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Instant;

use cgs::Ui;
use cgs::controls::Button;
use cgs::menu::{Menu, MenuItem};
use objc2::rc::Retained;
use objc2::{AnyThread, MainThreadOnly, Message};
use objc2_app_kit::{
    NSAccessibility, NSAppearance, NSAppearanceCustomization, NSAppearanceNameAqua,
    NSAppearanceNameDarkAqua, NSAttributedStringNSStringDrawing, NSBaselineOffsetAttributeName,
    NSBezelStyle, NSButtonType, NSCellImagePosition, NSColor, NSCompositingOperation,
    NSControlStateValueOff, NSFont, NSFontAttributeName, NSGlassEffectView, NSGlassEffectViewStyle,
    NSImage, NSImageScaling, NSRunningApplication, NSView, NSVisualEffectBlendingMode,
    NSVisualEffectMaterial, NSVisualEffectState, NSVisualEffectView, NSWorkspace,
};
use objc2_core_graphics::CGPath;
use objc2_foundation::{
    MainThreadMarker, NSArray, NSAttributedString, NSCopying, NSDictionary, NSNumber, NSPoint,
    NSRect, NSRunLoop, NSRunLoopCommonModes, NSSize, NSString, NSTimer,
};
use objc2_quartz_core::{
    CALayer, CAMediaTiming, CAMediaTimingFunction, CAShapeLayer, CATransaction, CATransition,
    kCATransitionFade,
};

use super::{MenuAction, MenuActionHandler, ReactorEvent, own_pid};
use crate::actor::menu_bar::{DisplayWorkspaces, MenuWindow};
use crate::common::config::{MenuBarDisplayMode, MenuBarSettings};
use crate::ui::motion::{RectSample, RectSpring};

const ICON_SIZE: f64 = 18.0;
const BUTTON_WIDTH: f64 = 24.0;
const LIST_GAP: f64 = 6.0;
const WORKSPACE_GAP: f64 = 8.0;
const WORKSPACE_PADDING: f64 = 8.0;
pub(super) const CAPSULE_HEIGHT: f64 = 26.0;
const CONTROL_INSET: f64 = 2.0;
const WORKSPACE_TRANSITION: &str = "transition";
const MAX_VISIBLE_GROUPS: usize = 12;
const STACK_OFFSET: f64 = 7.0;
const MAX_STACK_ICONS: usize = MAX_VISIBLE_GROUPS;

#[derive(Hash, PartialEq, Eq)]
enum ApplicationKey {
    Bundle(String),
    Process(i32),
}

struct WindowGroup<'a> {
    first: &'a MenuWindow,
    additional: Vec<&'a MenuWindow>,
}

impl<'a> WindowGroup<'a> {
    fn windows(&self) -> impl Iterator<Item = &'a MenuWindow> + '_ {
        std::iter::once(self.first).chain(self.additional.iter().copied())
    }

    fn len(&self) -> usize { 1 + self.additional.len() }

    fn focused(&self) -> bool { self.windows().any(|window| window.focused) }
}

fn window_groups(windows: &[MenuWindow], grouped: bool) -> Vec<WindowGroup<'_>> {
    let mut groups: Vec<WindowGroup<'_>> = Vec::new();
    let mut positions: HashMap<ApplicationKey, usize> = HashMap::new();
    for window in windows {
        let key = match window.bundle_id.as_deref().filter(|bundle| !bundle.is_empty()) {
            Some(bundle) => ApplicationKey::Bundle(bundle.to_owned()),
            None => ApplicationKey::Process(window.id.pid),
        };
        if grouped && let Some(&index) = positions.get(&key) {
            groups[index].additional.push(window);
        } else {
            positions.insert(key, groups.len());
            groups.push(WindowGroup {
                first: window,
                additional: Vec::new(),
            });
        }
    }
    groups
}

fn active_groups(windows: &[MenuWindow], grouped: bool) -> Vec<WindowGroup<'_>> {
    if !grouped {
        return window_groups(windows, false);
    }
    #[derive(Hash, PartialEq, Eq)]
    enum Key {
        Column(usize),
        Application(ApplicationKey),
    }
    let mut positions: HashMap<Key, usize> = HashMap::new();
    let mut groups: Vec<WindowGroup<'_>> = Vec::new();
    for window in windows {
        let key = match window.layout_position {
            Some(position) => Key::Column(position.column),
            None => {
                Key::Application(match window.bundle_id.as_deref().filter(|id| !id.is_empty()) {
                    Some(id) => ApplicationKey::Bundle(id.to_owned()),
                    None => ApplicationKey::Process(window.id.pid),
                })
            }
        };
        if let Some(&index) = positions.get(&key) {
            groups[index].additional.push(window);
        } else {
            positions.insert(key, groups.len());
            groups.push(WindowGroup {
                first: window,
                additional: Vec::new(),
            });
        }
    }
    groups
}

fn stack_width(count: usize) -> f64 {
    ICON_SIZE + STACK_OFFSET * count.min(MAX_STACK_ICONS).saturating_sub(1) as f64
}

fn group_button_width(group: &WindowGroup<'_>, grouped: bool) -> f64 {
    if group.len() == 1 {
        BUTTON_WIDTH
    } else if grouped && group.first.layout_position.is_some() {
        stack_width(group.len()) + 6.0
    } else {
        BUTTON_WIDTH + 6.0 + group.len().to_string().len() as f64 * 6.0
    }
}

fn overflow_button_width(count: usize) -> f64 {
    BUTTON_WIDTH.max(8.0 + count.to_string().len() as f64 * 7.0)
}

fn empty_workspace_title(index: usize) -> String { format!("{:02}", index + 1) }

fn empty_workspace_font() -> Retained<NSFont> {
    NSFont::monospacedDigitSystemFontOfSize_weight(10.0, 0.0)
}

fn empty_workspace_width(index: usize) -> f64 {
    let font = empty_workspace_font();
    let attributes = NSDictionary::from_slices(&[unsafe { NSFontAttributeName }], &[
        &*font as &objc2::runtime::AnyObject
    ]);
    let title = unsafe {
        NSAttributedString::initWithString_attributes(
            NSAttributedString::alloc(),
            &NSString::from_str(&empty_workspace_title(index)),
            Some(&attributes),
        )
    };
    CAPSULE_HEIGHT.max(title.size().width.ceil() + 10.0)
}

fn expanded_width(workspace: &WorkspaceWindows, grouped: bool) -> f64 {
    let controls = if workspace.windows.is_empty() {
        empty_workspace_width(workspace.index)
    } else {
        let groups = active_groups(&workspace.windows, grouped);
        let overflow = groups.len().saturating_sub(MAX_VISIBLE_GROUPS);
        groups
            .iter()
            .take(MAX_VISIBLE_GROUPS)
            .map(|group| group_button_width(group, grouped))
            .sum::<f64>()
            + if overflow > 0 {
                overflow_button_width(overflow)
            } else {
                0.0
            }
    };
    controls + workspace.horizontal_padding() * 2.0
}

fn preview_width(workspace: &WorkspaceWindows) -> f64 {
    if workspace.windows.is_empty() {
        return empty_workspace_width(workspace.index);
    }
    let count = window_groups(&workspace.windows, true).len();
    let extra = count.saturating_sub(MAX_STACK_ICONS);
    stack_width(count)
        + 6.0
        + workspace.horizontal_padding() * 2.0
        + if extra > 0 {
            6.0 + (extra.to_string().len() + 1) as f64 * 6.0
        } else {
            0.0
        }
}

fn total_width(widths: impl Iterator<Item = f64>) -> f64 {
    let widths = widths.collect::<Vec<_>>();
    if widths.is_empty() {
        0.0
    } else {
        LIST_GAP + widths.iter().sum::<f64>() + WORKSPACE_GAP * (widths.len() - 1) as f64
    }
}

pub(super) fn viewport_width(
    display: &DisplayWorkspaces,
    settings: &MenuBarSettings,
    presentation: StripPresentation,
) -> f64 {
    if !settings.show_window_list {
        return 0.0;
    }
    let catalog = workspace_catalog(display);
    catalog
        .iter()
        .map(|selected| {
            total_width(
                catalog
                    .iter()
                    .filter(|workspace| {
                        let active = workspace.key == selected.key;
                        (settings.mode != MenuBarDisplayMode::Active || active)
                            && (active || settings.show_empty || !workspace.windows.is_empty())
                    })
                    .map(|workspace| {
                        if presentation == StripPresentation::Combined
                            && workspace.key == selected.key
                        {
                            expanded_width(workspace, settings.group_app_icons)
                        } else {
                            preview_width(workspace)
                        }
                    }),
            )
        })
        .fold(0.0, f64::max)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum StripPresentation {
    Combined,
    WorkspaceOverview,
}

#[derive(Clone, PartialEq, Eq)]
pub(super) struct WorkspaceWindows {
    id: String,
    key: crate::layout_engine::VirtualWorkspaceId,
    columns: Vec<crate::actor::menu_bar::MenuColumn>,
    index: usize,
    name: String,
    active: bool,
    windows: Vec<MenuWindow>,
}

impl WorkspaceWindows {
    fn horizontal_padding(&self) -> f64 {
        if self.windows.is_empty() {
            0.0
        } else {
            WORKSPACE_PADDING
        }
    }
}

fn workspace_catalog(display: &DisplayWorkspaces) -> Vec<WorkspaceWindows> {
    display
        .workspaces
        .iter()
        .map(|workspace| WorkspaceWindows {
            id: workspace.id.clone(),
            key: workspace.key,
            columns: workspace.columns.clone(),
            index: workspace.index,
            name: workspace.name.clone(),
            active: workspace.is_active,
            windows: workspace.window_list.clone(),
        })
        .collect()
}

pub(super) fn workspace_windows(
    display: &DisplayWorkspaces,
    settings: &MenuBarSettings,
) -> Vec<WorkspaceWindows> {
    if !settings.show_window_list {
        return Vec::new();
    }
    workspace_catalog(display)
        .into_iter()
        .filter(|workspace| settings.mode != MenuBarDisplayMode::Active || workspace.active)
        .filter(|workspace| {
            workspace.active || settings.show_empty || !workspace.windows.is_empty()
        })
        .collect()
}

fn window_label(window: &MenuWindow) -> String {
    if window.title.is_empty() || window.title == window.app_name {
        window.app_name.clone()
    } else {
        format!("{}: {}", window.app_name, window.title)
    }
}

#[derive(Clone, Copy)]
enum ControlSelection {
    WindowFocus(bool),
    WorkspaceSelection(bool),
}

struct WindowButton {
    button: Button,
    selection: ControlSelection,
    width: f64,
    gap_before: f64,
}

enum WorkspaceMaterial {
    Glass(Retained<NSGlassEffectView>),
    Visual(Retained<NSVisualEffectView>),
}

fn motion_timer(tick: &block2::RcBlock<dyn Fn(std::ptr::NonNull<NSTimer>)>) -> Retained<NSTimer> {
    unsafe {
        let timer = NSTimer::timerWithTimeInterval_repeats_block(1.0 / 120.0, true, tick);
        NSRunLoop::mainRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes);
        timer
    }
}

struct FocusIndicator {
    layer: Retained<CALayer>,
    target: Cell<Option<NSRect>>,
    sample: Rc<Cell<RectSample>>,
    timer: RefCell<Option<Retained<NSTimer>>>,
}

impl FocusIndicator {
    fn new() -> Self {
        let layer = CALayer::layer();
        layer.setHidden(true);
        layer.setContentsScale(2.0);
        Self {
            layer,
            target: Cell::new(None),
            sample: Rc::new(Cell::new(RectSample::stationary(NSRect::ZERO))),
            timer: RefCell::new(None),
        }
    }

    fn stop(&self) {
        if let Some(timer) = self.timer.borrow_mut().take() {
            timer.invalidate();
        }
        if let Some(frame) = self.target.get() {
            self.apply(frame);
        }
    }

    fn apply(&self, frame: NSRect) {
        self.sample.set(RectSample::stationary(frame));
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        self.layer.setFrame(frame);
        CATransaction::commit();
    }

    fn present(&self, target: Option<NSRect>, animate: bool) {
        let previous = self.target.replace(target);
        if previous == target && animate {
            return;
        }
        if let Some(timer) = self.timer.borrow_mut().take() {
            timer.invalidate();
        }
        self.layer.setHidden(target.is_none());
        let Some(to) = target else {
            self.sample.set(RectSample::stationary(self.layer.frame()));
            return;
        };
        let spring = RectSpring::new(self.sample.get(), to, 0.24);
        let scale = self.layer.contentsScale();
        if !animate || previous.is_none() || spring.settled(self.sample.get(), scale) {
            self.apply(to);
            return;
        }
        let layer = self.layer.clone();
        let sample = self.sample.clone();
        let started = Instant::now();
        let tick = block2::RcBlock::new(move |timer: std::ptr::NonNull<NSTimer>| {
            let next = spring.sample(started.elapsed().as_secs_f64());
            let done = spring.settled(next, scale)
                || super::drag::busy()
                || NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion();
            let next = if done {
                RectSample::stationary(to)
            } else {
                next
            };
            sample.set(next);
            CATransaction::begin();
            CATransaction::setDisableActions(true);
            layer.setFrame(next.frame);
            CATransaction::commit();
            if done {
                unsafe { timer.as_ref() }.invalidate();
            }
        });
        *self.timer.borrow_mut() = Some(motion_timer(&tick));
    }
}

impl Drop for FocusIndicator {
    fn drop(&mut self) { self.stop(); }
}

struct WorkspaceCapsule {
    host: Retained<NSView>,
    material: WorkspaceMaterial,
    outline: Retained<CAShapeLayer>,
    content_host: Retained<NSView>,
    content: Retained<super::drag::DragContent>,
    focus: FocusIndicator,
    workspace: WorkspaceWindows,
    space: crate::sys::screen::SpaceId,
    start: usize,
    end: usize,
    width: f64,
}

impl WorkspaceCapsule {
    fn new(
        mtm: MainThreadMarker,
        start: usize,
        workspace: &WorkspaceWindows,
        handler: &MenuActionHandler,
    ) -> Self {
        let content = super::drag::DragContent::new(
            mtm,
            handler.space.unwrap_or(crate::sys::screen::SpaceId::new(0)),
            workspace.key,
            handler.reactor_tx.clone(),
        );
        let content_host = NSView::new(mtm);
        content_host.setWantsLayer(true);
        let focus = FocusIndicator::new();
        content_host.layer().unwrap().addSublayer(&focus.layer);
        content_host.addSubview(&content);
        let material = if objc2::available!(macos = 26.0) {
            let glass = NSGlassEffectView::new(mtm);
            glass.setStyle(NSGlassEffectViewStyle::Regular);
            glass.setContentView(Some(&content_host));
            WorkspaceMaterial::Glass(glass)
        } else {
            let effect = NSVisualEffectView::new(mtm);
            effect.setMaterial(NSVisualEffectMaterial::Menu);
            effect.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
            effect.setState(NSVisualEffectState::Active);
            effect.addSubview(&content_host);
            WorkspaceMaterial::Visual(effect)
        };
        let host = NSView::new(mtm);
        host.setWantsLayer(true);
        match &material {
            WorkspaceMaterial::Glass(view) => host.addSubview(view),
            WorkspaceMaterial::Visual(view) => host.addSubview(view),
        }
        let outline = CAShapeLayer::new();
        outline.setZPosition(1.0);
        outline.setFillColor(None);
        outline.setLineWidth(0.5);
        outline.setStrokeColor(Some(&NSColor::separatorColor().CGColor()));
        host.layer().unwrap().addSublayer(&outline);
        let result = Self {
            host,
            material,
            outline,
            content_host,
            content,
            focus,
            workspace: workspace.clone(),
            space: handler.space.unwrap_or(crate::sys::screen::SpaceId::new(0)),
            start,
            end: start,
            width: 0.0,
        };
        result.material_view().setWantsLayer(true);
        result
    }

    fn host(&self) -> &NSView { &self.host }

    fn material_view(&self) -> &NSView {
        match &self.material {
            WorkspaceMaterial::Glass(view) => view,
            WorkspaceMaterial::Visual(view) => view,
        }
    }

    fn prepare_geometry(&self, height: f64) {
        self.host.layer().unwrap().setCornerRadius(height / 2.0);
        self.content_host.layer().unwrap().setCornerRadius(height / 2.0);
        match &self.material {
            WorkspaceMaterial::Glass(glass) => {
                glass.setCornerRadius(height / 2.0);
                if let Some(layer) = glass.layer() {
                    layer.setCornerRadius(height / 2.0);
                }
            }
            WorkspaceMaterial::Visual(view) => {
                if let Some(layer) = view.layer() {
                    layer.setCornerRadius(height / 2.0);
                    layer.setMasksToBounds(true);
                }
            }
        }
    }

    fn replace_content(
        &mut self,
        workspace: &WorkspaceWindows,
        handler: &MenuActionHandler,
        animate: bool,
    ) {
        if animate {
            self.focus.present(None, false);
        }
        if let Some(layer) = self.content_host.layer() {
            let key = NSString::from_str(WORKSPACE_TRANSITION);
            if animate && !NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion() {
                let transition = CATransition::new();
                transition.setType(unsafe { kCATransitionFade });
                transition.setDuration(0.18);
                transition.setTimingFunction(Some(
                    &CAMediaTimingFunction::functionWithControlPoints(0.2, 0.8, 0.2, 1.0),
                ));
                layer.addAnimation_forKey(&transition, Some(&key));
            } else if NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion() {
                layer.removeAnimationForKey(&key);
            }
        }
        let content = super::drag::DragContent::new(
            self.content.mtm(),
            self.space,
            workspace.key,
            handler.reactor_tx.clone(),
        );
        content.setFrame(self.content.bounds());
        self.content.removeFromSuperview();
        self.content_host.addSubview(&content);
        self.content = content;
        self.workspace = workspace.clone();
    }

    fn stop_motion(&self) {
        self.focus.stop();
        if let Some(layer) = self.content_host.layer() {
            layer.removeAnimationForKey(&NSString::from_str(WORKSPACE_TRANSITION));
        }
    }

    fn set_mode(&self, mode_active: bool) {
        let appearance = self.host().effectiveAppearance();
        let dark = dark_appearance(&appearance);
        let contrast = NSWorkspace::sharedWorkspace().accessibilityDisplayShouldIncreaseContrast();
        appearance.performAsCurrentDrawingAppearance(&block2::RcBlock::new(|| {
            let color = NSColor::systemBlueColor();
            let mode_wash = (self.workspace.active && mode_active).then(|| {
                NSColor::systemPurpleColor()
                    .colorWithAlphaComponent(if dark { 0.16 } else { 0.10 })
                    .CGColor()
            });
            self.content_host.layer().unwrap().setBackgroundColor(mode_wash.as_deref());
            let wash = self
                .workspace
                .active
                .then(|| color.colorWithAlphaComponent(if dark { 0.16 } else { 0.10 }));
            if let WorkspaceMaterial::Glass(glass) = &self.material {
                glass.setTintColor(wash.as_deref());
            }
            let layer = &self.outline;
            let background = if matches!(self.material, WorkspaceMaterial::Visual(_)) {
                wash.as_ref().map(|color| color.CGColor())
            } else {
                None
            };
            self.material_view().layer().unwrap().setBackgroundColor(background.as_deref());
            layer.setLineWidth(if self.workspace.active {
                if contrast { 2.0 } else { 1.5 }
            } else {
                0.5
            });
            let border = if self.workspace.active {
                color.colorWithAlphaComponent(if contrast {
                    1.0
                } else if dark {
                    0.95
                } else {
                    0.85
                })
            } else {
                NSColor::separatorColor()
            };
            layer.setStrokeColor(Some(&border.CGColor()));
            set_outline_frame(layer, layer.frame());
        }));
    }
}

fn set_outline_frame(outline: &CAShapeLayer, frame: NSRect) {
    CATransaction::begin();
    CATransaction::setDisableActions(true);
    outline.setFrame(frame);
    let width = outline.lineWidth();
    if frame.size.width > width && frame.size.height > width {
        let rect = NSRect::new(
            NSPoint::new(width / 2.0, width / 2.0),
            NSSize::new(frame.size.width - width, frame.size.height - width),
        );
        let radius = rect.size.height / 2.0;
        let path = unsafe { CGPath::with_rounded_rect(rect, radius, radius, std::ptr::null()) };
        outline.setPath(Some(&path));
    } else {
        outline.setPath(None);
    }
    CATransaction::commit();
}

#[derive(Clone)]
struct GeometryFrame {
    host: Retained<NSView>,
    material: Retained<NSView>,
    content: Retained<NSView>,
    outline: Retained<CAShapeLayer>,
    from: NSRect,
    to: NSRect,
}
impl GeometryFrame {
    fn apply(&self, frame: NSRect) {
        self.host.setFrame(frame);
        let bounds = NSRect::new(NSPoint::ZERO, frame.size);
        self.material.setFrame(bounds);
        self.content.setFrame(bounds);
        set_outline_frame(&self.outline, bounds);
        for child in self.content.subviews() {
            child.setFrame(bounds);
        }
        self.host.layoutSubtreeIfNeeded();
    }
}

struct MovingGeometry {
    frame: GeometryFrame,
    spring: RectSpring,
    sample: Cell<RectSample>,
    scale: f64,
}

impl MovingGeometry {
    fn finish(&self) {
        let sample = RectSample::stationary(self.frame.to);
        self.sample.set(sample);
        self.frame.apply(sample.frame);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum GeometryChange {
    Navigate,
    Preserve,
    Immediate,
}

#[derive(Default)]
struct GeometryMotion {
    timer: Option<Retained<NSTimer>>,
    frames: Rc<Vec<MovingGeometry>>,
}
impl GeometryMotion {
    fn cancel(&mut self, finish: bool) {
        if let Some(timer) = self.timer.take() {
            timer.invalidate();
        }
        if finish {
            for frame in self.frames.iter() {
                frame.finish();
            }
        }
    }

    fn present(&mut self, frames: Vec<GeometryFrame>, change: GeometryChange) {
        let unchanged = frames.len() == self.frames.len()
            && frames
                .iter()
                .zip(self.frames.iter())
                .all(|(a, b)| std::ptr::eq(&*a.host, &*b.frame.host) && a.to == b.frame.to);
        if change == GeometryChange::Preserve
            && unchanged
            && self.timer.as_ref().is_some_and(|timer| timer.isValid())
        {
            return;
        }
        let frames = frames
            .into_iter()
            .map(|frame| {
                let mut initial = RectSample::stationary(frame.from);
                if let Some(previous) = self
                    .frames
                    .iter()
                    .find(|previous| std::ptr::eq(&*frame.host, &*previous.frame.host))
                {
                    initial.velocity = previous.sample.get().velocity;
                }
                let scale = frame.host.window().map_or(2.0, |window| window.backingScaleFactor());
                MovingGeometry {
                    spring: RectSpring::new(initial, frame.to, 0.18),
                    sample: Cell::new(initial),
                    scale,
                    frame,
                }
            })
            .collect::<Vec<_>>();
        self.cancel(false);
        self.frames = Rc::new(frames);
        if change != GeometryChange::Navigate
            || self
                .frames
                .iter()
                .all(|frame| frame.spring.settled(frame.sample.get(), frame.scale))
        {
            for frame in self.frames.iter() {
                frame.finish();
            }
            return;
        }
        for frame in self.frames.iter() {
            frame.frame.apply(frame.sample.get().frame);
        }
        let frames = self.frames.clone();
        let started = Instant::now();
        let tick = block2::RcBlock::new(move |timer: std::ptr::NonNull<NSTimer>| {
            let elapsed = started.elapsed().as_secs_f64();
            let immediate = super::drag::busy()
                || NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion();
            let mut done = true;
            for frame in frames.iter() {
                let sample = frame.spring.sample(elapsed);
                if immediate || frame.spring.settled(sample, frame.scale) {
                    frame.finish();
                } else {
                    frame.sample.set(sample);
                    frame.frame.apply(sample.frame);
                    done = false;
                }
            }
            if done {
                unsafe { timer.as_ref() }.invalidate();
            }
        });
        self.timer = Some(motion_timer(&tick));
    }
}

pub(super) struct WindowListView {
    mtm: MainThreadMarker,
    workspaces: Vec<WorkspaceWindows>,
    grouped: bool,
    buttons: Vec<WindowButton>,
    capsules: Vec<WorkspaceCapsule>,
    presentation: StripPresentation,
    icons: HashMap<(i32, Option<String>), Retained<NSImage>>,
    navigation: Cell<bool>,
    geometry: RefCell<GeometryMotion>,
}

impl WindowListView {
    pub(super) fn new(mtm: MainThreadMarker) -> Self {
        Self {
            mtm,
            workspaces: Vec::new(),
            grouped: false,
            buttons: Vec::new(),
            capsules: Vec::new(),
            presentation: StripPresentation::Combined,
            icons: HashMap::new(),
            navigation: Cell::new(false),
            geometry: RefCell::new(GeometryMotion::default()),
        }
    }

    pub(super) fn update(
        &mut self,
        workspaces: &[WorkspaceWindows],
        grouped: bool,
        presentation: StripPresentation,
        parent: &NSView,
        handler: &Rc<MenuActionHandler>,
    ) {
        for workspace in workspaces {
            super::drag::invalidate_if_changed(
                handler.space.unwrap_or(crate::sys::screen::SpaceId::new(0)),
                workspace.key,
                &workspace
                    .columns
                    .iter()
                    .map(|column| super::drag::ColumnIdentity {
                        id: column.id,
                        members: column.members.clone(),
                    })
                    .collect::<Vec<_>>(),
            );
        }
        if super::drag::busy() {
            self.stop_motion();
            return;
        }
        if self.workspaces == workspaces
            && self.grouped == grouped
            && self.presentation == presentation
        {
            return;
        }
        let switched = !self.workspaces.is_empty()
            && self
                .workspaces
                .iter()
                .find(|workspace| workspace.active)
                .map(|workspace| workspace.key)
                != workspaces
                    .iter()
                    .find(|workspace| workspace.active)
                    .map(|workspace| workspace.key);
        self.navigation.set(switched);
        let same_style = self.grouped == grouped && self.presentation == presentation;
        let mut previous =
            std::mem::take(&mut self.capsules).into_iter().map(Some).collect::<Vec<_>>();
        let mut previous_buttons =
            std::mem::take(&mut self.buttons).into_iter().map(Some).collect::<Vec<_>>();
        self.workspaces = workspaces.to_vec();
        self.grouped = grouped;
        self.presentation = presentation;
        let live: HashSet<_> = workspaces
            .iter()
            .flat_map(|workspace| &workspace.windows)
            .map(|window| (window.id.pid, window.bundle_id.clone()))
            .collect();
        self.icons.retain(|key, _| live.contains(key));
        let ui = Ui::new(self.mtm);
        for workspace in workspaces {
            let start = self.buttons.len();
            let space = handler.space.unwrap_or(crate::sys::screen::SpaceId::new(0));
            let matched = previous.iter().position(|capsule| {
                capsule.as_ref().is_some_and(|capsule| {
                    capsule.space == space && capsule.workspace.key == workspace.key
                })
            });
            let retained = matched.and_then(|index| previous[index].take());
            if let Some(mut capsule) = retained.filter(|capsule| capsule.space == space) {
                if same_style && capsule.workspace == *workspace {
                    for old in capsule.start..capsule.end {
                        self.buttons.push(previous_buttons[old].take().unwrap());
                    }
                    capsule.start = start;
                    capsule.end = self.buttons.len();
                    self.capsules.push(capsule);
                    continue;
                }
                capsule.replace_content(workspace, handler, switched);
                capsule.start = start;
                self.capsules.push(capsule);
            } else {
                self.capsules.push(WorkspaceCapsule::new(self.mtm, start, workspace, handler));
            }
            let mut capsule = self.capsules.pop().unwrap();
            if workspace.windows.is_empty() {
                self.add_preview(workspace, &capsule.content, handler, &ui);
            } else if workspace.active && presentation != StripPresentation::WorkspaceOverview {
                self.add_active(workspace, grouped, &capsule.content, handler, &ui);
            } else {
                self.add_preview(workspace, &capsule.content, handler, &ui);
            }
            capsule.end = self.buttons.len();
            capsule.width = workspace.horizontal_padding() * 2.0
                + self.buttons[start..]
                    .iter()
                    .map(|item| item.width + item.gap_before)
                    .sum::<f64>();
            if unsafe { capsule.host().superview() }
                .as_deref()
                .is_none_or(|view| !std::ptr::eq(view, parent))
            {
                parent.addSubview(capsule.host());
            }
            self.capsules.push(capsule);
        }
        for capsule in previous.into_iter().flatten() {
            capsule.host().removeFromSuperview();
        }
        for button in previous_buttons.into_iter().flatten() {
            button.button.ns_button().removeFromSuperview();
        }
        self.refresh_selection();
    }

    fn refresh_selection(&self) {
        for control in &self.buttons {
            set_control_selection(control.button.ns_button(), control.selection);
        }
        for capsule in &self.capsules {
            set_focus_style(&capsule.focus.layer, &capsule.host().effectiveAppearance());
        }
    }

    pub(super) fn stop_motion(&self) {
        self.navigation.set(false);
        self.geometry.borrow_mut().cancel(true);
        for capsule in &self.capsules {
            capsule.stop_motion();
        }
    }

    pub(super) fn set_context_menu(&self, menu: &objc2_app_kit::NSMenu) {
        for capsule in &self.capsules {
            unsafe {
                capsule.host().setMenu(Some(menu));
                capsule.content.setMenu(Some(menu));
            }
        }
        for button in &self.buttons {
            unsafe {
                button.button.ns_button().setMenu(Some(menu));
            }
        }
    }

    pub(super) fn set_mode(&self, active: bool) {
        for capsule in &self.capsules {
            capsule.set_mode(active);
        }
        self.refresh_selection();
    }

    fn add_active(
        &mut self,
        workspace: &WorkspaceWindows,
        grouped: bool,
        parent: &super::drag::DragContent,
        handler: &Rc<MenuActionHandler>,
        ui: &Ui,
    ) {
        let windows = &workspace.windows;
        let groups = active_groups(windows, grouped);
        for group in groups.iter().take(MAX_VISIBLE_GROUPS) {
            let button = if group.len() == 1 {
                let id = group.first.id;
                let actions = handler.action_tx.clone();
                Button::new(ui, "").borderless().on_click(move || {
                    let _ = actions.send(MenuAction::FocusWindow(id));
                })
            } else {
                let menu = self.window_menu(group.windows(), handler);
                popup_button(ui, "", menu)
            };
            let native = button.ns_button();
            native.setBezelStyle(NSBezelStyle::AccessoryBar);
            native.setButtonType(NSButtonType::MomentaryChange);
            let selection = ControlSelection::WindowFocus(group.focused());
            native.setImageScaling(NSImageScaling::ScaleProportionallyDown);
            let column_stack = grouped && group.first.layout_position.is_some() && group.len() > 1;
            if column_stack {
                let image = self.stack_image(group.windows());
                native.setImage(Some(&image));
            } else {
                native.setImage(self.icon(group.first).as_deref());
            };
            let (label, help, width) = if group.len() == 1 {
                native.setImagePosition(NSCellImagePosition::ImageOnly);
                (
                    window_label(group.first),
                    "Focus this window",
                    group_button_width(group, grouped),
                )
            } else if column_stack {
                native.setImagePosition(NSCellImagePosition::ImageOnly);
                let label = group.windows().map(window_label).collect::<Vec<_>>().join("\n");
                (
                    label,
                    "Choose a column window",
                    group_button_width(group, grouped),
                )
            } else {
                let count = group.len().to_string();
                native.setImagePosition(NSCellImagePosition::ImageLeading);
                let font = NSFont::systemFontOfSize(9.0);
                let baseline = NSNumber::new_f64(3.0);
                let attrs = NSDictionary::from_slices(
                    &[unsafe { NSFontAttributeName }, unsafe {
                        NSBaselineOffsetAttributeName
                    }],
                    &[&*font as &objc2::runtime::AnyObject, &*baseline],
                );
                let title = unsafe {
                    NSAttributedString::initWithString_attributes(
                        NSAttributedString::alloc(),
                        &NSString::from_str(&count),
                        Some(&attrs),
                    )
                };
                native.setAttributedTitle(&title);
                (
                    format!("{}, {} windows", group.first.app_name, group.len()),
                    "Choose a window",
                    group_button_width(group, grouped),
                )
            };
            native.setToolTip(Some(&NSString::from_str(&label)));
            native.setAccessibilityLabel(Some(&NSString::from_str(&label)));
            native.setAccessibilityHelp(Some(&NSString::from_str(help)));
            parent.addSubview(native);
            if grouped
                && let Some(position) = group.first.layout_position
                && let Some(column) =
                    workspace.columns.iter().find(|column| column.id == position.column_id)
            {
                parent.add_column(native, super::drag::ColumnIdentity {
                    id: column.id,
                    members: column.members.clone(),
                });
            }
            self.buttons.push(WindowButton {
                button,
                selection,
                width,
                gap_before: 0.0,
            });
        }
        if groups.len() > MAX_VISIBLE_GROUPS {
            let menu = self.window_menu(windows.iter(), handler);
            let count = groups.len() - MAX_VISIBLE_GROUPS;
            let button = popup_button(ui, &format!("+{count}"), menu);
            let focused = groups.iter().skip(MAX_VISIBLE_GROUPS).any(WindowGroup::focused);
            let native = button.ns_button();
            native.setBezelStyle(NSBezelStyle::AccessoryBar);
            native.setButtonType(NSButtonType::MomentaryChange);
            let selection = ControlSelection::WindowFocus(focused);
            let unit = if grouped { "groups" } else { "windows" };
            native
                .setAccessibilityLabel(Some(&NSString::from_str(&format!("{count} more {unit}"))));
            native.setToolTip(Some(&NSString::from_str("Show all workspace windows")));
            parent.addSubview(native);
            self.buttons.push(WindowButton {
                button,
                selection,
                width: overflow_button_width(count),
                gap_before: 0.0,
            });
        }
    }

    fn add_preview(
        &mut self,
        workspace: &WorkspaceWindows,
        parent: &NSView,
        handler: &Rc<MenuActionHandler>,
        ui: &Ui,
    ) {
        let index = workspace.index;
        let scoped = handler.clone();
        let groups = window_groups(&workspace.windows, true);
        let empty = workspace.windows.is_empty();
        let extra = groups.len().saturating_sub(MAX_STACK_ICONS);
        let title = if empty {
            empty_workspace_title(index)
        } else if extra == 0 {
            String::new()
        } else {
            format!("+{extra}")
        };
        let button = Button::new(ui, &title).borderless().on_click(move || {
            scoped.emit(MenuAction::SwitchToWorkspace(index));
        });
        let native = button.ns_button();
        native.setButtonType(NSButtonType::MomentaryChange);
        let selection = ControlSelection::WorkspaceSelection(workspace.active);
        let (width, label) = if empty {
            native.setFont(Some(&empty_workspace_font()));
            native.setImagePosition(NSCellImagePosition::NoImage);
            (
                empty_workspace_width(index),
                format!("Workspace {}: empty", workspace.name),
            )
        } else {
            let image = self.stack_image(groups.iter().map(|group| group.first));
            native.setImagePosition(if extra == 0 {
                NSCellImagePosition::ImageOnly
            } else {
                NSCellImagePosition::ImageLeading
            });
            native.setFont(Some(&NSFont::systemFontOfSize(9.0)));
            native.setImageScaling(NSImageScaling::ScaleProportionallyDown);
            native.setImage(Some(&image));
            let apps = groups
                .iter()
                .map(|group| group.first.app_name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            (
                image.size().width
                    + 6.0
                    + if extra == 0 {
                        0.0
                    } else {
                        6.0 + title.len() as f64 * 6.0
                    },
                format!(
                    "Workspace {}: {}, {} windows",
                    workspace.name,
                    apps,
                    workspace.windows.len()
                ),
            )
        };
        native.setToolTip(Some(&NSString::from_str(&label)));
        native.setAccessibilityLabel(Some(&NSString::from_str(&label)));
        native.setAccessibilityHelp(Some(&NSString::from_str("Switch to this workspace")));
        parent.addSubview(native);
        self.buttons.push(WindowButton {
            button,
            selection,
            width,
            gap_before: 0.0,
        });
    }

    fn stack_image<'a>(
        &mut self,
        windows: impl IntoIterator<Item = &'a MenuWindow>,
    ) -> Retained<NSImage> {
        let icons = windows
            .into_iter()
            .take(MAX_STACK_ICONS)
            .map(|window| self.icon(window))
            .collect::<Vec<_>>();
        let width = stack_width(icons.len());
        let draw = block2::RcBlock::new(move |_: NSRect| {
            for (index, icon) in icons.iter().enumerate().rev() {
                if let Some(icon) = icon {
                    icon.drawInRect_fromRect_operation_fraction(
                        NSRect::new(
                            NSPoint::new(index as f64 * STACK_OFFSET, 0.0),
                            NSSize::new(ICON_SIZE, ICON_SIZE),
                        ),
                        NSRect::ZERO,
                        NSCompositingOperation::SourceOver,
                        1.0,
                    );
                }
            }
            objc2::runtime::Bool::YES
        });
        NSImage::imageWithSize_flipped_drawingHandler(NSSize::new(width, ICON_SIZE), false, &draw)
    }

    fn icon(&mut self, window: &MenuWindow) -> Option<Retained<NSImage>> {
        let key = (window.id.pid, window.bundle_id.clone());
        if let Some(image) = self.icons.get(&key) {
            return Some(image.clone());
        }
        let app = NSRunningApplication::runningApplicationWithProcessIdentifier(window.id.pid);
        let icon = app
            .as_ref()
            .and_then(|app| app.bundleURL())
            .and_then(|url| url.path())
            .map(|path| NSWorkspace::sharedWorkspace().iconForFile(&path))
            .or_else(|| {
                window.bundle_id.as_deref().and_then(cgs::app::Application::icon_for_bundle)
            })
            .or_else(|| app.and_then(|app| app.icon()))
            .or_else(|| cgs::image::Symbol::named("app"));
        if let Some(icon) = icon {
            let image = prepare_icon(&icon);
            self.icons.insert(key, image.clone());
            Some(image)
        } else {
            None
        }
    }

    fn window_menu<'a>(
        &mut self,
        windows: impl IntoIterator<Item = &'a MenuWindow>,
        handler: &Rc<MenuActionHandler>,
    ) -> Rc<Menu> {
        let reactor = handler.reactor_tx.clone();
        let ui = Ui::new(self.mtm);
        let menu = Rc::new(Menu::new(&ui).on_tracking(move |open| {
            reactor.send(if open {
                ReactorEvent::MenuOpened(own_pid())
            } else {
                ReactorEvent::MenuClosed(own_pid())
            });
        }));
        for window in windows {
            let id = window.id;
            let actions = handler.action_tx.clone();
            let item = MenuItem::new(&ui, &window_label(window)).on_click(move || {
                let _ = actions.send(MenuAction::FocusWindow(id));
            });
            item.set_checked(window.focused);
            item.ns_menu_item().setImage(self.icon(window).as_deref());
            menu.add(item);
        }
        menu
    }

    pub(super) fn width(&self) -> f64 {
        if self.capsules.is_empty() {
            0.0
        } else {
            LIST_GAP
                + self.capsules.iter().map(|capsule| capsule.width).sum::<f64>()
                + WORKSPACE_GAP * (self.capsules.len() - 1) as f64
        }
    }

    pub(super) fn position_after(&self, x: f64, height: f64) {
        let height = height.max(CAPSULE_HEIGHT);
        let reduced = NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion();
        if reduced {
            self.stop_motion();
        }
        let mut x = x + LIST_GAP;
        let mut frames = Vec::new();
        let change = if reduced {
            GeometryChange::Immediate
        } else if self.navigation.replace(false) {
            GeometryChange::Navigate
        } else {
            GeometryChange::Preserve
        };
        for capsule in &self.capsules {
            let from = capsule.host().frame();
            let to = NSRect::new(NSPoint::new(x, 0.0), NSSize::new(capsule.width, height));
            capsule.prepare_geometry(height);
            frames.push(GeometryFrame {
                host: capsule.host().retain(),
                material: capsule.material_view().retain(),
                content: capsule.content_host.clone(),
                outline: capsule.outline.clone(),
                from: if from.size.width > 0.0 { from } else { to },
                to,
            });
            let mut button_x = capsule.workspace.horizontal_padding();
            for item in &self.buttons[capsule.start..capsule.end] {
                button_x += item.gap_before;
                item.button.ns_button().setFrame(NSRect::new(
                    NSPoint::new(button_x, CONTROL_INSET),
                    NSSize::new(item.width, height - CONTROL_INSET * 2.0),
                ));
                button_x += item.width;
            }
            let target = self.buttons[capsule.start..capsule.end]
                .iter()
                .find(|item| matches!(item.selection, ControlSelection::WindowFocus(true)))
                .map(|item| item.button.ns_button().frame());
            capsule.focus.layer.setContentsScale(
                capsule.host().window().map_or(2.0, |window| window.backingScaleFactor()),
            );
            capsule.focus.present(target, !reduced && change != GeometryChange::Navigate);
            capsule.host().layoutSubtreeIfNeeded();
            x += capsule.width + WORKSPACE_GAP;
        }
        self.geometry.borrow_mut().present(frames, change);
    }
}

fn prepare_icon(icon: &NSImage) -> Retained<NSImage> {
    let image = icon.copy();
    let representations = image.representations();
    let artwork_width = representations
        .iter()
        .map(|rep| rep.size().width)
        .filter(|width| *width <= 256.0)
        .reduce(f64::max)
        .unwrap_or_else(|| representations.iter().map(|rep| rep.size().width).fold(0.0, f64::max));
    for representation in representations.iter() {
        if representation.size().width != artwork_width {
            image.removeRepresentation(&representation);
        }
    }
    image.setSize(NSSize::new(ICON_SIZE, ICON_SIZE));
    image
}

fn dark_appearance(appearance: &NSAppearance) -> bool {
    let names = NSArray::from_slice(&[unsafe { NSAppearanceNameAqua }, unsafe {
        NSAppearanceNameDarkAqua
    }]);
    appearance.bestMatchFromAppearancesWithNames(&names).as_deref()
        == Some(unsafe { NSAppearanceNameDarkAqua })
}

fn set_control_selection(button: &objc2_app_kit::NSButton, selection: ControlSelection) {
    let selected = match selection {
        ControlSelection::WindowFocus(selected)
        | ControlSelection::WorkspaceSelection(selected) => selected,
    };
    button.setBordered(false);
    button.setState(NSControlStateValueOff);
    button.setAccessibilitySelected(selected);
    button.setWantsLayer(true);
}

fn set_focus_style(layer: &CALayer, appearance: &NSAppearance) {
    let dark = dark_appearance(appearance);
    let contrast = NSWorkspace::sharedWorkspace().accessibilityDisplayShouldIncreaseContrast();
    appearance.performAsCurrentDrawingAppearance(&block2::RcBlock::new(|| {
        layer.setCornerRadius(6.0);
        let border = NSColor::labelColor()
            .colorWithAlphaComponent(match (dark, contrast) {
                (true, false) => 0.65,
                (false, false) => 0.50,
                (true, true) => 0.90,
                (false, true) => 0.80,
            })
            .CGColor();
        layer.setBorderColor(Some(&border));
        layer.setBorderWidth(1.0);
        let alpha = match (dark, contrast) {
            (true, false) => 0.24,
            (false, false) => 0.16,
            (true, true) => 0.32,
            (false, true) => 0.24,
        };
        layer.setBackgroundColor(Some(
            &NSColor::labelColor().colorWithAlphaComponent(alpha).CGColor(),
        ));
    }));
}

pub(super) fn popup_button(ui: &Ui, title: &str, menu: Rc<Menu>) -> Button {
    let button = Button::new(ui, title).borderless();
    let anchor = button.ns_button().retain();
    button.on_click(move || {
        menu.ns_menu().popUpMenuPositioningItem_atLocation_inView(
            None,
            NSPoint::ZERO,
            Some(&anchor),
        );
    })
}

impl Drop for WindowListView {
    fn drop(&mut self) {
        self.stop_motion();
        for capsule in &self.capsules {
            capsule.host().removeFromSuperview();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::app::WindowId;

    fn window(pid: i32, idx: u32, bundle: Option<&str>) -> MenuWindow {
        MenuWindow {
            id: WindowId::new(pid, idx),
            app_name: "Editor".into(),
            bundle_id: bundle.map(str::to_owned),
            title: format!("file{idx}"),
            focused: false,
            layout_position: None,
        }
    }

    #[test]
    fn menu_bar_groups_preserve_first_app_and_member_layout_order() {
        let windows = vec![
            window(1, 1, Some("editor")),
            window(2, 2, Some("browser")),
            window(3, 3, Some("editor")),
            window(1, 4, Some("editor")),
        ];
        let groups = window_groups(&windows, true);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].windows().map(|w| w.id).collect::<Vec<_>>(), [
            windows[0].id,
            windows[2].id,
            windows[3].id
        ]);
        assert_eq!(groups[1].first.id, windows[1].id);
        assert_eq!(window_groups(&windows, false).len(), 4);
    }

    #[test]
    fn menu_bar_groups_use_pid_for_missing_or_empty_bundle_ids() {
        let windows = vec![
            window(1, 1, None),
            window(2, 2, None),
            window(1, 3, Some("")),
        ];
        let groups = window_groups(&windows, true);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].len(), 2);
        assert_eq!(groups[1].first.id.pid, 2);
    }

    #[test]
    fn menu_bar_column_groups_preserve_columns_rows_and_individual_artwork_members() {
        use crate::actor::menu_bar::MenuLayoutPosition;
        let mut windows = vec![
            window(1, 1, Some("editor")),
            window(1, 2, Some("editor")),
            window(2, 3, Some("browser")),
            window(1, 4, Some("editor")),
        ];
        for (window, (column, row)) in windows.iter_mut().zip([(0, 0), (1, 0), (1, 1), (1, 2)]) {
            window.layout_position = Some(MenuLayoutPosition {
                column_id: crate::layout_engine::ColumnId(column as u64),
                column,
                row,
            });
        }
        let groups = active_groups(&windows, true);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].len(), 1);
        assert_eq!(
            groups[1].windows().map(|window| window.id).collect::<Vec<_>>(),
            windows[1..].iter().map(|window| window.id).collect::<Vec<_>>()
        );
        assert_eq!(active_groups(&windows, false).len(), 4);
        windows[0].layout_position = None;
        windows[1].layout_position = None;
        assert_eq!(active_groups(&windows, true)[0].len(), 2);
        assert_eq!(window_groups(&windows, true).len(), 2);
    }

    #[test]
    fn menu_bar_window_labels_distinguish_windows_and_handle_missing_titles() {
        let mut entry = window(1, 1, None);
        entry.title = "first.txt".into();
        assert_eq!(window_label(&entry), "Editor: first.txt");
        entry.title.clear();
        assert_eq!(window_label(&entry), "Editor");
        entry.title = entry.app_name.clone();
        assert_eq!(window_label(&entry), "Editor");
    }
}
