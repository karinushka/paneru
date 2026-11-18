use std::cell::RefCell;
use std::rc::Rc;

use bevy::ecs::message::Messages;
use bevy::ecs::query::Has;
use bevy::ecs::system::RunSystemOnce as _;
use bevy::ecs::world::World;
use bevy::math::IRect;

use super::*;
use crate::commands::{MouseMove, MoveFocus, Operation};
use crate::manager::{Origin, Size};
use crate::tests::{
    EXT_DISPLAY_HEIGHT, EXT_DISPLAY_ID, EXT_DISPLAY_WIDTH, EXT_WORKSPACE_ID, TEST_DISPLAY_WIDTH,
    TEST_MENUBAR_HEIGHT, TEST_PROCESS_ID, TEST_WINDOW_HEIGHT, TEST_WINDOW_WIDTH, TEST_WORKSPACE_ID,
    TestHarness,
};

fn tick() -> Event {
    Event::Command {
        command: Command::PrintState,
    }
}

fn laptop_rows(world: &mut World) -> Vec<(u32, usize, bool)> {
    let mut strips = world.query::<(&LayoutStrip, Has<SelectedVirtualMarker>)>();
    let mut rows: Vec<_> = strips
        .iter(world)
        .filter(|(strip, _)| strip.id() == TEST_WORKSPACE_ID)
        .map(|(strip, selected)| (strip.virtual_index, strip.len(), selected))
        .collect();
    rows.sort_unstable();
    rows
}

fn window_origin(world: &mut World, id: i32) -> Option<Origin> {
    let mut windows = world.query::<(&Window, &Position)>();
    windows
        .iter(world)
        .find(|(window, _)| window.id() == id)
        .map(|(_, position)| position.0)
}

fn focused_ids(world: &mut World) -> Vec<i32> {
    let mut focus = world.query_filtered::<&Window, With<FocusedMarker>>();
    focus.iter(world).map(|window| window.id()).collect()
}

/// Laptop (3 virtual rows, windows 100 & 101) + external monitor (window 200).
fn dual_display_row_harness() -> TestHarness {
    let config = Config::try_from("default_workspaces = 3\n").expect("config parses");
    TestHarness::new()
        .with_config(config)
        .with_display(
            EXT_DISPLAY_ID,
            IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
            vec![EXT_WORKSPACE_ID],
        )
        .with_workspace_window(100, TEST_WORKSPACE_ID, |_| {})
        .with_workspace_window(101, TEST_WORKSPACE_ID, |_| {})
        .with_workspace_window(200, EXT_WORKSPACE_ID, |data| {
            data.frame = IRect::new(
                0,
                -EXT_DISPLAY_HEIGHT,
                TEST_WINDOW_WIDTH,
                -EXT_DISPLAY_HEIGHT + TEST_WINDOW_HEIGHT,
            );
        })
}

fn scroll_laptop_to_row(entries: &mut [DisplayEntry], virtual_index: u32) {
    let entry = entries
        .iter_mut()
        .find(|entry| entry.cg_bounds.y >= 0.0)
        .expect("laptop joins");
    let target = entry
        .layout
        .strips
        .iter()
        .position(|strip| strip.virtual_index == virtual_index)
        .expect("target virtual row exists");
    while middle_row(&entry.layout) != Some(target) {
        assert!(
            entry.layout.scroll_by(10.0),
            "scrolls to row {virtual_index}"
        );
    }
}

fn assert_laptop_switched_to_second_row(world: &mut World) {
    let after = laptop_rows(world);
    let selected: Vec<_> = after.iter().filter(|row| row.2).map(|row| row.0).collect();
    assert_eq!(selected, [1], "the second row is shown: {after:?}");
    let shown = window_origin(world, 100).expect("window 100");
    assert!(
        (0..TEST_DISPLAY_WIDTH).contains(&shown.x) && shown.y == TEST_MENUBAR_HEIGHT,
        "100 is on the laptop screen: {shown}"
    );
    let hidden = window_origin(world, 101).expect("window 101");
    assert!(
        hidden.x >= TEST_DISPLAY_WIDTH - PARKED_STRIP_SLIVER,
        "101 stays parked: {hidden}"
    );
}

#[test]
fn a_pan_on_a_display_without_focus_is_kept_after_closing() {
    let mut harness = TestHarness::new();
    harness.mock_state.add_display(
        EXT_DISPLAY_ID,
        IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
        vec![EXT_WORKSPACE_ID],
    );
    let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
    let origin = Origin::new(0, 0);
    harness.mock_state.spawn_window(
        TEST_PROCESS_ID,
        TEST_WORKSPACE_ID,
        100,
        IRect::from_corners(origin, origin + size),
    );
    // Six 400pt columns on a 1920pt display: the strip can scroll.
    for i in 0..6 {
        let origin = Origin::new(
            i * TEST_WINDOW_WIDTH,
            -EXT_DISPLAY_HEIGHT + TEST_MENUBAR_HEIGHT,
        );
        harness.mock_state.spawn_window(
            TEST_PROCESS_ID,
            EXT_WORKSPACE_ID,
            200 + i,
            IRect::from_corners(origin, origin + size),
        );
    }
    let ext_x = |world: &mut World| {
        let mut windows = world.query::<(&Window, &Position)>();
        let mut xs: Vec<_> = windows
            .iter(world)
            .filter(|(window, _)| window.id() >= 200)
            .map(|(window, position)| (window.id(), position.0.x))
            .collect();
        xs.sort_unstable();
        xs
    };
    let before = Rc::new(RefCell::new((Vec::new(), 0)));
    let (record, check) = (before.clone(), before);
    harness
        .on_iteration(0, move |world, _| {
            let xs = ext_x(world);
            let dx = world
                .run_system_once(|scene: Scene, windows: Windows, mut commands: Commands| {
                    let mut entries = gather(&scene, None);
                    let entry = entries
                        .iter_mut()
                        .find(|entry| entry.cg_bounds.y < 0.0)
                        .expect("external display joins");
                    let row = entry
                        .layout
                        .strips
                        .iter()
                        .position(|strip| strip.active)
                        .expect("a shown row");
                    entry.layout.pan_by(row, -100_000.0);
                    #[allow(clippy::cast_possible_truncation)]
                    let dx = entry.layout.pan_points(row).round() as i32;
                    write_back(&entries, &scene.strips, &windows, &mut commands, None);
                    dx
                })
                .expect("commit runs");
            assert!(dx < 0, "the strip can pan: {dx}");
            *record.borrow_mut() = (xs, dx);
        })
        .on_iteration(5, move |world, _| {
            let (before, dx) = check.borrow().clone();
            let after = ext_x(world);
            let on_screen = |x: i32| (0..=EXT_DISPLAY_WIDTH - TEST_WINDOW_WIDTH).contains(&x);
            let kept: Vec<_> = before
                .iter()
                .zip(&after)
                .filter(|((_, x), (_, y))| on_screen(*x) && on_screen(*y))
                .map(|(&(id, x), &(_, y))| (id, y - x))
                .collect();
            assert!(kept.len() >= 2, "{before:?} -> {after:?}");
            assert!(
                kept.iter().all(|&(_, moved)| moved == dx),
                "panned by {dx} and stays there: {before:?} -> {after:?}"
            );
        })
        .run(vec![tick(); 6]);
}

#[test]
fn a_pan_on_the_focused_display_is_kept_and_focus_follows_it() {
    let harness = TestHarness::new();
    let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
    // Five 400pt columns on a 1024pt display.
    for i in 0..5 {
        let origin = Origin::new(i * TEST_WINDOW_WIDTH, TEST_MENUBAR_HEIGHT);
        harness.mock_state.spawn_window(
            TEST_PROCESS_ID,
            TEST_WORKSPACE_ID,
            100 + i,
            IRect::from_corners(origin, origin + size),
        );
    }
    let strip_x = |world: &mut World| {
        let mut strips = world.query_filtered::<&Position, With<ActiveWorkspaceMarker>>();
        strips.single(world).map(|position| position.0.x).ok()
    };
    let focused = |world: &mut World| {
        let mut windows =
            world.query_filtered::<(&Window, &Position, &Bounds), With<FocusedMarker>>();
        windows
            .single(world)
            .map(|(window, position, bounds)| (window.id(), position.0.x, bounds.0.x))
            .ok()
    };
    let before = Rc::new(RefCell::new((None, 0, None)));
    let (record, check) = (before.clone(), before);
    harness
        .on_iteration(0, move |world, _| {
            let origin = strip_x(world);
            let was_focused = focused(world);
            let dx = world
                .run_system_once(|scene: Scene, windows: Windows, mut commands: Commands| {
                    let mut entries = gather(&scene, None);
                    let entry = entries.first_mut().expect("the display joins");
                    let row = entry
                        .layout
                        .strips
                        .iter()
                        .position(|strip| strip.active)
                        .expect("a shown row");
                    let (lo, hi) = entry.layout.pan_range(row);
                    let pan = if lo.abs() > hi.abs() { lo } else { hi };
                    entry.layout.pan_by(row, pan);
                    #[allow(clippy::cast_possible_truncation)]
                    let dx = entry.layout.pan_points(row).round() as i32;
                    write_back(&entries, &scene.strips, &windows, &mut commands, None);
                    dx
                })
                .expect("commit runs");
            assert!(dx != 0, "the strip can pan");
            *record.borrow_mut() = (origin, dx, was_focused);
        })
        .on_iteration(5, move |world, _| {
            let (origin, dx, was_focused) = *check.borrow();
            let origin = origin.expect("a shown strip");
            assert_eq!(
                strip_x(world),
                Some(origin + dx),
                "panned by {dx} from {origin} and stays there (focus was {was_focused:?})"
            );
            let (id, x, width) = focused(world).expect("a focused window");
            assert!(
                x >= 0 && x + width <= TEST_DISPLAY_WIDTH,
                "focus {id} is on screen at {x} (was {was_focused:?})"
            );
        })
        .run(vec![tick(); 6]);
}

#[test]
fn clicking_another_display_keeps_the_row_scrolled_to_on_the_focused_one() {
    let commands = vec![
        Event::MenuOpened { window_id: 100 },
        Event::Command {
            command: Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Stay)),
        },
        tick(),
        tick(),
        tick(),
        tick(),
        tick(),
    ];
    dual_display_row_harness()
        .on_iteration(3, |world, _| {
            let before = laptop_rows(world);
            assert_eq!(focused_ids(world), [101], "laptop focused: {before:?}");
            assert!(
                before.contains(&(1, 1, false)),
                "100 sits on the hidden second row: {before:?}"
            );
            world
                .run_system_once(|scene: Scene, windows: Windows, mut commands: Commands| {
                    let mut entries = gather(&scene, None);
                    scroll_laptop_to_row(&mut entries, 1);
                    let external = entries
                        .iter()
                        .position(|entry| entry.cg_bounds.y < 0.0)
                        .expect("external joins");
                    let key = entries[external]
                        .layout
                        .strips
                        .iter()
                        .flat_map(|strip| &strip.windows)
                        .find(|w| w.window_id == 200)
                        .map(|w| w.key)
                        .expect("window 200 has a card");
                    let pick = Pick {
                        display: external,
                        virtual_index: 0,
                        focus: Some(key),
                    };
                    write_back(
                        &entries,
                        &scene.strips,
                        &windows,
                        &mut commands,
                        Some(&pick),
                    );
                    commands.focus_entity(key, true);
                })
                .expect("commit runs");
        })
        .on_iteration(6, |world, _| {
            assert_laptop_switched_to_second_row(world);
            assert_eq!(focused_ids(world), [200], "the clicked window has focus");
            let mut active = world.query_filtered::<&LayoutStrip, With<ActiveWorkspaceMarker>>();
            let active: Vec<_> = active.iter(world).map(LayoutStrip::id).collect();
            assert_eq!(
                active,
                [EXT_WORKSPACE_ID],
                "one active row, the clicked display's"
            );
        })
        .run(commands);
}

#[test]
fn scrolling_the_focused_display_to_another_row_switches_to_it_on_close() {
    let commands = vec![
        Event::MenuOpened { window_id: 100 },
        Event::Command {
            command: Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Stay)),
        },
        tick(),
        tick(),
        tick(),
        tick(),
        tick(),
    ];
    dual_display_row_harness()
        .on_iteration(3, |world, _| {
            let before = laptop_rows(world);
            assert_eq!(focused_ids(world), [101], "laptop focused: {before:?}");
            assert!(
                before.contains(&(1, 1, false)),
                "100 sits on the hidden second row: {before:?}"
            );
            world
                .run_system_once(|scene: Scene, windows: Windows, mut commands: Commands| {
                    let mut entries = gather(&scene, None);
                    scroll_laptop_to_row(&mut entries, 1);
                    write_back(&entries, &scene.strips, &windows, &mut commands, None);
                })
                .expect("commit runs");
        })
        .on_iteration(6, |world, _| {
            assert_laptop_switched_to_second_row(world);
            assert_eq!(focused_ids(world), [100], "focus moves into the row shown");
            let mut active = world.query_filtered::<&LayoutStrip, With<ActiveWorkspaceMarker>>();
            let active: Vec<_> = active.iter(world).map(|s| s.virtual_index).collect();
            assert_eq!(active, [1], "the second row is the active one");
        })
        .run(commands);
}

#[test]
fn scrolling_a_display_without_focus_to_another_row_shows_it_on_close() {
    let commands = vec![
        Event::MenuOpened { window_id: 100 },
        Event::Command {
            command: Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Stay)),
        },
        Event::Command {
            command: Command::Mouse(MouseMove::ToNextDisplay),
        },
        tick(),
        tick(),
        tick(),
        tick(),
    ];
    dual_display_row_harness()
        .on_iteration(3, |world, _| {
            let before = laptop_rows(world);
            assert_eq!(focused_ids(world), [200], "external focused: {before:?}");
            assert!(
                before.contains(&(1, 1, false)),
                "100 sits on the hidden second row: {before:?}"
            );
            // Simulate a secondary display whose shown row still holds a parked origin.
            let mut displays = world.query::<&Display>();
            let parked = displays
                .iter(world)
                .find(|display| display.bounds().min.y >= 0)
                .map(|display| display.bounds().max - PARKED_STRIP_SLIVER)
                .expect("laptop");
            let mut strips = world.query::<(&LayoutStrip, &mut Position)>();
            for (strip, mut position) in strips.iter_mut(world) {
                if strip.id() == TEST_WORKSPACE_ID && strip.virtual_index == 0 {
                    position.0 = parked;
                }
            }
            world
                .run_system_once(|scene: Scene, windows: Windows, mut commands: Commands| {
                    let mut entries = gather(&scene, None);
                    scroll_laptop_to_row(&mut entries, 1);
                    write_back(&entries, &scene.strips, &windows, &mut commands, None);
                })
                .expect("commit runs");
        })
        .on_iteration(6, |world, _| {
            assert_laptop_switched_to_second_row(world);
            assert_eq!(
                focused_ids(world),
                [200],
                "focus stays on the external display"
            );
            let mut previous = world.query::<(&LayoutStrip, &PreviousStripPosition)>();
            let remembered: Vec<_> = previous
                .iter(world)
                .filter(|(strip, _)| strip.id() == TEST_WORKSPACE_ID && strip.virtual_index == 0)
                .map(|(_, previous)| previous.origin)
                .collect();
            assert_eq!(
                remembered.len(),
                1,
                "the first row is hidden: {remembered:?}"
            );
            assert!(
                remembered[0].x <= 0 && remembered[0].y < TEST_WINDOW_HEIGHT,
                "remembered on screen, not parked: {remembered:?}"
            );
        })
        .run(commands);
}

fn two_display_window_harness() -> TestHarness {
    let mut harness = TestHarness::new();
    harness.mock_state.add_display(
        EXT_DISPLAY_ID,
        IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
        vec![EXT_WORKSPACE_ID],
    );
    let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
    let origin = Origin::new(0, 0);
    let ext_origin = Origin::new(0, -EXT_DISPLAY_HEIGHT + TEST_MENUBAR_HEIGHT);
    harness.mock_state.spawn_window(
        TEST_PROCESS_ID,
        TEST_WORKSPACE_ID,
        100,
        IRect::from_corners(origin, origin + size),
    );
    harness.mock_state.spawn_window(
        TEST_PROCESS_ID,
        EXT_WORKSPACE_ID,
        200,
        IRect::from_corners(ext_origin, ext_origin + size),
    );
    harness
}

#[test]
fn shown_row_with_a_parked_origin_is_placed_where_its_windows_are() {
    two_display_window_harness()
        .on_iteration(0, |world, _| {
            let mut strips = world.query::<(&LayoutStrip, &ChildOf, &mut Position)>();
            let mut displays = world.query::<&Display>();
            let parks: Vec<_> = displays
                .iter(world)
                .filter(|display| display.id() == EXT_DISPLAY_ID)
                .map(|display| display.bounds().max - PARKED_STRIP_SLIVER)
                .collect();
            let [park] = parks[..] else {
                panic!("external display: {parks:?}")
            };
            for (strip, _, mut position) in strips.iter_mut(world) {
                if strip.id() == EXT_WORKSPACE_ID {
                    position.0 = park;
                }
            }
            let frames = world
                .run_system_once(|scene: Scene| {
                    gather(&scene, None)
                        .iter()
                        .filter(|entry| entry.cg_bounds.y < 0.0)
                        .flat_map(|entry| {
                            entry
                                .layout
                                .strips
                                .iter()
                                .filter(|strip| strip.active)
                                .flat_map(|strip| strip.windows.iter().map(|w| w.layout))
                                .collect::<Vec<_>>()
                        })
                        .collect::<Vec<_>>()
                })
                .expect("scene system runs");
            let [frame] = frames[..] else {
                panic!("one window on the external row: {frames:?}")
            };
            assert_eq!(
                (frame.x, frame.y),
                (0.0, f64::from(TEST_MENUBAR_HEIGHT)),
                "placed on screen, not a screen off"
            );
        })
        .run(vec![tick()]);
}

#[test]
fn every_display_joins_the_overview() {
    two_display_window_harness()
        .on_iteration(0, |world, _| {
            let shown = world
                .run_system_once(|scene: Scene| {
                    gather(&scene, None)
                        .iter()
                        .map(|entry| {
                            let active: Vec<_> = entry
                                .layout
                                .strips
                                .iter()
                                .filter(|strip| strip.active)
                                .flat_map(|strip| strip.windows.iter().map(|w| w.window_id))
                                .collect();
                            (entry.cg_bounds.y < 0.0, active)
                        })
                        .collect::<Vec<_>>()
                })
                .expect("scene system runs");
            assert_eq!(shown.len(), 2, "both displays join: {shown:?}");
            for (external, windows) in shown {
                let expected = if external { 200 } else { 100 };
                assert_eq!(windows, [expected], "external display: {external}");
            }
        })
        .run(vec![tick()]);
}

fn scrubbing(last_sample: Option<Instant>) -> Scrub {
    Scrub {
        progress: 0.0,
        raw: 0.0,
        velocity: 0.0,
        last_sample,
        prebuilt_down_travel: None,
    }
}

#[test]
fn a_quiet_scrub_times_out() {
    let now = Instant::now();
    let fresh = scrubbing(Some(now));
    assert!(!fresh.timed_out(now + Duration::from_millis(100)));
    assert!(fresh.timed_out(now + SCRUB_TIMEOUT + Duration::from_millis(1)));
    // A scrub that never got a sample (opened by a grab) is released too.
    assert!(scrubbing(None).timed_out(now));
}

fn prebuilt() -> Scrub {
    Scrub {
        prebuilt_down_travel: Some(0.0),
        ..scrubbing(Some(Instant::now()))
    }
}

#[test]
fn a_swipe_down_cancels_an_overview_built_on_touch_down() {
    let mut scrub = prebuilt();
    // A little jitter downwards is ignored, not cancelled.
    assert_eq!(scrub.downward_cancel(-0.01), Some(false));
    assert_eq!(scrub.downward_cancel(-0.015), Some(true));
}

#[test]
fn once_swiped_up_a_swipe_down_scrubs_instead() {
    let mut scrub = prebuilt();
    assert_eq!(scrub.downward_cancel(-0.01), Some(false));
    assert_eq!(scrub.downward_cancel(0.01), None);
    assert_eq!(scrub.downward_cancel(-0.5), None);
    // Nor does it apply to an overview opened any other way.
    assert_eq!(scrubbing(None).downward_cancel(-0.5), None);
}

#[test]
fn tick_condition_stays_idle_during_gpu_springs_and_settled_open() {
    let mut world = World::new();
    for (phase, expected) in [
        (OverviewPhase::Closed, false),
        (OverviewPhase::Opening, false),
        (OverviewPhase::Open, false),
        (OverviewPhase::Closing, false),
        (OverviewPhase::Scrubbing, true),
    ] {
        world.insert_resource(phase);
        assert_eq!(
            world
                .run_system_once(overview_tick_needed)
                .expect("run condition evaluates"),
            expected,
            "phase: {phase:?}"
        );
    }
}

#[test]
fn session_lifecycle_and_drop_own_overview_open_flag() {
    let _guard = crate::overview::OVERVIEW_OPEN_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut phase = OverviewPhase::default();
    {
        let mut session = OverviewSession::default();
        assert!(!OVERVIEW_OPEN.load(Ordering::Relaxed));
        assert!(!crate::overview::OVERVIEW_ACCEPTS_KEYS.load(Ordering::Relaxed));

        // Opening / Open accepts Escape and Return in the event tap.
        session.activate(&mut phase, OverviewPhase::Open, Vec::new(), Motion::Settled);
        assert_eq!(phase, OverviewPhase::Open);
        assert!(OVERVIEW_OPEN.load(Ordering::Relaxed));
        assert!(crate::overview::OVERVIEW_ACCEPTS_KEYS.load(Ordering::Relaxed));

        // Touch-down prebuild at progress 0 owns scroll (`OVERVIEW_OPEN`), but
        // must NOT hijack Escape / Return until swiped up.
        session.activate(
            &mut phase,
            OverviewPhase::Scrubbing,
            Vec::new(),
            Motion::Scrub(prebuilt()),
        );
        assert!(OVERVIEW_OPEN.load(Ordering::Relaxed));
        assert!(!crate::overview::OVERVIEW_ACCEPTS_KEYS.load(Ordering::Relaxed));

        // Active scrub past touch-down accepts Escape / Return.
        session.activate(
            &mut phase,
            OverviewPhase::Scrubbing,
            Vec::new(),
            Motion::Scrub(scrubbing(Some(Instant::now()))),
        );
        assert!(OVERVIEW_OPEN.load(Ordering::Relaxed));
        assert!(crate::overview::OVERVIEW_ACCEPTS_KEYS.load(Ordering::Relaxed));

        // While closing, scroll stays captured (`OVERVIEW_OPEN`), while Escape
        // and Return pass straight through to the newly focused app.
        phase = OverviewPhase::Closing;
        sync_tap_flags(phase, session.0.as_ref());
        assert!(OVERVIEW_OPEN.load(Ordering::Relaxed));
        assert!(!crate::overview::OVERVIEW_ACCEPTS_KEYS.load(Ordering::Relaxed));

        // Reopening sets `OVERVIEW_ACCEPTS_KEYS` again, and dropping
        // `OverviewSession` without `close` clears both tap flags.
        session.activate(
            &mut phase,
            OverviewPhase::Opening,
            Vec::new(),
            Motion::Settled,
        );
        assert!(OVERVIEW_OPEN.load(Ordering::Relaxed));
        assert!(crate::overview::OVERVIEW_ACCEPTS_KEYS.load(Ordering::Relaxed));
    }
    assert!(!OVERVIEW_OPEN.load(Ordering::Relaxed));
    assert!(!crate::overview::OVERVIEW_ACCEPTS_KEYS.load(Ordering::Relaxed));
}

#[test]
fn demux_keeps_overview_stream_silent_on_ordinary_desktop_events() {
    let mut harness = TestHarness::new().with_windows(1);
    harness.world().init_resource::<OverviewPhase>();
    harness.advance(Duration::from_millis(100));

    // Ordinary input and space events while closed never populate `OverviewEvent`.
    harness.world().write_message(Event::MouseMoved {
        point: CGPoint::new(50.0, 50.0),
        modifiers: crate::platform::Modifiers::empty(),
    });
    harness.world().write_message(Event::SpaceChanged);
    harness.app.update();
    assert!(
        harness
            .world()
            .resource::<Messages<OverviewEvent>>()
            .is_empty(),
        "OverviewEvent must stay empty while closed for non-overview events"
    );

    // Once active, environment changes (`SpaceChanged`, Mission Control, display changes)
    // are forwarded onto `OverviewEvent` so the overview tears down promptly.
    *harness.world().resource_mut::<OverviewPhase>() = OverviewPhase::Open;
    harness.world().write_message(Event::SpaceChanged);
    harness
        .world()
        .write_message(Event::MissionControlShowAllWindows);
    harness.app.update();
    assert_eq!(
        harness.world().resource::<Messages<OverviewEvent>>().len(),
        2
    );
}

#[test]
fn tabbed_display_stacks_only_expose_their_active_tab_to_the_overview() {
    let mut world = World::new();
    let [w1, w2, w3, w4] = std::array::from_fn(|_| world.spawn_empty().id());
    let mut strip = LayoutStrip::new(1, 0);
    strip.append(w1);
    strip.append_tabbed_stack(&[w2, w3, w4], w3);

    let shown: Vec<Entity> = visible_windows(&strip).collect();
    assert_eq!(shown, vec![w1, w3]);

    // Switching the active tab updates the overview's single card for that column.
    assert!(strip.set_active_tab(w2));
    let shown: Vec<Entity> = visible_windows(&strip).collect();
    assert_eq!(shown, vec![w1, w2]);

    // Toggling tabbed display off exposes all three stack members vertically.
    assert_eq!(strip.toggle_tabbed_display(w2), Some(false));
    let shown: Vec<Entity> = visible_windows(&strip).collect();
    assert_eq!(shown, vec![w1, w2, w3, w4]);
}

#[test]
fn layout_change_during_opening_triggers_follow_once_open() {
    #[derive(Resource, Default)]
    struct FollowRuns(usize);

    let mut app = App::new();
    app.init_resource::<OverviewPhase>()
        .init_resource::<FollowRuns>()
        .add_systems(
            Last,
            configure_overview_follow(|mut runs: ResMut<FollowRuns>| {
                runs.0 += 1;
            }),
        );

    let w1 = app
        .world_mut()
        .spawn((
            LayoutPosition(Origin::ZERO),
            Bounds(Size::new(800, 600)),
            FocusedMarker,
        ))
        .id();
    let w2 = app
        .world_mut()
        .spawn((
            LayoutPosition(Origin::new(800, 0)),
            Bounds(Size::new(800, 600)),
        ))
        .id();
    let mut strip = LayoutStrip::new(TEST_WORKSPACE_ID, 0);
    strip.append(w1);
    strip.append(w2);
    app.world_mut().spawn((strip, ActiveWorkspaceMarker));

    // 1. Changes while `Closed` are drained and do not trigger a stale follow on open.
    app.update();
    assert_eq!(app.world().resource::<FollowRuns>().0, 0);

    *app.world_mut().resource_mut::<OverviewPhase>() = OverviewPhase::Opening;
    app.update();
    *app.world_mut().resource_mut::<OverviewPhase>() = OverviewPhase::Open;
    app.update();
    assert_eq!(
        app.world().resource::<FollowRuns>().0,
        0,
        "opening without layout edits must not spuriously run overview_follow_layout"
    );

    // 2. Pressing `focus east` (`cmd - l`) or `swap` while `Opening` is still
    //    in flight (~362ms) must NOT be lost: as soon as `OverviewPhase::Open`
    //    is entered, `overview_follow_layout` must run once to catch up.
    *app.world_mut().resource_mut::<OverviewPhase>() = OverviewPhase::Opening;
    app.update();

    app.world_mut().entity_mut(w1).remove::<FocusedMarker>();
    app.world_mut().entity_mut(w2).insert(FocusedMarker);
    app.update();
    assert_eq!(
        app.world().resource::<FollowRuns>().0,
        0,
        "must wait for opening spring to finish before morphing"
    );

    *app.world_mut().resource_mut::<OverviewPhase>() = OverviewPhase::Open;
    app.update();
    assert_eq!(
        app.world().resource::<FollowRuns>().0,
        1,
        "focus/layout change during Opening must trigger follow once Open"
    );

    // Subsequent idle frames stay silent.
    app.update();
    assert_eq!(app.world().resource::<FollowRuns>().0, 1);
}
