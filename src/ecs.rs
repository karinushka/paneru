use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use bevy::MinimalPlugins;
use bevy::app::App as BevyApp;
use bevy::app::{First, Last, PostUpdate, PreUpdate, Startup};
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::lifecycle::RemovedComponents;
use bevy::ecs::query::{Added, Changed, With};
use bevy::ecs::resource::Resource;
use bevy::ecs::schedule::SystemCondition;
use bevy::ecs::schedule::common_conditions::{not, resource_exists};
use bevy::ecs::schedule::{ScheduleLabel as _, SingleThreadedExecutor};
use bevy::ecs::system::{Commands, EntityCommands, Query, Res, SystemId};
use bevy::prelude::Event as BevyEvent;
use bevy::tasks::Task;
use bevy::time::Timer;
use bevy::time::common_conditions::on_timer;
use bevy::time::{Time, Virtual};
use bevy::{
    app::Update,
    ecs::{component::Component, entity::Entity, schedule::IntoScheduleConfigs},
};
use derive_more::{Deref, DerefMut};
use tracing::{Level, error, instrument, warn};

use crate::commands::register_commands;
use crate::config::{CONFIGURATION_FILE, Config, WindowParams};
use crate::ecs::layout::LayoutStrip;
use crate::ecs::state::PaneruState;
use crate::errors::Result;
use crate::events::{Event, EventSender, InputEvent};
#[cfg(feature = "lua")]
use crate::lua;
use crate::manager::{
    Application, Origin, ProcessApi, Size, Window, WindowManager, WindowManagerApi, WindowManagerOS,
};
use crate::menubar::MenuBarManager;
use crate::overlay::{FlashMessageManager, OverlayManager};
use crate::platform::{Modifiers, PlatformCallbacks, WinID, WorkspaceId};

pub mod display;
pub mod focus;
pub mod layout;
#[cfg(feature = "lua")]
pub mod layout_ops;
pub mod mouse;
pub mod params;
pub(crate) mod restore;
pub mod script_state;
pub mod scroll;
pub mod state;
pub(crate) mod systems;
mod triggers;
pub mod workspace;

// Shared by the Lua reload system so a `paneru.setup{...}` reload applies the
// same menubar/passthrough side effects as a TOML reload.
#[cfg(feature = "lua")]
pub(crate) use triggers::apply_config_side_effects;

/// Registers the Bevy systems for the `WindowManager`.
/// This function adds various systems to the `Update` schedule, including event dispatchers,
/// process/application/window lifecycle management, animation, and periodic watchers.
///
/// # Arguments
///
/// * `app` - The Bevy application to register the systems with.
#[allow(clippy::too_many_lines)]
pub fn register_systems(app: &mut bevy::app::App) {
    const CLOSED_WINDOW_CHECK_FREQ_MS: u64 = 1000;
    const LOW_POWER_MODE_CHECK_SEC: u64 = 60;
    const APP_OBSERVABILITY_CHECK_FREQ: Duration = Duration::from_millis(200);

    let not_swiping = |scrolling: Query<&Scrolling, With<ActiveWorkspaceMarker>>| {
        scrolling
            .iter()
            .next()
            .is_none_or(|marker| !marker.is_user_swiping)
    };
    let dimming_enabled = |config: Option<Res<Config>>| {
        config
            .is_some_and(|config| config.has_dim_inactive_color() || config.border_active_window())
    };
    // The overlay must refresh not just when the active strip's layout changes,
    // but also whenever focus moves — including focus *loss* (e.g. switching to
    // an empty virtual workspace), which otherwise leaves a stale outline.
    // Position changes on the focused window also dirty the overlay so that
    // dragging a floating window moves the highlight with it.
    let vw_indicator_dirty =
        |strip_changed: Query<(), (With<ActiveWorkspaceMarker>, Changed<LayoutStrip>)>,
         focus_gained: Query<(), Added<FocusedMarker>>,
         workspace_changed: Query<(), Added<ActiveWorkspaceMarker>>,
         focused_moved: Query<(), (With<FocusedMarker>, Changed<Position>)>| {
            !strip_changed.is_empty()
                || !focus_gained.is_empty()
                || !workspace_changed.is_empty()
                || !focused_moved.is_empty()
        };
    // The menu bar additionally shows how many virtual workspaces exist, so it
    // has to redraw when one is created or reaped, neither of which touches the
    // active strip.
    let strip_count_changed =
        |added: Query<(), Added<LayoutStrip>>, mut removed: RemovedComponents<LayoutStrip>| {
            !added.is_empty() || removed.read().next().is_some()
        };
    let native_tabs_enabled =
        |config: Option<Res<Config>>| config.is_none_or(|config| config.native_tabs_enabled());

    app.add_systems(
        Startup,
        (
            systems::gather_displays,
            systems::gather_initial_processes,
            systems::initialise_workspaces,
        )
            .chain(),
    );
    // Registered with `add_message`, not `init_resource`, so the buffer is
    // double-buffered and dropped after a frame like any other message stream.
    app.add_message::<InputEvent>();
    app.add_systems(
        PreUpdate,
        (
            systems::window_creation_event,
            systems::pump_events,
            systems::demux_input_events.after(systems::pump_events),
        ),
    );
    app.add_systems(
        Update,
        (
            (
                triggers::apply_window_defaults,
                systems::detect_tabbed_windows.run_if(native_tabs_enabled),
                triggers::apply_window_positions,
            )
                .chain(),
            (
                systems::add_existing_process,
                systems::add_existing_application,
                systems::finish_setup,
            )
                .chain()
                .run_if(resource_exists::<Initializing>),
            systems::add_launched_process.run_if(on_timer(APP_OBSERVABILITY_CHECK_FREQ)),
            systems::add_launched_application.run_if(on_timer(APP_OBSERVABILITY_CHECK_FREQ)),
            systems::fresh_marker_cleanup,
            systems::timeout_ticker,
            workspace::cleanup_unordered_windows
                .run_if(not(resource_exists::<Initializing>))
                .run_if(on_timer(Duration::from_millis(CLOSED_WINDOW_CHECK_FREQ_MS))),
            systems::auto_discover_unmanaged_focused_windows,
            systems::retry_front_switch,
            systems::update_low_power_state
                .run_if(resource_exists::<LowPowerMode>)
                .run_if(on_timer(Duration::from_secs(LOW_POWER_MODE_CHECK_SEC))),
            (
                systems::window_resized_update_frame,
                systems::window_moved_update_frame,
            )
                .chain()
                .run_if(not_swiping),
            systems::cleanup_on_exit,
            restore::tick_restore_grace,
            state::periodic_state_save.run_if(on_timer(Duration::from_mins(5))),
            state::cleanup_on_exit,
            script_state::periodic_script_state_save.run_if(on_timer(Duration::from_mins(5))),
            script_state::script_state_cleanup_on_exit,
        ),
    );
    app.add_systems(
        PostUpdate,
        (
            (
                systems::animate_entities,
                systems::commit_window_position.run_if(not(resource_exists::<Initializing>)),
                systems::verify_window_position.run_if(not(resource_exists::<Initializing>)),
            )
                .chain(),
            (
                systems::animate_resize_entities,
                systems::commit_window_size.run_if(not(resource_exists::<Initializing>)),
            )
                .chain(),
            (
                systems::update_overlays
                    .after(systems::animate_entities)
                    .after(systems::animate_resize_entities)
                    .run_if(dimming_enabled)
                    .run_if(vw_indicator_dirty),
                systems::update_flash_messages,
            )
                .chain(),
            crate::menubar::update_menu_bar
                .run_if(vw_indicator_dirty.or_eager(strip_count_changed)),
        ),
    );
}

/// Registers all the event triggers for the window manager.
pub fn register_triggers(app: &mut bevy::app::App) {
    app.add_systems(
        Update,
        (
            triggers::front_switched_trigger,
            triggers::window_focused_trigger,
            triggers::mission_control_trigger,
            triggers::application_event_trigger,
            triggers::dispatch_application_messages,
            triggers::window_destroyed_trigger,
            triggers::invalidate_window_title,
            triggers::refresh_configuration_trigger,
            triggers::theme_change_trigger,
            triggers::window_resize_verifier,
        ),
    );
    app.add_observer(triggers::window_unmanaged_trigger)
        .add_observer(triggers::window_managed_trigger)
        .add_observer(triggers::window_minimized_trigger)
        .add_observer(triggers::spawn_window_trigger)
        .add_observer(triggers::send_message_trigger)
        .add_observer(triggers::window_removal_trigger)
        .add_observer(triggers::cleanup_timeout_trigger)
        .add_observer(restore::restore_window_state);
}

/// Marker component for the currently focused window.
#[derive(Component)]
pub struct FocusedMarker;

#[derive(Component)]
pub struct ActiveWorkspaceMarker;

#[derive(Component)]
pub struct SelectedVirtualMarker;

#[derive(Component)]
pub struct FlashMessage(pub String);

/// Marker component for the currently active display.
#[derive(Component)]
pub struct ActiveDisplayMarker;

/// Marker component signifying a freshly created process, application, or window.
#[derive(Component)]
pub struct FreshMarker;

/// Marker component used to gather existing processes and windows during initialization.
#[derive(Component)]
pub struct ExistingMarker;

/// Component representing a request to reposition a window.
#[derive(Component, Debug, Deref, DerefMut)]
pub struct RepositionMarker(pub Origin);

/// Component representing a request to resize a window.
#[derive(Component, Debug, Deref, DerefMut)]
pub struct ResizeMarker(pub Size);

/// Marker component indicating that windows around the marked entity need to be reshuffled.
#[derive(Component)]
pub struct ReshuffleAroundMarker;

/// Marker component requesting that the strip scroll *minimally* to keep the
/// entity's NEW layout position inside the viewport. Unlike
/// [`ReshuffleAroundMarker`], this does not anchor the entity to its old visual
/// position — if the new layout slot is already on-screen, the strip is left
/// alone and the entity is free to slide there. Only when the new slot would
/// fall off the edge does the strip scroll just enough to expose it.
#[derive(Component)]
pub struct EnsureVisibleMarker;

#[derive(Component, Debug)]
pub struct Scrolling {
    pub velocity: f64,
    pub position: f64,
    /// When true, the user's fingers are on the trackpad.
    pub is_user_swiping: bool,
    /// Last time a physical swipe event was received.
    pub last_event: Duration,
}

#[derive(Component, Clone, Debug, Default, Deref, DerefMut)]
pub struct LayoutPosition(pub Origin);

#[derive(Component, Clone, Debug, Deref, DerefMut)]
pub struct Position(pub Origin);

#[derive(Component, Clone, Debug, Deref, DerefMut)]
pub struct Bounds(pub Size);

#[derive(Component, Clone, Debug, Deref, DerefMut)]
pub struct WidthRatio(pub f64);

/// Marks a window entity that is currently on a native macOS fullscreen space.
/// The window has been removed from its tiled position in the strip.
/// `order` gives the sequence in which windows went fullscreen (0, 1, 2, …)
/// so they can be navigated left-to-right in that order after the tiled strip.
#[derive(Clone, Component, Debug)]
pub struct NativeFullscreenMarker {
    pub layout_strip: Entity,
    pub workspace_id: WorkspaceId,
    pub index: usize,
}

#[derive(Component)]
pub struct FullWidthMarker {
    pub width_ratio: f64,
}

/// Enum component indicating the unmanaged state of a window.
#[derive(Component, Debug)]
pub enum Unmanaged {
    /// The window is floating and not part of the tiling layout.
    Floating,
    /// The window is minimized.
    Minimized,
    /// The window is hidden.
    Hidden,
}

#[derive(Clone, Component, Copy, Debug)]
pub struct PreviousManagedStrip {
    pub workspace_id: WorkspaceId,
    pub virtual_index: u32,
    pub index: usize,
}

/// Wrapper component for a `ProcessApi` trait object, enabling dynamic dispatch for process-related operations within Bevy.
#[derive(Component, Deref, DerefMut)]
pub struct BProcess(pub Box<dyn ProcessApi>);

/// Component to manage a timeout, often used for delaying actions or retries.
#[derive(Component)]
pub struct Timeout {
    /// The Bevy timer instance.
    pub timer: Timer,
    /// An optional system to execute on timeout.
    pub system_id: Option<SystemId>,
}

impl Timeout {
    /// Creates a new `Timeout` with a specified duration and an optional message.
    /// The timer is set to run once.
    ///
    /// # Arguments
    ///
    /// * `duration` - The `Duration` for the timeout.
    /// * `message` - An `Option<String>` containing a message to associate with the timeout.
    ///
    /// # Returns
    ///
    /// A new `Timeout` instance.
    pub fn new(duration: Duration, message: Option<String>, commands: &mut Commands) -> Self {
        let timer = Timer::from_seconds(duration.as_secs_f32(), bevy::time::TimerMode::Once);
        if let Some(message) = message {
            let callback = move || {
                tracing::debug!("{message}");
            };
            let system_id = Some(commands.register_system(callback));

            Self { timer, system_id }
        } else {
            Self {
                timer,
                system_id: None,
            }
        }
    }

    /// Creates an action timeout, which oneshots a provided system id.
    pub fn callback(duration: Duration, system_id: SystemId, commands: &mut Commands) {
        let timer = Timer::from_seconds(duration.as_secs_f32(), bevy::time::TimerMode::Once);
        commands.spawn(Self {
            timer,
            system_id: Some(system_id),
        });
    }
}

/// Component used as a retry mechanism for stray focus events that arrive before the target window is fully created.
#[derive(Component)]
pub struct StrayFocusEvent(pub WinID);

/// Component used as a retry mechanism when `focused_window_id()` fails during
/// an `ApplicationFrontSwitched` event (e.g. transient `kAXErrorCannotComplete`).
#[derive(Component)]
pub struct RetryFrontSwitch(pub Entity);

#[derive(Component)]
pub struct BruteforceWindows(Task<Vec<Window>>);

#[derive(Component, Debug)]
pub enum DockPosition {
    Bottom(i32),
    Left(i32),
    Right(i32),
    Hidden,
}

#[derive(Component)]
pub struct RefreshWindowSizes(pub Instant);

impl Default for RefreshWindowSizes {
    fn default() -> Self {
        Self(Instant::now())
    }
}

impl RefreshWindowSizes {
    pub fn ready(&self) -> bool {
        const REFRESH_WINDOW_SIZE_DELAY_SEC: u64 = 5;
        self.0.elapsed() > Duration::from_secs(REFRESH_WINDOW_SIZE_DELAY_SEC)
    }
}

#[derive(Component)]
pub struct VerifyWindowPosition {
    remaining: u8,
}

impl Default for VerifyWindowPosition {
    fn default() -> Self {
        Self { remaining: 3 }
    }
}

impl VerifyWindowPosition {
    pub fn tick(&mut self) -> bool {
        self.remaining = self.remaining.saturating_sub(1);
        self.remaining == 0
    }
}

#[derive(Deref, DerefMut, Resource)]
pub struct LowPowerMode(pub bool);

#[derive(Resource)]
pub struct SystemTheme {
    pub is_dark: bool,
}

/// Resource to control whether window reshuffling should be skipped.
#[derive(Resource)]
pub struct SkipReshuffle(pub bool);

/// Component marking a deferred reshuffle while the mouse button is held down.
/// Spawned with a `Timeout` so it auto-despawns if the mouse-up event is lost.
#[derive(Component)]
pub struct MouseHeldMarker(pub Entity);

/// Resource indicating whether Mission Control is currently active.
#[derive(Resource)]
pub struct MissionControlActive(pub bool);

/// Resource holding the `WinID` of a window that should gain focus when focus-follows-mouse is enabled.
#[derive(Resource)]
pub struct FocusFollowsMouse(pub Option<WinID>);

#[derive(Resource)]
pub struct Initializing;

/// Bevy event trigger for spawning new windows.
#[derive(BevyEvent)]
pub struct SpawnWindowTrigger(pub Vec<Window>);

#[derive(BevyEvent)]
pub struct ReadDisplayProperties(pub Entity);

#[derive(BevyEvent)]
pub struct SendMessageTrigger(pub Event);

#[derive(BevyEvent)]
pub struct RestoreWindowState;

#[derive(BevyEvent)]
pub struct RaiseWindow {
    pub entity: Entity,
    pub with_strip: bool,
}

pub trait SpawnCommandsExt {
    fn reposition_entity(&mut self, entity: Entity, origin: Origin);

    fn resize_entity(&mut self, entity: Entity, size: Size);

    fn reshuffle_around(&mut self, entity: Entity);

    fn ensure_visible(&mut self, entity: Entity);

    fn focus_entity(&mut self, entity: Entity, raise: bool);

    fn flash_message(&mut self, message: String, duration: f32);

    // Spawns a layout strip in a single place, to properly insert all components.
    fn spawn_layout_strip(
        &mut self,
        layout_strip: LayoutStrip,
        origin: Origin,
        display_entity: Entity,
        active: bool,
    ) -> EntityCommands<'_>;
}

impl SpawnCommandsExt for Commands<'_, '_> {
    #[instrument(level = Level::TRACE, skip(self))]
    fn reposition_entity(&mut self, entity: Entity, origin: Origin) {
        if let Ok(mut entity_commands) = self.get_entity(entity) {
            entity_commands.try_insert(RepositionMarker(origin));
        }
    }

    #[instrument(level = Level::TRACE, skip(self))]
    fn resize_entity(&mut self, entity: Entity, size: Size) {
        if size.x <= 0 || size.y <= 0 {
            return;
        }
        if let Ok(mut entity_commands) = self.get_entity(entity) {
            entity_commands.try_insert(ResizeMarker(size));
        }
    }

    #[instrument(level = Level::TRACE, skip(self))]
    fn reshuffle_around(&mut self, entity: Entity) {
        if let Ok(mut entity_commands) = self.get_entity(entity) {
            entity_commands.try_insert(ReshuffleAroundMarker);
        }
    }

    #[instrument(level = Level::TRACE, skip(self))]
    fn ensure_visible(&mut self, entity: Entity) {
        if let Ok(mut entity_commands) = self.get_entity(entity) {
            entity_commands.try_insert(EnsureVisibleMarker);
        }
    }

    #[instrument(level = Level::TRACE, skip(self))]
    fn focus_entity(&mut self, entity: Entity, raise: bool) {
        if let Ok(mut entity_commands) = self.get_entity(entity) {
            entity_commands.try_insert(FocusedMarker);
            self.trigger(focus::FocusWindow { entity, raise });
        }
    }

    #[instrument(level = Level::TRACE, skip(self))]
    fn flash_message(&mut self, message: String, duration: f32) {
        let timeout = Timeout::new(Duration::from_secs_f32(duration), None, self);
        self.spawn((timeout, FlashMessage(message)));
    }

    #[instrument(level = Level::TRACE, skip(self))]
    fn spawn_layout_strip(
        &mut self,
        layout_strip: LayoutStrip,
        origin: Origin,
        display_entity: Entity,
        active: bool,
    ) -> EntityCommands<'_> {
        let mut spawned = self.spawn((layout_strip, Position(origin), ChildOf(display_entity)));
        if active {
            spawned.insert(ActiveWorkspaceMarker);
        } else {
            spawned.insert(SelectedVirtualMarker);
        }
        spawned
    }
}

/// Rebuilds the config watcher around `changed`, then re-registers every other
/// config file. Editors that save atomically (write-new-then-rename) break the
/// original watch, and since the TOML and Lua script share one watcher,
/// rebuilding it for just the changed file would otherwise silently stop the
/// other one from hot-reloading.
pub(crate) fn rewatch_configs(
    window_manager: &WindowManager,
    changed: &std::path::Path,
) -> Option<Box<dyn notify::Watcher>> {
    let mut watcher = window_manager
        .setup_config_watcher(changed)
        .inspect_err(|err| error!("watching the config '{}': {err}", changed.display()))
        .ok()?;

    let others = [
        CONFIGURATION_FILE.clone(),
        #[cfg(feature = "lua")]
        crate::config::discover_lua_file(),
    ];
    for other in others.into_iter().flatten() {
        if other == changed {
            continue;
        }
        if let Err(err) = watcher.watch(&other, notify::RecursiveMode::NonRecursive) {
            warn!("re-watching config '{}': {err}", other.display());
        }
    }
    Some(watcher)
}

pub fn setup_bevy_app(sender: EventSender, receiver: Receiver<Event>) -> Result<BevyApp> {
    let window_manager: Box<dyn WindowManagerApi> = Box::new(WindowManagerOS::new(sender.clone()));

    // Discover (or create) the Lua init script first: whether it exists decides
    // whether the TOML path runs at all, so it has to be settled before
    // `CONFIGURATION_FILE` is first read.
    #[cfg(feature = "lua")]
    let lua_path = crate::config::ensure_lua_file()
        .inspect_err(|err| warn!("preparing Lua script: {err}"))
        .ok()
        .flatten();

    // With an init.lua there is no TOML at all, so watch whichever config files
    // actually exist. Both feed the same `ConfigRefresh` event.
    let toml_path = CONFIGURATION_FILE.as_deref();
    #[cfg(feature = "lua")]
    let primary = toml_path.or(lua_path.as_deref());
    #[cfg(not(feature = "lua"))]
    let primary = toml_path;
    let primary = primary.ok_or_else(|| {
        crate::errors::Error::InvalidConfig("no configuration file to watch".to_string())
    })?;

    #[cfg_attr(not(feature = "lua"), allow(unused_mut))]
    let mut watcher = window_manager.setup_config_watcher(primary)?;

    #[cfg(feature = "lua")]
    if let Some(path) = &lua_path
        && path.as_path() != primary
        && let Err(err) = watcher.watch(path, notify::RecursiveMode::NonRecursive)
    {
        warn!("watching Lua script '{}': {err}", path.display());
    }

    let mut app = BevyApp::new();

    app.add_plugins(MinimalPlugins)
        // `add_message`, not `init_resource`: the latter never registers the
        // buffer with bevy's `MessageRegistry`, so it's never double-buffered
        // and grows unbounded instead — every event lived for the process's
        // lifetime. Messages now live two frames, which every reader here
        // tolerates: readers gated on `not_swiping` or IPC subscribers would
        // rather drop a missed frame than act on a backlog.
        .add_message::<Event>()
        .insert_resource(Time::<Virtual>::from_max_delta(Duration::from_secs(10)))
        .insert_resource(WindowManager(window_manager))
        .insert_resource(SkipReshuffle(false))
        .insert_resource(SystemTheme {
            is_dark: crate::util::is_dark_mode(),
        })
        .insert_resource(MissionControlActive(false))
        .insert_resource(FocusFollowsMouse(None))
        .insert_resource(Initializing)
        .insert_non_send(watcher)
        .add_plugins(mouse::MouseEventsPlugin)
        .add_plugins(scroll::ScrollEventsPlugin)
        .add_plugins(workspace::WorkspaceEventsPlugin)
        .add_plugins(layout::LayoutEventsPlugin)
        .add_plugins(focus::FocusEventsPlugin)
        .add_plugins(display::DisplayEventsPlugin)
        .add_plugins((register_triggers, register_systems, register_commands));

    // Run every schedule inline rather than fanning systems out across the task
    // pool: the task-pool handoff measured ~45% of main-thread time against
    // ~16% actually spent on accessibility calls, dropping to ~10% once
    // inlined. The expensive systems here all take `&mut Window` and are
    // already mutually exclusive, so the fan-out bought little; genuine
    // parallelism (`par_iter_mut`) still goes through `ComputeTaskPool`
    // directly. `First`/`Last` are included even though unused because an
    // empty schedule still costs a task-pool scope per frame.
    for label in [
        First.intern(),
        PreUpdate.intern(),
        Update.intern(),
        PostUpdate.intern(),
        Last.intern(),
    ] {
        app.edit_schedule(label, |schedule| {
            schedule.set_executor(SingleThreadedExecutor::new());
        });
    }

    let menu_events = sender.clone();
    let mut platform_callbacks = PlatformCallbacks::new(sender);
    platform_callbacks.setup_handlers()?;
    let mtm = platform_callbacks.main_thread_marker;
    let overlay_manager = OverlayManager::new(mtm);
    let flash_message_manager = FlashMessageManager::new(mtm);
    let menu_bar_manager = MenuBarManager::new(mtm, menu_events);
    app.insert_non_send(platform_callbacks)
        .insert_non_send(overlay_manager)
        .insert_non_send(flash_message_manager)
        .insert_non_send(menu_bar_manager)
        .insert_non_send(receiver);

    if let Some(previous_state) =
        PaneruState::load_from_file(&PaneruState::default_state_file_path())
    {
        app.insert_resource(previous_state);
    }

    // Overwrites the empty store `register_commands` put there, which is what
    // the mock harness keeps: only the real app reads the user's file.
    app.insert_resource(script_state::ScriptStateStore::load());

    // Do not insert this in mocks.
    app.insert_resource(LowPowerMode(false));

    // Start the Lua worker and install its hot-reload plugin (kept out of the
    // mock harness). A missing/broken script falls back to an empty runtime so
    // the watcher can still pick up a later fix. `spawn` blocks until the
    // script finishes loading, so its keybinds are published before the event
    // tap can see a keypress.
    #[cfg(feature = "lua")]
    if let Some(path) = lua_path {
        // `paneru.bind` resolves chords on the worker, and the layout-aware
        // keymap behind that goes through Carbon/TIS — must capture it here,
        // on the main thread, before the worker can ask for it.
        crate::config::prime_virtual_keymap();
        // The worker caches the script state store and watches this stamp to
        // know when its copy is stale — including when the writer was a client
        // rather than the script itself.
        let revision = app
            .world()
            .resource::<script_state::ScriptStateStore>()
            .revision_handle();
        let worker = lua::LuaWorker::spawn(lua::LuaSource::Path(path.clone()), revision);
        // A script that called `paneru.setup{...}` is authoritative: insert its
        // config now, before `app.run()`, so it exists ahead of the Startup
        // schedule and wins over the TOML `InitialConfig` (see
        // `gather_initial_processes`). Without `setup`, the TOML config is used.
        if let Some(config) = worker.built_config() {
            app.insert_resource(config);
        }
        app.insert_resource(worker);
        app.insert_resource(lua::LuaScriptPath(path));
        app.add_plugins(lua::LuaPlugin {});
    }

    Ok(app)
}

struct WindowProperties {
    params: Vec<WindowParams>,
}

impl WindowProperties {
    pub fn new(app: &Application, window: &Window, config: &Config) -> Self {
        let bundle_id = app.bundle_id().unwrap_or_default();
        let title = window.title().unwrap_or_default();
        let params = config.find_window_properties(&title, &bundle_id);
        Self { params }
    }

    pub fn floating(&self) -> bool {
        self.params
            .iter()
            .find_map(|props| props.floating)
            .unwrap_or(false)
    }

    pub fn insertion(&self) -> Option<usize> {
        self.params.iter().find_map(|props| props.index)
    }

    pub fn dont_focus(&self) -> bool {
        self.params
            .iter()
            .find_map(|props| props.dont_focus)
            .unwrap_or(false)
    }

    pub fn border_radius(&self) -> Option<f64> {
        self.params.iter().find_map(|p| p.border_radius)
    }

    pub fn grid_ratios(&self) -> Option<(f64, f64, f64, f64)> {
        self.params.iter().find_map(WindowParams::grid_ratios)
    }

    pub fn passthrough_keys(&self) -> Vec<(u8, Modifiers)> {
        self.params
            .iter()
            .flat_map(|p| p.passthrough_keys().to_vec())
            .collect::<Vec<_>>()
    }

    pub fn width_ratio(&self) -> Option<f64> {
        self.params.iter().find_map(|props| props.width)
    }

    pub fn vertical_padding(&self) -> i32 {
        self.params
            .iter()
            .find_map(|props| props.vertical_padding)
            .unwrap_or(0)
    }

    pub fn horizontal_padding(&self) -> i32 {
        self.params
            .iter()
            .find_map(|props| props.horizontal_padding)
            .unwrap_or(0)
    }
}
