//! Reorder a tiled column while macOS moves its window with the pointer.

use bevy::app::{App, Update};
use bevy::ecs::change_detection::DetectChangesMut as _;
use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::query::{Has, Without};
use bevy::ecs::resource::Resource;
use bevy::ecs::schedule::IntoScheduleConfigs as _;
use bevy::ecs::system::{Commands, ParamSet, Query, Res, ResMut};
use bevy::math::IRect;
use tracing::debug;

use crate::config::Config;
use crate::ecs::desktop::DesktopCensus;
use crate::ecs::layout::LayoutStrip;
use crate::ecs::params::Windows;
use crate::ecs::workspace::PreviousStripPosition;
use crate::ecs::{
    LayoutPosition, MissionControlActive, MouseHeldMarker, Position, RepositionMarker,
    ResizeMarker, Scrolling, SpawnCommandsExt as _, UpdateSet, VerifyWindowPosition,
};
use crate::events::{Event, MouseButton};
use crate::manager::{Display, Origin, Window, origin_from};
use crate::platform::input::set_dragging;

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
    origin: usize,
    held: Vec<Entity>,
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
    ),
>;
type DragWindows<'w, 's> = Query<
    'w,
    's,
    (&'static Window, Has<RepositionMarker>, Has<ResizeMarker>),
    Without<LayoutStrip>,
>;

#[allow(clippy::too_many_arguments)]
fn reorder_column(
    mut state: ResMut<WindowDrag>,
    mut strips: DragStrips,
    windows: DragWindows,
    mut geometry: ParamSet<(Windows, Query<&mut LayoutPosition>)>,
    displays: Query<&Display>,
    desktops: Option<Res<DesktopCensus>>,
    mission_control: Res<MissionControlActive>,
    overview: Option<Res<crate::ecs::overview::OverviewPhase>>,
    config: Res<Config>,
    mouse_held: Query<(Entity, &MouseHeldMarker)>,
    mut commands: Commands,
) {
    let Some(press) = state.0.as_mut() else {
        return;
    };
    let Ok((mut strip, parent, offset, moving, scrolling, parked)) = strips.get_mut(press.strip)
    else {
        finish(&mut state, &mut geometry.p1(), &mouse_held, &mut commands);
        return;
    };
    let (Ok(index), Ok((window, repositioning, resizing))) =
        (strip.index_of(press.window), windows.get(press.window))
    else {
        finish(&mut state, &mut geometry.p1(), &mouse_held, &mut commands);
        return;
    };

    let frame = window.frame();
    let display = displays.get(parent.parent()).ok();
    let space_is_shown = display.is_some_and(|display| {
        desktops
            .as_ref()
            .and_then(|census| census.active_space(display.id()))
            .is_none_or(|active| active == strip.id())
    });
    let available = space_is_shown
        && !parked
        && !strip.is_fullscreen()
        && !mission_control.0
        && crate::ecs::overview::overview_inactive(overview);
    if press.drag.is_none() {
        if !available || matches!(press.end, Some(End::Cancel)) {
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
            origin: index,
            held,
        });
        set_dragging(true);
    }

    let Some(drag) = &press.drag else {
        return;
    };
    let inside = press
        .pointer
        .zip(display)
        .is_some_and(|(point, display)| display.frame().contains(point));
    let end = press.end.or((!available).then_some(End::Cancel));
    let target = if matches!(end, Some(End::Cancel)) || !inside {
        drag.origin.min(strip.len() - 1)
    } else {
        slot(&strip, index, offset.0.x, frame.min.x, &geometry.p0()).unwrap_or(index)
    };
    if target != index {
        strip.move_column(index, target);
    }
    if let Some(end) = end {
        if matches!(end, End::Drop) && inside && config.window_hidden_ratio() < 1.0 {
            commands.reshuffle_around(press.window);
        }
        debug!(
            window_id = window.id(),
            column = target,
            "Finished tiled title-bar drag"
        );
        finish(&mut state, &mut geometry.p1(), &mouse_held, &mut commands);
    }
}

fn finish(
    state: &mut WindowDrag,
    positions: &mut Query<&mut LayoutPosition>,
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
    for entity in drag.held {
        if let Ok(mut position) = positions.get_mut(entity) {
            position.set_changed();
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
    held: usize,
    mut edge: i32,
    x: i32,
    geometry: &Windows,
) -> Option<usize> {
    let mut index = 0;
    for (column_index, column) in strip.columns().enumerate() {
        if column_index == held {
            continue;
        }
        let width = column.width(&|entity| geometry.moving_frame(entity))?;
        if x <= edge.saturating_add(width / 2) {
            break;
        }
        edge = edge.saturating_add(width);
        index += 1;
    }
    Some(index)
}
