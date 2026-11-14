// Modified by kdnk for wm on 2026-11-15. Original attribution is preserved in LICENSE.
pub use wm::{actor, common, layout_engine, model, sys};

mod preview {
    include!("../src/ui/drag_preview.rs");

    use std::path::Path;
    use std::time::Duration;

    use crate::actor::drag::{DropIntent, DropZone};
    use crate::sys::screen::SpaceId;

    pub fn verify(capture: Option<&Path>) -> Result<(), Box<dyn std::error::Error>> {
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
        for (preview, target) in &mut previews {
            preview.hide();
            preview.show(*target)?;
            preview.finish();
        }
        drop(previews);
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
    let capture = match args.next().as_deref() {
        None => None,
        Some("--capture") => Some(std::path::PathBuf::from(
            args.next().expect("--capture requires a path"),
        )),
        Some(_) => panic!("usage: drag-preview-check [--capture PATH]"),
    };
    assert!(args.next().is_none(), "unexpected argument");
    preview::verify(capture.as_deref())
}
