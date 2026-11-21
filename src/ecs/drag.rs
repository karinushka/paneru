//! Reorder a tiled column while macOS moves its window with the pointer.

use bevy::app::{App, Update};
use bevy::ecs::change_detection::DetectChangesMut as _;
use bevy::ecs::component::Component;
use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::query::{Has, Without};
use bevy::ecs::resource::Resource;
use bevy::ecs::schedule::IntoScheduleConfigs as _;
use bevy::ecs::system::{Commands, ParamSet, Query, Res, ResMut};
use bevy::math::IRect;
use std::time::Duration;
use tracing::debug;

use crate::config::Config;
use crate::ecs::desktop::DesktopCensus;
use crate::ecs::layout::LayoutStrip;
use crate::ecs::params::Windows;
use crate::ecs::workspace::PreviousStripPosition;
use crate::ecs::{
    ActiveWorkspaceMarker, Bounds, DockPosition, FocusedMarker, LayoutPosition,
    MissionControlActive, MouseHeldMarker, Position, RepositionMarker, ResizeMarker, Scrolling,
    SelectedVirtualMarker, SpawnCommandsExt as _, Timeout, UpdateSet, VerifyWindowPosition,
};
use crate::events::{Event, MouseButton};
use crate::manager::{Display, Origin, Window, WindowManager, origin_from};
use crate::platform::{WorkspaceId, input::set_dragging};
use crate::util::round_px;

#[derive(Default, Resource)]
pub(crate) struct WindowDrag(Option<Press>);

struct Press {
    window: Entity,
    strip: Entity,
    frame: IRect,
    pointer: Option<Origin>,
    end: Option<End>,
    drag: Option<Drag>,
}

#[derive(Clone, Copy)]
enum End {
    Drop,
    Cancel,
}

struct Drag {
    home: Entity,
    home_width: i32,
    members: Vec<(Entity, Option<i32>)>,
    // The last display under the pointer and its Space when the census is unavailable.
    entered: Option<(Entity, Option<WorkspaceId>)>,
    crossed: bool,
    origin: usize,
    held: Vec<Entity>,
}

/// Briefly ignore stale Space membership while a dropped column reaches its slot.
#[derive(Component)]
pub(super) struct ColumnInTransit {
    members: Vec<Entity>,
}

impl ColumnInTransit {
    pub(super) fn contains(&self, entity: Entity) -> bool {
        self.members.contains(&entity)
    }
}

impl WindowDrag {
    #[cfg(test)]
    pub(crate) fn pressed(&self) -> bool {
        self.0.is_some()
    }

    pub(super) fn press(&mut self, window: Option<(Entity, Entity, IRect)>) {
        self.0 = window.map(|(window, strip, frame)| Press {
            window,
            strip,
            frame,
            pointer: None,
            end: None,
            drag: None,
        });
    }

    pub(super) fn track(&mut self, event: &Event) {
        let Some(press) = self.0.as_mut().filter(|press| press.end.is_none()) else {
            return;
        };
        match event {
            Event::MouseDragged {
                point,
                button: MouseButton::Left,
                ..
            } => {
                press.pointer = Some(origin_from(*point));
            }
            Event::MouseUp {
                point,
                button: MouseButton::Left,
                ..
            }
            | Event::MouseMoved { point, .. } => {
                press.pointer = press.pointer.map(|_| origin_from(*point));
                press.end = Some(End::Drop);
            }
            Event::CancelDrag => press.end = Some(End::Cancel),
            _ => {}
        }
    }

    pub(super) fn holds(&self, entity: Entity) -> bool {
        self.0
            .as_ref()
            .and_then(|press| press.drag.as_ref())
            .is_some_and(|drag| drag.held.contains(&entity))
    }

    pub(super) fn carries(&self, entity: Entity) -> bool {
        self.0
            .as_ref()
            .and_then(|press| press.drag.as_ref())
            .is_some_and(|drag| drag.members.iter().any(|(member, _)| *member == entity))
    }

    pub(super) fn keeps_strip(&self, entity: Entity) -> bool {
        self.0
            .as_ref()
            .and_then(|press| press.drag.as_ref())
            .is_some_and(|drag| drag.home == entity)
    }

    pub(crate) fn dragging(&self) -> bool {
        self.0.as_ref().is_some_and(|press| press.drag.is_some())
    }
}

pub(super) fn register(app: &mut App) {
    app.init_resource::<WindowDrag>().add_systems(
        Update,
        reorder_column
            .in_set(UpdateSet::Workspace)
            .run_if(|drag: Res<WindowDrag>| drag.0.is_some()),
    );
}

type DragStrips<'w, 's> = Query<
    'w,
    's,
    (
        &'static mut LayoutStrip,
        &'static ChildOf,
        &'static Position,
        Has<RepositionMarker>,
        Has<Scrolling>,
        Has<PreviousStripPosition>,
        Has<SelectedVirtualMarker>,
        Entity,
    ),
>;
type DragWindows<'w, 's> = Query<
    'w,
    's,
    (
        &'static Window,
        Has<RepositionMarker>,
        Has<ResizeMarker>,
        Has<FocusedMarker>,
    ),
    Without<LayoutStrip>,
>;

type Displays<'w, 's> = Query<'w, 's, (Entity, &'static Display, Option<&'static DockPosition>)>;
type LandingGeometry<'w, 's> = (
    Query<'w, 's, &'static mut LayoutPosition>,
    Query<'w, 's, &'static mut Bounds>,
);

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn reorder_column(
    mut state: ResMut<WindowDrag>,
    mut strips: DragStrips,
    windows: DragWindows,
    mut geometry: ParamSet<(Windows, LandingGeometry)>,
    displays: Displays,
    desktops: Option<Res<DesktopCensus>>,
    manager: Res<WindowManager>,
    mission_control: Res<MissionControlActive>,
    overview: Option<Res<crate::ecs::overview::OverviewPhase>>,
    config: Res<Config>,
    mouse_held: Query<(Entity, &MouseHeldMarker)>,
    mut commands: Commands,
) {
    let Some(press) = state.0.as_mut() else {
        return;
    };
    let Ok((strip, parent, _, moving, scrolling, parked, _, _)) = strips.get(press.strip) else {
        finish(
            &mut state,
            &mut strips,
            &mut geometry.p1(),
            &windows,
            &mouse_held,
            &mut commands,
        );
        return;
    };
    let (Ok(index), Ok((window, repositioning, resizing, focused))) =
        (strip.index_of(press.window), windows.get(press.window))
    else {
        finish(
            &mut state,
            &mut strips,
            &mut geometry.p1(),
            &windows,
            &mouse_held,
            &mut commands,
        );
        return;
    };

    let frame = window.frame();
    let available = !mission_control.0 && crate::ecs::overview::overview_inactive(overview);
    if press.drag.is_none() {
        let display = displays.get(parent.parent()).ok();
        let shown = display.is_some_and(|(_, display, _)| {
            desktops
                .as_ref()
                .and_then(|census| census.active_space(display.id()))
                .is_none_or(|active| active == strip.id())
        });
        if !available
            || !shown
            || parked
            || strip.is_fullscreen()
            || matches!(press.end, Some(End::Cancel))
        {
            state.0 = None;
            return;
        }
        let layout_moving = moving || scrolling || repositioning || resizing;
        if layout_moving || press.pointer.is_none() || frame.min == press.frame.min {
            if press.end.is_some() {
                state.0 = None;
            } else if layout_moving || press.pointer.is_none() {
                press.frame = frame;
            }
            return;
        }
        if frame.size() != press.frame.size() {
            state.0 = None;
            return;
        }
        let held = strip
            .tab_group(press.window)
            .unwrap_or_else(|| vec![press.window]);
        let members = strip
            .get(index)
            .map(|column| {
                let windows = geometry.p0();
                column
                    .window_iter()
                    .map(|entity| {
                        (
                            entity,
                            windows.moving_frame(entity).map(|rect| rect.width()),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        let home_width = display.map_or(0, |(_, display, dock)| {
            display.actual_display_bounds(dock, &config).width()
        });
        for &entity in &held {
            if let Ok(mut entity_commands) = commands.get_entity(entity) {
                entity_commands.try_remove::<(RepositionMarker, VerifyWindowPosition)>();
            }
        }
        debug!(
            window_id = window.id(),
            column = index,
            "Started tiled title-bar drag"
        );
        press.drag = Some(Drag {
            home: press.strip,
            home_width,
            members,
            entered: None,
            crossed: false,
            origin: index,
            held,
        });
        set_dragging(true);
    }

    let Some(drag) = &mut press.drag else {
        return;
    };
    let end = press.end.or((!available).then_some(End::Cancel));
    let target = if matches!(end, Some(End::Cancel)) {
        None
    } else {
        destination(
            press.pointer,
            drag,
            &displays,
            &strips,
            desktops.as_deref(),
            &manager,
        )
    };
    let dest = target.unwrap_or(drag.home);
    let current = press.strip;
    let drop_slot = strips.get(dest).ok().map(|(strip, _, offset, ..)| {
        if target.is_some() {
            slot(
                strip,
                (dest == current).then_some(index),
                offset.0.x,
                frame.min.x,
                &geometry.p0(),
            )
        } else {
            drag.origin
                .min(strip.len().saturating_sub(usize::from(dest == current)))
        }
    });

    if let Some(drop_slot) = drop_slot {
        if dest == current {
            if index != drop_slot
                && let Ok((mut strip, ..)) = strips.get_mut(current)
            {
                strip.move_column(index, drop_slot);
            }
        } else if let Ok([mut from, mut to]) = strips.get_many_mut([current, dest])
            && let Ok((_, target_display, dock)) = displays.get(to.1.parent())
            && from.0.transfer_column(&mut to.0, index, drop_slot)
        {
            let width = target_display.actual_display_bounds(dock, &config).width();
            let (_, mut bounds) = geometry.p1();
            size_column(drag, width, &mut bounds, &mut commands);
            press.strip = dest;
            drag.crossed = true;
            debug!(
                window_id = window.id(),
                display_id = target_display.id(),
                column = drop_slot,
                "Carried tiled column to display"
            );
        }
    }
    if let Some(end) = end {
        if matches!(end, End::Drop) && target == Some(press.strip) {
            if focused
                && press.strip != drag.home
                && let Ok(mut strip) = commands.get_entity(press.strip)
            {
                strip.try_insert(ActiveWorkspaceMarker);
            }
            if config.window_hidden_ratio() < 1.0 {
                commands.reshuffle_around(press.window);
            }
        }
        debug!(window_id = window.id(), "Finished tiled title-bar drag");
        finish(
            &mut state,
            &mut strips,
            &mut geometry.p1(),
            &windows,
            &mouse_held,
            &mut commands,
        );
    }
}

fn destination(
    pointer: Option<Origin>,
    drag: &mut Drag,
    displays: &Displays,
    strips: &DragStrips,
    desktops: Option<&DesktopCensus>,
    manager: &WindowManager,
) -> Option<Entity> {
    let point = pointer?;
    let under = displays
        .iter()
        .find(|(_, display, _)| display.frame().contains(point));
    // Use the reactive census when available. The older query is a fallback
    // when first entering a display, not a WindowServer round-trip per frame.
    let Some((entity, display, _)) = under else {
        drag.entered = None;
        return None;
    };
    let census_space = desktops.and_then(|census| census.active_space(display.id()));
    if drag.entered.map(|(entity, _)| entity) != Some(entity) {
        let space = census_space.or_else(|| manager.active_display_space(display.id()).ok());
        drag.entered = Some((entity, space));
    }
    let active_space = census_space.or(drag.entered.and_then(|(_, space)| space))?;
    strips
        .iter()
        .filter(|(strip, parent, _, _, _, parked, _, _)| {
            parent.parent() == entity
                && strip.id() == active_space
                && !parked
                && !strip.is_fullscreen()
        })
        .min_by_key(|(strip, _, _, _, _, _, selected, _)| (!*selected, strip.virtual_index))
        .map(|(.., strip_entity)| strip_entity)
}

fn size_column(drag: &Drag, width: i32, bounds: &mut Query<&mut Bounds>, commands: &mut Commands) {
    if drag.home_width <= 0 || width <= 0 {
        return;
    }
    for &(entity, original) in &drag.members {
        if let Some(original) = original
            && let Ok(mut size) = bounds.get_mut(entity)
        {
            let scaled =
                round_px(f64::from(original) * f64::from(width) / f64::from(drag.home_width));
            size.0.x = if width == drag.home_width {
                original
            } else {
                scaled.clamp(1, width)
            };
            if let Ok(mut entity_commands) = commands.get_entity(entity) {
                entity_commands.try_remove::<ResizeMarker>();
            }
        }
    }
}

fn finish(
    state: &mut WindowDrag,
    strips: &mut DragStrips,
    geometry: &mut LandingGeometry,
    windows: &DragWindows,
    mouse_held: &Query<(Entity, &MouseHeldMarker)>,
    commands: &mut Commands,
) {
    let Some(press) = state.0.take() else {
        return;
    };
    let Some(drag) = press.drag else {
        return;
    };
    set_dragging(false);
    if let Ok((mut home, ..)) = strips.get_mut(drag.home) {
        // Let normal empty-workspace reaping see the source after the drag.
        home.set_changed();
    }
    if drag.crossed {
        let timeout = Timeout::new(Duration::from_secs(2), None, commands);
        commands.spawn((
            timeout,
            ColumnInTransit {
                members: drag.members.into_iter().map(|(entity, _)| entity).collect(),
            },
        ));
    }
    let (positions, sizes) = geometry;
    for entity in drag.held {
        if let Ok(mut position) = positions.get_mut(entity) {
            position.set_changed();
        }
        if let Ok(mut size) = sizes.get_mut(entity)
            && windows
                .get(entity)
                .is_ok_and(|(window, ..)| window.frame().size() != size.0)
        {
            size.set_changed();
        }
    }
    for (entity, marker) in mouse_held {
        if marker.0 == press.window
            && let Ok(mut entity_commands) = commands.get_entity(entity)
        {
            entity_commands.try_despawn();
        }
    }
}

/// Choose the slot nearest the dragged window's left edge. Passing the
/// midpoint of a neighbour moves the dragged column to its other side.
fn slot(
    strip: &LayoutStrip,
    held: Option<usize>,
    mut edge: i32,
    x: i32,
    geometry: &Windows,
) -> usize {
    let mut index = 0;
    for (column_index, column) in strip.columns().enumerate() {
        if Some(column_index) == held {
            continue;
        }
        let width = column
            .width(&|entity| geometry.moving_frame(entity))
            .unwrap_or_default();
        if x <= edge.saturating_add(width / 2) {
            break;
        }
        edge = edge.saturating_add(width);
        index += 1;
    }
    index
}
