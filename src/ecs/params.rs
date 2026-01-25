use std::time::Duration;

use bevy::{
    ecs::{
        query::{With, Without},
        system::{Local, Query, Res, ResMut, Single, SystemParam},
        world::Mut,
    },
    time::Time,
};
use objc2_core_foundation::CGRect;
use objc2_core_graphics::CGDirectDisplayID;

use super::{ActiveDisplayMarker, FocusFollowsMouse, MissionControlActive, SkipReshuffle};
use crate::{
    config::{Config, WindowParams},
    manager::{Display, WindowManager, WindowPane},
    platform::WinID,
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

    /// Returns `true` if continuous swipe behavior is enabled.
    /// If the configuration option is not set, it defaults to `true`.
    pub fn continuous_swipe(&self) -> bool {
        // Default is enabled.
        self.config.options().continuous_swipe.is_none_or(|cs| cs)
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

    /// Returns `true` if Mission Control is currently active.
    ///
    /// # Returns
    ///
    /// `true` if Mission Control is active, `false` otherwise.
    pub fn mission_control_active(&self) -> bool {
        self.mission_control_active.0
    }
}

/// Similar to the `ThrottledSystem`, but only allows an event to happen no events happened for a
/// specified Duration.
#[derive(SystemParam)]
pub struct DebouncedSystem<'w, 's> {
    time: Res<'w, Time>,
    elapsed: Local<'s, Duration>,
}

impl DebouncedSystem<'_, '_> {
    /// Returns `true` if the event should ignored (debounced).
    pub fn bounce(&mut self, duration: Duration) -> bool {
        if self.time.elapsed().saturating_sub(*self.elapsed) > duration {
            *self.elapsed = self.time.elapsed();
            return false;
        }
        true
    }
}

/// A Bevy `SystemParam` that provides immutable access to the currently active `Display` and other displays.
/// It ensures that only one display is marked as active at any given time.
#[derive(SystemParam)]
pub struct ActiveDisplay<'w, 's> {
    /// The single active `Display` component, marked with `ActiveDisplayMarker`.
    display: Single<'w, 's, &'static Display, With<ActiveDisplayMarker>>,
    /// The `WindowManager` resource for querying display and space information.
    window_manager: Res<'w, WindowManager>,
    /// A query for all other `Display` components that are not marked as active.
    other_displays: Query<'w, 's, &'static Display, Without<ActiveDisplayMarker>>,
}

impl ActiveDisplay<'_, '_> {
    /// Returns an immutable reference to the active `Display`.
    pub fn display(&self) -> &Display {
        &self.display
    }

    /// Returns the `CGDirectDisplayID` of the active display.
    pub fn id(&self) -> CGDirectDisplayID {
        self.display.id()
    }

    /// Returns an iterator over immutable references to all other displays (non-active).
    pub fn other(&self) -> impl Iterator<Item = &Display> {
        self.other_displays.iter()
    }

    /// Retrieves an immutable reference to the `WindowPane` of the active space on the active display.
    ///
    /// # Returns
    ///
    /// `Ok(&WindowPane)` if the active panel is found, otherwise `Err(Error)`.
    pub fn active_panel(&self) -> crate::errors::Result<&WindowPane> {
        self.window_manager
            .0
            .active_display_space(self.display.id())
            .and_then(|workspace_id| self.display.active_panel(workspace_id))
    }

    /// Returns the `CGRect` representing the bounds of the active display.
    pub fn bounds(&self) -> CGRect {
        self.display.bounds
    }
}

/// A Bevy `SystemParam` that provides mutable access to the currently active `Display` and other displays.
/// It allows systems to modify the active display and its associated `WindowPane`s.
#[derive(SystemParam)]
pub struct ActiveDisplayMut<'w, 's> {
    /// The single active `Display` component, marked with `ActiveDisplayMarker`.
    display: Single<'w, 's, &'static mut Display, With<ActiveDisplayMarker>>,
    /// The `WindowManager` resource for querying display and space information.
    window_manager: Res<'w, WindowManager>,
    /// A query for all other `Display` components that are not marked as active.
    other_displays: Query<'w, 's, &'static mut Display, Without<ActiveDisplayMarker>>,
}

impl ActiveDisplayMut<'_, '_> {
    /// Returns a mutable reference to the active `Display`.
    pub fn display(&mut self) -> &mut Display {
        &mut self.display
    }

    /// Returns the `CGDirectDisplayID` of the active display.
    pub fn id(&self) -> CGDirectDisplayID {
        self.display.id()
    }

    /// Returns an iterator over mutable references to all other displays (non-active).
    pub fn other(&mut self) -> impl Iterator<Item = Mut<'_, Display>> {
        self.other_displays.iter_mut()
    }

    /// Retrieves a mutable reference to the `WindowPane` of the active space on the active display.
    ///
    /// # Returns
    ///
    /// `Ok(&mut WindowPane)` if the active panel is found, otherwise `Err(Error)`.
    pub fn active_panel(&mut self) -> crate::errors::Result<&mut WindowPane> {
        self.window_manager
            .0
            .active_display_space(self.display.id())
            .and_then(|workspace_id| self.display.active_panel_mut(workspace_id))
    }

    /// Returns the `CGRect` representing the bounds of the active display.
    pub fn bounds(&self) -> CGRect {
        self.display.bounds
    }
}
