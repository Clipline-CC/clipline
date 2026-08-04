//! Framework-neutral capture-region geometry.
//!
//! Coordinates and sizes in this module are physical pixels unless their type
//! explicitly says `Logical`. The math is allocation-free. The one bridge back
//! to persisted [`CaptureRegionSettings`] performs a fallible allocation only
//! when the selected display id changes.

use thiserror::Error;

use crate::validation::{MAX_CAPTURE_REGION_SIDE, MIN_CAPTURE_REGION_SIDE};
use crate::CaptureRegionSettings;

/// Matches the Milestone 8 bounded settings-field contract without requiring
/// an owned display DTO in the geometry layer.
pub const MAX_DISPLAY_ID_BYTES: usize = crate::preferences::MAX_SETTINGS_FIELD_BYTES;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum RegionGeometryError {
    #[error("display id must not be empty")]
    EmptyDisplayId,
    #[error("display id exceeds the byte limit")]
    DisplayIdTooLong,
    #[error("region side is smaller than the minimum")]
    SideTooSmall,
    #[error("region side exceeds the maximum")]
    SideTooLarge,
    #[error("region coordinate exceeds the physical coordinate range")]
    CoordinateOverflow,
    #[error("logical coordinate is not finite")]
    NonFiniteCoordinate,
    #[error("DPI scale must be finite and positive")]
    InvalidScale,
    #[error("no displays are available")]
    NoDisplays,
    #[error("display-id allocation failed")]
    AllocationFailed,
}

/// Borrowed, bounded physical geometry for one enumerated display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayGeometry<'a> {
    id: &'a str,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub is_primary: bool,
}

impl<'a> DisplayGeometry<'a> {
    pub fn new(
        id: &'a str,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        is_primary: bool,
    ) -> Result<Self, RegionGeometryError> {
        if id.trim().is_empty() {
            return Err(RegionGeometryError::EmptyDisplayId);
        }
        if id.len() > MAX_DISPLAY_ID_BYTES {
            return Err(RegionGeometryError::DisplayIdTooLong);
        }
        validate_sides(width, height)?;
        checked_far_edge(x, width)?;
        checked_far_edge(y, height)?;
        Ok(Self {
            id,
            x,
            y,
            width,
            height,
            is_primary,
        })
    }

    #[must_use]
    pub const fn id(self) -> &'a str {
        self.id
    }

    pub fn full_region(self) -> Result<RegionGeometry, RegionGeometryError> {
        RegionGeometry::new(self.x, self.y, self.width, self.height)
    }

    fn right(self) -> i64 {
        i64::from(self.x) + i64::from(self.width)
    }

    fn bottom(self) -> i64 {
        i64::from(self.y) + i64::from(self.height)
    }
}

/// Physical capture rectangle. Width and height obey persisted capture bounds;
/// x and y may be negative for displays left of or above the primary display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegionGeometry {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl RegionGeometry {
    pub fn new(x: i32, y: i32, width: u32, height: u32) -> Result<Self, RegionGeometryError> {
        validate_sides(width, height)?;
        Ok(Self {
            x,
            y,
            width,
            height,
        })
    }

    pub fn from_settings(settings: &CaptureRegionSettings) -> Result<Self, RegionGeometryError> {
        Self::new(settings.x, settings.y, settings.width, settings.height)
    }

    /// Apply the same even-size and containment rule as the retained JavaScript
    /// editor. Position is clamped only after the normalized size is known.
    pub fn clamp_to(self, display: DisplayGeometry<'_>) -> Result<Self, RegionGeometryError> {
        let width = even_size(i64::from(self.width), display.width);
        let height = even_size(i64::from(self.height), display.height);
        let max_x = display.right() - i64::from(width);
        let max_y = display.bottom() - i64::from(height);
        let x = i64::from(self.x).clamp(i64::from(display.x), max_x);
        let y = i64::from(self.y).clamp(i64::from(display.y), max_y);
        Self::from_i64(x, y, width, height)
    }

    pub fn apply(
        self,
        display: DisplayGeometry<'_>,
        action: RegionAction,
    ) -> Result<Self, RegionGeometryError> {
        match action {
            RegionAction::MoveBy { dx, dy } => {
                let normalized = self.clamp_to(display)?;
                let next_x = i64::from(normalized.x) + i64::from(dx);
                let next_y = i64::from(normalized.y) + i64::from(dy);
                normalized.with_position_clamped(next_x, next_y, display)
            }
            RegionAction::ResizeBy {
                width_delta,
                height_delta,
            } => {
                let width = even_size(
                    i64::from(self.width) + i64::from(width_delta),
                    display.width,
                );
                let height = even_size(
                    i64::from(self.height) + i64::from(height_delta),
                    display.height,
                );
                Self::from_i64(i64::from(self.x), i64::from(self.y), width, height)?
                    .clamp_to(display)
            }
            RegionAction::Align(align) => self.align_to(display, align),
            RegionAction::FullDisplay => display.full_region(),
        }
    }

    #[must_use]
    pub const fn resolve(self, display: DisplayGeometry<'_>) -> ResolvedCaptureRegion<'_> {
        ResolvedCaptureRegion {
            display,
            region: self,
        }
    }

    fn align_to(
        self,
        display: DisplayGeometry<'_>,
        align: Align,
    ) -> Result<Self, RegionGeometryError> {
        let mut next = self.clamp_to(display)?;
        match align {
            Align::Left => next.x = display.x,
            Align::Right => {
                next.x = i32::try_from(display.right() - i64::from(next.width))
                    .map_err(|_| RegionGeometryError::CoordinateOverflow)?;
            }
            Align::Top => next.y = display.y,
            Align::Bottom => {
                next.y = i32::try_from(display.bottom() - i64::from(next.height))
                    .map_err(|_| RegionGeometryError::CoordinateOverflow)?;
            }
            Align::Center => {
                let horizontal_gap = display.width - next.width;
                let vertical_gap = display.height - next.height;
                // JavaScript `Math.round(integer + gap / 2)` rounds a .5 tie
                // toward positive infinity, hence the ceiling halves here.
                let x = i64::from(display.x) + i64::from(horizontal_gap.div_ceil(2));
                let y = i64::from(display.y) + i64::from(vertical_gap.div_ceil(2));
                next.x = i32::try_from(x).map_err(|_| RegionGeometryError::CoordinateOverflow)?;
                next.y = i32::try_from(y).map_err(|_| RegionGeometryError::CoordinateOverflow)?;
            }
        }
        Ok(next)
    }

    fn with_position_clamped(
        self,
        x: i64,
        y: i64,
        display: DisplayGeometry<'_>,
    ) -> Result<Self, RegionGeometryError> {
        let normalized = self.clamp_to(display)?;
        let max_x = display.right() - i64::from(normalized.width);
        let max_y = display.bottom() - i64::from(normalized.height);
        Self::from_i64(
            x.clamp(i64::from(display.x), max_x),
            y.clamp(i64::from(display.y), max_y),
            normalized.width,
            normalized.height,
        )
    }

    fn from_i64(x: i64, y: i64, width: u32, height: u32) -> Result<Self, RegionGeometryError> {
        let x = i32::try_from(x).map_err(|_| RegionGeometryError::CoordinateOverflow)?;
        let y = i32::try_from(y).map_err(|_| RegionGeometryError::CoordinateOverflow)?;
        Self::new(x, y, width, height)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
    Top,
    Bottom,
    Center,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionAction {
    MoveBy { dx: i32, dy: i32 },
    ResizeBy { width_delta: i32, height_delta: i32 },
    Align(Align),
    FullDisplay,
}

/// Region paired with the exact display selected after current-id, primary,
/// then first-display fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedCaptureRegion<'a> {
    pub display: DisplayGeometry<'a>,
    pub region: RegionGeometry,
}

impl ResolvedCaptureRegion<'_> {
    /// Apply geometry to persistence without partial mutation. An unchanged
    /// display id reuses its existing allocation.
    pub fn apply_to_settings(
        self,
        settings: &mut CaptureRegionSettings,
    ) -> Result<(), RegionGeometryError> {
        if settings.display_id.as_deref() != Some(self.display.id()) {
            let mut display_id = String::new();
            display_id
                .try_reserve_exact(self.display.id().len())
                .map_err(|_| RegionGeometryError::AllocationFailed)?;
            display_id.push_str(self.display.id());
            settings.display_id = Some(display_id);
        }
        settings.x = self.region.x;
        settings.y = self.region.y;
        settings.width = self.region.width;
        settings.height = self.region.height;
        Ok(())
    }
}

/// Resolve persisted selection after enumeration changes. Missing ids fall
/// back to the primary display, then the first enumerated display, matching the
/// retained Settings UI. The returned region is clamped to the selected display.
pub fn resolve_display<'a>(
    settings: &CaptureRegionSettings,
    displays: &'a [DisplayGeometry<'a>],
) -> Result<ResolvedCaptureRegion<'a>, RegionGeometryError> {
    let selected = settings
        .display_id
        .as_deref()
        .and_then(|id| displays.iter().copied().find(|display| display.id() == id))
        .or_else(|| displays.iter().copied().find(|display| display.is_primary))
        .or_else(|| displays.first().copied())
        .ok_or(RegionGeometryError::NoDisplays)?;
    let region = RegionGeometry::from_settings(settings)?.clamp_to(selected)?;
    Ok(region.resolve(selected))
}

/// Validated logical-to-physical scale (for example, 1.25 at 125%).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DpiScale(f64);

impl DpiScale {
    pub fn new(value: f64) -> Result<Self, RegionGeometryError> {
        if !value.is_finite() || value <= 0.0 {
            Err(RegionGeometryError::InvalidScale)
        } else {
            Ok(Self(value))
        }
    }

    #[must_use]
    pub const fn get(self) -> f64 {
        self.0
    }

    pub fn logical_to_physical_point(
        self,
        point: LogicalPoint,
    ) -> Result<PhysicalPoint, RegionGeometryError> {
        validate_finite(point.x)?;
        validate_finite(point.y)?;
        Ok(PhysicalPoint {
            x: checked_round_i32(point.x * self.0)?,
            y: checked_round_i32(point.y * self.0)?,
        })
    }

    pub fn logical_to_physical_size(
        self,
        size: LogicalSize,
    ) -> Result<PhysicalSize, RegionGeometryError> {
        validate_logical_side(size.width)?;
        validate_logical_side(size.height)?;
        Ok(PhysicalSize {
            width: checked_round_side(size.width * self.0)?,
            height: checked_round_side(size.height * self.0)?,
        })
    }

    pub fn physical_to_logical_point(
        self,
        point: PhysicalPoint,
    ) -> Result<LogicalPoint, RegionGeometryError> {
        LogicalPoint::new(f64::from(point.x) / self.0, f64::from(point.y) / self.0)
            .map_err(|_| RegionGeometryError::CoordinateOverflow)
    }

    pub fn physical_to_logical_size(
        self,
        size: PhysicalSize,
    ) -> Result<LogicalSize, RegionGeometryError> {
        validate_sides(size.width, size.height)?;
        LogicalSize::new(
            f64::from(size.width) / self.0,
            f64::from(size.height) / self.0,
        )
        .map_err(|error| match error {
            RegionGeometryError::NonFiniteCoordinate => RegionGeometryError::SideTooLarge,
            other => other,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LogicalPoint {
    pub x: f64,
    pub y: f64,
}

impl LogicalPoint {
    pub fn new(x: f64, y: f64) -> Result<Self, RegionGeometryError> {
        validate_finite(x)?;
        validate_finite(y)?;
        Ok(Self { x, y })
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LogicalSize {
    pub width: f64,
    pub height: f64,
}

impl LogicalSize {
    pub fn new(width: f64, height: f64) -> Result<Self, RegionGeometryError> {
        validate_logical_side(width)?;
        validate_logical_side(height)?;
        Ok(Self { width, height })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysicalPoint {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysicalSize {
    pub width: u32,
    pub height: u32,
}

fn validate_sides(width: u32, height: u32) -> Result<(), RegionGeometryError> {
    if width < MIN_CAPTURE_REGION_SIDE || height < MIN_CAPTURE_REGION_SIDE {
        Err(RegionGeometryError::SideTooSmall)
    } else if width > MAX_CAPTURE_REGION_SIDE || height > MAX_CAPTURE_REGION_SIDE {
        Err(RegionGeometryError::SideTooLarge)
    } else {
        Ok(())
    }
}

fn checked_far_edge(origin: i32, side: u32) -> Result<i32, RegionGeometryError> {
    let edge = i64::from(origin) + i64::from(side);
    i32::try_from(edge).map_err(|_| RegionGeometryError::CoordinateOverflow)
}

fn even_size(value: i64, maximum: u32) -> u32 {
    let clamped = value.clamp(i64::from(MIN_CAPTURE_REGION_SIDE), i64::from(maximum));
    let clamped = u32::try_from(clamped).expect("bounded side fits u32");
    if clamped.is_multiple_of(2) {
        clamped
    } else {
        (clamped - 1).max(MIN_CAPTURE_REGION_SIDE)
    }
}

fn validate_finite(value: f64) -> Result<(), RegionGeometryError> {
    if value.is_finite() {
        Ok(())
    } else {
        Err(RegionGeometryError::NonFiniteCoordinate)
    }
}

fn validate_logical_side(value: f64) -> Result<(), RegionGeometryError> {
    validate_finite(value)?;
    if value <= 0.0 {
        Err(RegionGeometryError::SideTooSmall)
    } else {
        Ok(())
    }
}

fn checked_round_i32(value: f64) -> Result<i32, RegionGeometryError> {
    if !value.is_finite() {
        return Err(RegionGeometryError::CoordinateOverflow);
    }
    // Match JavaScript Math.round for both positive and negative coordinates.
    let rounded = (value + 0.5).floor();
    if rounded < f64::from(i32::MIN) || rounded > f64::from(i32::MAX) {
        Err(RegionGeometryError::CoordinateOverflow)
    } else {
        Ok(rounded as i32)
    }
}

fn checked_round_side(value: f64) -> Result<u32, RegionGeometryError> {
    if !value.is_finite() {
        return Err(RegionGeometryError::SideTooLarge);
    }
    let rounded = (value + 0.5).floor();
    if rounded < f64::from(MIN_CAPTURE_REGION_SIDE) {
        Err(RegionGeometryError::SideTooSmall)
    } else if rounded > f64::from(MAX_CAPTURE_REGION_SIDE) {
        Err(RegionGeometryError::SideTooLarge)
    } else {
        Ok(rounded as u32)
    }
}
