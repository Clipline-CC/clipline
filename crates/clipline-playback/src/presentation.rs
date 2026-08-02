use thiserror::Error;

const MAX_SCALE_FACTOR: f32 = 16.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScaleFactor(f32);

impl ScaleFactor {
    pub fn new(value: f32) -> Result<Self, PresentationError> {
        if !value.is_finite() || value <= 0.0 || value > MAX_SCALE_FACTOR {
            return Err(PresentationError::InvalidScaleFactor(value));
        }
        Ok(Self(value))
    }

    pub const fn get(self) -> f32 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LogicalVideoRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl LogicalVideoRect {
    pub fn new(x: f32, y: f32, width: f32, height: f32) -> Result<Self, PresentationError> {
        if ![x, y, width, height].into_iter().all(f32::is_finite)
            || x < 0.0
            || y < 0.0
            || width < 0.0
            || height < 0.0
        {
            return Err(PresentationError::InvalidLogicalRectangle);
        }
        Ok(Self {
            x,
            y,
            width,
            height,
        })
    }

    pub fn to_physical(self, scale: ScaleFactor) -> Result<PhysicalVideoRect, PresentationError> {
        let scale = f64::from(scale.get());
        let left = (f64::from(self.x) * scale).floor();
        let top = (f64::from(self.y) * scale).floor();
        let right = (f64::from(self.x + self.width) * scale).ceil();
        let bottom = (f64::from(self.y + self.height) * scale).ceil();
        if left < 0.0
            || top < 0.0
            || right < left
            || bottom < top
            || right > f64::from(i32::MAX)
            || bottom > f64::from(i32::MAX)
        {
            return Err(PresentationError::PhysicalRectangleOverflow);
        }
        let left = left as i32;
        let top = top as i32;
        let right = right as i32;
        let bottom = bottom as i32;
        Ok(PhysicalVideoRect::new(
            left,
            top,
            (right - left) as u32,
            (bottom - top) as u32,
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysicalVideoRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl PhysicalVideoRect {
    pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn right(self) -> i32 {
        self.x
            .saturating_add(i32::try_from(self.width).unwrap_or(i32::MAX))
    }

    pub fn bottom(self) -> i32 {
        self.y
            .saturating_add(i32::try_from(self.height).unwrap_or(i32::MAX))
    }

    pub const fn has_area(self) -> bool {
        self.width != 0 && self.height != 0
    }
}

pub fn fit_aspect_ratio(
    bounds: PhysicalVideoRect,
    aspect_width: u32,
    aspect_height: u32,
) -> Result<PhysicalVideoRect, PresentationError> {
    if aspect_width == 0 || aspect_height == 0 {
        return Err(PresentationError::InvalidAspectRatio);
    }
    if !bounds.has_area() {
        return Ok(PhysicalVideoRect::new(bounds.x, bounds.y, 0, 0));
    }

    let height_for_full_width = u64::from(bounds.width)
        .checked_mul(u64::from(aspect_height))
        .ok_or(PresentationError::PhysicalRectangleOverflow)?
        / u64::from(aspect_width);
    let (width, height) = if height_for_full_width <= u64::from(bounds.height) {
        (u64::from(bounds.width), height_for_full_width)
    } else {
        let width = u64::from(bounds.height)
            .checked_mul(u64::from(aspect_width))
            .ok_or(PresentationError::PhysicalRectangleOverflow)?
            / u64::from(aspect_height);
        (width, u64::from(bounds.height))
    };
    let width = u32::try_from(width).map_err(|_| PresentationError::PhysicalRectangleOverflow)?;
    let height = u32::try_from(height).map_err(|_| PresentationError::PhysicalRectangleOverflow)?;
    let x_offset = i32::try_from((bounds.width - width) / 2)
        .map_err(|_| PresentationError::PhysicalRectangleOverflow)?;
    let y_offset = i32::try_from((bounds.height - height) / 2)
        .map_err(|_| PresentationError::PhysicalRectangleOverflow)?;
    Ok(PhysicalVideoRect::new(
        bounds.x.saturating_add(x_offset),
        bounds.y.saturating_add(y_offset),
        width,
        height,
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentationState {
    Visible,
    Occluded,
    Minimized,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentationUpdate {
    Unchanged {
        revision: u64,
    },
    Changed {
        revision: u64,
        release_pending_frame: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresentationLifecycle {
    revision: u64,
    geometry: Option<PhysicalVideoRect>,
    state: PresentationState,
}

impl Default for PresentationLifecycle {
    fn default() -> Self {
        Self::new()
    }
}

impl PresentationLifecycle {
    pub const fn new() -> Self {
        Self {
            revision: 0,
            geometry: None,
            state: PresentationState::Occluded,
        }
    }

    pub const fn with_revision(revision: u64) -> Self {
        Self {
            revision,
            geometry: None,
            state: PresentationState::Occluded,
        }
    }

    pub fn update(
        &mut self,
        geometry: PhysicalVideoRect,
        state: PresentationState,
    ) -> Result<PresentationUpdate, PresentationError> {
        if self.geometry == Some(geometry) && self.state == state {
            return Ok(PresentationUpdate::Unchanged {
                revision: self.revision,
            });
        }
        let revision = self
            .revision
            .checked_add(1)
            .ok_or(PresentationError::RevisionOverflow)?;
        let release_pending_frame = !is_presentable(geometry, state);
        self.revision = revision;
        self.geometry = Some(geometry);
        self.state = state;
        Ok(PresentationUpdate::Changed {
            revision,
            release_pending_frame,
        })
    }

    pub const fn latest_revision(&self) -> u64 {
        self.revision
    }

    pub const fn latest_geometry(&self) -> Option<PhysicalVideoRect> {
        self.geometry
    }

    pub fn is_presentable(&self) -> bool {
        self.geometry
            .is_some_and(|geometry| is_presentable(geometry, self.state))
    }

    pub fn accepts_revision(&self, revision: u64) -> bool {
        self.is_presentable() && revision == self.revision
    }
}

fn is_presentable(geometry: PhysicalVideoRect, state: PresentationState) -> bool {
    state == PresentationState::Visible && geometry.has_area()
}

#[derive(Debug, Clone, Copy, PartialEq, Error)]
pub enum PresentationError {
    #[error("invalid display scale factor {0}")]
    InvalidScaleFactor(f32),
    #[error("logical video rectangle must be finite and non-negative")]
    InvalidLogicalRectangle,
    #[error("physical video rectangle overflow")]
    PhysicalRectangleOverflow,
    #[error("video aspect ratio must be non-zero")]
    InvalidAspectRatio,
    #[error("presentation geometry revision overflow")]
    RevisionOverflow,
}
