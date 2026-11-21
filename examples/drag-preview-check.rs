// Modified by kdnk for wm on 2026-11-15. Original attribution is preserved in LICENSE.
pub use wm::{actor, common, layout_engine, model, sys};
mod ui {
    pub(crate) mod motion {
        include!("../src/ui/motion.rs");
    }
}

mod preview {
    include!("../src/ui/drag_preview.rs");

    use std::path::Path;
    use std::time::Duration;

    use crate::actor::drag::{DropIntent, DropTarget, DropZone};
    use crate::layout_engine::WindowDropAction;
    use crate::sys::screen::SpaceId;

    struct Recording(std::process::Child);

    impl Drop for Recording {
        fn drop(&mut self) {
            if self.0.try_wait().ok().flatten().is_none() {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
    }

    fn verify_motion(
        target: DropTarget,
        movie: Option<&Path>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let appearance = PreviewAppearance {
            increase_contrast: false,
            reduce_transparency: false,
            reduce_motion: false,
        };
        let mut preview = DragPreview::new(target)?;
        preview.show_with_appearance(target, appearance)?;
        assert!(crate::sys::window_server::window_is_ordered_in(
            preview.window_id()
        ));
        let recorder = movie
            .map(|path| {
                std::process::Command::new("/usr/sbin/screencapture")
                    .args(["-x", "-v", "-V", "2", "-R100,100,900,320"])
                    .arg(path)
                    .spawn()
                    .map(Recording)
            })
            .transpose()?;
        if recorder.is_some() {
            std::thread::sleep(Duration::from_millis(300));
        }
        let mut trace = Vec::new();
        let mut destination = target;
        for step in 0..80 {
            if [5, 8, 12, 15, 30, 34].contains(&step) {
                let shifted = [5, 12, 30].contains(&step);
                destination = target;
                if shifted {
                    destination.preview_area.origin.x += 180.0;
                    destination.preview_area.size.width += 50.0;
                }
                preview.show_with_appearance(destination, appearance)?;
                let shape_motion = unsafe {
                    preview.shape.animationForKey(&NSString::from_str("preview.position"))
                }
                .unwrap();
                let began = preview.shape.convertTime_toLayer(shape_motion.beginTime(), None);
                for layer in
                    [&*preview.tint, &*preview.card].into_iter().chain(preview.backdrop.as_deref())
                {
                    for key in ["preview.position", "preview.bounds"] {
                        let animation =
                            unsafe { layer.animationForKey(&NSString::from_str(key)) }.unwrap();
                        assert_eq!(animation.duration(), shape_motion.duration());
                        assert!(
                            (layer.convertTime_toLayer(animation.beginTime(), None) - began).abs()
                                < 1e-9,
                            "preview layers must share one compositor clock"
                        );
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(8));
            let frame = preview.presented_frame();
            let carrier = preview.carrier;
            assert!(frame.origin.x >= carrier.origin.x - 0.125);
            assert!(frame.origin.y >= carrier.origin.y - 0.125);
            assert!(
                frame.origin.x + frame.size.width <= carrier.origin.x + carrier.size.width + 0.125
            );
            assert!(
                frame.origin.y + frame.size.height
                    <= carrier.origin.y + carrier.size.height + 0.125
            );
            trace.push(serde_json::json!({
                "time": CACurrentMediaTime(),
                "frame": [frame.origin.x, frame.origin.y, frame.size.width, frame.size.height],
                "carrier": [carrier.origin.x, carrier.origin.y, carrier.size.width, carrier.size.height],
            }));
        }
        if let Some(path) = movie {
            std::fs::write(path.with_extension("json"), serde_json::to_vec_pretty(&trace)?)?;
        }
        assert!(
            preview.presented_frame().same_as(preview_frame(destination)),
            "presented {:?} must settle at {:?}",
            preview.presented_frame(),
            preview_frame(destination)
        );
        let mut shifted = destination;
        shifted.preview_area.origin.x += 100.0;
        preview.show_with_appearance(shifted, appearance)?;
        preview.show_with_appearance(shifted, PreviewAppearance {
            reduce_motion: true,
            ..appearance
        })?;
        assert_eq!(preview.carrier, preview_frame(shifted));
        assert!(preview.motion.is_none());
        assert!(
            unsafe { preview.shape.animationForKey(&NSString::from_str("preview.position")) }
                .is_none()
        );
        if let Some(mut recorder) = recorder {
            assert!(recorder.0.wait()?.success(), "preview motion recording failed");
        }
        preview.finish();
        println!(
            "PASS: native preview reversals, compositor clock, carrier containment, and Reduce Motion settlement"
        );
        Ok(())
    }

    pub fn verify(
        capture: Option<&Path>,
        movie: Option<&Path>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let frame = CGRect::new(CGPoint::new(100.0, 100.0), CGSize::new(900.0, 320.0));
        let background = CgsWindow::new_compositor(frame, 12.0)?;
        let stage = CALayer::layer();
        with_disabled_actions(|| {
            stage.setFrame(DragPreview::bounds(frame.size));
            stage.setGeometryFlipped(true);
            stage.setBackgroundColor(Some(&CGColor::new_generic_rgb(0.96, 0.96, 0.97, 1.0)));
            let dark = CALayer::layer();
            dark.setFrame(CGRect::new(CGPoint::new(0.0, 160.0), CGSize::new(900.0, 160.0)));
            dark.setBackgroundColor(Some(&CGColor::new_generic_rgb(0.08, 0.09, 0.11, 1.0)));
            stage.addSublayer(&dark);
        });
        let surface = WindowSurface::new(background.id(), DragPreview::bounds(frame.size), &stage)?;
        surface.flush();
        background.set_alpha(1.0)?;
        background.order_above(None)?;
        let normal = PreviewAppearance {
            increase_contrast: false,
            reduce_transparency: false,
            reduce_motion: true,
        };
        let mut previews = Vec::new();
        for (index, (label, appearance)) in [
            ("Standard", normal),
            ("Increase Contrast", PreviewAppearance {
                increase_contrast: true,
                ..normal
            }),
            ("Reduce Transparency", PreviewAppearance {
                reduce_transparency: true,
                ..normal
            }),
        ]
        .into_iter()
        .enumerate()
        {
            let x = 20.0 + index as f64 * 300.0;
            let label_layer = objc2_quartz_core::CATextLayer::layer();
            with_disabled_actions(|| unsafe {
                label_layer.setFrame(CGRect::new(
                    CGPoint::new(x + 4.0, 12.0),
                    CGSize::new(280.0, 25.0),
                ));
                label_layer.setString(Some(&NSString::from_str(label)));
                label_layer.setFontSize(16.0);
                label_layer.setContentsScale(2.0);
                label_layer
                    .setForegroundColor(Some(&CGColor::new_generic_rgb(0.08, 0.09, 0.11, 1.0)));
                stage.addSublayer(&label_layer);
            });
            let area = CGRect::new(
                CGPoint::new(frame.origin.x + x, frame.origin.y + 45.0),
                CGSize::new(280.0, 255.0),
            );
            let target = DropTarget {
                intent: DropIntent {
                    window: crate::actor::app::WindowId::new(
                        std::process::id() as i32,
                        background.id(),
                    ),
                    space: SpaceId::new(1),
                    frame: area,
                    zone: DropZone::Center,
                    action: WindowDropAction::Stack,
                },
                preview_area: area,
            };
            let mut preview = DragPreview::new(target)?;
            preview.show_with_appearance(target, normal)?;
            let before = preview.shape.frame();
            preview.show_with_appearance(target, appearance)?;
            assert_eq!(
                preview.shape.frame(),
                before,
                "appearance changes must preserve target geometry"
            );
            assert_eq!(
                preview.appearance, appearance,
                "an unchanged target must refresh native preferences"
            );
            assert_eq!(preview.stage.opacity(), 1.0);
            assert_eq!(preview.card.opacity(), 1.0);
            if let Some(backdrop) = &preview.backdrop {
                assert_eq!(backdrop.isHidden(), appearance.reduce_transparency);
            }
            if appearance.reduce_transparency {
                assert_eq!(CGColor::alpha(preview.tint.backgroundColor().as_deref()), 1.0);
            }
            if appearance.increase_contrast {
                assert_eq!(preview.shape.borderWidth(), 3.0);
            }
            let moving = PreviewAppearance {
                reduce_motion: false,
                ..appearance
            };
            preview.show_with_appearance(target, moving)?;
            let mut moved = target;
            moved.preview_area.origin.x += 20.0;
            preview.show_with_appearance(moved, moving)?;
            assert!(unsafe {
                preview.shape.animationForKey(&NSString::from_str("preview.position")).is_some()
            });
            preview.show_with_appearance(moved, appearance)?;
            assert!(unsafe {
                preview.shape.animationForKey(&NSString::from_str("preview.position")).is_none()
            });
            assert!(unsafe {
                preview.shape.animationForKey(&NSString::from_str("preview.bounds")).is_none()
            });
            assert_eq!(preview.carrier, preview_frame(moved));
            preview.show_with_appearance(target, appearance)?;
            previews.push((preview, target));
            println!("PASS {label}: native appearance and same-target Reduce Motion settlement");
        }
        surface.flush();
        std::thread::sleep(Duration::from_millis(250));
        if let Some(path) = capture {
            let result = std::process::Command::new("/usr/sbin/screencapture")
                .args(["-x", "-R100,100,900,320"])
                .arg(path)
                .status()?;
            assert!(result.success(), "native compositor capture failed");
            println!("Captured {}", path.display());
        }
        let motion_target = previews[0].1;
        for (preview, target) in &mut previews {
            preview.hide();
            preview.show(*target)?;
            preview.finish();
        }
        drop(previews);
        verify_motion(motion_target, movie)?;
        drop(surface);
        drop(background);
        Ok(())
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mtm = objc2::MainThreadMarker::new().unwrap();
    let app = objc2_app_kit::NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(objc2_app_kit::NSApplicationActivationPolicy::Accessory);
    app.finishLaunching();
    let mut args = std::env::args().skip(1);
    let mut capture = None;
    let mut movie = None;
    while let Some(argument) = args.next() {
        let path = std::path::PathBuf::from(args.next().expect("capture option requires a path"));
        match argument.as_str() {
            "--capture" => capture = Some(path),
            "--motion-capture" => movie = Some(path),
            _ => panic!("usage: drag-preview-check [--capture PATH] [--motion-capture PATH]"),
        }
    }
    preview::verify(capture.as_deref(), movie.as_deref())
}
