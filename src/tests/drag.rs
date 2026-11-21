use std::time::Duration;

use bevy::prelude::*;
use objc2_core_foundation::CGPoint;

use crate::commands::{Command, Direction, Operation};
use crate::config::{Config, MainOptions};
use crate::ecs::drag::WindowDrag;
use crate::ecs::layout::LayoutStrip;
use crate::ecs::{ActiveWorkspaceMarker, Position, Unmanaged};
use crate::events::{Event, MouseButton};
use crate::manager::{Origin, Window};
use crate::platform::{Modifiers, WinID};

use super::*;

const SETTLE: Duration = Duration::from_secs(1);

fn order(h: &mut TestHarness) -> Vec<WinID> {
    let world = h.world();
    let columns = {
        let mut strips = world.query_filtered::<&LayoutStrip, With<ActiveWorkspaceMarker>>();
        strips
            .single(world)
            .expect("one active strip")
            .columns()
            .filter_map(crate::ecs::layout::Column::top)
            .collect::<Vec<_>>()
    };
    let mut windows = world.query::<&Window>();
    columns
        .into_iter()
        .map(|entity| windows.get(world, entity).expect("a window").id())
        .collect()
}

fn os_frame(h: &mut TestHarness, id: WinID) -> IRect {
    let world = h.world();
    let mut windows = world.query::<&Window>();
    windows
        .iter(world)
        .find(|window| window.id() == id)
        .expect("a window")
        .frame()
}

fn send(h: &mut TestHarness, event: Event) {
    h.world().write_message::<Event>(event);
    h.advance(Duration::from_millis(20));
}

fn point(origin: Origin) -> CGPoint {
    CGPoint::new(origin.x.into(), origin.y.into())
}

fn button_down(h: &mut TestHarness, at: Origin, button: MouseButton) {
    send(
        h,
        Event::MouseDown {
            point: point(at),
            modifiers: Modifiers::empty(),
            button,
        },
    );
}

fn button_up(h: &mut TestHarness, at: Origin, button: MouseButton) {
    send(
        h,
        Event::MouseUp {
            point: point(at),
            modifiers: Modifiers::empty(),
            button,
        },
    );
}

fn press(h: &mut TestHarness, at: Origin) {
    button_down(h, at, MouseButton::Left);
}

fn release(h: &mut TestHarness, at: Origin) {
    button_up(h, at, MouseButton::Left);
}

fn drag(h: &mut TestHarness, id: WinID, from: Origin, steps: &[i32]) -> Origin {
    let steps = steps
        .iter()
        .map(|dx| IVec2::new(*dx, 0))
        .collect::<Vec<_>>();
    drag_by(h, id, from, &steps)
}

fn drag_by(h: &mut TestHarness, id: WinID, from: Origin, steps: &[IVec2]) -> Origin {
    let mut at = from;
    for step in steps {
        at += *step;
        h.world().write_message::<Event>(Event::MouseDragged {
            button: MouseButton::Left,
            point: point(at),
            modifiers: Modifiers::empty(),
        });
        let frame = os_frame(h, id);
        h.mock_state.os_move_window(id, frame.min + *step);
        h.advance(Duration::from_millis(20));
    }
    at
}

fn x(h: &mut TestHarness, id: WinID) -> i32 {
    os_frame(h, id).min.x
}

fn held(h: &mut TestHarness) -> bool {
    h.world().resource::<WindowDrag>().pressed()
}

fn dragging(h: &mut TestHarness) -> bool {
    h.world().resource::<WindowDrag>().dragging()
}

fn three() -> TestHarness {
    let mut h = TestHarness::new().with_windows(3);
    h.advance(SETTLE);
    assert_eq!(order(&mut h), [0, 1, 2]);
    h
}

fn three_gliding() -> TestHarness {
    let config: Config = (
        MainOptions {
            animation_speed: Some(10.0),
            ..Default::default()
        },
        vec![],
    )
        .into();
    let mut h = TestHarness::new().with_config(config).with_windows(3);
    h.advance(SETTLE);
    assert_eq!(order(&mut h), [0, 1, 2]);
    h
}

fn title_bar(h: &mut TestHarness, id: WinID) -> Origin {
    os_frame(h, id).min + IVec2::new(200, 10)
}

fn focused(h: &mut TestHarness) -> WinID {
    let world = h.world();
    let mut windows = world.query_filtered::<&Window, With<crate::ecs::FocusedMarker>>();
    windows.single(world).expect("a focused window").id()
}

fn past_both(h: &mut TestHarness) -> Origin {
    h.mock_state.focus_window(1);
    h.advance(SETTLE);
    assert_eq!(focused(h), 1);
    let start = title_bar(h, 0);
    press(h, start);
    h.mock_state.focus_window(0);
    let at = drag(h, 0, start, &[100; 7]);
    h.advance(SETTLE);
    assert_eq!(order(h), [1, 2, 0]);
    assert_eq!(focused(h), 0);
    at
}

#[test]
fn a_press_on_no_window_holds_nothing() {
    let mut h = three();
    press(&mut h, Origin::new(500, 5));
    assert!(!held(&mut h));
}

#[test]
fn a_press_on_a_floating_window_holds_nothing() {
    let mut h = three();
    let world = h.world();
    let entity = find_window_entity(2, world);
    let mut strips = world.query_filtered::<&mut LayoutStrip, With<ActiveWorkspaceMarker>>();
    strips
        .single_mut(world)
        .expect("one active strip")
        .remove(entity);
    world.entity_mut(entity).insert(Unmanaged::Floating);
    h.advance(SETTLE);
    // Paneru centres it, over window 1: press where only it is.
    let frame = os_frame(&mut h, 2);
    let at = Origin::new(frame.max.x - 20, frame.min.y + 10);
    assert!(at.x > os_frame(&mut h, 1).max.x);
    press(&mut h, at);
    assert!(!held(&mut h));
}

#[test]
fn a_right_click_holds_nothing() {
    let mut h = three();
    let at = title_bar(&mut h, 1);
    button_down(&mut h, at, MouseButton::Right);
    assert!(!held(&mut h));
    button_up(&mut h, at, MouseButton::Right);
    assert!(!held(&mut h));
}

#[test]
fn the_right_button_leaves_a_drag_alone() {
    let mut h = three();
    let start = title_bar(&mut h, 0);
    press(&mut h, start);
    let at = drag(&mut h, 0, start, &[50; 5]);
    h.advance(SETTLE);
    assert_eq!(order(&mut h), [1, 0, 2]);
    let elsewhere = title_bar(&mut h, 2);
    button_down(&mut h, elsewhere, MouseButton::Right);
    button_up(&mut h, elsewhere, MouseButton::Right);
    h.advance(SETTLE);
    // Still held where the pointer has it, in the slot it took.
    assert!(dragging(&mut h));
    assert_eq!(order(&mut h), [1, 0, 2]);
    assert_eq!(x(&mut h, 0), 250);
    release(&mut h, at);
    h.advance(SETTLE);
    assert!(!held(&mut h));
    assert_eq!(x(&mut h, 0), 400);
}

#[test]
fn a_click_reorders_nothing() {
    let mut h = three();
    let at = title_bar(&mut h, 0);
    press(&mut h, at);
    release(&mut h, at);
    h.advance(SETTLE);
    assert_eq!(order(&mut h), [0, 1, 2]);
    assert_eq!(x(&mut h, 0), 0);
}

#[test]
fn an_app_moving_its_own_window_is_no_drag() {
    let mut h = three();
    let at = title_bar(&mut h, 0);
    press(&mut h, at);
    for _ in 0..10 {
        let frame = os_frame(&mut h, 0);
        h.mock_state
            .os_move_window(0, frame.min + IVec2::new(50, 0));
        h.advance(Duration::from_millis(20));
    }
    release(&mut h, at);
    h.advance(SETTLE);
    assert_eq!(order(&mut h), [0, 1, 2]);
}

#[test]
fn an_edge_resize_is_no_drag() {
    let mut h = three();
    let mut at = os_frame(&mut h, 1).min + IVec2::new(1, 300);
    press(&mut h, at);
    for _ in 0..6 {
        at.x += 50;
        h.world().write_message::<Event>(Event::MouseDragged {
            button: MouseButton::Left,
            point: point(at),
            modifiers: Modifiers::empty(),
        });
        let frame = os_frame(&mut h, 1);
        h.mock_state
            .os_move_window(1, frame.min + IVec2::new(50, 0));
        h.mock_state
            .os_resize_window(1, frame.size() - IVec2::new(50, 0));
        h.advance(Duration::from_millis(20));
    }
    release(&mut h, at);
    h.advance(SETTLE);
    assert_eq!(order(&mut h), [0, 1, 2]);
}

#[test]
fn past_a_neighbours_middle_the_column_takes_its_slot() {
    let mut h = three();
    let start = title_bar(&mut h, 0);
    press(&mut h, start);
    // Its left edge at 150 is nearer its own slot at 0 than the next at 400.
    let at = drag(&mut h, 0, start, &[50, 50, 50]);
    h.advance(SETTLE);
    assert_eq!(order(&mut h), [0, 1, 2]);
    // At 250 it is nearer 400: window 1 glides into the slot it left.
    drag(&mut h, 0, at, &[50, 50]);
    h.advance(SETTLE);
    assert_eq!(order(&mut h), [1, 0, 2]);
    assert_eq!(x(&mut h, 1), 0);
    // Paneru leaves the held window where the pointer has it.
    assert_eq!(x(&mut h, 0), 250);
}

#[test]
fn dragged_back_the_column_goes_back() {
    let mut h = three();
    let start = title_bar(&mut h, 0);
    press(&mut h, start);
    let at = drag(&mut h, 0, start, &[50; 10]);
    h.advance(SETTLE);
    assert_eq!(order(&mut h), [1, 0, 2]);
    drag(&mut h, 0, at, &[-50; 10]);
    h.advance(SETTLE);
    assert_eq!(order(&mut h), [0, 1, 2]);
    assert_eq!(x(&mut h, 1), 400);
}

#[test]
fn a_release_lands_the_window_in_its_slot() {
    let mut h = three();
    let start = title_bar(&mut h, 0);
    press(&mut h, start);
    let at = drag(&mut h, 0, start, &[50; 10]);
    release(&mut h, at);
    h.advance(SETTLE);
    assert!(!held(&mut h));
    assert_eq!(order(&mut h), [1, 0, 2]);
    assert_eq!(x(&mut h, 1), 0);
    assert_eq!(x(&mut h, 0), 400);
}

#[test]
fn drag_and_release_in_the_same_frame_keep_the_drop() {
    let mut h = three();
    let start = title_bar(&mut h, 0);
    press(&mut h, start);
    let delta = IVec2::new(250, 0);
    h.world().write_message(Event::MouseDragged {
        button: MouseButton::Left,
        point: point(start + delta),
        modifiers: Modifiers::empty(),
    });
    let frame = os_frame(&mut h, 0);
    h.mock_state.os_move_window(0, frame.min + delta);
    release(&mut h, start + delta);
    h.advance(SETTLE);

    assert!(!held(&mut h));
    assert_eq!(order(&mut h), [1, 0, 2]);
    assert_eq!(x(&mut h, 0), 400);
}

#[test]
fn a_lost_release_ends_the_drag_as_a_release() {
    let mut h = three();
    let start = title_bar(&mut h, 0);
    press(&mut h, start);
    let at = drag(&mut h, 0, start, &[50; 10]);
    send(
        &mut h,
        Event::MouseMoved {
            point: point(at),
            modifiers: Modifiers::empty(),
        },
    );
    h.advance(SETTLE);
    assert!(!held(&mut h));
    assert_eq!(order(&mut h), [1, 0, 2]);
    assert_eq!(x(&mut h, 0), 400);
}

#[test]
fn off_its_display_the_column_goes_back_to_where_it_began() {
    let mut h = three();
    let start = title_bar(&mut h, 0);
    press(&mut h, start);
    let at = drag(&mut h, 0, start, &[50; 10]);
    h.advance(SETTLE);
    assert_eq!(order(&mut h), [1, 0, 2]);
    // The pointer past the display's right edge, at 1024.
    let at = drag(&mut h, 0, at, &[50; 7]);
    assert!(at.x > 1024);
    h.advance(SETTLE);
    assert_eq!(order(&mut h), [0, 1, 2]);
    release(&mut h, at);
    h.advance(SETTLE);
    assert_eq!(order(&mut h), [0, 1, 2]);
    assert_eq!(x(&mut h, 0), 0);
}

#[test]
fn a_window_closed_while_dragged_ends_the_drag() {
    let mut h = three();
    let start = title_bar(&mut h, 0);
    press(&mut h, start);
    drag(&mut h, 0, start, &[50; 10]);
    h.mock_state.os_close_window(0);
    h.advance(SETTLE);
    assert!(!held(&mut h));
    assert_eq!(order(&mut h), [1, 2]);
}

#[test]
fn a_stacked_window_carries_its_column() {
    let mut h = three();
    for command in [
        Command::Window(Operation::Focus(Direction::First)),
        Command::Window(Operation::Focus(Direction::East)),
        Command::Window(Operation::Stack(true)),
    ] {
        send(&mut h, Event::Command { command });
    }
    h.advance(SETTLE);
    assert_eq!(order(&mut h), [0, 2]);
    let lower = os_frame(&mut h, 1);
    assert!(lower.min.y > os_frame(&mut h, 0).min.y, "1 is below 0");
    let start = title_bar(&mut h, 1);
    press(&mut h, start);
    let at = drag(&mut h, 1, start, &[50; 10]);
    h.advance(SETTLE);
    // Window 0, its row above, goes with the column; window 1 stays put.
    assert_eq!(order(&mut h), [2, 0]);
    assert_eq!(x(&mut h, 0), 400);
    assert_eq!(x(&mut h, 1), 500);
    release(&mut h, at);
    h.advance(SETTLE);
    assert_eq!(os_frame(&mut h, 1).min, IVec2::new(400, lower.min.y));
}

#[test]
fn reordering_a_tabbed_column_keeps_its_active_tab() {
    let mut h = three();
    for operation in [
        Operation::Focus(Direction::First),
        Operation::Focus(Direction::East),
        Operation::Stack(true),
        Operation::ToggleTabbedDisplay,
    ] {
        send(
            &mut h,
            Event::Command {
                command: Command::Window(operation),
            },
        );
    }
    h.advance(SETTLE);
    let active = {
        let world = h.world();
        let first = find_window_entity(0, world);
        world
            .query_filtered::<&LayoutStrip, With<ActiveWorkspaceMarker>>()
            .single(world)
            .expect("one active strip")
            .active_tab(first)
            .expect("an active tab")
    };
    let active_id = h.world().get::<Window>(active).expect("active window").id();
    let start = title_bar(&mut h, active_id);
    press(&mut h, start);
    let at = drag(&mut h, active_id, start, &[50; 10]);
    release(&mut h, at);
    h.advance(SETTLE);

    assert_eq!(order(&mut h), [2, 0]);
    let world = h.world();
    let strip = world
        .query_filtered::<&LayoutStrip, With<ActiveWorkspaceMarker>>()
        .single(world)
        .expect("one active strip");
    assert_eq!(strip.active_tab(active), Some(active));
}

#[test]
fn a_relayout_leaves_the_held_window_alone() {
    let mut h = three();
    let start = title_bar(&mut h, 2);
    press(&mut h, start);
    let at = drag(&mut h, 2, start, &[-50, -50]);
    h.advance(SETTLE);
    assert_eq!(order(&mut h), [0, 1, 2]);
    // Window 0 narrowed by 200 lays the strip out again: window 1 follows
    // it, and the held window stays where the pointer has it.
    let size = os_frame(&mut h, 0).size();
    h.mock_state.os_resize_window(0, size - IVec2::new(200, 0));
    h.advance(SETTLE);
    let after_0 = x(&mut h, 0) + 200;
    assert_eq!(x(&mut h, 1), after_0);
    assert_eq!(x(&mut h, 2), 700);
    release(&mut h, at);
    h.advance(SETTLE);
    let after_1 = x(&mut h, 1) + 400;
    assert_eq!(x(&mut h, 2), after_1);
}

#[test]
fn escape_puts_the_column_back_and_leaves_it_focused() {
    let mut h = three_gliding();
    let at = past_both(&mut h);
    send(&mut h, Event::CancelDrag);
    assert!(!held(&mut h));
    // The window server stops the window; the pointer goes on without it.
    send(
        &mut h,
        Event::MouseMoved {
            point: point(at + IVec2::new(-300, 0)),
            modifiers: Modifiers::empty(),
        },
    );
    h.advance(SETTLE);
    assert_eq!(order(&mut h), [0, 1, 2]);
    assert_eq!(x(&mut h, 0), 0);
    assert_eq!(x(&mut h, 2), 800);
    assert_eq!(strip_x(&mut h), 0);
    assert_eq!(focused(&mut h), 0);
}

#[test]
fn escape_during_a_press_lets_it_go() {
    let mut h = three();
    let start = title_bar(&mut h, 1);
    press(&mut h, start);
    send(&mut h, Event::CancelDrag);
    assert!(!held(&mut h));
    assert_eq!(order(&mut h), [0, 1, 2]);
}
fn strip_x(h: &mut TestHarness) -> i32 {
    let world = h.world();
    let mut strips = world.query_filtered::<&Position, With<ActiveWorkspaceMarker>>();
    strips.single(world).expect("one active strip").0.x
}
