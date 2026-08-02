use std::io::{Read, Seek};

use clipline_mp4::{
    IndexedMovie, MovieIndex, PlaybackIndexError, PlaybackTime, SeekPlan, TrackIndex,
};

use crate::annexb::{
    AnnexBError, AnnexBLimits, H264AnnexBConverter, H264DecoderConfig, ParameterSetSubmission,
};
use crate::WorkGeneration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoSampleBufferPlan {
    pub max_encoded_sample_size: usize,
    pub config: H264DecoderConfig,
}

pub fn plan_video_sample_buffers(
    track: &TrackIndex,
    limits: AnnexBLimits,
) -> Result<VideoSampleBufferPlan, AnnexBError> {
    let max_encoded_sample_size = track
        .samples
        .iter()
        .map(|sample| sample.size as usize)
        .max()
        .unwrap_or(0);
    if max_encoded_sample_size > limits.max_encoded_sample_bytes() {
        return Err(AnnexBError::EncodedSampleTooLarge {
            size: max_encoded_sample_size,
            limit: limits.max_encoded_sample_bytes(),
        });
    }

    // This validates parameter-set sizes/types by reference before cloning them.
    let config = H264DecoderConfig::from_playback_config(&track.config, limits)?;
    Ok(VideoSampleBufferPlan {
        max_encoded_sample_size,
        config,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SampleBufferTelemetry {
    pub encoded_capacity: usize,
    pub converted_capacity: usize,
    pub encoded_high_water: usize,
    pub converted_high_water: usize,
    pub encoded_reserve_count: usize,
    pub converted_reserve_count: usize,
    pub encoded_logical_limit: usize,
    pub converted_logical_limit: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub struct VideoAccessUnit<'a> {
    pub bytes: &'a [u8],
    pub sample_index: usize,
    pub encoded_size: usize,
    pub dts: u64,
    pub pts: i64,
    pub duration: u32,
    pub is_sync: bool,
    pub generation: WorkGeneration,
    pub parameter_set_submission: Option<ParameterSetSubmission>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadedVideoSample {
    id: u64,
    sample_index: usize,
    encoded_size: usize,
    dts: u64,
    pts: i64,
    duration: u32,
    is_sync: bool,
    generation: WorkGeneration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConvertedVideoSample {
    id: u64,
    sample_index: usize,
    encoded_size: usize,
    dts: u64,
    pts: i64,
    duration: u32,
    is_sync: bool,
    generation: WorkGeneration,
    parameter_set_submission: Option<ParameterSetSubmission>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoStepTarget {
    pub time: PlaybackTime,
    pub sample_index: usize,
}

impl ConvertedVideoSample {
    pub const fn sample_index(self) -> usize {
        self.sample_index
    }

    pub const fn generation(self) -> WorkGeneration {
        self.generation
    }
}

impl LoadedVideoSample {
    pub const fn sample_index(self) -> usize {
        self.sample_index
    }

    pub const fn encoded_size(self) -> usize {
        self.encoded_size
    }

    pub const fn generation(self) -> WorkGeneration {
        self.generation
    }
}

pub struct VideoSampleTransport<R: Read + Seek> {
    movie: IndexedMovie<R>,
    track_index: usize,
    limits: AnnexBLimits,
    encoded: Vec<u8>,
    encoded_high_water: usize,
    encoded_reserve_count: usize,
    next_loaded_id: u64,
    loaded_sample_id: Option<u64>,
    next_converted_id: u64,
    converted_sample_id: Option<u64>,
    converter: H264AnnexBConverter,
}

impl<R: Read + Seek> std::fmt::Debug for VideoSampleTransport<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoSampleTransport")
            .field("track_index", &self.track_index)
            .field("telemetry", &self.buffer_telemetry())
            .finish_non_exhaustive()
    }
}

impl<R: Read + Seek> VideoSampleTransport<R> {
    pub fn new(
        movie: IndexedMovie<R>,
        track_index: usize,
        generation: WorkGeneration,
    ) -> Result<Self, AnnexBError> {
        Self::with_limits(movie, track_index, AnnexBLimits::default(), generation)
    }

    pub fn with_limits(
        movie: IndexedMovie<R>,
        track_index: usize,
        limits: AnnexBLimits,
        generation: WorkGeneration,
    ) -> Result<Self, AnnexBError> {
        let track = movie.index().tracks.get(track_index).ok_or_else(|| {
            clipline_mp4::PlaybackIndexError::InvalidTrack(format!(
                "movie has no track {track_index}"
            ))
        })?;
        let plan = plan_video_sample_buffers(track, limits)?;
        let converter = H264AnnexBConverter::new(plan.config, limits, generation)?;
        let mut encoded = Vec::new();
        let encoded_reserve_count = usize::from(plan.max_encoded_sample_size != 0);
        encoded
            .try_reserve_exact(plan.max_encoded_sample_size)
            .map_err(|_| AnnexBError::AllocationFailed {
                requested: plan.max_encoded_sample_size,
            })?;

        Ok(Self {
            movie,
            track_index,
            limits,
            encoded,
            encoded_high_water: 0,
            encoded_reserve_count,
            next_loaded_id: 0,
            loaded_sample_id: None,
            next_converted_id: 0,
            converted_sample_id: None,
            converter,
        })
    }

    pub fn index(&self) -> &MovieIndex {
        self.movie.index()
    }

    pub fn video_track_index(&self) -> usize {
        self.track_index
    }

    pub fn read_sample(
        &mut self,
        sample_index: usize,
        generation: WorkGeneration,
    ) -> Result<VideoAccessUnit<'_>, AnnexBError> {
        let loaded = self.read_encoded_sample(sample_index, generation)?;
        self.convert_loaded_sample(loaded, generation)
    }

    pub fn read_encoded_sample(
        &mut self,
        sample_index: usize,
        generation: WorkGeneration,
    ) -> Result<LoadedVideoSample, AnnexBError> {
        let active_generation = self.converter.active_generation();
        if generation != active_generation {
            return Err(AnnexBError::StaleGeneration {
                active: active_generation,
                actual: generation,
            });
        }
        let sample = self.movie.index().tracks[self.track_index]
            .samples
            .get(sample_index)
            .cloned()
            .ok_or_else(|| {
                clipline_mp4::PlaybackIndexError::InvalidSample(format!(
                    "track {} has no sample {sample_index}",
                    self.track_index
                ))
            })?;
        let size = sample.size as usize;
        if size > self.limits.max_encoded_sample_bytes() || size > self.encoded.capacity() {
            return Err(AnnexBError::EncodedSampleTooLarge {
                size,
                limit: self.limits.max_encoded_sample_bytes(),
            });
        }
        self.loaded_sample_id = None;
        self.encoded.resize(size, 0);
        let read_size =
            match self
                .movie
                .read_sample_into(self.track_index, sample_index, &mut self.encoded)
            {
                Ok(read_size) => read_size,
                Err(error) => {
                    self.encoded.clear();
                    self.loaded_sample_id = None;
                    return Err(error.into());
                }
            };
        if read_size != size {
            self.encoded.clear();
            return Err(clipline_mp4::PlaybackIndexError::InvalidSample(format!(
                "track {} sample {sample_index} indexed {size} bytes but read {read_size}",
                self.track_index
            ))
            .into());
        }
        self.encoded_high_water = self.encoded_high_water.max(read_size);
        let id = self
            .next_loaded_id
            .checked_add(1)
            .ok_or(AnnexBError::LoadedSampleCounterExhausted)?;
        self.next_loaded_id = id;
        self.loaded_sample_id = Some(id);
        Ok(LoadedVideoSample {
            id,
            sample_index,
            encoded_size: read_size,
            dts: sample.dts,
            pts: sample.pts,
            duration: sample.duration,
            is_sync: sample.is_sync,
            generation,
        })
    }

    pub fn convert_loaded_sample(
        &mut self,
        loaded: LoadedVideoSample,
        generation: WorkGeneration,
    ) -> Result<VideoAccessUnit<'_>, AnnexBError> {
        let converted = self.prepare_loaded_sample(loaded, generation)?;
        self.converted_sample(converted, generation)
    }

    pub fn prepare_loaded_sample(
        &mut self,
        loaded: LoadedVideoSample,
        generation: WorkGeneration,
    ) -> Result<ConvertedVideoSample, AnnexBError> {
        let active_generation = self.converter.active_generation();
        if generation != active_generation || loaded.generation != generation {
            return Err(AnnexBError::StaleGeneration {
                active: active_generation,
                actual: generation,
            });
        }
        if self.loaded_sample_id != Some(loaded.id) {
            return Err(AnnexBError::LoadedSampleSuperseded {
                loaded_id: loaded.id,
                current_id: self.loaded_sample_id,
            });
        }
        let id = self
            .next_converted_id
            .checked_add(1)
            .ok_or(AnnexBError::ConvertedSampleCounterExhausted)?;
        self.loaded_sample_id = None;
        self.converted_sample_id = None;
        let converted = self.converter.convert(
            &self.encoded[..loaded.encoded_size],
            loaded.is_sync,
            generation,
        )?;
        self.next_converted_id = id;
        self.converted_sample_id = Some(id);
        Ok(ConvertedVideoSample {
            id,
            sample_index: loaded.sample_index,
            encoded_size: loaded.encoded_size,
            dts: loaded.dts,
            pts: loaded.pts,
            duration: loaded.duration,
            is_sync: loaded.is_sync,
            generation,
            parameter_set_submission: converted.parameter_set_submission,
        })
    }

    pub fn converted_sample(
        &self,
        converted: ConvertedVideoSample,
        generation: WorkGeneration,
    ) -> Result<VideoAccessUnit<'_>, AnnexBError> {
        let active_generation = self.converter.active_generation();
        if generation != active_generation || converted.generation != generation {
            return Err(AnnexBError::StaleGeneration {
                active: active_generation,
                actual: generation,
            });
        }
        if self.converted_sample_id != Some(converted.id) {
            return Err(AnnexBError::ConvertedSampleSuperseded {
                converted_id: converted.id,
                current_id: self.converted_sample_id,
            });
        }
        Ok(VideoAccessUnit {
            bytes: self.converter.output_bytes(),
            sample_index: converted.sample_index,
            encoded_size: converted.encoded_size,
            dts: converted.dts,
            pts: converted.pts,
            duration: converted.duration,
            is_sync: converted.is_sync,
            generation,
            parameter_set_submission: converted.parameter_set_submission,
        })
    }

    pub fn seek_plan(
        &self,
        audio_track_indices: &[usize],
        requested_time: PlaybackTime,
    ) -> Result<SeekPlan, AnnexBError> {
        self.movie
            .seek_plan(self.track_index, audio_track_indices, requested_time)
            .map_err(Into::into)
    }

    pub fn resolve_step_target(
        &self,
        current: PlaybackTime,
        frames: i32,
    ) -> Result<VideoStepTarget, AnnexBError> {
        if current.timescale == 0 {
            return Err(PlaybackIndexError::InvalidTime(
                "step position timescale must be non-zero".into(),
            )
            .into());
        }
        if frames == 0 {
            return Err(
                PlaybackIndexError::InvalidTime("step count must be non-zero".into()).into(),
            );
        }
        let track = &self.movie.index().tracks[self.track_index];
        let last_sample_index = track.samples.len().checked_sub(1).ok_or_else(|| {
            PlaybackIndexError::InvalidSample("video track has no samples to step".into())
        })?;
        let current_end = track.samples.partition_point(|sample| {
            sample.pts < 0
                || u128::from(sample.pts as u64) * u128::from(current.timescale)
                    <= u128::from(current.ticks) * u128::from(track.timescale)
        });
        let current_sample_index = current_end.saturating_sub(1).min(last_sample_index);
        let stepped = (current_sample_index as i128 + i128::from(frames))
            .clamp(0, last_sample_index as i128) as usize;
        let target_ticks = u64::try_from(track.samples[stepped].pts).map_err(|_| {
            PlaybackIndexError::InvalidTime(format!(
                "video sample {stepped} has a negative presentation timestamp"
            ))
        })?;
        Ok(VideoStepTarget {
            time: PlaybackTime::new(target_ticks, track.timescale)?,
            sample_index: stepped,
        })
    }

    pub fn commit_parameter_sets(&mut self, submission: ParameterSetSubmission) -> bool {
        self.converter.commit_parameter_sets(submission)
    }

    pub fn reset_for_generation(&mut self, generation: WorkGeneration) {
        self.loaded_sample_id = None;
        self.converted_sample_id = None;
        self.encoded.clear();
        self.converter.reset_for_generation(generation);
    }

    pub fn active_generation(&self) -> WorkGeneration {
        self.converter.active_generation()
    }

    pub fn buffer_telemetry(&self) -> SampleBufferTelemetry {
        SampleBufferTelemetry {
            encoded_capacity: self.encoded.capacity(),
            converted_capacity: self.converter.output_capacity(),
            encoded_high_water: self.encoded_high_water,
            converted_high_water: self.converter.high_water(),
            encoded_reserve_count: self.encoded_reserve_count,
            converted_reserve_count: self.converter.reserve_count(),
            encoded_logical_limit: self.limits.max_encoded_sample_bytes(),
            converted_logical_limit: self.limits.max_access_unit_bytes(),
        }
    }

    pub fn into_movie(self) -> IndexedMovie<R> {
        self.movie
    }
}
