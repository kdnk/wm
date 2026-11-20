// Modified by kdnk for wm on 2026-11-19. Original attribution is preserved in LICENSE.
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSBitmapImageRep, NSColor,
    NSColorSpace, NSEventMask, NSPanel, NSWindow, NSWindowStyleMask,
};
use objc2_core_foundation::{CFRunLoop, CGPoint, CGRect, CGSize, kCFRunLoopDefaultMode};
use objc2_core_graphics::{CGDisplayBounds, CGMainDisplayID};
use objc2_foundation::{NSData, NSDate, NSDefaultRunLoopMode};
use wm::common::config::WindowBorderSettings;
use wm::sys::skylight::{CGSEventType, KnownCGSEvent};
use wm::sys::window_notify;
use wm::sys::window_server::{WindowIterator, WindowServerId};
use wm::ui::window_borders::{BorderTarget, WindowBorders};

type Rgb = [u8; 3];
const ACTIVE: Rgb = [255, 0, 255];
const INACTIVE: Rgb = [0, 255, 0];
const BLACK: Rgb = [0; 3];
const WHITE: Rgb = [255; 3];

fn center_line_rect(x: f64, width: f64, height: f64) -> CGRect {
    CGRect::new(CGPoint::new(x, 150.0 - height / 2.0), CGSize::new(width, height))
}

fn cocoa_frame(frame: CGRect) -> CGRect {
    let screen_height = CGDisplayBounds(CGMainDisplayID()).size.height;
    CGRect::new(
        CGPoint::new(
            frame.origin.x,
            screen_height - frame.origin.y - frame.size.height,
        ),
        frame.size,
    )
}

fn panel(mtm: MainThreadMarker, frame: CGRect, color: &NSColor) -> Retained<NSPanel> {
    let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
        NSPanel::alloc(mtm),
        cocoa_frame(frame),
        NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
        NSBackingStoreType::Buffered,
        false,
    );
    panel.setHidesOnDeactivate(false);
    panel.setHasShadow(false);
    panel.setLevel(3);
    panel.setBackgroundColor(Some(color));
    panel.orderFrontRegardless();
    panel.display();
    panel
}

fn check_screen(label: &str, samples: &[(u32, Rgb)]) -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("borders.png");
    let mut mismatches = Vec::new();
    for _ in 0..4 {
        CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, 0.06, false);
        let status = Command::new("/usr/sbin/screencapture")
            .args(["-x", "-R100,100,240,100"])
            .arg(&path)
            .status()?;
        if !status.success() {
            return Err("screen recording permission is required for the border fixture".into());
        }
        let data = NSData::with_bytes(&std::fs::read(&path)?);
        let image = NSBitmapImageRep::imageRepWithData(&data).ok_or("invalid screen capture")?;
        mismatches.clear();
        let sample = |x: u32| -> Result<Rgb, Box<dyn std::error::Error>> {
            let column = ((x - 100) as f64 * image.pixelsWide() as f64 / 240.0) as isize;
            let rgb = image
                .colorAtX_y(column, image.pixelsHigh() / 2)
                .and_then(|color| color.colorUsingColorSpace(&NSColorSpace::sRGBColorSpace()))
                .ok_or("screen capture has no RGB sample")?;
            Ok([
                rgb.redComponent(),
                rgb.greenComponent(),
                rgb.blueComponent(),
            ]
            .map(|channel| (255.0 * channel).round() as u8))
        };
        let palette = [sample(115)?, sample(320)?, sample(332)?, sample(104)?];
        for (i, color) in palette.iter().enumerate() {
            for other in &palette[..i] {
                if color.iter().zip(other).all(|(a, b)| a.abs_diff(*b) < 80) {
                    return Err(
                        format!("fixture swatches are indistinguishable: {palette:?}").into()
                    );
                }
            }
        }
        for &(x, requested) in samples {
            let reference = match requested {
                ACTIVE => palette[0],
                INACTIVE => palette[1],
                WHITE => palette[2],
                BLACK => palette[3],
                _ => unreachable!(),
            };
            let actual = sample(x)?;
            if actual.iter().zip(reference).any(|(a, e)| a.abs_diff(e) > 20) {
                mismatches.push((x, reference, actual));
            }
        }
        if mismatches.is_empty() {
            println!("PASS {label}");
            return Ok(());
        }
    }
    Err(format!("{label}: pixels (x, expected, actual) {mismatches:?}").into())
}

fn pump_appkit(app: &NSApplication) {
    if let Some(event) = unsafe {
        app.nextEventMatchingMask_untilDate_inMode_dequeue(
            NSEventMask::Any,
            Some(&NSDate::dateWithTimeIntervalSinceNow(0.01)),
            NSDefaultRunLoopMode,
            true,
        )
    } {
        app.sendEvent(&event);
    }
    app.updateWindows();
}

fn observed_frame(
    events: &mut wm::actor::Receiver<window_notify::EventData>,
    id: WindowServerId,
    kind: KnownCGSEvent,
    expected: CGRect,
) -> Result<CGRect, Box<dyn std::error::Error>> {
    let app = NSApplication::sharedApplication(MainThreadMarker::new().unwrap());
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(3) {
        pump_appkit(&app);
        while let Ok((_, event)) = events.try_recv() {
            if event.window_id == Some(id.0) && event.event_type == CGSEventType::Known(kind) {
                let query = WindowIterator::new(&[id]).unwrap();
                let frame = query.advance().unwrap().bounds();
                assert_eq!(frame, expected, "{kind:?} delivered stale native bounds");
                println!("PASS {kind:?} reports the fixture window and its new geometry");
                return Ok(frame);
            }
        }
    }
    Err(format!("no {kind:?} notification for fixture window {}", id.0).into())
}

fn left() -> CGRect { center_line_rect(130.0, 50.0, 44.0) }
fn right() -> CGRect { center_line_rect(255.0, 50.0, 44.0) }
fn overlap() -> CGRect { center_line_rect(172.0, 50.0, 44.0) }
fn resized() -> CGRect { center_line_rect(165.0, 65.0, 60.0) }

fn fixture_panels(mtm: MainThreadMarker) -> Result<(), Box<dyn std::error::Error>> {
    let backdrop = panel(
        mtm,
        center_line_rect(100.0, 240.0, 100.0),
        &NSColor::blackColor(),
    );
    let _references: Vec<_> = [(115.0, ACTIVE), (320.0, INACTIVE), (332.0, WHITE)]
        .into_iter()
        .map(|(x, rgb)| {
            let [r, g, b] = rgb.map(|v| f64::from(v) / 255.0);
            let color = NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, 1.0);
            panel(mtm, center_line_rect(x - 3.0, 6.0, 20.0), &color)
        })
        .collect();
    let first = panel(mtm, left(), &NSColor::whiteColor());
    let second = panel(mtm, right(), &NSColor::whiteColor());
    let app = NSApplication::sharedApplication(mtm);
    pump_appkit(&app);
    println!(
        "{}",
        serde_json::to_string(&[
            backdrop.windowNumber(),
            first.windowNumber(),
            second.windowNumber()
        ])?
    );
    for command in std::io::stdin().lock().lines() {
        match command?.as_str() {
            "overlap" => {
                second.setFrame_display(cocoa_frame(overlap()), true);
                second.orderFrontRegardless();
            }
            "separate" => second.setFrame_display(cocoa_frame(right()), true),
            "resize" => first.setFrame_display(cocoa_frame(resized()), true),
            "elevate" => {
                for target in [&first, &second] {
                    cgs::compositor::CgsWindow::from_existing(target.windowNumber().try_into()?)
                        .set_sublevel(2)?;
                }
            }
            _ => return Err("unknown fixture command".into()),
        }
        pump_appkit(&app);
        println!("ready");
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mtm = MainThreadMarker::new().unwrap();
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    app.finishLaunching();
    if std::env::args().nth(1).as_deref() == Some("--panels") {
        return fixture_panels(mtm);
    }
    wm::common::log::init_logging();
    let mut child = Command::new(std::env::current_exe()?)
        .arg("--panels")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let ids: [u32; 3] = serde_json::from_str(&lines.next().ok_or("missing fixture window IDs")??)?;
    let [backdrop_id, first_id, second_id] = ids.map(WindowServerId);
    let mut command = |step: &str| -> Result<(), Box<dyn std::error::Error>> {
        writeln!(child.stdin.as_mut().unwrap(), "{step}")?;
        assert_eq!(lines.next().transpose()?.as_deref(), Some("ready"));
        Ok(())
    };
    let mut events = window_notify::take_receiver().unwrap();
    for kind in [KnownCGSEvent::WindowMoved, KnownCGSEvent::WindowResized] {
        assert_eq!(window_notify::init(CGSEventType::Known(kind)), 0);
    }
    window_notify::update_window_notifications(&[first_id.0, second_id.0]);
    let target = |window, frame| BorderTarget {
        window,
        frame,
        follows_parent: false,
    };
    let mut targets = [target(first_id, left()), target(second_id, right())];
    let mut settings = WindowBorderSettings {
        enabled: true,
        width: 6.0,
        active_color: 0xffff00ff,
        inactive_color: 0xff00ff00,
    };
    let mut borders = WindowBorders::default();
    borders.sync(&targets, Some(first_id), &settings, true);
    check_screen("active and inactive outlines with transparent centers", &[
        (127, ACTIVE),
        (150, WHITE),
        (183, ACTIVE),
        (252, INACTIVE),
        (280, WHITE),
        (308, INACTIVE),
    ])?;

    let screen_y = cocoa_frame(center_line_rect(0.0, 0.0, 0.0)).origin.y;
    for (x, expected) in [
        (127.0, backdrop_id.0 as isize),
        (150.0, first_id.0 as isize),
    ] {
        assert_eq!(
            NSWindow::windowNumberAtPoint_belowWindowWithWindowNumber(
                CGPoint::new(x, screen_y),
                0,
                mtm,
            ),
            expected,
            "border intercepted a hit at x={x}"
        );
    }
    println!("PASS native hit testing through the outline and its center");

    command("overlap")?;
    targets[1].frame =
        observed_frame(&mut events, second_id, KnownCGSEvent::WindowMoved, overlap())?;
    borders.sync(&targets, Some(first_id), &settings, true);
    let number_at = |x| {
        NSWindow::windowNumberAtPoint_belowWindowWithWindowNumber(CGPoint::new(x, screen_y), 0, mtm)
    };
    assert_eq!(number_at(176.), second_id.0 as isize);
    let occlusion = check_screen("a foreground window covers the outline behind it", &[
        (169, INACTIVE),
        (183, WHITE),
        (195, WHITE),
        (270, BLACK),
    ]);

    targets[0].follows_parent = true;
    borders.sync(&targets, Some(first_id), &settings, false);
    check_screen("a focused dialog keeps its outline above the editor", &[
        (183, ACTIVE),
        (195, WHITE),
    ])?;
    borders.sync(&targets, Some(second_id), &settings, false);
    check_screen(
        "an inactive dialog restores target-relative outline ordering",
        &[(169, ACTIVE), (183, WHITE)],
    )?;
    targets[0].follows_parent = false;

    command("separate")?;
    targets[1].frame = observed_frame(&mut events, second_id, KnownCGSEvent::WindowMoved, right())?;
    borders.sync(&targets, Some(second_id), &settings, true);
    check_screen("focus changes both outline colors", &[
        (127, INACTIVE),
        (252, ACTIVE),
    ])?;

    command("resize")?;
    targets[0].frame =
        observed_frame(&mut events, first_id, KnownCGSEvent::WindowResized, resized())?;
    borders.sync(&targets, Some(second_id), &settings, false);
    check_screen(
        "moving and resizing clears the old edge and draws both new edges",
        &[(127, BLACK), (162, INACTIVE), (200, WHITE), (233, INACTIVE)],
    )?;

    borders.sync(&targets[1..], Some(second_id), &settings, false);
    check_screen("removing a target clears only its outline", &[
        (162, BLACK),
        (252, ACTIVE),
    ])?;
    settings.enabled = false;
    borders.sync(&targets, Some(second_id), &settings, false);
    check_screen("disabling borders clears the remaining outline", &[
        (252, BLACK),
        (280, WHITE),
    ])?;

    command("elevate")?;
    for id in [first_id, second_id] {
        assert_eq!(wm::sys::window_server::window_sub_level(id.0), 2);
    }
    command("overlap")?;
    targets[1].frame =
        observed_frame(&mut events, second_id, KnownCGSEvent::WindowMoved, overlap())?;
    settings.enabled = true;
    borders.sync(&targets, Some(first_id), &settings, true);
    check_screen(
        "target-relative outlines follow non-default window sublevels",
        &[(162, ACTIVE), (169, INACTIVE), (200, WHITE)],
    )?;
    drop(child.stdin.take());
    assert!(child.wait()?.success());
    occlusion
}
