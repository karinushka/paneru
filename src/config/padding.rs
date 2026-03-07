use serde::Deserialize;

#[derive(Deserialize, Clone, Debug, Default)]
pub struct PaddingOptions {
    /// Padding applied at screen edges (in pixels). Independent from between-window gaps.
    /// Default: 0 on all sides.
    pub top: Option<u16>,
    pub bottom: Option<u16>,
    pub left: Option<u16>,
    pub right: Option<u16>,
}
