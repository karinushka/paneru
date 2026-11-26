//! Window pinning (spike), ECS side. See `crate::pin_mirror` for the
//! `ScreenCaptureKit` mirror that does the actual drawing.
//!
//! A `Pinned` window is floated (pinning a tiled window floats it first) and,
//! while it does not have focus, covered by a floating-level live copy of
//! itself. Clicking the copy focuses the real window, which hides the copy.

use bevy::app::{App, Plugin, PostUpdate, PreUpdate};
use bevy::ecs::change_detection::Ref;
use bevy::ecs::change_detection::{DetectChanges, DetectChangesMut};
use bevy::ecs::component::Component;
use bevy::ecs::entity::Entity;
use bevy::ecs::lifecycle::RemovedComponents;
use bevy::ecs::message::MessageReader;
use bevy::ecs::query::{Added, Changed, Has, Or, With, Without};
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
use crate::config::Config;
use crate::events::Event;
use crate::manager::{Origin, Window};
use crate::pin_mirror::{PinMirrorManager, PinTarget};
use crate::types::commands::Operation;

/// Marks a window the user pinned above all others.
#[derive(Component, Debug, Default)]
pub struct Pinned;

/// A pin added by the `pin_floating` option rather than by `window_pin`, so
/// turning the option off removes only these.
#[derive(Component, Debug, Default)]
pub struct AutoPinned;

/// Keeps `pin_floating` from re-pinning a window the user unpinned, or whose
/// mirror failed. Cleared when the window is tiled again.
#[derive(Component, Debug, Default)]
pub struct PinOptOut;

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
            (
                (unpin_retiled_windows, auto_pin_floating).run_if(pin_membership_dirty),
                sync_pin_mirrors.run_if(pin_mirrors_dirty),
            )
                .chain()
                .in_set(PostUpdateSet::Ui),
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
        entity_commands.try_remove::<(Pinned, AutoPinned)>();
        entity_commands.try_insert(PinOptOut);
    } else {
        debug!(window_id, "pinning window");
        if unmanaged.is_none() {
            entity_commands.try_insert(Unmanaged::Floating);
        }
        entity_commands.try_remove::<PinOptOut>();
        entity_commands.try_insert(Pinned);
    }
}

/// Whether floating/tiled membership or the `pin_floating` option changed.
fn pin_membership_dirty(
    config: Option<Res<Config>>,
    unmanaged_changed: Query<(), Changed<Unmanaged>>,
    mut unmanaged_removed: RemovedComponents<Unmanaged>,
) -> bool {
    let removed = unmanaged_removed.read().count() > 0;
    removed || !unmanaged_changed.is_empty() || config.is_some_and(|config| config.is_changed())
}

type RetiledPins<'w, 's> =
    Query<'w, 's, Entity, (Without<Unmanaged>, Or<(With<Pinned>, With<PinOptOut>)>)>;
type UnmanagedPinState<'w, 's> =
    Query<'w, 's, (Entity, Ref<'static, Unmanaged>, Has<Pinned>, Has<PinOptOut>)>;

/// A pinned window that is tiled again (e.g. `manage`) loses its pin: pinning
/// only makes sense for windows the layout does not place.
fn unpin_retiled_windows(retiled: RetiledPins, mut commands: Commands) {
    for entity in &retiled {
        if let Ok(mut entity_commands) = commands.get_entity(entity) {
            entity_commands.try_remove::<(Pinned, AutoPinned, PinOptOut)>();
        }
    }
}

/// `pin_floating`: pins windows as they become floating, pins every floating
/// window when the option is turned on, and drops the auto pins when it is
/// turned off. Manual pins (`window_pin`) are left alone either way.
fn auto_pin_floating(
    config: Option<Res<Config>>,
    floating: UnmanagedPinState,
    auto_pinned: Query<Entity, With<AutoPinned>>,
    mut commands: Commands,
) {
    let Some(config) = config else {
        return;
    };
    if !config.pin_floating() {
        if config.is_changed() {
            for entity in &auto_pinned {
                if let Ok(mut entity_commands) = commands.get_entity(entity) {
                    entity_commands.try_remove::<(Pinned, AutoPinned)>();
                }
            }
        }
        return;
    }
    for (entity, unmanaged, pinned, opted_out) in &floating {
        if pinned
            || opted_out
            || !matches!(*unmanaged, Unmanaged::Floating)
            || !(config.is_changed() || unmanaged.is_changed())
        {
            continue;
        }
        if let Ok(mut entity_commands) = commands.get_entity(entity) {
            debug!(?entity, "pin_floating: pinning floating window");
            entity_commands.try_insert((Pinned, AutoPinned));
        }
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
                    entity_commands.try_remove::<(Pinned, AutoPinned)>();
                    entity_commands.try_insert(PinOptOut);
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
        Has<AutoPinned>,
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
    config: Option<Res<Config>>,
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
        // `pin_indicator` may have changed which windows carry a badge.
        || config.is_some_and(|config| config.is_changed())
}

/// Pushes every pinned window's frame and visibility to the mirror manager.
/// Reads ECS state only (no AX calls).
fn sync_pin_mirrors(
    pinned: PinnedWindows,
    manager: Option<NonSendMut<PinMirrorManager>>,
    mission_control: Option<Res<MissionControlActive>>,
    config: Option<Res<Config>>,
    mut commands: Commands,
) {
    let Some(mut manager) = manager else {
        return;
    };
    let mission_control = mission_control.is_some_and(|mc| mc.0);
    let indicator = config
        .map(|config| config.pin_indicator())
        .unwrap_or_default();
    let targets = pinned
        .iter()
        .map(|(window, _, focused, auto_pinned, unmanaged)| {
            let on_screen = !mission_control
                && !matches!(unmanaged, Some(Unmanaged::Minimized | Unmanaged::Hidden));
            PinTarget {
                window_id: window.id(),
                frame: content_rect(window),
                visible: on_screen && !focused,
                badge: on_screen && indicator.shows(auto_pinned),
            }
        })
        .collect::<Vec<_>>();

    let failed = manager.sync(&targets);
    if failed.is_empty() {
        return;
    }
    // Fail closed: drop the pin rather than retrying capture every tick.
    commands.flash_message("Pinning needs Screen Recording permission".to_string(), 2.0);
    for window_id in failed {
        if let Some((_, entity, _, _, _)) = pinned
            .iter()
            .find(|(window, _, _, _, _)| window.id() == window_id)
            && let Ok(mut entity_commands) = commands.get_entity(entity)
        {
            entity_commands.try_remove::<(Pinned, AutoPinned)>();
            entity_commands.try_insert(PinOptOut);
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
