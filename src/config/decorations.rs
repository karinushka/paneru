use serde::{Deserialize, Deserializer, de};

#[derive(Deserialize, Clone, Debug, Default)]
pub struct DecorationsOptions {
    pub active: Option<GeneralDecorationsOptions>,
    pub inactive: Option<GeneralDecorationsOptions>,
    pub workspace_menu_status: Option<bool>,
    pub menu: Option<MenubarOptions>,
    pub workspace_popup_status: Option<bool>,
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
    /// Opacity of the dim overlay on inactive windows when in Dark Mode.
    pub opacity_night: Option<f32>,
    /// Hex color for the dim overlay, e.g. "#000000".
    /// Default: "#000000" (black).
    pub color: Option<String>,
}
#[derive(Deserialize, Debug, Clone, Default)]
pub struct MenubarOptions {
    pub orientation: Option<MenubarOrientation>,
    pub colors: Option<Vec<String>>,
    pub angle: Option<f64>,
    pub descriptor: Option<MenubarDescriptorOptions>,
    pub indicator: Option<MenubarIndicatorOptions>,
}
#[derive(Deserialize, Debug, Clone, Default)]
pub struct MenubarDescriptorOptions {
    /// How the graphic/text descriptor to the left of the
    /// VW #(s) is displayed. Options are symbol, text, both
    /// or Hidden. Default is symbol.
    pub style: Option<DescriptorStyle>,
    /// Text string preceding VW #(s)
    /// Defaults to "VW"
    pub text: Option<String>,
    /// Symbol System Name (SF Symbols)
    /// Defaults to "fish.fill"
    pub symbol: Option<String>,
}
#[derive(Deserialize, Debug, Clone, Copy, Default)]
pub struct MenubarIndicatorOptions {
    /// Format for virtual workspace indicator. Options are mono,
    /// which shows just the current workspace, or multi, which
    /// shows all active workspaces.
    /// Defaults to mono.
    pub style: Option<IndicatorStyle>,
    /// Character style for virtual workspace indicator. Options
    /// are default (1 or 1 2 3), roman (I or I II III), or
    /// unicode (incompatible with mono or ○ ☉ ○). Roman
    /// only properly formats numbers < 90.
    pub format: Option<IndicatorFormat>,
    /// Font size in pt.
    /// Defaults to 13
    pub font_size: Option<f64>,
    /// Only relevant for multi style/Unicode format.
    /// Defaults to ☉
    pub active_character: Option<char>,
    /// Only relevant for multi style/Unicode format.
    /// Defaults to ○
    pub inactive_character: Option<char>,
}
#[derive(Deserialize, Debug, Copy, Clone, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum IndicatorFormat {
    Default,
    Roman,
    // If set must define active/inactive char. Only relevant for multi style
    Unicode,
}
#[derive(Deserialize, Debug, Copy, Clone, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MenubarOrientation {
    Default,
    Flipped,
}
#[derive(Deserialize, Debug, Copy, Clone, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum IndicatorStyle {
    Mono,
    Multi,
}
#[derive(Deserialize, Debug, Copy, Clone, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DescriptorStyle {
    Symbol,
    Text,
    Both,
    Hidden,
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
