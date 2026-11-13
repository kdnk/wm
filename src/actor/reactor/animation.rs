// Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use parking_lot::Mutex;
use tracing::{debug, trace};

use super::TransactionId;
use crate::actor::app::{AppThreadHandle, FrameMode, FrameSource, Request, WindowId, pid_t};
use crate::actor::gesture::{Context, Control};
use crate::actor::reactor::Reactor;
use crate::common::collections::{HashMap, HashSet};
use crate::layout_engine::systems::scrolling::{PresentedViewport, ViewportPresentation};
use crate::layout_engine::{LayoutId, LayoutSystem, LayoutSystemKind, VirtualWorkspaceId};
use crate::model::live_resize::{
    Client as ResizeClient, Phase as ResizePhase, Plan as ResizePlan, Ticket as ResizeTicket,
};
use crate::model::tx_store::WindowTxStore;
use crate::sys::display_link::DisplayLink;
use crate::sys::geometry::{Round, SameAs};
use crate::sys::power;
use crate::sys::screen::SpaceId;
use crate::sys::window_server::WindowServerId;

#[derive(Debug)]
pub struct AnimationSender {
    tx: Sender<Message>,
    control: Arc<Mutex<PresenterControl>>,
}
impl AnimationSender {
    pub fn channel() -> (Self, AnimationReceiver) {
        let (tx, commands) = crossbeam_channel::unbounded();
        let control = Arc::default();
        (
            Self {
                tx,
                control: Arc::clone(&control),
            },
            AnimationReceiver { commands, control },
        )
    }

    pub(crate) fn set_resize_client(&self, resize: Option<ResizeClient>) {
        self.control.lock().resize = resize;
    }

    pub fn send(&self, message: Message) -> Result<(), crossbeam_channel::SendError<Message>> {
        self.tx.send(message)
    }

    pub fn cancel(&self, windows: Vec<WindowId>) -> Vec<(WindowId, CGRect)> {
        let mut control = self.control.lock();
        let frames = windows
            .iter()
            .filter_map(|wid| {
                let (handle, frame) = control.frames.remove(wid)?;
                handle.cancel_window_animation(*wid);
                let native =
                    control.resize.as_ref().and_then(|resize| resize.cancel(*wid)).unwrap_or(frame);
                Some((*wid, native))
            })
            .collect();
        control.cancelled.extend(windows.iter().copied());
        let _ = self.send(Message::Stop(windows));
        frames
    }
}

#[derive(Debug, Default)]
struct PresenterControl {
    frames: HashMap<WindowId, (AppThreadHandle, CGRect)>,
    cancelled: HashSet<WindowId>,
    resize: Option<ResizeClient>,
}
impl PresenterControl {
    fn frame(&self, wid: WindowId, fallback: CGRect) -> CGRect {
        self.resize
            .as_ref()
            .and_then(|resize| resize.visual(wid))
            .unwrap_or_else(|| self.frames.get(&wid).map_or(fallback, |(_, frame)| *frame))
    }
}

pub struct AnimationReceiver {
    pub(super) commands: Receiver<Message>,
    control: Arc<Mutex<PresenterControl>>,
}

#[derive(Debug)]
pub enum Message {
    Replace(Animation),
    SkipToEnd(Animation),
    Stop(Vec<WindowId>),
    Camera(Box<CameraAnimation>),
    StopCamera(SpaceId),
    Displays(Vec<u32>),
}

#[derive(Debug, Default)]
pub struct AnimationManager {
    motions: Vec<Motion>,
    frames: FrameBatch,
    resize: Option<ResizeClient>,
}

/// Display scheduling, cancellation and retirement are shared by both motion
/// policies. A display may present a viewport alongside unrelated transitions.
#[derive(Debug)]
enum Motion {
    Transition(ActiveAnimation),
    Viewport(CameraAnimation),
}

type Frame = (WindowId, CGRect, bool, TransactionId, FrameSource);

/// Reusable presenter-local batches: all motions on a display publish through
/// one lock/wake per application, and terminal leases follow that publication.
#[derive(Debug, Default)]
struct FrameBatch(HashMap<(pid_t, bool), (AppThreadHandle, Vec<Frame>)>);
impl FrameBatch {
    fn extend(
        &mut self,
        handle: &AppThreadHandle,
        pid: pid_t,
        frames: impl Iterator<Item = Frame>,
    ) {
        if !handle.can_write_frames() {
            return;
        }
        let (actor, pending) = self
            .0
            .entry((pid, handle.constrains_scrolling()))
            .or_insert_with(|| (handle.clone(), Vec::new()));
        if !actor.same_actor(handle) || !actor.can_write_frames() {
            *actor = handle.clone();
        }
        pending.extend(frames);
    }

    fn flush(&mut self) {
        for (handle, frames) in self.0.values_mut() {
            if !frames.is_empty() {
                handle.send_interactive_frames(frames.drain(..));
            }
        }
    }
}

struct DisplayPresenter {
    manager: AnimationManager,
    link: DisplayLink,
    fallback_deadline: Instant,
}

#[derive(Debug)]
struct ActiveAnimation {
    animation: Animation,
    started: Instant,
    progress: f64,
    next_sample: Instant,
    ended: bool,
}

#[derive(Debug)]
pub struct Animation {
    interval: Duration,
    duration: Duration,
    display: u32,
    windows: Vec<PresentedWindow>,
    handled_windows: Vec<WindowId>,
}

#[derive(Clone, Debug)]
pub(super) struct CameraIdentity {
    pub space: SpaceId,
    pub workspace: VirtualWorkspaceId,
    pub layout: LayoutId,
}

#[derive(Debug)]
pub(super) struct ViewportHandle {
    pub identity: CameraIdentity,
    // Headless execution uses the same renderer without a presenter thread.
    pub state: Arc<Mutex<PresentedCamera>>,
    windows: Vec<WindowId>,
    pub headless: Option<CameraAnimation>,
}

#[derive(Debug)]
pub(super) struct PresentedCamera {
    viewport: PresentedViewport,
    gesture: Option<(f64, Duration)>,
    active: bool,
    stopped: bool,
}

#[derive(Debug)]
pub struct CameraAnimation {
    pub(super) presentation: ViewportPresentation,
    windows: Vec<PresentedWindow>,
    geometry_started: Instant,
    geometry_progress: f64,
    duration: Duration,
    pub(super) gesture: Option<(Context, Control, f64, Duration)>,
    pub(super) active: bool,
    identity: CameraIdentity,
    events: Option<super::Sender>,
    state: Arc<Mutex<PresentedCamera>>,
    display: u32,
    animate: bool,
    scale: f64,
    store: WindowTxStore,
    interval: Duration,
    bound: Option<super::managers::ScrollingDisplayBounds>,
}

#[derive(Debug)]
struct PresentedWindow {
    handle: AppThreadHandle,
    wid: WindowId,
    wsid: Option<WindowServerId>,
    // Transitions interpolate from/to screen frames. Viewport motion translates
    // the prepared world frame in `to`; `frame` is always the last publication.
    from: CGRect,
    to: CGRect,
    frame: CGRect,
    motion: Option<FrameSpring>,
    velocity: FrameVelocity,
    txid: TransactionId,
    fixed: bool,
    leased: bool,
    announced: bool,
    proxy: Option<ProxyPresentation>,
}

#[derive(Clone, Debug)]
struct ProxyPresentation {
    ticket: ResizeTicket,
    spring: FrameSpring,
}

#[derive(Clone, Copy)]
enum Sample<'a> {
    Transition {
        progress: f64,
        duration: Duration,
    },
    Viewport {
        presentation: &'a ViewportPresentation,
        offset: f64,
        scale: f64,
        bound: Option<super::managers::ScrollingDisplayBounds>,
        progress: f64,
        duration: Duration,
        camera_velocity: f64,
    },
}

impl PresentedWindow {
    fn begin(&mut self) {
        if !std::mem::replace(&mut self.leased, true) {
            let _ = self.handle.send(Request::BeginWindowAnimation(self.wid, Default::default()));
        }
    }

    fn end(&mut self) {
        if let Some(proxy) = &self.proxy {
            proxy.ticket.retire();
        }
        if std::mem::replace(&mut self.leased, false) {
            let _ = self.handle.send(Request::EndWindowAnimation(self.wid, Default::default()));
        }
    }

    fn cancel(&mut self, store: Option<&WindowTxStore>) {
        if let Some(proxy) = self.proxy.take() {
            proxy.ticket.revoke();
            proxy.ticket.wake();
            proxy.ticket.retire();
        }
        self.leased = false;
        self.handle.cancel_window_animation(self.wid);
        if let Some(store) = store
            && let Some(wsid) = self.wsid
        {
            store.clear_target_if_current(&wsid, self.txid);
        }
    }

    fn proxy_pending(&self) -> bool {
        self.proxy.as_ref().is_some_and(|proxy| {
            !matches!(
                proxy.ticket.state().phase,
                ResizePhase::Retired | ResizePhase::Fallback
            )
        })
    }

    fn sample_proxy(&mut self, now: Instant, phase: Option<ResizePhase>) -> bool {
        let Some(proxy) = &mut self.proxy else { return false };
        let phase = phase.unwrap_or_else(|| proxy.ticket.phase_at(proxy.ticket.started()));
        match phase {
            ResizePhase::Preparing | ResizePhase::AwaitingNative | ResizePhase::AwaitingTarget => {}
            ResizePhase::Presenting(started) => {
                let duration = proxy.ticket.plan().duration;
                let progress = if duration.is_zero() {
                    1.
                } else {
                    (now.saturating_duration_since(started).as_secs_f64() / duration.as_secs_f64())
                        .clamp(0., 1.)
                };
                let accepted = proxy.ticket.state().accepted.unwrap_or(proxy.ticket.plan().to);
                proxy.spring.to = accepted;
                let (frame, mut velocity) = proxy.spring.sample(progress, duration);
                self.frame = positive_size(frame, &mut velocity);
                self.velocity = velocity;
                let mut state = proxy.ticket.state();
                state.visual = self.frame;
                if progress == 1. {
                    state.phase = ResizePhase::Finishing;
                    self.motion = None;
                }
                drop(state);
            }
            ResizePhase::Finishing => {}
            ResizePhase::Retired => {
                if proxy.ticket.state().accepted.is_some() {
                    self.frame = proxy.ticket.native();
                }
                self.motion = None;
                self.velocity = FrameVelocity::default();
            }
            ResizePhase::Fallback => {
                if proxy.ticket.state().accepted.is_some() {
                    self.frame = proxy.ticket.native();
                    self.motion = None;
                    self.velocity = FrameVelocity::default();
                } else if {
                    let state = proxy.ticket.state();
                    state.continued || state.attempted
                } {
                    self.handle.send_interactive_frame(
                        self.wid,
                        proxy.ticket.plan().to,
                        true,
                        self.txid,
                        FrameSource::Ordinary,
                    );
                    self.frame = proxy.ticket.plan().to;
                    self.motion = None;
                    self.velocity = FrameVelocity::default();
                    proxy.ticket.state().phase = ResizePhase::Retired;
                } else {
                    proxy.ticket.retire();
                    self.proxy = None;
                    return false;
                }
            }
        }
        true
    }

    fn sample(
        &mut self,
        sample: Sample<'_>,
        now: Instant,
        phase: Option<ResizePhase>,
    ) -> Option<Frame> {
        if self.sample_proxy(now, phase) {
            return None;
        }
        let (frame, set_size, source, force) = match sample {
            Sample::Transition { progress, duration } => {
                let spring = self.motion.unwrap_or_else(|| {
                    FrameSpring::new(self.from, self.to, FrameVelocity::default())
                });
                let (frame, mut velocity) = spring.sample(progress, duration);
                let frame = positive_size(frame, &mut velocity);
                self.velocity = velocity;
                let set_size = frame.size != self.frame.size || progress == 1.0;
                (frame, set_size, FrameSource::Ordinary, progress == 1.0)
            }
            Sample::Viewport {
                presentation,
                offset,
                scale,
                bound,
                progress,
                duration,
                camera_velocity,
            } => {
                if self.fixed && self.announced && self.motion.is_none() {
                    return None;
                }
                let mut frame = presentation.frame_at_offset(self.to, self.fixed, scale, offset);
                let mut velocity = FrameVelocity::default();
                if !self.fixed {
                    velocity.origin.x = -camera_velocity;
                }
                let had_motion = self.motion.is_some();
                if let Some(spring) = self.motion {
                    let (residual, residual_velocity) = spring.sample(progress, duration);
                    frame = add_frame(frame, residual);
                    velocity = velocity.add(residual_velocity);
                    if progress == 1.0 {
                        self.motion = None;
                    }
                }
                let frame = positive_size(frame, &mut velocity);
                let constrained = bound.map_or(frame, |bounds| bounds.apply(frame));
                if constrained.origin.x != frame.origin.x {
                    velocity.origin.x = 0.0;
                }
                if constrained.origin.y != frame.origin.y {
                    velocity.origin.y = 0.0;
                }
                self.velocity = velocity;
                (
                    constrained,
                    constrained.size != self.frame.size,
                    FrameSource::Viewport,
                    !self.announced || progress == 1.0 && had_motion,
                )
            }
        };
        let changed = frame != self.frame || force;
        self.frame = frame;
        self.announced = true;
        changed.then_some((self.wid, frame, set_size, self.txid, source))
    }
}

fn stage_windows(windows: &mut [PresentedWindow], sample: Sample<'_>, frames: &mut FrameBatch) {
    let now = Instant::now();
    let mut clocks = HashMap::default();
    for window in &*windows {
        if let Some(proxy) = &window.proxy {
            clocks.entry(proxy.ticket.scene_id()).or_insert_with(|| proxy.ticket.started());
        }
    }
    for group in windows.chunk_by_mut(|a, b| {
        a.wid.pid == b.wid.pid && a.handle.constrains_scrolling() == b.handle.constrains_scrolling()
    }) {
        let handle = group[0].handle.clone();
        frames.extend(
            &handle,
            group[0].wid.pid,
            group.iter_mut().filter_map(|window| {
                let phase =
                    window.proxy.as_ref().map(|p| p.ticket.phase_at(clocks[&p.ticket.scene_id()]));
                window.sample(sample, now, phase)
            }),
        );
    }
}

impl CameraAnimation {
    pub(super) fn sample(&mut self, now: Instant) {
        let mut frames = FrameBatch::default();
        self.sample_into(now, &mut frames);
        frames.flush();
        if !self.active {
            self.end();
        }
    }

    fn sample_into(&mut self, now: Instant, frames: &mut FrameBatch) {
        let scale = self.scale;
        if self.state.lock().stopped {
            self.stop();
        }
        if !self.active {
            return;
        }
        if let Some((context, control, total, time)) = &mut self.gesture {
            if !control.valid(context.epoch) {
                self.stop();
                return;
            }
            if let Some(motion) = control.latest(context.session)
                && motion.timestamp > *time
            {
                self.presentation.update(motion.total_x - *total, motion.timestamp);
                *total = motion.total_x;
                *time = motion.timestamp;
            }
        }
        let ongoing = self.presentation.sample(now, scale);
        let progress = if self.duration.is_zero() {
            1.0
        } else {
            (now.saturating_duration_since(self.geometry_started).as_secs_f64()
                / self.duration.as_secs_f64())
            .clamp(0.0, 1.0)
        };
        let progress = progress.max(self.geometry_progress);
        self.geometry_progress = progress;
        let camera_velocity = self.presentation.position_velocity(now).1;
        stage_windows(
            &mut self.windows,
            Sample::Viewport {
                presentation: &self.presentation,
                offset: self.presentation.offset(),
                scale,
                bound: self.bound,
                progress,
                duration: self.duration,
                camera_velocity,
            },
            frames,
        );
        self.active = ongoing
            || self
                .windows
                .iter()
                .any(|window| window.motion.is_some() || window.proxy_pending());
        self.publish_state(now);
    }

    fn publish_state(&self, now: Instant) {
        let mut state = self.state.lock();
        state.viewport = self.presentation.snapshot(now);
        state.gesture = self.gesture.as_ref().map(|(_, _, total, time)| (*total, *time));
        state.active = self.active;
    }

    fn replace(&mut self, mut previous: Option<Self>) {
        if let Some(old) = &mut previous
            && (old.identity.workspace, old.identity.layout)
                != (self.identity.workspace, self.identity.layout)
        {
            old.stop();
            previous = None;
        }
        let now = Instant::now();
        if self.animate && self.gesture.is_none() && !self.presentation.animated() {
            let (from, velocity) = if let Some(old) = &previous {
                let (from, velocity) = old.presentation.position_velocity(now);
                // Continue in the semantic viewport's rebased coordinates.
                (
                    from + self.presentation.offset() - old.presentation.offset(),
                    velocity,
                )
            } else {
                // Native frames can be clamped after parking. The viewport
                // retains its own starting position when a target changes.
                (self.presentation.offset(), 0.0)
            };
            self.presentation.retarget(from, velocity, now);
        }
        if let Some(mut old) = previous {
            for window in &mut self.windows {
                if let Some(old) = old.windows.iter().find(|old| old.wid == window.wid) {
                    window.frame = old.frame;
                    window.velocity = old.velocity;
                    window.leased = old.leased;
                    window.proxy = old.proxy.clone();
                }
            }
            old.windows.retain(|old| !self.windows.iter().any(|new| new.wid == old.wid));
            old.stop();
        }
        self.geometry_started = now;
        self.geometry_progress = 0.0;
        let camera_velocity = self.presentation.position_velocity(now).1;
        for window in &mut self.windows {
            window.motion = None;
            if self.animate && !window.fixed {
                let base = self.presentation.frame_at_offset(
                    window.to,
                    window.fixed,
                    self.scale,
                    self.presentation.offset(),
                );
                let mut velocity = window.velocity;
                velocity.origin.x += camera_velocity;
                let residual = subtract_frame(window.frame, base);
                if !residual.same_as(CGRect::ZERO) || !velocity.is_zero() {
                    window.motion = Some(FrameSpring::new(residual, CGRect::ZERO, velocity));
                }
            }
        }
    }

    fn begin(&mut self) {
        self.windows.sort_unstable_by_key(|w| w.wid.pid);
        for window in &mut self.windows {
            window.announced = false;
            window.txid = window.wsid.map_or_else(TransactionId::default, |wsid| {
                let frame = self.presentation.target_frame(window.to, window.fixed, self.scale);
                let frame = self.bound.map_or(frame, |bounds| bounds.apply(frame));
                if self.animate
                    && self.gesture.is_none()
                    && !self.presentation.gesturing()
                    && let Some(proxy) = &window.proxy
                    && proxy.ticket.is_current()
                    && !matches!(
                        proxy.ticket.state().phase,
                        ResizePhase::Retired | ResizePhase::Fallback
                    )
                {
                    let plan = proxy.ticket.plan();
                    if plan.wid == window.wid
                        && plan.window_id == wsid.as_u32()
                        && plan.display == self.display
                        && plan.handle.same_actor(&window.handle)
                        && plan.to.same_as(frame)
                        && self.store.last_txid(&wsid) == plan.txid
                    {
                        return plan.txid;
                    }
                }
                let txid = self.store.next_txid(wsid);
                if self.gesture.is_none() && !self.presentation.gesturing() {
                    self.store.insert(wsid, txid, frame);
                }
                txid
            });
            window.begin();
        }
    }

    fn cancel_windows(&mut self, windows: &[WindowId]) {
        self.windows.retain_mut(|window| {
            if windows.binary_search(&window.wid).is_ok() {
                window.cancel(Some(&self.store));
                false
            } else {
                true
            }
        });
        if self.windows.is_empty() {
            self.active = false;
        }
    }

    fn end(&mut self) {
        self.active = false;
        for window in &mut self.windows {
            window.end();
        }
    }

    pub(super) fn stop(&mut self) {
        if std::mem::replace(&mut self.active, false) {
            for window in &mut self.windows {
                window.cancel(Some(&self.store));
            }
        }
    }
}

impl Reactor {
    pub(super) fn cancel_window_presentations(&mut self, mut windows: Vec<WindowId>) {
        if windows.is_empty() {
            return;
        }
        windows.sort_unstable();
        let mut frames = Vec::new();
        for camera in self.presentations.values_mut() {
            if let Some(state) = &mut camera.headless {
                frames.extend(
                    state
                        .windows
                        .iter()
                        .filter(|w| windows.binary_search(&w.wid).is_ok())
                        .map(|w| (w.wid, w.frame)),
                );
                state.cancel_windows(&windows);
                state.publish_state(Instant::now());
            }
        }
        if let Some(tx) = &self.animation_tx {
            frames.extend(tx.cancel(windows));
        }
        for (wid, frame) in frames {
            if let Some(window) = self.state.windows.window_mut(wid) {
                window.frame_monotonic = frame;
                if let Some(wsid) = window.info.sys_id {
                    self.transaction_manager.clear_target_for_window(wsid);
                }
            }
        }
        self.commit_presentations();
    }

    pub(super) fn commit_presentations(&mut self) {
        if self.refreshes_blocked() {
            return;
        }
        let control = self.animation_tx.as_ref().map(|tx| tx.control.clone());
        let control = control.as_ref().map(|c| c.lock());
        let mut completed = Vec::new();
        for (space, camera) in &self.presentations {
            let state = camera.state.lock();
            let identity = &camera.identity;
            if let Some(ws) = self
                .layout_manager
                .layout_engine
                .workspaces_mut()
                .workspaces
                .get_mut(identity.workspace)
                && let LayoutSystemKind::Scrolling(system) = &mut ws.layout_system
            {
                system.commit_presented_viewport(identity.layout, &state.viewport);
            }
            for &wid in &camera.windows {
                // Proxy frames never reached AX. Native feedback must retain
                // the last native rectangle while the proxy owns presentation.
                if control
                    .as_ref()
                    .and_then(|control| control.resize.as_ref())
                    .and_then(|resize| resize.visual(wid))
                    .is_some()
                {
                    continue;
                }
                let frame = if let Some(control) = &control {
                    control.frames.get(&wid).map(|(_, frame)| *frame)
                } else {
                    camera
                        .headless
                        .as_ref()
                        .and_then(|c| c.windows.iter().find(|w| w.wid == wid))
                        .map(|w| w.frame)
                };
                if let Some(frame) = frame
                    && let Some(model) = self.state.windows.window_mut(wid)
                {
                    model.frame_monotonic = frame;
                }
            }
            if let Some(session) = &mut self.viewport_gesture
                && session.workspace == identity.workspace
                && session.layout == identity.layout
                && let Some((total, time)) = state.gesture
            {
                session.applied = total;
                session.timestamp = time;
            }
            if !state.active {
                completed.push(*space);
            }
        }
        for space in completed {
            self.presentations.remove(&space);
            if self
                .viewport_gesture
                .as_ref()
                .is_some_and(|s| s.released && s.context.space == space)
            {
                self.retire_viewport_session();
            }
        }
    }

    pub(super) fn stop_camera(&mut self, space: SpaceId) {
        if let Some(mut camera) = self.presentations.remove(&space) {
            camera.state.lock().stopped = true;
            let frames = if let Some(headless) = &mut camera.headless {
                let frames = headless.windows.iter().map(|w| (w.wid, w.frame)).collect();
                headless.stop();
                frames
            } else if let Some(tx) = &self.animation_tx {
                tx.cancel(camera.windows)
            } else {
                Vec::new()
            };
            for (wid, frame) in frames {
                if let Some(window) = self.state.windows.window_mut(wid) {
                    window.frame_monotonic = frame;
                }
            }
            if let Some(tx) = &self.animation_tx {
                let _ = tx.send(Message::StopCamera(space));
            }
        }
    }

    pub(super) fn retire_presentations(&mut self) {
        let invalid: Vec<_> = self.presentations.iter().filter_map(|(space, camera)| {
            let valid = self.layout_manager.layout_engine.workspaces().active_layout_for_space(*space)
                == Some((camera.identity.workspace, camera.identity.layout))
                && self.active_spaces.contains(space)
                && self.space_state.screen_by_space(*space).is_some()
                && self.layout_manager.layout_engine.workspaces().workspaces.get(camera.identity.workspace)
                    .is_some_and(|ws| matches!(&ws.layout_system, LayoutSystemKind::Scrolling(system) if system.contains_layout(camera.identity.layout)))
                && matches!(self.mission_control_manager.mission_control_state, super::MissionControlState::Inactive);
            (!valid).then_some(*space)
        }).collect();
        for space in invalid {
            self.stop_camera(space);
        }
    }

    pub(super) fn present_camera(
        &mut self,
        space: SpaceId,
        animate: bool,
        gesture: Option<(Context, Control, f64, Duration)>,
        skip: Option<WindowId>,
    ) -> Option<HashSet<WindowId>> {
        if self.animation_tx.is_none() && gesture.is_none() && self.viewport_gesture.is_none() {
            return None;
        }
        let screen = self.space_state.screen_by_space(space)?;
        let display = screen.id.as_u32();
        let scale = screen.backing_scale;
        let screen_frames: Vec<_> =
            self.space_state.screens.iter().map(|screen| screen.frame).collect();
        let bound =
            super::managers::ScrollingDisplayBounds::for_screen(screen.frame, &screen_frames);
        let (workspace, layout) =
            self.layout_manager.layout_engine.workspaces().active_layout_for_space(space)?;
        let LayoutSystemKind::Scrolling(system) =
            &self.layout_manager.layout_engine.workspaces()[workspace].layout_system
        else {
            return None;
        };
        let (mut presentation, frames) = system.presentation(layout)?;
        let previous = self.presentations.remove(&space).and_then(|p| p.headless);
        let gesture = gesture.or_else(|| {
            self.viewport_gesture
                .as_ref()
                .filter(|s| {
                    presentation.gesturing()
                        && !s.released
                        && s.workspace == workspace
                        && s.layout == layout
                })
                .map(|s| (s.context.clone(), s.control.clone(), s.applied, s.timestamp))
        });
        let now = Instant::now();
        let control = self.animation_tx.as_ref().map(|tx| tx.control.lock());
        let windows = frames
            .into_iter()
            .filter_map(|(wid, base_frame, fixed)| {
                if Some(wid) == skip {
                    return None;
                }
                let window = self.state.windows.window(wid)?;
                let app = self.app_manager.apps.get(&wid.pid)?;
                let frame = control.as_ref().map_or(window.frame_monotonic, |control| {
                    control.frame(wid, window.frame_monotonic)
                });
                Some(PresentedWindow {
                    handle: app.handle.with_scrolling_bounds(bound),
                    wid,
                    wsid: window.info.sys_id,
                    from: window.frame_monotonic,
                    to: base_frame,
                    fixed,
                    announced: false,
                    proxy: None,
                    leased: false,
                    frame,
                    motion: None,
                    velocity: FrameVelocity::default(),
                    txid: TransactionId::default(),
                })
            })
            .collect();
        drop(control);
        if !animate {
            presentation.finish();
        }
        let identity = CameraIdentity { space, workspace, layout };
        let state = Arc::new(Mutex::new(PresentedCamera {
            viewport: presentation.snapshot(now),
            gesture: None,
            active: true,
            stopped: false,
        }));
        let mut next = CameraAnimation {
            presentation,
            geometry_started: Instant::now(),
            geometry_progress: 0.0,
            duration: Duration::from_secs_f64(self.config.settings.animation_duration),
            windows,
            gesture,
            active: true,
            identity: identity.clone(),
            events: self.communication_manager.events_tx.clone(),
            state: state.clone(),
            display,
            animate,
            scale,
            store: self.transaction_manager.store.clone(),
            interval: Duration::from_secs_f64(1.0 / self.config.settings.animation_fps),
            bound,
        };
        let previous_was_absent = previous.is_none();
        if !previous_was_absent {
            next.replace(previous);
        }
        let windows = next.windows.iter().map(|w| w.wid).collect();
        let ids = next.windows.iter().map(|w| w.wid).collect();
        self.presentations.insert(space, ViewportHandle {
            identity,
            state,
            windows: ids,
            headless: None,
        });
        if let Some(tx) = &self.animation_tx {
            let _ = tx.send(Message::Camera(Box::new(next)));
        } else {
            if previous_was_absent {
                next.replace(None);
            }
            next.begin();
            next.sample(now);
            self.presentations.get_mut(&space).unwrap().headless = Some(next);
            self.commit_presentations();
        }
        Some(windows)
    }
}

fn scrolling_bounds(
    reactor: &Reactor,
    space: SpaceId,
) -> Option<super::managers::ScrollingDisplayBounds> {
    if reactor.layout_manager.layout_engine.active_layout_mode_at(space)
        != crate::common::config::LayoutMode::Scrolling
    {
        return None;
    }
    let screen = reactor.space_state.screen_by_space(space)?;
    let screens: Vec<_> = reactor.space_state.screens.iter().map(|s| s.frame).collect();
    super::managers::ScrollingDisplayBounds::for_screen(screen.frame, &screens)
}

impl AnimationManager {
    pub fn new() -> Self { Self::default() }

    /// One runner for all displays. Commands are semantic work; display wakes
    /// are bounded to one permit and each link retains only its newest timing.
    pub fn run(receiver: AnimationReceiver) {
        let AnimationReceiver { commands: rx, control } = receiver;
        let (wake, ticks) = crossbeam_channel::bounded(1);
        let mut displays: HashMap<u32, DisplayPresenter> = HashMap::default();
        loop {
            let deadline = displays
                .values()
                .filter(|d| d.manager.has_work())
                .map(|d| d.fallback_deadline)
                .min();
            let timeout =
                deadline.map_or(Duration::MAX, |t| t.saturating_duration_since(Instant::now()));
            crossbeam_channel::select! {
                recv(rx) -> message => {
                    let Ok(message) = message else { break };
                    match message {
                        Message::Displays(known) => {
                            displays.retain(|display, presenter| {
                                if known.contains(display) { return true; }
                                presenter.manager.stop_all();
                                false
                            });
                        }
                        Message::StopCamera(space) => {
                            for d in displays.values_mut() {
                                for motion in &mut d.manager.motions {
                                    if let Motion::Viewport(camera) = motion && camera.identity.space == space { camera.stop(); }
                                }
                                d.manager.retire();
                            }
                        }
                        Message::Stop(mut windows) => {
                            windows.sort_unstable();
                            for d in displays.values_mut() { d.manager.stop_windows(&windows); d.manager.retire(); }
                            let mut shared = control.lock();
                            for wid in &windows { shared.cancelled.remove(wid); shared.frames.remove(wid); }
                        }
                        mut message => {
                            let display = match &message {
                                Message::Replace(a) | Message::SkipToEnd(a) => a.display,
                                Message::Camera(c) => c.display,
                                Message::Stop(..) | Message::StopCamera(..) | Message::Displays(..) => unreachable!(),
                            };
                            let entry = displays.entry(display).or_insert_with(|| DisplayPresenter {
                                manager: Self { resize: control.lock().resize.clone(), ..Self::new() }, link: DisplayLink::for_display(display, wake.clone()),
                                fallback_deadline: Instant::now(),
                            });
                            let mut shared = control.lock();
                            let mut cancelled: Vec<_> = shared.cancelled.iter().copied().collect();
                            cancelled.sort_unstable();
                            entry.manager.stop_windows(&cancelled);
                            entry.manager.retire();
                            // A command queued before cancellation cannot resurrect its
                            // windows or replay an obsolete instant-layout target.
                            match &mut message {
                                Message::Replace(a) | Message::SkipToEnd(a) => a.windows.retain(|w| !shared.cancelled.contains(&w.wid)),
                                Message::Camera(c) => {
                                    c.windows.retain(|w| !shared.cancelled.contains(&w.wid));
                                    c.active = !c.windows.is_empty();
                                }
                                _ => unreachable!(),
                            }
                            entry.manager.handle_message(message);
                            entry.manager.publish_frames(&mut shared, true);
                            entry.manager.retire();
                            entry.link.set_paused(!entry.manager.has_work());
                            entry.fallback_deadline = Instant::now();
                        }
                    }
                }
                recv(ticks) -> _ => {}
                default(timeout) => {}
            }
            let now = Instant::now();
            let mut shared = control.lock();
            let cancelled: Vec<_> = {
                let mut w: Vec<_> = shared.cancelled.iter().copied().collect();
                w.sort_unstable();
                w
            };
            for DisplayPresenter {
                manager,
                link,
                fallback_deadline: deadline,
            } in displays.values_mut()
            {
                manager.stop_windows(&cancelled);
                if !manager.has_work() {
                    manager.retire();
                    link.set_paused(true);
                    continue;
                }
                let native = link.latest();
                let sample =
                    native.map(|(_, target)| target).or_else(|| (now >= *deadline).then_some(now));
                if let Some(sample) = sample {
                    let delay =
                        manager.tick_at(sample).unwrap_or(Duration::from_secs_f64(1.0 / 60.0));
                    // Resume fallback after three missing native refreshes.
                    *deadline = if let Some((tick, _)) = native {
                        sample
                            + Duration::try_from_secs_f64(
                                3.0 * (tick.target_timestamp - tick.timestamp),
                            )
                            .ok()
                            .filter(|d| !d.is_zero())
                            .unwrap_or(delay)
                            .min(Duration::from_millis(250))
                    } else {
                        now + delay
                    };
                }
                manager.publish_frames(&mut shared, false);
                manager.retire();
            }
            for d in displays.values() {
                d.link.set_paused(!d.manager.has_work());
            }
        }
        for d in displays.values_mut() {
            for motion in &mut d.manager.motions {
                match motion {
                    Motion::Transition(a) => a.animation.finish_all(),
                    Motion::Viewport(c) => c.stop(),
                }
                motion.complete();
            }
        }
    }

    fn retire(&mut self) {
        self.motions.retain_mut(|motion| {
            if motion.has_work() {
                return true;
            }
            motion.finish_frame();
            motion.complete();
            false
        });
    }

    fn stop_all(&mut self) {
        for motion in &mut self.motions {
            motion.stop();
        }
        self.retire();
    }

    fn publish_frames(&self, shared: &mut PresenterControl, initialize: bool) {
        for w in self.motions.iter().flat_map(Motion::windows) {
            let (handle, frame) =
                shared.frames.entry(w.wid).or_insert_with(|| (w.handle.clone(), w.frame));
            *frame = w.frame;
            if initialize {
                *handle = w.handle.clone();
            }
        }
    }

    fn has_work(&self) -> bool { self.motions.iter().any(Motion::has_work) }

    fn stop_windows(&mut self, windows: &[WindowId]) {
        if !windows.is_empty() {
            for motion in &mut self.motions {
                motion.stop_windows(windows);
            }
        }
    }

    pub fn handle_message(&mut self, message: Message) {
        self.retire();
        match message {
            Message::Camera(mut camera) => {
                let old = self
                    .motions
                    .iter()
                    .position(
                        |m| matches!(m, Motion::Viewport(c) if c.identity.space == camera.identity.space),
                    )
                    .map(|idx| self.motions.swap_remove(idx));
                camera.replace(old.map(|m| {
                    let Motion::Viewport(c) = m else { unreachable!() };
                    c
                }));
                let mut windows: Vec<_> = camera.windows.iter().map(|w| w.wid).collect();
                windows.sort_unstable();
                self.stop_windows(&windows);
                camera.begin();
                self.motions.push(Motion::Viewport(*camera));
            }
            Message::Stop(mut windows) => {
                windows.sort_unstable();
                self.stop_windows(&windows);
            }
            Message::Replace(animation) => {
                let old = self
                    .motions
                    .iter()
                    .position(|m| matches!(m, Motion::Transition(_)))
                    .map(|idx| self.motions.swap_remove(idx));
                let next = match old {
                    Some(Motion::Transition(active)) => Some(active.replace_with(animation)),
                    None => ActiveAnimation::start(animation),
                    _ => unreachable!(),
                };
                if let Some(next) = next {
                    self.motions.push(Motion::Transition(next));
                }
            }
            Message::SkipToEnd(animation) => {
                self.motions.retain_mut(|m| {
                    if let Motion::Transition(a) = m {
                        a.animation.finish_all();
                        false
                    } else {
                        true
                    }
                });
                animation.skip_to_end();
            }
            _ => unreachable!("runner handles display control"),
        }
        self.prepare_proxies();
    }

    fn prepare_proxies(&mut self) {
        let Some(client) = &self.resize else { return };
        for motion in &mut self.motions {
            let unsupported = matches!(&*motion,Motion::Viewport(camera) if !camera.animate || camera.gesture.is_some() || camera.presentation.gesturing());
            if unsupported {
                for window in motion.windows_mut() {
                    if let Some(proxy) = window.proxy.take() {
                        proxy.ticket.revoke();
                        proxy.ticket.wake();
                        proxy.ticket.retire();
                        window.frame = proxy.ticket.native();
                        window.from = window.frame;
                        window.motion = None;
                        window.velocity = FrameVelocity::default();
                        window.announced = false;
                    }
                }
                continue;
            }
            let (display, duration, targets) = match &*motion {
                Motion::Transition(active) => (
                    active.animation.display,
                    active.animation.duration,
                    active.animation.windows.iter().map(|w| w.to).collect::<Vec<_>>(),
                ),
                Motion::Viewport(camera) => {
                    let targets = camera
                        .windows
                        .iter()
                        .map(|w| {
                            let frame =
                                camera.presentation.target_frame(w.to, w.fixed, camera.scale);
                            camera.bound.map_or(frame, |bound| bound.apply(frame))
                        })
                        .collect();
                    (camera.display, camera.duration, targets)
                }
            };
            let mut plans = Vec::new();
            for (window, target) in motion.windows_mut().iter_mut().zip(targets) {
                if window
                    .proxy
                    .as_ref()
                    .is_some_and(|p| matches!(p.ticket.state().phase, ResizePhase::Retired))
                {
                    window.proxy = None;
                }
                let Some(wsid) = window.wsid else { continue };
                if !crate::model::live_resize::eligible(window.frame, target)
                    && window.proxy.is_none()
                {
                    continue;
                }
                plans.push(ResizePlan {
                    wid: window.wid,
                    window_id: wsid.as_u32(),
                    display,
                    handle: window.handle.clone(),
                    txid: window.txid,
                    from: window.frame,
                    to: target,
                    duration,
                });
            }
            if plans.is_empty() {
                continue;
            }
            let scene = client.prepare_scene(plans);
            for ticket in scene.members {
                let window =
                    motion.windows_mut().iter_mut().find(|w| w.wid == ticket.plan().wid).unwrap();
                if window
                    .proxy
                    .as_ref()
                    .is_some_and(|proxy| proxy.ticket.generation() == ticket.generation())
                {
                    continue;
                }
                window.proxy = Some(ProxyPresentation {
                    spring: FrameSpring::new(window.frame, ticket.plan().to, window.velocity),
                    ticket,
                });
            }
        }
    }

    pub fn tick_at(&mut self, now: Instant) -> Option<Duration> {
        let delay = self
            .motions
            .iter_mut()
            .filter_map(|motion| motion.sample(now, &mut self.frames))
            .min();
        if let Some(client) = &self.resize {
            client.publish_visuals(self.motions.iter().flat_map(|motion| {
                motion.windows().iter().filter_map(|window| {
                    window.proxy.as_ref().map(|proxy| (proxy.ticket.clone(), window.frame))
                })
            }));
        }
        self.frames.flush();
        for motion in &mut self.motions {
            motion.finish_frame();
        }
        if !self.has_work() {
            self.frames.0.clear();
        }
        delay
    }

    pub fn animate_layout(
        reactor: &mut Reactor,
        space: SpaceId,
        layout: &[(WindowId, CGRect)],
        is_resize: bool,
        skip_wid: Option<WindowId>,
        allow_animation: bool,
    ) -> bool {
        reactor.retire_presentations();
        let setting = reactor.layout_manager.layout_engine.layout_specific_animate_settings(space);
        let animate_camera = allow_animation
            && !is_resize
            && setting.unwrap_or(reactor.config.settings.animate)
            && !(setting.is_none() && power::is_low_power_mode_enabled());
        let presentation = reactor.present_camera(space, animate_camera, None, skip_wid);
        let camera = presentation.is_some();
        let presented = presentation.unwrap_or_default();
        if !animate_camera {
            let windows = layout
                .iter()
                .map(|(wid, _)| *wid)
                .filter(|wid| !presented.contains(wid))
                .collect();
            reactor.cancel_window_presentations(windows);
        }
        let Some(active_ws) =
            reactor.layout_manager.layout_engine.workspaces().active_workspace(space)
        else {
            return false;
        };
        let mut anim = Animation::new(
            reactor.config.settings.animation_fps,
            reactor.config.settings.animation_duration,
        );
        let Some(screen) = reactor.space_state.screen_by_space(space) else {
            return false;
        };
        anim.display = screen.id.as_u32();
        let bounds = scrolling_bounds(reactor, space);
        let mut any_frame_changed = camera;

        for &(wid, target_frame) in layout {
            if presented.contains(&wid) {
                anim.mark_handled(wid);
                continue;
            }
            if skip_wid == Some(wid) {
                anim.mark_handled(wid);
                trace!(
                    ?wid,
                    "Skipping animated layout update for window currently being dragged"
                );
                continue;
            }

            let target_frame = target_frame.round();
            let Some(window) = reactor.state.windows.window(wid) else {
                continue;
            };
            let current_frame =
                reactor.animation_tx.as_ref().map_or(window.frame_monotonic, |tx| {
                    tx.control.lock().frame(wid, window.frame_monotonic)
                });
            let window_server_id = window.info.sys_id;
            let pending = window_server_id
                .and_then(|wsid| reactor.transaction_manager.get_target_frame(wsid));
            // Matching the observed frame cannot cancel an older, different target.
            if pending.is_some_and(|frame| frame.same_as(target_frame))
                || (pending.is_none() && target_frame.same_as(current_frame))
            {
                continue;
            }
            any_frame_changed = true;
            let txid = window_server_id
                .map(|wsid| reactor.transaction_manager.generate_next_txid(wsid))
                .unwrap_or_default();

            let Some(app_state) = &reactor.app_manager.apps.get(&wid.pid) else {
                debug!(?wid, "Skipping for window - app no longer exists");
                continue;
            };

            let is_active = reactor.state.windows.workspace_for_window(wid) == Some(active_ws);

            if let Some(wsid) = window_server_id {
                reactor.transaction_manager.update_txid_entries([(wsid, txid, target_frame)]);
            }
            if is_active {
                trace!(?wid, ?current_frame, ?target_frame, "Animating visible window");
                let handle = app_state.handle.with_scrolling_bounds(
                    bounds
                        .filter(|_| !reactor.layout_manager.layout_engine.is_window_floating(wid)),
                );
                anim.add_window(&handle, wid, current_frame, target_frame, txid);
                anim.windows.last_mut().unwrap().wsid = window_server_id;
            } else {
                anim.mark_handled(wid);
                trace!(
                    ?wid,
                    ?current_frame,
                    ?target_frame,
                    "Direct positioning hidden window"
                );
                if let Err(e) =
                    app_state.handle.send(Request::set_window_frame(wid, target_frame, txid, true))
                {
                    debug!(?wid, ?e, "Failed to send frame request for hidden window");
                    continue;
                }
            }

            if let Some(window) = reactor.state.windows.window_mut(wid) {
                window.frame_monotonic = target_frame;
            }
        }

        if !anim.is_empty() {
            if let Some(tx) = &reactor.animation_tx {
                let message = if !animate_camera {
                    Message::SkipToEnd(anim)
                } else {
                    Message::Replace(anim)
                };
                if let Err(err) = tx.send(message) {
                    match err.0 {
                        Message::Replace(animation) => animation.skip_to_end(),
                        Message::SkipToEnd(animation) => animation.skip_to_end(),
                        Message::Stop(..)
                        | Message::Camera(_)
                        | Message::StopCamera(_)
                        | Message::Displays(_) => {}
                    }
                }
            } else {
                anim.skip_to_end();
            }
        }

        any_frame_changed
    }

    pub fn instant_layout(
        reactor: &mut Reactor,
        space: SpaceId,
        layout: &[(WindowId, CGRect)],
        skip_wid: Option<WindowId>,
    ) -> bool {
        Self::instant_layout_inner(reactor, space, layout, skip_wid, false, false)
    }

    /// Apply the position-only layout used while switching virtual workspaces.
    ///
    /// Keep this entry point separate from `instant_layout`: layouts merely suppressed
    /// while a switch is in progress may still change window sizes and must use the
    /// full-frame request.
    pub fn workspace_switch_layout(
        reactor: &mut Reactor,
        space: SpaceId,
        layout: &[(WindowId, CGRect)],
        skip_wid: Option<WindowId>,
    ) -> bool {
        Self::instant_layout_inner(reactor, space, layout, skip_wid, true, false)
    }

    /// Restore native geometry after a display change invalidated cached frames.
    pub fn reapply_layout(
        reactor: &mut Reactor,
        space: SpaceId,
        layout: &[(WindowId, CGRect)],
        skip_wid: Option<WindowId>,
    ) -> bool {
        Self::instant_layout_inner(reactor, space, layout, skip_wid, false, true)
    }

    fn instant_layout_inner(
        reactor: &mut Reactor,
        space: SpaceId,
        layout: &[(WindowId, CGRect)],
        skip_wid: Option<WindowId>,
        position_only: bool,
        force_frames: bool,
    ) -> bool {
        reactor.cancel_window_presentations(layout.iter().map(|(wid, _)| *wid).collect());
        let bounds = scrolling_bounds(reactor, space);
        let active = reactor.layout_manager.layout_engine.workspaces().active_workspace(space);
        let mut per_app: HashMap<(pid_t, bool), _> = HashMap::default();
        let mut any_frame_changed = false;

        for &(wid, target_frame) in layout {
            if skip_wid == Some(wid) {
                trace!(?wid, "Skipping layout update for window currently being dragged");
                continue;
            }

            let constrained = bounds.is_some()
                && reactor.state.windows.workspace_for_window(wid) == active
                && !reactor.layout_manager.layout_engine.is_window_floating(wid);
            let window_store = &mut reactor.state.windows;
            let Some(window) = window_store.window_mut(wid) else {
                debug!(?wid, "Skipping layout - window no longer exists");
                continue;
            };
            let target_frame = target_frame.round();
            let current_frame = window.frame_monotonic;
            if !force_frames && target_frame.same_as(current_frame) {
                continue;
            }
            if !force_frames
                && let Some(wsid) = window.info.sys_id
                && reactor
                    .transaction_manager
                    .get_target_frame(wsid)
                    .is_some_and(|pending| pending.same_as(target_frame))
            {
                trace!(?wid, ?target_frame, "Skipping redundant instant layout request");
                continue;
            }
            any_frame_changed = true;
            trace!(
                ?wid,
                ?current_frame,
                ?target_frame,
                "Instant workspace positioning"
            );

            let size_unchanged = !force_frames && current_frame.size.same_as(target_frame.size);
            window.frame_monotonic = target_frame;
            let (frames, positions, first_wsid) = per_app
                .entry((wid.pid, constrained))
                .or_insert_with(|| (Vec::new(), Vec::new(), None));
            *first_wsid = first_wsid.or(window.info.sys_id);
            if position_only && size_unchanged {
                positions.push((wid, target_frame));
            } else {
                frames.push((wid, target_frame));
            }
        }

        for ((pid, constrained), (frames, positions, first_wsid)) in per_app {
            let Some(app_state) = reactor.app_manager.apps.get(&pid) else {
                debug!(?pid, "Skipping layout update for app - app no longer exists");
                continue;
            };

            let handle = app_state.handle.with_scrolling_bounds(bounds.filter(|_| constrained));

            let txid = first_wsid
                .map(|wsid| reactor.transaction_manager.generate_next_txid(wsid))
                .unwrap_or_default();
            for wid in frames.iter().map(|(wid, _)| wid).chain(positions.iter().map(|(wid, _)| wid))
            {
                if let Some(window) = reactor.state.windows.window(*wid)
                    && let Some(wsid) = window.info.sys_id
                {
                    reactor.transaction_manager.store_txid(wsid, txid, window.frame_monotonic);
                }
            }
            let requests = [
                (!positions.is_empty()).then(|| {
                    Request::SetWindowFrames(
                        positions,
                        txid,
                        FrameMode::Position,
                        true,
                        Default::default(),
                    )
                }),
                (!frames.is_empty()).then(|| {
                    Request::SetWindowFrames(
                        frames,
                        txid,
                        FrameMode::Full,
                        true,
                        Default::default(),
                    )
                }),
            ];
            for request in requests.into_iter().flatten() {
                if let Err(e) = handle.send(request) {
                    debug!(
                        ?pid,
                        ?e,
                        "Failed to send instant layout request - app may have quit"
                    );
                    break;
                }
            }
        }

        any_frame_changed
    }
}

impl Motion {
    fn windows(&self) -> &[PresentedWindow] {
        match self {
            Self::Transition(a) => &a.animation.windows,
            Self::Viewport(c) => &c.windows,
        }
    }

    fn windows_mut(&mut self) -> &mut Vec<PresentedWindow> {
        match self {
            Self::Transition(a) => &mut a.animation.windows,
            Self::Viewport(c) => &mut c.windows,
        }
    }

    fn store(&self) -> Option<WindowTxStore> {
        match self {
            Self::Transition(_) => None,
            Self::Viewport(c) => Some(c.store.clone()),
        }
    }

    fn stop_windows(&mut self, windows: &[WindowId]) {
        let store = self.store();
        self.windows_mut().retain_mut(|window| {
            if windows.binary_search(&window.wid).is_ok() {
                window.cancel(store.as_ref());
                false
            } else {
                true
            }
        });
        if let Self::Viewport(c) = self
            && c.windows.is_empty()
        {
            c.active = false;
        }
    }

    fn stop(&mut self) {
        let store = self.store();
        for window in self.windows_mut().iter_mut() {
            window.cancel(store.as_ref());
        }
        self.windows_mut().clear();
        if let Self::Viewport(c) = self {
            c.active = false;
        }
    }

    fn has_work(&self) -> bool {
        match self {
            Self::Transition(a) => !a.animation.is_empty() && !a.is_complete(),
            Self::Viewport(c) => c.active,
        }
    }

    fn sample(&mut self, now: Instant, frames: &mut FrameBatch) -> Option<Duration> {
        match self {
            Self::Viewport(c) => {
                c.sample_into(now, frames);
                c.active.then_some(c.interval)
            }
            Self::Transition(a) => {
                if a.animation.is_empty() || a.is_complete() {
                    return None;
                }
                if now + Duration::from_micros(1) < a.next_sample
                    && now.saturating_duration_since(a.started) < a.animation.duration
                {
                    return Some(a.next_sample - now);
                }
                let missed = now.saturating_duration_since(a.next_sample).as_nanos()
                    / a.animation.interval.as_nanos();
                a.next_sample += a.animation.interval.mul_f64((missed + 1) as f64);
                a.send_frame(now, frames);
                (!a.is_complete()).then_some(a.animation.interval)
            }
        }
    }

    fn finish_frame(&mut self) {
        match self {
            Self::Transition(a) if a.is_complete() && !a.ended => {
                a.animation.end();
                a.ended = true;
            }
            Self::Viewport(c) if !c.active => c.end(),
            _ => {}
        }
    }

    fn complete(&mut self) {
        match self {
            Self::Transition(_) => {}
            Self::Viewport(c) => {
                c.publish_state(Instant::now());
                if let Some(events) = c.events.take() {
                    events.send(super::Event::CameraFinished);
                }
            }
        }
    }
}

impl ActiveAnimation {
    fn start(mut animation: Animation) -> Option<Self> {
        if animation.is_empty() {
            return None;
        }
        animation.windows.sort_unstable_by_key(|w| w.wid.pid);
        for window in &mut animation.windows {
            window.motion = Some(FrameSpring::new(window.from, window.to, window.velocity));
        }
        animation.begin();
        let started = Instant::now();
        let next_sample = started + animation.interval;
        Some(Self {
            animation,
            started,
            progress: 0.0,
            next_sample,
            ended: false,
        })
    }

    fn replace_with(self, mut next: Animation) -> Self {
        for window in &mut next.windows {
            if let Some(old) = self.animation.windows.iter().find(|old| old.wid == window.wid) {
                window.from = old.frame;
                window.frame = old.frame;
                window.velocity = old.velocity;
                window.leased = old.leased;
                window.proxy = old.proxy.clone();
            } else {
                window.begin();
            }
        }
        for mut old in self.animation.windows {
            if !next.handled_windows.contains(&old.wid) {
                old.from = old.frame;
                next.windows.push(old);
            } else if !next.windows.iter().any(|w| w.wid == old.wid) {
                old.cancel(None);
            }
        }
        for window in &mut next.windows {
            window.motion = Some(FrameSpring::new(window.from, window.to, window.velocity));
        }
        next.windows.sort_unstable_by_key(|w| w.wid.pid);
        let started = Instant::now();
        let next_sample = started + next.interval;
        Self {
            animation: next,
            started,
            progress: 0.0,
            next_sample,
            ended: false,
        }
    }

    fn send_frame(&mut self, now: Instant, frames: &mut FrameBatch) {
        let t = if self.animation.duration.is_zero() {
            1.0
        } else {
            (now.saturating_duration_since(self.started).as_secs_f64()
                / self.animation.duration.as_secs_f64())
            .clamp(0.0, 1.0)
        };
        let t = t.max(self.progress);
        self.animation.stage_frame(t, frames);
        self.progress = t;
    }

    fn is_complete(&self) -> bool {
        self.progress >= 1.0 && !self.animation.windows.iter().any(PresentedWindow::proxy_pending)
    }
}

impl Animation {
    fn new(fps: f64, duration: f64) -> Self {
        let interval = Duration::from_secs_f64(1.0 / fps);
        Self {
            interval,
            duration: Duration::from_secs_f64(duration),
            display: 0,
            windows: vec![],
            handled_windows: vec![],
        }
    }

    fn add_window(
        &mut self,
        handle: &AppThreadHandle,
        wid: WindowId,
        start: CGRect,
        finish: CGRect,
        txid: TransactionId,
    ) {
        self.windows.push(PresentedWindow {
            handle: handle.clone(),
            wid,
            wsid: None,
            from: start,
            to: finish,
            fixed: false,
            announced: false,
            proxy: None,
            frame: start,
            motion: None,
            velocity: FrameVelocity::default(),
            txid,
            leased: false,
        });
        self.mark_handled(wid);
    }

    fn mark_handled(&mut self, wid: WindowId) {
        if !self.handled_windows.contains(&wid) {
            self.handled_windows.push(wid);
        }
    }

    pub fn skip_to_end(&self) {
        for window in &self.windows {
            _ = window.handle.send(Request::set_window_frame(
                window.wid,
                window.to,
                window.txid,
                true,
            ));
        }
    }

    fn is_empty(&self) -> bool { self.windows.is_empty() }

    fn begin(&mut self) {
        for window in &mut self.windows {
            window.begin();
        }
    }

    fn finish_all(&mut self) {
        for window in &mut self.windows {
            if let Some(proxy) = window.proxy.take() {
                proxy.ticket.revoke();
                proxy.ticket.wake();
                proxy.ticket.retire();
            }
        }
        let mut frames = FrameBatch::default();
        self.stage_frame(1.0, &mut frames);
        frames.flush();
        self.end();
    }

    fn stage_frame(&mut self, t: f64, frames: &mut FrameBatch) {
        stage_windows(
            &mut self.windows,
            Sample::Transition {
                progress: t,
                duration: self.duration,
            },
            frames,
        );
    }

    fn end(&mut self) {
        for window in &mut self.windows {
            window.end();
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct FrameVelocity {
    origin: CGPoint,
    size: CGSize,
}

impl FrameVelocity {
    fn add(self, other: Self) -> Self {
        Self {
            origin: CGPoint::new(self.origin.x + other.origin.x, self.origin.y + other.origin.y),
            size: CGSize::new(
                self.size.width + other.size.width,
                self.size.height + other.size.height,
            ),
        }
    }

    fn is_zero(self) -> bool { self.origin == CGPoint::ZERO && self.size == CGSize::ZERO }
}

#[derive(Clone, Copy, Debug)]
struct FrameSpring {
    from: CGRect,
    to: CGRect,
    velocity: FrameVelocity,
}

impl FrameSpring {
    fn new(from: CGRect, to: CGRect, velocity: FrameVelocity) -> Self {
        Self { from, to, velocity }
    }

    fn sample(self, progress: f64, duration: Duration) -> (CGRect, FrameVelocity) {
        if progress >= 1.0 || duration.is_zero() {
            return (self.to, FrameVelocity::default());
        }
        let omega = 12.0 / duration.as_secs_f64();
        let time = progress.max(0.0) * duration.as_secs_f64();
        let envelope = (-omega * time).exp();
        let component = |from: f64, to: f64, velocity: f64| {
            let displacement = from - to;
            let b = velocity + omega * displacement;
            (
                to + (displacement + b * time) * envelope,
                (velocity - omega * b * time) * envelope,
            )
        };
        let (x, vx) = component(self.from.origin.x, self.to.origin.x, self.velocity.origin.x);
        let (y, vy) = component(self.from.origin.y, self.to.origin.y, self.velocity.origin.y);
        let (width, vw) = component(
            self.from.size.width,
            self.to.size.width,
            self.velocity.size.width,
        );
        let (height, vh) = component(
            self.from.size.height,
            self.to.size.height,
            self.velocity.size.height,
        );
        (
            CGRect::new(CGPoint::new(x, y), CGSize::new(width, height)),
            FrameVelocity {
                origin: CGPoint::new(vx, vy),
                size: CGSize::new(vw, vh),
            },
        )
    }
}

fn add_frame(a: CGRect, b: CGRect) -> CGRect {
    CGRect::new(
        CGPoint::new(a.origin.x + b.origin.x, a.origin.y + b.origin.y),
        CGSize::new(a.size.width + b.size.width, a.size.height + b.size.height),
    )
}

fn subtract_frame(a: CGRect, b: CGRect) -> CGRect {
    CGRect::new(
        CGPoint::new(a.origin.x - b.origin.x, a.origin.y - b.origin.y),
        CGSize::new(a.size.width - b.size.width, a.size.height - b.size.height),
    )
}

fn positive_size(mut frame: CGRect, velocity: &mut FrameVelocity) -> CGRect {
    if frame.size.width < 1.0 {
        frame.size.width = 1.0;
        velocity.size.width = 0.0;
    }
    if frame.size.height < 1.0 {
        frame.size.height = 1.0;
        velocity.size.height = 0.0;
    }
    frame
}

#[cfg(test)]
mod tests {
    use objc2_core_foundation::{CGPoint, CGSize};

    use super::*;

    fn rect(origin_x: f64, origin_y: f64, width: f64, height: f64) -> CGRect {
        CGRect::new(CGPoint::new(origin_x, origin_y), CGSize::new(width, height))
    }

    fn empty_animation() -> Animation {
        let settings = crate::common::config::Config::default().settings;
        Animation::new(settings.animation_fps, settings.animation_duration)
    }

    fn animation(handle: &AppThreadHandle, wid: WindowId, from: CGRect, to: CGRect) -> Animation {
        let mut animation = empty_animation();
        animation.add_window(handle, wid, from, to, TransactionId::default());
        animation
    }

    fn transition_state(manager: &AnimationManager) -> Option<&ActiveAnimation> {
        manager.motions.iter().find_map(|m| match m {
            Motion::Transition(a) if !a.is_complete() && !a.animation.is_empty() => Some(a),
            _ => None,
        })
    }
    fn active_mut(manager: &mut AnimationManager) -> Option<&mut ActiveAnimation> {
        manager.motions.iter_mut().find_map(|m| match m {
            Motion::Transition(a) => Some(a),
            _ => None,
        })
    }
    fn camera_mut(manager: &mut AnimationManager) -> Option<&mut CameraAnimation> {
        manager.motions.iter_mut().find_map(|m| match m {
            Motion::Viewport(c) => Some(c),
            _ => None,
        })
    }

    #[test]
    fn macos_motion_resizes_throughout_the_transition_and_finishes_exactly() {
        let (handle, mut rx) = AppThreadHandle::channel();
        let wid = WindowId::new(1, 1);
        let from = rect(0., 0., 400., 800.);
        let to = rect(200., 100., 800., 400.);
        let mut manager = AnimationManager::new();
        manager.handle_message(Message::Replace(animation(&handle, wid, from, to)));
        let start = transition_state(&manager).unwrap().started;
        collect_requests(&mut rx);
        let mut previous = from;
        for ms in [15, 30, 75, 150, 240] {
            manager.tick_at(start + Duration::from_millis(ms));
            let requests = collect_requests(&mut rx);
            let Observed::Frame { frame, set_size, .. } = requests[0] else {
                panic!("frame");
            };
            assert!(set_size);
            assert!(frame.size.width > previous.size.width && frame.size.width < to.size.width);
            assert!(frame.size.height < previous.size.height && frame.size.height > to.size.height);
            assert!(frame.origin.x > previous.origin.x && frame.origin.x < to.origin.x);
            previous = frame;
        }
        manager.tick_at(start + Duration::from_millis(300));
        assert_animation_frame(&collect_requests(&mut rx)[0], wid, to);
        assert!(!manager.has_work());
        let mut immediate = animation(&handle, wid, from, to);
        immediate.duration = Duration::ZERO;
        manager.handle_message(Message::Replace(immediate));
        manager.tick_at(Instant::now());
        let requests = collect_requests(&mut rx);
        assert!(requests.iter().any(|request| matches!(request,
            Observed::Frame { frame, .. } if *frame == to)));
        assert!(!manager.has_work());
    }

    #[test]
    fn macos_motion_retarget_preserves_perpendicular_and_resize_velocity() {
        let (handle, mut rx) = AppThreadHandle::channel();
        let wid = WindowId::new(1, 1);
        let from = rect(0., 0., 400., 400.);
        let mut manager = AnimationManager::new();
        manager.handle_message(Message::Replace(animation(
            &handle,
            wid,
            from,
            rect(500., 0., 800., 600.),
        )));
        let start = transition_state(&manager).unwrap().started;
        manager.tick_at(start + Duration::from_millis(50));
        collect_requests(&mut rx);
        let old = &transition_state(&manager).unwrap().animation.windows[0];
        let published = old.frame;
        let velocity = old.velocity;
        assert!(velocity.origin.x > 0. && velocity.size.width > 0.);
        manager.handle_message(Message::Replace(animation(
            &handle,
            wid,
            from,
            rect(published.origin.x, 500., 200., 700.),
        )));
        let mut frames = FrameBatch::default();
        active_mut(&mut manager).unwrap().animation.stage_frame(0., &mut frames);
        frames.flush();
        collect_requests(&mut rx);
        let next = &transition_state(&manager).unwrap().animation.windows[0];
        assert_eq!(next.frame, published);
        let epsilon = 0.0000001;
        let mut frames = FrameBatch::default();
        active_mut(&mut manager)
            .unwrap()
            .animation
            .stage_frame(epsilon / 0.3, &mut frames);
        frames.flush();
        let requests = collect_requests(&mut rx);
        let Observed::Frame { frame, .. } = requests[0] else {
            panic!("frame");
        };
        assert!(
            ((frame.origin.x - published.origin.x) / epsilon - velocity.origin.x).abs() < 0.2,
            "frame={frame:?} published={published:?} velocity={velocity:?}"
        );
        assert!(
            ((frame.size.width - published.size.width) / epsilon - velocity.size.width).abs() < 0.5
        );
        assert!(
            frame.origin.x > published.origin.x,
            "perpendicular momentum must survive"
        );
        assert!(
            frame.size.width > published.size.width,
            "width reversal must decelerate first"
        );
        let target = rect(-200., -100., 20., 50.);
        manager.handle_message(Message::Replace(animation(&handle, wid, from, target)));
        let start = transition_state(&manager).unwrap().started;
        for ms in [10, 50, 150, 300] {
            manager.tick_at(start + Duration::from_millis(ms));
            for request in collect_requests(&mut rx) {
                if let Observed::Frame { frame, .. } = request {
                    assert!(frame.size.width >= 1. && frame.size.height >= 1.);
                }
            }
        }
        assert!(!manager.has_work());
        let completed = &manager.motions[0];
        assert_eq!(completed.windows()[0].frame, target);
    }

    pub(super) fn motion_camera(
        handle: &AppThreadHandle,
        from: CGRect,
        to: CGRect,
    ) -> CameraAnimation {
        use crate::layout_engine::systems::ScrollingLayoutSystem;
        let wid = WindowId::new(1, 1);
        let mut system =
            ScrollingLayoutSystem::new(&crate::common::config::ScrollingLayoutSettings {
                alignment: crate::common::config::ScrollingAlignment::Left,
                column_width_ratio: 1.0,
                max_column_width_ratio: 1.0,
                ..Default::default()
            });
        let layout = system.create_layout();
        system.add_window_after_selection(layout, wid);
        system.prepare_layout(
            layout,
            rect(0., 0., 1000., 800.),
            &Default::default(),
            &Default::default(),
        );
        let presentation = system.presentation(layout).unwrap().0;
        let now = Instant::now();
        let state = Arc::new(Mutex::new(PresentedCamera {
            viewport: presentation.snapshot(now),
            gesture: None,
            active: true,
            stopped: false,
        }));
        CameraAnimation {
            presentation,
            windows: vec![PresentedWindow {
                handle: handle.clone(),
                wid,
                wsid: None,
                from,
                to,
                frame: from,
                motion: None,
                velocity: FrameVelocity::default(),
                txid: TransactionId::default(),
                fixed: false,
                leased: false,
                announced: false,
                proxy: None,
            }],
            geometry_started: now,
            geometry_progress: 0.,
            duration: Duration::from_millis(300),
            gesture: None,
            active: true,
            identity: CameraIdentity {
                space: SpaceId::new(1),
                workspace: VirtualWorkspaceId::default(),
                layout,
            },
            events: None,
            state,
            display: 1,
            animate: true,
            scale: 1.,
            store: WindowTxStore::new(),
            interval: Duration::from_secs_f64(1. / 120.),
            bound: None,
        }
    }

    #[test]
    fn macos_motion_viewport_resize_has_no_eager_jump_or_stale_rewind() {
        let (handle, mut rx) = AppThreadHandle::channel();
        let from = rect(0., 0., 400., 400.);
        let to = rect(0., 0., 800., 600.);
        let mut camera = motion_camera(&handle, from, to);
        camera.replace(None);
        let start = camera.geometry_started;
        camera.begin();
        assert!(collect_requests(&mut rx).iter().all(|request| matches!(
            request,
            Observed::Request(Request::BeginWindowAnimation(..))
        )));
        camera.sample(start);
        assert_eq!(camera.windows[0].frame, from);
        collect_requests(&mut rx);
        camera.sample(start + Duration::from_millis(30));
        let first = camera.windows[0].frame;
        assert!(first.size.width > from.size.width && first.size.width < to.size.width);
        assert!(camera.active, "stationary camera must keep a resize alive");
        camera.sample(start + Duration::from_millis(15));
        assert_eq!(
            camera.windows[0].frame, first,
            "old ticks cannot rewind a resize"
        );
        camera.sample(start + Duration::from_millis(150));
        assert!(camera.windows[0].frame.size.width > first.size.width);
        camera.sample(start + Duration::from_millis(300));
        assert_eq!(camera.windows[0].frame, to);
        assert!(!camera.active);
        camera.duration = Duration::ZERO;
        camera.active = true;
        camera.windows[0].to = from;
        camera.replace(None);
        camera.sample(camera.geometry_started);
        assert_eq!(camera.windows[0].frame, from);
        assert!(!camera.active);
    }

    #[test]
    fn macos_motion_viewport_retarget_preserves_resize_velocity() {
        let (handle, mut rx) = AppThreadHandle::channel();
        let from = rect(0., 0., 400., 400.);
        let mut old = motion_camera(&handle, from, rect(100., 0., 800., 600.));
        old.replace(None);
        old.begin();
        old.sample(old.geometry_started + Duration::from_millis(50));
        collect_requests(&mut rx);
        let published = old.windows[0].frame;
        let velocity = old.windows[0].velocity;
        let mut next = motion_camera(&handle, from, rect(0., 100., 200., 700.));
        next.replace(Some(old));
        next.begin();
        let start = next.geometry_started;
        next.sample(start);
        assert_eq!(next.windows[0].frame, published);
        next.sample(start + Duration::from_nanos(100));
        let frame = next.windows[0].frame;
        assert!(((frame.origin.x - published.origin.x) * 1e7 - velocity.origin.x).abs() < 0.2);
        assert!(
            ((frame.size.width - published.size.width) * 1e7 - velocity.size.width).abs() < 0.5,
            "frame={frame:?} published={published:?} velocity={velocity:?}"
        );
        next.sample(start + Duration::from_secs(1));
        assert_eq!(next.windows[0].frame, rect(0., 100., 200., 700.));
        assert!(!next.active);
    }

    #[test]
    fn macos_motion_resizing_during_camera_motion_preserves_screen_velocity() {
        let (handle, mut rx) = AppThreadHandle::channel();
        let mut old = motion_camera(&handle, rect(0., 0., 400., 400.), rect(0., 0., 800., 600.));
        let now = Instant::now();
        old.presentation.retarget(-500., 1200., now);
        old.replace(None);
        old.begin();
        old.sample(old.geometry_started + Duration::from_millis(50));
        collect_requests(&mut rx);
        let published = old.windows[0].frame;
        let velocity = old.windows[0].velocity;
        assert_ne!(old.presentation.position_velocity(now).1, 0.);
        let mut next = motion_camera(&handle, published, rect(0., 0., 1000., 700.));
        next.replace(Some(old));
        next.begin();
        let start = next.geometry_started;
        next.sample(start);
        assert_eq!(next.windows[0].frame, published);
        assert!((next.windows[0].velocity.origin.x - velocity.origin.x).abs() < 1e-6);
        next.sample(start + Duration::from_millis(1));
        let frame = next.windows[0].frame;
        assert!(((frame.origin.x - published.origin.x) - velocity.origin.x * 0.001).abs() < 2.);
        assert!(
            ((frame.size.width - published.size.width) - velocity.size.width * 0.001).abs() < 2.
        );
        next.sample(start + Duration::from_secs(2));
        assert_eq!(
            next.windows[0].frame,
            next.presentation.target_frame(next.windows[0].to, false, next.scale)
        );
        assert!(!next.active);
    }

    #[test]
    fn retained_frame_batch_accepts_a_fresh_producer_for_the_same_app_actor() {
        let (events, _) = crate::actor::channel();
        let (wm, _) = crate::actor::channel();
        let (spaces, sender) = crate::actor::spaces::SpacesActor::new_for_tests(events, wm);
        let guard = spaces.topology_guard();
        let (mut handle, mut requests) = AppThreadHandle::channel();
        handle.set_topology_authority(&guard, 0);
        let old = handle.clone();
        let wid = WindowId::new(1, 1);
        let frame = rect(1., 2., 300., 400.);
        let mut batch = FrameBatch::default();
        let sample =
            || std::iter::once((wid, frame, true, Default::default(), FrameSource::Viewport));
        batch.extend(&old, wid.pid, sample());
        batch.flush();
        collect_requests(&mut requests);
        sender.send(crate::actor::spaces::Event::SystemWillSleep);
        let mut running = std::pin::pin!(spaces.run());
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(std::future::Future::poll(running.as_mut(), &mut context).is_pending());
        handle.set_topology_authority(&guard, guard.invalidated_revision());
        assert!(old.same_actor(&handle));
        batch.extend(&handle, wid.pid, sample());
        // A late old motion must not replace or contaminate the new batch.
        batch.extend(&old, wid.pid, sample());
        batch.flush();
        let frames = collect_requests(&mut requests);
        assert!(
            matches!(frames.as_slice(), [Observed::Frame { wid: id, frame: actual, .. }]
            if *id == wid && *actual == frame)
        );
    }

    #[derive(Debug)]
    enum Observed {
        Frame {
            wid: WindowId,
            frame: CGRect,
            set_size: bool,
            txid: TransactionId,
        },
        Request(Request),
    }
    fn collect_requests(rx: &mut crate::actor::Receiver<Request>) -> Vec<Observed> {
        let mut requests = Vec::new();
        while let Ok((_, request)) = rx.try_recv() {
            if let Request::InteractiveFramesPending(queue) = request {
                queue.drain_with(|wid, frame, set_size, txid, _, _, _| {
                    requests.push(Observed::Frame { wid, frame, set_size, txid })
                });
            } else {
                requests.push(Observed::Request(request));
            }
        }
        requests
    }

    fn assert_set_window_frame(request: &Observed, wid: WindowId, frame: CGRect) {
        match request {
            Observed::Request(Request::SetWindowFrames(frames, txid, FrameMode::Full, eui, _)) => {
                let (req_wid, req_frame) = frames[0];
                assert_eq!(req_wid, wid);
                assert_eq!(req_frame, frame);
                assert_eq!(*txid, TransactionId::default());
                assert!(*eui);
            }
            _ => panic!("expected SetWindowFrame, got {request:?}"),
        }
    }

    fn assert_animation_frame(request: &Observed, wid: WindowId, frame: CGRect) {
        match request {
            Observed::Frame {
                wid: req_wid,
                frame: req_frame,
                set_size,
                txid,
            } => {
                assert_eq!(*req_wid, wid);
                assert_eq!(*req_frame, frame);
                assert!(*set_size, "expected a set_size frame");
                assert_eq!(*txid, TransactionId::default());
            }
            _ => panic!("expected coalesced frame, got {request:?}"),
        }
    }

    fn assert_animation_pos(request: &Observed, wid: WindowId, pos: CGPoint) {
        match request {
            Observed::Frame {
                wid: req_wid,
                frame,
                set_size,
                txid,
            } => {
                assert_eq!(*req_wid, wid);
                assert_eq!(frame.origin, pos);
                assert!(!*set_size, "expected a position-only frame");
                assert_eq!(*txid, TransactionId::default());
            }
            _ => panic!("expected coalesced frame, got {request:?}"),
        }
    }

    #[test]
    fn native_and_fallback_sampling_use_elapsed_time_and_finish_exactly() {
        let start = Instant::now();
        let mut results = Vec::new();
        for fps in [60.0, 120.0] {
            let (tx, mut rx) = crate::actor::channel();
            let handle = AppThreadHandle::new_for_test(tx);
            let mut a = Animation::new(fps, 0.3);
            a.add_window(
                &handle,
                WindowId::new(1, 1),
                rect(0.0, 0.0, 10.0, 10.0),
                rect(100.0, 50.0, 20.0, 30.0),
                TransactionId::default(),
            );
            let mut manager = AnimationManager::new();
            manager.handle_message(Message::Replace(a));
            active_mut(&mut manager).unwrap().started = start;
            collect_requests(&mut rx);
            let mut frames = Vec::new();
            for ms in [40, 150, 300] {
                manager.tick_at(start + Duration::from_millis(ms));
                frames.extend(collect_requests(&mut rx).into_iter().filter_map(|r| match r {
                    Observed::Frame { frame, .. } => Some(frame),
                    _ => None,
                }));
                // A duplicate wake must not advance time or replay a frame.
                manager.tick_at(start + Duration::from_millis(ms));
                assert!(collect_requests(&mut rx).is_empty());
            }
            assert_eq!(frames.len(), 3);
            assert!(frames[1].origin.x > 50.0 && frames[1].origin.x < 100.0);
            assert_eq!(frames[1].origin.y, frames[1].origin.x / 2.0);
            assert!(frames[0].size.width > 10.0 && frames[0].size.width < frames[1].size.width);
            assert!(frames[1].size.width < 20.0);
            assert_eq!(frames[2], rect(100.0, 50.0, 20.0, 30.0));
            assert!(transition_state(&manager).is_none());
            results.push(frames);
        }
        assert_eq!(results[0], results[1]);
    }

    #[test]
    fn native_ticks_gate_generic_fps_skip_missed_intervals_and_keep_displays_independent() {
        let start = Instant::now();
        for (fps, expected) in [(60.0, 60), (30.0, 30)] {
            let (handle, mut rx) = AppThreadHandle::channel();
            let mut a = Animation::new(fps, 1.0);
            let wid = WindowId::new(1, 1);
            a.add_window(
                &handle,
                wid,
                rect(0.0, 0.0, 10.0, 10.0),
                rect(1000.0, 0.0, 10.0, 10.0),
                TransactionId::default(),
            );
            let mut manager = AnimationManager::new();
            manager.handle_message(Message::Replace(a));
            let active = active_mut(&mut manager).unwrap();
            active.started = start;
            active.next_sample = start + active.animation.interval;
            collect_requests(&mut rx);
            let mut count = 0;
            for tick in 1..=120 {
                manager.tick_at(start + Duration::from_secs_f64(tick as f64 / 120.0));
                count += collect_requests(&mut rx)
                    .iter()
                    .filter(|r| matches!(r, Observed::Frame { .. }))
                    .count();
            }
            assert_eq!(count, expected);
            assert!(!manager.has_work());
        }
        let (handle, mut rx) = AppThreadHandle::channel();
        let wid = WindowId::new(1, 1);
        let mut a = animation(
            &handle,
            wid,
            rect(0.0, 0.0, 10.0, 10.0),
            rect(1000.0, 0.0, 10.0, 10.0),
        );
        a.interval = Duration::from_millis(20);
        a.duration = Duration::from_secs(1);
        let mut manager = AnimationManager::new();
        manager.handle_message(Message::Replace(a));
        let active = active_mut(&mut manager).unwrap();
        active.started = start;
        active.next_sample = start;
        collect_requests(&mut rx);
        manager.tick_at(start + Duration::from_millis(200));
        let frames = collect_requests(&mut rx);
        assert_eq!(frames.len(), 1, "missed frames must not replay");
        let Observed::Frame { frame: last, .. } = frames[0] else {
            panic!("frame");
        };
        manager.handle_message(Message::Replace(animation(
            &handle,
            wid,
            last,
            rect(2000.0, 0.0, 10.0, 10.0),
        )));
        assert_eq!(
            transition_state(&manager).unwrap().animation.windows[0].from,
            last
        );
        manager.tick_at(transition_state(&manager).unwrap().next_sample - Duration::from_millis(1));
        assert!(
            collect_requests(&mut rx).is_empty(),
            "old timestamps cannot sample a new transition"
        );
    }

    #[test]
    fn completed_camera_keeps_its_offset_after_native_window_clamping() {
        use crate::common::config::{ScrollingAlignment, ScrollingLayoutSettings};
        use crate::layout_engine::systems::ScrollingLayoutSystem;
        use crate::layout_engine::{Direction, ResizeOrientation};

        let (handle, _rx) = AppThreadHandle::channel();
        let hidden = WindowId::new(1, 1);
        let visible = WindowId::new(1, 2);
        let mut system = ScrollingLayoutSystem::new(&ScrollingLayoutSettings {
            column_width_ratio: 1.0,
            max_column_width_ratio: 1.0,
            alignment: ScrollingAlignment::Left,
            preserve_window_sizes: false,
            ..Default::default()
        });
        let layout = system.create_layout();
        system.add_window_after_selection(layout, hidden);
        system.add_window_after_selection(layout, visible);
        system.select_window(layout, hidden);
        system.prepare_layout(
            layout,
            rect(0.0, 0.0, 1000.0, 800.0),
            &Default::default(),
            &Default::default(),
        );
        let make_camera = |system: &ScrollingLayoutSystem,
                           native: &[(WindowId, CGRect)],
                           control: &PresenterControl| {
            let (presentation, frames) = system.presentation(layout).unwrap();
            let state = Arc::new(Mutex::new(PresentedCamera {
                viewport: presentation.snapshot(Instant::now()),
                gesture: None,
                active: true,
                stopped: false,
            }));
            CameraAnimation {
                presentation,
                geometry_started: Instant::now(),
                geometry_progress: 0.0,
                duration: Duration::from_secs_f64(0.3),
                windows: frames
                    .into_iter()
                    .map(|(wid, world, fixed)| {
                        let frame =
                            control.frame(wid, native.iter().find(|(id, _)| *id == wid).unwrap().1);
                        PresentedWindow {
                            handle: handle.clone(),
                            wid,
                            wsid: None,
                            from: frame,
                            to: world,
                            fixed,
                            announced: false,
                            proxy: None,
                            leased: false,
                            frame,
                            motion: None,
                            velocity: FrameVelocity::default(),
                            txid: TransactionId::default(),
                        }
                    })
                    .collect(),
                gesture: None,
                active: true,
                identity: CameraIdentity {
                    space: SpaceId::new(1),
                    workspace: VirtualWorkspaceId::default(),
                    layout,
                },
                events: None,
                state,
                display: 1,
                animate: true,
                scale: 1.0,
                store: WindowTxStore::new(),
                interval: Duration::from_secs_f64(1.0 / 60.0),
                bound: None,
            }
        };
        let mut control = PresenterControl::default();
        let native = vec![
            (hidden, rect(0.0, 0.0, 1000.0, 800.0)),
            (visible, rect(1000.0, 0.0, 1000.0, 800.0)),
        ];
        system.select_window(layout, visible);
        let mut manager = AnimationManager::new();
        let camera = make_camera(&system, &native, &control);
        let state = camera.state.clone();
        manager.handle_message(Message::Camera(Box::new(camera)));
        assert_eq!(camera_mut(&mut manager).unwrap().presentation.offset(), 0.0);
        let started = Instant::now();
        manager.tick_at(started + Duration::from_millis(50));
        let offset = camera_mut(&mut manager).unwrap().presentation.offset();
        assert!(
            offset > 0.0 && offset < 1000.0,
            "focus changes must still animate"
        );
        manager.tick_at(started + Duration::from_secs(2));
        system.commit_presented_viewport(layout, &state.lock().viewport);
        manager.publish_frames(&mut control, false);
        manager.retire();
        assert!(manager.motions.is_empty());

        // The completed animation parked the first window at -1000. macOS
        // reports it 40 points farther right. A click in the selected window
        // must not turn that native correction into a new viewport position.
        let native = vec![
            (hidden, rect(-960.0, 0.0, 1000.0, 800.0)),
            (visible, rect(0.0, 0.0, 1000.0, 800.0)),
        ];
        system.select_window(layout, visible);
        manager.handle_message(Message::Camera(Box::new(make_camera(
            &system, &native, &control,
        ))));
        assert_eq!(
            camera_mut(&mut manager).unwrap().presentation.offset(),
            1000.0,
            "an unchanged viewport must keep its completed offset"
        );
        manager.tick_at(Instant::now() + Duration::from_millis(16));
        let camera = camera_mut(&mut manager).unwrap();
        assert_eq!(
            camera.windows.iter().find(|w| w.wid == visible).unwrap().frame,
            rect(0.0, 0.0, 1000.0, 800.0)
        );
        assert_eq!(camera.presentation.offset(), 1000.0);
        assert!(!manager.has_work(), "a click must not start another scroll");

        // Reorders commit immediately, but both windows start at their last
        // publication, including when another reorder interrupts the swap.
        let Motion::Viewport(mut old) = manager.motions.pop().unwrap() else {
            panic!("viewport");
        };
        for direction in [Direction::Left, Direction::Right] {
            let before: Vec<_> = old.windows.iter().map(|w| (w.wid, w.frame)).collect();
            assert!(system.move_selection(layout, direction));
            let (_, targets) = system.presentation(layout).unwrap();
            let moved = targets.iter().find(|(wid, _, _)| *wid == visible).unwrap().1;
            let neighbor = targets.iter().find(|(wid, _, _)| *wid == hidden).unwrap().1;
            assert_eq!(moved.origin.x < neighbor.origin.x, direction == Direction::Left);
            let started = Instant::now();
            let mut next = make_camera(&system, &before, &control);
            next.replace(Some(old));
            next.begin();
            next.sample(started);
            for window in &next.windows {
                assert_eq!(
                    window.frame.origin,
                    before.iter().find(|(id, _)| *id == window.wid).unwrap().1.origin
                );
                assert_eq!(window.frame.size, window.to.size);
            }
            next.sample(started + Duration::from_millis(40));
            assert!(next.active);
            let neighbor = next.windows.iter().find(|window| window.wid == hidden).unwrap();
            assert_ne!(
                neighbor.frame.origin,
                before.iter().find(|(id, _)| *id == hidden).unwrap().1.origin
            );
            old = next;
        }
        old.sample(Instant::now() + Duration::from_secs(2));
        assert!(!old.active);
        for window in &old.windows {
            assert_eq!(
                window.frame,
                old.presentation.target_frame(window.to, window.fixed, old.scale)
            );
            assert!(!window.leased);
        }
        manager.motions.push(Motion::Viewport(old));
        manager.publish_frames(&mut control, false);
        manager.retire();
        assert!(manager.motions.is_empty());
        let before: Vec<_> =
            control.frames.iter().map(|(wid, (_, frame))| (*wid, *frame)).collect();
        assert!(system.move_selection(layout, Direction::Left));
        let started = Instant::now();
        let mut next = make_camera(&system, &before, &control);
        next.replace(None);
        next.sample(started);
        for window in &next.windows {
            assert_eq!(
                window.frame.origin,
                before.iter().find(|(id, _)| *id == window.wid).unwrap().1.origin
            );
        }

        // Unequal widths: move the first of three columns past its neighbor.
        next.sample(Instant::now() + Duration::from_secs(2));
        system.commit_presented_viewport(layout, &next.state.lock().viewport);
        system.resize_selection_by(layout, -0.4, ResizeOrientation::Horizontal);
        system.add_window_after_selection(layout, WindowId::new(1, 3));
        system.select_window(layout, visible);
        let before: Vec<_> =
            crate::layout_engine::systems::scrolling::tests::presented_frames(&system, layout)
                .collect();
        let mut old = make_camera(&system, &before, &PresenterControl::default());
        old.replace(None);
        old.sample(Instant::now() + Duration::from_secs(2));
        system.commit_presented_viewport(layout, &old.state.lock().viewport);
        let before: Vec<_> = old.windows.iter().map(|w| (w.wid, w.frame)).collect();
        assert!(system.move_selection(layout, Direction::Right));
        assert_eq!(system.window_slot(layout, visible), Some(vec![1, 0]));
        let started = Instant::now();
        let mut next = make_camera(&system, &before, &PresenterControl::default());
        next.replace(Some(old));
        next.sample(started);
        for window in &next.windows {
            assert_eq!(
                window.frame,
                before.iter().find(|(id, _)| *id == window.wid).unwrap().1
            );
        }
        next.sample(started + Duration::from_millis(40));
        let neighbor = next.windows.iter().find(|w| w.wid == WindowId::new(1, 3)).unwrap();
        assert!(
            neighbor.frame.origin.x
                < before.iter().find(|(id, _)| *id == neighbor.wid).unwrap().1.origin.x
        );

        // Removing a preceding column rebases an in-flight camera. Preserve
        // both its position in the new coordinates and its ongoing velocity.
        system.commit_presented_viewport(layout, &next.state.lock().viewport);
        system.remove_window(WindowId::new(1, 3));
        system.scroll_by_delta(layout, 0.1);
        let mut replacement = make_camera(&system, &before, &PresenterControl::default());
        assert!(!replacement.presentation.animated());
        let delta = replacement.presentation.offset() - next.presentation.offset();
        assert_eq!(delta, -1000.0);
        let (position, velocity) = next.presentation.position_velocity(started);
        assert_ne!(velocity, 0.0);
        replacement.replace(Some(next));
        let handoff = replacement.presentation.snapshot(started);
        assert!((handoff.offset - position - delta).abs() < 1e-9);
        assert!((handoff.velocity - velocity).abs() < 1e-9);
    }

    #[test]
    fn generic_camera_drag_handoffs_fence_old_frames_and_retire_work() {
        use crate::layout_engine::systems::ScrollingLayoutSystem;
        let (handle, mut rx) = AppThreadHandle::channel();
        let wid = WindowId::new(1, 1);
        let mut manager = AnimationManager::new();
        manager.handle_message(Message::Replace(animation(
            &handle,
            wid,
            rect(0.0, 0.0, 10.0, 10.0),
            rect(100.0, 0.0, 10.0, 10.0),
        )));
        collect_requests(&mut rx);
        let now = transition_state(&manager).unwrap().started + Duration::from_millis(100);
        manager.tick_at(now);
        let Request::InteractiveFramesPending(old_wake) = rx.try_recv().unwrap().1 else {
            panic!("wake");
        };
        let mut system = ScrollingLayoutSystem::new(&Default::default());
        let layout = system.create_layout();
        system.add_window_after_selection(layout, wid);
        system.prepare_layout(
            layout,
            rect(0.0, 0.0, 1000.0, 800.0),
            &Default::default(),
            &Default::default(),
        );
        system.begin_viewport_gesture(layout, now);
        let store = WindowTxStore::new();
        let wsid = WindowServerId::new(1);
        let camera = CameraAnimation {
            presentation: system.presentation(layout).unwrap().0,
            geometry_started: Instant::now(),
            geometry_progress: 0.0,
            duration: Duration::from_secs_f64(0.3),
            windows: vec![PresentedWindow {
                handle: handle.clone(),
                wid,
                wsid: Some(wsid),
                from: rect(0.0, 0.0, 10.0, 10.0),
                to: system.presentation(layout).unwrap().1[0].1,
                fixed: false,
                announced: false,
                proxy: None,
                leased: false,
                frame: rect(0.0, 0.0, 10.0, 10.0),
                motion: None,
                velocity: FrameVelocity::default(),
                txid: TransactionId::default(),
            }],
            gesture: None,
            active: true,
            identity: CameraIdentity {
                space: SpaceId::new(1),
                workspace: VirtualWorkspaceId::default(),
                layout,
            },
            events: None,
            state: Arc::new(Mutex::new(PresentedCamera {
                viewport: system.presentation(layout).unwrap().0.snapshot(now),
                gesture: None,
                active: true,
                stopped: false,
            })),
            display: 1,
            animate: false,
            scale: 2.0,
            store: store.clone(),
            interval: Duration::from_secs_f64(1.0 / 60.0),
            bound: None,
        };
        manager.handle_message(Message::Camera(Box::new(camera)));
        assert!(transition_state(&manager).is_none());
        let Request::CancelWindowAnimation(_, through, _) = rx.try_recv().unwrap().1 else {
            panic!("cancel");
        };
        assert!(
            matches!(rx.try_recv().unwrap().1, Request::BeginWindowAnimation(id, _) if id == wid)
        );
        // A native camera frame reuses the old outstanding generic wake.
        camera_mut(&mut manager).unwrap().windows[0].to.origin.x += 0.5;
        camera_mut(&mut manager).unwrap().sample(now);
        assert!(rx.try_recv().is_err());
        old_wake.drain_with(|id, frame, _, _, source, sequence, _| {
            assert_eq!(id, wid);
            assert_eq!(source, FrameSource::Viewport);
            assert!(sequence > through);
            assert_eq!(
                frame.origin.x.fract().abs(),
                0.5,
                "camera must use its cached 2x scale"
            );
        });
        let presentation_txid = store.last_txid(&wsid);
        // Camera work remains eligible on consecutive 120Hz input refreshes.
        for tick in 1..=2 {
            let camera = camera_mut(&mut manager).unwrap();
            camera.presentation.update(0.001, Duration::from_millis(tick * 8));
            camera.sample(now + Duration::from_secs_f64(tick as f64 / 120.0));
            assert_eq!(collect_requests(&mut rx).len(), 1);
            assert_eq!(
                store.last_txid(&wsid),
                presentation_txid,
                "display sampling cannot advance transactions"
            );
            assert_eq!(
                store.get(&wsid).unwrap().target,
                None,
                "free gestures have no semantic target"
            );
        }
        camera_mut(&mut manager)
            .unwrap()
            .presentation
            .update(0.001, Duration::from_millis(30));
        camera_mut(&mut manager).unwrap().sample(now + Duration::from_millis(30));
        let Request::InteractiveFramesPending(old_wake) = rx.try_recv().unwrap().1 else {
            panic!("wake");
        };
        manager.handle_message(Message::Stop(vec![wid]));
        let Request::CancelWindowAnimation(_, through, _) = rx.try_recv().unwrap().1 else {
            panic!("cancel");
        };
        let dragged = rect(12.0, 34.0, 10.0, 10.0);
        handle.send_interactive_frame(
            wid,
            dragged,
            false,
            TransactionId::default(),
            FrameSource::Drag,
        );
        camera_mut(&mut manager).unwrap().sample(now + Duration::from_secs(1));
        assert!(!manager.has_work());
        assert!(rx.try_recv().is_err());
        old_wake.drain_with(|_, frame, _, _, source, sequence, _| {
            assert_eq!(frame, dragged);
            assert_eq!(source, FrameSource::Drag);
            assert!(sequence > through);
        });
        system.cancel_viewport_gesture(layout);
        {
            let c = camera_mut(&mut manager).unwrap();
            let (presentation, frames) = system.presentation(layout).unwrap();
            c.presentation = presentation;
            let mut base_frame = frames[0].1;
            base_frame.origin.x += 0.5;
            c.windows.push(PresentedWindow {
                handle,
                wid,
                wsid: None,
                from: dragged,
                to: base_frame,
                fixed: false,
                announced: false,
                proxy: None,
                leased: false,
                frame: dragged,
                motion: None,
                velocity: FrameVelocity::default(),
                txid: TransactionId::default(),
            });
            c.active = true;
            c.begin();
            collect_requests(&mut rx);
            c.sample(now);
            assert!(!c.active, "instant target must retire immediately");
        }
        let requests = collect_requests(&mut rx);
        let Observed::Frame { frame, .. } = requests[0] else {
            panic!("frame");
        };
        assert_eq!(frame.origin.x.fract().abs(), 0.5);
        assert!(
            matches!(requests[1], Observed::Request(Request::EndWindowAnimation(id, _)) if id == wid)
        );

        // A completed camera must wake semantic reconciliation and leave the
        // presenter even when no new reactor command arrives.
        let idx = manager.motions.iter().position(|m| matches!(m, Motion::Viewport(_))).unwrap();
        let Motion::Viewport(mut camera) = manager.motions.swap_remove(idx) else {
            panic!("viewport");
        };
        // Removing completed cameras must not make the next focus jump instant.
        system.scroll_by_delta(layout, 1.0);
        camera.presentation = system.presentation(layout).unwrap().0;
        camera.animate = true;
        camera.active = true;
        let before = camera.windows[0].frame;
        let restarted = Instant::now();
        camera.replace(None);
        assert_eq!(
            camera.presentation.frame_at_offset(
                camera.windows[0].to,
                false,
                2.0,
                camera.presentation.offset()
            ),
            before
        );
        let target = camera.presentation.target();
        let from = camera.presentation.offset();
        assert_ne!(from, target, "next camera must start at the displayed position");
        camera.sample(restarted + Duration::from_millis(50));
        let current = camera.presentation.offset();
        assert!(
            (current - from) * (target - from) > 0.0 && (target - current) * (target - from) > 0.0
        );
        camera.sample(restarted + Duration::from_secs(1));
        assert!(!camera.active);
        assert_eq!(camera.presentation.offset(), target);
        collect_requests(&mut rx);
        camera.animate = false;
        camera.active = true;
        let expected_final = camera.windows[0].frame;
        let (sender, commands_rx) = AnimationSender::channel();
        let (events, mut events_rx) = crate::actor::channel();
        camera.events = Some(events);
        let state = camera.state.clone();
        state.lock().active = true;
        let runner = std::thread::spawn(move || AnimationManager::run(commands_rx));
        sender.send(Message::Camera(Box::new(camera))).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while state.lock().active && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(
            !state.lock().active,
            "completion cannot depend on another reactor event"
        );
        assert_eq!(sender.control.lock().frames[&wid].1, expected_final);
        drop(sender);
        runner.join().unwrap();
        assert!(matches!(
            events_rx.try_recv().unwrap().1,
            super::super::Event::CameraFinished
        ));
        let terminal = collect_requests(&mut rx);
        assert!(matches!(terminal.as_slice(), [
            Observed::Request(Request::BeginWindowAnimation(_, _)),
            Observed::Frame { frame, .. },
            Observed::Request(Request::EndWindowAnimation(_, _))
        ] if *frame == expected_final));
    }

    #[test]
    fn display_transfer_resets_scrolling_frames_and_preserves_tiling() {
        use wm_protocol::DisplaySelector;

        use super::super::testing::*;
        use super::super::{Command, Event, ReactorCommand};
        use crate::common::config::LayoutMode;
        use crate::layout_engine::LayoutCommand;

        let (mut apps, mut reactor) = test_context();
        let spaces = [SpaceId::new(1), SpaceId::new(2)];
        let screens = [rect(0., 0., 1000., 800.), rect(0., -1000., 1000., 800.)];
        reactor.handle_event(space_state_event(screens.to_vec(), spaces.map(Some).to_vec()));
        apps.make_app_and_settle(&mut reactor, 1, make_windows(2));
        for space in spaces {
            reactor.space_state.command_space = Some(space);
            reactor.handle_test_layout_command(LayoutCommand::SetWorkspaceLayout {
                workspace: None,
                mode: LayoutMode::Scrolling,
            });
        }
        let wid = WindowId::new(1, 1);
        let (sender, mut receiver) = AnimationSender::channel();
        reactor.animation_tx = Some(sender);
        let handle = reactor.app_manager.apps[&1].handle.clone();
        for turn in 0..4 {
            let source = turn % 2;
            let target = 1 - source;
            reactor.space_state.command_space = Some(spaces[source]);
            let retained = rect(20., screens[source].origin.y + 30., 400., 500.);
            reactor
                .animation_tx
                .as_ref()
                .unwrap()
                .control
                .lock()
                .frames
                .insert(wid, (handle.clone(), retained));
            // Within a Space, a new camera must preserve the last publication.
            reactor.present_camera(spaces[source], true, None, None).unwrap();
            let Message::Camera(source_camera) = receiver.commands.try_recv().unwrap() else {
                panic!("expected source camera");
            };
            assert_eq!(
                source_camera.windows.iter().find(|w| w.wid == wid).unwrap().frame,
                retained
            );
            // Leave older work queued across the cancellation fence.
            reactor
                .animation_tx
                .as_ref()
                .unwrap()
                .send(Message::Camera(source_camera))
                .unwrap();
            let uuid = reactor
                .space_state
                .screen_by_space(spaces[target])
                .unwrap()
                .display_uuid
                .clone();
            reactor.handle_event(Event::Command(Command::Reactor(
                ReactorCommand::MoveWindowToDisplay {
                    selector: if turn % 2 == 0 {
                        DisplaySelector::Uuid(uuid)
                    } else {
                        DisplaySelector::Direction(wm_protocol::Direction::Down)
                    },
                    window_id: Some(1),
                },
            )));
            let queued: Vec<_> = receiver.commands.try_iter().collect();
            let stop = queued
                .iter()
                .position(|m| matches!(m, Message::Stop(ids) if ids == &[wid]))
                .expect("transfer must fence the source presentation");
            let destination = queued
                .iter()
                .position(|m| matches!(m, Message::Camera(c) if c.identity.space == spaces[target]))
                .expect("destination camera");
            assert!(stop < destination);
            let Message::Camera(camera) = &queued[destination] else {
                unreachable!()
            };
            let window = camera.windows.iter().find(|w| w.wid == wid).unwrap();
            assert_eq!(
                window.frame, window.from,
                "start from the destination transfer frame"
            );
            assert!(window.frame.origin.y >= screens[target].origin.y);
            assert!(window.frame.origin.y < screens[target].max().y);
            let wsid = reactor.state.windows.window(wid).unwrap().info.sys_id.unwrap();
            reactor.handle_event(Event::WindowServerAppeared(
                wsid,
                spaces[source],
                super::super::SpaceEventKind::User,
            ));
            assert_eq!(reactor.assigned_space_for_window_id(wid), Some(spaces[target]));
            for (index, space) in spaces.into_iter().enumerate() {
                let gaps = reactor.config.settings.layout.gaps.clone();
                let layout = reactor.layout_manager.layout_engine.calculate_layout(
                    space,
                    screens[index],
                    &gaps,
                    Default::default(),
                );
                assert_eq!(layout.iter().any(|(id, _)| *id == wid), index == target);
            }
            let state = camera.state.clone();
            for message in queued {
                reactor.animation_tx.as_ref().unwrap().send(message).unwrap();
            }
            let runner = std::thread::spawn(move || AnimationManager::run(receiver));
            let deadline = Instant::now() + Duration::from_secs(2);
            while state.lock().active && Instant::now() < deadline {
                std::thread::yield_now();
            }
            assert!(!state.lock().active, "destination camera must run after Stop");
            let control = reactor.animation_tx.as_ref().unwrap().control.lock();
            assert!(!control.cancelled.contains(&wid));
            assert!(control.frames[&wid].1.origin.y < screens[target].max().y);
            drop(control);
            reactor.animation_tx.take();
            runner.join().unwrap();
            let (sender, next_receiver) = AnimationSender::channel();
            reactor.animation_tx = Some(sender);
            receiver = next_receiver;
        }
    }

    #[test]
    fn restated_space_snapshot_preserves_animation_but_topology_change_cancels_it() {
        use super::super::testing::*;
        use crate::actor::reactor::Event;

        let (mut apps, mut reactor) = test_context();
        let space = SpaceId::new(1);
        let Event::SpaceStateChanged(mut snapshot) =
            space_state_event(vec![rect(0., 0., 1000., 800.)], vec![Some(space)])
        else {
            unreachable!()
        };
        reactor.handle_event(Event::SpaceStateChanged(snapshot.clone()));
        apps.make_app_and_settle(&mut reactor, 1, make_windows(1));
        let wid = WindowId::new(1, 1);
        let (sender, receiver) = AnimationSender::channel();
        let frame = rect(10., 10., 500., 400.);
        sender
            .control
            .lock()
            .frames
            .insert(wid, (reactor.app_manager.apps[&1].handle.clone(), frame));
        let control = Arc::clone(&sender.control);
        reactor.animation_tx = Some(sender);
        for revision in [
            snapshot.revision,
            snapshot.revision.saturating_sub(1),
            snapshot.revision + 1,
        ] {
            let mut restated = snapshot.clone();
            restated.revision = revision;
            reactor.handle_event(Event::SpaceStateChanged(restated));
            assert!(control.lock().frames.contains_key(&wid));
            assert!(
                receiver.commands.try_iter().all(|message| !matches!(message, Message::Stop(_)))
            );
        }
        snapshot.revision += 2;
        snapshot.screens[0].frame.size.width += 100.;
        reactor.handle_event(Event::SpaceStateChanged(snapshot));
        assert!(!control.lock().frames.contains_key(&wid));
        assert!(receiver.commands.try_iter().any(|message| matches!(message, Message::Stop(_))));
    }

    #[test]
    fn cancellation_discards_pending_frames_and_does_not_replay_the_target() {
        let (handle, mut rx) = AppThreadHandle::channel();
        let wid = WindowId::new(1, 1);
        let (sender, commands_rx) = AnimationSender::channel();
        let runner = std::thread::spawn(move || AnimationManager::run(commands_rx));
        sender
            .send(Message::Replace(animation(
                &handle,
                wid,
                rect(0.0, 0.0, 10.0, 10.0),
                rect(100.0, 50.0, 10.0, 10.0),
            )))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        // Leave the display wake outstanding so cancellation must discard it.
        while !sender
            .control
            .lock()
            .frames
            .get(&wid)
            .is_some_and(|(_, frame)| frame.origin.x > 0.0)
            && Instant::now() < deadline
        {
            std::thread::yield_now();
        }
        let frames = sender.cancel(vec![wid]);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].0, wid);
        assert!(frames[0].1.origin.x > 0.0 && frames[0].1.origin.x < 100.0);
        drop(sender);
        runner.join().unwrap();
        let requests = collect_requests(&mut rx);
        assert!(requests.iter().any(
            |r| matches!(r, Observed::Request(Request::CancelWindowAnimation(id, _, _)) if *id == wid)
        ));
        assert!(
            requests.iter().all(|r| matches!(
                r,
                Observed::Request(
                    Request::BeginWindowAnimation(_, _) | Request::CancelWindowAnimation(..)
                )
            )),
            "cancelled frames and the target cannot be replayed: {requests:?}"
        );
    }

    #[test]
    fn replacement_uses_last_animated_frame_for_continuing_windows() {
        let (tx, mut rx) = crate::actor::channel();
        let handle = AppThreadHandle::new_for_test(tx);
        let wid = WindowId::new(1, 1);
        let first = animation(
            &handle,
            wid,
            rect(0.0, 0.0, 10.0, 10.0),
            rect(50.0, 60.0, 10.0, 10.0),
        );
        let second = animation(
            &handle,
            wid,
            rect(50.0, 60.0, 10.0, 10.0),
            rect(80.0, 90.0, 10.0, 10.0),
        );

        let mut manager = AnimationManager::new();
        manager.handle_message(Message::Replace(first));
        assert!(matches!(
            collect_requests(&mut rx).as_slice(),
            [Observed::Request(Request::BeginWindowAnimation(req_wid, _))] if *req_wid == wid
        ));

        manager.tick_at(transition_state(&manager).unwrap().started + Duration::from_millis(10));
        let active = transition_state(&manager).unwrap();
        let continuing_frame = active.animation.windows[0].frame;
        assert_animation_pos(&collect_requests(&mut rx)[0], wid, continuing_frame.origin);

        manager.handle_message(Message::Replace(second));
        assert!(collect_requests(&mut rx).is_empty());

        let resumed_start = transition_state(&manager).unwrap().animation.windows[0].from;
        assert_eq!(resumed_start, continuing_frame);

        // The presenter publishes immediately after a replacement, before the
        // next display tick. Queries and subsequent layouts must see the last
        // displayed frame rather than the new command's stale starting frame.
        let mut control = PresenterControl::default();
        manager.publish_frames(&mut control, true);
        assert_eq!(control.frame(wid, CGRect::ZERO), continuing_frame);

        manager.tick_at(transition_state(&manager).unwrap().started + Duration::from_millis(10));
        let next = collect_requests(&mut rx);
        let Observed::Frame { frame, .. } = next[0] else {
            panic!("frame");
        };
        assert!(frame.origin.x > resumed_start.origin.x);
        assert!(frame.origin.y > resumed_start.origin.y);
    }

    fn animation_contains(manager: &AnimationManager, wid: WindowId) -> bool {
        transition_state(manager).is_some_and(|a| a.animation.windows.iter().any(|w| w.wid == wid))
    }

    #[test]
    fn replacement_only_restarts_changed_windows() {
        let (tx, mut rx) = crate::actor::channel();
        let handle = AppThreadHandle::new_for_test(tx);
        let wid1 = WindowId::new(1, 1);
        let wid2 = WindowId::new(1, 2);
        let wid3 = WindowId::new(1, 3);
        let mut first = empty_animation();
        first.add_window(
            &handle,
            wid1,
            rect(0.0, 0.0, 10.0, 10.0),
            rect(50.0, 60.0, 10.0, 10.0),
            TransactionId::default(),
        );
        first.add_window(
            &handle,
            wid2,
            rect(10.0, 0.0, 10.0, 10.0),
            rect(60.0, 60.0, 10.0, 10.0),
            TransactionId::default(),
        );
        let mut second = empty_animation();
        second.add_window(
            &handle,
            wid1,
            rect(50.0, 60.0, 10.0, 10.0),
            rect(80.0, 90.0, 10.0, 10.0),
            TransactionId::default(),
        );
        second.add_window(
            &handle,
            wid3,
            rect(20.0, 0.0, 10.0, 10.0),
            rect(90.0, 90.0, 10.0, 10.0),
            TransactionId::default(),
        );

        let mut manager = AnimationManager::new();
        manager.handle_message(Message::Replace(first));
        assert_eq!(collect_requests(&mut rx).len(), 2);
        manager.handle_message(Message::Replace(second));

        let requests = collect_requests(&mut rx);
        assert_eq!(requests.len(), 1);
        assert!(
            matches!(requests[0], Observed::Request(Request::BeginWindowAnimation(req_wid, _)) if req_wid == wid3)
        );
        assert!(animation_contains(&manager, wid2));

        let carried = transition_state(&manager)
            .unwrap()
            .animation
            .windows
            .iter()
            .find(|w| w.wid == wid2)
            .unwrap();
        assert_eq!(carried.to, rect(60.0, 60.0, 10.0, 10.0));
    }

    #[test]
    fn replacement_does_not_carry_over_explicitly_handled_windows() {
        let (tx, mut rx) = crate::actor::channel();
        let handle = AppThreadHandle::new_for_test(tx);
        let wid1 = WindowId::new(1, 1);
        let wid2 = WindowId::new(1, 2);
        let mut first = empty_animation();
        first.add_window(
            &handle,
            wid1,
            rect(0.0, 0.0, 10.0, 10.0),
            rect(50.0, 60.0, 10.0, 10.0),
            TransactionId::default(),
        );
        first.add_window(
            &handle,
            wid2,
            rect(10.0, 0.0, 10.0, 10.0),
            rect(60.0, 60.0, 10.0, 10.0),
            TransactionId::default(),
        );
        let mut second = empty_animation();
        second.add_window(
            &handle,
            wid1,
            rect(50.0, 60.0, 10.0, 10.0),
            rect(80.0, 90.0, 10.0, 10.0),
            TransactionId::default(),
        );
        second.mark_handled(wid2);

        let mut manager = AnimationManager::new();
        manager.handle_message(Message::Replace(first));
        let _ = collect_requests(&mut rx);
        manager.handle_message(Message::Replace(second));

        assert!(!animation_contains(&manager, wid2));
    }

    #[test]
    fn skip_to_end_finishes_active_animation_and_applies_new_layout() {
        let (tx, mut rx) = crate::actor::channel();
        let handle = AppThreadHandle::new_for_test(tx);
        let wid = WindowId::new(1, 1);
        let first = animation(
            &handle,
            wid,
            rect(0.0, 0.0, 10.0, 10.0),
            rect(50.0, 60.0, 10.0, 10.0),
        );
        let second = animation(
            &handle,
            wid,
            rect(50.0, 60.0, 10.0, 10.0),
            rect(80.0, 90.0, 10.0, 10.0),
        );

        let mut manager = AnimationManager::new();
        manager.handle_message(Message::Replace(first));
        manager.handle_message(Message::SkipToEnd(second));

        let requests = collect_requests(&mut rx);
        assert_eq!(requests.len(), 4);
        assert!(
            matches!(requests[0], Observed::Request(Request::BeginWindowAnimation(req_wid, _)) if req_wid == wid)
        );
        assert_animation_frame(&requests[1], wid, rect(50.0, 60.0, 10.0, 10.0));
        assert!(
            matches!(requests[2], Observed::Request(Request::EndWindowAnimation(req_wid, _)) if req_wid == wid)
        );
        assert_set_window_frame(&requests[3], wid, rect(80.0, 90.0, 10.0, 10.0));
    }
}

#[cfg(test)]
mod live_resize_tests {
    use super::*;
    use crate::model::live_resize::{Client, Event};

    fn rect(width: f64) -> CGRect { CGRect::new(CGPoint::new(10., 40.), CGSize::new(width, 600.)) }
    fn animation(handle: &AppThreadHandle, from: f64, to: f64) -> Animation {
        let mut a = Animation::new(120., 0.3);
        a.add_window(
            handle,
            WindowId::new(1, 1),
            rect(from),
            rect(to),
            TransactionId::default(),
        );
        a.windows[0].wsid = Some(WindowServerId::new(42));
        a
    }
    #[test]
    fn live_resize_two_app_rows_share_the_same_presented_boundary() {
        let (sender, mut events) = crate::actor::channel();
        let client = Client::new(sender);
        let (first, mut first_requests) = AppThreadHandle::channel();
        let (second, mut second_requests) = AppThreadHandle::channel();
        let row = |y, height| CGRect::new(CGPoint::new(0., y), CGSize::new(1000., height));
        let mut animation = Animation::new(120., 0.3);
        animation.add_window(
            &first,
            WindowId::new(1, 1),
            row(0., 400.),
            row(0., 500.),
            TransactionId::default(),
        );
        animation.add_window(
            &second,
            WindowId::new(2, 2),
            row(400., 400.),
            row(500., 300.),
            TransactionId::default(),
        );
        animation.windows[0].wsid = Some(WindowServerId::new(42));
        animation.windows[1].wsid = Some(WindowServerId::new(43));
        let mut manager = AnimationManager {
            resize: Some(client),
            ..AnimationManager::new()
        };
        manager.handle_message(Message::Replace(animation));
        let Event::Prepare(scene) = events.try_recv().unwrap().1 else {
            panic!("complete scene")
        };
        assert_eq!(scene.members.len(), 2);
        for ticket in &scene.members {
            ticket.accepted(ticket.plan().to);
        }
        scene.start(Instant::now() - Duration::from_millis(100));
        while first_requests.try_recv().is_ok() {}
        while second_requests.try_recv().is_ok() {}
        manager.tick_at(Instant::now() + Duration::from_secs(1));
        let windows = manager.motions[0].windows();
        let top = windows.iter().find(|w| w.wid.pid == 1).unwrap().frame;
        let bottom = windows.iter().find(|w| w.wid.pid == 2).unwrap().frame;
        assert!((top.origin.y + top.size.height - bottom.origin.y).abs() < 0.000_001);
        let snapshot = scene.visuals();
        assert_eq!(snapshot.frames[&WindowId::new(1, 1)].1, top);
        assert_eq!(snapshot.frames[&WindowId::new(2, 2)].1, bottom);
        assert!(first_requests.try_recv().is_err());
        assert!(second_requests.try_recv().is_err());
    }

    #[test]
    fn live_resize_noop_commit_end_ack_clears_the_new_transaction_before_external_resize() {
        use super::super::testing::*;
        use crate::actor::reactor::{Event as ReactorEvent, Requested};
        use crate::sys::event::MouseState;
        let (mut apps, mut reactor) = test_context();
        let space = SpaceId::new(1);
        reactor.handle_event(space_state_event(
            vec![CGRect::new(CGPoint::ZERO, CGSize::new(1000., 800.))],
            vec![Some(space)],
        ));
        apps.make_app_and_settle(&mut reactor, 1, make_windows(1));
        let wid = WindowId::new(1, 1);
        let wsid = reactor.test_window_server_id(wid);
        let native = reactor.state.windows.window(wid).unwrap().frame_monotonic;
        let prior = reactor.transaction_manager.generate_next_txid(wsid);
        reactor.transaction_manager.generate_next_txid(wsid);
        let reversal = reactor.transaction_manager.generate_next_txid(wsid);
        assert_ne!(prior, TransactionId::default());
        reactor.transaction_manager.store_txid(wsid, reversal, native);
        let (sender, _events) = crate::actor::channel();
        let client = Client::new(sender);
        let (handle, mut requests) = AppThreadHandle::channel();
        let scene = client.prepare_scene(vec![ResizePlan {
            wid,
            window_id: wsid.as_u32(),
            display: 1,
            handle,
            txid: reversal,
            from: native,
            to: native,
            duration: Duration::from_millis(300),
        }]);
        requests.try_recv().unwrap();
        assert!(scene.commit_native());
        let Request::CommitResize(committed, _) =
            requests.try_recv().expect("even a no-op commit must reach the app actor").1
        else {
            panic!("actor-owned acknowledgement")
        };
        reactor
            .dispatch_workflow(ReactorEvent::WindowFrameChanged(
                wid,
                native,
                Some(prior),
                Requested(true),
                Some(MouseState::Up),
            ))
            .unwrap();
        assert_eq!(reactor.transaction_manager.get_target_frame(wsid), Some(native));
        reactor
            .dispatch_workflow(ReactorEvent::WindowFrameChanged(
                wid,
                native,
                Some(committed.plan().txid),
                Requested(true),
                Some(MouseState::Up),
            ))
            .unwrap();
        assert_eq!(reactor.transaction_manager.get_target_frame(wsid), None);
        let mut resized = native;
        resized.size.width -= 40.;
        let outcome = reactor
            .dispatch_workflow(ReactorEvent::WindowFrameChanged(
                wid,
                resized,
                Some(reversal),
                Requested(false),
                Some(MouseState::Up),
            ))
            .unwrap();
        assert_eq!(
            outcome.arrange.passes, 1,
            "a stale target cannot absorb native geometry"
        );
        assert!(!outcome.arrange.is_resize);
        assert!(
            outcome.layout_events.is_empty(),
            "mouse-up geometry cannot persist layout widths"
        );
        assert_eq!(
            reactor.state.windows.window(wid).unwrap().frame_monotonic,
            resized
        );
    }

    #[test]
    fn live_resize_duplicate_native_completion_does_not_resize_the_layout() {
        use super::super::testing::*;
        use crate::actor::reactor::{Event as ReactorEvent, Requested};
        use crate::sys::event::MouseState;

        let (mut apps, mut reactor) = test_context();
        let space = SpaceId::new(1);
        reactor.handle_event(space_state_event(
            vec![CGRect::new(CGPoint::ZERO, CGSize::new(1000., 800.))],
            vec![Some(space)],
        ));
        apps.make_app_and_settle(&mut reactor, 1, make_windows(1));
        let wid = WindowId::new(1, 1);
        let wsid = reactor.test_window_server_id(wid);
        let handle = reactor.app_manager.apps[&1].handle.clone();
        let (tx, _events) = crate::actor::channel();
        let client = Client::new(tx);
        let txid = reactor.transaction_manager.generate_next_txid(wsid);
        reactor.transaction_manager.store_txid(wsid, txid, rect(900.));
        let ticket = client.prepare(ResizePlan {
            wid,
            window_id: wsid.as_u32(),
            display: 1,
            handle: handle.clone(),
            txid,
            from: rect(500.),
            to: rect(900.),
            duration: Duration::from_millis(300),
        });
        ticket.accepted(rect(900.));
        ticket.state().visual = rect(750.);
        ticket.state().phase = ResizePhase::Presenting(Instant::now());
        let (sender, _receiver) = AnimationSender::channel();
        sender.set_resize_client(Some(client));
        sender.control.lock().frames.insert(wid, (handle.clone(), rect(750.)));
        reactor.animation_tx = Some(sender);
        let camera = super::tests::motion_camera(&handle, rect(500.), rect(900.));
        reactor.presentations.insert(space, ViewportHandle {
            identity: camera.identity,
            state: camera.state,
            windows: vec![wid],
            headless: None,
        });

        for _ in 0..2 {
            let outcome = reactor
                .dispatch_workflow(ReactorEvent::WindowFrameChanged(
                    wid,
                    rect(900.),
                    Some(txid),
                    Requested(false),
                    Some(MouseState::Up),
                ))
                .unwrap();
            assert_eq!(
                outcome.arrange.passes, 0,
                "native completion is not an external resize"
            );
            assert!(outcome.layout_events.is_empty());
            assert_eq!(
                reactor.state.windows.window(wid).unwrap().frame_monotonic,
                rect(900.)
            );
            assert_eq!(reactor.transaction_manager.get_target_frame(wsid), None);
            assert_eq!(reactor.transaction_manager.get_last_sent_txid(wsid), txid);
            assert!(ticket.is_current());
        }
        let outcome = reactor
            .dispatch_workflow(ReactorEvent::WindowFrameChanged(
                wid,
                rect(950.),
                Some(txid),
                Requested(false),
                Some(MouseState::Up),
            ))
            .unwrap();
        assert_eq!(
            outcome.arrange.passes, 1,
            "a different native size remains observable"
        );
        assert!(!outcome.arrange.is_resize);
        assert!(
            outcome.layout_events.is_empty(),
            "mouse-up geometry cannot persist layout widths"
        );
        assert_eq!(
            reactor.state.windows.window(wid).unwrap().frame_monotonic,
            rect(950.)
        );
        let outcome = reactor
            .dispatch_workflow(ReactorEvent::WindowFrameChanged(
                wid,
                rect(940.),
                Some(txid),
                Requested(false),
                Some(MouseState::Down),
            ))
            .unwrap();
        assert!(
            outcome.layout_events.iter().any(|event| matches!(
                event,
                crate::layout_engine::LayoutEvent::WindowResized { wid: resized, new_frame, .. }
                    if *resized == wid && *new_frame == rect(940.)
            )),
            "a real resize gesture updates layout widths"
        );
        assert!(
            !ticket.is_current(),
            "a real resize gesture owns native geometry"
        );
    }

    #[test]
    fn live_resize_unadmitted_native_geometry_cannot_arrange_another_window() {
        use super::super::testing::*;
        use crate::actor::reactor::{Event as ReactorEvent, Requested};
        use crate::model::window_store::ClaimOwner;
        use crate::sys::event::MouseState;

        for admission in [
            "transient",
            "excluded",
            "pending",
            "minimized",
            "invalidated",
            "claimed",
        ] {
            let (mut apps, mut reactor) = test_context();
            let space = SpaceId::new(1);
            reactor.handle_event(space_state_event(
                vec![CGRect::new(CGPoint::ZERO, CGSize::new(1000., 800.))],
                vec![Some(space)],
            ));
            let mut windows = make_windows(2);
            if admission == "transient" {
                windows[1].ax_role = Some("AXWindow".into());
                windows[1].ax_subrole = Some("AXDialog".into());
                windows[1].is_resizable = false;
            }
            apps.make_app_and_settle(&mut reactor, 1, windows);
            let wid = WindowId::new(1, 1);
            let panel = WindowId::new(1, 2);
            match admission {
                "excluded" => {
                    reactor.state.windows.window_mut(panel).unwrap().manage_override = Some(false)
                }
                "pending" => {
                    reactor.state.windows.window_mut(panel).unwrap().is_manageable = false;
                    reactor.state.windows.remove_window_assignment(panel);
                }
                "minimized" => {
                    reactor.state.windows.observe_minimized(panel);
                }
                "invalidated" => reactor.state.windows.invalidate_ax_identity(panel),
                "claimed" => reactor.state.windows.insert_claim(
                    panel,
                    ClaimOwner(10),
                    wm_protocol::WindowClaimFlags::empty(),
                ),
                _ => {}
            }
            assert!(!reactor.state.windows.is_admitted(panel), "{admission}");
            let assignment = reactor.state.windows.workspace_for_window(panel);
            let claimed_assignment =
                reactor.state.windows.claim(panel).map(|claim| claim.workspace());
            let wsid = reactor.test_window_server_id(wid);
            let handle = reactor.app_manager.apps[&1].handle.clone();
            let (tx, _events) = crate::actor::channel();
            let client = Client::new(tx);
            let ticket = client.prepare(ResizePlan {
                wid,
                window_id: wsid.as_u32(),
                display: 1,
                handle: handle.clone(),
                txid: reactor.transaction_manager.get_last_sent_txid(wsid),
                from: rect(500.),
                to: rect(900.),
                duration: Duration::from_millis(300),
            });
            ticket.accepted(rect(900.));
            ticket.state().visual = rect(750.);
            ticket.state().phase = ResizePhase::Presenting(Instant::now());
            let (sender, receiver) = AnimationSender::channel();
            sender.set_resize_client(Some(client));
            sender.control.lock().frames.insert(wid, (handle.clone(), rect(750.)));
            reactor.animation_tx = Some(sender);
            let camera = super::tests::motion_camera(&handle, rect(500.), rect(900.));
            reactor.presentations.insert(space, ViewportHandle {
                identity: camera.identity,
                state: camera.state,
                windows: vec![wid],
                headless: None,
            });
            let mut changed = reactor.state.windows.window(panel).unwrap().frame_monotonic;
            changed.size.width += 1.;
            for frame in [
                changed,
                CGRect::new(CGPoint::new(2000., 100.), changed.size),
            ] {
                let outcome = reactor
                    .dispatch_workflow(ReactorEvent::WindowFrameChanged(
                        panel,
                        frame,
                        None,
                        Requested(false),
                        Some(MouseState::Up),
                    ))
                    .unwrap();
                assert_eq!(
                    outcome.arrange.passes, 0,
                    "{admission} cannot arrange a managed window"
                );
                assert!(!outcome.arrange.is_resize);
                assert!(outcome.layout_events.is_empty());
                reactor.apply_event_outcome(outcome);
                assert_eq!(
                    reactor.state.windows.window(panel).unwrap().frame_monotonic,
                    frame
                );
                assert_eq!(reactor.state.windows.workspace_for_window(panel), assignment);
                assert_eq!(
                    reactor.state.windows.claim(panel).map(|claim| claim.workspace()),
                    claimed_assignment
                );
                assert!(!reactor.state.windows.is_admitted(panel));
                assert!(ticket.is_current());
                assert!(
                    receiver.commands.try_iter().next().is_none(),
                    "unrelated proxy cannot be replaced or stopped"
                );
            }
        }
    }

    #[test]
    fn live_resize_same_target_camera_echo_preserves_transaction_and_visual_clock() {
        for presenting in [false, true] {
            let (tx, mut events) = crate::actor::channel();
            let client = Client::new(tx);
            let (handle, mut requests) = AppThreadHandle::channel();
            let mut manager = AnimationManager {
                resize: Some(client),
                ..AnimationManager::new()
            };
            let wsid = WindowServerId::new(42);
            let store = WindowTxStore::new();
            let mut camera = super::tests::motion_camera(&handle, rect(500.), rect(900.));
            camera.windows[0].wsid = Some(wsid);
            camera.store = store.clone();
            manager.handle_message(Message::Camera(Box::new(camera)));
            let Event::Prepare(prepared_scene) = events.try_recv().unwrap().1 else {
                panic!("prepare")
            };
            let ticket = prepared_scene.members[0].clone();
            let txid = ticket.plan().txid;
            if presenting {
                ticket.accepted(rect(900.));
                ticket.state().phase = ResizePhase::Presenting(Instant::now());
                store.clear_target(&wsid);
            }
            let phase = ticket.state().phase;
            let pending_target = store.get(&wsid).unwrap().target;
            while requests.try_recv().is_ok() {}
            while events.try_recv().is_ok() {}
            for _ in 0..3 {
                let mut next = super::tests::motion_camera(&handle, rect(500.), rect(900.));
                next.windows[0].wsid = Some(wsid);
                next.store = store.clone();
                manager.handle_message(Message::Camera(Box::new(next)));
                let proxy = manager.motions[0].windows()[0].proxy.as_ref().unwrap();
                assert_eq!(proxy.ticket.generation(), ticket.generation());
                assert_eq!(manager.motions[0].windows()[0].txid, txid);
                assert_eq!(store.last_txid(&wsid), txid);
                assert_eq!(store.get(&wsid).unwrap().target, pending_target);
                assert_eq!(ticket.state().phase, phase);
                assert!(ticket.is_current());
                assert!(
                    requests.try_recv().is_err(),
                    "an echo cannot inspect or commit native geometry again"
                );
                assert!(
                    events.try_recv().is_err(),
                    "an echo cannot prepare another capture"
                );
            }
            let mut changed = super::tests::motion_camera(&handle, rect(500.), rect(800.));
            changed.windows[0].wsid = Some(wsid);
            changed.store = store.clone();
            manager.handle_message(Message::Camera(Box::new(changed)));
            assert!(
                !ticket.is_current(),
                "a changed semantic target starts a new transaction"
            );
            assert_ne!(store.last_txid(&wsid), txid);
            assert!(matches!(events.try_recv().unwrap().1, Event::Prepare(_)));
            assert!(matches!(
                requests.try_recv().unwrap().1,
                Request::InspectResize(..)
            ));
        }
    }

    #[test]
    fn live_resize_camera_echo_cannot_inherit_another_native_owner() {
        for changed in ["actor", "source", "display", "writer"] {
            let (tx, mut events) = crate::actor::channel();
            let (handle, mut requests) = AppThreadHandle::channel();
            let mut manager = AnimationManager {
                resize: Some(Client::new(tx)),
                ..AnimationManager::new()
            };
            let wsid = WindowServerId::new(42);
            let store = WindowTxStore::new();
            let mut camera = super::tests::motion_camera(&handle, rect(500.), rect(900.));
            camera.windows[0].wsid = Some(wsid);
            camera.store = store.clone();
            manager.handle_message(Message::Camera(Box::new(camera)));
            let Event::Prepare(prepared_scene) = events.try_recv().unwrap().1 else {
                panic!("prepare")
            };
            let ticket = prepared_scene.members[0].clone();
            while requests.try_recv().is_ok() {}
            let mut next = super::tests::motion_camera(&handle, rect(500.), rect(900.));
            next.windows[0].wsid = Some(wsid);
            next.store = store.clone();
            let (other_handle, _other_requests) = AppThreadHandle::channel();
            match changed {
                "actor" => next.windows[0].handle = other_handle,
                "source" => next.windows[0].wsid = Some(WindowServerId::new(43)),
                "display" => next.display = 2,
                "writer" => {
                    store.next_txid(wsid);
                }
                _ => unreachable!(),
            }
            manager.handle_message(Message::Camera(Box::new(next)));
            assert!(!ticket.is_current(), "{changed} must replace the native owner");
            let Event::Prepare(prepared_scene) = events.try_recv().unwrap().1 else {
                panic!("replacement")
            };
            let replacement = prepared_scene.members[0].clone();
            assert_ne!(replacement.generation(), ticket.generation());
            assert!(replacement.is_current());
        }
    }
    #[test]
    fn live_resize_holds_ax_samples_until_ui_retirement_and_keeps_visual_clock() {
        let (tx, mut events) = crate::actor::channel();
        let client = Client::new(tx);
        let (handle, mut requests) = AppThreadHandle::channel();
        let mut manager = AnimationManager {
            resize: Some(client),
            ..AnimationManager::new()
        };
        manager.handle_message(Message::Replace(animation(&handle, 500., 900.)));
        let Event::Prepare(prepared_scene) = events.try_recv().unwrap().1 else {
            panic!("prepare")
        };
        let ticket = prepared_scene.members[0].clone();
        assert!(matches!(
            requests.try_recv().unwrap().1,
            Request::BeginWindowAnimation(..)
        ));
        assert!(matches!(
            requests.try_recv().unwrap().1,
            Request::InspectResize(..)
        ));
        manager.tick_at(Instant::now() + Duration::from_secs(2));
        assert!(
            requests.try_recv().is_err(),
            "no intermediate native geometry during capture wait"
        );
        assert!(
            manager.has_work(),
            "global animation deadline cannot retire a preparing proxy"
        );
        ticket.accepted(rect(900.));
        ticket.state().phase = ResizePhase::Presenting(Instant::now() - Duration::from_millis(60));
        manager.tick_at(Instant::now() + Duration::from_secs(2));
        let frame = manager.motions[0].windows()[0].frame;
        assert!(frame.size.width > 500. && frame.size.width < 900.);
        assert!(
            requests.try_recv().is_err(),
            "visual spring never enters the native AX queue"
        );
        ticket.state().phase = ResizePhase::Presenting(Instant::now() - Duration::from_secs(1));
        manager.tick_at(Instant::now() + Duration::from_secs(2));
        assert_eq!(ticket.state().phase, ResizePhase::Finishing);
        assert!(manager.has_work(), "UI teardown must precede lease release");
        ticket.state().phase = ResizePhase::Retired;
        manager.tick_at(Instant::now() + Duration::from_secs(2));
        assert!(matches!(
            requests.try_recv().unwrap().1,
            Request::EndWindowAnimation(..)
        ));
        assert!(requests.try_recv().is_err());
    }
    #[test]
    fn live_resize_reversal_before_capture_replaces_the_pending_native_target() {
        let (tx, mut events) = crate::actor::channel();
        let client = Client::new(tx);
        let (handle, mut requests) = AppThreadHandle::channel();
        let mut manager = AnimationManager {
            resize: Some(client),
            ..AnimationManager::new()
        };
        manager.handle_message(Message::Replace(animation(&handle, 500., 900.)));
        let Event::Prepare(prepared_scene) = events.try_recv().unwrap().1 else {
            panic!("prepare")
        };
        let old = prepared_scene.members[0].clone();
        let mut next = animation(&handle, 500., 500.);
        next.windows[0].txid = TransactionId::default().next();
        manager.handle_message(Message::Replace(next));
        let Event::Prepare(prepared_scene) = events.try_recv().unwrap().1 else {
            panic!("replacement")
        };
        let next = prepared_scene.members[0].clone();
        assert!(!old.is_current());
        assert!(next.is_current());
        assert_eq!(next.plan().to, rect(500.));
        assert_ne!(next.generation(), old.generation());
        assert!(matches!(
            requests.try_recv().unwrap().1,
            Request::BeginWindowAnimation(..)
        ));
        assert!(matches!(
            requests.try_recv().unwrap().1,
            Request::InspectResize(..)
        ));
        assert!(matches!(
            requests.try_recv().unwrap().1,
            Request::InspectResize(..)
        ));
        assert!(
            requests.try_recv().is_err(),
            "retarget transfers the existing lease"
        );
        manager.tick_at(Instant::now() + Duration::from_secs(1));
        assert!(requests.try_recv().is_err());
    }
    #[test]
    fn live_resize_async_retirement_releases_lease_before_a_new_message() {
        let (tx, mut events) = crate::actor::channel();
        let client = Client::new(tx);
        let (handle, mut requests) = AppThreadHandle::channel();
        let mut manager = AnimationManager {
            resize: Some(client),
            ..AnimationManager::new()
        };
        manager.handle_message(Message::Replace(animation(&handle, 500., 900.)));
        let Event::Prepare(prepared_scene) = events.try_recv().unwrap().1 else {
            panic!("prepare")
        };
        let ticket = prepared_scene.members[0].clone();
        while requests.try_recv().is_ok() {}
        ticket.accepted(rect(900.));
        ticket.state().phase = ResizePhase::Presenting(Instant::now() - Duration::from_secs(1));
        manager.tick_at(Instant::now() + Duration::from_secs(2));
        assert_eq!(ticket.state().phase, ResizePhase::Finishing);
        ticket.state().phase = ResizePhase::Retired;
        manager.handle_message(Message::Stop(vec![]));
        assert!(matches!(
            requests.try_recv().unwrap().1,
            Request::EndWindowAnimation(..)
        ));
        assert!(requests.try_recv().is_err());
        assert!(!manager.has_work());
    }
    #[test]
    fn live_resize_in_doubt_commit_timeout_never_resumes_intermediate_native_sizes() {
        let (tx, mut events) = crate::actor::channel();
        let client = Client::new(tx);
        let (handle, mut requests) = AppThreadHandle::channel();
        let mut manager = AnimationManager {
            resize: Some(client),
            ..AnimationManager::new()
        };
        manager.handle_message(Message::Replace(animation(&handle, 500., 900.)));
        let Event::Prepare(prepared_scene) = events.try_recv().unwrap().1 else {
            panic!("prepare")
        };
        let ticket = prepared_scene.members[0].clone();
        while requests.try_recv().is_ok() {}
        ticket.state().attempted = true;
        ticket.fallback();
        manager.tick_at(Instant::now() + Duration::from_millis(100));
        let Request::InteractiveFramesPending(frames) = requests.try_recv().unwrap().1 else {
            panic!("final reconciliation")
        };
        let mut observed = vec![];
        frames.drain_with(|_, frame, _, _, _, _, _| observed.push(frame));
        assert_eq!(observed, vec![rect(900.)]);
        manager.tick_at(Instant::now() + Duration::from_millis(160));
        assert!(
            requests.try_recv().is_err(),
            "an uncertain commit converges once, without an AX size spring"
        );
    }
    #[test]
    fn live_resize_nonanimated_camera_detaches_inherited_proxy_before_native_positioning() {
        let (tx, mut events) = crate::actor::channel();
        let client = Client::new(tx);
        let (handle, mut requests) = AppThreadHandle::channel();
        let mut manager = AnimationManager {
            resize: Some(client),
            ..AnimationManager::new()
        };
        let from = CGRect::new(CGPoint::ZERO, CGSize::new(500., 600.));
        let to = CGRect::new(CGPoint::ZERO, CGSize::new(900., 600.));
        let mut camera = super::tests::motion_camera(&handle, from, to);
        camera.windows[0].wsid = Some(WindowServerId::new(42));
        manager.handle_message(Message::Camera(Box::new(camera)));
        let Event::Prepare(prepared_scene) = events.try_recv().unwrap().1 else {
            panic!("prepare")
        };
        let ticket = prepared_scene.members[0].clone();
        ticket.accepted(to);
        while requests.try_recv().is_ok() {}
        let target = CGRect::new(CGPoint::new(30., 0.), to.size);
        let mut next = super::tests::motion_camera(&handle, to, target);
        next.windows[0].wsid = Some(WindowServerId::new(42));
        next.animate = false;
        manager.handle_message(Message::Camera(Box::new(next)));
        assert!(!ticket.is_current());
        assert!(manager.motions[0].windows()[0].proxy.is_none());
        manager.tick_at(Instant::now() + Duration::from_millis(10));
        let Request::InteractiveFramesPending(frames) = requests.try_recv().unwrap().1 else {
            panic!("camera native frame")
        };
        let mut observed = vec![];
        frames.drain_with(|_, frame, _, _, _, _, _| observed.push(frame));
        assert_eq!(observed, vec![target]);
    }
}
