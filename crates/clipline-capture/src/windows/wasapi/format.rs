use super::*;

pub(crate) fn wasapi_timestamp_valid(flags: u32) -> bool {
    flags & (AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32) == 0
}

pub(crate) fn wasapi_data_discontinuous(flags: u32) -> bool {
    flags & (AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32) != 0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SampleFormat {
    Float32,
    Pcm16,
    Pcm24,
    Pcm32,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct MixFormat {
    pub(crate) channels: u16,
    pub(crate) sample_rate: u32,
    pub(crate) sample_format: SampleFormat,
}

#[derive(Debug, Default)]
pub(crate) struct AudioLevelAccumulator {
    sum_squares: f64,
    peak: f32,
    sample_count: usize,
}

impl AudioLevelAccumulator {
    pub(crate) fn add(&mut self, samples: &[f32]) {
        for &sample in samples {
            let abs = sample.abs();
            self.peak = self.peak.max(abs);
            self.sum_squares += sample as f64 * sample as f64;
        }
        self.sample_count += samples.len();
    }

    pub(crate) fn take(&mut self) -> AudioLevel {
        let rms = if self.sample_count == 0 {
            0.0
        } else {
            (self.sum_squares / self.sample_count as f64).sqrt() as f32
        };
        let level = AudioLevel {
            rms,
            peak: self.peak,
            sample_count: self.sample_count,
        };
        *self = Self::default();
        level
    }
}

pub(crate) fn audio_poll_silence_horizon(until_pts_s: f64) -> Option<f64> {
    (until_pts_s.is_finite() && until_pts_s != f64::MAX)
        .then(|| (until_pts_s - AUDIO_DELIVERY_HEADROOM_S).max(0.0))
}

pub(crate) fn parse_mix_format(format: &WAVEFORMATEX) -> Option<MixFormat> {
    // Copy packed fields to locals (references into packed structs are UB).
    let channels = format.nChannels;
    let rate = format.nSamplesPerSec;
    let bits = format.wBitsPerSample;
    if channels == 0 || rate == 0 {
        return None;
    }
    let tag = format.wFormatTag as u32;
    let sample_format = match tag {
        WAVE_FORMAT_IEEE_FLOAT if bits == 32 => SampleFormat::Float32,
        WAVE_FORMAT_PCM => pcm_sample_format(bits)?,
        WAVE_FORMAT_EXTENSIBLE => {
            // SAFETY: extensible tag guarantees the larger layout.
            let ext = unsafe { &*(format as *const WAVEFORMATEX as *const WAVEFORMATEXTENSIBLE) };
            let sub = ext.SubFormat;
            if sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT && bits == 32 {
                SampleFormat::Float32
            } else if sub == KSDATAFORMAT_SUBTYPE_PCM {
                pcm_sample_format(bits)?
            } else {
                return None;
            }
        }
        _ => return None,
    };
    Some(MixFormat {
        channels,
        sample_rate: rate,
        sample_format,
    })
}

fn pcm_sample_format(bits: u16) -> Option<SampleFormat> {
    match bits {
        16 => Some(SampleFormat::Pcm16),
        24 => Some(SampleFormat::Pcm24),
        32 => Some(SampleFormat::Pcm32),
        _ => None,
    }
}

impl SampleFormat {
    pub(crate) const fn bytes_per_sample(self) -> usize {
        match self {
            Self::Float32 | Self::Pcm32 => 4,
            Self::Pcm16 => 2,
            Self::Pcm24 => 3,
        }
    }
}

pub(crate) fn decode_sample_bytes(
    bytes: &[u8],
    sample_format: SampleFormat,
    sample_count: usize,
) -> Result<Vec<f32>, &'static str> {
    let expected_len = sample_count
        .checked_mul(sample_format.bytes_per_sample())
        .ok_or("WASAPI buffer size overflow")?;
    if bytes.len() != expected_len {
        return Err("WASAPI buffer length does not match its frame count");
    }
    Ok(match sample_format {
        SampleFormat::Float32 => bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|sample| f32::from_le_bytes(*sample))
            .collect(),
        SampleFormat::Pcm16 => bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|sample| i16::from_le_bytes(*sample) as f32 / 32_768.0)
            .collect(),
        SampleFormat::Pcm24 => bytes
            .as_chunks::<3>()
            .0
            .iter()
            .map(|sample| {
                let raw = sample[0] as i32 | ((sample[1] as i32) << 8) | ((sample[2] as i32) << 16);
                let signed = (raw << 8) >> 8;
                signed as f32 / 8_388_608.0
            })
            .collect(),
        SampleFormat::Pcm32 => bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|sample| i32::from_le_bytes(*sample) as f32 / 2_147_483_648.0)
            .collect(),
    })
}

pub(crate) fn process_loopback_format() -> WAVEFORMATEX {
    const CHANNELS: u16 = 2;
    const BITS_PER_SAMPLE: u16 = 16;
    const SAMPLE_RATE: u32 = 44_100;
    let block_align = CHANNELS * (BITS_PER_SAMPLE / 8);
    WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_PCM as u16,
        nChannels: CHANNELS,
        nSamplesPerSec: SAMPLE_RATE,
        nAvgBytesPerSec: SAMPLE_RATE * block_align as u32,
        nBlockAlign: block_align,
        wBitsPerSample: BITS_PER_SAMPLE,
        cbSize: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_decoder_accepts_misaligned_little_endian_buffers() {
        fn misaligned(samples: impl IntoIterator<Item = u8>) -> Vec<u8> {
            std::iter::once(0xAA).chain(samples).collect()
        }

        let float = misaligned(
            (-1.0f32)
                .to_le_bytes()
                .into_iter()
                .chain(0.5f32.to_le_bytes()),
        );
        assert_eq!(
            decode_sample_bytes(&float[1..], SampleFormat::Float32, 2).unwrap(),
            [-1.0, 0.5]
        );

        let pcm16 = misaligned(
            i16::MIN
                .to_le_bytes()
                .into_iter()
                .chain(16_384i16.to_le_bytes()),
        );
        assert_eq!(
            decode_sample_bytes(&pcm16[1..], SampleFormat::Pcm16, 2).unwrap(),
            [-1.0, 0.5]
        );

        let pcm32 = misaligned(
            i32::MIN
                .to_le_bytes()
                .into_iter()
                .chain(1_073_741_824i32.to_le_bytes()),
        );
        assert_eq!(
            decode_sample_bytes(&pcm32[1..], SampleFormat::Pcm32, 2).unwrap(),
            [-1.0, 0.5]
        );

        let pcm24 = misaligned([0x00, 0x00, 0x80, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x7F]);
        let decoded = decode_sample_bytes(&pcm24[1..], SampleFormat::Pcm24, 3).unwrap();
        assert_eq!(decoded[0], -1.0);
        assert_eq!(decoded[1], -1.0 / 8_388_608.0);
        assert!((decoded[2] - 8_388_607.0 / 8_388_608.0).abs() < f32::EPSILON);
    }

    #[test]
    fn sample_decoder_rejects_truncated_or_extra_bytes() {
        assert!(decode_sample_bytes(&[0; 3], SampleFormat::Float32, 1).is_err());
        assert!(decode_sample_bytes(&[0; 3], SampleFormat::Pcm16, 1).is_err());
        assert!(decode_sample_bytes(&[0; 2], SampleFormat::Pcm24, 1).is_err());
        assert!(decode_sample_bytes(&[0; 5], SampleFormat::Pcm32, 1).is_err());
    }

    #[test]
    fn audio_poll_horizon_leaves_thirty_milliseconds_for_delivery() {
        assert_eq!(audio_poll_silence_horizon(0.5), Some(0.47));
        assert_eq!(audio_poll_silence_horizon(0.01), Some(0.0));
    }

    #[test]
    fn audio_poll_horizon_does_not_synthesize_for_monitor_drains() {
        assert_eq!(audio_poll_silence_horizon(f64::MAX), None);
        assert_eq!(audio_poll_silence_horizon(f64::INFINITY), None);
        assert_eq!(audio_poll_silence_horizon(f64::NAN), None);
    }

    #[test]
    fn wasapi_timestamp_error_flag_marks_timestamp_invalid() {
        assert!(wasapi_timestamp_valid(0));
        assert!(wasapi_timestamp_valid(
            AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32
        ));
        assert!(!wasapi_timestamp_valid(
            AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32
        ));
        assert!(wasapi_data_discontinuous(
            AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32
        ));
    }

    #[test]
    fn process_loopback_format_matches_windows_sample_pcm16() {
        let format = process_loopback_format();
        let tag = format.wFormatTag;
        let channels = format.nChannels;
        let sample_rate = format.nSamplesPerSec;
        let bits = format.wBitsPerSample;
        let block_align = format.nBlockAlign;
        assert_eq!(tag as u32, WAVE_FORMAT_PCM);
        assert_eq!(channels, 2);
        assert_eq!(sample_rate, 44_100);
        assert_eq!(bits, 16);
        assert_eq!(block_align, 4);
    }
}
