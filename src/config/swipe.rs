use serde::Deserialize;

use crate::errors::Error;
use crate::platform::Modifiers;

#[derive(Clone, Debug, Deserialize)]
pub enum SwipeGestureDirection {
    Natural,
    Reversed,
}

#[derive(Deserialize, Clone, Debug, Default)]
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
pub struct GestureOptions {
    /// The number of fingers required for swipe gestures to move windows.
    pub fingers_count: Option<usize>,

    /// Which direction swipe gestures should move windows.
    pub direction: Option<SwipeGestureDirection>,
}

#[derive(Deserialize, Clone, Debug, Default)]
pub struct ScrollOptions {
    /// Modifier key(s) required for scroll wheel swiping.
    /// Accepts the same format as keybindings: "alt", "cmd", "alt + cmd", "alt + rcmd" etc.
    #[serde(default, deserialize_with = "deserialize_modifier")]
    pub modifier: Option<Modifiers>,

    /// Additional modifier key(s) that, combined with the scroll modifier,
    /// switches virtual workspaces vertically instead of scrolling horizontally.
    #[serde(default, deserialize_with = "deserialize_modifier")]
    pub vertical_modifier: Option<Modifiers>,
}

fn deserialize_modifier<'de, D>(deserializer: D) -> Result<Option<Modifiers>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let Some(s) = Option::<String>::deserialize(deserializer)? else {
        return Ok(None);
    };
    super::parse_modifiers(&s)
        .map(Some)
        .map_err(|e: Error| serde::de::Error::custom(e.to_string()))
}
