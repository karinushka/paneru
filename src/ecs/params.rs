use bevy::{
    ecs::{
        entity::Entity,
        hierarchy::ChildOf,
        query::{With, Without},
        system::{Query, Res, ResMut, Single, SystemParam},
        world::Mut,
    },
    math::IRect,
};
use objc2_core_graphics::CGDirectDisplayID;
use tracing::warn;

use super::{ActiveDisplayMarker, FocusFollowsMouse, MissionControlActive, SkipReshuffle};
use crate::{
    config::{Config, WindowParams},
    ecs::{ActiveWorkspaceMarker, DockPosition, FocusedMarker, FullWidthMarker, Unmanaged},
    manager::{Application, Display, LayoutStrip, Window},
    platform::{ProcessSerialNumber, WinID},
};

/// A Bevy `SystemParam` that provides access to the application's configuration and related state.
/// It allows systems to query various configuration options and modify flags like `FocusFollowsMouse` or `SkipReshuffle`.
#[derive(SystemParam)]
pub struct Configuration<'w> {
    /// The main application `Config` resource.
    config: Res<'w, Config>,
    /// Resource to manage the window ID for focus-follows-mouse behavior.
    focus_follows_mouse_id: ResMut<'w, FocusFollowsMouse>,
    /// Resource to determine if window reshuffling should be skipped.
    skip_reshuffle: ResMut<'w, SkipReshuffle>,
    /// Resource indicating whether Mission Control is currently active.
    mission_control_active: Res<'w, MissionControlActive>,
}

impl Configuration<'_> {
    /// Returns `true` if focus should follow the mouse based on the current configuration.
    /// If the configuration option is not set, it defaults to `true`.
    pub fn focus_follows_mouse(&self) -> bool {
        // Default is enabled.
        self.config
            .options()
            .focus_follows_mouse
            .is_none_or(|ffm| ffm)
    }

    /// Returns `true` if the mouse cursor should follow the focused window based on the current configuration.
    /// If the configuration option is not set, it defaults to `true`.
    pub fn mouse_follows_focus(&self) -> bool {
        // Default is enabled.
        self.config
            .options()
            .mouse_follows_focus
            .is_none_or(|mff| mff)
    }

    pub fn auto_center(&self) -> bool {
        // Default is enabled.
        self.config
            .options()
            .auto_center
            .is_some_and(|centered| centered)
    }

    /// Returns the configured number of fingers for swipe gestures.
    ///
    /// # Returns
    ///
    /// An `Option<usize>` containing the number of fingers, or `None` if not configured.
    pub fn swipe_gesture_fingers(&self) -> Option<usize> {
        self.config.options().swipe_gesture_fingers
    }

    /// Finds window properties for a given `title` and `bundle_id` based on the application configuration.
    ///
    /// # Arguments
    ///
    /// * `title` - The title of the window to match.
    /// * `bundle_id` - The bundle identifier of the application owning the window.
    ///
    /// # Returns
    ///
    /// `Some(WindowParams)` if matching window properties are found, otherwise `None`.
    pub fn find_window_properties(&self, title: &str, bundle_id: &str) -> Vec<WindowParams> {
        self.config.find_window_properties(title, bundle_id)
    }

    /// Returns the `WinID` of the window currently marked for focus-follows-mouse.
    ///
    /// # Returns
    ///
    /// An `Option<WinID>` if a window is marked, otherwise `None`.
    pub fn ffm_flag(&self) -> Option<WinID> {
        self.focus_follows_mouse_id.0
    }

    /// Sets the `WinID` for the focus-follows-mouse flag.
    ///
    /// # Arguments
    ///
    /// * `flag` - An `Option<WinID>` to set as the focus-follows-mouse target.
    pub fn set_ffm_flag(&mut self, flag: Option<WinID>) {
        self.focus_follows_mouse_id.as_mut().0 = flag;
    }

    /// Sets the `skip_reshuffle` flag.
    /// When `true`, window reshuffling logic will be temporarily bypassed.
    ///
    /// # Arguments
    ///
    /// * `to` - A boolean value to set the `skip_reshuffle` flag to.
    pub fn set_skip_reshuffle(&mut self, to: bool) {
        self.skip_reshuffle.as_mut().0 = to;
    }

    /// Returns `true` if window reshuffling should be skipped.
    ///
    /// # Returns
    ///
    /// `true` if reshuffling is skipped, `false` otherwise.
    pub fn skip_reshuffle(&self) -> bool {
        self.skip_reshuffle.0
    }

    pub fn edge_padding(&self) -> (i32, i32, i32, i32) {
        self.config.edge_padding()
    }

    /// Returns `true` if Mission Control is currently active.
    ///
    /// # Returns
    ///
    /// `true` if Mission Control is active, `false` otherwise.
    pub fn mission_control_active(&self) -> bool {
        self.mission_control_active.0
    }
}

/// A Bevy `SystemParam` that provides immutable access to the currently active `Display` and other displays.
/// It ensures that only one display is marked as active at any given time.
#[derive(SystemParam)]
pub struct ActiveDisplay<'w, 's> {
    strip: Single<'w, 's, &'static LayoutStrip, With<ActiveWorkspaceMarker>>,
    /// The single active `Display` component, marked with `ActiveDisplayMarker`.
    display: Single<
        'w,
        's,
        (&'static Display, Option<&'static DockPosition>),
        With<ActiveDisplayMarker>,
    >,
    /// A query for all other `Display` components that are not marked as active.
    other_displays: Query<'w, 's, &'static Display, Without<ActiveDisplayMarker>>,
}

impl ActiveDisplay<'_, '_> {
    /// Returns an immutable reference to the active `Display`.
    pub fn display(&self) -> &Display {
        self.display.0
    }

    /// Returns the `CGDirectDisplayID` of the active display.
    pub fn id(&self) -> CGDirectDisplayID {
        self.display.0.id()
    }

    /// Returns an iterator over immutable references to all other displays (non-active).
    pub fn other(&self) -> impl Iterator<Item = &Display> {
        self.other_displays.iter()
    }

    pub fn active_strip(&self) -> &LayoutStrip {
        *self.strip
    }

    /// Returns the `CGRect` representing the bounds of the active display.
    pub fn bounds(&self) -> IRect {
        self.display.0.bounds()
    }

    pub fn dock(&self) -> Option<&DockPosition> {
        self.display.1
    }
}

/// A Bevy `SystemParam` that provides mutable access to the currently active `Display` and other displays.
/// It allows systems to modify the active display and its associated `LayoutStrip`s.
#[derive(SystemParam)]
pub struct ActiveDisplayMut<'w, 's> {
    strip: Single<'w, 's, &'static mut LayoutStrip, With<ActiveWorkspaceMarker>>,
    /// The single active `Display` component, marked with `ActiveDisplayMarker`.
    display: Single<'w, 's, &'static mut Display, With<ActiveDisplayMarker>>,
    /// A query for all other `Display` components that are not marked as active.
    other_displays: Query<'w, 's, &'static mut Display, Without<ActiveDisplayMarker>>,
}

impl ActiveDisplayMut<'_, '_> {
    /// Returns the `CGDirectDisplayID` of the active display.
    pub fn id(&self) -> CGDirectDisplayID {
        self.display.id()
    }

    /// Returns an iterator over mutable references to all other displays (non-active).
    pub fn other(&mut self) -> impl Iterator<Item = Mut<'_, Display>> {
        self.other_displays.iter_mut()
    }

    pub fn active_strip(&mut self) -> &mut LayoutStrip {
        &mut self.strip
    }

    /// Returns the `CGRect` representing the bounds of the active display.
    pub fn bounds(&self) -> IRect {
        self.display.bounds()
    }
}

#[derive(SystemParam)]
pub struct Windows<'w, 's> {
    #[allow(clippy::type_complexity)]
    all: Query<
        'w,
        's,
        (
            &'static Window,
            Entity,
            &'static ChildOf,
            Option<&'static Unmanaged>,
        ),
    >,
    focus: Query<'w, 's, (&'static Window, Entity), With<FocusedMarker>>,
    previous_size: Query<'w, 's, (&'static Window, Entity, &'static FullWidthMarker)>,
}

impl Windows<'_, '_> {
    #[allow(clippy::type_complexity)]
    fn get_all(&self, entity: Entity) -> Option<(&Window, Entity, &ChildOf, Option<&Unmanaged>)> {
        self.all
            .get(entity)
            .inspect_err(|err| warn!("unable to find window: {err}"))
            .ok()
    }

    pub fn get_managed(&self, entity: Entity) -> Option<(&Window, Entity, Option<&Unmanaged>)> {
        self.get_all(entity)
            .map(|(window, entity, _, unmanaged)| (window, entity, unmanaged))
    }

    pub fn get(&self, entity: Entity) -> Option<&Window> {
        self.get_all(entity).map(|(window, _, _, _)| window)
    }

    pub fn find(&self, window_id: WinID) -> Option<(&Window, Entity)> {
        self.all
            .into_iter()
            .find(|(window, _, _, _)| window.id() == window_id)
            .map(|(window, entity, _, _)| (window, entity))
    }

    pub fn find_parent(&self, window_id: WinID) -> Option<(&Window, Entity, Entity)> {
        self.all.iter().find_map(|(window, entity, childof, _)| {
            (window.id() == window_id).then_some((window, entity, childof.parent()))
        })
    }

    pub fn find_managed(&self, window_id: WinID) -> Option<(&Window, Entity)> {
        self.all.iter().find_map(|(window, entity, _, unmanaged)| {
            (unmanaged.is_none() && window.id() == window_id).then_some((window, entity))
        })
    }

    pub fn focused(&self) -> Option<(&Window, Entity)> {
        self.focus.single().ok()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&Window, Entity)> {
        self.all
            .iter()
            .map(|(window, entity, _, _)| (window, entity))
    }

    pub fn full_width(&self, entity: Entity) -> Option<f64> {
        self.previous_size
            .get(entity)
            .map(|(_, _, width)| width.0)
            .ok()
    }

    pub fn psn(&self, window_id: WinID, apps: &Query<&Application>) -> Option<ProcessSerialNumber> {
        self.find_parent(window_id)
            .and_then(|(_, _, parent)| apps.get(parent).ok())
            .map(|app| app.psn())
    }
}
