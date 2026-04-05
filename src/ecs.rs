use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use bevy::MinimalPlugins;
use bevy::app::App as BevyApp;
use bevy::app::{PostUpdate, PreUpdate, Startup};
use bevy::ecs::message::Messages;
use bevy::ecs::resource::Resource;
use bevy::ecs::schedule::common_conditions::{not, resource_exists};
use bevy::ecs::system::{Commands, Res};
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
use objc2_core_graphics::CGDirectDisplayID;
use tracing::{Level, instrument};

use crate::commands::register_commands;
use crate::config::{CONFIGURATION_FILE, Config, WindowParams};
use crate::ecs::state::PaneruState;
use crate::errors::Result;
use crate::events::{Event, EventSender};
use crate::manager::{
    Application, Origin, ProcessApi, Size, Window, WindowManager, WindowManagerApi, WindowManagerOS,
};
use crate::overlay::{FlashMessageManager, OverlayManager};
use crate::platform::{Modifiers, PlatformCallbacks, WinID, WorkspaceId};

mod focus;
pub mod layout;
mod mouse;
pub mod params;
mod scroll;
pub mod state;
mod systems;
mod triggers;
mod workspace;

/// Registers the Bevy systems for the `WindowManager`.
/// This function adds various systems to the `Update` schedule, including event dispatchers,
/// process/application/window lifecycle management, animation, and periodic watchers.
/// Systems that poll for notifications are conditionally run based on the `PollForNotifications` resource.
///
/// # Arguments
///
/// * `app` - The Bevy application to register the systems with.
#[allow(clippy::too_many_lines)]
pub fn register_systems(app: &mut bevy::app::App) {
    const DISPLAY_CHANGE_CHECK_FREQ_MS: u64 = 1000;
    const REFRESH_WINDOW_CHECK_FREQ_MS: u64 = 1000;
    app.add_systems(
        Startup,
        (systems::gather_displays, systems::gather_initial_processes).chain(),
    );
    app.add_systems(
        PreUpdate,
        (
            systems::dispatch_toplevel_triggers,
            systems::pump_events,
            workspace::switch_virtual_workspace_bind,
            workspace::move_virtual_workspace_bind,
        ),
    );
    app.add_systems(
        Update,
        (
            (
                systems::add_existing_process,
                systems::add_existing_application,
                systems::finish_setup,
            )
                .chain()
                .run_if(resource_exists::<Initializing>),
            systems::add_launched_process,
            systems::add_launched_application,
            systems::fresh_marker_cleanup,
            systems::timeout_ticker,
            systems::retry_front_switch,
            systems::window_update_frame,
            systems::displays_rearranged,
            systems::reposition_dragged_window,
            focus::autocenter_window_on_focus,
            workspace::detect_moved_windows.run_if(not(resource_exists::<Initializing>)),
            workspace::hide_inactive_workspace,
            workspace::show_active_workspace,
            workspace::cleanup_virtual_workspaces,
            workspace::handle_virtual_window_moves,
            workspace::detect_moved_windows.run_if(not(resource_exists::<Initializing>)),
            workspace::refresh_workspace_window_sizes.run_if(on_timer(Duration::from_millis(
                REFRESH_WINDOW_CHECK_FREQ_MS,
            ))),
            workspace::find_orphaned_workspaces
                .after(systems::displays_rearranged)
                .run_if(on_timer(Duration::from_millis(
                    DISPLAY_CHANGE_CHECK_FREQ_MS,
                ))),
            systems::cleanup_on_exit,
        ),
    );
    app.add_systems(
        Update,
        (
            state::periodic_state_save.run_if(on_timer(Duration::from_secs(300))),
            state::cleanup_on_exit,
        ),
    );
    app.add_systems(
        Update,
        (
            scroll::vertical_swipe_gesture,
            (
                scroll::swipe_gesture,
                scroll::apply_inertia,
                scroll::apply_snap_force,
                scroll::scrolling_integrator,
                scroll::apply_scrolling_constraints,
                scroll::swiping_timeout,
            )
                .chain(),
            layout::layout_sizes_changed,
            (
                layout::layout_strip_changed,
                layout::reshuffle_layout_strip,
                layout::position_layout_strips,
                layout::position_layout_windows,
            )
                .chain(),
        ),
    );
    app.add_systems(
        Update,
        (
            systems::display_changes_watcher,
            workspace::workspace_change_watcher,
        )
            .run_if(resource_exists::<PollForNotifications>)
            .run_if(on_timer(Duration::from_millis(
                DISPLAY_CHANGE_CHECK_FREQ_MS,
            ))),
    );
    app.add_systems(
        PostUpdate,
        (
            focus::mouse_follows_focus,
            systems::animate_entities,
            systems::animate_resize_entities,
            systems::update_overlays
                .after(systems::animate_entities)
                .after(systems::animate_resize_entities)
                .run_if(|config: Option<Res<Config>>| {
                    config.is_some_and(|config| {
                        config.has_dim_inactive_color() || config.border_active_window()
                    })
                }),
            systems::update_flash_messages.after(systems::update_overlays),
            systems::commit_window_position.after(systems::animate_entities),
            systems::commit_window_size.after(systems::animate_resize_entities),
        ),
    );
}

/// Registers all the event triggers for the window manager.
pub fn register_triggers(app: &mut bevy::app::App) {
    app.add_observer(mouse::mouse_moved_trigger)
        .add_observer(mouse::mouse_down_trigger)
        .add_observer(mouse::mouse_up_trigger)
        .add_observer(mouse::mouse_dragged_trigger)
        .add_observer(triggers::display_change_trigger)
        .add_observer(triggers::front_switched_trigger)
        .add_observer(triggers::window_focused_trigger)
        .add_observer(triggers::mission_control_trigger)
        .add_observer(triggers::application_event_trigger)
        .add_observer(triggers::dispatch_application_messages)
        .add_observer(triggers::window_destroyed_trigger)
        .add_observer(triggers::window_unmanaged_trigger)
        .add_observer(triggers::window_managed_trigger)
        .add_observer(triggers::window_minimized_trigger)
        .add_observer(triggers::spawn_window_trigger)
        .add_observer(triggers::refresh_configuration_trigger)
        .add_observer(triggers::stray_focus_observer)
        .add_observer(triggers::locate_dock_trigger)
        .add_observer(triggers::send_message_trigger)
        .add_observer(triggers::window_removal_trigger)
        .add_observer(triggers::theme_change_trigger)
        .add_observer(triggers::apply_window_properties)
        .add_observer(triggers::restore_window_state)
        .add_observer(focus::dim_remove_window_trigger)
        .add_observer(focus::dim_window_trigger)
        .add_observer(focus::maintain_focus_singleton)
        .add_observer(focus::virtual_strip_activated)
        .add_observer(focus::focus_window_trigger)
        .add_observer(workspace::cleanup_active_workspace_marker)
        .add_observer(workspace::cleanup_selected_space_marker)
        .add_observer(workspace::workspace_change_trigger)
        .add_observer(workspace::workspace_created_trigger)
        .add_observer(workspace::workspace_destroyed_trigger);
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

/// Marker component indicating that a window is currently being dragged by the mouse.
#[derive(Component)]
pub struct WindowDraggedMarker {
    /// The entity ID of the dragged window.
    pub entity: Entity,
    /// The ID of the display the window is being dragged on.
    pub display_id: CGDirectDisplayID,
}

/// Marker component indicating that windows around the marked entity need to be reshuffled.
#[derive(Component)]
pub struct ReshuffleAroundMarker;

/// Marker component placed on a window that was resized internally to compensate
/// for an adjacent stacked window's top-edge drag. When the OS echoes back a
/// `WindowResized` event for this window, the reshuffle is skipped and the marker
/// is removed to prevent a feedback loop.
#[derive(Component)]
pub struct StackAdjustedResize;

#[derive(Component, Debug)]
pub struct Scrolling {
    pub velocity: f64,
    pub position: f64,
    /// When true, the user's fingers are on the trackpad.
    pub is_user_swiping: bool,
    /// Last time a physical swipe event was received.
    pub last_event: Instant,
}

impl Default for Scrolling {
    fn default() -> Self {
        Self {
            velocity: 0.0,
            position: 0.0,
            is_user_swiping: false,
            last_event: Instant::now(),
        }
    }
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
    pub previous_strip: WorkspaceId,
    pub previous_index: usize,
}

/// Stores the width ratio of a window before it was made full-width.
/// When a stacked window goes full-width, it is unstacked first;
/// `was_stacked` records whether to restack on exit.
#[derive(Component)]
pub struct FullWidthMarker {
    pub width_ratio: f64,
    pub was_stacked: bool,
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

/// Wrapper component for a `ProcessApi` trait object, enabling dynamic dispatch for process-related operations within Bevy.
#[derive(Component, Deref, DerefMut)]
pub struct BProcess(pub Box<dyn ProcessApi>);

/// Component to manage a timeout, often used for delaying actions or retries.
#[derive(Component)]
pub struct Timeout {
    /// The Bevy timer instance.
    pub timer: Timer,
    /// An optional message associated with the timeout.
    pub message: Option<String>,
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
    pub fn new(duration: Duration, message: Option<String>) -> Self {
        let timer = Timer::from_seconds(duration.as_secs_f32(), bevy::time::TimerMode::Once);
        Self { timer, message }
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
#[derive(PartialEq, Resource)]
pub struct MissionControlActive(pub bool);

/// Resource holding the `WinID` of a window that should gain focus when focus-follows-mouse is enabled.
#[derive(Resource)]
pub struct FocusFollowsMouse(pub Option<WinID>);

/// Resource to control whether the application should poll for notifications.
#[derive(PartialEq, Resource)]
pub struct PollForNotifications;

#[derive(PartialEq, Resource)]
pub struct Initializing;

/// Bevy event trigger for general window manager events.
#[derive(BevyEvent)]
pub struct WMEventTrigger(pub Event);

/// Bevy event trigger for spawning new windows.
#[derive(BevyEvent)]
pub struct SpawnWindowTrigger(pub Vec<Window>);

#[derive(BevyEvent)]
pub struct LocateDockTrigger(pub Entity);

#[derive(BevyEvent)]
pub struct SendMessageTrigger(pub Event);

#[derive(BevyEvent)]
pub struct RestoreWindowState;

#[instrument(level = Level::TRACE, skip(commands))]
pub fn reposition_entity(entity: Entity, origin: Origin, commands: &mut Commands) {
    if let Ok(mut entity_commands) = commands.get_entity(entity) {
        entity_commands.try_insert(RepositionMarker(origin));
    }
}

#[instrument(level = Level::TRACE, skip(commands))]
pub fn resize_entity(entity: Entity, size: Size, commands: &mut Commands) {
    if size.x <= 0 || size.y <= 0 {
        return;
    }
    if let Ok(mut entity_commands) = commands.get_entity(entity) {
        entity_commands.try_insert(ResizeMarker(size));
    }
}

#[instrument(level = Level::TRACE, skip(commands))]
pub fn reshuffle_around(entity: Entity, commands: &mut Commands) {
    if let Ok(mut entity_commands) = commands.get_entity(entity) {
        entity_commands.try_insert(ReshuffleAroundMarker);
    }
}

pub fn focus_entity(entity: Entity, raise: bool, commands: &mut Commands) {
    if let Ok(mut entity_commands) = commands.get_entity(entity) {
        entity_commands.try_insert(FocusedMarker);
        commands.trigger(focus::FocusWindow { entity, raise });
    }
}

pub fn flash_message(message: String, duration: f32, commands: &mut Commands) {
    let timeout = Timeout::new(Duration::from_secs_f32(duration), None);
    commands.spawn((timeout, FlashMessage(message)));
}

pub fn setup_bevy_app(sender: EventSender, receiver: Receiver<Event>) -> Result<BevyApp> {
    let window_manager: Box<dyn WindowManagerApi> = Box::new(WindowManagerOS::new(sender.clone()));
    let watcher = window_manager.setup_config_watcher(CONFIGURATION_FILE.as_path())?;

    let mut app = BevyApp::new();

    app.add_plugins(MinimalPlugins)
        .init_resource::<Messages<Event>>()
        .insert_resource(Time::<Virtual>::from_max_delta(Duration::from_secs(10)))
        .insert_resource(WindowManager(window_manager))
        .insert_resource(SkipReshuffle(false))
        .insert_resource(SystemTheme {
            is_dark: crate::util::is_dark_mode(),
        })
        .insert_resource(MissionControlActive(false))
        .insert_resource(FocusFollowsMouse(None))
        .insert_resource(PollForNotifications)
        .insert_resource(Initializing)
        .insert_non_send_resource(watcher)
        .add_plugins((register_triggers, register_systems, register_commands));

    let mut platform_callbacks = PlatformCallbacks::new(sender);
    platform_callbacks.setup_handlers()?;
    let mtm = platform_callbacks.main_thread_marker;
    let overlay_manager = OverlayManager::new(mtm);
    let flash_message_manager = FlashMessageManager::new(mtm);
    app.insert_non_send_resource(platform_callbacks);
    app.insert_non_send_resource(overlay_manager);
    app.insert_non_send_resource(flash_message_manager);
    app.insert_non_send_resource(receiver);

    if let Some(previous_state) = PaneruState::load_from_file(state::STATE_FILE_PATH) {
        app.insert_resource(previous_state);
    }

    Ok(app)
}

struct WindowProperties {
    pub params: Vec<WindowParams>,
}

impl WindowProperties {
    pub fn new(app: &Application, window: &Window, config: &Config) -> Self {
        let bundle_id = app.bundle_id().unwrap_or_default();
        let title = window.title().unwrap_or_default();
        let params = config.find_window_properties(&title, bundle_id);
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
}
