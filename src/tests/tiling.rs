use std::sync::{Arc, RwLock};

use bevy::prelude::*;
use tracing::debug;

use crate::commands::{Command, Direction, Operation, ResizeDirection};
use crate::config::{Config, MainOptions, WindowParams};
use crate::events::Event;
use crate::manager::{WindowApi, WindowManager, WindowPadding};
use crate::platform::ProcessSerialNumber;

use super::*;

#[test]
fn test_set_padding_expands_frame() {
    let psn = ProcessSerialNumber { high: 0, low: 0 };
    let app = MockApplication::new(psn, 1);
    let event_queue = Arc::new(RwLock::new(Vec::new()));

    // Window at (100, 50) with size (400, 300).
    let frame = IRect::new(100, 50, 500, 350);
    let mut window = MockWindow::new(1, frame, event_queue, app);

    assert_eq!(window.frame().width(), 400);
    assert_eq!(window.frame().height(), 300);

    // Setting horizontal padding should expand the frame by the padding on each side.
    window.set_padding(WindowPadding::Horizontal(8));
    assert_eq!(
        window.frame().min.x,
        92,
        "min.x should shift left by padding"
    );
    assert_eq!(
        window.frame().max.x,
        508,
        "max.x should shift right by padding"
    );
    assert_eq!(
        window.frame().width(),
        416,
        "width should grow by 2 * padding"
    );

    // Setting vertical padding should expand the frame vertically.
    window.set_padding(WindowPadding::Vertical(5));
    assert_eq!(window.frame().min.y, 45, "min.y should shift up by padding");
    assert_eq!(
        window.frame().max.y,
        355,
        "max.y should shift down by padding"
    );
    assert_eq!(
        window.frame().height(),
        310,
        "height should grow by 2 * padding"
    );

    // Changing padding from 8 to 12 should only expand by the delta (4).
    window.set_padding(WindowPadding::Horizontal(12));
    assert_eq!(window.frame().min.x, 88);
    assert_eq!(window.frame().max.x, 512);
    assert_eq!(window.frame().width(), 424);
}

#[test]
#[allow(clippy::too_many_lines)]
fn test_window_shuffle() {
    const PADDING_LEFT: u16 = 3;
    const PADDING_RIGHT: u16 = 5;
    const PADDING_TOP: u16 = 7;
    const PADDING_BOTTOM: u16 = 9;
    const SLIVER_WIDTH: u16 = 5;
    const H_PAD: i32 = 2;

    let commands = vec![
        Event::MenuOpened { window_id: 0 }, // Noop allowing everything to settle
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::Last)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::First)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::Stack(true)),
        },
        Event::Command {
            command: Command::Window(Operation::Center),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::Stack(true)),
        },
        Event::Command {
            command: Command::Window(Operation::Center),
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    // Logical width includes padding expansion on each side.
    let logical_width = TEST_WINDOW_WIDTH + 2 * H_PAD;
    let top_edge = TEST_MENUBAR_HEIGHT + i32::from(PADDING_TOP);
    let left_edge = i32::from(PADDING_LEFT);
    let right_edge = TEST_DISPLAY_WIDTH - i32::from(PADDING_RIGHT);
    let offscreen_right = right_edge - i32::from(SLIVER_WIDTH) + i32::from(PADDING_RIGHT) - H_PAD;
    let offscreen_left =
        left_edge - logical_width + i32::from(SLIVER_WIDTH) - i32::from(PADDING_LEFT) + H_PAD;
    let centered = (TEST_DISPLAY_WIDTH - logical_width) / 2;

    let expected_positions_last = [
        (4, (offscreen_left, top_edge)),
        (3, (offscreen_left, top_edge)),
        (2, (right_edge - 3 * logical_width, top_edge)),
        (1, (right_edge - 2 * logical_width, top_edge)),
        (0, (right_edge - logical_width, top_edge)),
    ];
    let expected_positions_first = [
        (4, (left_edge, top_edge)),
        (3, (left_edge + logical_width, top_edge)),
        (2, (left_edge + 2 * logical_width, top_edge)),
        (1, (offscreen_right, top_edge)),
        (0, (offscreen_right, top_edge)),
    ];

    let expected_positions_stacked = [
        (4, (centered, top_edge)),
        (3, (centered, 393)),
        (2, (centered + logical_width, top_edge)),
        (1, (offscreen_right, top_edge)),
        (0, (offscreen_right, top_edge)),
    ];
    let expected_positions_stacked2 = [
        (4, (centered, top_edge)),
        (3, (centered, 271)),
        (2, (centered, 515)),
        (1, (centered + logical_width, top_edge)),
        (0, (offscreen_right, top_edge)),
    ];

    let check = |iteration, world: &mut World| {
        let iterations = [
            None,
            Some(expected_positions_last.as_slice()),
            Some(expected_positions_first.as_slice()),
            None,
            None,
            Some(expected_positions_stacked.as_slice()),
            None,
            None,
            None,
            Some(expected_positions_stacked2.as_slice()),
        ];

        if let Some(positions) = iterations[iteration] {
            debug!("Iteration: {iteration}");
            verify_window_positions(positions, world);
        }
    };

    let mut bevy = setup_world();
    let mock_app = setup_process(bevy.world_mut());
    let internal_queue = Arc::new(RwLock::new(Vec::<Event>::new()));
    let event_queue = internal_queue.clone();
    let windows = window_spawner(5, event_queue, mock_app);
    let window_manager = MockWindowManager {
        windows,
        workspaces: vec![TEST_WORKSPACE_ID],
    };
    bevy.world_mut()
        .insert_resource(WindowManager(Box::new(window_manager)));

    let mut params = WindowParams::new(".*", None);
    params.vertical_padding = Some(3);
    params.horizontal_padding = Some(2);
    let config: Config = (
        MainOptions {
            padding_left: Some(PADDING_LEFT),
            padding_right: Some(PADDING_RIGHT),
            padding_top: Some(PADDING_TOP),
            padding_bottom: Some(PADDING_BOTTOM),
            ..Default::default()
        },
        vec![params],
    )
        .into();
    bevy.insert_resource(config);

    run_main_loop(&mut bevy, &internal_queue, &commands, check);
}

#[test]
fn test_startup_windows() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 }, // Noop allowing everything to settle
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::First)),
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    let expected_positions = [
        (4, (0, TEST_MENUBAR_HEIGHT)),
        (3, (400, TEST_MENUBAR_HEIGHT)),
        (2, (800, TEST_MENUBAR_HEIGHT)),
    ];

    let check = |iteration, world: &mut World| {
        let iterations = [None, None, None, None, Some(expected_positions.as_slice())];

        if let Some(positions) = iterations[iteration] {
            debug!("Iteration: {iteration}");
            verify_window_positions(positions, world);
        }
    };

    let mut bevy = setup_world();
    let mock_app = setup_process(bevy.world_mut());
    let internal_queue = Arc::new(RwLock::new(Vec::<Event>::new()));
    let event_queue = internal_queue.clone();
    let windows = window_spawner(5, event_queue, mock_app);
    let window_manager = MockWindowManager {
        windows,
        workspaces: vec![TEST_WORKSPACE_ID],
    };
    bevy.world_mut()
        .insert_resource(WindowManager(Box::new(window_manager)));

    run_main_loop(&mut bevy, &internal_queue, &commands, check);
}

#[test]
fn test_window_resize_grow_and_shrink_cycle() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 }, // Noop allowing everything to settle
        Event::Command {
            command: Command::Window(Operation::Resize(ResizeDirection::Grow)),
        },
        Event::Command {
            command: Command::Window(Operation::Resize(ResizeDirection::Grow)),
        },
        Event::Command {
            command: Command::Window(Operation::Resize(ResizeDirection::Grow)),
        },
        Event::Command {
            command: Command::Window(Operation::Resize(ResizeDirection::Shrink)),
        },
    ];

    let expected_widths = [None, Some(512), Some(768), Some(256), Some(768)];

    let check = |iteration, world: &mut World| {
        let Some(expected_width) = expected_widths[iteration] else {
            return;
        };
        let mut query = world.query::<&Window>();
        let window = query
            .iter(world)
            .find(|window| window.id() == 0)
            .expect("expected window with id 0");
        assert_eq!(
            window.frame().width(),
            expected_width,
            "iteration {iteration}: expected width {expected_width}, got {}",
            window.frame().width()
        );
    };

    let mut bevy = setup_world();
    let mock_app = setup_process(bevy.world_mut());
    let internal_queue = Arc::new(RwLock::new(Vec::<Event>::new()));
    let event_queue = internal_queue.clone();
    let windows = window_spawner(1, event_queue, mock_app);
    let window_manager = MockWindowManager {
        windows,
        workspaces: vec![TEST_WORKSPACE_ID],
    };
    bevy.world_mut()
        .insert_resource(WindowManager(Box::new(window_manager)));

    let config: Config = (
        MainOptions {
            preset_column_widths: vec![0.25, 0.5, 0.75],
            ..Default::default()
        },
        vec![],
    )
        .into();
    bevy.insert_resource(config);

    run_main_loop(&mut bevy, &internal_queue, &commands, check);
}
