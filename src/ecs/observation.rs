use bevy::ecs::component::Component;
use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::Children;
use bevy::ecs::message::MessageReader;
use bevy::ecs::query::{Has, With, Without};
use bevy::ecs::system::{Commands, NonSend, Query, SystemParam};
use std::time::{Duration, Instant};
use tracing::{error, warn};

use super::{ApplicationObserved, BProcess, FreshMarker, Timeout, Unmanaged};
use crate::config::Config;
use crate::ecs::runtime::FreshPollDeadline;
use crate::errors::Result as AppResult;
use crate::events::Event;
use crate::manager::{Application, Process, Window};
use crate::platform::{AxMainThread, WinID};

const DETACH_RETRY_INTERVAL: Duration = Duration::from_millis(250);
const DETACH_RETRY_LIMIT: u8 = 3;

#[derive(Component, Debug)]
pub(crate) struct ObserverDetachRetry {
    deadline: Instant,
    attempts: u8,
    remove_application_marker: bool,
}

impl ObserverDetachRetry {
    fn new(remove_application_marker: bool) -> Self {
        Self {
            deadline: Instant::now() + DETACH_RETRY_INTERVAL,
            attempts: 0,
            remove_application_marker,
        }
    }

    pub(crate) fn next_deadline(&self) -> Instant {
        self.deadline
    }

    fn due(&self, now: Instant) -> bool {
        now >= self.deadline
    }

    fn failed_attempt(&mut self, now: Instant) -> bool {
        self.attempts += 1;
        self.deadline = now + DETACH_RETRY_INTERVAL;
        self.attempts >= DETACH_RETRY_LIMIT
    }
}

fn schedule_detach_retry(
    app_entity: Entity,
    remove_application_marker: bool,
    commands: &mut Commands,
) {
    if let Ok(mut entity_commands) = commands.get_entity(app_entity) {
        entity_commands.try_insert(ObserverDetachRetry::new(remove_application_marker));
    }
}

/// Owns broad application AX observer reconciliation. Broad notifications are
/// retained only for the frontmost app and applications with managed windows.
#[derive(SystemParam)]
pub(crate) struct ApplicationObservationScope<'w, 's> {
    applications: Query<
        'w,
        's,
        (
            Entity,
            &'static mut Application,
            Option<&'static Children>,
            Has<ApplicationObserved>,
        ),
    >,
    managed_windows: Query<'w, 's, (), (With<Window>, Without<Unmanaged>)>,
}

impl ApplicationObservationScope<'_, '_> {
    pub(crate) fn activate(
        &mut self,
        target: Option<Entity>,
        config: &Config,
        _main_thread: &AxMainThread,
        commands: &mut Commands,
    ) -> Option<AppResult<WinID>> {
        for (entity, mut app, children, observed) in &mut self.applications {
            let owns_managed = children.is_some_and(|children| {
                children
                    .iter()
                    .any(|child| self.managed_windows.get(*child).is_ok())
            });
            let required = Some(entity) == target || owns_managed;
            if required && !observed && app.observe().is_ok_and(|good| good) {
                if let Ok(mut entity_commands) = commands.get_entity(entity) {
                    entity_commands.try_insert(ApplicationObserved);
                }
            } else if !required && observed {
                if app.unobserve() {
                    if let Ok(mut entity_commands) = commands.get_entity(entity) {
                        entity_commands.try_remove::<ApplicationObserved>();
                    }
                } else {
                    schedule_detach_retry(entity, true, commands);
                }
            }
        }

        let target = target?;
        let Ok((_, app, _, _)) = self.applications.get_mut(target) else {
            return None;
        };
        let windows = app.window_list(config);
        if !windows.is_empty() {
            commands.trigger(super::SpawnWindowTrigger(windows));
        }
        Some(app.focused_window_id())
    }
}

pub(crate) fn attach_managed_window(
    app_entity: Entity,
    app: &mut Application,
    window: &Window,
    observed: bool,
    main_thread: &AxMainThread,
    commands: &mut Commands,
) {
    ensure_application_observer(app_entity, app, observed, main_thread, commands);
    if app.observe_window(window).is_err() {
        warn!("Error observing managed window {}.", window.id());
    }
}

pub(crate) fn ensure_application_observer(
    app_entity: Entity,
    app: &mut Application,
    observed: bool,
    _main_thread: &AxMainThread,
    commands: &mut Commands,
) {
    if !observed
        && app.observe().is_ok_and(|good| good)
        && let Ok(mut entity_commands) = commands.get_entity(app_entity)
    {
        entity_commands.try_insert(ApplicationObserved);
    }
}

pub(crate) fn detach_unmanaged_window(
    app_entity: Entity,
    app: &mut Application,
    window: &Window,
    observed: bool,
    owns_other_managed_windows: bool,
    _main_thread: &AxMainThread,
    commands: &mut Commands,
) {
    let mut retry = !app.unobserve_window(window);
    let mut remove_application_marker = false;
    if observed && !app.is_frontmost() && !owns_other_managed_windows {
        if app.unobserve() {
            if let Ok(mut entity_commands) = commands.get_entity(app_entity) {
                entity_commands.try_remove::<ApplicationObserved>();
            }
        } else {
            retry = true;
            remove_application_marker = true;
        }
    }
    if retry {
        schedule_detach_retry(app_entity, remove_application_marker, commands);
    }
}

pub(crate) fn shutdown_application_observers<'a>(
    app: &mut Application,
    windows: impl IntoIterator<Item = &'a Window>,
    _main_thread: &AxMainThread,
) {
    for window in windows {
        _ = app.unobserve_window(window);
    }
    _ = app.unobserve();
}

#[allow(clippy::needless_pass_by_value)]
pub(crate) fn retry_observer_detaches(
    _main_thread: NonSend<AxMainThread>,
    mut applications: Query<(
        Entity,
        &mut Application,
        &mut ObserverDetachRetry,
        Has<ApplicationObserved>,
    )>,
    mut commands: Commands,
) {
    let now = Instant::now();
    for (entity, mut app, mut retry, observed) in &mut applications {
        if !retry.due(now) {
            continue;
        }
        if app.retry_observer_removals() {
            if let Ok(mut entity_commands) = commands.get_entity(entity) {
                entity_commands.try_remove::<ObserverDetachRetry>();
                if retry.remove_application_marker && observed {
                    entity_commands.try_remove::<ApplicationObserved>();
                }
            }
        } else if retry.failed_attempt(now) {
            error!(
                "AX observer detach still ambiguous after {DETACH_RETRY_LIMIT} retries; retaining ECS observation marker/context for safety"
            );
            if let Ok(mut entity_commands) = commands.get_entity(entity) {
                entity_commands.try_remove::<ObserverDetachRetry>();
            }
        } else {
            warn!(
                attempt = retry.attempts,
                "AX observer detach remains ambiguous; scheduled bounded retry"
            );
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
pub(crate) fn application_event_trigger(
    main_thread: NonSend<AxMainThread>,
    mut messages: MessageReader<Event>,
    processes: Query<(&BProcess, Entity, Option<&Children>)>,
    mut applications: Query<(&mut Application, Option<&Children>)>,
    windows: Query<&Window>,
    mut commands: Commands,
) {
    const PROCESS_READY_TIMEOUT_SEC: u64 = 5;
    let find_process = |psn| {
        processes
            .iter()
            .find(|(BProcess(process), _, _)| process.psn() == psn)
    };

    for event in messages.read() {
        match event {
            Event::ApplicationLaunched { psn, observer } if find_process(*psn).is_none() => {
                let process: BProcess = Process::new(psn, observer.clone()).into();
                let timeout = Timeout::new(
                    Duration::from_secs(PROCESS_READY_TIMEOUT_SEC),
                    Some(format!(
                        "Process '{}' did not become ready in {PROCESS_READY_TIMEOUT_SEC}s.",
                        process.name()
                    )),
                    &mut commands,
                );
                commands.spawn((FreshMarker, FreshPollDeadline::default(), timeout, process));
            }
            Event::ApplicationTerminated { psn } => {
                if let Some((_, entity, app_entities)) = find_process(*psn) {
                    if let Some(app_entities) = app_entities {
                        for app_entity in app_entities {
                            if let Ok((mut app, window_entities)) =
                                applications.get_mut(*app_entity)
                            {
                                let owned_windows = window_entities
                                    .into_iter()
                                    .flat_map(|children| children.iter())
                                    .filter_map(|window_entity| windows.get(*window_entity).ok());
                                shutdown_application_observers(
                                    &mut app,
                                    owned_windows,
                                    &main_thread,
                                );
                            }
                        }
                    }
                    if let Ok(mut entity_commands) = commands.get_entity(entity) {
                        entity_commands.try_despawn();
                    }
                }
            }
            _ => (),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ApplicationObservationScope, DETACH_RETRY_INTERVAL, DETACH_RETRY_LIMIT,
        ObserverDetachRetry, attach_managed_window, detach_unmanaged_window,
        retry_observer_detaches, shutdown_application_observers,
    };
    use crate::config::Config;
    use crate::ecs::ApplicationObserved;
    use crate::manager::app::MockApplicationApi;
    use crate::manager::{Application, MockWindowApi, Window};
    use crate::platform::AxMainThread;
    use bevy::app::{App, Update};
    use bevy::ecs::entity::Entity;
    use bevy::ecs::query::Has;
    use bevy::ecs::resource::Resource;
    use bevy::ecs::system::{Commands, Local, NonSend, Query, Res, Single};
    use std::time::Instant;

    fn required(frontmost: bool, owns_managed: bool) -> bool {
        frontmost || owns_managed
    }

    #[test]
    fn observation_scope_excludes_irrelevant_background_apps() {
        assert!(required(true, false));
        assert!(required(false, true));
        assert!(!required(false, false));
    }

    fn app_with_token() -> App {
        let mut app = App::new();
        app.insert_non_send_resource(AxMainThread::for_tests())
            .insert_resource(Config::default());
        app
    }

    #[allow(clippy::needless_pass_by_value)]
    fn attach_system(
        main_thread: NonSend<AxMainThread>,
        mut apps: Query<(Entity, &mut Application, Has<ApplicationObserved>)>,
        window: Single<&Window>,
        mut commands: Commands,
    ) {
        for (entity, mut app, observed) in &mut apps {
            attach_managed_window(
                entity,
                &mut app,
                *window,
                observed,
                &main_thread,
                &mut commands,
            );
        }
    }

    #[test]
    fn startup_managed_owner_attaches_broad_observer_once() {
        let mut mock = MockApplicationApi::new();
        mock.expect_observe().times(1).returning(|| Ok(true));
        mock.expect_observe_window()
            .times(2)
            .returning(|_| Ok(true));
        let mut app = app_with_token();
        let app_entity = app.world_mut().spawn(Application::new(Box::new(mock))).id();
        app.world_mut()
            .spawn(Window::new(Box::new(MockWindowApi::new())));
        app.add_systems(Update, attach_system);
        app.update();
        app.update();
        assert!(
            app.world()
                .entity(app_entity)
                .contains::<ApplicationObserved>()
        );
    }

    #[allow(clippy::needless_pass_by_value)]
    fn detach_once_system(
        main_thread: NonSend<AxMainThread>,
        mut done: Local<bool>,
        mut app: Single<(Entity, &mut Application, Has<ApplicationObserved>)>,
        window: Single<&Window>,
        mut commands: Commands,
    ) {
        if *done {
            return;
        }
        *done = true;
        let app_entity = app.0;
        let observed = app.2;
        detach_unmanaged_window(
            app_entity,
            &mut app.1,
            *window,
            observed,
            false,
            &main_thread,
            &mut commands,
        );
    }

    #[test]
    fn last_background_managed_window_detaches_window_and_app() {
        let mut mock = MockApplicationApi::new();
        mock.expect_unobserve_window().times(1).return_const(true);
        mock.expect_is_frontmost().times(1).return_const(false);
        mock.expect_unobserve().times(1).return_const(true);
        let mut app = app_with_token();
        let app_entity = app
            .world_mut()
            .spawn((Application::new(Box::new(mock)), ApplicationObserved))
            .id();
        app.world_mut()
            .spawn(Window::new(Box::new(MockWindowApi::new())));
        app.add_systems(Update, detach_once_system);
        app.update();
        assert!(
            !app.world()
                .entity(app_entity)
                .contains::<ApplicationObserved>()
        );
    }

    #[derive(Resource)]
    struct ActivationTarget(Option<Entity>);

    #[allow(clippy::needless_pass_by_value)]
    fn activate_system(
        target: Res<ActivationTarget>,
        config: Res<Config>,
        main_thread: NonSend<AxMainThread>,
        mut scope: ApplicationObservationScope,
        mut commands: Commands,
    ) {
        scope.activate(target.0, &config, &main_thread, &mut commands);
    }

    fn activation_mock(observe: usize, unobserve: usize, activations: usize) -> Application {
        let mut mock = MockApplicationApi::new();
        mock.expect_observe().times(observe).returning(|| Ok(true));
        mock.expect_unobserve().times(unobserve).return_const(true);
        mock.expect_window_list()
            .times(activations)
            .returning(|_| Vec::new());
        mock.expect_focused_window_id()
            .times(activations)
            .returning(|| Ok(1));
        Application::new(Box::new(mock))
    }

    #[test]
    fn repeated_front_switches_are_idempotent_and_detach_previous_app() {
        let mut app = app_with_token();
        let first = app.world_mut().spawn(activation_mock(1, 1, 2)).id();
        let second = app.world_mut().spawn(activation_mock(1, 0, 1)).id();
        app.insert_resource(ActivationTarget(Some(first)))
            .add_systems(Update, activate_system);
        app.update();
        app.update();
        app.world_mut().resource_mut::<ActivationTarget>().0 = Some(second);
        app.update();
        assert!(!app.world().entity(first).contains::<ApplicationObserved>());
        assert!(app.world().entity(second).contains::<ApplicationObserved>());
    }

    #[test]
    fn switching_to_untracked_process_detaches_unowned_previous_observer() {
        let mut app = app_with_token();
        let tracked = app.world_mut().spawn(activation_mock(1, 1, 1)).id();
        app.insert_resource(ActivationTarget(Some(tracked)))
            .add_systems(Update, activate_system);
        app.update();
        assert!(
            app.world()
                .entity(tracked)
                .contains::<ApplicationObserved>()
        );

        app.world_mut().resource_mut::<ActivationTarget>().0 = None;
        app.update();
        assert!(
            !app.world()
                .entity(tracked)
                .contains::<ApplicationObserved>()
        );
    }

    #[test]
    fn termination_shutdown_detaches_all_window_and_application_observers() {
        let mut mock = MockApplicationApi::new();
        mock.expect_unobserve_window().times(1).return_const(true);
        mock.expect_unobserve().times(1).return_const(true);
        let mut app = Application::new(Box::new(mock));
        let window = Window::new(Box::new(MockWindowApi::new()));
        let main_thread = AxMainThread::for_tests();
        shutdown_application_observers(&mut app, [&window], &main_thread);
    }

    #[test]
    fn ambiguous_detach_retry_is_bounded_and_non_spinning() {
        let mut retry = ObserverDetachRetry::new(true);
        let first = retry.next_deadline();
        assert!(first > Instant::now());
        assert!(!retry.failed_attempt(first));
        assert_eq!(retry.next_deadline(), first + DETACH_RETRY_INTERVAL);
        let second = retry.next_deadline();
        assert!(!retry.failed_attempt(second));
        let third = retry.next_deadline();
        assert!(retry.failed_attempt(third));
    }

    #[test]
    fn successful_retry_reconciles_context_and_ecs_marker() {
        let mut mock = MockApplicationApi::new();
        mock.expect_retry_observer_removals()
            .times(1)
            .return_const(true);
        let mut retry = ObserverDetachRetry::new(true);
        retry.deadline = Instant::now();
        let mut app = app_with_token();
        let entity = app
            .world_mut()
            .spawn((Application::new(Box::new(mock)), ApplicationObserved, retry))
            .id();
        app.add_systems(Update, retry_observer_detaches);

        app.update();

        assert!(!app.world().entity(entity).contains::<ObserverDetachRetry>());
        assert!(!app.world().entity(entity).contains::<ApplicationObserved>());
    }

    #[test]
    fn exhausted_retry_stops_waking_but_keeps_observed_marker() {
        let mut mock = MockApplicationApi::new();
        mock.expect_retry_observer_removals()
            .times(usize::from(DETACH_RETRY_LIMIT))
            .return_const(false);
        let mut retry = ObserverDetachRetry::new(true);
        retry.deadline = Instant::now();
        let mut app = app_with_token();
        let entity = app
            .world_mut()
            .spawn((Application::new(Box::new(mock)), ApplicationObserved, retry))
            .id();
        app.add_systems(Update, retry_observer_detaches);

        for _ in 0..DETACH_RETRY_LIMIT {
            app.world_mut()
                .get_mut::<ObserverDetachRetry>(entity)
                .unwrap()
                .deadline = Instant::now();
            app.update();
        }

        assert!(!app.world().entity(entity).contains::<ObserverDetachRetry>());
        assert!(app.world().entity(entity).contains::<ApplicationObserved>());
    }
}
