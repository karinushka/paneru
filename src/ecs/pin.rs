//! Window pinning (spike), ECS side. See `crate::pin_mirror` for the
//! `ScreenCaptureKit` mirror that does the actual drawing.
//!
//! A `Pinned` window is floated (pinning a tiled window floats it first) and,
//! while it does not have focus, covered by a floating-level live copy of
//! itself. Clicking the copy focuses the real window, which hides the copy.

use bevy::app::{App, Plugin, PostUpdate, PreUpdate};
use bevy::ecs::change_detection::{DetectChanges, DetectChangesMut};
use bevy::ecs::component::Component;
use bevy::ecs::entity::Entity;
use bevy::ecs::lifecycle::RemovedComponents;
use bevy::ecs::message::MessageReader;
use bevy::ecs::query::{Added, Changed, Has, With};
use bevy::ecs::schedule::IntoScheduleConfigs as _;
use bevy::ecs::schedule::common_conditions::on_message;
use bevy::ecs::system::{Commands, NonSendMut, Query, Res};
use bevy::math::IRect;
use tracing::{debug, warn};

use super::params::Windows;
use super::{
    FocusedMarker, MissionControlActive, PostUpdateSet, PreUpdateSet, SpawnCommandsExt, Unmanaged,
};
use crate::commands::filter_window_operations;
use crate::events::Event;
use crate::manager::{Origin, Window};
use crate::pin_mirror::{PinMirrorManager, PinTarget};
use crate::types::commands::Operation;

/// Marks a window the user pinned above all others.
#[derive(Component, Debug, Default)]
pub struct Pinned;

pub struct PinPlugin;

impl Plugin for PinPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            PreUpdate,
            (pin_command_handler, pin_mirror_event_handler)
                .in_set(PreUpdateSet::Dispatch)
                .run_if(on_message::<Event>),
        );
        app.add_systems(
            PostUpdate,
            sync_pin_mirrors
                .in_set(PostUpdateSet::Ui)
                .run_if(pin_mirrors_dirty),
        );
    }
}

/// `Operation::Pin`: toggles `Pinned` on the focused window, floating it
/// first if it is tiled so the layout never moves a window the user pinned.
fn pin_command_handler(
    mut messages: MessageReader<Event>,
    windows: Windows,
    pinned: Query<(), With<Pinned>>,
    mut commands: Commands,
) {
    if filter_window_operations(&mut messages, |op| matches!(op, Operation::Pin))
        .next()
        .is_none()
    {
        return;
    }
    let Some((window, entity, unmanaged)) = windows
        .focused()
        .and_then(|(_, entity)| windows.get_managed(entity))
    else {
        return;
    };
    let window_id = window.id();
    let Ok(mut entity_commands) = commands.get_entity(entity) else {
        return;
    };
    if pinned.contains(entity) {
        debug!(window_id, "unpinning window");
        entity_commands.try_remove::<Pinned>();
    } else {
        debug!(window_id, "pinning window");
        if unmanaged.is_none() {
            entity_commands.try_insert(Unmanaged::Floating);
        }
        entity_commands.try_insert(Pinned);
    }
}

/// Mirror lifecycle signals from `PinMirrorManager`. Clicks are routed to the
/// shared click-to-focus path in `focus::handle_tab_indicator_click`.
fn pin_mirror_event_handler(
    mut messages: MessageReader<Event>,
    windows: Windows,
    mut pinned: Query<&mut Pinned>,
    mut commands: Commands,
) {
    for event in messages.read() {
        match event {
            Event::PinMirrorReady { window_id } => {
                // Re-run the sync so the panel shows now that it has content.
                if let Some((_, entity)) = windows.find(*window_id)
                    && let Ok(mut pin) = pinned.get_mut(entity)
                {
                    pin.set_changed();
                }
            }
            Event::PinMirrorStopped { window_id } => {
                if let Some((_, entity)) = windows.find(*window_id)
                    && pinned.contains(entity)
                    && let Ok(mut entity_commands) = commands.get_entity(entity)
                {
                    warn!(window_id, "pin mirror stopped; unpinning");
                    entity_commands.try_remove::<Pinned>();
                }
            }
            _ => {}
        }
    }
}

type PinnedWindows<'w, 's> = Query<
    'w,
    's,
    (
        &'static Window,
        Entity,
        Has<FocusedMarker>,
        Option<&'static Unmanaged>,
    ),
    With<Pinned>,
>;

#[allow(clippy::too_many_arguments)]
fn pin_mirrors_dirty(
    any_pinned: Query<(), With<Pinned>>,
    pinned_changed: Query<(), Changed<Pinned>>,
    mut pinned_removed: RemovedComponents<Pinned>,
    pinned_moved: Query<(), (With<Pinned>, Changed<Window>)>,
    pinned_unmanaged: Query<(), (With<Pinned>, Changed<Unmanaged>)>,
    focus_gained: Query<(), (With<Pinned>, Added<FocusedMarker>)>,
    mut focus_lost: RemovedComponents<FocusedMarker>,
    mission_control: Option<Res<MissionControlActive>>,
) -> bool {
    // Drain both readers every run so stale removals never trigger later.
    let unpinned = pinned_removed.read().count() > 0;
    let unfocused = focus_lost.read().count() > 0;
    if unpinned || !pinned_changed.is_empty() {
        return true;
    }
    if any_pinned.is_empty() {
        return false;
    }
    unfocused
        || !pinned_moved.is_empty()
        || !pinned_unmanaged.is_empty()
        || !focus_gained.is_empty()
        || mission_control.is_some_and(|mc| mc.is_changed())
}

/// Pushes every pinned window's frame and visibility to the mirror manager.
/// Reads ECS state only (no AX calls).
fn sync_pin_mirrors(
    pinned: PinnedWindows,
    manager: Option<NonSendMut<PinMirrorManager>>,
    mission_control: Option<Res<MissionControlActive>>,
    mut commands: Commands,
) {
    let Some(mut manager) = manager else {
        return;
    };
    let mission_control = mission_control.is_some_and(|mc| mc.0);
    let targets = pinned
        .iter()
        .map(|(window, _, focused, unmanaged)| PinTarget {
            window_id: window.id(),
            frame: content_rect(window),
            visible: !focused
                && !mission_control
                && !matches!(unmanaged, Some(Unmanaged::Minimized | Unmanaged::Hidden)),
        })
        .collect::<Vec<_>>();

    let failed = manager.sync(&targets);
    if failed.is_empty() {
        return;
    }
    // Fail closed: drop the pin rather than retrying capture every tick.
    commands.flash_message("Pinning needs Screen Recording permission".to_string(), 2.0);
    for window_id in failed {
        if let Some((_, entity, _, _)) = pinned
            .iter()
            .find(|(window, _, _, _)| window.id() == window_id)
            && let Ok(mut entity_commands) = commands.get_entity(entity)
        {
            entity_commands.try_remove::<Pinned>();
        }
    }
}

/// `Window::frame()` includes Paneru's padding; the mirror must match the
/// real window's own rect or it drifts off it.
fn content_rect(window: &Window) -> IRect {
    let frame = window.frame();
    let pad = Origin::new(window.horizontal_padding(), window.vertical_padding());
    IRect::from_corners(frame.min + pad, frame.max - pad)
}
