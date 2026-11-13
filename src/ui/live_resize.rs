use std::fs::{File, OpenOptions};
use std::io::Write;
use std::ptr::NonNull;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use block2::RcBlock;
use cgs::compositor::preview::{
    CaptureMode, CaptureResolution, PreviewCaptureSession, PreviewLayer, PreviewSource,
    PreviewSurfaceFrame, SourceStatus,
};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AnyThread, MainThreadOnly};
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSScreen, NSView, NSWindow, NSWindowAnimationBehavior,
    NSWindowOrderingMode, NSWindowStyleMask, NSWorkspace,
};
use objc2_core_foundation::{CFRetained, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGDisplayBounds, CGImage, CGMainDisplayID, CGPreflightScreenCaptureAccess,
};
use objc2_foundation::{MainThreadMarker, NSArray, NSError, NSString};
use objc2_quartz_core::CALayer;
use objc2_screen_capture_kit::{
    SCContentFilter, SCScreenshotManager, SCShareableContent, SCStreamConfiguration,
};
use serde_json::{Value, json};

use crate::actor;
use crate::actor::app::WindowId;
use crate::common::collections::HashMap;
use crate::model::live_resize::{Client, Event, Phase, Plan, Scene, Ticket, matches_pixels};
use crate::sys::geometry::SameAs;
use crate::sys::screen::{NSScreenExt, ScreenId};
use crate::sys::skylight::SLSWindowTags;
use crate::sys::window_server::{
    WindowIterator, WindowServerId, get_window, window_ids_above, window_parent,
};

const PREPARE_TIMEOUT: Duration = Duration::from_secs(2);
const RENDER_TIMEOUT: Duration = Duration::from_secs(2);
const BLEND: Duration = Duration::from_millis(80);

pub struct ResizeActor {
    mtm: MainThreadMarker,
    tx: actor::Sender<Event>,
    rx: actor::Receiver<Event>,
    client: Client,
    scenes: HashMap<u32, DisplayScene>,
    trace: Option<File>,
}

struct Member {
    ticket: Ticket,
    captures: PreviewCaptureSession,
    container: Retained<CALayer>,
    old: PreviewLayer,
    old_size: CGSize,
    target_size: CGSize,
    previous: Vec<(PreviewLayer, CGSize)>,
    target: PreviewLayer,
    inspection: PreviewLayer,
    live_updates: u64,
    target_ready: bool,
    accepted_logged: bool,
}

impl Drop for Member {
    fn drop(&mut self) { self.container.removeFromSuperlayer(); }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Preparing,
    AwaitingTargets,
    Presenting,
}

struct DisplayScene {
    ticket: Scene,
    members: HashMap<WindowId, Member>,
    overlay: Option<Overlay>,
    backdrop: Option<CFRetained<CGImage>>,
    needs_backdrop: bool,
    display_frame: CGRect,
    scale: f64,
    deadline: Instant,
    stage: Stage,
    pending: Option<Scene>,
    foreground: Option<i32>,
    stack: StackPlan,
    next_order_check: Instant,
}

struct RetainedScene {
    members: HashMap<WindowId, Member>,
    overlay: Option<Overlay>,
    display_frame: CGRect,
    scale: f64,
}

struct StackPlan {
    anchor: u32,
    back_to_front: Vec<WindowId>,
    excluded: Vec<WindowServerId>,
}
struct Overlay {
    window: Option<Retained<NSWindow>>,
    backdrop: Retained<CALayer>,
    root: Retained<CALayer>,
    union: CGRect,
}

impl Drop for Overlay {
    fn drop(&mut self) {
        if let Some(window) = self.window.take() {
            let _tx = cgs::compositor::LayerTransaction::disabled().flush_on_commit();
            window.orderOut(None);
            window.setContentView(None);
            window.close();
        }
    }
}

impl ResizeActor {
    pub fn new(mtm: MainThreadMarker) -> (Self, Client) {
        let (tx, rx) = actor::channel();
        let client = Client::new(tx.clone());
        let trace = std::env::var_os("RIFT_LIVE_RESIZE_TRACE")
            .and_then(|path| OpenOptions::new().append(true).create(true).open(path).ok());
        let mut actor = Self {
            mtm,
            tx,
            rx,
            client: client.clone(),
            scenes: HashMap::default(),
            trace,
        };
        actor.log(
            None,
            "permission_available",
            json!({"available":CGPreflightScreenCaptureAccess()}),
        );
        (actor, client)
    }

    pub async fn run(mut self) {
        loop {
            let Some((_, event)) = next_event(&mut self.rx, !self.scenes.is_empty()).await else {
                break;
            };
            let mut pending_events = vec![event];
            while let Ok((_, queued)) = self.rx.try_recv() {
                pending_events.push(queued);
            }
            for event in pending_events {
                match event {
                    Event::Prepare(ticket) => self.prepare(ticket),
                    Event::Cancel(wid, generation) => {
                        let display = self.scenes.iter().find_map(|(display, scene)| {
                            scene
                                .members
                                .get(&wid)
                                .filter(|m| m.ticket.generation() == generation)
                                .map(|_| *display)
                        });
                        if let Some(display) = display
                            && let Some(mut scene) = self.scenes.remove(&display)
                        {
                            self.abort(&mut scene, "cancelled");
                        }
                    }
                    Event::Backdrop(ticket, image) => {
                        if let Some(scene) = self.scenes.get_mut(&ticket.display())
                            && scene.ticket.id() == ticket.id()
                        {
                            match image {
                                Ok(image) => scene.backdrop = Some(image),
                                Err(_) => {
                                    for member in &scene.ticket.members {
                                        member.fallback();
                                    }
                                }
                            }
                        }
                    }
                    Event::Retire(ticket) => self.client.retire(&ticket),
                    Event::Wake => {}
                }
            }
            let displays: Vec<_> = self.scenes.keys().copied().collect();
            for display in displays {
                let Some(mut scene) = self.scenes.remove(&display) else {
                    continue;
                };
                if self.pump(&mut scene) {
                    self.scenes.insert(display, scene);
                }
            }
        }
        for (_, scene) in self.scenes.drain() {
            for ticket in scene.ticket.members {
                ticket.revoke();
            }
        }
    }

    fn log(&mut self, ticket: Option<&Ticket>, phase: &str, details: Value) {
        let mut item = json!({"phase":phase,"timestamp_us":wall_time_us()});
        if let Some(ticket) = ticket {
            item["window_id"] = json!(ticket.plan().window_id);
            item["generation"] = json!(ticket.generation());
            item["cohort_id"] = json!(ticket.scene_id());
            item["from"] = json!(components(ticket.plan().from));
            item["to"] = json!(components(ticket.plan().to));
        }
        if let Some(object) = details.as_object() {
            for (key, value) in object {
                item[key] = value.clone();
            }
        }
        tracing::debug!(target:"rift::live_resize", event = %item);
        if let Some(file) = &mut self.trace {
            let _ = writeln!(file, "{item}");
        }
    }

    fn fail(&mut self, scene: &Scene, reason: &str) {
        for ticket in &scene.members {
            self.log(Some(ticket), "fallback", json!({"reason":reason,"capture_available":CGPreflightScreenCaptureAccess(),"frontmost_pid":foreground_pid()}));
            ticket.fallback();
        }
    }

    fn prepare(&mut self, ticket: Scene) {
        if ticket.members.iter().any(|t| !t.is_current()) {
            return;
        }
        if !CGPreflightScreenCaptureAccess() {
            self.fail(&ticket, "screen_capture_permission");
            return;
        }
        if NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion() {
            self.fail(&ticket, "reduce_motion");
            return;
        }
        if let Some(mut old) = self.scenes.remove(&ticket.display()) {
            if old.ticket.id() == ticket.id() {
                self.scenes.insert(ticket.display(), old);
                return;
            }
            if old.ticket.started().is_some_and(|started| started.elapsed() < BLEND) {
                old.pending = Some(ticket);
                self.scenes.insert(old.ticket.display(), old);
                return;
            }
            if self.retarget(&mut old, ticket) {
                self.scenes.insert(old.ticket.display(), old);
            }
            return;
        }
        match self.make_scene(ticket.clone(), None) {
            Ok(scene) => {
                self.scenes.insert(ticket.display(), scene);
            }
            Err(reason) => self.fail(&ticket, reason),
        }
    }

    fn make_scene(
        &mut self,
        ticket: Scene,
        previous: Option<RetainedScene>,
    ) -> Result<DisplayScene, &'static str> {
        let display = ticket.display();
        let display_frame = CGDisplayBounds(display);
        if ticket.members.iter().any(|t| {
            !fits_display(display, display_frame, t.plan()) || !normal_source(t.plan().window_id)
        }) {
            return Err("display_or_source");
        }
        let screen = NSScreen::screens(self.mtm)
            .iter()
            .find(|s| s.get_number() == Ok(ScreenId::new(display)))
            .ok_or("display")?;
        let scale = screen.backingScaleFactor();
        let stack = scene_stack(&ticket).ok_or("interleaved_window")?;
        let foreground = foreground_pid();
        let mut previous = previous;
        if previous
            .as_ref()
            .is_some_and(|s| s.display_frame != display_frame || s.scale != scale)
        {
            return Err("display_changed");
        }
        let mut members = HashMap::default();
        let needs_backdrop = ticket.members.iter().any(|t| !contained(t.plan().from, t.plan().to));
        for member_ticket in &ticket.members {
            let wid = member_ticket.plan().wid;
            let mut member = if let Some(member) =
                previous.as_mut().and_then(|s| s.members.remove(&wid)).filter(|old| {
                    old.ticket.plan().window_id == member_ticket.plan().window_id
                        && old.ticket.plan().handle.same_actor(&member_ticket.plan().handle)
                }) {
                member
            } else {
                let wake = self.tx.clone();
                let mut captures = PreviewCaptureSession::new(move || wake.send(Event::Wake))
                    .map_err(|_| "capture_session")?;
                captures
                    .set_sources(&[capture_source(member_ticket, scale)])
                    .map_err(|_| "capture_source")?;
                Member {
                    ticket: member_ticket.clone(),
                    captures,
                    container: CALayer::layer(),
                    old: PreviewLayer::new(),
                    old_size: member_ticket.native().size,
                    target_size: member_ticket.plan().to.size,
                    previous: Vec::new(),
                    target: PreviewLayer::new(),
                    inspection: PreviewLayer::new(),
                    live_updates: 0,
                    target_ready: false,
                    accepted_logged: false,
                }
            };
            if member.ticket.generation() != member_ticket.generation() {
                if member.target.has_frame() {
                    let visible = member_ticket.plan().from.size;
                    if member.target_size.width >= visible.width
                        && member.target_size.height >= visible.height
                    {
                        std::mem::swap(&mut member.old, &mut member.target);
                        member.old_size = member.target_size;
                        member.previous.clear();
                    } else {
                        if member.previous.len() >= 4 {
                            return Err("retarget_surface_budget");
                        }
                        let layer = std::mem::replace(&mut member.target, PreviewLayer::new());
                        layer.layer().setOpacity(1.);
                        member.previous.push((layer, member.target_size));
                    }
                }
                member.target.clear();
            }
            member.ticket = member_ticket.clone();
            member.target_size = member_ticket.plan().to.size;
            member.target_ready = false;
            member.live_updates = 0;
            member.accepted_logged = false;
            member.target.layer().setOpacity(0.);
            self.log(Some(member_ticket), "preparing", json!({"scale":scale,"participants":ticket.members.len(),"retarget":previous.is_some(),"capture_resolution":format!("{:?}",capture_source(member_ticket,scale).resolution)}));
            members.insert(wid, member);
        }
        let overlay = previous.as_mut().and_then(|s| s.overlay.take());
        let scene = DisplayScene {
            ticket: ticket.clone(),
            members,
            overlay,
            backdrop: None,
            needs_backdrop,
            display_frame,
            scale,
            deadline: Instant::now() + PREPARE_TIMEOUT,
            stage: Stage::Preparing,
            pending: None,
            foreground,
            stack,
            next_order_check: Instant::now() + Duration::from_millis(100),
        };
        if needs_backdrop {
            capture_backdrop(
                ticket,
                self.tx.clone(),
                scale,
                scene.stack.anchor,
                scene.stack.excluded.clone(),
            );
        }
        Ok(scene)
    }

    fn retarget(&mut self, scene: &mut DisplayScene, ticket: Scene) -> bool {
        let old = RetainedScene {
            members: std::mem::take(&mut scene.members),
            overlay: scene.overlay.take(),
            display_frame: scene.display_frame,
            scale: scene.scale,
        };
        let _tx = cgs::compositor::LayerTransaction::disabled().flush_on_commit();
        match self.make_scene(ticket.clone(), Some(old)) {
            Ok(next) => {
                *scene = next;
                true
            }
            Err(reason) => {
                self.fail(&ticket, reason);
                false
            }
        }
    }

    fn pump(&mut self, scene: &mut DisplayScene) -> bool {
        let now = Instant::now();
        if !CGPreflightScreenCaptureAccess()
            || NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion()
            || foreground_pid() != scene.foreground
            || scene
                .members
                .values()
                .any(|member| !member.ticket.plan().handle.can_write_frames())
        {
            self.abort(scene, "source_or_session_changed");
            return false;
        }
        if let Some(pending) = scene.pending.take() {
            if scene
                .ticket
                .started()
                .is_none_or(|started| now.saturating_duration_since(started) >= BLEND)
            {
                if !self.retarget(scene, pending) {
                    return false;
                }
            } else {
                scene.pending = Some(pending);
                self.draw(scene, now);
                return true;
            }
        }
        if scene.ticket.members.iter().any(|ticket| !ticket.is_current()) {
            self.abort(scene, "revoked");
            return false;
        }
        if scene.members.values().any(|m| {
            !crate::sys::window_server::window_is_ordered_in(WindowServerId::new(
                m.ticket.plan().window_id,
            ))
        }) {
            self.abort(scene, "source_or_session_changed");
            return false;
        }
        if now >= scene.next_order_check {
            scene.next_order_check = now + Duration::from_millis(100);
            let Some(stack) = scene_stack(&scene.ticket) else {
                self.abort(scene, "interleaved_window");
                return false;
            };
            scene.stack = stack;
            if let Some(window) = scene.overlay.as_ref().and_then(|o| o.window.as_ref()) {
                order_above_source(window, scene.stack.anchor);
            }
        }
        if now >= scene.deadline && !matches!(scene.stage, Stage::Presenting) {
            self.abort(scene, "capture_timeout");
            return false;
        }
        for member in scene.members.values_mut() {
            let phase = member.ticket.state().phase;
            if matches!(phase, Phase::Fallback | Phase::Retired) {
                self.abort(scene, "member_retired");
                return false;
            }
            let accepted = member.ticket.state().accepted;
            if let Some(accepted) = accepted {
                if !accepted.same_as(member.ticket.plan().to) {
                    self.abort(scene, "native_geometry_changed");
                    return false;
                }
                if !member.accepted_logged {
                    self.log(
                        Some(&member.ticket),
                        "accepted",
                        json!({"accepted":components(accepted),"capture_resolution":format!("{:?}",capture_source(&member.ticket,scene.scale).resolution)}),
                    );
                    member.accepted_logged = true;
                }
            }
            let source = capture_source(&member.ticket, scene.scale);
            let pump_error = member
                .captures
                .update_sources(&[source])
                .and_then(|_| member.captures.pump())
                .err()
                .map(|error| error.to_string());
            let status = member.captures.source_status(member.ticket.plan().window_id);
            if pump_error.is_some() || matches!(status, Some(SourceStatus::Failed(_))) {
                let stats = member.captures.stats();
                self.log(
                    Some(&member.ticket),
                    "capture_error",
                    json!({
                        "pump_error":pump_error,
                        "source_status":format!("{status:?}"),
                        "process_capture_jobs":stats.process_capture_jobs,
                        "process_streams":stats.process_streams,
                        "streams":stats.streams,
                        "reserved_capture_bytes":stats.reserved_capture_bytes,
                        "latest_frame_bytes":stats.latest_frame_bytes,
                        "captured_frames":stats.captured_frames,
                        "capture_resolution":format!("{:?}",source.resolution),
                    }),
                );
                self.abort(scene, "capture_failed");
                return false;
            }
            if let Some(frame) = member.captures.captured_frame(member.ticket.plan().window_id) {
                member.inspection.set_surface_frame(&frame);
                let pixels = surface_dimensions(&member.inspection, &frame);
                match scene.stage {
                    Stage::Preparing
                        if member.ticket.state().inspected
                            && !member.old.has_frame()
                            && matches_pixels(pixels, member.ticket.native(), scene.scale) =>
                    {
                        set_capture_frame(&mut member.old, &frame);
                        member.old_size = member.ticket.native().size;
                    }
                    Stage::AwaitingTargets | Stage::Presenting => {
                        if member.ticket.state().accepted.is_some_and(|accepted| {
                            target_pixels_ready(
                                pixels,
                                frame.native_pixel_size(),
                                accepted,
                                scene.scale,
                            )
                        }) && set_capture_frame(&mut member.target, &frame)
                        {
                            member.live_updates += 1;
                            member.target_ready = true;
                        }
                    }
                    _ => {}
                }
            }
        }
        match scene.stage {
            Stage::Preparing
                if ready_action(
                    scene.stage,
                    old_images_ready(scene),
                    target_images_ready(scene),
                ) == Some(ReadyAction::Commit) =>
            {
                if self.show(scene, true).is_err() {
                    self.abort(scene, "overlay");
                    return false;
                }
                let overlay_id = scene
                    .overlay
                    .as_ref()
                    .and_then(|o| o.window.as_ref())
                    .map(|w| w.windowNumber());
                for member in scene.members.values() {
                    self.log(
                        Some(&member.ticket),
                        "commit_requested",
                        json!({"overlay_window_id":overlay_id}),
                    );
                }
                if !scene.ticket.commit_native() {
                    self.abort(scene, "native_dispatch");
                    return false;
                }
                scene.stage = Stage::AwaitingTargets;
                scene.deadline = now + RENDER_TIMEOUT;
            }
            Stage::AwaitingTargets
                if ready_action(
                    scene.stage,
                    old_images_ready(scene),
                    target_images_ready(scene),
                ) == Some(ReadyAction::Start) =>
            {
                if self.show(scene, false).is_err() {
                    self.abort(scene, "overlay");
                    return false;
                }
                let started = Instant::now();
                let started_us = wall_time_us();
                for member in scene.members.values() {
                    self.log(Some(&member.ticket), "target_ready", json!({"accepted":components(member.ticket.native()),"live_updates":member.live_updates}));
                    self.log(Some(&member.ticket), "animating", json!({"accepted":components(member.ticket.native()),"live_updates":member.live_updates,"presentation_started_us":started_us}));
                }
                scene.ticket.start(started);
                scene.stage = Stage::Presenting;
            }
            Stage::Presenting => {
                let visuals = scene.ticket.visuals();
                if scene.members.values().all(|m| {
                    m.ticket.state().phase == Phase::Finishing
                        && visuals.frames.get(&m.ticket.plan().wid).is_some_and(
                            |(generation, frame)| {
                                *generation == m.ticket.generation()
                                    && frame.same_as(m.ticket.native())
                            },
                        )
                }) {
                    scene.overlay.take();
                    for member in scene.members.values() {
                        member.ticket.state().phase = Phase::Retired;
                        self.log(Some(&member.ticket), "finished", json!({"accepted":components(member.ticket.native()),"live_updates":member.live_updates}));
                        self.client.retire(&member.ticket);
                    }
                    return false;
                }
            }
            _ => {}
        }
        self.draw(scene, now);
        true
    }

    fn abort(&mut self, scene: &mut DisplayScene, reason: &str) {
        scene.overlay.take();
        for member in scene.members.values() {
            member.ticket.fallback();
            self.log(Some(&member.ticket), "fallback", json!({"reason":reason,"accepted":components(member.ticket.native()),"live_updates":member.live_updates,"frontmost_pid":foreground_pid(),"active_generation":member.ticket.active_generation(),"authority_current":member.ticket.plan().handle.can_write_frames()}));
        }
        if let Some(pending) = scene.pending.take() {
            self.fail(&pending, reason);
        }
    }

    fn draw(&self, scene: &DisplayScene, now: Instant) {
        let Some(overlay) = &scene.overlay else { return };
        let visuals = scene.ticket.visuals();
        let blend = scene.ticket.started().map_or(0., |started| {
            (now.saturating_duration_since(started).as_secs_f64() / BLEND.as_secs_f64())
                .clamp(0., 1.)
        }) as f32;
        let _tx = cgs::compositor::LayerTransaction::disabled().flush_on_commit();
        for member in scene.members.values() {
            let Some((generation, frame)) = visuals.frames.get(&member.ticket.plan().wid) else {
                continue;
            };
            if *generation != member.ticket.generation() {
                continue;
            }
            member.container.setFrame(local(*frame, overlay.union));
            native_layer(member.old.layer(), member.old_size, frame.size.height);
            for (layer, size) in &member.previous {
                native_layer(layer.layer(), *size, frame.size.height);
            }
            native_layer(member.target.layer(), member.target_size, frame.size.height);
            member.old.layer().setOpacity(1.);
            member.target.layer().setOpacity(blend);
        }
    }

    fn show(&self, scene: &mut DisplayScene, native_pending: bool) -> Result<(), &'static str> {
        let _tx = cgs::compositor::LayerTransaction::disabled().flush_on_commit();
        let region = scene
            .ticket
            .members
            .iter()
            .map(|t| cover_region(t.plan().from, t.plan().to, t.native(), native_pending))
            .reduce(union)
            .ok_or("empty scene")?;
        let primary = CGDisplayBounds(CGMainDisplayID());
        let screen_rect = CGRect::new(
            CGPoint::new(
                region.origin.x,
                primary.size.height - region.origin.y - region.size.height,
            ),
            region.size,
        );
        let view =
            NSView::initWithFrame(NSView::alloc(self.mtm), CGRect::new(CGPoint::ZERO, region.size));
        view.setWantsLayer(true);
        let mut existing = scene.overlay.take();
        let root = existing
            .as_ref()
            .map(|o| o.root.clone())
            .or_else(|| view.layer())
            .ok_or("missing layer")?;
        root.setFrame(CGRect::new(CGPoint::ZERO, region.size));
        if let Some(old) = &existing {
            old.backdrop.removeFromSuperlayer();
        }
        let backdrop = CALayer::layer();
        backdrop.setAnchorPoint(CGPoint::ZERO);
        backdrop.setFrame(CGRect::new(CGPoint::ZERO, region.size));
        if let Some(image) = &scene.backdrop {
            for ticket in &scene.ticket.members {
                let footprint = if native_pending {
                    union(ticket.plan().to, ticket.native())
                } else {
                    ticket.native()
                };
                let clip = CALayer::layer();
                clip.setAnchorPoint(CGPoint::ZERO);
                clip.setMasksToBounds(true);
                clip.setCornerRadius(12.);
                clip.setFrame(local(footprint, region));
                let image_layer = CALayer::layer();
                image_layer.setAnchorPoint(CGPoint::ZERO);
                unsafe {
                    image_layer
                        .setContents(Some(&*((&**image as *const CGImage).cast::<AnyObject>())));
                }
                let target = local(footprint, region);
                image_layer.setFrame(CGRect::new(
                    CGPoint::new(
                        scene.display_frame.origin.x - region.origin.x - target.origin.x,
                        region.origin.y + region.size.height
                            - scene.display_frame.origin.y
                            - scene.display_frame.size.height
                            - target.origin.y,
                    ),
                    scene.display_frame.size,
                ));
                clip.addSublayer(&image_layer);
                backdrop.addSublayer(&clip);
            }
        }
        root.addSublayer(&backdrop);
        for wid in &scene.stack.back_to_front {
            let member = &scene.members[wid];
            member.container.setAnchorPoint(CGPoint::ZERO);
            member.container.setMasksToBounds(true);
            member.container.setCornerRadius(12.);
            member.container.setBackgroundColor(Some(
                &NSColor::colorWithSRGBRed_green_blue_alpha(17. / 255., 17. / 255., 17. / 255., 1.)
                    .CGColor(),
            ));
            member.container.setFrame(local(member.ticket.plan().from, region));
            member.old.layer().setOpacity(1.);
            member.target.layer().setOpacity(0.);
            native_layer(
                member.old.layer(),
                member.old_size,
                member.ticket.plan().from.size.height,
            );
            native_layer(
                member.target.layer(),
                member.target_size,
                member.ticket.plan().from.size.height,
            );
            member.container.addSublayer(member.old.layer());
            for (layer, size) in &member.previous {
                native_layer(layer.layer(), *size, member.ticket.plan().from.size.height);
                member.container.addSublayer(layer.layer());
            }
            member.container.addSublayer(member.target.layer());
            root.addSublayer(&member.container);
        }
        let window = if let Some(mut old) = existing.take() {
            let window = old.window.take().unwrap();
            window.setFrame_display(screen_rect, true);
            if let Some(content) = window.contentView() {
                content.setFrameSize(region.size);
            }
            window
        } else {
            let window = unsafe {
                NSWindow::initWithContentRect_styleMask_backing_defer(
                    NSWindow::alloc(self.mtm),
                    screen_rect,
                    NSWindowStyleMask::Borderless,
                    NSBackingStoreType::Buffered,
                    false,
                )
            };
            window.setContentView(Some(&view));
            window
        };
        unsafe {
            window.setReleasedWhenClosed(false);
        }
        window.setTitle(&NSString::from_str("Rift live resize"));
        window.setAnimationBehavior(NSWindowAnimationBehavior::None);
        window.setLevel(0);
        window.setIgnoresMouseEvents(false);
        window.setHasShadow(false);
        window.setOpaque(false);
        window.setBackgroundColor(Some(&NSColor::clearColor()));
        order_above_source(&window, scene.stack.anchor);
        scene.overlay = Some(Overlay {
            window: Some(window),
            backdrop,
            root,
            union: region,
        });
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ReadyAction {
    Commit,
    Start,
}
fn ready_action(stage: Stage, all_old_ready: bool, all_targets_ready: bool) -> Option<ReadyAction> {
    match stage {
        Stage::Preparing if all_old_ready => Some(ReadyAction::Commit),
        Stage::AwaitingTargets if all_targets_ready => Some(ReadyAction::Start),
        _ => None,
    }
}

fn foreground_pid() -> Option<i32> {
    NSWorkspace::sharedWorkspace()
        .frontmostApplication()
        .map(|app| app.processIdentifier())
}
fn wall_time_us() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_micros()
}
fn old_images_ready(scene: &DisplayScene) -> bool {
    (!scene.needs_backdrop || scene.backdrop.is_some())
        && scene.members.values().all(|m| m.ticket.state().inspected && m.old.has_frame())
}
fn target_images_ready(scene: &DisplayScene) -> bool {
    scene
        .members
        .values()
        .all(|m| m.ticket.state().accepted.is_some() && m.target_ready)
}

fn passive_decoration(tags: SLSWindowTags) -> bool {
    tags.contains(SLSWindowTags::WINDOW_MANAGER_AWARE | SLSWindowTags::IGNORE_FOR_EVENTS)
        && !tags.intersects(
            SLSWindowTags::DOCUMENT
                | SLSWindowTags::MODAL
                | SLSWindowTags::KIT_VISIBLE
                | SLSWindowTags::OPAQUE_FOR_EVENTS
                | SLSWindowTags::USER_INPUT_ACCESSORY,
        )
}

fn scene_stack(scene: &Scene) -> Option<StackPlan> {
    let sources: Vec<_> =
        scene.members.iter().map(|t| WindowServerId::new(t.plan().window_id)).collect();
    let mut lists = Vec::new();
    for id in &sources {
        lists.push((*id, window_ids_above(*id)?));
    }
    let (_, mut ordered) = lists.into_iter().max_by_key(|(_, above)| above.len())?;
    let lowest = sources.iter().find(|id| !ordered.contains(id)).copied()?;
    ordered.push(lowest);
    let first = ordered.iter().position(|id| sources.contains(id))?;
    let anchor = ordered[first];
    let last = ordered.iter().rposition(|id| sources.contains(id))?;
    let swept = scene.members.iter().map(|t| union(t.plan().from, t.plan().to)).reduce(union)?;
    let query = WindowIterator::new(&ordered)?;
    let mut facts = HashMap::default();
    while query.advance().is_some() {
        facts.insert(
            query.window_id(),
            (
                query.pid(),
                query.level(),
                query.bounds(),
                SLSWindowTags::from_bits_retain(query.tags()),
                query.alpha(),
            ),
        );
    }
    let mut decorations = Vec::new();
    for id in &ordered {
        if facts
            .get(&id.as_u32())
            .is_some_and(|(_, _, _, tags, alpha)| passive_decoration(*tags) || *alpha <= 0.)
        {
            decorations.push(*id);
        }
    }
    for id in &ordered[first..=last] {
        if sources.contains(id) || decorations.contains(id) {
            continue;
        }
        let (pid, level, bounds, _, _) = facts.get(&id.as_u32())?;
        if *pid == std::process::id() as i32 || *level != 0 || !intersects(*bounds, swept) {
            continue;
        }
        if window_parent(*id).is_some_and(|parent| sources.contains(&parent)) {
            continue;
        }
        return None;
    }
    let back_to_front = ordered[first..=last]
        .iter()
        .rev()
        .filter_map(|id| {
            scene
                .members
                .iter()
                .find(|t| t.plan().window_id == id.as_u32())
                .map(|t| t.plan().wid)
        })
        .collect::<Vec<_>>();
    if back_to_front.len() != sources.len() {
        return None;
    }
    let mut excluded = ordered[..first].to_vec();
    excluded.extend(sources);
    excluded.extend(decorations);
    Some(StackPlan {
        anchor: anchor.as_u32(),
        back_to_front,
        excluded,
    })
}
fn intersects(a: CGRect, b: CGRect) -> bool {
    a.origin.x < b.origin.x + b.size.width
        && b.origin.x < a.origin.x + a.size.width
        && a.origin.y < b.origin.y + b.size.height
        && b.origin.y < a.origin.y + a.size.height
}
async fn next_event(
    rx: &mut actor::Receiver<Event>,
    active: bool,
) -> Option<(tracing::Span, Event)> {
    if !active {
        return rx.recv().await;
    }
    tokio::select! {
        event = rx.recv() => event,
        _ = crate::sys::timer::Timer::sleep(Duration::from_millis(8)) => Some((tracing::Span::none(), Event::Wake)),
    }
}

fn native_layer(layer: &CALayer, size: CGSize, container_height: f64) {
    layer.setAnchorPoint(CGPoint::ZERO);
    layer.setFrame(CGRect::new(
        CGPoint::new(0., container_height - size.height),
        size,
    ));
}
fn components(r: CGRect) -> [f64; 4] { [r.origin.x, r.origin.y, r.size.width, r.size.height] }
fn fits_display(display: u32, bounds: CGRect, plan: &Plan) -> bool {
    display == plan.display && contained(bounds, union(plan.from, plan.to))
}

fn contained(outer: CGRect, inner: CGRect) -> bool {
    inner.origin.x >= outer.origin.x - 0.5
        && inner.origin.y >= outer.origin.y - 0.5
        && inner.origin.x + inner.size.width <= outer.origin.x + outer.size.width + 0.5
        && inner.origin.y + inner.size.height <= outer.origin.y + outer.size.height + 0.5
}
fn union(a: CGRect, b: CGRect) -> CGRect {
    let x = a.origin.x.min(b.origin.x);
    let y = a.origin.y.min(b.origin.y);
    CGRect::new(
        CGPoint::new(x, y),
        CGSize::new(
            (a.origin.x + a.size.width).max(b.origin.x + b.size.width) - x,
            (a.origin.y + a.size.height).max(b.origin.y + b.size.height) - y,
        ),
    )
}
fn cover_region(from: CGRect, to: CGRect, native: CGRect, native_pending: bool) -> CGRect {
    let visual = union(from, to);
    if native_pending {
        union(visual, native)
    } else {
        visual
    }
}
fn local(r: CGRect, union: CGRect) -> CGRect {
    CGRect::new(
        CGPoint::new(
            r.origin.x - union.origin.x,
            union.origin.y + union.size.height - r.origin.y - r.size.height,
        ),
        r.size,
    )
}
fn normal_source(wid: u32) -> bool {
    let id = WindowServerId::new(wid);
    get_window(id).is_some_and(|window| window.layer == 0)
        && crate::sys::window_server::window_is_ordered_in(id)
}

fn order_above_source(window: &NSWindow, source: u32) {
    window.orderWindow_relativeTo(NSWindowOrderingMode::Above, source as isize);
}

fn exclude_backdrop_window(
    id: u32,
    owner: Option<i32>,
    source: u32,
    above: &[WindowServerId],
) -> bool {
    id == source
        || above.iter().any(|window| window.as_u32() == id)
        || owner == Some(std::process::id() as i32)
}

#[link(name = "IOSurface", kind = "framework")]
unsafe extern "C" {
    fn IOSurfaceGetWidth(surface: *const std::ffi::c_void) -> usize;
    fn IOSurfaceGetHeight(surface: *const std::ffi::c_void) -> usize;
}
fn set_capture_frame(layer: &mut PreviewLayer, frame: &PreviewSurfaceFrame) -> bool {
    let _tx = cgs::compositor::LayerTransaction::disabled();
    if !layer.set_surface_frame(frame) {
        return false;
    }
    layer
        .layer()
        .setContentsRect(capture_contents_rect(frame.texture_coordinates()));
    true
}
fn capture_source(ticket: &Ticket, scale: f64) -> PreviewSource {
    let size = ticket.native().size;
    PreviewSource {
        window_id: ticket.plan().window_id,
        process_id: ticket.plan().wid.pid,
        mode: CaptureMode::Live,
        resolution: capture_resolution(size, scale),
        priority: 1,
        frame_interval: Duration::from_secs_f64(1. / 60.),
    }
}
fn capture_resolution(size: CGSize, scale: f64) -> CaptureResolution {
    CaptureResolution::Exact {
        width: (size.width * scale).ceil() as usize,
        height: (size.height * scale).ceil() as usize,
    }
}
fn target_pixels_ready(
    pixels: [f64; 2],
    native_pixels: Option<(usize, usize)>,
    accepted: CGRect,
    scale: f64,
) -> bool {
    matches_pixels(pixels, accepted, scale)
        && native_pixels.is_some_and(|(width, height)| {
            matches_pixels([width as f64, height as f64], accepted, scale)
        })
}
fn capture_contents_rect([x, y, width, height]: [f32; 4]) -> CGRect {
    // Capture UVs start at the top-left; Core Animation image crops start at the bottom-left.
    CGRect::new(
        CGPoint::new(x as f64, (1. - y - height) as f64),
        CGSize::new(width as f64, height as f64),
    )
}
fn surface_dimensions(layer: &PreviewLayer, frame: &PreviewSurfaceFrame) -> [f64; 2] {
    let Some(contents) = (unsafe { layer.layer().contents() }) else {
        return [0., 0.];
    };
    let pointer = Retained::as_ptr(&contents).cast::<std::ffi::c_void>();
    let uv = frame.texture_coordinates();
    [
        unsafe { IOSurfaceGetWidth(pointer) } as f64 * uv[2] as f64,
        unsafe { IOSurfaceGetHeight(pointer) } as f64 * uv[3] as f64,
    ]
}

fn capture_backdrop(
    ticket: Scene,
    tx: actor::Sender<Event>,
    scale: f64,
    anchor: u32,
    excluded_ids: Vec<WindowServerId>,
) {
    let callback = RcBlock::new(move |content: *mut SCShareableContent, error: *mut NSError| {
        if ticket.members.iter().any(|t| !t.is_current()) {
            return;
        }
        let Some(content) = (unsafe { content.as_ref() }) else {
            tx.send(Event::Backdrop(
                ticket.clone(),
                Err(error_text(error, "shareable content unavailable")),
            ));
            return;
        };
        let displays = unsafe { content.displays() };
        let Some(display) = displays.iter().find(|d| unsafe { d.displayID() } == ticket.display())
        else {
            tx.send(Event::Backdrop(
                ticket.clone(),
                Err("display unavailable".into()),
            ));
            return;
        };
        let windows = unsafe { content.windows() };
        let excluded: Vec<_> = windows
            .iter()
            .filter(|w| {
                let id = unsafe { w.windowID() };
                let owner = unsafe { w.owningApplication() }.map(|app| unsafe { app.processID() });
                exclude_backdrop_window(id, owner, anchor, &excluded_ids)
            })
            .collect();
        let excluded = NSArray::from_retained_slice(&excluded);
        let filter = unsafe {
            SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                &display,
                &excluded,
            )
        };
        let config = unsafe { SCStreamConfiguration::new() };
        let size = unsafe { display.frame() }.size;
        unsafe {
            config.setWidth((size.width * scale).ceil() as usize);
            config.setHeight((size.height * scale).ceil() as usize);
            config.setShowsCursor(false);
        }
        let ticket = ticket.clone();
        let tx = tx.clone();
        let result = RcBlock::new(move |image: *mut CGImage, error: *mut NSError| {
            let image = NonNull::new(image)
                .map(|image| unsafe { CFRetained::retain(image) })
                .ok_or_else(|| error_text(error, "backdrop unavailable"));
            tx.send(Event::Backdrop(ticket.clone(), image));
        });
        unsafe {
            SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(
                &filter,
                &config,
                Some(&result),
            );
        }
    });
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(false,true,&callback);
    }
}
fn error_text(error: *mut NSError, fallback: &str) -> String {
    unsafe { error.as_ref() }
        .map(ToString::to_string)
        .unwrap_or_else(|| fallback.into())
}

#[cfg(test)]
mod live_resize_tests {
    use super::*;
    #[test]
    fn live_resize_capture_extents_follow_observed_native_size_at_display_scale() {
        assert_eq!(
            capture_resolution(CGSize::new(1280., 705.), 2.),
            CaptureResolution::Exact { width: 2560, height: 1410 },
        );
        assert_eq!(
            capture_resolution(CGSize::new(2560., 705.), 2.),
            CaptureResolution::Exact { width: 5120, height: 1410 },
        );
    }

    #[test]
    fn live_resize_exact_output_cannot_make_stale_same_aspect_content_ready() {
        let accepted = CGRect::new(CGPoint::ZERO, CGSize::new(1010., 1010.));
        let output = [2020., 2020.];
        assert!(!target_pixels_ready(output, Some((2000, 2000)), accepted, 2.));
        assert!(!target_pixels_ready(output, None, accepted, 2.));
        assert!(!target_pixels_ready(
            [2000., 2000.],
            Some((2020, 2020)),
            accepted,
            2.
        ));
        assert!(target_pixels_ready(output, Some((2020, 2020)), accepted, 2.));
    }

    #[test]
    fn live_resize_cohort_readiness_waits_for_every_old_and_target_image() {
        assert_eq!(ready_action(Stage::Preparing, false, false), None);
        assert_eq!(
            ready_action(Stage::Preparing, true, false),
            Some(ReadyAction::Commit)
        );
        assert_eq!(ready_action(Stage::AwaitingTargets, true, false), None);
        assert_eq!(
            ready_action(Stage::AwaitingTargets, true, true),
            Some(ReadyAction::Start)
        );
        assert_eq!(ready_action(Stage::Presenting, true, true), None);
    }

    #[test]
    fn live_resize_native_decorations_do_not_hide_interactive_foreign_windows() {
        let border = SLSWindowTags::from_bits_retain(0x100000202);
        assert!(passive_decoration(border));
        for interactive in [
            SLSWindowTags::DOCUMENT,
            SLSWindowTags::MODAL,
            SLSWindowTags::KIT_VISIBLE,
            SLSWindowTags::OPAQUE_FOR_EVENTS,
            SLSWindowTags::USER_INPUT_ACCESSORY,
        ] {
            assert!(!passive_decoration(border | interactive));
        }
        assert!(!passive_decoration(SLSWindowTags::IGNORE_FOR_EVENTS));
        assert!(!passive_decoration(SLSWindowTags::FLOATING));
    }

    #[test]
    fn live_resize_capture_crop_keeps_top_content_out_of_bottom_padding() {
        let full = capture_contents_rect([0., 0., 1., 1.]);
        assert_eq!(full, CGRect::new(CGPoint::ZERO, CGSize::new(1., 1.)));
        let top_half = capture_contents_rect([0., 0., 1., 0.5]);
        assert_eq!(
            top_half,
            CGRect::new(CGPoint::new(0., 0.5), CGSize::new(1., 0.5))
        );
        let inset = capture_contents_rect([0.25, 0.125, 0.5, 0.25]);
        assert_eq!(
            inset,
            CGRect::new(CGPoint::new(0.25, 0.625), CGSize::new(0.5, 0.25))
        );
    }
    #[test]
    fn live_resize_retarget_cannot_reuse_surfaces_across_displays_or_display_edges() {
        let (handle, _requests) = crate::actor::app::AppThreadHandle::channel();
        let bounds = CGRect::new(CGPoint::ZERO, CGSize::new(2048., 1330.));
        let mut plan = Plan {
            wid: WindowId::new(1, 1),
            window_id: 42,
            display: 1,
            handle,
            txid: Default::default(),
            from: CGRect::new(CGPoint::new(1024., 45.), CGSize::new(1024., 1285.)),
            to: CGRect::new(CGPoint::new(0., 45.), CGSize::new(2048., 1285.)),
            duration: Duration::from_millis(300),
        };
        assert!(fits_display(1, bounds, &plan));
        plan.display = 2;
        assert!(!fits_display(1, bounds, &plan));
        plan.display = 1;
        plan.to.origin.x = 2048.;
        assert!(!fits_display(1, bounds, &plan));
        plan.to.origin.x = 0.;
        plan.from.origin.y = -100.;
        assert!(!fits_display(1, bounds, &plan));
    }

    #[test]
    fn live_resize_backdrop_keeps_lower_windows_and_leaves_higher_windows_live() {
        let source = 42;
        let above = [WindowServerId::new(7), WindowServerId::new(9)];
        assert!(exclude_backdrop_window(source, Some(100), source, &above));
        assert!(exclude_backdrop_window(7, Some(200), source, &above));
        assert!(exclude_backdrop_window(9, Some(300), source, &above));
        assert!(exclude_backdrop_window(
            11,
            Some(std::process::id() as i32),
            source,
            &above
        ));
        assert!(!exclude_backdrop_window(13, Some(400), source, &above));
        assert!(!exclude_backdrop_window(15, None, source, &above));
    }

    #[test]
    fn live_resize_deadline_wakes_on_rift_executor_without_tokio_runtime() {
        let (sender, mut receiver) = actor::channel();
        crate::sys::executor::Executor::run(async move {
            let _sender = sender;
            assert!(matches!(
                next_event(&mut receiver, true).await,
                Some((_, Event::Wake))
            ));
        });
    }

    #[test]
    fn live_resize_retarget_cover_keeps_previous_native_extent_until_acceptance() {
        let frame = |width| CGRect::new(CGPoint::new(0., 45.), CGSize::new(width, 1000.));
        let visual = frame(1400.);
        let next = frame(1024.);
        let old_native = frame(2048.);
        let pending = cover_region(visual, next, old_native, true);
        assert!(
            contained(pending, old_native),
            "the previous expanded native window stays hidden during the next AX commit"
        );
        let accepted = cover_region(visual, next, next, false);
        assert_eq!(
            accepted, visual,
            "after native acceptance, expose the live desktop outside the proxy"
        );
        assert!(!contained(accepted, old_native));
    }
}
