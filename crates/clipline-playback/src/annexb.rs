use clipline_mp4::PlaybackTrackConfig;
use thiserror::Error;

use crate::WorkGeneration;

/// More than 2.5 seconds of data at Clipline's reviewed 100 Mbps bitrate ceiling.
pub const MAX_ENCODED_VIDEO_SAMPLE_BYTES: usize = 32 * 1024 * 1024;
/// Separate expansion cap for start codes plus decoder configuration parameter sets.
pub const MAX_ANNEX_B_ACCESS_UNIT_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnnexBLimits {
    max_encoded_sample_bytes: usize,
    max_access_unit_bytes: usize,
}

impl AnnexBLimits {
    pub fn new(
        max_encoded_sample_bytes: usize,
        max_access_unit_bytes: usize,
    ) -> Result<Self, AnnexBError> {
        if max_encoded_sample_bytes == 0
            || max_access_unit_bytes == 0
            || max_encoded_sample_bytes > MAX_ENCODED_VIDEO_SAMPLE_BYTES
            || max_access_unit_bytes > MAX_ANNEX_B_ACCESS_UNIT_BYTES
        {
            return Err(AnnexBError::InvalidLimits {
                encoded: max_encoded_sample_bytes,
                converted: max_access_unit_bytes,
            });
        }
        Ok(Self {
            max_encoded_sample_bytes,
            max_access_unit_bytes,
        })
    }

    pub fn max_encoded_sample_bytes(self) -> usize {
        self.max_encoded_sample_bytes
    }

    pub fn max_access_unit_bytes(self) -> usize {
        self.max_access_unit_bytes
    }
}

impl Default for AnnexBLimits {
    fn default() -> Self {
        Self {
            max_encoded_sample_bytes: MAX_ENCODED_VIDEO_SAMPLE_BYTES,
            max_access_unit_bytes: MAX_ANNEX_B_ACCESS_UNIT_BYTES,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct H264DecoderConfig {
    width: u16,
    height: u16,
    nal_length_size: u8,
    sps: Vec<Vec<u8>>,
    pps: Vec<Vec<u8>>,
}

impl H264DecoderConfig {
    pub fn new(
        width: u16,
        height: u16,
        nal_length_size: u8,
        sps: Vec<Vec<u8>>,
        pps: Vec<Vec<u8>>,
    ) -> Result<Self, AnnexBError> {
        validate_h264_parts(
            width,
            height,
            nal_length_size,
            &sps,
            &pps,
            AnnexBLimits::default(),
        )?;
        Ok(Self {
            width,
            height,
            nal_length_size,
            sps,
            pps,
        })
    }

    pub(crate) fn from_playback_config(
        config: &PlaybackTrackConfig,
        limits: AnnexBLimits,
    ) -> Result<Self, AnnexBError> {
        let PlaybackTrackConfig::H264 {
            width,
            height,
            nal_length_size,
            sps,
            pps,
        } = config
        else {
            return match config {
                PlaybackTrackConfig::Hevc { .. } => {
                    Err(AnnexBError::UnsupportedCodec(UnsupportedVideoCodec::Hevc))
                }
                PlaybackTrackConfig::Av1 { .. } => {
                    Err(AnnexBError::UnsupportedCodec(UnsupportedVideoCodec::Av1))
                }
                PlaybackTrackConfig::Opus { .. } => Err(AnnexBError::NotVideoTrack),
                PlaybackTrackConfig::H264 { .. } => unreachable!(),
            };
        };

        // Validate by reference before cloning attacker-controlled decoder configuration.
        validate_h264_parts(*width, *height, *nal_length_size, sps, pps, limits)?;
        Ok(Self {
            width: *width,
            height: *height,
            nal_length_size: *nal_length_size,
            sps: sps.clone(),
            pps: pps.clone(),
        })
    }

    pub fn width(&self) -> u16 {
        self.width
    }

    pub fn height(&self) -> u16 {
        self.height
    }

    pub fn nal_length_size(&self) -> u8 {
        self.nal_length_size
    }

    pub fn sequence_parameter_sets(&self) -> &[Vec<u8>] {
        &self.sps
    }

    pub fn picture_parameter_sets(&self) -> &[Vec<u8>] {
        &self.pps
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsupportedVideoCodec {
    Hevc,
    Av1,
}

impl std::fmt::Display for UnsupportedVideoCodec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Hevc => f.write_str("HEVC"),
            Self::Av1 => f.write_str("AV1"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeVideoCapability {
    H264(H264DecoderConfig),
    Unsupported(UnsupportedVideoCodec),
    NotVideo,
}

impl NativeVideoCapability {
    pub fn inspect(config: &PlaybackTrackConfig) -> Result<Self, AnnexBError> {
        match config {
            PlaybackTrackConfig::H264 { .. } => Ok(Self::H264(
                H264DecoderConfig::from_playback_config(config, AnnexBLimits::default())?,
            )),
            PlaybackTrackConfig::Hevc { .. } => Ok(Self::Unsupported(UnsupportedVideoCodec::Hevc)),
            PlaybackTrackConfig::Av1 { .. } => Ok(Self::Unsupported(UnsupportedVideoCodec::Av1)),
            PlaybackTrackConfig::Opus { .. } => Ok(Self::NotVideo),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParameterSetSubmission {
    generation: WorkGeneration,
    id: u64,
}

impl ParameterSetSubmission {
    pub fn generation(self) -> WorkGeneration {
        self.generation
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ConvertedAnnexB<'a> {
    pub bytes: &'a [u8],
    pub parameter_set_submission: Option<ParameterSetSubmission>,
}

#[derive(Debug, Error)]
pub enum AnnexBError {
    #[error("invalid Annex B limits: encoded={encoded}, converted={converted}")]
    InvalidLimits { encoded: usize, converted: usize },
    #[error("H.264 dimensions must be non-zero, not {width}x{height}")]
    InvalidDimensions { width: u16, height: u16 },
    #[error("H.264 NAL length size must be 1, 2, or 4 bytes, not {0}")]
    InvalidNalLengthSize(u8),
    #[error("H.264 decoder configuration requires at least one SPS and PPS")]
    MissingParameterSets,
    #[error("H.264 decoder configuration contains an empty {kind} at index {index}")]
    EmptyParameterSet { kind: &'static str, index: usize },
    #[error("H.264 {kind} {index} has NAL type {actual}, expected {expected}")]
    UnexpectedParameterSetType {
        kind: &'static str,
        index: usize,
        expected: u8,
        actual: u8,
    },
    #[error("encoded video sample is empty")]
    EmptySample,
    #[error("NAL length at byte {offset} needs {needed} bytes, but only {remaining} remain")]
    TruncatedLength {
        offset: usize,
        needed: usize,
        remaining: usize,
    },
    #[error("NAL at byte {offset} declares zero bytes")]
    ZeroLengthNal { offset: usize },
    #[error("NAL at byte {offset} declares {declared} bytes, but only {remaining} remain")]
    NalLengthExceedsSample {
        offset: usize,
        declared: usize,
        remaining: usize,
    },
    #[error("encoded video sample has {count} trailing bytes")]
    TrailingBytes { count: usize },
    #[error("encoded video sample is {size} bytes, above the {limit}-byte cap")]
    EncodedSampleTooLarge { size: usize, limit: usize },
    #[error("Annex B access unit requires {required} bytes, above the {limit}-byte cap")]
    OutputTooLarge { required: usize, limit: usize },
    #[error("could not reserve {requested} bytes for a bounded playback buffer")]
    AllocationFailed { requested: usize },
    #[error("{0} playback is not supported by the native milestone")]
    UnsupportedCodec(UnsupportedVideoCodec),
    #[error("selected playback track is not video")]
    NotVideoTrack,
    #[error("stale sample generation {actual:?}; active generation is {active:?}")]
    StaleGeneration {
        active: WorkGeneration,
        actual: WorkGeneration,
    },
    #[error("parameter-set submission counter is exhausted")]
    SubmissionExhausted,
    #[error("loaded video sample counter is exhausted")]
    LoadedSampleCounterExhausted,
    #[error("loaded video sample token {loaded_id} was superseded by {current_id:?}")]
    LoadedSampleSuperseded {
        loaded_id: u64,
        current_id: Option<u64>,
    },
    #[error(transparent)]
    Index(#[from] clipline_mp4::PlaybackIndexError),
}

pub struct H264AnnexBConverter {
    config: H264DecoderConfig,
    limits: AnnexBLimits,
    active_generation: WorkGeneration,
    parameter_sets_committed: bool,
    prepared_submission: Option<ParameterSetSubmission>,
    next_submission_id: u64,
    output: Vec<u8>,
    high_water: usize,
    reserve_count: usize,
}

impl H264AnnexBConverter {
    pub fn new(
        config: H264DecoderConfig,
        limits: AnnexBLimits,
        generation: WorkGeneration,
    ) -> Result<Self, AnnexBError> {
        validate_h264_parts(
            config.width,
            config.height,
            config.nal_length_size,
            &config.sps,
            &config.pps,
            limits,
        )?;
        Ok(Self {
            config,
            limits,
            active_generation: generation,
            parameter_sets_committed: false,
            prepared_submission: None,
            next_submission_id: 1,
            output: Vec::new(),
            high_water: 0,
            reserve_count: 0,
        })
    }

    pub fn convert(
        &mut self,
        sample: &[u8],
        is_sync: bool,
        generation: WorkGeneration,
    ) -> Result<ConvertedAnnexB<'_>, AnnexBError> {
        if generation != self.active_generation {
            return Err(AnnexBError::StaleGeneration {
                active: self.active_generation,
                actual: generation,
            });
        }
        self.output.clear();
        self.prepared_submission = None;
        let result = self.convert_inner(sample, is_sync, generation);
        if result.is_err() {
            self.output.clear();
            self.prepared_submission = None;
        }
        result?;
        Ok(ConvertedAnnexB {
            bytes: &self.output,
            parameter_set_submission: self.prepared_submission,
        })
    }

    fn convert_inner(
        &mut self,
        sample: &[u8],
        is_sync: bool,
        generation: WorkGeneration,
    ) -> Result<(), AnnexBError> {
        if sample.is_empty() {
            return Err(AnnexBError::EmptySample);
        }
        if sample.len() > self.limits.max_encoded_sample_bytes {
            return Err(AnnexBError::EncodedSampleTooLarge {
                size: sample.len(),
                limit: self.limits.max_encoded_sample_bytes,
            });
        }

        let sample_output_size = validate_sample(sample, self.config.nal_length_size as usize)?;
        let inject_parameter_sets = is_sync && !self.parameter_sets_committed;
        let parameter_set_size = if inject_parameter_sets {
            parameter_set_output_size(&self.config.sps, &self.config.pps)?
        } else {
            0
        };
        let required = parameter_set_size.checked_add(sample_output_size).ok_or(
            AnnexBError::OutputTooLarge {
                required: usize::MAX,
                limit: self.limits.max_access_unit_bytes,
            },
        )?;
        if required > self.limits.max_access_unit_bytes {
            return Err(AnnexBError::OutputTooLarge {
                required,
                limit: self.limits.max_access_unit_bytes,
            });
        }

        if required > self.output.capacity() {
            self.output
                .try_reserve_exact(required)
                .map_err(|_| AnnexBError::AllocationFailed {
                    requested: required,
                })?;
            self.reserve_count += 1;
        }
        if inject_parameter_sets {
            for parameter_set in self.config.sps.iter().chain(&self.config.pps) {
                push_nal(&mut self.output, parameter_set);
            }
        }
        append_sample(
            &mut self.output,
            sample,
            self.config.nal_length_size as usize,
        );
        self.high_water = self.high_water.max(self.output.len());
        if inject_parameter_sets {
            let submission = ParameterSetSubmission {
                generation,
                id: self.next_submission_id,
            };
            self.next_submission_id = self
                .next_submission_id
                .checked_add(1)
                .ok_or(AnnexBError::SubmissionExhausted)?;
            self.prepared_submission = Some(submission);
        }
        Ok(())
    }

    pub fn commit_parameter_sets(&mut self, submission: ParameterSetSubmission) -> bool {
        if submission.generation != self.active_generation
            || self.prepared_submission != Some(submission)
        {
            return false;
        }
        self.parameter_sets_committed = true;
        self.prepared_submission = None;
        true
    }

    pub fn reset_for_generation(&mut self, generation: WorkGeneration) {
        self.active_generation = generation;
        self.parameter_sets_committed = false;
        self.prepared_submission = None;
        self.output.clear();
    }

    pub fn active_generation(&self) -> WorkGeneration {
        self.active_generation
    }

    pub fn parameter_sets_pending(&self) -> bool {
        !self.parameter_sets_committed
    }

    pub fn output_len(&self) -> usize {
        self.output.len()
    }

    pub fn output_capacity(&self) -> usize {
        self.output.capacity()
    }

    pub fn high_water(&self) -> usize {
        self.high_water
    }

    pub fn reserve_count(&self) -> usize {
        self.reserve_count
    }
}

fn validate_h264_parts(
    width: u16,
    height: u16,
    nal_length_size: u8,
    sps: &[Vec<u8>],
    pps: &[Vec<u8>],
    limits: AnnexBLimits,
) -> Result<(), AnnexBError> {
    if width == 0 || height == 0 {
        return Err(AnnexBError::InvalidDimensions { width, height });
    }
    if !matches!(nal_length_size, 1 | 2 | 4) {
        return Err(AnnexBError::InvalidNalLengthSize(nal_length_size));
    }
    if sps.is_empty() || pps.is_empty() {
        return Err(AnnexBError::MissingParameterSets);
    }
    validate_parameter_sets(sps, "SPS", 7)?;
    validate_parameter_sets(pps, "PPS", 8)?;
    let parameter_set_size = parameter_set_output_size(sps, pps)?;
    if parameter_set_size > limits.max_access_unit_bytes {
        return Err(AnnexBError::OutputTooLarge {
            required: parameter_set_size,
            limit: limits.max_access_unit_bytes,
        });
    }
    Ok(())
}

fn validate_parameter_sets(
    parameter_sets: &[Vec<u8>],
    kind: &'static str,
    expected_type: u8,
) -> Result<(), AnnexBError> {
    for (index, parameter_set) in parameter_sets.iter().enumerate() {
        let Some(first_byte) = parameter_set.first() else {
            return Err(AnnexBError::EmptyParameterSet { kind, index });
        };
        let actual_type = first_byte & 0x1f;
        if actual_type != expected_type {
            return Err(AnnexBError::UnexpectedParameterSetType {
                kind,
                index,
                expected: expected_type,
                actual: actual_type,
            });
        }
    }
    Ok(())
}

fn parameter_set_output_size(sps: &[Vec<u8>], pps: &[Vec<u8>]) -> Result<usize, AnnexBError> {
    sps.iter()
        .chain(pps)
        .try_fold(0_usize, |size, parameter_set| {
            size.checked_add(4)
                .and_then(|size| size.checked_add(parameter_set.len()))
                .ok_or(AnnexBError::OutputTooLarge {
                    required: usize::MAX,
                    limit: MAX_ANNEX_B_ACCESS_UNIT_BYTES,
                })
        })
}

fn validate_sample(sample: &[u8], nal_length_size: usize) -> Result<usize, AnnexBError> {
    let mut offset = 0;
    let mut output_size = 0_usize;
    let mut nal_count = 0_usize;
    while offset < sample.len() {
        let remaining = sample.len() - offset;
        if remaining < nal_length_size {
            return if nal_count == 0 {
                Err(AnnexBError::TruncatedLength {
                    offset,
                    needed: nal_length_size,
                    remaining,
                })
            } else {
                Err(AnnexBError::TrailingBytes { count: remaining })
            };
        }
        let length_offset = offset;
        let mut nal_length = 0_usize;
        for byte in &sample[offset..offset + nal_length_size] {
            nal_length = (nal_length << 8) | usize::from(*byte);
        }
        offset += nal_length_size;
        if nal_length == 0 {
            return Err(AnnexBError::ZeroLengthNal {
                offset: length_offset,
            });
        }
        let remaining = sample.len() - offset;
        if nal_length > remaining {
            return Err(AnnexBError::NalLengthExceedsSample {
                offset: length_offset,
                declared: nal_length,
                remaining,
            });
        }
        output_size = output_size
            .checked_add(4)
            .and_then(|size| size.checked_add(nal_length))
            .ok_or(AnnexBError::OutputTooLarge {
                required: usize::MAX,
                limit: MAX_ANNEX_B_ACCESS_UNIT_BYTES,
            })?;
        offset += nal_length;
        nal_count += 1;
    }
    Ok(output_size)
}

fn append_sample(output: &mut Vec<u8>, sample: &[u8], nal_length_size: usize) {
    let mut offset = 0;
    while offset < sample.len() {
        let mut nal_length = 0_usize;
        for byte in &sample[offset..offset + nal_length_size] {
            nal_length = (nal_length << 8) | usize::from(*byte);
        }
        offset += nal_length_size;
        push_nal(output, &sample[offset..offset + nal_length]);
        offset += nal_length;
    }
}

fn push_nal(output: &mut Vec<u8>, nal: &[u8]) {
    output.extend_from_slice(&[0, 0, 0, 1]);
    output.extend_from_slice(nal);
}
