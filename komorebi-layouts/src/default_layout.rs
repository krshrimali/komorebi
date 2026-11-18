use std::collections::HashMap;

use clap::ValueEnum;
use serde::Deserialize;
use serde::Serialize;
use strum::Display;
use strum::EnumString;

use super::OperationDirection;
use super::Rect;
use super::Sizing;

/// Maximum number of ratio values that can be specified for column_ratios and row_ratios
pub const MAX_RATIOS: usize = 5;

/// Minimum allowed ratio value (prevents zero-sized windows)
pub const MIN_RATIO: f32 = 0.1;

/// Maximum allowed ratio value (ensures space for remaining windows)
pub const MAX_RATIO: f32 = 0.9;

/// Default ratio value when none is specified
pub const DEFAULT_RATIO: f32 = 0.5;

/// Default secondary ratio value for UltrawideVerticalStack layout
pub const DEFAULT_SECONDARY_RATIO: f32 = 0.25;

/// Validates and converts a Vec of ratios into a fixed-size array.
/// - Clamps values to MIN_RATIO..MAX_RATIO range
/// - Truncates when cumulative sum reaches or exceeds 1.0
/// - Limits to MAX_RATIOS values
#[must_use]
pub fn validate_ratios(ratios: &[f32]) -> [Option<f32>; MAX_RATIOS] {
    let mut arr = [None; MAX_RATIOS];
    let mut cumulative_sum = 0.0_f32;

    for (i, &val) in ratios.iter().take(MAX_RATIOS).enumerate() {
        let clamped_val = val.clamp(MIN_RATIO, MAX_RATIO);

        // Only add this ratio if cumulative sum stays below 1.0
        if cumulative_sum + clamped_val < 1.0 {
            arr[i] = Some(clamped_val);
            cumulative_sum += clamped_val;
        } else {
            // Stop adding ratios - cumulative sum would reach or exceed 1.0
            tracing::debug!(
                "Truncating ratios at index {} - cumulative sum {} + {} would reach/exceed 1.0",
                i,
                cumulative_sum,
                clamped_val
            );
            break;
        }
    }
    arr
}

#[derive(
    Clone, Copy, Debug, Serialize, Deserialize, Eq, PartialEq, Hash, Display, EnumString, ValueEnum,
)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
/// A predefined komorebi layout
pub enum DefaultLayout {
    /// BSP Layout
    ///
    /// ```text
    /// +-------+-----+
    /// |       |     |
    /// |       +--+--+
    /// |       |  |--|
    /// +-------+--+--+
    /// ```
    BSP,
    /// Columns Layout
    ///
    /// ```text
    /// +--+--+--+--+
    /// |  |  |  |  |
    /// |  |  |  |  |
    /// |  |  |  |  |
    /// +--+--+--+--+
    /// ```
    Columns,
    /// Rows Layout
    ///
    /// ```text
    /// +-----------+
    /// |-----------|
    /// |-----------|
    /// |-----------|
    /// +-----------+
    /// ```
    Rows,
    /// Vertical Stack Layout
    ///
    /// ```text
    /// +-------+-----+
    /// |       |     |
    /// |       +-----+
    /// |       |     |
    /// +-------+-----+
    /// ```
    VerticalStack,
    /// Horizontal Stack Layout
    ///
    /// ```text
    /// +------+------+
    /// |             |
    /// |------+------+
    /// |      |      |
    /// +------+------+
    /// ```
    HorizontalStack,
    /// Ultrawide Vertical Stack Layout
    ///
    /// ```text
    /// +-----+-----------+-----+
    /// |     |           |     |
    /// |     |           +-----+
    /// |     |           |     |
    /// |     |           +-----+
    /// |     |           |     |
    /// +-----+-----------+-----+
    /// ```
    UltrawideVerticalStack,
    /// Grid Layout
    ///
    /// ```text
    /// +-----+-----+   +---+---+---+   +---+---+---+   +---+---+---+
    /// |     |     |   |   |   |   |   |   |   |   |   |   |   |   |
    /// |     |     |   |   |   |   |   |   |   |   |   |   |   +---+
    /// +-----+-----+   |   +---+---+   +---+---+---+   +---+---|   |
    /// |     |     |   |   |   |   |   |   |   |   |   |   |   +---+
    /// |     |     |   |   |   |   |   |   |   |   |   |   |   |   |
    /// +-----+-----+   +---+---+---+   +---+---+---+   +---+---+---+
    ///   4 windows       5 windows       6 windows       7 windows
    /// ```
    Grid,
    /// Right Main Vertical Stack Layout
    ///
    /// ```text
    /// +-----+-------+
    /// |     |       |
    /// +-----+       |
    /// |     |       |
    /// +-----+-------+
    /// ```
    RightMainVerticalStack,
    /// Scrolling Layout
    ///
    /// ```text
    /// +--+--+--+--+--+--+
    /// |     |     |     |
    /// |     |     |     |
    /// |     |     |     |
    /// +--+--+--+--+--+--+
    /// ```
    Scrolling,
    // NOTE: If any new layout is added, please make sure to register the same in `DefaultLayout::cycle`
}

/// Helper to deserialize a variable-length array into a fixed [Option<f32>; MAX_RATIOS]
/// Ratios are truncated when their cumulative sum reaches or exceeds 1.0 to ensure
/// there's always remaining space for additional windows.
fn deserialize_ratios<'de, D>(
    deserializer: D,
) -> Result<Option<[Option<f32>; MAX_RATIOS]>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt: Option<Vec<f32>> = Option::deserialize(deserializer)?;
    Ok(opt.map(|vec| validate_ratios(&vec)))
}

/// Helper to serialize [Option<f32>; MAX_RATIOS] as a compact array (without trailing nulls)
fn serialize_ratios<S>(
    value: &Option<[Option<f32>; MAX_RATIOS]>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match value {
        None => serializer.serialize_none(),
        Some(arr) => {
            // Find last non-None index
            let last_idx = arr
                .iter()
                .rposition(|x| x.is_some())
                .map(|i| i + 1)
                .unwrap_or(0);
            let vec: Vec<f32> = arr.iter().take(last_idx).filter_map(|&x| x).collect();
            serializer.serialize_some(&vec)
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
/// Options for specific layouts
pub struct LayoutOptions {
    /// Options related to the Scrolling layout
    pub scrolling: Option<ScrollingLayoutOptions>,
    /// Options related to the Grid layout
    pub grid: Option<GridLayoutOptions>,
    /// Column width ratios (up to MAX_RATIOS values between 0.1 and 0.9)
    ///
    /// - Used by Columns layout: ratios for each column width
    /// - Used by Grid layout: ratios for column widths
    /// - Used by BSP, VerticalStack, RightMainVerticalStack: column_ratios[0] as primary split ratio
    /// - Used by HorizontalStack: column_ratios[0] as primary split ratio (top area height)
    /// - Used by UltrawideVerticalStack: column_ratios[0] as center ratio, column_ratios[1] as left ratio
    ///
    /// Columns without a ratio share remaining space equally.
    /// Example: `[0.3, 0.4, 0.3]` for 30%-40%-30% columns
    #[serde(
        default,
        deserialize_with = "deserialize_ratios",
        serialize_with = "serialize_ratios"
    )]
    pub column_ratios: Option<[Option<f32>; MAX_RATIOS]>,
    /// Row height ratios (up to MAX_RATIOS values between 0.1 and 0.9)
    ///
    /// - Used by Rows layout: ratios for each row height
    /// - Used by Grid layout: ratios for row heights
    ///
    /// Rows without a ratio share remaining space equally.
    /// Example: `[0.5, 0.5]` for 50%-50% rows
    #[serde(
        default,
        deserialize_with = "deserialize_ratios",
        serialize_with = "serialize_ratios"
    )]
    pub row_ratios: Option<[Option<f32>; MAX_RATIOS]>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, Eq, PartialEq)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
/// Options for the Scrolling layout
pub struct ScrollingLayoutOptions {
    /// Desired number of visible columns (default: 3)
    pub columns: usize,
    /// With an odd number of visible columns, keep the focused window column centered
    pub center_focused_column: Option<bool>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, Eq, PartialEq)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
/// Options for the Grid layout
pub struct GridLayoutOptions {
    /// Maximum number of rows per grid column
    pub rows: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
/// Per-layout default options entry for the `layout_defaults` global setting.
/// Contains both base layout options and threshold-based layout options rules.
pub struct LayoutDefaultEntry {
    /// Default layout options for this layout
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layout_options: Option<LayoutOptions>,
    /// Threshold-based layout options rules in the format of threshold => options.
    /// When container count >= threshold, the highest matching threshold's options
    /// fully replace the base `layout_options`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layout_options_rules: Option<HashMap<usize, LayoutOptions>>,
}

impl DefaultLayout {
    pub fn leftmost_index(&self, len: usize) -> usize {
        match self {
            Self::UltrawideVerticalStack | Self::RightMainVerticalStack => match len {
                n if n > 1 => 1,
                _ => 0,
            },
            Self::Scrolling => 0,
            DefaultLayout::BSP
            | DefaultLayout::Columns
            | DefaultLayout::Rows
            | DefaultLayout::VerticalStack
            | DefaultLayout::HorizontalStack
            | DefaultLayout::Grid => 0,
        }
    }

    pub fn rightmost_index(&self, len: usize) -> usize {
        match self {
            DefaultLayout::BSP
            | DefaultLayout::Columns
            | DefaultLayout::Rows
            | DefaultLayout::VerticalStack
            | DefaultLayout::HorizontalStack
            | DefaultLayout::Grid => len.saturating_sub(1),
            DefaultLayout::UltrawideVerticalStack => match len {
                2 => 0,
                _ => len.saturating_sub(1),
            },
            DefaultLayout::RightMainVerticalStack => 0,
            DefaultLayout::Scrolling => len.saturating_sub(1),
        }
    }

    #[must_use]
    #[allow(clippy::cast_precision_loss, clippy::only_used_in_recursion)]
    pub fn resize(
        &self,
        unaltered: &Rect,
        resize: &Option<Rect>,
        edge: OperationDirection,
        sizing: Sizing,
        delta: i32,
    ) -> Option<Rect> {
        if !matches!(
            self,
            Self::BSP
                | Self::Columns
                | Self::Rows
                | Self::VerticalStack
                | Self::RightMainVerticalStack
                | Self::HorizontalStack
                | Self::UltrawideVerticalStack
                | Self::Scrolling
        ) {
            return None;
        };

        let mut r = resize.unwrap_or_default();

        // Adjustments are in resize units; the default layouts apply roughly half
        // of an adjustment to the visible edge. Keep every tile at least
        // MIN_TILE_SIZE pixels wide/tall, instead of the old limit of half the
        // tile's unaltered size, which stopped mouse drags short.
        //
        // The old checks also tested a different value than the one they applied
        // for Up (Increase tested `top + delta` but applied `top - delta`), so
        // after a large top-edge resize the edge could never be moved back. And
        // a step that would cross the limit was dropped entirely instead of
        // stopping at the limit.
        //
        // Note: the limit cannot account for changes made to adjacent containers,
        // so callers that can (like mouse_drag) should also validate the result.
        const MIN_TILE_SIZE: i32 = 100;
        let width_limit = (unaltered.right - MIN_TILE_SIZE).max(0) * 2;
        let height_limit = (unaltered.bottom - MIN_TILE_SIZE).max(0) * 2;

        let change = match sizing {
            Sizing::Increase => delta,
            Sizing::Decrease => -delta,
        };

        match edge {
            // Growing to the left/up means a more negative left/top adjustment
            OperationDirection::Left => {
                r.left = bounded_adjustment(r.left, -change, width_limit);
            }
            OperationDirection::Up => {
                r.top = bounded_adjustment(r.top, -change, height_limit);
            }
            OperationDirection::Right => {
                r.right = bounded_adjustment(r.right, change, width_limit);
            }
            OperationDirection::Down => {
                r.bottom = bounded_adjustment(r.bottom, change, height_limit);
            }
        };

        if r.eq(&Rect::default()) {
            None
        } else {
            Option::from(r)
        }
    }

    #[must_use]
    pub const fn cycle_next(self) -> Self {
        match self {
            Self::BSP => Self::Columns,
            Self::Columns => Self::Rows,
            Self::Rows => Self::VerticalStack,
            Self::VerticalStack => Self::HorizontalStack,
            Self::HorizontalStack => Self::UltrawideVerticalStack,
            Self::UltrawideVerticalStack => Self::Grid,
            Self::Grid => Self::RightMainVerticalStack,
            Self::RightMainVerticalStack => Self::Scrolling,
            Self::Scrolling => Self::BSP,
        }
    }

    #[must_use]
    pub const fn cycle_previous(self) -> Self {
        match self {
            Self::Scrolling => Self::RightMainVerticalStack,
            Self::RightMainVerticalStack => Self::Grid,
            Self::Grid => Self::UltrawideVerticalStack,
            Self::UltrawideVerticalStack => Self::HorizontalStack,
            Self::HorizontalStack => Self::VerticalStack,
            Self::VerticalStack => Self::Rows,
            Self::Rows => Self::Columns,
            Self::Columns => Self::BSP,
            Self::BSP => Self::RightMainVerticalStack,
        }
    }
}

#[cfg(test)]
#[path = "default_layout_tests.rs"]
mod tests;

/// Apply `change` to a resize adjustment, clamping its magnitude to `limit`.
/// A change that moves the adjustment back towards zero is always allowed, so a
/// resize can always be undone.
const fn bounded_adjustment(current: i32, change: i32, limit: i32) -> i32 {
    let next = current + change;
    if next.abs() <= limit || next.abs() <= current.abs() {
        next
    } else if next < 0 {
        -limit
    } else {
        limit
    }
}
