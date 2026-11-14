// Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
use cgs::menu::{Menu, StatusItem};
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use objc2_foundation::MainThreadMarker;
use tokio::sync::mpsc::UnboundedSender;
use wm::actor::reactor::{Event as ReactorEvent, Sender as ReactorSender};
use wm::ui::menu_bar::MenuAction;
pub use wm::{actor, common, layout_engine, sys, ui};
#[allow(dead_code)]
mod status_mode {
    include!("../src/ui/menu_bar.rs");

    pub(super) fn verify() {
        let mtm = MainThreadMarker::new().unwrap();
        let (actions, _) = tokio::sync::mpsc::unbounded_channel();
        let (reactor, _) = crate::actor::channel();
        let mut icon = MenuIcon::new(
            mtm,
            actions,
            reactor,
            Path::new("/nonexistent-wm-fixture-layouts"),
        );
        let mut settings = MenuBarSettings {
            show_binding_mode: true,
            ..Default::default()
        };
        icon.update_status_icon(&[], &settings, "window");
        let native = icon.status_item.ns_status_item();
        let width = native.length();
        assert!(
            width > 0.0,
            "the native binding-mode caption must have positive width"
        );
        if let Some(window) = native.button(mtm).and_then(|button| button.window()) {
            window.setAlphaValue(0.0);
        }
        for _ in 0..2 {
            settings.show_window_list = true;
            icon.update_status_icon(&[], &settings, "window");
            assert_eq!(icon.status_item.ns_status_item().length(), 0.0);
            settings.show_window_list = false;
            icon.update_status_icon(&[], &settings, "window");
            assert_eq!(
                icon.status_item.ns_status_item().length(),
                width,
                "disabling the window list must restore the native status item's unchanged width"
            );
            assert!(icon.status_item.ns_status_item().isVisible());
        }
        println!("PASS: native status width survives repeated window-list mode transitions");
    }
}

mod drag {
    include!("../src/ui/menu_bar/drag.rs");
    use objc2_app_kit::{NSEventModifierFlags, NSEventType, NSPasteboard};
    use objc2_foundation::NSObject;

    struct DragFixtureState {
        point: Cell<NSPoint>,
        pasteboard: Retained<NSPasteboard>,
    }
    define_class!(
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[ivars = DragFixtureState]
        struct DragFixture;
        unsafe impl NSObjectProtocol for DragFixture {}
        unsafe impl NSDraggingInfo for DragFixture {
            #[unsafe(method(draggingLocation))]
            fn location(&self) -> NSPoint { self.ivars().point.get() }

            #[unsafe(method(draggingPasteboard))]
            fn pasteboard(&self) -> *mut NSPasteboard {
                (&*self.ivars().pasteboard as *const NSPasteboard).cast_mut()
            }
        }
    );

    pub fn verify(parent: &NSView) {
        let mtm = parent.mtm();
        let (tx, mut rx) = crate::actor::channel();
        let space = SpaceId::new(1);
        let workspace = VirtualWorkspaceId::default();
        let content = DragContent::new(mtm, space, workspace, tx);
        content.setFrame(NSRect::new(NSPoint::ZERO, NSSize::new(116.0, 26.0)));
        parent.addSubview(&content);
        let columns: Vec<_> = (0..4)
            .map(|index| ColumnIdentity {
                id: ColumnId(index + 1),
                members: vec![WindowId::new(std::process::id() as i32, index as u32 + 1)],
            })
            .collect();
        let buttons: Vec<_> = columns
            .iter()
            .enumerate()
            .map(|(index, identity)| {
                let button = NSButton::new(mtm);
                button.setFrame(NSRect::new(
                    NSPoint::new(4.0 + index as f64 * 26.0, 1.0),
                    NSSize::new(26.0, 24.0),
                ));
                content.addSubview(&button);
                content.add_column(&button, identity.clone());
                button
            })
            .collect();
        buttons[0].setAlphaValue(0.8);
        let source_point = content.convertPoint_toView(NSPoint::new(17.0, 13.0), None);
        let event = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
            NSEventType::LeftMouseDown, source_point, NSEventModifierFlags::empty(), 0.0,
            parent.window().unwrap().windowNumber(), None, 0, 1, 1.0,
        ).unwrap();
        let pasteboard = NSPasteboard::pasteboardWithUniqueName();
        let info: Retained<DragFixture> = unsafe {
            msg_send![
                super(DragFixture::alloc(mtm).set_ivars(DragFixtureState {
                    point: Cell::new(NSPoint::ZERO),
                    pasteboard: pasteboard.clone(),
                })),
                init
            ]
        };
        let sender = ProtocolObject::from_ref(&*info);
        let start = || {
            content.mouseDown(&event);
            assert_eq!(buttons[0].alphaValue(), 0.8, "a press must not dim the icon");
            let feedback = Feedback::lift(&buttons[0]);
            let token = GESTURE.with(|gesture| {
                let mut gesture = gesture.borrow_mut();
                let active = gesture.as_mut().unwrap();
                active.native = true;
                active.feedback = Some(feedback);
                active.token.clone()
            });
            pasteboard.clearContents();
            pasteboard.setString_forType(&NSString::from_str(&token), &NSString::from_str(TYPE));
            assert!((buttons[0].alphaValue() - 0.24).abs() < 0.0001);
        };
        let point =
            |x| info.ivars().point.set(content.convertPoint_toView(NSPoint::new(x, 13.0), None));
        let marker = &content.ivars().marker;
        let key = NSString::from_str(MARKER_MOTION);
        start();
        for x in [4.0, 30.0] {
            point(x);
            assert_eq!(content.draggingUpdated(sender), NSDragOperation::None);
            assert!(marker.isHidden(), "no-op slots must not show a destination");
        }
        point(56.0);
        assert_eq!(content.draggingUpdated(sender), NSDragOperation::Move);
        assert!(!marker.isHidden());
        assert_eq!(marker.frame().size, NSSize::new(2.0, 14.0));
        assert!(rx.try_recv().is_err(), "hover must not reorder real windows");
        let layer = marker.layer().unwrap();
        assert!(
            unsafe { layer.animationForKey(&key) }.is_none(),
            "first insertion must appear in place"
        );
        point(82.0);
        let target = content.target(sender).unwrap();
        content.show_target(Some(&target), true);
        assert_eq!(unsafe { layer.animationForKey(&key) }.unwrap().duration(), 0.12);
        content.show_target(Some(&target), false);
        assert!(
            unsafe { layer.animationForKey(&key) }.is_none(),
            "Reduce Motion must remove active movement"
        );
        content.draggingExited(None);
        assert!(marker.isHidden());
        assert!(
            (buttons[0].alphaValue() - 0.24).abs() < 0.0001,
            "exiting keeps the source lifted"
        );
        assert_eq!(content.draggingEntered(sender), NSDragOperation::Move);
        assert!(content.performDragOperation(sender));
        let (_, event) = rx.try_recv().unwrap();
        assert!(matches!(event, Event::MenuBarReorderColumn(request)
            if request.source == columns[0].id && request.target == columns[3].id
                && request.placement == ColumnPlacement::Before));
        assert!(
            !content.performDragOperation(sender),
            "one native drop must emit one reorder"
        );
        assert_eq!(buttons[0].alphaValue(), 0.8);
        assert!(marker.isHidden());
        content.finish();
        assert!(!busy());
        assert!(rx.try_recv().is_err());

        for invalidate in [false, true] {
            start();
            point(82.0);
            assert_eq!(content.draggingUpdated(sender), NSDragOperation::Move);
            if invalidate {
                invalidate_if_changed(space, workspace, &columns[1..]);
            } else {
                cancel_window(content.window().as_deref());
            }
            assert!(
                busy(),
                "cancelled native sessions remain retained until completion"
            );
            assert_eq!(buttons[0].alphaValue(), 0.8);
            assert!(marker.isHidden());
            assert!(!content.performDragOperation(sender));
            content.finish();
            assert!(!busy());
        }
        start();
        point(82.0);
        assert_eq!(content.draggingUpdated(sender), NSDragOperation::Move);
        content.finish();
        assert_eq!(
            buttons[0].alphaValue(),
            0.8,
            "Escape and outside completion restore source opacity"
        );
        assert!(marker.isHidden());
        assert!(rx.try_recv().is_err());
        pasteboard.clearContents();
        content.removeFromSuperview();
        println!(
            "PASS: native DnD insertion feedback, no-op slots, source restoration, cancellation, and deferred reorder"
        );
    }
}

struct MenuActionHandler {
    action_tx: UnboundedSender<MenuAction>,
    reactor_tx: ReactorSender,
    space: Option<sys::screen::SpaceId>,
}
impl MenuActionHandler {
    fn emit(&self, action: MenuAction) { let _ = self.action_tx.send(action); }
}

fn own_pid() -> i32 { std::process::id() as i32 }

// AppKit controls require the process main thread; cargo test runs test bodies on workers.
mod window_list {
    use objc2_app_kit::NSStatusBarButton;
    include!("../src/ui/menu_bar/window_list.rs");

    pub fn interactive() {
        use objc2_app_kit::{NSBackingStoreType, NSPanel, NSWindowStyleMask};
        use slotmap::SlotMap;

        let mtm = MainThreadMarker::new().unwrap();
        let ui = Ui::new(mtm);
        let window = NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(mtm),
            NSRect::new(NSPoint::new(200.0, 200.0), NSSize::new(640.0, 160.0)),
            NSWindowStyleMask::Titled
                | NSWindowStyleMask::Closable
                | NSWindowStyleMask::NonactivatingPanel,
            NSBackingStoreType::Buffered,
            false,
        );
        unsafe { window.setReleasedWhenClosed(false) };
        window.setFloatingPanel(true);
        window.setHidesOnDeactivate(false);
        window.setLevel(26);
        window.setTitle(&NSString::from_str("wm Menu Interaction"));
        let parent = window.contentView().unwrap();
        let (action_tx, action_rx) = tokio::sync::mpsc::unbounded_channel();
        let (reactor_tx, _reactor_rx) = crate::actor::channel();
        let handler = Rc::new(super::MenuActionHandler {
            action_tx,
            reactor_tx,
            space: Some(crate::sys::screen::SpaceId::new(1)),
        });
        let mut keys = SlotMap::<crate::layout_engine::VirtualWorkspaceId, ()>::with_key();
        let workspaces = Rc::new(RefCell::new(
            (0..3)
                .map(|index| WorkspaceWindows {
                    id: format!("fixture-{index}"),
                    key: keys.insert(()),
                    columns: Vec::new(),
                    index,
                    name: format!("{}", index + 1),
                    active: index == 0,
                    windows: if index == 2 {
                        Vec::new()
                    } else {
                        (1..=3)
                            .map(|idx| MenuWindow {
                                id: crate::actor::app::WindowId::new(999_990 + idx as i32, idx),
                                app_name: "Fixture".into(),
                                bundle_id: Some(
                                    if idx == 3 {
                                        "com.apple.finder"
                                    } else {
                                        "com.apple.TextEdit"
                                    }
                                    .into(),
                                ),
                                title: format!("Document {idx}"),
                                focused: index == 0 && idx == 1,
                                layout_position: None,
                            })
                            .collect()
                    },
                })
                .collect::<Vec<_>>(),
        ));
        let view = Rc::new(RefCell::new(WindowListView::new(mtm)));
        view.borrow_mut().update(
            &workspaces.borrow(),
            true,
            StripPresentation::Combined,
            &parent,
            &handler,
        );
        view.borrow().position_after(12.0, CAPSULE_HEIGHT);
        let probe = Button::new(&ui, "Move focus and open menu").on_click({
            let view = view.clone();
            let workspaces = workspaces.clone();
            let parent = parent.clone();
            let handler = handler.clone();
            move || {
                {
                    let mut workspaces = workspaces.borrow_mut();
                    let active = workspaces.iter_mut().find(|workspace| workspace.active).unwrap();
                    if active.windows.is_empty() {
                        return;
                    }
                    let target = if active.windows[2].focused { 0 } else { 2 };
                    for (index, window) in active.windows.iter_mut().enumerate() {
                        window.focused = index == target;
                    }
                    view.borrow_mut().update(
                        &workspaces,
                        true,
                        StripPresentation::Combined,
                        &parent,
                        &handler,
                    );
                    view.borrow().position_after(12.0, CAPSULE_HEIGHT);
                }
                let menu =
                    view.borrow_mut().window_menu(workspaces.borrow()[0].windows.iter(), &handler);
                let view_for_probe = view.clone();
                let probe = block2::RcBlock::new(move |_: std::ptr::NonNull<NSTimer>| {
                    let view = view_for_probe.borrow();
                    let capsule =
                        view.capsules.iter().find(|capsule| capsule.workspace.active).unwrap();
                    println!(
                        "TRACKING focus_settled={}",
                        capsule.focus.target.get() == Some(capsule.focus.layer.frame())
                    );
                });
                let timer =
                    unsafe { NSTimer::timerWithTimeInterval_repeats_block(0.35, false, &probe) };
                unsafe {
                    objc2_foundation::NSRunLoop::mainRunLoop()
                        .addTimer_forMode(&timer, objc2_foundation::NSRunLoopCommonModes)
                };
                menu.ns_menu().popUpMenuPositioningItem_atLocation_inView(
                    None,
                    NSPoint::new(18.0, 30.0),
                    Some(&parent),
                );
            }
        });
        probe
            .ns_button()
            .setFrame(NSRect::new(NSPoint::new(18.0, 65.0), NSSize::new(260.0, 32.0)));
        parent.addSubview(probe.ns_button());
        let action_rx = RefCell::new(action_rx);
        let update = block2::RcBlock::new(move |_: std::ptr::NonNull<NSTimer>| {
            while let Ok(action) = action_rx.borrow_mut().try_recv() {
                let mut workspaces = workspaces.borrow_mut();
                match action {
                    MenuAction::SwitchToWorkspace(index) => {
                        for workspace in workspaces.iter_mut() {
                            workspace.active = workspace.index == index;
                        }
                    }
                    MenuAction::FocusWindow(id) => {
                        for workspace in workspaces.iter_mut() {
                            for window in &mut workspace.windows {
                                window.focused = window.id == id && workspace.active;
                            }
                        }
                    }
                    _ => {}
                }
                view.borrow_mut().update(
                    &workspaces,
                    true,
                    StripPresentation::Combined,
                    &parent,
                    &handler,
                );
                view.borrow().position_after(12.0, CAPSULE_HEIGHT);
            }
        });
        let timer = unsafe {
            NSTimer::scheduledTimerWithTimeInterval_repeats_block(1.0 / 60.0, true, &update)
        };
        window.center();
        window.makeKeyAndOrderFront(None);
        let app = objc2_app_kit::NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(objc2_app_kit::NSApplicationActivationPolicy::Regular);
        let menu = Menu::new(&ui).item(
            MenuItem::new(&ui, "Quit")
                .shortcut("q", objc2_app_kit::NSEventModifierFlags::Command)
                .on_click(move || {
                    objc2_app_kit::NSApplication::sharedApplication(mtm).terminate(None)
                }),
        );
        let main = Menu::new(&ui).item(MenuItem::new(&ui, "wm Menu Interaction").submenu(menu));
        app.setMainMenu(Some(main.ns_menu()));
        app.run();
        timer.invalidate();
    }

    fn verify_tracking_motion(parent: &NSView) {
        use objc2_core_foundation::{CFRunLoop, CFString};
        use objc2_foundation::{NSRunLoop, NSRunLoopCommonModes};

        let mode = CFString::from_str("NSEventTrackingRunLoopMode");
        CFRunLoop::current().unwrap().add_common_mode(Some(&mode));
        let default_fired = Rc::new(Cell::new(false));
        let sentinel = block2::RcBlock::new({
            let default_fired = default_fired.clone();
            move |_: std::ptr::NonNull<NSTimer>| default_fired.set(true)
        });
        let sentinel =
            unsafe { NSTimer::scheduledTimerWithTimeInterval_repeats_block(0.01, true, &sentinel) };
        let keepalive = block2::RcBlock::new(|_: std::ptr::NonNull<NSTimer>| {});
        let keepalive =
            unsafe { NSTimer::timerWithTimeInterval_repeats_block(0.01, true, &keepalive) };
        unsafe { NSRunLoop::mainRunLoop().addTimer_forMode(&keepalive, NSRunLoopCommonModes) };

        let focus = FocusIndicator::new();
        let from = NSRect::new(NSPoint::ZERO, NSSize::new(24.0, 22.0));
        let to = NSRect::new(NSPoint::new(80.0, 0.0), from.size);
        focus.present(Some(from), false);
        focus.present(Some(to), true);
        let host = NSView::new(parent.mtm());
        host.setFrame(from);
        parent.addSubview(&host);
        let mut geometry = GeometryMotion::default();
        geometry.present(
            vec![GeometryFrame {
                host: host.clone(),
                material: NSView::new(parent.mtm()),
                content: NSView::new(parent.mtm()),
                outline: CAShapeLayer::new(),
                from,
                to,
            }],
            GeometryChange::Navigate,
        );

        CFRunLoop::run_in_mode(Some(&mode), 0.06, false);
        assert!(
            !default_fired.get(),
            "the probe must remain in menu tracking mode"
        );
        assert!(
            focus.layer.frame().origin.x > from.origin.x
                && focus.layer.frame().origin.x < to.origin.x,
            "focus feedback must visibly advance while a menu tracks"
        );
        assert!(
            host.frame().origin.x > from.origin.x && host.frame().origin.x < to.origin.x,
            "workspace geometry must visibly advance while a menu tracks"
        );
        CFRunLoop::run_in_mode(Some(&mode), 0.3, false);
        assert_eq!(
            focus.layer.frame(),
            to,
            "focus feedback must finish before the menu closes"
        );
        assert_eq!(
            host.frame(),
            to,
            "workspace motion must finish before the menu closes"
        );
        assert!(!default_fired.get());
        focus.stop();
        geometry.cancel(true);
        host.removeFromSuperview();
        sentinel.invalidate();
        keepalive.invalidate();
        println!("PASS: focus and workspace motion advance and finish during native menu tracking");
    }

    fn active_width(workspaces: &[WorkspaceWindows], grouped: bool) -> f64 {
        total_width(
            workspaces
                .iter()
                .filter(|workspace| workspace.active)
                .map(|workspace| expanded_width(workspace, grouped)),
        )
    }

    fn verify_focus_artwork(
        view: &mut WindowListView,
        parent: &NSStatusBarButton,
        handler: &Rc<MenuActionHandler>,
        windows: &[MenuWindow],
    ) {
        use objc2_app_kit::{NSBitmapImageRep, NSColorSpace};
        fn capture(button: &NSView, bounds: NSRect, name: &str) -> Retained<NSBitmapImageRep> {
            let image = button.bitmapImageRepForCachingDisplayInRect(bounds).unwrap();
            button.cacheDisplayInRect_toBitmapImageRep(bounds, &image);
            if let Ok(prefix) = std::env::var("WM_MENU_BAR_SNAPSHOT") {
                let data = unsafe {
                    image.representationUsingType_properties(
                        objc2_app_kit::NSBitmapImageFileType::PNG,
                        &NSDictionary::new(),
                    )
                }
                .unwrap();
                std::fs::write(format!("{prefix}.icon-{name}.png"), data.to_vec()).unwrap();
            }
            image
        }
        fn pixels(image: &NSBitmapImageRep) -> Vec<[f64; 4]> {
            let scale = image.pixelsWide() as f64 / BUTTON_WIDTH;
            (4..20)
                .flat_map(|x| (4..18).map(move |y| (x, y)))
                .map(|(x, y)| {
                    let color = image
                        .colorAtX_y((x as f64 * scale) as isize, (y as f64 * scale) as isize)
                        .unwrap()
                        .colorUsingColorSpace(&NSColorSpace::sRGBColorSpace())
                        .unwrap();
                    [
                        color.redComponent(),
                        color.greenComponent(),
                        color.blueComponent(),
                        color.alphaComponent(),
                    ]
                })
                .collect()
        }
        let mut section = WorkspaceWindows {
            key: Default::default(),
            columns: Vec::new(),
            id: "focus-artwork".into(),
            index: 0,
            name: "01".into(),
            active: true,
            windows: vec![windows[0].clone()],
        };
        section.windows[0].focused = false;
        view.update(
            &[section.clone()],
            true,
            StripPresentation::Combined,
            parent,
            handler,
        );
        view.position_after(0.0, parent.bounds().size.height);
        let button = view.buttons[0].button.ns_button();
        let image_data = button.image().unwrap().TIFFRepresentation().unwrap();
        let normal_rect = button.cell().unwrap().imageRectForBounds(button.bounds());
        let normal = capture(&view.capsules[0].content_host, button.frame(), "normal");
        section.windows[0].focused = true;
        view.update(&[section], true, StripPresentation::Combined, parent, handler);
        view.position_after(0.0, parent.bounds().size.height);
        let button = view.buttons[0].button.ns_button();
        let focused = capture(&view.capsules[0].content_host, button.frame(), "focused");
        assert_eq!(
            button.cell().unwrap().imageRectForBounds(button.bounds()),
            normal_rect
        );
        assert_eq!(normal_rect.size, NSSize::new(ICON_SIZE, ICON_SIZE));
        assert_eq!(
            button.image().unwrap().TIFFRepresentation().unwrap().to_vec(),
            image_data.to_vec()
        );
        let normal_pixels = pixels(&normal);
        assert!(
            normal_pixels.iter().any(|value| value[3] > 0.1),
            "artwork comparison must contain rendered pixels"
        );
        let focused_pixels = pixels(&focused);
        let opaque = normal_pixels
            .iter()
            .zip(&focused_pixels)
            .filter(|(normal, _)| normal[3] >= 0.9999)
            .collect::<Vec<_>>();
        assert!(
            !opaque.is_empty(),
            "artwork comparison must contain opaque icon pixels"
        );
        for (normal, selected) in opaque {
            assert!(
                normal.iter().zip(selected).all(|(a, b)| (a - b).abs() <= 1.0 / 255.0),
                "selection background must preserve opaque icon artwork"
            );
        }
        assert!(
            normal_pixels.iter().zip(&focused_pixels).any(|(normal, selected)| normal
                .iter()
                .zip(selected)
                .any(|(a, b)| (a - b).abs() > 1.0 / 255.0)),
            "selection background must visibly change rendered pixels"
        );
        assert_eq!(view.capsules[0].focus.layer.borderWidth(), 1.0);
        assert!(view.capsules[0].focus.layer.borderColor().is_some());
        assert!(view.capsules[0].focus.layer.backgroundColor().is_some());
        assert!(button.isAccessibilitySelected());
        assert_ne!(
            normal.TIFFRepresentation().unwrap().to_vec(),
            focused.TIFFRepresentation().unwrap().to_vec(),
            "focus must visibly change its neutral background"
        );
    }

    fn verify_icon_representations(ui: &Ui, parent: &NSStatusBarButton) {
        use objc2_app_kit::{NSBitmapImageRep, NSColorSpace, NSDeviceRGBColorSpace};
        let source = NSImage::initWithSize(NSImage::alloc(), NSSize::new(128.0, 128.0));
        for pixels in [18, 36, 128, 256, 512] {
            let bitmap = unsafe {
                NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
                    NSBitmapImageRep::alloc(), std::ptr::null_mut(), pixels, pixels,
                    8, 4, true, false, NSDeviceRGBColorSpace, 0, 0,
                ).unwrap()
            };
            bitmap.setSize(NSSize::new(pixels as f64, pixels as f64));
            let mut rgba: [usize; 4] = match pixels {
                256 => [0, 0, 255, 255],
                512 => [0, 255, 0, 255],
                _ => [255, 0, 0, 255],
            };
            for y in 0..pixels {
                for x in 0..pixels {
                    unsafe {
                        bitmap.setPixel_atX_y(
                            std::ptr::NonNull::new(rgba.as_mut_ptr()).unwrap(),
                            x,
                            y,
                        );
                    }
                }
            }
            let pixel = bitmap
                .colorAtX_y(pixels / 2, pixels / 2)
                .unwrap()
                .colorUsingColorSpace(&NSColorSpace::sRGBColorSpace())
                .unwrap();
            assert!(
                [
                    pixel.redComponent(),
                    pixel.greenComponent(),
                    pixel.blueComponent(),
                    pixel.alphaComponent()
                ]
                .iter()
                .zip(rgba)
                .all(|(actual, expected)| (actual - expected as f64 / 255.0).abs() < 0.1),
                "source representation must contain opaque fixture pixels before rendering"
            );
            source.addRepresentation(&bitmap);
        }
        let image = prepare_icon(&source);
        let large_source = source.copy();
        for representation in large_source.representations().iter() {
            if representation.size().width < 512.0 {
                large_source.removeRepresentation(&representation);
            }
        }
        let fallback = prepare_icon(&large_source);
        let original = source.copy();
        original.setSize(NSSize::new(ICON_SIZE, ICON_SIZE));
        let button = Button::new(ui, "").borderless();
        let native = button.ns_button();
        native.setImagePosition(NSCellImagePosition::ImageOnly);
        native.setImageScaling(NSImageScaling::ScaleProportionallyDown);
        native.setFrame(NSRect::new(
            NSPoint::ZERO,
            NSSize::new(BUTTON_WIDTH, CAPSULE_HEIGHT),
        ));
        parent.addSubview(native);
        for scale in [1.0, 2.0] {
            for (candidate, expected) in [
                (&original, [1.0, 0.0, 0.0]),
                (&image, [0.0, 0.0, 1.0]),
                (&fallback, [0.0, 1.0, 0.0]),
            ] {
                native.setImage(Some(candidate));
                let bounds = native.bounds();
                let bitmap = unsafe {
                    NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
                        NSBitmapImageRep::alloc(), std::ptr::null_mut(),
                        (bounds.size.width * scale) as isize, (bounds.size.height * scale) as isize,
                        8, 4, true, false, NSDeviceRGBColorSpace, 0, 0,
                    ).unwrap()
                };
                bitmap.setSize(bounds.size);
                native.cacheDisplayInRect_toBitmapImageRep(bounds, &bitmap);
                let color = bitmap
                    .colorAtX_y(bitmap.pixelsWide() / 2, bitmap.pixelsHigh() / 2)
                    .unwrap()
                    .colorUsingColorSpace(&NSColorSpace::sRGBColorSpace())
                    .unwrap();
                assert!(
                    [
                        color.redComponent(),
                        color.greenComponent(),
                        color.blueComponent()
                    ]
                    .iter()
                    .zip(expected)
                    .all(|(actual, expected)| (actual - expected).abs() < 0.1),
                    "menu icons must preserve menu-sized artwork, large-only fallback, and source pixels at {scale}x: expected={expected:?}, rgba={:?}",
                    [
                        color.redComponent(),
                        color.greenComponent(),
                        color.blueComponent(),
                        color.alphaComponent()
                    ]
                );
                assert_eq!(
                    native.cell().unwrap().imageRectForBounds(bounds).size,
                    NSSize::new(ICON_SIZE, ICON_SIZE)
                );
            }
        }
        native.removeFromSuperview();
    }

    fn verify_outline_pixels(capsule: &WorkspaceCapsule) {
        use objc2_app_kit::{
            NSBitmapImageRep, NSColorSpace, NSDeviceRGBColorSpace, NSGraphicsContext,
        };
        let original = capsule.host().frame();
        let frame = GeometryFrame {
            host: capsule.host().retain(),
            material: capsule.material_view().retain(),
            content: capsule.content_host.clone(),
            outline: capsule.outline.clone(),
            from: original,
            to: NSRect::new(
                original.origin,
                NSSize::new(original.size.width + 37.0, original.size.height),
            ),
        };
        for progress in [0.0, 0.5, 1.0, 0.0] {
            frame.apply(progress);
            assert_eq!(capsule.material_view().frame(), capsule.host().bounds());
            assert_eq!(capsule.outline.frame(), capsule.host().bounds());
            for scale in [1.0, 2.0] {
                let bounds = capsule.host().bounds();
                let capture = || {
                    let bitmap = unsafe {
                        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
                            NSBitmapImageRep::alloc(), std::ptr::null_mut(),
                            (bounds.size.width * scale).ceil() as isize,
                            (bounds.size.height * scale).ceil() as isize,
                            8, 4, true, false, NSDeviceRGBColorSpace, 0, 0,
                        ).unwrap()
                    };
                    let context = NSGraphicsContext::graphicsContextWithBitmapImageRep(&bitmap)
                        .unwrap()
                        .CGContext();
                    objc2_core_graphics::CGContext::scale_ctm(Some(&context), scale, scale);
                    capsule.outline.renderInContext(&context);
                    bitmap
                };
                let bitmap = capture();
                let layer = &capsule.outline;
                let stroke = layer.strokeColor();
                layer.setStrokeColor(None);
                let without_border = capture();
                layer.setStrokeColor(stroke.as_deref());
                let first = (bounds.size.height / 2.0 * scale).ceil() as isize + 2;
                let last = bitmap.pixelsWide() - first;
                assert!(last > first, "outline fixture must contain straight edges");
                for y in [0, bitmap.pixelsHigh() - 1] {
                    for x in first..last {
                        let color = bitmap
                            .colorAtX_y(x, y)
                            .unwrap()
                            .colorUsingColorSpace(&NSColorSpace::sRGBColorSpace())
                            .unwrap();
                        assert!(
                            color.alphaComponent() >= 0.04,
                            "workspace top and bottom edges must remain continuous at {scale}x after resize, x={x} y={y}"
                        );
                        let absent = without_border
                            .colorAtX_y(x, y)
                            .unwrap()
                            .colorUsingColorSpace(&NSColorSpace::sRGBColorSpace())
                            .unwrap();
                        let selected = [
                            color.redComponent(),
                            color.greenComponent(),
                            color.blueComponent(),
                            color.alphaComponent(),
                        ];
                        let unselected = [
                            absent.redComponent(),
                            absent.greenComponent(),
                            absent.blueComponent(),
                            absent.alphaComponent(),
                        ];
                        assert!(
                            selected.iter().zip(unselected).any(|(a, b)| (a - b).abs() >= 0.02),
                            "edge pixels must visibly contain the outline against the transparent background at {scale}x, x={x} y={y}"
                        );
                    }
                }
            }
        }
    }

    fn verify_mode_pixels(capsule: &WorkspaceCapsule) {
        use objc2_app_kit::{NSBitmapImageRep, NSColorSpace, NSDeviceRGBColorSpace};
        let bounds = capsule.content_host.bounds();
        for scale in [1.0, 2.0] {
            let capture = || {
                let bitmap = unsafe {
                    NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
                        NSBitmapImageRep::alloc(), std::ptr::null_mut(),
                        (bounds.size.width * scale).ceil() as isize,
                        (bounds.size.height * scale).ceil() as isize,
                        8, 4, true, false, NSDeviceRGBColorSpace, 0, 0,
                    ).unwrap()
                };
                bitmap.setSize(bounds.size);
                capsule.content_host.cacheDisplayInRect_toBitmapImageRep(bounds, &bitmap);
                [2.0, bounds.size.width - 2.0].map(|x| {
                    let color = bitmap
                        .colorAtX_y(
                            (x * scale) as isize,
                            (bounds.size.height * scale / 2.0) as isize,
                        )
                        .unwrap()
                        .colorUsingColorSpace(&NSColorSpace::sRGBColorSpace())
                        .unwrap();
                    [
                        color.redComponent(),
                        color.greenComponent(),
                        color.blueComponent(),
                        color.alphaComponent(),
                    ]
                })
            };
            capsule.set_mode(false);
            let normal = capture();
            capsule.set_mode(true);
            let mode = capture();
            for (normal, mode) in normal.iter().zip(mode) {
                assert!(
                    normal.iter().zip(mode).any(|(a, b)| (a - b).abs() > 0.02),
                    "binding mode must visibly change the interior on both sides at {scale}x"
                );
            }
            capsule.set_mode(false);
            assert_eq!(
                normal,
                capture(),
                "leaving binding mode must exactly restore interior pixels"
            );
        }
    }

    pub(super) fn verify() {
        use objc2_app_kit::{
            NSBitmapImageFileType, NSControlStateValueOn, NSEvent, NSEventModifierFlags,
            NSEventType,
        };
        use objc2_foundation::{NSDictionary, NSRect};
        use wm::sys::screen::SpaceId;

        use crate::actor::app::WindowId;
        use crate::actor::menu_bar::Workspace;
        use crate::common::config::{MenuBarSettings, WorkspaceDisplayStyle};

        let mtm = MainThreadMarker::new().expect("native verification must run on main thread");
        let ui = Ui::new(mtm);
        let status = super::StatusItem::new(&ui);
        let menu = super::Menu::new(&ui);
        menu.add(cgs::menu::MenuItem::new(&ui, "Test controls"));
        status.ns_status_item().setMenu(Some(menu.ns_menu()));
        status.ns_status_item().setLength(400.0);
        let parent = status.ns_status_item().button(mtm).unwrap();
        let native_window = parent.window().unwrap();
        native_window.setAlphaValue(0.0);
        assert_eq!(native_window.alphaValue(), 0.0);
        if !NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion() {
            verify_tracking_motion(&parent);
        }
        super::drag::verify(&parent);
        verify_icon_representations(&ui, &parent);
        let (action_tx, mut action_rx) = tokio::sync::mpsc::unbounded_channel();
        let (reactor_tx, _reactor_rx) = crate::actor::channel();
        let handler = Rc::new(super::MenuActionHandler {
            action_tx,
            reactor_tx,
            space: Some(SpaceId::new(1)),
        });
        let pid = std::process::id() as i32;
        let mut windows = (1..=14)
            .map(|idx| MenuWindow {
                id: WindowId::new(pid, idx),
                app_name: "Test application".into(),
                bundle_id: None,
                title: format!("Window {idx}"),
                focused: idx == 2,
                layout_position: None,
            })
            .collect::<Vec<_>>();
        let active = |windows: &[MenuWindow]| {
            vec![WorkspaceWindows {
                key: Default::default(),
                columns: Vec::new(),
                id: "active".into(),
                index: 0,
                name: "01".into(),
                active: true,
                windows: windows.to_vec(),
            }]
        };
        let mut view = WindowListView::new(mtm);
        verify_focus_artwork(&mut view, &parent, &handler, &windows);
        view.update(
            &active(&windows),
            false,
            StripPresentation::Combined,
            &parent,
            &handler,
        );
        view.position_after(0.0, parent.bounds().size.height);
        assert_eq!(view.buttons.len(), MAX_VISIBLE_GROUPS + 1);
        assert_eq!(
            view.width(),
            LIST_GAP + 13.0 * BUTTON_WIDTH + WORKSPACE_PADDING * 2.0
        );
        assert_eq!(view.width(), active_width(&active(&windows), false));
        assert!(parent.menu().is_none());
        assert!(
            status.ns_status_item().menu(mtm).is_some(),
            "original status menu must remain attached"
        );
        for (index, button) in view.buttons.iter().take(MAX_VISIBLE_GROUPS).enumerate() {
            let native = button.button.ns_button();
            let point = native.convertPoint_toView(
                NSPoint::new(
                    native.bounds().size.width / 2.0,
                    native.bounds().size.height / 2.0,
                ),
                Some(&parent),
            );
            let hit = parent.hitTest(point).expect("window icon must receive pointer events");
            assert!(
                std::ptr::eq(&*hit, &***native),
                "hit test must reach window button {index}: hit={hit:?}, point={point:?}, capsule={:?}, content={:?}, button={:?}",
                view.capsules[0].host().frame(),
                view.capsules[0].content.frame(),
                native.frame()
            );
            assert_eq!(
                native.toolTip().unwrap().to_string(),
                window_label(&windows[index])
            );
            assert_eq!(
                native.accessibilityLabel().unwrap().to_string(),
                window_label(&windows[index])
            );
            assert_eq!(native.state(), NSControlStateValueOff);
            assert!(!native.isBordered());
            assert_eq!(native.isAccessibilitySelected(), index == 1);
            assert_eq!(native.layer().unwrap().borderWidth(), 0.0);
            assert!(native.layer().unwrap().backgroundColor().is_none());
            if index == 1 {
                assert_eq!(view.capsules[0].focus.target.get(), Some(native.frame()));
                assert!(!view.capsules[0].focus.layer.isHidden());
            }
            let rect = native.cell().unwrap().imageRectForBounds(native.bounds());
            assert_eq!(rect.size, NSSize::new(ICON_SIZE, ICON_SIZE));
            assert!(native.image().is_some());
            unsafe { native.performClick(None) };
            assert!(
                matches!(action_rx.try_recv().unwrap(), MenuAction::FocusWindow(id) if id == windows[index].id)
            );
        }
        let first = view.buttons[0].button.ns_button();
        let point =
            first.convertPoint_toView(NSPoint::new(12.0, first.bounds().size.height / 2.0), None);
        let mouse = |kind, pressure| {
            NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
            kind, point, NSEventModifierFlags::empty(), 0.0, native_window.windowNumber(), None, 0, 1, pressure,
        ).unwrap()
        };
        super::NSApplication::sharedApplication(mtm)
            .postEvent_atStart(&mouse(NSEventType::LeftMouseUp, 0.0), true);
        native_window.sendEvent(&mouse(NSEventType::LeftMouseDown, 1.0));
        assert!(
            matches!(action_rx.try_recv().expect("native mouse dispatch must reach the icon"), MenuAction::FocusWindow(id) if id == windows[0].id)
        );
        let overflow = view.window_menu(windows.iter(), &handler);
        let items = overflow.ns_menu().itemArray();
        assert_eq!(items.len(), windows.len());
        for (index, item) in items.iter().enumerate() {
            assert_eq!(item.title().to_string(), window_label(&windows[index]));
            overflow.ns_menu().performActionForItemAtIndex(index as isize);
            assert!(
                matches!(action_rx.try_recv().unwrap(), MenuAction::FocusWindow(id) if id == windows[index].id)
            );
        }
        for window in &mut windows {
            window.focused = window.id.idx.get() == 13;
        }
        view.update(
            &active(&windows),
            false,
            StripPresentation::Combined,
            &parent,
            &handler,
        );
        view.position_after(0.0, parent.bounds().size.height);
        assert_eq!(
            view.capsules[0].focus.target.get(),
            Some(view.buttons[MAX_VISIBLE_GROUPS].button.ns_button().frame())
        );
        assert!(view.buttons[MAX_VISIBLE_GROUPS].button.ns_button().isAccessibilitySelected());
        windows.reverse();
        windows[0].title = "Renamed".into();
        view.update(
            &active(&windows),
            false,
            StripPresentation::Combined,
            &parent,
            &handler,
        );
        unsafe { view.buttons[0].button.ns_button().performClick(None) };
        assert!(
            matches!(action_rx.try_recv().unwrap(), MenuAction::FocusWindow(id) if id == windows[0].id)
        );
        let old = view.buttons[0].button.ns_button().retain();
        view.update(&[], false, StripPresentation::Combined, &parent, &handler);
        assert_eq!(view.width(), 0.0);
        assert!(view.buttons.is_empty());
        assert!(view.icons.is_empty());
        assert!(unsafe { old.superview() }.is_none());

        let mut grouped_windows = windows[..3].to_vec();
        grouped_windows[0].app_name = "Editor".into();
        grouped_windows[0].bundle_id = Some("local.fixture.editor".into());
        grouped_windows[0].focused = false;
        grouped_windows[1].app_name = "Browser".into();
        grouped_windows[1].bundle_id = Some("local.fixture.browser".into());
        grouped_windows[1].focused = false;
        grouped_windows[2].app_name = "Editor".into();
        grouped_windows[2].bundle_id = Some("local.fixture.editor".into());
        grouped_windows[2].focused = true;
        view.update(
            &active(&grouped_windows),
            true,
            StripPresentation::Combined,
            &parent,
            &handler,
        );
        view.position_after(0.0, parent.bounds().size.height);
        assert_eq!(view.buttons.len(), 2);
        assert_eq!(view.width(), active_width(&active(&grouped_windows), true));
        let group_button = view.buttons[0].button.ns_button();
        assert_eq!(group_button.attributedTitle().string().to_string(), "2");
        assert_eq!(
            group_button.accessibilityLabel().unwrap().to_string(),
            "Editor, 2 windows"
        );
        assert_eq!(view.capsules[0].focus.target.get(), Some(group_button.frame()));
        assert!(group_button.isAccessibilitySelected());
        assert_eq!(
            group_button.cell().unwrap().imageRectForBounds(group_button.bounds()).size,
            NSSize::new(ICON_SIZE, ICON_SIZE)
        );
        let count_point = group_button.convertPoint_toView(
            NSPoint::new(
                group_button.bounds().size.width - 3.0,
                parent.bounds().size.height / 2.0,
            ),
            Some(&parent),
        );
        let hit = parent.hitTest(count_point).unwrap();
        assert!(
            std::ptr::eq(&*hit, &***group_button),
            "count area shares the icon's native hit target"
        );
        let grouped = window_groups(&grouped_windows, true);
        let menu = view.window_menu(grouped[0].windows(), &handler);
        let members = [grouped_windows[0].id, grouped_windows[2].id];
        assert_eq!(menu.ns_menu().numberOfItems(), 2);
        assert_eq!(
            menu.ns_menu().itemAtIndex(1).unwrap().state(),
            NSControlStateValueOn
        );
        for (index, expected) in members.into_iter().enumerate() {
            menu.ns_menu().performActionForItemAtIndex(index as isize);
            assert!(
                matches!(action_rx.try_recv().unwrap(), MenuAction::FocusWindow(id) if id == expected)
            );
        }
        unsafe { view.buttons[1].button.ns_button().performClick(None) };
        assert!(
            matches!(action_rx.try_recv().unwrap(), MenuAction::FocusWindow(id) if id == grouped_windows[1].id)
        );
        if let Ok(path) = std::env::var("WM_MENU_BAR_SNAPSHOT") {
            let bounds = NSRect::new(
                NSPoint::ZERO,
                NSSize::new(view.width(), parent.bounds().size.height),
            );
            let rep = parent.bitmapImageRepForCachingDisplayInRect(bounds).unwrap();
            parent.cacheDisplayInRect_toBitmapImageRep(bounds, &rep);
            let data = unsafe {
                rep.representationUsingType_properties(
                    NSBitmapImageFileType::PNG,
                    &NSDictionary::new(),
                )
            }
            .unwrap();
            std::fs::write(path, data.to_vec()).unwrap();
        }
        view.update(
            &active(&grouped_windows),
            false,
            StripPresentation::Combined,
            &parent,
            &handler,
        );
        assert_eq!(
            view.buttons.len(),
            3,
            "config-only grouping changes must rebuild controls"
        );
        view.update(
            &active(&grouped_windows),
            true,
            StripPresentation::Combined,
            &parent,
            &handler,
        );
        assert_eq!(view.buttons.len(), 2);
        let mut columns = grouped_windows.clone();
        for (window, (column, row)) in columns.iter_mut().zip([(0, 0), (0, 1), (1, 0)]) {
            window.layout_position = Some(crate::actor::menu_bar::MenuLayoutPosition {
                column_id: crate::layout_engine::ColumnId(column as u64),
                column,
                row,
            });
        }
        view.update(
            &active(&columns),
            true,
            StripPresentation::Combined,
            &parent,
            &handler,
        );
        view.position_after(0.0, parent.bounds().size.height);
        assert_eq!(view.capsules.len(), 1);
        assert_eq!(
            view.buttons.len(),
            2,
            "same app in another column must remain separate"
        );
        assert_eq!(view.width(), active_width(&active(&columns), true));
        assert_eq!(
            view.buttons[0]
                .button
                .ns_button()
                .cell()
                .unwrap()
                .imageRectForBounds(view.buttons[0].button.ns_button().bounds())
                .size,
            NSSize::new(ICON_SIZE + STACK_OFFSET, ICON_SIZE)
        );
        assert_eq!(
            view.buttons[1]
                .button
                .ns_button()
                .cell()
                .unwrap()
                .imageRectForBounds(view.buttons[1].button.ns_button().bounds())
                .size,
            NSSize::new(ICON_SIZE, ICON_SIZE)
        );
        if objc2::available!(macos = 26.0) {
            assert!(matches!(&view.capsules[0].material, WorkspaceMaterial::Glass(_)));
        } else {
            assert!(matches!(
                &view.capsules[0].material,
                WorkspaceMaterial::Visual(_)
            ));
        }
        if let WorkspaceMaterial::Glass(glass) = &view.capsules[0].material {
            assert!(std::ptr::eq(
                &*glass.contentView().unwrap(),
                &*view.capsules[0].content_host
            ));
            assert_eq!(glass.cornerRadius(), CAPSULE_HEIGHT / 2.0);
            view.set_mode(true);
            assert!(glass.tintColor().is_some());
            assert_eq!(glass.layer().unwrap().borderWidth(), 0.0);
            assert_eq!(
                view.capsules[0].outline.lineWidth(),
                if NSWorkspace::sharedWorkspace().accessibilityDisplayShouldIncreaseContrast() {
                    1.0
                } else {
                    0.5
                }
            );
            assert!(glass.layer().unwrap().backgroundColor().is_none());
            view.set_mode(false);
            assert!(glass.tintColor().is_some());
        }
        let groups = active_groups(&columns, true);
        let menu = view.window_menu(groups[0].windows(), &handler);
        assert_eq!(menu.ns_menu().numberOfItems(), 2);
        for (index, window) in columns[..2].iter().enumerate() {
            menu.ns_menu().performActionForItemAtIndex(index as isize);
            assert!(
                matches!(action_rx.try_recv().unwrap(), MenuAction::FocusWindow(id) if id == window.id)
            );
        }
        unsafe { view.buttons[1].button.ns_button().performClick(None) };
        assert!(
            matches!(action_rx.try_recv().unwrap(), MenuAction::FocusWindow(id) if id == columns[2].id)
        );
        let overflowing = (0..14)
            .flat_map(|group| {
                (0..2).map(move |member| MenuWindow {
                    id: WindowId::new(pid, 100 + group * 2 + member),
                    app_name: format!("App {group}"),
                    bundle_id: Some(format!("local.fixture.group{group}")),
                    title: format!("Window {member}"),
                    focused: group == 13 && member == 1,
                    layout_position: None,
                })
            })
            .collect::<Vec<_>>();
        view.update(
            &active(&overflowing),
            true,
            StripPresentation::Combined,
            &parent,
            &handler,
        );
        assert_eq!(view.buttons.len(), MAX_VISIBLE_GROUPS + 1);
        assert_eq!(view.width(), active_width(&active(&overflowing), true));
        view.position_after(0.0, parent.bounds().size.height);
        let overflow = view.buttons.last().unwrap().button.ns_button();
        assert_eq!(overflow.title().to_string(), "+2");
        assert_eq!(
            overflow.accessibilityLabel().unwrap().to_string(),
            "2 more groups"
        );
        assert_eq!(view.capsules[0].focus.target.get(), Some(overflow.frame()));
        assert!(overflow.isAccessibilitySelected());
        view.update(&[], true, StripPresentation::Combined, &parent, &handler);

        let workspace = |id: &str, active: bool, entries: Vec<MenuWindow>| Workspace {
            key: Default::default(),
            columns: Vec::new(),
            id: id.into(),
            index: 0,
            name: id.into(),
            layout_mode: "scrolling".into(),
            is_active: active,
            window_count: entries.len(),
            windows: Vec::new(),
            window_list: entries,
            has_fullscreen: false,
        };
        let displays = vec![
            DisplayWorkspaces {
                screen_id: wm::sys::screen::ScreenId::new(0),
                is_builtin: false,
                display_uuid: "left".into(),
                space: SpaceId::new(1),
                is_active_context: false,
                space_is_activated: true,
                workspaces: vec![workspace("left", true, vec![windows[0].clone()])],
            },
            DisplayWorkspaces {
                screen_id: wm::sys::screen::ScreenId::new(0),
                is_builtin: false,
                display_uuid: "right".into(),
                space: SpaceId::new(2),
                is_active_context: true,
                space_is_activated: true,
                workspaces: vec![
                    workspace("inactive", false, vec![windows[1].clone()]),
                    workspace("active", true, vec![windows[2].clone()]),
                ],
            },
        ];
        let mut settings = MenuBarSettings {
            show_window_list: true,
            display_style: WorkspaceDisplayStyle::Label,
            ..Default::default()
        };
        let left_sections = workspace_windows(&displays[0], &settings);
        assert_eq!(left_sections.len(), 1);
        assert_eq!(left_sections[0].windows, windows[..1]);
        let sections = workspace_windows(&displays[1], &settings);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].windows, windows[1..2]);
        assert_eq!(sections[1].windows, windows[2..3]);
        assert!(!sections[0].active);
        assert!(sections[1].active);
        let mut stacked = sections.clone();
        stacked[0].index = 4;
        stacked[0].windows = grouped_windows.clone();
        for (entry, red) in [(&grouped_windows[0], true), (&grouped_windows[1], false)] {
            let draw = block2::RcBlock::new(move |rect: NSRect| {
                let color = if red {
                    NSColor::systemRedColor()
                } else {
                    NSColor::systemBlueColor()
                };
                color.setFill();
                objc2_app_kit::NSBezierPath::fillRect(rect);
                objc2::runtime::Bool::YES
            });
            let image = NSImage::imageWithSize_flipped_drawingHandler(
                NSSize::new(ICON_SIZE, ICON_SIZE),
                false,
                &draw,
            );
            view.icons.insert((entry.id.pid, entry.bundle_id.clone()), image);
        }
        stacked[1].index = 7;
        view.update(&stacked, true, StripPresentation::Combined, &parent, &handler);
        view.position_after(0.0, parent.bounds().size.height);
        assert_eq!(view.buttons.len(), 2);
        let preview = view.buttons[0].button.ns_button();
        assert_eq!(preview.title().to_string(), "");
        assert_eq!(
            preview.image().unwrap().size(),
            NSSize::new(ICON_SIZE + STACK_OFFSET, ICON_SIZE)
        );
        assert_eq!(
            preview.cell().unwrap().imageRectForBounds(preview.bounds()).size,
            NSSize::new(ICON_SIZE + STACK_OFFSET, ICON_SIZE)
        );
        let image = preview.image().unwrap();
        let bitmap =
            objc2_app_kit::NSBitmapImageRep::imageRepWithData(&image.TIFFRepresentation().unwrap())
                .unwrap();
        let scale = bitmap.pixelsWide() as f64 / image.size().width;
        let sample = |x: f64| {
            bitmap
                .colorAtX_y((x * scale) as isize, (ICON_SIZE * scale / 2.0) as isize)
                .unwrap()
                .colorUsingColorSpace(&objc2_app_kit::NSColorSpace::sRGBColorSpace())
                .unwrap()
        };
        let left = sample(3.0);
        let overlap = sample(10.0);
        let right = sample(22.0);
        assert!(left.redComponent() > left.blueComponent());
        assert!(
            overlap.redComponent() > overlap.blueComponent(),
            "first app must be in front across the overlap"
        );
        assert!(
            right.blueComponent() > right.redComponent(),
            "second app must retain a visible full-height strip"
        );
        if let Ok(prefix) = std::env::var("WM_MENU_BAR_SNAPSHOT") {
            let data = unsafe {
                bitmap.representationUsingType_properties(
                    NSBitmapImageFileType::PNG,
                    &NSDictionary::new(),
                )
            }
            .unwrap();
            std::fs::write(format!("{prefix}.stack.png"), data.to_vec()).unwrap();
        }
        for x in [3.0, preview.bounds().size.width - 3.0] {
            let point = preview.convertPoint_toView(
                NSPoint::new(x, preview.bounds().size.height / 2.0),
                Some(&parent),
            );
            let hit = parent.hitTest(point).unwrap();
            assert!(
                std::ptr::eq(&*hit, &***preview),
                "whole overlap strip must select its workspace"
            );
        }
        unsafe { preview.performClick(None) };
        assert!(matches!(
            action_rx.try_recv().unwrap(),
            MenuAction::SwitchToWorkspace(4)
        ));
        stacked[0].active = true;
        stacked[1].active = false;
        view.update(&stacked, true, StripPresentation::Combined, &parent, &handler);
        view.position_after(0.0, parent.bounds().size.height);
        assert_eq!(
            view.buttons.len(),
            3,
            "workspace activation expands exactly its app groups"
        );
        let preview = view.buttons[2].button.ns_button();
        unsafe { preview.performClick(None) };
        assert!(matches!(
            action_rx.try_recv().unwrap(),
            MenuAction::SwitchToWorkspace(7)
        ));
        stacked[0].active = false;
        stacked[1].active = true;
        view.update(&stacked, true, StripPresentation::Combined, &parent, &handler);
        assert_eq!(view.buttons.len(), 2);
        stacked[0].windows = overflowing.clone();
        view.update(&stacked, true, StripPresentation::Combined, &parent, &handler);
        view.position_after(0.0, parent.bounds().size.height);
        assert_eq!(view.buttons[0].button.ns_button().title().to_string(), "+2");
        assert_eq!(
            view.buttons[0].button.ns_button().image().unwrap().size().width,
            ICON_SIZE + STACK_OFFSET * 11.0
        );
        let first_frame = view.capsules[0].host().frame();
        let second_frame = view.capsules[1].host().frame();
        assert_eq!(
            second_frame.origin.x - first_frame.origin.x - first_frame.size.width,
            WORKSPACE_GAP
        );
        let mut empty = stacked[0].clone();
        empty.windows.clear();
        empty.index = 11;
        empty.name = "Empty".into();
        let overview = [stacked[0].clone(), stacked[1].clone(), empty];
        view.update(
            &overview,
            true,
            StripPresentation::WorkspaceOverview,
            &parent,
            &handler,
        );
        view.position_after(0.0, parent.bounds().size.height);
        assert_eq!(view.capsules.len(), 3);
        assert_eq!(view.buttons.len(), 3);
        unsafe { view.buttons[2].button.ns_button().performClick(None) };
        assert!(matches!(
            action_rx.try_recv().unwrap(),
            MenuAction::SwitchToWorkspace(11)
        ));
        let active_workspace = overview.iter().find(|workspace| workspace.active).unwrap();
        let popup = view.window_menu(active_workspace.windows.iter(), &handler);
        for (index, window) in active_workspace.windows.iter().enumerate() {
            popup.ns_menu().performActionForItemAtIndex(index as isize);
            assert!(
                matches!(action_rx.try_recv().unwrap(), MenuAction::FocusWindow(id) if id == window.id)
            );
        }
        let mut draggable = active(&windows[..2]);
        for (index, window) in draggable[0].windows.iter_mut().enumerate() {
            window.layout_position = Some(crate::actor::menu_bar::MenuLayoutPosition {
                column_id: crate::layout_engine::ColumnId(index as u64 + 40),
                column: index,
                row: 0,
            });
        }
        draggable[0].columns = draggable[0]
            .windows
            .iter()
            .map(|window| crate::actor::menu_bar::MenuColumn {
                id: window.layout_position.unwrap().column_id,
                members: vec![window.id],
            })
            .collect();
        view.update(&draggable, true, StripPresentation::Combined, &parent, &handler);
        view.position_after(0.0, parent.bounds().size.height);
        view.set_context_menu(menu.ns_menu());
        let content = view.capsules[0].content.clone();
        assert!(content.acceptsFirstMouse(None));
        assert_eq!(content.menu().unwrap(), menu.ns_menu().retain());
        for item in &view.buttons {
            assert_eq!(item.button.ns_button().menu().unwrap(), menu.ns_menu().retain());
        }
        let source = view.buttons[0].button.ns_button().retain();
        let image_rect = source.cell().unwrap().imageRectForBounds(source.bounds());
        let window_point = source.convertPoint_toView(
            NSPoint::new(
                source.bounds().size.width / 2.0,
                source.bounds().size.height / 2.0,
            ),
            None,
        );
        let content_point = content.convertPoint_fromView(window_point, None);
        let super_point =
            content.convertPoint_toView(content_point, unsafe { content.superview() }.as_deref());
        assert!(std::ptr::eq(&*content.hitTest(super_point).unwrap(), &**content));
        let event = |kind, point| {
            NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(kind,point,NSEventModifierFlags::empty(),0.0,native_window.windowNumber(),None,0,1,1.0).unwrap()
        };
        content.mouseDown(&event(NSEventType::LeftMouseDown, window_point));
        assert!(super::drag::busy());
        let mut latest = draggable.clone();
        latest[0].windows[0].title = "updated while pressed".into();
        latest[0].windows[0].focused = !latest[0].windows[0].focused;
        view.update(&latest, true, StripPresentation::Combined, &parent, &handler);
        assert!(std::ptr::eq(&*source, view.buttons[0].button.ns_button()));
        view.set_mode(true);
        assert_eq!(
            source.cell().unwrap().imageRectForBounds(source.bounds()),
            image_rect
        );
        content.mouseUp(&event(NSEventType::LeftMouseUp, window_point));
        assert!(!super::drag::busy());
        assert!(
            matches!(action_rx.try_recv().unwrap(),MenuAction::FocusWindow(id) if id==draggable[0].windows[0].id)
        );
        view.update(&latest, true, StripPresentation::Combined, &parent, &handler);
        assert!(!std::ptr::eq(&*source, view.buttons[0].button.ns_button()));
        view.position_after(0.0, parent.bounds().size.height);
        let content = view.capsules[0].content.clone();
        let source = view.buttons[0].button.ns_button();
        let point = source.convertPoint_toView(NSPoint::new(12.0, 11.0), None);
        content.mouseDown(&event(NSEventType::LeftMouseDown, point));
        assert!(super::drag::busy());
        content.mouseUp(&event(NSEventType::LeftMouseUp, NSPoint::new(-1000.0, -1000.0)));
        assert!(!super::drag::busy());
        assert!(
            action_rx.try_recv().is_err(),
            "release outside cancels without a click"
        );
        let source = view.buttons[0].button.ns_button();
        source.setImage(None);
        let point = source.convertPoint_toView(NSPoint::new(12.0, 11.0), None);
        content.mouseDown(&event(NSEventType::LeftMouseDown, point));
        assert!(super::drag::busy());
        content.mouseDragged(&event(
            NSEventType::LeftMouseDragged,
            NSPoint::new(point.x + 8.0, point.y),
        ));
        assert!(
            !super::drag::busy(),
            "missing source artwork must cancel without a stuck native session"
        );
        content.mouseUp(&event(NSEventType::LeftMouseUp, point));
        assert!(action_rx.try_recv().is_err());
        let mut empty = latest.clone();
        empty[0].windows.clear();
        empty[0].columns.clear();
        view.update(&empty, true, StripPresentation::Combined, &parent, &handler);
        assert_eq!(
            view.buttons.len(),
            1,
            "real empty workspace has a numbered target"
        );
        assert_eq!(view.width(), active_width(&empty, true));
        assert!(
            view.buttons[0]
                .button
                .ns_button()
                .accessibilityLabel()
                .unwrap()
                .to_string()
                .starts_with("Workspace ")
        );
        let assert_outline = |capsule: &WorkspaceCapsule, mode: bool| {
            let appearance = capsule.host().effectiveAppearance();
            let dark = dark_appearance(&appearance);
            let contrast =
                NSWorkspace::sharedWorkspace().accessibilityDisplayShouldIncreaseContrast();
            appearance.performAsCurrentDrawingAppearance(&block2::RcBlock::new(|| {
                let layer = &capsule.outline;
                let color = NSColor::systemBlueColor();
                assert_eq!(
                    capsule.content_host.layer().unwrap().backgroundColor().is_some(),
                    capsule.workspace.active && mode
                );
                let expected = if capsule.workspace.active {
                    NSColor::labelColor().colorWithAlphaComponent(if contrast {
                        0.65
                    } else if dark {
                        0.22
                    } else {
                        0.18
                    })
                } else {
                    NSColor::separatorColor()
                };
                assert_eq!(
                    layer.lineWidth(),
                    if capsule.workspace.active && contrast {
                        1.0
                    } else {
                        0.5
                    }
                );
                assert!(std::ptr::eq(
                    unsafe { capsule.material_view().superview() }.as_deref().unwrap(),
                    capsule.host()
                ));
                assert_eq!(capsule.material_view().frame(), capsule.host().bounds());
                let material_layer = capsule.material_view().layer().unwrap();
                assert_eq!(material_layer.borderWidth(), 0.0);
                assert!(objc2_core_graphics::CGColor::equal_to_color(
                    layer.strokeColor().as_deref(),
                    Some(&expected.CGColor())
                ));
                match &capsule.material {
                    WorkspaceMaterial::Glass(glass) => {
                        assert!(material_layer.backgroundColor().is_none());
                        assert_eq!(glass.tintColor().is_some(), capsule.workspace.active);
                        if let Some(tint) = glass.tintColor() {
                            let expected =
                                color.colorWithAlphaComponent(if dark { 0.16 } else { 0.10 });
                            assert!(objc2_core_graphics::CGColor::equal_to_color(
                                Some(&tint.CGColor()),
                                Some(&expected.CGColor())
                            ));
                        }
                    }
                    WorkspaceMaterial::Visual(_) => {
                        assert_eq!(
                            material_layer.backgroundColor().is_some(),
                            capsule.workspace.active
                        );
                        if capsule.workspace.active {
                            let expected =
                                color.colorWithAlphaComponent(if dark { 0.16 } else { 0.10 });
                            assert!(objc2_core_graphics::CGColor::equal_to_color(
                                material_layer.backgroundColor().as_deref(),
                                Some(&expected.CGColor())
                            ));
                        }
                    }
                }
            }));
        };
        let mut workspace_keys =
            slotmap::SlotMap::<crate::layout_engine::VirtualWorkspaceId, ()>::with_key();
        let mut compact = vec![empty[0].clone(); 3];
        for (workspace, index) in compact.iter_mut().zip([1, 2, 99]) {
            workspace.key = workspace_keys.insert(());
            workspace.id = format!("{:?}", workspace.key);
            workspace.active = false;
            workspace.index = index;
            workspace.name = format!("Custom {index}");
        }
        let prior_appearance = parent.appearance();
        let mut styled = compact.clone();
        styled[0].active = true;
        styled[0].windows = vec![windows[0].clone()];
        styled[0].windows[0].focused = true;
        let contrast = NSWorkspace::sharedWorkspace().accessibilityDisplayShouldIncreaseContrast();
        view.update(&styled, true, StripPresentation::Combined, &parent, &handler);
        view.position_after(0.0, CAPSULE_HEIGHT);
        view.stop_motion();
        let retained_host = view.capsules[0].host().retain();
        let retained_material = view.capsules[0].material_view().retain();
        let retained_button = view.buttons[0].button.ns_button().retain();
        for name in [
            unsafe { NSAppearanceNameAqua },
            unsafe { NSAppearanceNameDarkAqua },
            unsafe { NSAppearanceNameAqua },
        ] {
            let appearance = NSAppearance::appearanceNamed(name).unwrap();
            parent.setAppearance(Some(&appearance));
            for mode in [false, true, false] {
                view.set_mode(mode);
                assert!(std::ptr::eq(&*retained_host, view.capsules[0].host()));
                assert!(std::ptr::eq(
                    &*retained_material,
                    view.capsules[0].material_view()
                ));
                assert!(std::ptr::eq(
                    &*retained_button,
                    view.buttons[0].button.ns_button()
                ));
                assert_outline(&view.capsules[0], mode);
                verify_outline_pixels(&view.capsules[0]);
                let button = view.buttons[0].button.ns_button();
                let point = button.convertPoint_toView(
                    NSPoint::new(
                        button.bounds().size.width / 2.0,
                        button.bounds().size.height / 2.0,
                    ),
                    Some(&parent),
                );
                let hit = parent.hitTest(point).unwrap();
                assert!(
                    std::ptr::eq(&*hit, button as &NSView) || hit.isDescendantOf(button),
                    "wrapper must preserve exact icon hit targets"
                );
                appearance.performAsCurrentDrawingAppearance(&block2::RcBlock::new(|| {
                    let alpha = match (dark_appearance(&appearance), contrast) {
                        (true, false) => 0.24,
                        (false, false) => 0.16,
                        (true, true) => 0.32,
                        (false, true) => 0.24,
                    };
                    let expected = NSColor::labelColor().colorWithAlphaComponent(alpha);
                    assert!(objc2_core_graphics::CGColor::equal_to_color(
                        view.capsules[0].focus.layer.backgroundColor().as_deref(),
                        Some(&expected.CGColor())
                    ));
                }));
                assert!(retained_button.isAccessibilitySelected());
            }
            verify_mode_pixels(&view.capsules[0]);
        }
        for (name, dark) in [
            (unsafe { NSAppearanceNameAqua }, false),
            (unsafe { NSAppearanceNameDarkAqua }, true),
            (unsafe { NSAppearanceNameAqua }, false),
        ] {
            let appearance = NSAppearance::appearanceNamed(name).unwrap();
            parent.setAppearance(Some(&appearance));
            view.update(&styled, true, StripPresentation::Combined, &parent, &handler);
            view.position_after(0.0, CAPSULE_HEIGHT);
            view.stop_motion();
            for mode in [false, true, false] {
                view.set_mode(mode);
                for capsule in &view.capsules {
                    assert_outline(capsule, mode);
                }
                let focused = view.buttons[0].button.ns_button();
                assert!(focused.isAccessibilitySelected());
                assert_eq!(
                    focused
                        .effectiveAppearance()
                        .bestMatchFromAppearancesWithNames(&NSArray::from_slice(&[
                            unsafe { NSAppearanceNameAqua },
                            unsafe { NSAppearanceNameDarkAqua }
                        ]))
                        .as_deref(),
                    Some(name)
                );
                appearance.performAsCurrentDrawingAppearance(&block2::RcBlock::new(|| {
                    let alpha = match (dark, contrast) {
                        (true, false) => 0.24,
                        (false, false) => 0.16,
                        (true, true) => 0.32,
                        (false, true) => 0.24,
                    };
                    let expected = NSColor::labelColor().colorWithAlphaComponent(alpha);
                    assert!(
                        objc2_core_graphics::CGColor::equal_to_color(
                            view.capsules[0].focus.layer.backgroundColor().as_deref(),
                            Some(&expected.CGColor())
                        ),
                        "focused backgrounds must follow retained native light and dark appearances"
                    );
                }));
                assert_eq!(view.capsules[0].focus.layer.borderWidth(), 1.0);
                appearance.performAsCurrentDrawingAppearance(&block2::RcBlock::new(|| {
                    let alpha = match (dark, contrast) {
                        (true, false) => 0.65,
                        (false, false) => 0.50,
                        (true, true) => 0.90,
                        (false, true) => 0.80,
                    };
                    let expected = NSColor::labelColor().colorWithAlphaComponent(alpha);
                    assert!(objc2_core_graphics::CGColor::equal_to_color(
                        view.capsules[0].focus.layer.borderColor().as_deref(),
                        Some(&expected.CGColor())
                    ));
                }));
                assert_eq!(view.capsules[0].focus.layer.cornerRadius(), 6.0);
                if let Ok(prefix) = std::env::var("WM_MENU_BAR_SNAPSHOT") {
                    for (kind, native) in [
                        ("workspace", view.capsules[0].host()),
                        ("focused", focused as &NSView),
                    ] {
                        let bitmap =
                            native.bitmapImageRepForCachingDisplayInRect(native.bounds()).unwrap();
                        native.cacheDisplayInRect_toBitmapImageRep(native.bounds(), &bitmap);
                        let data = unsafe {
                            bitmap.representationUsingType_properties(
                                NSBitmapImageFileType::PNG,
                                &NSDictionary::new(),
                            )
                        }
                        .unwrap();
                        let theme = if dark { "dark" } else { "light" };
                        let state = if mode { "mode" } else { "normal" };
                        std::fs::write(
                            format!("{prefix}.{theme}-{state}-{kind}.png"),
                            data.to_vec(),
                        )
                        .unwrap();
                    }
                }
                assert!(
                    view.buttons[view.capsules[1].start]
                        .button
                        .ns_button()
                        .layer()
                        .unwrap()
                        .backgroundColor()
                        .is_none()
                );
            }
            verify_focus_artwork(&mut view, &parent, &handler, &windows);
            let mut active_empty = styled.clone();
            active_empty[0].active = false;
            active_empty[1].active = true;
            view.update(
                &active_empty,
                true,
                StripPresentation::WorkspaceOverview,
                &parent,
                &handler,
            );
            view.position_after(0.0, CAPSULE_HEIGHT);
            view.stop_motion();
            view.set_mode(false);
            let preview = view.buttons[view.capsules[1].start].button.ns_button();
            assert!(
                preview.isAccessibilitySelected(),
                "workspace selection must survive focused-background refresh"
            );
            assert!(preview.layer().unwrap().backgroundColor().is_none());
            assert_eq!(
                view.capsules[1].host().frame().size,
                NSSize::new(CAPSULE_HEIGHT, CAPSULE_HEIGHT)
            );
            let mut fallback = WorkspaceCapsule::new(mtm, 0, &styled[0], &handler);
            let effect = NSVisualEffectView::new(mtm);
            effect.addSubview(&fallback.content_host);
            fallback.material_view().removeFromSuperview();
            fallback.host().addSubview(&effect);
            fallback.material = WorkspaceMaterial::Visual(effect);
            fallback.material_view().setWantsLayer(true);
            parent.addSubview(fallback.host());
            fallback
                .host()
                .setFrame(NSRect::new(NSPoint::ZERO, NSSize::new(40.0, CAPSULE_HEIGHT)));
            fallback.material_view().setFrame(fallback.host().bounds());
            fallback.content_host.setFrame(fallback.host().bounds());
            fallback.prepare_geometry(CAPSULE_HEIGHT);
            for mode in [false, true, false] {
                fallback.set_mode(mode);
                assert_outline(&fallback, mode);
                verify_outline_pixels(&fallback);
            }
            fallback.workspace.active = false;
            fallback.set_mode(false);
            assert_outline(&fallback, false);
            fallback.host().removeFromSuperview();
        }
        parent.setAppearance(prior_appearance.as_deref());
        view.update(&[], true, StripPresentation::Combined, &parent, &handler);
        for presentation in [
            StripPresentation::Combined,
            StripPresentation::WorkspaceOverview,
        ] {
            view.update(&compact, true, presentation, &parent, &handler);
            view.position_after(0.0, CAPSULE_HEIGHT);
            assert_eq!(view.capsules.len(), 3);
            assert_eq!(view.capsules[0].width, CAPSULE_HEIGHT);
            assert_eq!(view.capsules[1].width, CAPSULE_HEIGHT);
            for capsule in &view.capsules[..2] {
                assert_eq!(
                    capsule.host().frame().size,
                    NSSize::new(CAPSULE_HEIGHT, CAPSULE_HEIGHT)
                );
                assert_eq!(
                    capsule.host().layer().unwrap().cornerRadius(),
                    CAPSULE_HEIGHT / 2.0
                );
            }
            assert_eq!(
                (view.capsules[1].host().frame().origin.x
                    + view.capsules[1].host().frame().size.width)
                    - view.capsules[0].host().frame().origin.x,
                60.0
            );
            assert!(view.capsules[2].width > BUTTON_WIDTH);
            for (button, workspace) in view.buttons.iter().zip(&compact) {
                let native = button.button.ns_button();
                assert_eq!(
                    native.title().to_string(),
                    empty_workspace_title(workspace.index)
                );
                assert_eq!(
                    native.accessibilityLabel().unwrap().to_string(),
                    format!("Workspace {}: empty", workspace.name)
                );
                assert!(native.image().is_none());
                let text = native.attributedTitle().size().width;
                assert!(
                    text <= native.cell().unwrap().titleRectForBounds(native.bounds()).size.width,
                    "empty workspace number must fit its native title rectangle"
                );
                unsafe { native.performClick(None) };
                assert!(
                    matches!(action_rx.try_recv().unwrap(), MenuAction::SwitchToWorkspace(index) if index == workspace.index)
                );
                if let Ok(prefix) = std::env::var("WM_MENU_BAR_SNAPSHOT") {
                    let bitmap =
                        native.bitmapImageRepForCachingDisplayInRect(native.bounds()).unwrap();
                    native.cacheDisplayInRect_toBitmapImageRep(native.bounds(), &bitmap);
                    let data = unsafe {
                        bitmap.representationUsingType_properties(
                            NSBitmapImageFileType::PNG,
                            &NSDictionary::new(),
                        )
                    }
                    .unwrap();
                    std::fs::write(
                        format!("{prefix}.empty-{}.png", workspace.index + 1),
                        data.to_vec(),
                    )
                    .unwrap();
                }
            }
            for mode in [false, true, false] {
                view.set_mode(mode);
                for capsule in &view.capsules {
                    assert_outline(capsule, mode);
                }
            }
        }
        for selected in 0..compact.len() {
            for (index, workspace) in compact.iter_mut().enumerate() {
                workspace.active = index == selected;
            }
            for presentation in [
                StripPresentation::Combined,
                StripPresentation::WorkspaceOverview,
            ] {
                view.update(&compact, true, presentation, &parent, &handler);
                view.position_after(0.0, CAPSULE_HEIGHT);
                for mode in [false, true, false] {
                    view.set_mode(mode);
                    for capsule in &view.capsules {
                        assert_outline(capsule, mode);
                    }
                }
            }
        }
        let mut ordered = vec![empty[0].clone(); 3];
        for (index, workspace) in ordered.iter_mut().enumerate() {
            workspace.key = workspace_keys.insert(());
            workspace.id = format!("{:?}", workspace.key);
            workspace.index = index;
            workspace.name = format!("{:02}", index + 1);
            workspace.active = index == 0;
        }
        ordered[0].windows = vec![windows[0].clone()];
        ordered[2].windows = vec![windows[1].clone()];
        view.update(&ordered, true, StripPresentation::Combined, &parent, &handler);
        let ordered_hosts =
            view.capsules.iter().map(|capsule| capsule.host().retain()).collect::<Vec<_>>();
        for selected in [0, 1, 2, 0] {
            for (index, workspace) in ordered.iter_mut().enumerate() {
                workspace.active = index == selected;
            }
            view.update(&ordered, true, StripPresentation::Combined, &parent, &handler);
            view.position_after(0.0, CAPSULE_HEIGHT);
            assert_eq!(view.capsules.len(), 3);
            for (index, capsule) in view.capsules.iter().enumerate() {
                assert_eq!(capsule.workspace.key, ordered[index].key);
                assert!(
                    std::ptr::eq(&*ordered_hosts[index], capsule.host()),
                    "activation must retain the host belonging to each workspace identity"
                );
                if index > 0 {
                    let previous = view.capsules[index - 1].host().frame();
                    assert!(
                        capsule.host().frame().origin.x >= previous.origin.x + previous.size.width
                    );
                }
                if index == 1 || index != selected {
                    let native = view.buttons[capsule.start].button.ns_button();
                    if index == 1 {
                        assert_eq!(native.title().to_string(), "02");
                    }
                    unsafe { native.performClick(None) };
                    assert!(
                        matches!(action_rx.try_recv().unwrap(), MenuAction::SwitchToWorkspace(target) if target == index),
                        "a middle empty workspace must keep its catalog position and click destination"
                    );
                }
            }
            for mode in [false, true, false] {
                view.set_mode(mode);
                for capsule in &view.capsules {
                    assert_outline(capsule, mode);
                }
            }
        }
        let mut capacity_display = displays[0].clone();
        capacity_display.workspaces = ordered
            .iter()
            .map(|section| {
                let mut raw = workspace(&section.id, section.active, section.windows.clone());
                raw.key = section.key;
                raw.index = section.index;
                raw.name = section.name.clone();
                raw
            })
            .collect();
        capacity_display.workspaces[0].window_list = windows[..7]
            .iter()
            .enumerate()
            .map(|(index, window)| {
                let mut window = window.clone();
                window.layout_position = Some(crate::actor::menu_bar::MenuLayoutPosition {
                    column_id: crate::layout_engine::ColumnId(index as u64 + 1),
                    column: index,
                    row: 0,
                });
                window
            })
            .collect();
        let mut builtin = capacity_display.clone();
        builtin.is_builtin = true;
        builtin.workspaces[0].window_list.truncate(1);
        for mode in [MenuBarDisplayMode::All, MenuBarDisplayMode::Active] {
            for show_empty in [false, true] {
                let mut capacity_settings = settings.clone();
                capacity_settings.mode = mode;
                capacity_settings.show_empty = show_empty;
                let overview = viewport_width(
                    &builtin,
                    &capacity_settings,
                    StripPresentation::WorkspaceOverview,
                );
                let capacities = [
                    StripPresentation::Combined,
                    StripPresentation::WorkspaceOverview,
                ]
                .map(|presentation| {
                    viewport_width(&capacity_display, &capacity_settings, presentation)
                });
                for selected in [0, 1, 2, 0] {
                    for (index, entry) in capacity_display.workspaces.iter_mut().enumerate() {
                        entry.is_active = index == selected;
                    }
                    for (index, presentation) in [
                        StripPresentation::Combined,
                        StripPresentation::WorkspaceOverview,
                    ]
                    .into_iter()
                    .enumerate()
                    {
                        assert_eq!(
                            viewport_width(&capacity_display, &capacity_settings, presentation),
                            capacities[index],
                            "activation must not resize a display viewport"
                        );
                        view.update(
                            &workspace_windows(&capacity_display, &capacity_settings),
                            true,
                            presentation,
                            &parent,
                            &handler,
                        );
                        assert!(view.width() <= capacities[index]);
                    }
                    assert_eq!(
                        viewport_width(
                            &builtin,
                            &capacity_settings,
                            StripPresentation::WorkspaceOverview
                        ),
                        overview
                    );
                }
                capacity_display.workspaces[0].window_list.push(windows[13].clone());
                assert_eq!(
                    viewport_width(
                        &builtin,
                        &capacity_settings,
                        StripPresentation::WorkspaceOverview
                    ),
                    overview,
                    "external window changes must not reserve space on the builtin display"
                );
            }
        }
        let mut moving = ordered.clone();
        moving[0].windows = capacity_display.workspaces[0].window_list[..7].to_vec();
        for (index, entry) in moving.iter_mut().enumerate() {
            entry.active = index == 0;
        }
        view.update(&moving, true, StripPresentation::Combined, &parent, &handler);
        view.position_after(0.0, CAPSULE_HEIGHT);
        view.stop_motion();
        let initial = view.capsules[0].host().frame();
        moving[0].active = false;
        moving[1].active = true;
        view.update(&moving, true, StripPresentation::Combined, &parent, &handler);
        view.position_after(0.0, CAPSULE_HEIGHT);
        let target_width = view.capsules[0].width;
        if !NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion() {
            assert_eq!(
                view.capsules[0].host().frame(),
                initial,
                "navigation begins at the actual previous frame"
            );
            let timer = view.geometry.borrow().timer.as_ref().unwrap().clone();
            std::thread::sleep(std::time::Duration::from_millis(45));
            timer.fire();
            let middle = view.capsules[0].host().frame();
            assert!(
                middle.size.width < initial.size.width && middle.size.width > target_width,
                "native frames must interpolate instead of jumping to the endpoint"
            );
            let native = view.buttons[view.capsules[1].start].button.ns_button();
            let point = native.convertPoint_toView(
                NSPoint::new(
                    native.bounds().size.width / 2.0,
                    native.bounds().size.height / 2.0,
                ),
                Some(&parent),
            );
            assert!(
                std::ptr::eq(&*parent.hitTest(point).unwrap(), &***native),
                "hit regions must follow the visible control during motion"
            );
            moving[0].windows[0].title.push_str(" during transition");
            view.update(&moving, true, StripPresentation::Combined, &parent, &handler);
            view.position_after(0.0, CAPSULE_HEIGHT);
            assert!(
                std::ptr::eq(&*timer, &**view.geometry.borrow().timer.as_ref().unwrap()),
                "title updates must preserve the existing transition"
            );
            assert_eq!(view.capsules[0].host().frame(), middle);
            moving[0].active = true;
            moving[1].active = false;
            view.update(&moving, true, StripPresentation::Combined, &parent, &handler);
            view.position_after(0.0, CAPSULE_HEIGHT);
            assert_eq!(
                view.capsules[0].host().frame(),
                middle,
                "rapid reversal must start at its current frame"
            );
            let reverse = view.geometry.borrow().timer.as_ref().unwrap().clone();
            std::thread::sleep(std::time::Duration::from_millis(190));
            reverse.fire();
            assert_eq!(view.capsules[0].host().frame(), initial);
            assert!(!reverse.isValid());
        } else {
            assert_eq!(view.capsules[0].host().frame().size.width, target_width);
        }
        moving[0].active = false;
        moving[1].active = true;
        view.update(&moving, true, StripPresentation::Combined, &parent, &handler);
        view.position_after(0.0, CAPSULE_HEIGHT);
        let previous_timer = view.geometry.borrow().timer.clone();
        let frames = view.geometry.borrow().frames.iter().cloned().collect();
        view.geometry.borrow_mut().present(frames, GeometryChange::Immediate);
        assert_eq!(
            view.capsules[0].host().frame().size.width,
            view.capsules[0].width,
            "disabled motion applies the endpoint immediately"
        );
        if let Some(timer) = previous_timer {
            assert!(!timer.isValid());
        }
        view.stop_motion();
        assert!(view.geometry.borrow().timer.is_none());
        view.update(&empty, true, StripPresentation::Combined, &parent, &handler);
        assert_eq!(view.width(), active_width(&empty, true));
        let mut populated_outline = empty.clone();
        populated_outline[0].windows = windows.clone();
        view.update(
            &populated_outline,
            true,
            StripPresentation::Combined,
            &parent,
            &handler,
        );
        for mode in [false, true, false] {
            view.set_mode(mode);
            assert_outline(&view.capsules[0], mode);
        }
        view.update(&empty, true, StripPresentation::Combined, &parent, &handler);
        view.stop_motion();
        let glass_host = view.capsules[0].host().retain();
        let stable_content = view.capsules[0].content_host.clone();
        let mut replacement = empty.clone();
        replacement[0].windows = draggable[0].windows.clone();
        replacement[0].columns = draggable[0].columns.clone();
        view.update(
            &replacement,
            true,
            StripPresentation::Combined,
            &parent,
            &handler,
        );
        view.position_after(0.0, CAPSULE_HEIGHT);
        assert!(
            std::ptr::eq(&*glass_host, view.capsules[0].host()),
            "workspace updates must retain the glass compositor surface"
        );
        assert!(std::ptr::eq(&*stable_content, &*view.capsules[0].content_host));
        for button in &view.buttons {
            let frame = button.button.ns_button().frame();
            assert!(frame.origin.x >= WORKSPACE_PADDING);
            assert_eq!(frame.origin.y, CONTROL_INSET);
            assert_eq!(frame.size.height, CAPSULE_HEIGHT - CONTROL_INSET * 2.0);
            assert!(
                frame.origin.x + frame.size.width <= view.capsules[0].width - WORKSPACE_PADDING
            );
            assert_eq!(
                button
                    .button
                    .ns_button()
                    .cell()
                    .unwrap()
                    .imageRectForBounds(button.button.ns_button().bounds())
                    .size
                    .height,
                ICON_SIZE
            );
        }
        view.stop_motion();
        let unchanged_content = view.capsules[0].content.clone();
        view.update(
            &replacement,
            true,
            StripPresentation::Combined,
            &parent,
            &handler,
        );
        assert!(std::ptr::eq(&*unchanged_content, &*view.capsules[0].content));
        replacement[0].windows[0].title.push_str(" refreshed");
        view.update(
            &replacement,
            true,
            StripPresentation::Combined,
            &parent,
            &handler,
        );
        assert!(std::ptr::eq(&*glass_host, view.capsules[0].host()));
        assert!(
            unsafe {
                stable_content
                    .layer()
                    .unwrap()
                    .animationForKey(&NSString::from_str(WORKSPACE_TRANSITION))
            }
            .is_none(),
            "title-only updates must not start navigation motion"
        );
        for (index, window) in replacement[0].windows.iter_mut().enumerate() {
            window.focused = index == 0;
        }
        view.update(
            &replacement,
            true,
            StripPresentation::Combined,
            &parent,
            &handler,
        );
        view.position_after(0.0, CAPSULE_HEIGHT);
        view.stop_motion();
        let indicator = view.capsules[0].focus.layer.clone();
        let origin = indicator.frame();
        let icon_frames: Vec<_> =
            view.buttons.iter().map(|item| item.button.ns_button().frame()).collect();
        replacement[0].windows[0].focused = false;
        replacement[0].windows[1].focused = true;
        view.update(
            &replacement,
            true,
            StripPresentation::Combined,
            &parent,
            &handler,
        );
        view.position_after(0.0, CAPSULE_HEIGHT);
        let target = view.capsules[0].focus.target.get().unwrap();
        assert_ne!(origin, target, "fixture must select distinct controls");
        assert!(std::ptr::eq(&*indicator, &*view.capsules[0].focus.layer));
        assert!(
            view.geometry.borrow().timer.is_none(),
            "focus feedback must not move workspace geometry"
        );
        assert!(
            unsafe {
                stable_content
                    .layer()
                    .unwrap()
                    .animationForKey(&NSString::from_str(WORKSPACE_TRANSITION))
            }
            .is_none(),
            "focus changes must move the indicator instead of dissolving the content"
        );
        if !NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion() {
            assert_eq!(
                indicator.frame(),
                origin,
                "focus motion must begin at the displayed selection"
            );
            let timer = view.capsules[0].focus.timer.borrow().as_ref().unwrap().clone();
            std::thread::sleep(std::time::Duration::from_millis(60));
            timer.fire();
            let middle = indicator.frame();
            assert!(
                middle.origin.x > origin.origin.x && middle.origin.x < target.origin.x,
                "selection background must visibly travel between icon positions"
            );
            replacement[0].windows[1].title.push_str(" during focus motion");
            view.update(
                &replacement,
                true,
                StripPresentation::Combined,
                &parent,
                &handler,
            );
            view.position_after(0.0, CAPSULE_HEIGHT);
            assert_eq!(indicator.frame(), middle);
            assert!(
                std::ptr::eq(
                    &*timer,
                    &**view.capsules[0].focus.timer.borrow().as_ref().unwrap()
                ),
                "metadata updates must preserve the running focus motion"
            );
            replacement[0].windows[1].focused = false;
            replacement[0].windows[0].focused = true;
            view.update(
                &replacement,
                true,
                StripPresentation::Combined,
                &parent,
                &handler,
            );
            view.position_after(0.0, CAPSULE_HEIGHT);
            assert_eq!(
                indicator.frame(),
                middle,
                "focus reversal must start at the visible intermediate frame"
            );
            assert!(!timer.isValid());
            let reverse = view.capsules[0].focus.timer.borrow().as_ref().unwrap().clone();
            std::thread::sleep(std::time::Duration::from_millis(250));
            reverse.fire();
            assert_eq!(indicator.frame(), origin);
            assert!(!reverse.isValid());
        } else {
            assert_eq!(indicator.frame(), target);
        }
        for (item, frame) in view.buttons.iter().zip(icon_frames) {
            assert_eq!(
                item.button.ns_button().frame(),
                frame,
                "icons must remain stationary during focus motion"
            );
            assert_eq!(
                item.button
                    .ns_button()
                    .cell()
                    .unwrap()
                    .imageRectForBounds(item.button.ns_button().bounds())
                    .size
                    .height,
                ICON_SIZE
            );
        }
        view.capsules[0].focus.present(Some(target), false);
        assert_eq!(
            indicator.frame(),
            target,
            "disabled motion must apply the selection immediately"
        );
        view.stop_motion();
        replacement[0].windows[0].title.push_str(" after focus");
        view.update(
            &replacement,
            true,
            StripPresentation::Combined,
            &parent,
            &handler,
        );
        assert!(
            view.capsules[0].focus.timer.borrow().is_none(),
            "metadata updates must not restart focus feedback"
        );
        let mut switched = replacement.clone();
        switched[0].active = false;
        view.update(&switched, true, StripPresentation::Combined, &parent, &handler);
        view.position_after(0.0, CAPSULE_HEIGHT);
        assert!(std::ptr::eq(&*glass_host, view.capsules[0].host()));
        let motion = unsafe {
            stable_content
                .layer()
                .unwrap()
                .animationForKey(&NSString::from_str(WORKSPACE_TRANSITION))
        };
        assert_eq!(
            motion.is_some(),
            !NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion()
        );
        if let Some(motion) = motion {
            assert_eq!(motion.duration(), 0.18);
        }
        view.stop_motion();
        assert!(
            unsafe {
                stable_content
                    .layer()
                    .unwrap()
                    .animationForKey(&NSString::from_str(WORKSPACE_TRANSITION))
            }
            .is_none()
        );
        settings.mode = MenuBarDisplayMode::Active;
        let selected = workspace_windows(&displays[1], &settings);
        assert_eq!(selected.len(), 1);
        assert!(selected[0].active);
        settings.show_window_list = false;
        assert!(workspace_windows(&displays[1], &settings).is_empty());
        println!(
            "PASS: native menubar fixed icon geometry/artwork, native light/dark selection, overlapping workspace pixels/actions, exact window menus, overflow, and cleanup"
        );
    }
}

fn main() {
    let mtm = MainThreadMarker::new().unwrap();
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    if std::env::args().any(|arg| arg == "--interactive") {
        window_list::interactive();
        return;
    }
    status_mode::verify();
    window_list::verify();
}
