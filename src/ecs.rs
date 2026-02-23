use std::sync::mpsc::Receiver;
use std::time::Duration;

use bevy::MinimalPlugins;
use bevy::app::App as BevyApp;
use bevy::app::{PostUpdate, PreUpdate, Startup};
use bevy::ecs::message::Messages;
use bevy::ecs::resource::Resource;
use bevy::ecs::schedule::common_conditions::resource_exists;
use bevy::ecs::system::Commands;
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

use crate::commands::register_commands;
use crate::config::CONFIGURATION_FILE;
use crate::errors::Result;
use crate::events::{Event, EventSender};
use crate::manager::{
    Origin, ProcessApi, Size, Window, WindowManager, WindowManagerApi, WindowManagerOS,
};
use crate::platform::{PlatformCallbacks, WinID};

pub mod params;
mod systems;
mod triggers;

/// Registers the Bevy systems for the `WindowManager`.
/// This function adds various systems to the `Update` schedule, including event dispatchers,
/// process/application/window lifecycle management, animation, and periodic watchers.
/// Systems that poll for notifications are conditionally run based on the `PollForNotifications` resource.
///
/// # Arguments
///
/// * `app` - The Bevy application to register the systems with.
pub fn register_systems(app: &mut bevy::app::App) {
    const DISPLAY_CHANGE_CHECK_FREQ_MS: u64 = 1000;
    app.add_systems(
        Startup,
        (systems::gather_displays, systems::gather_initial_processes).chain(),
    );
    app.add_systems(
        PreUpdate,
        (systems::dispatch_toplevel_triggers, systems::pump_events),
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
            systems::window_swiper,
            systems::add_launched_process,
            systems::add_launched_application,
            systems::fresh_marker_cleanup,
            systems::timeout_ticker,
            systems::window_update_frame,
            systems::displays_rearranged,
            systems::reposition_dragged_window,
            systems::find_orphaned_workspaces.run_if(on_timer(Duration::from_millis(
                DISPLAY_CHANGE_CHECK_FREQ_MS,
            ))),
        ),
    );
    app.add_systems(
        Update,
        (
            systems::display_changes_watcher,
            systems::workspace_change_watcher,
        )
            .run_if(resource_exists::<PollForNotifications>)
            .run_if(on_timer(Duration::from_millis(
                DISPLAY_CHANGE_CHECK_FREQ_MS,
            ))),
    );
    app.add_systems(
        PostUpdate,
        (
            systems::reshuffle_layout_strip,
            systems::animate_windows.after(systems::reshuffle_layout_strip),
            systems::animate_resize_windows.after(systems::reshuffle_layout_strip),
        ),
    );
}

/// Registers all the event triggers for the window manager.
pub fn register_triggers(app: &mut bevy::app::App) {
    app.add_observer(triggers::mouse_moved_trigger)
        .add_observer(triggers::mouse_down_trigger)
        .add_observer(triggers::mouse_dragged_trigger)
        .add_observer(triggers::workspace_change_trigger)
        .add_observer(triggers::active_workspace_trigger)
        .add_observer(triggers::display_change_trigger)
        .add_observer(triggers::front_switched_trigger)
        .add_observer(triggers::center_mouse_trigger)
        .add_observer(triggers::window_focused_trigger)
        .add_observer(triggers::swipe_gesture_trigger)
        .add_observer(triggers::mission_control_trigger)
        .add_observer(triggers::application_event_trigger)
        .add_observer(triggers::dispatch_application_messages)
        .add_observer(triggers::window_destroyed_trigger)
        .add_observer(triggers::window_unmanaged_trigger)
        .add_observer(triggers::window_managed_trigger)
        .add_observer(triggers::spawn_window_trigger)
        .add_observer(triggers::refresh_configuration_trigger)
        .add_observer(triggers::stray_focus_observer)
        .add_observer(triggers::locate_dock_trigger)
        .add_observer(triggers::send_message_trigger)
        .add_observer(triggers::window_removal_trigger);
}

/// Marker component for the currently focused window.
#[derive(Component)]
pub struct FocusedMarker;

#[derive(Component)]
pub struct ActiveWorkspaceMarker;

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
#[derive(Component)]
pub struct RepositionMarker {
    /// The new origin (x, y coordinates) for the window.
    pub origin: Origin,
    /// The ID of the display the window should be moved to.
    pub display_id: CGDirectDisplayID,
}

/// Component representing a request to resize a window.
#[derive(Component)]
pub struct ResizeMarker {
    /// The new size (width, height) for the window.
    pub size: Size,
    pub display_id: CGDirectDisplayID,
}

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

#[derive(Component)]
pub struct WindowSwipeMarker(pub f64);

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

#[derive(Component)]
pub struct BruteforceWindows(Task<Vec<Window>>);

#[derive(Component, Debug)]
pub enum DockPosition {
    Bottom(i32),
    Left(i32),
    Right(i32),
    Hidden,
}

/// Resource to control whether window reshuffling should be skipped.
#[derive(Resource)]
pub struct SkipReshuffle(pub bool);

/// Resource indicating whether Mission Control is currently active.
#[derive(Resource)]
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

pub fn reposition_entity(
    entity: Entity,
    origin: Origin,
    display_id: CGDirectDisplayID,
    commands: &mut Commands,
) {
    if let Ok(mut entity_cmmands) = commands.get_entity(entity) {
        entity_cmmands.try_insert(RepositionMarker { origin, display_id });
    }
}

pub fn resize_entity(
    entity: Entity,
    size: Size,
    display_id: CGDirectDisplayID,
    commands: &mut Commands,
) {
    if size.x <= 0 || size.y <= 0 {
        return;
    }
    if let Ok(mut entity_cmmands) = commands.get_entity(entity) {
        entity_cmmands.try_insert(ResizeMarker { size, display_id });
    }
}

pub fn reshuffle_around(entity: Entity, commands: &mut Commands) {
    if let Ok(mut entity_commands) = commands.get_entity(entity) {
        entity_commands.try_insert(ReshuffleAroundMarker);
    }
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
        .insert_resource(MissionControlActive(false))
        .insert_resource(FocusFollowsMouse(None))
        .insert_resource(PollForNotifications)
        .insert_resource(Initializing)
        .insert_non_send_resource(watcher)
        .add_plugins((register_triggers, register_systems, register_commands));

    let mut platform_callbacks = PlatformCallbacks::new(sender);
    platform_callbacks.setup_handlers()?;
    app.insert_non_send_resource(platform_callbacks);
    app.insert_non_send_resource(receiver);

    Ok(app)
}
