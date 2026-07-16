use bevy::ecs::entity::Entity;

use super::layout::LayoutStrip;

pub(super) fn width_ratio_for_owner<'a>(
    window: Entity,
    width: i32,
    strips: impl IntoIterator<Item = (&'a LayoutStrip, i32)>,
) -> Option<f64> {
    strips.into_iter().find_map(|(strip, display_width)| {
        (strip.contains(window) && display_width > 0)
            .then(|| f64::from(width) / f64::from(display_width))
    })
}

#[cfg(test)]
mod tests {
    use bevy::prelude::*;

    use super::*;
    use crate::ecs::{Bounds, Position, WidthRatio};
    use crate::manager::{Display, MockWindowApi, Origin, Size, Window};
    use crate::platform::AxMainThread;

    #[test]
    fn ecs_commit_uses_owning_display_and_runs_ax_on_main_thread() {
        let expected_thread = std::thread::current().id();
        let mut mock = MockWindowApi::new();
        mock.expect_resize()
            .with(mockall::predicate::eq(Size::new(1_200, 500)))
            .times(1)
            .return_const(());
        mock.expect_reposition()
            .with(mockall::predicate::eq(Origin::new(10, 20)))
            .times(1)
            .returning(move |_| assert_eq!(std::thread::current().id(), expected_thread));

        let mut app = App::new();
        app.insert_non_send_resource(AxMainThread::for_tests())
            .add_systems(
                Update,
                (
                    super::super::systems::commit_window_position,
                    super::super::systems::commit_window_size,
                ),
            );
        let primary_display = app
            .world_mut()
            .spawn(Display::new(1, IRect::new(0, 0, 2_000, 1_000), 0))
            .id();
        let external_display = app
            .world_mut()
            .spawn(Display::new(2, IRect::new(2_000, 0, 2_800, 1_000), 0))
            .id();
        let external_window = app
            .world_mut()
            .spawn((
                Window::new(Box::new(mock)),
                Position(Origin::new(10, 20)),
                Bounds(Size::new(1_200, 500)),
                WidthRatio(0.0),
            ))
            .id();
        let primary = LayoutStrip::new(1, 0);
        let mut external = LayoutStrip::new(2, 0);
        external.append(external_window);
        app.world_mut().spawn((primary, ChildOf(primary_display)));
        app.world_mut().spawn((external, ChildOf(external_display)));

        app.update();

        let saved_ratio = app.world().get::<WidthRatio>(external_window).unwrap().0;
        assert!((saved_ratio - 1.5).abs() < f64::EPSILON);
        assert_eq!((800.0 * saved_ratio).round() as i32, 1_200);
    }
}
