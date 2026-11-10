use std::collections::BTreeMap;
use std::time::Duration;

use bevy::prelude::*;

use crate::config::{Config, MainOptions};
use crate::ecs::layout::LayoutStrip;
use crate::ecs::setups::{DisplaySetups, setup_key};
use crate::events::Event;
use crate::manager::{Window, WindowManager};
use crate::platform::{WinID, WorkspaceId};

use super::*;

const RETURNED_EXT_WORKSPACE_ID: WorkspaceId = 31;
const SETTLE: Duration = Duration::from_secs(5);

fn ext_bounds() -> IRect {
    IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0)
}

fn strips_on(world: &mut World, space: WorkspaceId) -> Vec<(u32, Vec<WinID>)> {
    let strips = world
        .query::<&LayoutStrip>()
        .iter(world)
        .filter(|strip| strip.id() == space)
        .map(|strip| (strip.virtual_index, strip.all_windows()))
        .collect::<Vec<_>>();
    strips
        .into_iter()
        .map(|(index, entities)| {
            let ids = entities
                .into_iter()
                .filter_map(|entity| world.get::<Window>(entity).map(|window| window.id()))
                .collect::<Vec<_>>();
            (index, ids)
        })
        .collect::<BTreeMap<_, _>>()
        .into_iter()
        .collect()
}

fn current_key(world: &World) -> String {
    setup_key(world.resource::<WindowManager>()).expect("a key for the connected displays")
}

fn saved(world: &World, key: &str) -> Option<crate::ecs::state::PaneruState> {
    world.resource::<DisplaySetups>().saved(key).cloned()
}

fn unplug(harness: &mut TestHarness) {
    harness.mock_state.remove_display(EXT_DISPLAY_ID);
    harness
        .mock_state
        .update_window(100, |w| w.workspace_id = TEST_WORKSPACE_ID);
    harness
        .mock_state
        .update_window(101, |w| w.workspace_id = TEST_WORKSPACE_ID);
    harness.world().write_message(Event::DisplayRemoved {
        display_id: EXT_DISPLAY_ID,
    });
    harness.advance(SETTLE);
}

#[allow(clippy::too_many_lines)]
#[test]
fn test_each_display_setup_gets_its_own_layout_back() {
    let config: Config = (
        MainOptions {
            remember_display_setups: Some(true),
            ..Default::default()
        },
        vec![],
    )
        .into();
    let mut harness = TestHarness::new()
        .with_config(config)
        .with_display(EXT_DISPLAY_ID, ext_bounds(), vec![EXT_WORKSPACE_ID])
        .with_windows(2)
        .with_workspace_window(100, EXT_WORKSPACE_ID, |_| {})
        .with_workspace_window(101, EXT_WORKSPACE_ID, |_| {});
    harness.run(vec![Event::MenuOpened { window_id: 0 }]);
    harness.advance(SETTLE);

    let dual = current_key(harness.world());
    assert!(
        saved(harness.world(), &dual).is_some(),
        "the dual setup is captured once its layout is quiet"
    );

    unplug(&mut harness);
    let laptop = current_key(harness.world());
    assert_ne!(laptop, dual);
    assert_eq!(
        strips_on(harness.world(), TEST_WORKSPACE_ID),
        vec![(0, vec![0, 1]), (1, vec![100, 101])],
        "with no saved laptop layout, the monitor's windows arrive as the next virtual workspace"
    );

    let world = harness.world();
    let first_row = world
        .query::<(Entity, &LayoutStrip)>()
        .iter(world)
        .find_map(|(entity, strip)| {
            (strip.id() == TEST_WORKSPACE_ID && strip.virtual_index == 0).then_some(entity)
        })
        .expect("the laptop's first row");
    world
        .get_mut::<LayoutStrip>(first_row)
        .expect("the laptop's first row")
        .swap(0, 1);
    harness.advance(SETTLE);
    let laptop_layout = saved(harness.world(), &laptop).expect("the laptop setup is captured");

    harness.mock_state.add_display(
        EXT_DISPLAY_ID,
        ext_bounds(),
        vec![RETURNED_EXT_WORKSPACE_ID],
    );
    harness.world().write_message(Event::DisplayAdded {
        display_id: EXT_DISPLAY_ID,
    });
    harness.advance(Duration::from_secs(2));
    // The mock does not move a window to the space under its frame. macOS does.
    for window_id in [100, 101] {
        harness
            .mock_state
            .update_window(window_id, |w| w.workspace_id = RETURNED_EXT_WORKSPACE_ID);
    }
    harness.advance(SETTLE);

    let world = harness.world();
    assert_eq!(
        strips_on(world, RETURNED_EXT_WORKSPACE_ID),
        vec![(0, vec![100, 101])],
        "the monitor's windows return to it, on its new space, in their saved order"
    );
    assert_eq!(
        strips_on(world, TEST_WORKSPACE_ID).first(),
        Some(&(0, vec![0, 1])),
        "the laptop's first row returns to its dual-setup order"
    );
    for window_id in [100, 101] {
        let entity = find_window_entity(window_id, world);
        let center = world
            .get::<Window>(entity)
            .expect("window")
            .frame()
            .center();
        assert!(
            ext_bounds().contains(center),
            "window {window_id} at {center:?} must be on the monitor"
        );
    }
    assert_eq!(
        saved(world, &laptop),
        Some(laptop_layout),
        "a capture on the dual setup never writes the laptop layout"
    );
    let ext_parked = world
        .query::<(
            &LayoutStrip,
            Option<&crate::ecs::workspace::PreviousStripPosition>,
        )>()
        .iter(world)
        .any(|(strip, prev)| strip.id() == RETURNED_EXT_WORKSPACE_ID && prev.is_some());
    assert!(
        !ext_parked,
        "the secondary display's active strip must not be marked as parked"
    );

    unplug(&mut harness);
    assert_eq!(
        strips_on(harness.world(), TEST_WORKSPACE_ID).first(),
        Some(&(0, vec![1, 0])),
        "the laptop's own order returns, not the order of the migration"
    );
    let world = harness.world();
    let row1_pos = world
        .query::<(&LayoutStrip, &crate::ecs::Position)>()
        .iter(world)
        .find_map(|(strip, pos)| {
            (strip.id() == TEST_WORKSPACE_ID && strip.virtual_index == 1).then_some(pos.0)
        })
        .expect("restored parked row 1 on the laptop");
    assert_eq!(
        row1_pos.y,
        TEST_DISPLAY_HEIGHT - crate::ecs::layout::PARKED_STRIP_SLIVER,
        "parked row 1 must stay parked off-screen after width restoration"
    );
}
