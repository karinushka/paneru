//! Tests for `Operation::FocusOrVirtual` (bound as e.g.
//! `window_virtualfocus_north`/`_south`): focuses a stack neighbor
//! above/below if one exists, otherwise switches the virtual workspace like
//! `Operation::Virtual` would — see `switch_virtual_workspace_bind` in
//! `src/ecs/workspace.rs`. Deliberately a separate command from
//! `Operation::Virtual` (`window_virtual_north`/`_south`), which must keep
//! meaning exactly "switch workspace", unconditionally.

use bevy::prelude::*;

use crate::assert_focused;
use crate::commands::{Command, Direction, Operation};
use crate::ecs::ActiveWorkspaceMarker;
use crate::ecs::layout::LayoutStrip;
use crate::events::Event;

use super::*;

fn active_virtual_index(world: &mut World) -> u32 {
    let mut query = world.query_filtered::<&LayoutStrip, With<ActiveWorkspaceMarker>>();
    query
        .single(world)
        .expect("exactly one active strip")
        .virtual_index
}

#[test]
fn test_focus_or_virtual_south_focuses_stack_sibling_before_switching_workspace() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::Stack(true)),
        },
        // Jump to VW1 and straight back so it exists for the eventual
        // fallback switch, without relying on auto-create being enabled.
        Event::Command {
            command: Command::Window(Operation::VirtualNumber(1)),
        },
        Event::Command {
            command: Command::Window(Operation::VirtualNumber(0)),
        },
        // Focus window 0 (top of the 2-item stack), then press South twice.
        // The column jump returns to window 1, which had focus last, so step
        // up to window 0.
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::First)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::North)),
        },
        Event::Command {
            command: Command::Window(Operation::FocusOrVirtual(Direction::South)),
        },
        Event::Command {
            command: Command::Window(Operation::FocusOrVirtual(Direction::South)),
        },
    ];

    TestHarness::new()
        .with_windows(2)
        .on_iteration(7, |world, _state| {
            // First South: a sibling exists below window 0 in the stack, so
            // focus must move to it instead of switching virtual workspace.
            assert_focused!(world, 1);
            assert_eq!(
                active_virtual_index(world),
                0,
                "focusing a stack sibling must not switch virtual workspace"
            );
        })
        .on_iteration(8, |world, _state| {
            // Second South: window 1 is the last stack item, nothing left to
            // focus, so this must fall through to switching workspaces.
            assert_eq!(
                active_virtual_index(world),
                1,
                "with nothing left to focus, South must fall through to switching workspace"
            );
        })
        .run(commands);
}

#[test]
fn test_focus_or_virtual_north_switches_workspace_immediately_without_a_stack() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::VirtualNumber(1)),
        },
        Event::Command {
            command: Command::Window(Operation::FocusOrVirtual(Direction::North)),
        },
    ];

    TestHarness::new()
        .with_windows(1)
        .on_iteration(2, |world, _state| {
            assert_eq!(
                active_virtual_index(world),
                0,
                "with no stack sibling to focus, North must switch virtual workspace"
            );
        })
        .run(commands);
}

/// Regression: `Operation::Virtual` (`window_virtual_north`/`_south`) must
/// keep switching the virtual workspace unconditionally, even when the
/// focused window is in a stack with a focusable neighbor — that
/// focus-or-switch behavior belongs only to the separate
/// `Operation::FocusOrVirtual` command.
#[test]
fn test_plain_virtual_south_switches_workspace_even_with_a_stack_sibling() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::Stack(true)),
        },
        Event::Command {
            command: Command::Window(Operation::VirtualNumber(1)),
        },
        Event::Command {
            command: Command::Window(Operation::VirtualNumber(0)),
        },
        // The column jump returns to window 1, which had focus last, so step
        // up to window 0.
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::First)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::North)),
        },
        Event::Command {
            command: Command::Window(Operation::Virtual(Direction::South)),
        },
    ];

    TestHarness::new()
        .with_windows(2)
        .on_iteration(7, |world, _state| {
            assert_focused!(world, 0);
            assert_eq!(
                active_virtual_index(world),
                1,
                "plain Virtual(South) must switch workspace unconditionally, \
                 not focus the stack sibling"
            );
        })
        .run(commands);
}
