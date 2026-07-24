//! Centralized visual design tokens for the portable Bcode presentation.
//!
//! `HyperChad` component attributes still carry concrete renderer-neutral values, but those values
//! are owned here so typography, spacing, surfaces, borders, status colors, and readable widths do
//! not drift across components.

/// Application and reusable component surfaces.
pub(super) mod surface {
    pub const APP: &str = "#0d1117";
    pub const PANEL: &str = "#161b22";
    pub const INSET: &str = "#010409";
    pub const SELECTED: &str = "#1f2937";
    pub const CONTROL: &str = "#21262d";
    pub const DISABLED: &str = "#30363d";
    pub const ACTIVE: &str = "#1f6feb";
    pub const USER_MESSAGE: &str = "#0c2d48";
    pub const ERROR_INSET: &str = "#2d1015";
    pub const SUCCESS_INSET: &str = "#102818";
    pub const BORDER: &str = "#30363d";
}

/// Text and semantic status colors.
pub(super) mod color {
    pub const TEXT: &str = "#c9d1d9";
    pub const STRONG: &str = "#f0f6fc";
    pub const MUTED: &str = "#8b949e";
    pub const INFO: &str = "#58a6ff";
    pub const SUCCESS: &str = "#7ee787";
    pub const WARNING: &str = "#f2cc60";
    pub const ERROR: &str = "#f85149";
    pub const REASONING: &str = "#a371f7";
    pub const ON_ACCENT: &str = "#ffffff";
    pub const REMOVED_TEXT: &str = "#f0b8bd";
    pub const ADDED_TEXT: &str = "#b7efc5";
    pub const ERROR_BORDER: &str = "#6e3035";
    pub const SUCCESS_BORDER: &str = "#2f6f44";
}

/// Accent control colors.
pub(super) mod accent {
    pub const POSITIVE: &str = "#238636";
    pub const DESTRUCTIVE: &str = "#da3633";
}

/// Shared spacing increments in logical pixels.
pub(super) mod space {
    pub const S2: i32 = 2;
    pub const S3: i32 = 3;
    pub const XS: i32 = 4;
    pub const S6: i32 = 6;
    pub const S7: i32 = 7;
    pub const SM: i32 = 8;
    pub const S9: i32 = 9;
    pub const S10: i32 = 10;
    pub const MD: i32 = 12;
    pub const S14: i32 = 14;
    pub const LG: i32 = 16;
    pub const S18: i32 = 18;
    pub const XL: i32 = 24;
}

/// Shared corner radii in logical pixels.
pub(super) mod radius {
    pub const CONTROL: i32 = 6;
    pub const CARD: i32 = 8;
    pub const PANEL: i32 = 10;
    pub const PILL: i32 = 999;
}

/// Shared responsive and readable layout widths in logical pixels.
pub(super) mod width {
    pub const NAVIGATION: i32 = 280;
    pub const CONTENT: i32 = 960;
    pub const FLUID: i32 = 10_000;
}

/// Shared typography settings.
pub(super) mod typeface {
    pub const UI: &str = "system-ui, sans-serif";
    pub const CAPTION: i32 = 10;
    pub const DETAIL: i32 = 11;
    pub const LABEL: i32 = 12;
    pub const BODY: i32 = 13;
    pub const SUBHEADING: i32 = 14;
    pub const SECTION: i32 = 16;
    pub const HEADING: i32 = 20;
    pub const TITLE: i32 = 28;
}
