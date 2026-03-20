use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
pub enum SwipeGestureDirection {
    Natural,
    Reversed,
}
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum SwipeScrollModifier {
    Alt,
    Cmd,
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
    /// The modifier key required for scroll wheel swiping.
    pub modifier: Option<SwipeScrollModifier>,
}
