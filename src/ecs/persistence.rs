use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bevy::app::AppExit;
use bevy::ecs::change_detection::DetectChanges;
use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::lifecycle::RemovedComponents;
use bevy::ecs::message::MessageReader;
use bevy::ecs::query::{Changed, Has, Or, With, Without};
use bevy::ecs::resource::Resource;
use bevy::ecs::system::{Commands, NonSend, Query, Res, ResMut};
use tracing::{debug, warn};

use super::layout::LayoutStrip;
use super::params::Windows;
use super::state::PaneruState;
use super::{ActiveDisplayMarker, ActiveWorkspaceMarker, Bounds, Position, Unmanaged, WidthRatio};
use crate::config::Config;
use crate::events::Event;
use crate::manager::{Application, Display};
use crate::platform::AxMainThread;

const SAVE_DEBOUNCE: Duration = Duration::from_millis(250);
const RETRY_INITIAL: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(60);

pub(crate) trait StateStoreApi: Send + Sync {
    fn load(&self) -> Option<PaneruState>;
    fn save(&self, state: &PaneruState) -> io::Result<()>;
}

#[derive(Default)]
struct FileStateStore;

impl StateStoreApi for FileStateStore {
    fn load(&self) -> Option<PaneruState> {
        PaneruState::load_from_file(&PaneruState::default_state_file_path())
    }

    fn save(&self, state: &PaneruState) -> io::Result<()> {
        state.save_to_file(&PaneruState::default_state_file_path())
    }
}

#[derive(Resource)]
pub(crate) struct PersistenceStore(Arc<dyn StateStoreApi>);

impl Default for PersistenceStore {
    fn default() -> Self {
        Self(Arc::new(FileStateStore))
    }
}

#[derive(Debug, Resource)]
pub(crate) struct PersistenceState {
    dirty: bool,
    save_at: Option<Instant>,
    retry_delay: Duration,
}

impl Default for PersistenceState {
    fn default() -> Self {
        Self {
            dirty: false,
            save_at: None,
            retry_delay: RETRY_INITIAL,
        }
    }
}

impl PersistenceState {
    pub(crate) fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.save_at
    }

    fn mark_changed(&mut self, enabled: bool, now: Instant) {
        if enabled {
            self.dirty = true;
            self.save_at = Some(now + SAVE_DEBOUNCE);
            self.retry_delay = RETRY_INITIAL;
        } else {
            self.clear();
        }
    }

    fn due(&self, now: Instant) -> bool {
        self.dirty && self.save_at.is_some_and(|deadline| now >= deadline)
    }

    fn saved(&mut self) {
        self.clear();
    }

    fn save_failed(&mut self, now: Instant) {
        self.dirty = true;
        self.save_at = Some(now + self.retry_delay);
        self.retry_delay = self.retry_delay.saturating_mul(2).min(RETRY_MAX);
    }

    fn clear(&mut self) {
        self.dirty = false;
        self.save_at = None;
        self.retry_delay = RETRY_INITIAL;
    }
}

#[allow(clippy::needless_pass_by_value)]
pub(crate) fn load_restore_state(
    config: Res<Config>,
    store: Option<Res<PersistenceStore>>,
    mut commands: Commands,
) {
    if !config.restore_enabled() {
        return;
    }
    if let Some(state) = store.and_then(|store| store.0.load()) {
        commands.insert_resource(state);
    }
}

#[allow(
    clippy::needless_pass_by_value,
    clippy::too_many_arguments,
    clippy::type_complexity
)]
pub(crate) fn track_dirty_state(
    config: Res<Config>,
    structural_changes: Query<
        (),
        Or<(
            Changed<LayoutStrip>,
            Changed<Display>,
            Changed<ActiveWorkspaceMarker>,
            Changed<ActiveDisplayMarker>,
        )>,
    >,
    strip_parent_changes: Query<(), (With<LayoutStrip>, Changed<ChildOf>)>,
    managed_geometry_changes: Query<
        (),
        (
            Without<Unmanaged>,
            Or<(Changed<Position>, Changed<Bounds>, Changed<WidthRatio>)>,
        ),
    >,
    mut removed_strips: RemovedComponents<LayoutStrip>,
    mut removed_positions: RemovedComponents<Position>,
    mut removed_bounds: RemovedComponents<Bounds>,
    mut removed_widths: RemovedComponents<WidthRatio>,
    mut removed_parents: RemovedComponents<ChildOf>,
    mut removed_displays: RemovedComponents<Display>,
    mut removed_active_workspaces: RemovedComponents<ActiveWorkspaceMarker>,
    mut removed_active_displays: RemovedComponents<ActiveDisplayMarker>,
    mut identity_events: MessageReader<Event>,
    mut state: ResMut<PersistenceState>,
) {
    let enabled = config.restore_enabled();
    let removed = removed_strips.read().next().is_some()
        || removed_positions.read().next().is_some()
        || removed_bounds.read().next().is_some()
        || removed_widths.read().next().is_some()
        || removed_parents.read().next().is_some()
        || removed_displays.read().next().is_some()
        || removed_active_workspaces.read().next().is_some()
        || removed_active_displays.read().next().is_some();
    let identity_changed = identity_events
        .read()
        .any(|event| matches!(event, Event::WindowTitleChanged { .. }));
    if !enabled {
        state.mark_changed(false, Instant::now());
    } else if config.is_changed()
        || !structural_changes.is_empty()
        || !strip_parent_changes.is_empty()
        || !managed_geometry_changes.is_empty()
        || removed
        || identity_changed
    {
        state.mark_changed(true, Instant::now());
    }
}

#[allow(clippy::needless_pass_by_value, clippy::type_complexity)]
pub(crate) fn flush_due_state(
    _main_thread: NonSend<AxMainThread>,
    config: Res<Config>,
    store: Option<Res<PersistenceStore>>,
    mut state: ResMut<PersistenceState>,
    snapshot: PersistenceSnapshot,
) {
    let now = Instant::now();
    if !config.restore_enabled() || !state.due(now) {
        return;
    }
    save_snapshot(store.as_deref(), &snapshot, &mut state, now);
}

#[allow(clippy::needless_pass_by_value, clippy::type_complexity)]
pub(crate) fn flush_on_exit(
    _main_thread: NonSend<AxMainThread>,
    mut exits: MessageReader<AppExit>,
    config: Res<Config>,
    store: Option<Res<PersistenceStore>>,
    mut state: ResMut<PersistenceState>,
    snapshot: PersistenceSnapshot,
) {
    if exits.read().next().is_none() || !config.restore_enabled() || !state.is_dirty() {
        return;
    }
    save_snapshot(store.as_deref(), &snapshot, &mut state, Instant::now());
}

#[derive(bevy::ecs::system::SystemParam)]
#[allow(clippy::type_complexity)]
pub(crate) struct PersistenceSnapshot<'w, 's> {
    workspaces: Query<
        'w,
        's,
        (
            Option<&'static ChildOf>,
            &'static LayoutStrip,
            Option<&'static Position>,
            Has<ActiveWorkspaceMarker>,
        ),
    >,
    displays: Query<'w, 's, (&'static Display, Entity, Has<ActiveDisplayMarker>)>,
    windows: Windows<'w, 's>,
    apps: Query<'w, 's, &'static Application>,
}

fn save_snapshot(
    store: Option<&PersistenceStore>,
    snapshot: &PersistenceSnapshot,
    state: &mut PersistenceState,
    now: Instant,
) {
    let Some(store) = store else {
        state.save_failed(now);
        warn!("persistence store missing; retry scheduled");
        return;
    };
    let snapshot = PaneruState::extract(
        &snapshot.workspaces,
        &snapshot.displays,
        &snapshot.windows,
        &snapshot.apps,
    );
    match store.0.save(&snapshot) {
        Ok(()) => {
            state.saved();
            debug!("saved dirty state");
        }
        Err(err) => {
            state.save_failed(now);
            warn!(%err, "failed to save dirty state; retry scheduled");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        PersistenceState, PersistenceStore, RETRY_INITIAL, SAVE_DEBOUNCE, StateStoreApi,
        flush_due_state, flush_on_exit, load_restore_state, track_dirty_state,
    };
    use crate::config::Config;
    use crate::ecs::state::PaneruState;
    use crate::ecs::{Position, Unmanaged};
    use crate::events::Event;
    use crate::platform::AxMainThread;
    use bevy::MinimalPlugins;
    use bevy::app::{App, AppExit, Startup, Update};
    use bevy::ecs::message::Messages;
    use bevy::ecs::schedule::IntoScheduleConfigs;
    use bevy::math::IVec2;
    use std::io;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    #[derive(Default)]
    struct TestStore {
        loads: AtomicUsize,
        saves: AtomicUsize,
        fail_saves: AtomicBool,
    }

    impl StateStoreApi for TestStore {
        fn load(&self) -> Option<PaneruState> {
            self.loads.fetch_add(1, Ordering::Relaxed);
            None
        }

        fn save(&self, _: &PaneruState) -> io::Result<()> {
            self.saves.fetch_add(1, Ordering::Relaxed);
            if self.fail_saves.load(Ordering::Relaxed) {
                Err(io::Error::other("injected save failure"))
            } else {
                Ok(())
            }
        }
    }

    fn persistence_app(config: Config, store: Arc<TestStore>) -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .init_resource::<Messages<AppExit>>()
            .init_resource::<Messages<Event>>()
            .init_resource::<PersistenceState>()
            .insert_resource(config)
            .insert_resource(PersistenceStore(store))
            .insert_non_send_resource(AxMainThread::for_tests())
            .add_systems(
                Update,
                (track_dirty_state, flush_due_state, flush_on_exit).chain(),
            );
        app
    }

    fn make_due(app: &mut App) {
        app.world_mut().resource_mut::<PersistenceState>().save_at = Some(
            Instant::now()
                .checked_sub(Duration::from_millis(1))
                .unwrap(),
        );
    }

    #[test]
    fn trailing_debounce_moves_after_every_change() {
        let now = Instant::now();
        let mut state = PersistenceState::default();
        state.mark_changed(true, now);
        state.mark_changed(true, now + Duration::from_millis(200));
        assert!(!state.due(now + SAVE_DEBOUNCE));
        assert!(state.due(now + Duration::from_millis(200) + SAVE_DEBOUNCE));
    }

    #[test]
    fn disabled_restore_never_keeps_dirty_work() {
        let now = Instant::now();
        let mut state = PersistenceState::default();
        state.mark_changed(true, now);
        state.mark_changed(false, now);
        assert!(!state.is_dirty());
        assert_eq!(state.next_deadline(), None);
    }

    #[test]
    fn failed_save_stays_dirty_with_bounded_backoff() {
        let now = Instant::now();
        let mut state = PersistenceState::default();
        state.mark_changed(true, now);
        state.save_failed(now + SAVE_DEBOUNCE);
        assert!(state.is_dirty());
        assert_eq!(
            state.next_deadline(),
            Some(now + SAVE_DEBOUNCE + RETRY_INITIAL)
        );
    }

    #[test]
    fn schedule_continuous_mutation_does_not_save_until_quiet() {
        let store = Arc::new(TestStore::default());
        let mut app = persistence_app(Config::default(), Arc::clone(&store));
        let entity = app.world_mut().spawn(Position(IVec2::ZERO)).id();
        app.update();
        make_due(&mut app);

        app.world_mut()
            .entity_mut(entity)
            .get_mut::<Position>()
            .unwrap()
            .0
            .x += 1;
        app.update();
        assert_eq!(store.saves.load(Ordering::Relaxed), 0);

        make_due(&mut app);
        app.update();
        assert_eq!(store.saves.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn passthrough_geometry_does_not_dirty_persistence() {
        let store = Arc::new(TestStore::default());
        let mut app = persistence_app(Config::default(), store);
        app.update();
        app.world_mut().resource_mut::<PersistenceState>().clear();

        let entity = app
            .world_mut()
            .spawn((Position(IVec2::ZERO), Unmanaged::Passthrough))
            .id();
        app.update();
        assert!(!app.world().resource::<PersistenceState>().is_dirty());

        app.world_mut()
            .entity_mut(entity)
            .get_mut::<Position>()
            .unwrap()
            .0
            .x += 1;
        app.update();
        assert!(!app.world().resource::<PersistenceState>().is_dirty());
    }

    #[test]
    fn schedule_same_frame_change_and_exit_flushes_latest_state() {
        let store = Arc::new(TestStore::default());
        let mut app = persistence_app(Config::default(), Arc::clone(&store));
        app.update();
        app.world_mut().spawn(Position(IVec2::new(42, 0)));
        app.world_mut()
            .resource_mut::<Messages<AppExit>>()
            .write(AppExit::Success);
        app.update();
        assert_eq!(store.saves.load(Ordering::Relaxed), 1);
        assert!(!app.world().resource::<PersistenceState>().is_dirty());
    }

    #[test]
    fn disabled_restore_never_reads_or_writes_store() {
        let config = Config::try_from(
            r"
[options]

[restore]
enabled = false

[bindings]
",
        )
        .unwrap();
        let store = Arc::new(TestStore::default());
        let mut app = persistence_app(config, Arc::clone(&store));
        app.add_systems(Startup, load_restore_state);
        app.world_mut().spawn(Position(IVec2::new(7, 0)));
        app.world_mut()
            .resource_mut::<Messages<AppExit>>()
            .write(AppExit::Success);
        app.update();
        assert_eq!(store.loads.load(Ordering::Relaxed), 0);
        assert_eq!(store.saves.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn failed_due_save_is_retried_immediately_on_shutdown() {
        let store = Arc::new(TestStore::default());
        store.fail_saves.store(true, Ordering::Relaxed);
        let mut app = persistence_app(Config::default(), Arc::clone(&store));
        app.update();
        make_due(&mut app);
        app.update();
        assert_eq!(store.saves.load(Ordering::Relaxed), 1);
        assert!(app.world().resource::<PersistenceState>().is_dirty());

        store.fail_saves.store(false, Ordering::Relaxed);
        app.world_mut()
            .resource_mut::<Messages<AppExit>>()
            .write(AppExit::Success);
        app.update();
        assert_eq!(store.saves.load(Ordering::Relaxed), 2);
        assert!(!app.world().resource::<PersistenceState>().is_dirty());
    }

    #[test]
    fn expired_dirty_state_without_store_uses_backoff_instead_of_zero_wait() {
        let store = Arc::new(TestStore::default());
        let mut app = persistence_app(Config::default(), store);
        app.update();
        make_due(&mut app);
        app.world_mut().remove_resource::<PersistenceStore>();
        let before = Instant::now();

        app.update();

        let state = app.world().resource::<PersistenceState>();
        assert!(state.is_dirty());
        assert!(
            state
                .next_deadline()
                .is_some_and(|deadline| deadline > before)
        );
    }

    #[test]
    fn production_title_notification_marks_restore_identity_dirty() {
        let store = Arc::new(TestStore::default());
        let mut app = persistence_app(Config::default(), store);
        app.update();
        let window_id: crate::platform::WinID = 7;
        let event =
            crate::platform::notify::window_title_event_from_bytes(&window_id.to_ne_bytes())
                .expect("CGS title payload should produce a persistence event");
        app.world_mut()
            .resource_mut::<Messages<Event>>()
            .write(event);

        app.update();

        assert!(app.world().resource::<PersistenceState>().is_dirty());
    }
}
