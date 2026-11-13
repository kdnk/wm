// Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use objc2_core_foundation::CGRect;
use parking_lot::Mutex;

use crate::actor;
use crate::actor::app::{AppThreadHandle, WindowId};
use crate::actor::reactor::transaction_manager::TransactionId;
use crate::common::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    Preparing,
    AwaitingNative,
    AwaitingTarget,
    Presenting(Instant),
    Finishing,
    Retired,
    Fallback,
}

#[derive(Debug)]
pub(crate) struct State {
    pub phase: Phase,
    pub accepted: Option<CGRect>,
    pub visual: CGRect,
    pub continued: bool,
    pub inspected: bool,
    pub attempted: bool,
}

#[derive(Debug)]
struct TicketInner {
    generation: u64,
    permit: Arc<AtomicU64>,
    pub plan: Plan,
    pub state: Mutex<State>,
    actual: Arc<Mutex<CGRect>>,
    sender: actor::Sender<Event>,
    scene: Arc<SceneControl>,
}

#[derive(Clone, Debug)]
#[doc(hidden)]
pub struct Ticket(Arc<TicketInner>);

#[derive(Clone, Debug)]
pub(crate) struct Plan {
    pub wid: WindowId,
    pub window_id: u32,
    pub display: u32,
    pub handle: AppThreadHandle,
    pub txid: TransactionId,
    pub from: CGRect,
    pub to: CGRect,
    pub duration: Duration,
}

#[derive(Debug)]
struct SceneControl {
    id: u64,
    members: Vec<(WindowId, u64)>,
    started: Mutex<Option<Instant>>,
    visuals: Mutex<Arc<VisualSnapshot>>,
}

#[derive(Clone, Debug)]
pub(crate) struct Scene {
    pub members: Vec<Ticket>,
    control: Arc<SceneControl>,
}

#[derive(Debug)]
pub(crate) struct VisualSnapshot {
    pub frames: HashMap<WindowId, (u64, CGRect)>,
}

impl Scene {
    pub(crate) fn id(&self) -> u64 { self.control.id }

    pub(crate) fn display(&self) -> u32 { self.members[0].plan().display }

    pub(crate) fn started(&self) -> Option<Instant> { *self.control.started.lock() }

    pub(crate) fn visuals(&self) -> Arc<VisualSnapshot> { self.control.visuals.lock().clone() }

    pub(crate) fn commit_native(&self) -> bool {
        for ticket in &self.members {
            let mut state = ticket.state();
            state.attempted = true;
            state.phase = Phase::AwaitingNative;
        }
        for ticket in &self.members {
            if ticket
                .plan()
                .handle
                .send(crate::actor::app::Request::CommitResize(
                    ticket.clone(),
                    Default::default(),
                ))
                .is_err()
            {
                return false;
            }
        }
        true
    }

    pub(crate) fn start(&self, now: Instant) {
        let mut started = self.control.started.lock();
        for ticket in &self.members {
            ticket.state().phase = Phase::Presenting(now);
        }
        *started = Some(now);
        self.members[0].wake();
    }
}

pub(crate) enum Event {
    Prepare(Scene),
    Wake,
    Retire(Ticket),
    Cancel(WindowId, u64),
    Backdrop(
        Scene,
        Result<objc2_core_foundation::CFRetained<objc2_core_graphics::CGImage>, String>,
    ),
}

#[derive(Clone, Debug)]
pub struct Client {
    sender: actor::Sender<Event>,
    tickets: Arc<Mutex<HashMap<WindowId, Ticket>>>,
    next: Arc<AtomicU64>,
}

impl Client {
    pub(crate) fn new(sender: actor::Sender<Event>) -> Self {
        Self {
            sender,
            tickets: Arc::default(),
            next: Arc::new(AtomicU64::new(1)),
        }
    }

    pub(crate) fn prepare_scene(&self, plans: Vec<Plan>) -> Scene {
        assert!(!plans.is_empty());
        let mut tickets = self.tickets.lock();
        if let Some(first) = tickets.get(&plans[0].wid)
            && first.0.scene.members.len() == plans.len()
            && plans.iter().all(|plan| {
                tickets.get(&plan.wid).is_some_and(|ticket| {
                    let old = ticket.plan();
                    ticket.is_current()
                        && Arc::ptr_eq(&first.0.scene, &ticket.0.scene)
                        && !matches!(ticket.state().phase, Phase::Fallback | Phase::Retired)
                        && old.window_id == plan.window_id
                        && old.display == plan.display
                        && old.handle.same_actor(&plan.handle)
                        && old.txid == plan.txid
                        && old.to == plan.to
                        && old.duration == plan.duration
                })
            })
        {
            return Scene {
                members: plans.iter().map(|p| tickets[&p.wid].clone()).collect(),
                control: first.0.scene.clone(),
            };
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let keys: Vec<_> = plans
            .iter()
            .map(|plan| (plan.wid, self.next.fetch_add(1, Ordering::Relaxed)))
            .collect();
        let visuals = VisualSnapshot {
            frames: plans
                .iter()
                .zip(&keys)
                .map(|(p, (_, generation))| (p.wid, (*generation, p.from)))
                .collect(),
        };
        let control = Arc::new(SceneControl {
            id,
            members: keys.clone(),
            started: Mutex::new(None),
            visuals: Mutex::new(Arc::new(visuals)),
        });
        let mut members = Vec::new();
        for (plan, (_, generation)) in plans.into_iter().zip(keys) {
            let previous = tickets.get(&plan.wid);
            let actual = previous
                .map_or_else(|| Arc::new(Mutex::new(plan.from)), |old| old.0.actual.clone());
            let permit =
                previous.map_or_else(|| Arc::new(AtomicU64::new(0)), |old| old.0.permit.clone());
            let continued = previous.is_some_and(|old| {
                let state = old.state();
                !matches!(state.phase, Phase::Fallback | Phase::Retired)
                    || state.attempted
                    || state.accepted.is_some()
                    || state.continued
            });
            permit.store(generation, Ordering::Release);
            let ticket = Ticket(Arc::new(TicketInner {
                generation,
                permit,
                actual,
                scene: control.clone(),
                state: Mutex::new(State {
                    phase: Phase::Preparing,
                    accepted: None,
                    visual: plan.from,
                    continued,
                    inspected: false,
                    attempted: false,
                }),
                plan,
                sender: self.sender.clone(),
            }));
            tickets.insert(ticket.plan().wid, ticket.clone());
            members.push(ticket);
        }
        drop(tickets);
        let scene = Scene { members, control };
        for ticket in &scene.members {
            ticket.plan().handle.cancel_interactive_frame(ticket.plan().wid);
            let _ = ticket.plan().handle.send(crate::actor::app::Request::InspectResize(
                ticket.clone(),
                Default::default(),
            ));
        }
        self.sender.send(Event::Prepare(scene.clone()));
        scene
    }

    #[cfg(test)]
    pub(crate) fn prepare(&self, plan: Plan) -> Ticket {
        self.prepare_scene(vec![plan]).members.remove(0)
    }

    pub(crate) fn publish_visuals(&self, samples: impl IntoIterator<Item = (Ticket, CGRect)>) {
        let mut scenes: HashMap<u64, (Arc<SceneControl>, HashMap<WindowId, (u64, CGRect)>)> =
            HashMap::default();
        for (ticket, frame) in samples {
            if !ticket.is_current() {
                continue;
            }
            scenes
                .entry(ticket.0.scene.id)
                .or_insert_with(|| (ticket.0.scene.clone(), HashMap::default()))
                .1
                .insert(ticket.plan().wid, (ticket.generation(), frame));
        }
        for (_, (scene, frames)) in scenes {
            if scene
                .members
                .iter()
                .all(|(wid, generation)| frames.get(wid).is_some_and(|(g, _)| g == generation))
            {
                *scene.visuals.lock() = Arc::new(VisualSnapshot { frames });
            }
        }
        self.sender.send(Event::Wake);
    }

    pub(crate) fn cancel(&self, wid: WindowId) -> Option<CGRect> {
        let ticket = self.tickets.lock().remove(&wid)?;
        ticket.revoke();
        let native = ticket.native();
        self.sender.send(Event::Cancel(wid, ticket.generation()));
        Some(native)
    }

    pub(crate) fn visual(&self, wid: WindowId) -> Option<CGRect> {
        let tickets = self.tickets.lock();
        let ticket = tickets.get(&wid)?;
        if !ticket.is_current() {
            return None;
        }
        let state = ticket.0.state.lock();
        if matches!(state.phase, Phase::Retired | Phase::Fallback) {
            return None;
        }
        drop(state);
        ticket.0.scene.visuals.lock().frames.get(&wid).map(|(_, frame)| *frame)
    }

    pub(crate) fn retire(&self, ticket: &Ticket) {
        let mut tickets = self.tickets.lock();
        if tickets
            .get(&ticket.plan().wid)
            .is_some_and(|current| current.generation() == ticket.generation())
        {
            tickets.remove(&ticket.plan().wid);
        }
    }
}

impl Ticket {
    pub(crate) fn plan(&self) -> &Plan { &self.0.plan }

    pub(crate) fn started(&self) -> Option<Instant> { *self.0.scene.started.lock() }

    pub(crate) fn phase_at(&self, started: Option<Instant>) -> Phase {
        let phase = self.state().phase;
        match phase {
            Phase::Presenting(_) if self.0.scene.members.len() > 1 => {
                started.map_or(Phase::AwaitingTarget, Phase::Presenting)
            }
            _ => phase,
        }
    }

    pub(crate) fn scene_id(&self) -> u64 { self.0.scene.id }

    pub(crate) fn generation(&self) -> u64 { self.0.generation }

    pub(crate) fn is_current(&self) -> bool {
        self.0.permit.load(Ordering::Acquire) == self.0.generation
            && self.plan().handle.can_write_frames()
    }

    pub(crate) fn active_generation(&self) -> u64 { self.0.permit.load(Ordering::Acquire) }

    #[track_caller]
    pub(crate) fn revoke(&self) {
        if self.is_current() && std::env::var_os("WM_LIVE_RESIZE_TRACE").is_some() {
            tracing::info!(window_id = self.plan().window_id, generation = self.generation(), caller = %std::panic::Location::caller(), "Live resize revoked");
        }
        let _ = self.0.permit.compare_exchange(
            self.generation(),
            0,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    pub(crate) fn state(&self) -> parking_lot::MutexGuard<'_, State> { self.0.state.lock() }

    pub(crate) fn retire(&self) { self.0.sender.send(Event::Retire(self.clone())); }

    pub(crate) fn wake(&self) { self.0.sender.send(Event::Wake); }

    pub(crate) fn native(&self) -> CGRect { *self.0.actual.lock() }

    pub(crate) fn observed(&self, frame: CGRect) { *self.0.actual.lock() = frame; }

    pub(crate) fn accepted(&self, frame: CGRect) {
        self.observed(frame);
        if !self.is_current() {
            return;
        }
        let mut state = self.state();
        state.accepted = Some(frame);
        state.phase = Phase::AwaitingTarget;
        drop(state);
        self.wake();
    }

    pub(crate) fn fallback(&self) {
        if self.is_current() {
            self.state().phase = Phase::Fallback;
            self.revoke();
            self.wake();
        }
    }
}

pub(crate) fn eligible(from: CGRect, to: CGRect) -> bool {
    let size_changes = (from.size.width - to.size.width).abs() >= 1.
        || (from.size.height - to.size.height).abs() >= 1.;
    size_changes
        && [from.origin.x, from.origin.y, to.origin.x, to.origin.y]
            .into_iter()
            .all(f64::is_finite)
        && [
            from.size.width,
            from.size.height,
            to.size.width,
            to.size.height,
        ]
        .into_iter()
        .all(|value| value.is_finite() && value >= 1.)
}

pub(crate) fn matches_pixels(actual: [f64; 2], frame: CGRect, scale: f64) -> bool {
    scale.is_finite()
        && scale > 0.
        && actual
            .into_iter()
            .zip([frame.size.width * scale, frame.size.height * scale])
            .all(|(a, b)| a.is_finite() && (a - b).abs() <= 2.)
}

#[cfg(test)]
mod live_resize_tests {
    use objc2_core_foundation::{CGPoint, CGSize};

    use super::*;
    fn frame(w: f64, h: f64) -> CGRect { CGRect::new(CGPoint::new(10., 20.), CGSize::new(w, h)) }
    #[test]
    fn live_resize_gate_accepts_width_height_and_mixed_axis_changes() {
        assert!(eligible(frame(500., 600.), frame(900., 600.)));
        assert!(eligible(frame(500., 600.), frame(500., 900.)));
        assert!(eligible(frame(500., 900.), frame(900., 600.)));
        assert!(!eligible(frame(500., 600.), frame(500., 600.)));
        let mut moved = frame(900., 600.);
        moved.origin.x += 1.;
        assert!(eligible(frame(500., 600.), moved));
        assert!(!eligible(frame(500., 600.), frame(500.2, 600.)));
        let mut only_moved = frame(500., 600.);
        only_moved.origin.x += 100.;
        assert!(!eligible(frame(500., 600.), only_moved));
        let mut invalid = frame(900., 600.);
        invalid.origin.x = f64::NAN;
        assert!(!eligible(frame(500., 600.), invalid));
        assert!(!eligible(frame(500., 600.), frame(f64::NAN, 600.)));
    }
    #[test]
    fn live_resize_rejects_old_and_letterboxed_pixels_but_accepts_one_final_surface() {
        let target = frame(900., 600.);
        assert!(!matches_pixels([1000., 1200.], target, 2.));
        assert!(!matches_pixels([1800., 690.], target, 2.));
        assert!(matches_pixels([1800., 1200.], target, 2.));
        assert!(!matches_pixels([1800., 1200.], target, 0.));
    }
    fn plan(handle: AppThreadHandle, from: CGRect, to: CGRect) -> Plan {
        Plan {
            wid: WindowId::new(1, 1),
            window_id: 42,
            display: 1,
            handle,
            txid: TransactionId::default(),
            from,
            to,
            duration: Duration::from_millis(300),
        }
    }
    fn pair() -> (
        Client,
        Scene,
        actor::Receiver<crate::actor::app::Request>,
        actor::Receiver<crate::actor::app::Request>,
    ) {
        let (sender, _events) = actor::channel();
        let client = Client::new(sender);
        let (a, ra) = AppThreadHandle::channel();
        let (b, rb) = AppThreadHandle::channel();
        let first = plan(a, frame(500., 600.), frame(900., 600.));
        let mut second = plan(b, frame(500., 600.), frame(900., 600.));
        second.wid = WindowId::new(2, 2);
        second.window_id = 43;
        let scene = client.prepare_scene(vec![first, second]);
        (client, scene, ra, rb)
    }

    #[test]
    fn live_resize_cohort_commit_marks_every_member_before_any_native_dispatch() {
        let (_, scene, mut a, mut b) = pair();
        a.try_recv().unwrap();
        b.try_recv().unwrap();
        assert!(a.try_recv().is_err());
        assert!(b.try_recv().is_err());
        assert!(scene.commit_native());
        assert!(scene.members.iter().all(|ticket| ticket.state().attempted));
        assert!(matches!(
            a.try_recv().unwrap().1,
            crate::actor::app::Request::CommitResize(..)
        ));
        assert!(matches!(
            b.try_recv().unwrap().1,
            crate::actor::app::Request::CommitResize(..)
        ));
        assert!(a.try_recv().is_err());
        assert!(b.try_recv().is_err());
        for ticket in &scene.members {
            ticket.fallback();
            assert!(ticket.state().attempted);
        }
    }

    #[test]
    fn live_resize_cohort_clock_release_cannot_split_a_presenter_tick() {
        let (_, scene, _, _) = pair();
        let held_clock = scene.started();
        let now = Instant::now();
        scene.start(now);
        assert_eq!(held_clock, None);
        assert!(
            scene
                .members
                .iter()
                .all(|ticket| ticket.phase_at(held_clock) == Phase::AwaitingTarget)
        );
        let next_tick_clock = scene.started();
        assert!(
            scene
                .members
                .iter()
                .all(|ticket| ticket.phase_at(next_tick_clock) == Phase::Presenting(now))
        );
    }

    #[test]
    fn live_resize_visual_publication_never_exposes_half_of_a_cohort() {
        let (client, scene, _, _) = pair();
        let before = scene.visuals();
        let a = scene.members[0].clone();
        let b = scene.members[1].clone();
        client.publish_visuals([(a.clone(), frame(700., 600.))]);
        assert!(Arc::ptr_eq(&before, &scene.visuals()));
        client.publish_visuals([
            (a.clone(), frame(700., 600.)),
            (b.clone(), frame(700., 600.)),
        ]);
        let after = scene.visuals();
        assert!(!Arc::ptr_eq(&before, &after));
        assert_eq!(after.frames[&a.plan().wid].1, frame(700., 600.));
        assert_eq!(after.frames[&b.plan().wid].1, frame(700., 600.));
        assert_eq!(before.frames[&a.plan().wid].1, frame(500., 600.));
    }

    #[test]
    fn live_resize_cohort_echo_preserves_clock_and_retarget_replaces_all_members() {
        let (client, scene, mut a, mut b) = pair();
        a.try_recv().unwrap();
        b.try_recv().unwrap();
        let now = Instant::now();
        scene.start(now);
        let mut plans: Vec<_> = scene.members.iter().map(|ticket| ticket.plan().clone()).collect();
        for plan in &mut plans {
            plan.from = frame(700., 600.);
        }
        let echo = client.prepare_scene(plans.clone());
        assert_eq!(echo.id(), scene.id());
        assert_eq!(echo.started(), Some(now));
        assert!(a.try_recv().is_err());
        assert!(b.try_recv().is_err());
        plans[0].to = frame(800., 600.);
        let next = client.prepare_scene(plans);
        assert_ne!(next.id(), scene.id());
        assert!(scene.members.iter().all(|old| !old.is_current()));
        assert!(next.members.iter().all(|ticket| ticket.is_current()));
        scene.members[0].accepted(frame(900., 600.));
        assert_eq!(next.members[0].native(), frame(900., 600.));
        assert_eq!(next.members[0].state().phase, Phase::Preparing);
    }

    #[test]
    fn live_resize_equal_native_target_still_acknowledges_its_transaction_in_the_app_actor() {
        let (_, scene, mut a, mut b) = pair();
        a.try_recv().unwrap();
        b.try_recv().unwrap();
        scene.members[0].observed(scene.members[0].plan().to);
        assert!(scene.commit_native());
        assert!(matches!(
            a.try_recv().unwrap().1,
            crate::actor::app::Request::CommitResize(..)
        ));
        assert_eq!(scene.members[0].state().phase, Phase::AwaitingNative);
        assert!(matches!(
            b.try_recv().unwrap().1,
            crate::actor::app::Request::CommitResize(..)
        ));
    }

    #[test]
    fn live_resize_stale_acceptance_updates_actual_but_never_reveals_replacement() {
        let (tx, _rx) = actor::channel();
        let client = Client::new(tx);
        let (handle, _requests) = AppThreadHandle::channel();
        let old = client.prepare(plan(handle.clone(), frame(500., 600.), frame(900., 600.)));
        old.accepted(frame(900., 600.));
        let next = client.prepare(plan(handle, frame(700., 600.), frame(500., 600.)));
        assert!(!old.is_current());
        old.accepted(frame(880., 600.));
        assert_eq!(next.native(), frame(880., 600.));
        assert_eq!(next.state().phase, Phase::Preparing);
        assert_eq!(next.state().accepted, None);
        old.revoke();
        client.retire(&old);
        assert!(
            next.is_current(),
            "stale cancellation cannot revoke the replacement"
        );
        assert_eq!(client.visual(next.plan().wid), Some(frame(700., 600.)));
        next.accepted(frame(500., 600.));
        assert_eq!(client.cancel(next.plan().wid), Some(frame(500., 600.)));
        assert!(!next.is_current());
        assert_eq!(client.visual(next.plan().wid), None);
    }
    #[test]
    fn live_resize_cancel_before_commit_returns_native_not_visual_geometry() {
        let (tx, _rx) = actor::channel();
        let client = Client::new(tx);
        let (handle, _requests) = AppThreadHandle::channel();
        let old = client.prepare(plan(handle.clone(), frame(500., 600.), frame(900., 600.)));
        old.accepted(frame(900., 600.));
        old.state().visual = frame(650., 600.);
        let next = client.prepare(plan(handle, frame(650., 600.), frame(500., 600.)));
        assert_eq!(client.cancel(next.plan().wid), Some(frame(900., 600.)));
        assert!(!next.is_current());
    }
    #[test]
    fn live_resize_failed_preparation_does_not_turn_the_next_resize_into_committed_fallback() {
        let (tx, _events) = actor::channel();
        let client = Client::new(tx);
        let (handle, _requests) = AppThreadHandle::channel();
        let old = client.prepare(plan(handle.clone(), frame(500., 600.), frame(900., 600.)));
        old.fallback();
        old.retire();
        let next = client.prepare(plan(handle, frame(500., 600.), frame(900., 600.)));
        assert!(
            !next.state().continued,
            "an unconsumed UI retirement must not imply a native commit"
        );
        assert_eq!(next.native(), frame(500., 600.));
        client.retire(&old);
        assert!(
            next.is_current(),
            "late old retirement cannot remove a replacement"
        );
    }
    #[test]
    fn live_resize_preparation_revokes_older_queued_native_samples() {
        let (tx, _events) = actor::channel();
        let client = Client::new(tx);
        let (handle, mut requests) = AppThreadHandle::channel();
        let wid = WindowId::new(1, 1);
        handle.send_interactive_frame(
            wid,
            frame(700., 600.),
            true,
            Default::default(),
            crate::actor::app::FrameSource::Ordinary,
        );
        let _ticket = client.prepare(plan(handle, frame(500., 600.), frame(900., 600.)));
        let crate::actor::app::Request::InteractiveFramesPending(frames) =
            requests.try_recv().unwrap().1
        else {
            panic!("old native queue")
        };
        let mut writes = 0;
        frames.drain_with(|_, _, _, _, _, _, _| writes += 1);
        assert_eq!(
            writes, 0,
            "native source must stop changing while its old pixels are captured"
        );
        assert!(matches!(
            requests.try_recv().unwrap().1,
            crate::actor::app::Request::InspectResize(..)
        ));
    }
}
