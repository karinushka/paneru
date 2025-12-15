use bevy::ecs::system::{Res, ResMut, SystemParam};

use crate::{
    config::{Config, WindowParams},
    events::{FocusFollowsMouse, MissionControlActive, SkipReshuffle},
    skylight::WinID,
};

#[derive(SystemParam)]
pub struct Configuration<'w> {
    config: Res<'w, Config>,
    focus_follows_mouse_id: ResMut<'w, FocusFollowsMouse>,
    skip_reshuffle: ResMut<'w, SkipReshuffle>,
    mission_control_active: Res<'w, MissionControlActive>,
}

impl Configuration<'_> {
    /// Returns true if focus should follow the mouse.
    pub fn focus_follows_mouse(&self) -> bool {
        // Default is enabled.
        self.config
            .options()
            .focus_follows_mouse
            .is_none_or(|ffm| ffm)
    }

    /// Returns true if the mouse should follow focus.
    pub fn mouse_follows_focus(&self) -> bool {
        // Default is enabled.
        self.config
            .options()
            .mouse_follows_focus
            .is_none_or(|mff| mff)
    }

    /// Returns the number of fingers for swipe gestures.
    pub fn swipe_gesture_fingers(&self) -> Option<usize> {
        self.config.options().swipe_gesture_fingers
    }

    /// Finds window properties for a given title and bundle ID.
    pub fn find_window_properties(&self, title: &str, bundle_id: &str) -> Option<WindowParams> {
        self.config.find_window_properties(title, bundle_id)
    }

    /// Returns the window ID that is currently being focused by focus-follows-mouse.
    pub fn ffm_flag(&self) -> Option<WinID> {
        self.focus_follows_mouse_id.0
    }

    /// Sets the focus-follows-mouse window ID.
    pub fn set_ffm_flag(&mut self, flag: Option<WinID>) {
        self.focus_follows_mouse_id.as_mut().0 = flag;
    }

    /// Sets the `skip_reshuffle` flag.
    pub fn set_skip_reshuffle(&mut self, to: bool) {
        self.skip_reshuffle.as_mut().0 = to;
    }

    /// Returns true if reshuffling should be skipped.
    pub fn skip_reshuffle(&self) -> bool {
        self.skip_reshuffle.0
    }

    /// Returns true if Mission Control is active.
    pub fn mission_control_active(&self) -> bool {
        self.mission_control_active.0
    }
}
