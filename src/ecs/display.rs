use bevy::app::{App, Plugin, PreUpdate, Update};
use bevy::ecs::change_detection::DetectChangesMut;
use bevy::ecs::component::Component;
use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::lifecycle::Add;
use bevy::ecs::message::{MessageReader, MessageWriter};
use bevy::ecs::observer::On;
use bevy::ecs::query::{Changed, Has, Or, With, Without};
use bevy::ecs::resource::Resource;
use bevy::ecs::system::ResMut;
use bevy::ecs::system::{Commands, Local, NonSend, Populated, Query, Res};
use bevy::math::IRect;
use bevy::platform::collections::HashSet;
use bevy::time::Time;
use objc2_app_kit::NSScreen;
use objc2_core_graphics::CGDirectDisplayID;
use std::collections::HashMap;
use std::pin::Pin;
use std::time::Duration;
use tracing::{Level, debug, error, instrument, warn};

use crate::config::Config;
use crate::ecs::layout::LayoutStrip;
use crate::ecs::layout::clamp_origin_to_viewport;
use crate::ecs::{
    ActiveDisplayMarker, ActiveWorkspaceMarker, Bounds, DockPosition, FocusedMarker,
    FullWidthMarker, LayoutPosition, Position, ReadDisplayProperties, ResizeMarker,
    SendMessageTrigger, SpawnCommandsExt, Timeout, Unmanaged,
};
use crate::events::Event;
use crate::manager::{Display, Size, Window, WindowManager, irect_from};
use crate::platform::{PlatformCallbacks, WorkspaceId};
use crate::util::{read_screen_property, round_px};

const ORPHANED_SPACES_TIMEOUT_SEC: u64 = 30;

/// How long the geometry is re-read after a chrome change, and how often. The
/// Dock can take several seconds to report its new size, well past the
/// notification that announced it, so the watch outlives the event by a wide
/// margin and [`retile_changed_viewports`] re-arms it whenever the usable area
/// actually moves.
const CHROME_SETTLE: Duration = Duration::from_secs(10);
const CHROME_POLL: Duration = Duration::from_millis(250);

#[derive(Default, Resource)]
struct ChromeWatch {
    settling: Duration,
    since_read: Duration,
}

/// Windows as a viewport change needs them: the live frame to re-read, and the
/// layout fields that are overwritten from it.
type RefreshedWindows<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static mut Window,
        &'static mut LayoutPosition,
        &'static mut Bounds,
        Option<&'static Unmanaged>,
    ),
    Without<LayoutStrip>,
>;

/// Displays whose geometry was touched this tick, either the display itself or
/// the edge the Dock sits on.
type ResurveyedDisplays<'w, 's> =
    Populated<'w, 's, Entity, Or<(Changed<Display>, Changed<DockPosition>)>>;

pub struct DisplayEventsPlugin;

impl Plugin for DisplayEventsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ChromeWatch>();
        app.add_systems(PreUpdate, (display_change_handler, chrome_change_handler));
        app.add_systems(Update, (reconcile_displays, retile_changed_viewports))
            .add_observer(read_display_properties_trigger)
            .add_observer(cleanup_active_display_marker);
    }
}

#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
fn cleanup_active_display_marker(
    trigger: On<Add, ActiveDisplayMarker>,
    displays: Query<(Entity, Has<ActiveDisplayMarker>), With<Display>>,
    mut commands: Commands,
) {
    for (entity, active) in displays {
        if active
            && entity != trigger.entity
            && let Ok(mut cmd) = commands.get_entity(entity)
        {
            debug!("Display id {entity} lost active marker.");
            cmd.try_remove::<ActiveDisplayMarker>();
        }
    }
}

/// Handles display change events.
#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
fn display_change_handler(
    mut messages: MessageReader<Event>,
    displays: Query<(&Display, Entity, Has<ActiveDisplayMarker>)>,
    window_manager: Res<WindowManager>,
    mut commands: Commands,
) {
    if !messages
        .read()
        .any(|event| matches!(event, Event::DisplayChanged))
    {
        return;
    }

    let Ok(active_id) = window_manager.active_display_id() else {
        error!("Unable to get active display id!");
        return;
    };

    for (display, entity, focused) in displays {
        let display_id = display.id();
        if !focused
            && display_id == active_id
            && let Ok(mut cmd) = commands.get_entity(entity)
        {
            debug!("Display id {display_id} is active");
            cmd.try_insert(ActiveDisplayMarker);
        }
    }
    commands.trigger(SendMessageTrigger(Event::SpaceChanged));
}

/// Toggling Dock or menubar auto-hide changes `visibleFrame` without raising a
/// display event, so nothing would re-read the geometry. The notification also
/// lands before `AppKit` has updated `visibleFrame`, hence the settling window
/// rather than a single read; `retile_changed_viewports` reacts once the usable
/// area actually moves.
fn chrome_change_handler(
    mut messages: MessageReader<Event>,
    mut watch: ResMut<ChromeWatch>,
    clock: Res<Time>,
    displays: Query<Entity, With<Display>>,
    mut commands: Commands,
) {
    if messages.read().any(|event| {
        matches!(
            event,
            Event::ScreenParametersChanged
                | Event::MenuBarHiddenChanged { .. }
                | Event::DockDidChangePref { .. }
                | Event::DockDidRestart { .. }
        )
    }) {
        watch.settling = CHROME_SETTLE;
        watch.since_read = CHROME_POLL;
    } else if watch.settling.is_zero() {
        return;
    } else {
        let delta = clock.delta();
        watch.settling = watch.settling.saturating_sub(delta);
        watch.since_read = watch.since_read.saturating_add(delta);
    }

    if watch.since_read < CHROME_POLL {
        return;
    }
    watch.since_read = Duration::ZERO;
    for entity in displays {
        commands.trigger(ReadDisplayProperties(entity));
    }
}

/// Re-tiles the active workspace of any display whose usable area moved. Both
/// the menubar and the Dock can take or give back space without a display event
/// behind it, so the strip keeps its old viewport until something recomputes
/// it, and meanwhile macOS is free to shove the windows around itself.
#[allow(clippy::too_many_arguments)]
#[instrument(level = Level::DEBUG, skip_all)]
fn retile_changed_viewports(
    touched: ResurveyedDisplays,
    displays: Query<(&Display, Option<&DockPosition>)>,
    mut strips: Query<(&mut LayoutStrip, &mut Position, &ChildOf), With<ActiveWorkspaceMarker>>,
    mut windows: RefreshedWindows,
    window_state: Query<(Has<FullWidthMarker>, Has<FocusedMarker>), With<Window>>,
    window_manager: Res<WindowManager>,
    mut watch: ResMut<ChromeWatch>,
    config: Res<Config>,
    mut viewports: Local<HashMap<Entity, IRect>>,
    mut commands: Commands,
) {
    viewports.retain(|entity, _| displays.contains(*entity));
    for display_entity in touched {
        let Ok((display, dock)) = displays.get(display_entity) else {
            continue;
        };
        let viewport = display.actual_display_bounds(dock, &config);
        if viewports
            .insert(display_entity, viewport)
            .is_none_or(|previous| previous == viewport)
        {
            continue;
        }
        let display_id = display.id();
        debug!("display {display_id} usable area is now {viewport:?}");
        // The area just moved, so another move may still be coming.
        watch.settling = CHROME_SETTLE;

        for (mut strip, mut position, child) in &mut strips {
            if child.parent() != display_entity {
                continue;
            }
            // The strip origin carries the menubar inset, and marking the strip
            // re-binpacks the columns into the height that is left.
            if position.0.y != viewport.min.y {
                position.0.y = viewport.min.y;
            }

            let mut in_workspace = window_manager
                .windows_in_workspace(strip.id())
                .inspect_err(|err| warn!("getting windows in workspace: {err}"))
                .unwrap_or_default();

            for entity in strip.all_windows() {
                let Ok((_, ref mut window, ref mut layout_position, ref mut bounds, _)) =
                    windows.get_mut(entity)
                else {
                    continue;
                };
                // macOS may have moved the window while the usable area changed,
                // so the frame has to be re-read rather than trusted.
                layout_position.set_changed();
                let Ok(frame) = window.update_frame() else {
                    continue;
                };
                let (full_width, focused) = window_state.get(entity).unwrap_or_default();
                let width = if full_width {
                    viewport.width()
                } else {
                    frame.width().clamp(0, viewport.width())
                };
                // Layout owns the new size; an old animation target would fight it.
                bounds.0 = Size::new(width, frame.height().clamp(0, viewport.height()));
                commands.entity(entity).remove::<ResizeMarker>();
                if focused {
                    commands.reshuffle_around(entity);
                }

                in_workspace.retain(|window_id| *window_id != window.id());
            }

            // Whatever is left in the workspace sits outside the strip.
            let floating = in_workspace
                .into_iter()
                .filter_map(|window_id| {
                    windows
                        .iter()
                        .find_map(|(entity, window, _, _, unmanaged)| {
                            (window_id == window.id())
                                .then_some(unmanaged.zip(Some((entity, window.frame()))))
                        })
                        .flatten()
                })
                .filter_map(|(unmanaged, window)| {
                    matches!(unmanaged, Unmanaged::Floating).then_some(window)
                })
                .collect::<Vec<_>>();
            for (window_entity, frame) in floating {
                let origin = clamp_origin_to_viewport(frame.min, frame.size(), viewport);
                if origin != frame.min {
                    debug!("repositioning floating window {window_entity}");
                    commands.reposition_entity(window_entity, origin);
                }
            }

            // Relayout so columns retake a viewport that grew, rather than only
            // shrinking to fit one that got smaller.
            strip.set_changed();
        }
    }
}

/// Full reconciliation of the ECS display set against the OS truth.
///
/// Runs on events where the per-display add/remove/move flags are unreliable or
/// absent: waking from sleep, resolution / arrangement changes, and configuration
/// events. Rather than trust a single `display_id` flag, it diffs the live
/// `present_displays()` list against the spawned `Display` entities and applies
/// the same add / remove / move primitives the event handlers use. It also
/// forces the active workspace to re-tile, because macOS relocates windows while
/// asleep even when the display set is unchanged.
pub(crate) fn reconcile_displays(
    mut messages: MessageReader<Event>,
    workspaces: Query<(&LayoutStrip, Entity, Option<&ChildOf>)>,
    mut displays: Query<(&mut Display, Entity)>,
    window_manager: Res<WindowManager>,
    mut retries: Local<u8>,
    mut commands: Commands,
) {
    const DISPLAY_RETRY_TIMEOUT: u64 = 5;
    const DISPLAY_RETRIES: u8 = 3;

    let needs_reconcile = messages.read().any(|event| {
        matches!(
            event,
            Event::SystemWoke { .. }
                | Event::DisplayAdded { .. }
                | Event::DisplayRemoved { .. }
                | Event::DisplayMoved { .. }
                | Event::DisplayResized { .. }
                | Event::DisplayConfigured { .. }
        )
    });
    if !needs_reconcile {
        return;
    }

    debug!("Reconciling displays against OS after wake / resize / configure");

    let mut present_displays: HashMap<CGDirectDisplayID, _> = window_manager
        .0
        .present_displays()
        .into_iter()
        .map(|(display, workspaces)| (display.id(), (display, workspaces)))
        .collect();
    if present_displays.is_empty() {
        warn!("No present displays found... retrying again in {DISPLAY_RETRY_TIMEOUT} seconds.");
        *retries = retries.saturating_sub(1);
        if *retries > 0 {
            let retry_displays = move |mut messages: MessageWriter<Event>| {
                messages.write(Event::SystemWoke {
                    msg: "Retrying display scan".to_string(),
                });
            };
            let system_id = commands.register_system(retry_displays);
            Timeout::callback(
                Duration::from_secs(DISPLAY_RETRY_TIMEOUT),
                system_id,
                &mut commands,
            );
        }
    }
    *retries = DISPLAY_RETRIES;

    let existing_displays: HashMap<CGDirectDisplayID, _> = displays
        .iter()
        .map(|(display, workspaces)| (display.id(), (display, workspaces)))
        .collect();

    let present_ids = present_displays.keys().copied().collect::<HashSet<_>>();
    let existing_ids = existing_displays.keys().copied().collect::<HashSet<_>>();

    // Displays that vanished while we were away (e.g. unplugged during sleep).
    for display_id in existing_ids.difference(&present_ids) {
        let Some((display, _)) = existing_displays.get(display_id) else {
            error!("Unable to find removed display: {display_id}");
            continue;
        };
        remove_display(display, &workspaces, &displays, &mut commands);
    }

    // Displays that appeared while we were away.
    for display_id in present_ids.difference(&existing_ids) {
        let Some((display, workspace_ids)) = present_displays.remove(display_id) else {
            error!("Unable to find added display: {display_id}");
            continue;
        };
        add_display(display, &workspace_ids, &workspaces, &mut commands);
    }

    // Displays that are still present: refresh their bounds (resolution or
    // menubar may have changed) and re-home any workspaces that drifted.
    for display_id in present_ids.intersection(&existing_ids) {
        move_display(
            *display_id,
            &mut displays,
            &window_manager,
            &workspaces,
            &mut commands,
        );
    }

    commands.trigger(SendMessageTrigger(Event::DisplayChanged));
}

#[instrument(level = Level::DEBUG, skip_all, fields(display_id))]
fn add_display(
    display: Display,
    workspace_ids: &[WorkspaceId],
    existing_strips: &Query<(&LayoutStrip, Entity, Option<&ChildOf>)>,
    commands: &mut Commands,
) {
    let display_id = display.id();
    debug!("Display Added: {display_id}");

    let display_bounds = display.bounds();
    let display_entity = commands.spawn(display).id();
    commands.trigger(ReadDisplayProperties(display_entity));

    reparent_existing_workspaces(
        workspace_ids,
        display_entity,
        &display_bounds,
        existing_strips,
        commands,
    );
}

#[instrument(level = Level::DEBUG, skip_all, fields(display_id))]
fn remove_display(
    display: &Display,
    workspaces: &Query<(&LayoutStrip, Entity, Option<&ChildOf>)>,
    displays: &Query<(&mut Display, Entity)>,
    commands: &mut Commands,
) {
    let display_id = display.id();
    debug!("Display Removed: {display_id:?}");
    let Some((display, display_entity)) = displays
        .into_iter()
        .find(|(display, _)| display.id() == display_id)
    else {
        error!("Unable to find removed display!");
        return;
    };

    for (strip, entity, _) in workspaces
        .into_iter()
        .filter(|(_, _, child)| child.is_some_and(|child| child.parent() == display_entity))
    {
        let display_id = display.id();
        debug!(
            "orphaning strip {} after removal of display {display_id}.",
            strip.id(),
        );
        let timeout = Timeout::new(
            Duration::from_secs(ORPHANED_SPACES_TIMEOUT_SEC),
            Some(format!(
                "Orphaned strip {} ({strip}) could not be re-inserted after {ORPHANED_SPACES_TIMEOUT_SEC}s.",
                strip.id()
            )),
            commands,
        );
        if let Ok(mut commands) = commands.get_entity(entity) {
            commands.try_insert(timeout);
        }
        if let Ok(mut commands) = commands.get_entity(display_entity) {
            commands.detach_child(entity);
        }
    }

    if let Ok(mut commands) = commands.get_entity(display_entity) {
        commands.try_despawn();
    }
}

#[instrument(level = Level::DEBUG, skip_all, fields(display_id))]
fn move_display(
    display_id: CGDirectDisplayID,
    displays: &mut Query<(&mut Display, Entity)>,
    window_manager: &Res<WindowManager>,
    existing_strips: &Query<(&LayoutStrip, Entity, Option<&ChildOf>)>,
    commands: &mut Commands,
) {
    debug!("Display Moved: {display_id:?}");
    let Some((mut display, display_entity)) = displays
        .iter_mut()
        .find(|(display, _)| display.id() == display_id)
    else {
        error!("Unable to find moved display!");
        return;
    };
    let Some((moved_display, workspace_ids)) = window_manager
        .0
        .present_displays()
        .into_iter()
        .find(|(display, _)| display.id() == display_id)
    else {
        return;
    };
    *display = moved_display;
    commands.trigger(ReadDisplayProperties(display_entity));

    reparent_existing_workspaces(
        &workspace_ids,
        display_entity,
        &display.bounds(),
        existing_strips,
        commands,
    );
}

fn reparent_existing_workspaces(
    workspace_ids: &[WorkspaceId],
    display_entity: Entity,
    display_bounds: &IRect,
    existing_strips: &Query<(&LayoutStrip, Entity, Option<&ChildOf>)>,
    commands: &mut Commands,
) {
    // Verifies that a moved display has all the workspaces which it owns.
    for &id in workspace_ids {
        let mut found = false;
        for (strip, entity, child) in existing_strips {
            if strip.id() == id {
                found = true;
                if child.is_none_or(|child| child.parent() != display_entity) {
                    // Re-parent this workspace
                    if let Ok(mut cmd) = commands.get_entity(entity) {
                        debug!("reparenting workspace {id} to display {display_entity}");
                        cmd.try_remove::<Timeout>()
                            .try_remove::<ChildOf>()
                            .try_insert(ChildOf(display_entity));
                    }
                }
            }
        }

        if !found {
            // New workspace.
            let origin = display_bounds.min;
            debug!("new workspace {id} on display {display_entity}");
            commands.spawn_layout_strip(LayoutStrip::new(id, 0), origin, display_entity, false);
        }
    }
}

/// Tracks whether floating windows on a workspace sit above or behind tiled
/// ones in the OS z-order. Default is `Front` (floats above tiles).
#[derive(Clone, Component, Copy)]
pub struct FloatingLayer {
    pub workspace_id: WorkspaceId,
    pub front: bool,
}

impl FloatingLayer {
    pub fn new(workspace_id: WorkspaceId) -> Self {
        Self {
            workspace_id,
            front: false,
        }
    }

    pub fn flip(&mut self) {
        self.front = !self.front;
    }
}

fn read_display_properties_trigger(
    trigger: On<ReadDisplayProperties>,
    mut displays: Query<(&mut Display, Entity)>,
    platform: Option<NonSend<Pin<Box<PlatformCallbacks>>>>,
    config: Option<Res<Config>>,
    mut commands: Commands,
) {
    let Ok((mut display, entity)) = displays.get_mut(trigger.event().0) else {
        return;
    };
    let display_id = display.id();

    // NSScreen::screen needs to run in the main thread, thus we run it in a NonSend trigger.
    let Some(screens) = platform.map(|platform| NSScreen::screens(platform.main_thread_marker))
    else {
        return;
    };

    let notch = read_screen_property(&screens, display_id, |screen| {
        let insets = screen.safeAreaInsets();
        debug!("notch on display {display_id}: {insets:?}");
        round_px(insets.top)
    });
    if let Some(height) = notch {
        display.set_notch_height(height);
    }

    // SLSGetDisplayMenubarHeight keeps reporting the nominal height while the
    // menubar is set to auto-hide. The gap between frame and visibleFrame is
    // what actually occupies the top edge, and it is zero while it is hidden.
    let menubar = read_screen_property(&screens, display_id, |screen| {
        let frame = screen.frame();
        let visible = screen.visibleFrame();
        round_px((frame.origin.y + frame.size.height) - (visible.origin.y + visible.size.height))
    });
    if let Some(height) = menubar {
        debug!("menubar inset on display {display_id}: {height}");
        display.set_menubar_height(height);
    }

    let dock = read_screen_property(&screens, display_id, |screen| {
        let visible_frame = irect_from(screen.visibleFrame());
        display.locate_dock(&visible_frame)
    });
    if let Some(dock) = dock {
        debug!("dock on display {display_id}: {:?}", dock);
        if let Ok(mut entity_commands) = commands.get_entity(entity) {
            entity_commands.try_insert(dock);
        }
    }

    if let Some(config) = config {
        let height = config.menubar_height();
        display.set_menubar_height_override(height);
    }
}
