use std::ops::{Deref, DerefMut};
use std::time::Duration;

use bevy::ecs::resource::Resource;
use bevy::prelude::Event as BevyEvent;
use bevy::time::Timer;
use bevy::{
    app::Update,
    ecs::{
        component::Component,
        entity::Entity,
        schedule::{IntoScheduleConfigs, common_conditions::resource_equals},
    },
};
use objc2_core_foundation::{CGPoint, CGSize};
use objc2_core_graphics::CGDirectDisplayID;

use crate::commands::Command;
use crate::events::Event;
use crate::manager::{ProcessApi, Window, WindowPane};
use crate::skylight::WinID;
pub use systems::gather_displays;
pub use systems::run_initial_oneshot_systems;

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
    app.add_systems(
        Update,
        (
            // NOTE: To avoid weird timing issues, the dispatcher should be the first one.
            systems::dispatch_toplevel_triggers,
            triggers::reshuffle_around_window,
            systems::add_launched_process,
            systems::add_launched_application,
            systems::fresh_marker_cleanup,
            systems::timeout_ticker,
            systems::retry_stray_focus,
            systems::find_orphaned_spaces,
            systems::animate_windows,
            systems::animate_resize_windows,
        ),
    );
    app.add_systems(
        Update,
        (
            systems::display_changes_watcher,
            systems::workspace_change_watcher,
        )
            .run_if(resource_equals(PollForNotifications(true))),
    );
}

/// Registers all the event triggers for the window manager.
pub fn register_triggers(app: &mut bevy::app::App) {
    app.add_observer(triggers::mouse_moved_trigger)
        .add_observer(triggers::mouse_down_trigger)
        .add_observer(triggers::mouse_dragged_trigger)
        .add_observer(triggers::workspace_change_trigger)
        .add_observer(triggers::display_change_trigger)
        .add_observer(triggers::active_display_trigger)
        .add_observer(triggers::display_add_trigger)
        .add_observer(triggers::display_remove_trigger)
        .add_observer(triggers::display_moved_trigger)
        .add_observer(triggers::front_switched_trigger)
        .add_observer(triggers::center_mouse_trigger)
        .add_observer(triggers::window_focused_trigger)
        .add_observer(triggers::swipe_gesture_trigger)
        .add_observer(triggers::mission_control_trigger)
        .add_observer(triggers::application_event_trigger)
        .add_observer(triggers::dispatch_application_messages)
        .add_observer(triggers::window_resized_trigger)
        .add_observer(triggers::window_destroyed_trigger)
        .add_observer(triggers::window_unmanaged_trigger)
        .add_observer(triggers::window_managed_trigger)
        .add_observer(triggers::spawn_window_trigger);
}

/// Marker component for the currently focused window.
#[derive(Component)]
pub struct FocusedMarker;

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
    pub origin: CGPoint,
    /// The ID of the display the window should be moved to.
    pub display_id: CGDirectDisplayID,
}

/// Component representing a request to resize a window.
#[derive(Component)]
pub struct ResizeMarker {
    /// The new size (width, height) for the window.
    pub size: CGSize,
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

/// Enum component indicating the unmanaged state of a window.
#[derive(Component)]
pub enum Unmanaged {
    /// The window is floating and not part of the tiling layout.
    Floating,
    /// The window is minimized.
    Minimized,
    /// The window is hidden.
    Hidden,
}

/// Wrapper component for a `ProcessApi` trait object, enabling dynamic dispatch for process-related operations within Bevy.
#[derive(Component)]
pub struct BProcess(pub Box<dyn ProcessApi>);

impl Deref for BProcess {
    type Target = Box<dyn ProcessApi>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for BProcess {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

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

/// Component representing a `WindowPane` that has become orphaned, typically due to a space being destroyed or reassigned.
#[derive(Component)]
pub struct OrphanedPane {
    /// The ID of the orphaned space.
    pub id: u64,
    /// The `WindowPane` that was orphaned.
    pub pane: WindowPane,
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
pub struct PollForNotifications(pub bool);

/// Bevy event trigger for general window manager events.
#[derive(BevyEvent)]
pub struct WMEventTrigger(pub Event);

/// Bevy event trigger for commands issued to the window manager.
#[derive(BevyEvent)]
pub struct CommandTrigger(pub Command);

/// Bevy event trigger for spawning new windows.
#[derive(BevyEvent)]
pub struct SpawnWindowTrigger(pub Vec<Window>);
