use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use clipline_buffer::{DiskReplayRing, DiskSegment, ReplayRing, Segment};

use crate::traits::{CaptureError, EncodeError};

pub(crate) const MAX_PENDING_GOP_BYTES: usize = 64 * 1024 * 1024;
/// Normal replay GOPs are about 500 ms. This generous ceiling prevents a
/// broken encoder from retaining an arbitrarily long video/audio segment.
pub(crate) const MAX_PENDING_GOP_DURATION_S: f64 = 10.0;
pub(crate) const FULL_SESSION_QUEUE_MAX_BYTES: usize = 128 * 1024 * 1024;
pub(crate) const FULL_SESSION_QUEUE_MAX_SEGMENTS: usize = 8;
pub(crate) const MID_STREAM_REPLAY_OPUS_PRE_SKIP: u16 = 960; // One 20 ms Opus frame at 48 kHz.

#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    #[error(transparent)]
    Capture(#[from] CaptureError),
    #[error(transparent)]
    Encode(#[from] EncodeError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Where the rolling replay buffer lives, and how much it retains.
///
/// `retention_s` bounds retention by wall-clock span as well as bytes. The byte
/// budget carries an encoder-overshoot headroom, so on its own it lets the
/// buffer grow to the whole budget — roughly twice the intended span, and
/// further when the encoder undershoots. `f64::INFINITY` disables the bound.
#[derive(Debug)]
pub enum ReplayStorageConfig {
    Memory {
        max_bytes: usize,
        retention_s: f64,
    },
    Disk {
        max_bytes: usize,
        retention_s: f64,
        dir: PathBuf,
    },
}

pub(crate) enum ReplayStorage {
    Memory(ReplayRing),
    Disk(DiskReplayRing),
}

pub(crate) enum ReplayWindow<'a> {
    Memory(Vec<&'a Segment>),
    Disk(Vec<&'a DiskSegment>),
}

impl ReplayWindow<'_> {
    pub(crate) fn bounds(&self) -> Option<(f64, f64)> {
        match self {
            Self::Memory(segments) => {
                Some((segments.first()?.pts_start_s, segments.last()?.pts_end_s()))
            }
            Self::Disk(segments) => {
                Some((segments.first()?.pts_start_s, segments.last()?.pts_end_s()))
            }
        }
    }

    pub(crate) fn bytes(&self) -> usize {
        match self {
            Self::Memory(segments) => segments.iter().map(|segment| segment.byte_len()).sum(),
            Self::Disk(segments) => segments.iter().map(|segment| segment.byte_len()).sum(),
        }
    }
}

impl ReplayStorage {
    pub(crate) fn len(&self) -> usize {
        match self {
            Self::Memory(ring) => ring.len(),
            Self::Disk(ring) => ring.len(),
        }
    }

    pub(crate) fn bytes(&self) -> usize {
        match self {
            Self::Memory(ring) => ring.bytes(),
            Self::Disk(ring) => ring.bytes(),
        }
    }

    pub(crate) fn buffered_span_s(&self) -> f64 {
        match self {
            Self::Memory(ring) => segment_span(
                ring.segments()
                    .map(|segment| (segment.pts_start_s, segment.pts_end_s())),
            ),
            Self::Disk(ring) => segment_span(
                ring.segments()
                    .map(|segment| (segment.pts_start_s, segment.pts_end_s())),
            ),
        }
    }

    pub(crate) fn save_window_bounds(
        &self,
        window_s: f64,
        exclude_before_s: Option<f64>,
    ) -> Option<(f64, f64)> {
        self.save_window(window_s, exclude_before_s).bounds()
    }

    pub(crate) fn save_window_bytes(&self, window_s: f64, exclude_before_s: Option<f64>) -> usize {
        self.save_window(window_s, exclude_before_s).bytes()
    }

    pub(crate) fn save_window(&self, window_s: f64, exclude_before_s: Option<f64>) -> ReplayWindow<'_> {
        match self {
            Self::Memory(ring) => {
                ReplayWindow::Memory(ring.save_window(window_s, exclude_before_s))
            }
            Self::Disk(ring) => ReplayWindow::Disk(ring.save_window(window_s, exclude_before_s)),
        }
    }

    pub(crate) fn push(&mut self, segment: Arc<Segment>) -> io::Result<()> {
        match self {
            Self::Memory(ring) => ring.push_shared(segment),
            Self::Disk(ring) => ring.push_ref(&segment)?,
        }
        Ok(())
    }
}

fn segment_span(mut segments: impl Iterator<Item = (f64, f64)>) -> f64 {
    let Some((first_start, first_end)) = segments.next() else {
        return 0.0;
    };
    segments.fold(first_end - first_start, |_, (_, end)| end - first_start)
}

pub(crate) fn pending_byte_budget(max_buffer_bytes: usize) -> usize {
    max_buffer_bytes.clamp(1, MAX_PENDING_GOP_BYTES)
}
