use std::pin::Pin;
use std::sync::mpsc::TryRecvError;
use std::time::{Duration, Instant};

use bevy::app::AppExit;
use bevy::ecs::component::Component;
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::message::MessageWriter;
use bevy::ecs::query::{With, Without};
use bevy::ecs::resource::Resource;
use bevy::ecs::system::{NonSend, NonSendMut, Query, Res, ResMut, SystemParam};
use bevy::time::{Real, Time};

use super::{
    ActiveWorkspaceMarker, FlashMessage, FocusedMarker, FreshMarker, Initializing,
    RefreshWindowSizes, RepositionMarker, ResizeMarker, RetryFrontSwitch, Scrolling, Timeout,
    VerifyWindowPosition,
};
use crate::ecs::observation::ObserverDetachRetry;
use crate::events::{Event, EventReceiver};
use crate::manager::Window;
use crate::menubar::MenuBarManager;
use crate::platform::PlatformCallbacks;

pub(crate) const ACTIVE_FRAME_INTERVAL: Duration = Duration::from_millis(16);
const FRESH_POLL_INTERVAL: Duration = Duration::from_millis(50);
const ORPHAN_RECONCILE_INTERVAL: Duration = Duration::from_secs(1);
const FOCUS_RECOVERY_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Component, Debug)]
pub(crate) struct FreshPollDeadline(Instant);

impl Default for FreshPollDeadline {
    fn default() -> Self {
        Self(Instant::now())
    }
}

impl FreshPollDeadline {
    pub(crate) fn due(&self, now: Instant) -> bool {
        now >= self.0
    }

    pub(crate) fn schedule_next(&mut self, now: Instant) {
        self.0 = now + FRESH_POLL_INTERVAL;
    }

    fn next_deadline(&self) -> Instant {
        self.0
    }
}

#[derive(Component, Debug)]
pub(crate) struct OrphanReconcileDeadline(Instant);

impl Default for OrphanReconcileDeadline {
    fn default() -> Self {
        Self(Instant::now() + ORPHAN_RECONCILE_INTERVAL)
    }
}

impl OrphanReconcileDeadline {
    pub(crate) fn due(&self, now: Instant) -> bool {
        now >= self.0
    }

    pub(crate) fn schedule_next(&mut self, now: Instant) {
        self.0 = now + ORPHAN_RECONCILE_INTERVAL;
    }

    fn next_deadline(&self) -> Instant {
        self.0
    }
}

#[derive(Resource, Debug)]
pub(crate) struct FocusRecoveryDeadline(Instant);

impl Default for FocusRecoveryDeadline {
    fn default() -> Self {
        Self(Instant::now())
    }
}

impl FocusRecoveryDeadline {
    pub(crate) fn due(&self, now: Instant) -> bool {
        now >= self.0
    }

    pub(crate) fn schedule_next(&mut self, now: Instant) {
        self.0 = now + FOCUS_RECOVERY_INTERVAL;
    }

    fn next_deadline(&self) -> Instant {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RuntimeActivity {
    pub frame_work: bool,
    pub nearest_deadline: Option<Instant>,
}

/// Latches synthetic Bevy messages produced after the `First`-stage native
/// event pump. The next pump consumes the latch and skips its blocking wait so
/// those already-published messages reach their readers without requiring an
/// unrelated native wake.
#[derive(Resource, Debug, Default)]
pub(super) struct SyntheticEventPending(bool);

impl SyntheticEventPending {
    pub(super) fn mark(&mut self) {
        self.0 = true;
    }

    fn take(&mut self) -> bool {
        std::mem::take(&mut self.0)
    }
}

impl RuntimeActivity {
    pub(crate) fn wait(self, now: Instant) -> Option<Duration> {
        if self.frame_work {
            Some(ACTIVE_FRAME_INTERVAL)
        } else {
            self.nearest_deadline
                .map(|deadline| deadline.saturating_duration_since(now))
        }
    }

    pub(crate) fn nearest(deadlines: impl IntoIterator<Item = Option<Instant>>) -> Option<Instant> {
        deadlines.into_iter().flatten().min()
    }
}

#[derive(SystemParam)]
pub(super) struct RuntimeWork<'w, 's> {
    repositioning: Query<'w, 's, (), With<RepositionMarker>>,
    resizing: Query<'w, 's, (), With<ResizeMarker>>,
    scrolling: Query<'w, 's, (), With<Scrolling>>,
    flash_messages: Query<'w, 's, (), With<FlashMessage>>,
    timeouts: Query<'w, 's, &'static Timeout>,
    fresh_polls: Query<'w, 's, &'static FreshPollDeadline, With<FreshMarker>>,
    refreshes: Query<'w, 's, &'static RefreshWindowSizes, With<ActiveWorkspaceMarker>>,
    observer_detaches: Query<'w, 's, &'static ObserverDetachRetry>,
    orphan_checks: Query<'w, 's, &'static OrphanReconcileDeadline, Without<ChildOf>>,
    windows: Query<'w, 's, (), With<Window>>,
    focused_windows: Query<'w, 's, (), (With<Window>, With<FocusedMarker>)>,
    focus_recovery: Option<Res<'w, FocusRecoveryDeadline>>,
    retries: Query<'w, 's, &'static RetryFrontSwitch>,
    verifications: Query<'w, 's, &'static VerifyWindowPosition>,
    initializing: Option<Res<'w, Initializing>>,
    restore: Option<Res<'w, crate::ecs::restore::SessionRestore>>,
    persistence: Option<Res<'w, crate::ecs::persistence::PersistenceState>>,
    menu_bar: Option<NonSend<'w, MenuBarManager>>,
}

fn runtime_activity(work: &RuntimeWork<'_, '_>, now: Instant) -> RuntimeActivity {
    let timeout_deadline = work.timeouts.iter().map(Timeout::next_deadline).min();
    let fresh_poll_deadline = work
        .fresh_polls
        .iter()
        .map(FreshPollDeadline::next_deadline)
        .min();
    let refresh_deadline = work
        .refreshes
        .iter()
        .map(RefreshWindowSizes::next_deadline)
        .min();
    let observer_detach_deadline = work
        .observer_detaches
        .iter()
        .map(ObserverDetachRetry::next_deadline)
        .min();
    let orphan_deadline = work
        .orphan_checks
        .iter()
        .map(OrphanReconcileDeadline::next_deadline)
        .min();
    let focus_recovery_deadline = (!work.windows.is_empty() && work.focused_windows.is_empty())
        .then(|| {
            work.focus_recovery
                .as_deref()
                .map(FocusRecoveryDeadline::next_deadline)
        })
        .flatten();
    let retry_deadline = work
        .retries
        .iter()
        .map(RetryFrontSwitch::next_deadline)
        .min();
    let verification_deadline = work
        .verifications
        .iter()
        .map(VerifyWindowPosition::next_deadline)
        .min();
    let restore_deadline = work
        .restore
        .as_deref()
        .map(super::restore::SessionRestore::next_deadline);
    let persistence_deadline = work
        .persistence
        .as_deref()
        .and_then(crate::ecs::persistence::PersistenceState::next_deadline);
    let updater_deadline = work
        .menu_bar
        .as_deref()
        .and_then(MenuBarManager::updater_deadline);
    let immediate_deferred = work
        .initializing
        .is_some()
        .then_some(now + ACTIVE_FRAME_INTERVAL);
    RuntimeActivity {
        frame_work: !work.repositioning.is_empty()
            || !work.resizing.is_empty()
            || !work.scrolling.is_empty()
            || !work.flash_messages.is_empty(),
        nearest_deadline: RuntimeActivity::nearest([
            timeout_deadline,
            fresh_poll_deadline,
            refresh_deadline,
            observer_detach_deadline,
            orphan_deadline,
            focus_recovery_deadline,
            retry_deadline,
            verification_deadline,
            restore_deadline,
            persistence_deadline,
            updater_deadline,
            immediate_deferred,
        ]),
    }
}

#[allow(clippy::needless_pass_by_value)]
pub(super) fn pump_events(
    mut exit: MessageWriter<AppExit>,
    mut messages: MessageWriter<Event>,
    incoming_events: Option<NonSend<EventReceiver>>,
    platform: Option<NonSendMut<Pin<Box<PlatformCallbacks>>>>,
    mut real_time: Option<ResMut<Time<Real>>>,
    mut synthetic_events: ResMut<SyntheticEventPending>,
    work: RuntimeWork,
) {
    let Some((ref mut platform, incoming_events)) = platform.zip(incoming_events) else {
        // No platform interface or incoming event pipe - probably executing in a unit test.
        return;
    };

    let now = Instant::now();
    let activity = runtime_activity(&work, now);
    let synthetic_pending = synthetic_events.take();
    let (received_events, should_exit, did_wait) =
        pump_receiver(&incoming_events, activity, now, synthetic_pending, |wait| {
            platform.pump_cocoa_event_loop(wait);
        });
    if did_wait && let Some(real_time) = real_time.as_mut() {
        real_time.update_with_instant(Instant::now());
    }
    if should_exit {
        exit.write(AppExit::Success);
    }
    messages.write_batch(received_events);
}

fn pump_receiver(
    receiver: &EventReceiver,
    activity: RuntimeActivity,
    now: Instant,
    synthetic_pending: bool,
    mut pump: impl FnMut(Option<Duration>),
) -> (Vec<Event>, bool, bool) {
    let generation_before_drain = receiver.generation();
    let (mut received_events, mut should_exit) = drain_event_channel(receiver);
    let should_wait = !synthetic_pending
        && received_events.is_empty()
        && !should_exit
        && receiver.generation() == generation_before_drain;
    if should_wait {
        pump(activity.wait(now));
        let (after_wait, exit_after_wait) = drain_event_channel(receiver);
        received_events = after_wait;
        should_exit = exit_after_wait;
    }
    (received_events, should_exit, should_wait)
}

fn drain_event_channel(receiver: &EventReceiver) -> (Vec<Event>, bool) {
    let mut received_events = Vec::new();
    let mut pending_mouse = None;
    let mut should_exit = false;
    loop {
        match receiver.try_recv() {
            Ok(Event::Exit) | Err(TryRecvError::Disconnected) => {
                should_exit = true;
                break;
            }
            Ok(event) if matches!(event, Event::MouseMoved { .. }) => {
                pending_mouse = Some(event);
            }
            Ok(event) => {
                received_events.extend(pending_mouse.take());
                received_events.push(event);
            }
            Err(TryRecvError::Empty) => break,
        }
    }
    received_events.extend(pending_mouse);
    (received_events, should_exit)
}

#[cfg(test)]
mod tests {
    use super::{
        ACTIVE_FRAME_INTERVAL, FreshPollDeadline, RuntimeActivity, RuntimeWork,
        SyntheticEventPending, pump_receiver, runtime_activity,
    };
    use crate::ecs::{ActiveWorkspaceMarker, RefreshWindowSizes, Scrolling, SendMessageTrigger};
    use crate::events::{Event, EventReceiver, EventSender};
    use bevy::app::{App, First, PostUpdate, PreUpdate, Update};
    use bevy::ecs::message::{MessageReader, MessageWriter, Messages};
    use bevy::ecs::resource::Resource;
    use bevy::ecs::schedule::IntoScheduleConfigs;
    use bevy::ecs::system::{Commands, Local, NonSend, Res, ResMut};
    use bevy::time::{Real, Time, TimeSystems, Virtual};
    use std::time::{Duration, Instant};

    #[test]
    fn idle_runtime_has_no_periodic_deadline() {
        let now = Instant::now();
        assert_eq!(
            RuntimeActivity {
                frame_work: false,
                nearest_deadline: None,
            }
            .wait(now),
            None
        );
    }

    #[test]
    fn frame_work_and_real_deadlines_have_distinct_waits() {
        let now = Instant::now();
        let deadline = now + Duration::from_secs(3);
        assert_eq!(
            RuntimeActivity {
                frame_work: true,
                nearest_deadline: Some(deadline),
            }
            .wait(now),
            Some(ACTIVE_FRAME_INTERVAL)
        );
        assert_eq!(
            RuntimeActivity {
                frame_work: false,
                nearest_deadline: Some(deadline),
            }
            .wait(now),
            Some(Duration::from_secs(3))
        );
    }

    #[test]
    fn absolute_deadline_is_charged_once_after_sleep() {
        let now = Instant::now();
        let deadline = now + Duration::from_millis(50);
        let activity = RuntimeActivity {
            frame_work: false,
            nearest_deadline: Some(deadline),
        };
        assert_eq!(activity.wait(now), Some(Duration::from_millis(50)));
        assert_eq!(activity.wait(deadline), Some(Duration::ZERO));
    }

    #[test]
    fn deferred_deadline_wakes_real_pump_core_and_delivers_event() {
        let (sender, receiver) = EventSender::new();
        let now = Instant::now();
        let activity = RuntimeActivity {
            frame_work: false,
            nearest_deadline: Some(now + Duration::from_millis(25)),
        };
        let mut observed_wait = None;

        let (events, should_exit, did_wait) =
            pump_receiver(&receiver, activity, now, false, |wait| {
                observed_wait = wait;
                sender
                    .send(Event::UpdaterStatusChanged)
                    .expect("deadline wake event should send");
            });

        assert_eq!(observed_wait, Some(Duration::from_millis(25)));
        assert!(!should_exit);
        assert!(did_wait);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::UpdaterStatusChanged))
        );
    }

    #[test]
    fn fresh_poll_exposes_monotonic_retry_deadline() {
        let mut poll = FreshPollDeadline(Instant::now());
        let now = poll.next_deadline();
        assert!(poll.due(now));
        poll.schedule_next(now);
        assert_eq!(poll.next_deadline(), now + Duration::from_millis(50));
        assert!(!poll.due(now));
    }

    #[derive(Resource, Default)]
    struct CapturedActivity(Option<RuntimeActivity>);

    #[allow(clippy::needless_pass_by_value)]
    fn capture_activity(work: RuntimeWork, mut captured: ResMut<CapturedActivity>) {
        captured.0 = Some(runtime_activity(&work, Instant::now()));
    }

    #[test]
    fn inactive_past_due_refresh_does_not_force_zero_wait() {
        let now = Instant::now();
        let active_deadline = now + Duration::from_secs(2);
        let mut app = App::new();
        app.init_resource::<CapturedActivity>()
            .add_systems(Update, capture_activity);
        app.world_mut().spawn(RefreshWindowSizes(now));
        app.world_mut()
            .spawn((RefreshWindowSizes(active_deadline), ActiveWorkspaceMarker));

        app.update();

        let activity = app.world().resource::<CapturedActivity>().0.unwrap();
        assert_eq!(activity.nearest_deadline, Some(active_deadline));
        assert!(activity.wait(now).is_some_and(|wait| wait > Duration::ZERO));
    }

    #[derive(Resource, Default)]
    struct ScrollProbe(f64);

    fn simulate_idle_wait_once(mut once: Local<bool>, mut real_time: ResMut<Time<Real>>) {
        if *once {
            return;
        }
        *once = true;
        std::thread::sleep(Duration::from_millis(40));
        real_time.update_with_instant(Instant::now());
    }

    #[allow(clippy::needless_pass_by_value)]
    fn advance_scroll_probe(time: Res<Time<Virtual>>, mut probe: ResMut<ScrollProbe>) {
        probe.0 += time.delta_secs_f64() * 1_000.0;
    }

    #[test]
    fn long_idle_wait_is_not_charged_to_next_scroll_frame() {
        let mut app = App::new();
        app.add_plugins(bevy::MinimalPlugins)
            .init_resource::<ScrollProbe>()
            .add_systems(PreUpdate, simulate_idle_wait_once)
            .add_systems(Update, advance_scroll_probe);
        app.world_mut().spawn(Scrolling::default());

        app.update();
        app.update();

        assert!(
            app.world().resource::<ScrollProbe>().0 < 20.0,
            "40ms idle wait leaked into the next animation delta"
        );
    }

    #[derive(Resource, Default)]
    struct EventPipelineProbe {
        waits: usize,
        first_reader: usize,
        second_reader: usize,
    }

    #[allow(clippy::needless_pass_by_value)]
    fn publish_native_events(
        receiver: NonSend<EventReceiver>,
        mut messages: MessageWriter<Event>,
        mut synthetic_events: ResMut<SyntheticEventPending>,
        mut probe: ResMut<EventPipelineProbe>,
    ) {
        let activity = RuntimeActivity {
            frame_work: false,
            nearest_deadline: None,
        };
        let synthetic_pending = synthetic_events.take();
        let (events, _, _) = pump_receiver(
            &receiver,
            activity,
            Instant::now(),
            synthetic_pending,
            |_| {
                probe.waits += 1;
            },
        );
        messages.write_batch(events);
    }

    fn write_synthetic_after_pump(mut written: Local<bool>, mut commands: Commands) {
        if *written {
            return;
        }
        *written = true;
        commands.trigger(SendMessageTrigger(Event::UpdaterStatusChanged));
    }

    fn first_native_reader(
        mut messages: MessageReader<Event>,
        mut probe: ResMut<EventPipelineProbe>,
    ) {
        probe.first_reader += messages
            .read()
            .filter(|event| matches!(event, Event::UpdaterStatusChanged))
            .count();
    }

    fn second_native_reader(
        mut messages: MessageReader<Event>,
        mut probe: ResMut<EventPipelineProbe>,
    ) {
        probe.second_reader += messages
            .read()
            .filter(|event| matches!(event, Event::UpdaterStatusChanged))
            .count();
    }

    #[test]
    fn first_stage_pump_publishes_before_every_preupdate_reader() {
        let (sender, receiver) = EventSender::new();
        sender.send(Event::UpdaterStatusChanged).unwrap();
        let mut app = App::new();
        app.add_plugins(bevy::MinimalPlugins)
            .init_resource::<Messages<Event>>()
            .init_resource::<EventPipelineProbe>()
            .init_resource::<SyntheticEventPending>()
            .insert_non_send_resource(receiver)
            .add_systems(
                First,
                publish_native_events
                    .after(TimeSystems)
                    .after(bevy::ecs::message::message_update_system),
            )
            .add_systems(PreUpdate, (first_native_reader, second_native_reader));

        app.update();

        let probe = app.world().resource::<EventPipelineProbe>();
        assert_eq!(probe.waits, 0, "queued native work must prevent sleeping");
        assert_eq!(probe.first_reader, 1);
        assert_eq!(probe.second_reader, 1);
    }

    #[test]
    fn synthetic_event_after_pump_prevents_next_frame_wait() {
        let (_sender, receiver) = EventSender::new();
        let mut app = App::new();
        app.add_plugins(bevy::MinimalPlugins)
            .init_resource::<Messages<Event>>()
            .init_resource::<EventPipelineProbe>()
            .init_resource::<SyntheticEventPending>()
            .insert_non_send_resource(receiver)
            .add_observer(crate::ecs::triggers::send_message_trigger)
            .add_systems(
                First,
                publish_native_events
                    .after(TimeSystems)
                    .after(bevy::ecs::message::message_update_system),
            )
            .add_systems(PreUpdate, first_native_reader)
            .add_systems(PostUpdate, write_synthetic_after_pump);

        app.update();
        let waits_after_first_pump = app.world().resource::<EventPipelineProbe>().waits;
        assert_eq!(
            app.world().resource::<EventPipelineProbe>().first_reader,
            0,
            "synthetic event is intentionally produced after the first pump"
        );

        app.update();

        let probe = app.world().resource::<EventPipelineProbe>();
        assert_eq!(
            probe.waits, waits_after_first_pump,
            "latched synthetic work must prevent the next pump from waiting"
        );
        assert_eq!(probe.first_reader, 1);
    }
}
