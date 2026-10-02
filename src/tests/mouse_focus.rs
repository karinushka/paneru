use std::time::Duration;

use crate::assert_focused;
use crate::commands::{Command, Direction, Operation};
use crate::config::{Config, MainOptions};
use crate::events::Event;
use crate::manager::Window;

use super::*;

#[test]
fn keyboard_focus_warps_mouse_to_centered_window() {
    for speed in [12.0, 30.0] {
        let config: Config = (
            MainOptions {
                auto_center: Some(true),
                mouse_follows_focus: Some(true),
                focus_follows_mouse: Some(false),
                animation_speed: Some(speed),
                ..Default::default()
            },
            vec![],
        )
            .into();
        let mut h = TestHarness::new().with_config(config).with_windows(3);
        h.advance(Duration::from_secs(1));

        // Exercise both directions and a window initially outside the viewport.
        for (direction, window_id) in [
            (Direction::First, 0),
            (Direction::Last, 2),
            (Direction::West, 1),
            (Direction::East, 2),
        ] {
            h.app.world_mut().write_message(Event::Command {
                command: Command::Window(Operation::Focus(direction)),
            });
            h.advance(Duration::from_secs(1));

            let world = h.app.world_mut();
            assert_focused!(world, window_id);
            let window = world
                .query::<&Window>()
                .iter(world)
                .find(|window| window.id() == window_id)
                .expect("focused window");
            let center = window.frame().center();
            assert!(
                (center.x - TEST_DISPLAY_WIDTH / 2).abs() <= 2,
                "window should have finished centering: {center:?}"
            );
            let cursor = h.mock_state.cursor_position();
            assert!(
                (cursor.x - center.x).abs() <= 2 && window.frame().contains(cursor),
                "speed {speed}: pointer {cursor:?} was left behind after window {window_id} centered at {center:?}"
            );
        }
    }
}

#[test]
fn keyboard_focus_does_not_warp_mouse_when_disabled() {
    let config: Config = (
        MainOptions {
            auto_center: Some(true),
            mouse_follows_focus: Some(false),
            focus_follows_mouse: Some(false),
            animation_speed: Some(30.0),
            ..Default::default()
        },
        vec![],
    )
        .into();
    let mut h = TestHarness::new().with_config(config).with_windows(3);
    h.advance(Duration::from_secs(1));
    let cursor = h.mock_state.cursor_position();
    h.app.world_mut().write_message(Event::Command {
        command: Command::Window(Operation::Focus(Direction::Last)),
    });
    h.advance(Duration::from_secs(1));
    assert_focused!(h.app.world_mut(), 2);
    assert_eq!(h.mock_state.cursor_position(), cursor);
}
