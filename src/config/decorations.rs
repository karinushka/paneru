use serde::{Deserialize, Deserializer, de};

#[derive(Deserialize, Clone, Debug, Default)]
pub struct DecorationsOptions {
    pub active: Option<GeneralDecorationsOptions>,
    pub inactive: Option<GeneralDecorationsOptions>,
}

#[derive(Deserialize, Clone, Debug, Default)]
pub struct GeneralDecorationsOptions {
    pub border: Option<GeneralBorderOptions>,
    pub dim: Option<GeneralDimOptions>,
}

#[derive(Deserialize, Clone, Debug, Default)]
pub struct GeneralBorderOptions {
    /// Is border enabled
    /// Default: false.
    pub enabled: Option<bool>,
    /// Hex color for the window border, e.g. "#FF0000".
    /// Default: "#FFFFFF" (white).
    pub color: Option<String>,
    /// Opacity of the window border (0.0–1.0).
    /// Default: 1.0.
    pub opacity: Option<f64>,

    /// Width of the window border in pixels.
    /// Default: 2.0.
    pub width: Option<f64>,
    /// Corner radius of the window border.
    /// Default: 10.0.
    #[serde(default, deserialize_with = "deserialize_border_radius_option")]
    pub radius: Option<BorderRadiusOption>,
}

#[derive(Deserialize, Clone, Debug, Default)]
pub struct GeneralDimOptions {
    /// Opacity of the dim overlay on inactive windows (0.0=off, 1.0=fully black).
    /// Default: 0.0 (disabled).
    pub opacity: Option<f32>,
    /// Hex color for the dim overlay, e.g. "#000000".
    /// Default: "#000000" (black).
    pub color: Option<String>,
}
#[derive(Clone, Debug, PartialEq)]
pub enum BorderRadiusOption {
    Auto,
    Value(f64),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum BorderRadiusValue {
    Number(f64),
    Text(String),
}

pub fn deserialize_border_radius_option<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<BorderRadiusOption>, D::Error>
where
    D: Deserializer<'de>,
{
    let input = Option::<BorderRadiusValue>::deserialize(deserializer)?;
    input
        .map(|value| match value {
            BorderRadiusValue::Number(radius) => Ok(BorderRadiusOption::Value(radius)),
            BorderRadiusValue::Text(value) if value.eq_ignore_ascii_case("auto") => {
                Ok(BorderRadiusOption::Auto)
            }
            BorderRadiusValue::Text(value) => Err(de::Error::custom(format!(
                "invalid border_radius value: {value}. Expected a number or \"auto\"",
            ))),
        })
        .transpose()
}
