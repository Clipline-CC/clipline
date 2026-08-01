use crate::audio::AudioError;

pub const MAX_AUDIO_QUEUE_FRAMES: usize = 24_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RingTelemetry {
    pub capacity_frames: usize,
    pub queued_frames: usize,
    pub high_water_frames: usize,
    pub allocation_count: usize,
}

#[derive(Debug)]
pub struct StereoRingBuffer {
    samples: Vec<f32>,
    capacity_frames: usize,
    head_frame: usize,
    queued_frames: usize,
    high_water_frames: usize,
}

impl StereoRingBuffer {
    pub fn new(capacity_frames: usize) -> Result<Self, AudioError> {
        if capacity_frames == 0 || capacity_frames > MAX_AUDIO_QUEUE_FRAMES {
            return Err(AudioError::InvalidQueueCapacity {
                requested_frames: capacity_frames,
                max_frames: MAX_AUDIO_QUEUE_FRAMES,
            });
        }
        let sample_capacity =
            capacity_frames
                .checked_mul(2)
                .ok_or(AudioError::InvalidQueueCapacity {
                    requested_frames: capacity_frames,
                    max_frames: MAX_AUDIO_QUEUE_FRAMES,
                })?;
        Ok(Self {
            samples: vec![0.0; sample_capacity],
            capacity_frames,
            head_frame: 0,
            queued_frames: 0,
            high_water_frames: 0,
        })
    }

    pub fn push_interleaved(&mut self, samples: &[f32]) -> Result<(), AudioError> {
        if !samples.len().is_multiple_of(2) {
            return Err(AudioError::NonStereoSampleCount {
                samples: samples.len(),
            });
        }
        let frames = samples.len() / 2;
        self.ensure_available(frames)?;
        for (frame_index, frame) in samples.chunks_exact(2).enumerate() {
            let destination =
                (self.head_frame + self.queued_frames + frame_index) % self.capacity_frames;
            self.samples[destination * 2..destination * 2 + 2].copy_from_slice(frame);
        }
        self.queued_frames += frames;
        self.high_water_frames = self.high_water_frames.max(self.queued_frames);
        Ok(())
    }

    pub fn push_silence(&mut self, frames: usize) -> Result<(), AudioError> {
        self.ensure_available(frames)?;
        for frame_index in 0..frames {
            let destination =
                (self.head_frame + self.queued_frames + frame_index) % self.capacity_frames;
            self.samples[destination * 2] = 0.0;
            self.samples[destination * 2 + 1] = 0.0;
        }
        self.queued_frames += frames;
        self.high_water_frames = self.high_water_frames.max(self.queued_frames);
        Ok(())
    }

    pub fn drain_into(&mut self, output: &mut [f32]) -> Result<usize, AudioError> {
        if !output.len().is_multiple_of(2) {
            return Err(AudioError::NonStereoSampleCount {
                samples: output.len(),
            });
        }
        let frames = self.queued_frames.min(output.len() / 2);
        for frame_index in 0..frames {
            let source = (self.head_frame + frame_index) % self.capacity_frames;
            output[frame_index * 2..frame_index * 2 + 2]
                .copy_from_slice(&self.samples[source * 2..source * 2 + 2]);
        }
        self.head_frame = (self.head_frame + frames) % self.capacity_frames;
        self.queued_frames -= frames;
        if self.queued_frames == 0 {
            self.head_frame = 0;
        }
        Ok(frames)
    }

    pub fn clear(&mut self) {
        self.head_frame = 0;
        self.queued_frames = 0;
    }

    pub fn available_frames(&self) -> usize {
        self.capacity_frames - self.queued_frames
    }

    pub fn queued_frames(&self) -> usize {
        self.queued_frames
    }

    pub fn telemetry(&self) -> RingTelemetry {
        RingTelemetry {
            capacity_frames: self.capacity_frames,
            queued_frames: self.queued_frames,
            high_water_frames: self.high_water_frames,
            allocation_count: 1,
        }
    }

    fn ensure_available(&self, frames: usize) -> Result<(), AudioError> {
        if frames > self.available_frames() {
            return Err(AudioError::QueueFull {
                requested_frames: frames,
                available_frames: self.available_frames(),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_pointer_is_stable_across_wrap_and_clear() {
        let mut ring = StereoRingBuffer::new(4).unwrap();
        let pointer = ring.samples.as_ptr();
        ring.push_interleaved(&[1.0; 8]).unwrap();
        let mut drained = [0.0; 4];
        ring.drain_into(&mut drained).unwrap();
        ring.push_interleaved(&[2.0; 4]).unwrap();
        ring.clear();
        assert_eq!(ring.samples.as_ptr(), pointer);
    }
}
