use serde::{Deserialize, Deserializer, de};

use crate::{commands::Direction, config::deserialize_modifier, platform::Modifiers};

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(serde::Serialize))]
pub enum SwipeGestureDirection {
    Natural,
    Reversed,
}

pub(crate) fn focus_direction(gesture_direction: SwipeGestureDirection, delta: f64) -> Direction {
    let natural = matches!(gesture_direction, SwipeGestureDirection::Natural);
    if (delta > 0.0) == natural {
        Direction::East
    } else {
        Direction::West
    }
}

#[derive(Deserialize, Clone, Debug, Default)]
#[cfg_attr(test, derive(serde::Serialize))]
pub struct SwipeOptions {
    /// Swipe sensitivity multiplier. Lower values = less distance per finger
    /// movement. Range: 0.1–2.0. Default: 0.35.
    pub sensitivity: Option<f64>,

    /// Swipe inertia deceleration rate. Higher values = faster stop.
    /// Range: 1.0–10.0. Default: 4.0.
    pub deceleration: Option<f64>,

    /// Swiping keeps sliding windows until the first or last window.
    /// Set to false to clamp so edge windows stay on-screen. Default: true.
    #[allow(dead_code)]
    pub continuous: Option<bool>,

    pub gesture: Option<GestureOptions>,
    pub scroll: Option<ScrollOptions>,
}

#[derive(Deserialize, Clone, Debug, Default)]
#[cfg_attr(test, derive(serde::Serialize))]
pub struct GestureOptions {
    /// The number of fingers required for swipe gestures to move windows.
    pub fingers_count: Option<usize>,

    /// Which direction swipe gestures should move windows.
    pub direction: Option<SwipeGestureDirection>,

    /// Whether to intercept vertical swipes.
    pub vertical: Option<bool>,

    /// Focus one adjacent window per configured horizontal finger distance.
    pub paging: Option<bool>,

    /// Finger travel required for one page. Default: 2 cm.
    #[serde(default, deserialize_with = "positive_distance_cm")]
    pub page_distance_cm: Option<f64>,

    /// Play a haptic pulse for each confirmed paging focus change. Default: true.
    pub haptics: Option<bool>,
}

fn positive_distance_cm<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: Deserializer<'de>,
{
    let distance = Option::<f64>::deserialize(deserializer)?;
    if distance.is_some_and(|distance| !distance.is_finite() || distance <= 0.0) {
        return Err(de::Error::custom(
            "page_distance_cm must be finite and greater than zero",
        ));
    }
    Ok(distance)
}

#[derive(Deserialize, Clone, Debug, Default)]
#[cfg_attr(test, derive(serde::Serialize))]
pub struct ScrollOptions {
    /// Select one adjacent window per wheel input. Default: false.
    pub window_step: Option<bool>,

    /// Modifier key(s) required for scroll wheel swiping.
    /// Accepts the same format as keybindings: "alt", "cmd", "alt + cmd", "alt + rcmd" etc.
    #[serde(default, deserialize_with = "deserialize_modifier")]
    #[cfg_attr(test, serde(serialize_with = "crate::config::serialize_modifier"))]
    pub modifier: Option<Modifiers>,

    /// Additional modifier key(s) that, combined with the scroll modifier,
    /// switches virtual workspaces vertically instead of scrolling horizontally.
    #[serde(default, deserialize_with = "deserialize_modifier")]
    #[cfg_attr(test, serde(serialize_with = "crate::config::serialize_modifier"))]
    pub vertical_modifier: Option<Modifiers>,
}
