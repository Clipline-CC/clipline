/// Metadata for one encoded sample inside a segment's `data`.
#[derive(Debug, Clone, Copy)]
pub struct SampleInfo {
    /// Byte length within `Segment::data`.
    pub size: u32,
    pub duration_s: f64,
    pub is_sync: bool,
}

/// One track's worth of encoded samples: opaque concatenated `data`
/// indexed by `samples`.
#[derive(Debug, Clone, Default)]
pub struct TrackSamples {
    /// Presentation start of the first sample, seconds since recording t0.
    pub pts_start_s: Option<f64>,
    /// Later contiguous audio runs: (first sample index, absolute PTS).
    /// Durations keep the encoded packet cadence; omitted time is a decode gap.
    pub discontinuities: Vec<(usize, f64)>,
    pub data: Vec<u8>,
    pub samples: Vec<SampleInfo>,
}

impl TrackSamples {
    /// Iterate `data` sliced per the sample index.
    pub fn sample_slices(&self) -> impl Iterator<Item = std::io::Result<&[u8]>> {
        slice_samples(&self.data, &self.samples)
    }
}

fn slice_samples<'a>(
    data: &'a [u8],
    samples: &'a [SampleInfo],
) -> impl Iterator<Item = std::io::Result<&'a [u8]>> {
    let mut offset = 0usize;
    samples.iter().map(move |s| {
        let start = offset;
        offset = offset.checked_add(s.size as usize).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "sample byte range overflow",
            )
        })?;
        data.get(start..offset).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "sample metadata exceeds encoded track data",
            )
        })
    })
}

/// One encoded, GOP-aligned media segment (ddoc §6). `data` is the opaque
/// concatenation of encoded samples; `samples` indexes it so a saved
/// window can be sliced back into muxer samples.
#[derive(Debug, Clone)]
pub struct Segment {
    /// True when the segment begins with a keyframe (IDR). Saved clips must
    /// start at such a segment so they decode cleanly.
    pub starts_with_keyframe: bool,
    /// Presentation start, seconds since recording t0.
    pub pts_start_s: f64,
    pub duration_s: f64,
    pub data: Vec<u8>,
    pub samples: Vec<SampleInfo>,
    /// Audio tracks riding alongside this video GOP (ddoc §10 multi-track).
    pub audio: Vec<TrackSamples>,
}

impl Segment {
    pub fn pts_end_s(&self) -> f64 {
        self.pts_start_s + self.duration_s
    }

    /// Total payload bytes across video and all audio tracks — the unit of
    /// ring byte-accounting.
    pub fn byte_len(&self) -> usize {
        self.data.len() + self.audio.iter().map(|t| t.data.len()).sum::<usize>()
    }

    /// Iterate `data` sliced per the sample index.
    pub fn sample_slices(&self) -> impl Iterator<Item = std::io::Result<&[u8]>> {
        slice_samples(&self.data, &self.samples)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_index_slices_data_back_into_samples() {
        let seg = Segment {
            starts_with_keyframe: true,
            pts_start_s: 0.0,
            duration_s: 0.1,
            data: b"AAAABBBCC".to_vec(),
            samples: vec![
                SampleInfo {
                    size: 4,
                    duration_s: 0.04,
                    is_sync: true,
                },
                SampleInfo {
                    size: 3,
                    duration_s: 0.03,
                    is_sync: false,
                },
                SampleInfo {
                    size: 2,
                    duration_s: 0.03,
                    is_sync: false,
                },
            ],
            audio: Vec::new(),
        };
        let slices: Vec<&[u8]> = seg.sample_slices().collect::<Result<_, _>>().unwrap();
        assert_eq!(
            slices,
            vec![b"AAAA".as_slice(), b"BBB".as_slice(), b"CC".as_slice()]
        );
    }

    #[test]
    fn byte_len_counts_video_and_audio() {
        let seg = Segment {
            starts_with_keyframe: true,
            pts_start_s: 0.0,
            duration_s: 1.0,
            data: vec![0; 100],
            samples: vec![],
            audio: vec![
                TrackSamples {
                    pts_start_s: Some(0.0),
                    discontinuities: Vec::new(),
                    data: vec![0; 30],
                    samples: vec![],
                },
                TrackSamples {
                    pts_start_s: Some(0.0),
                    discontinuities: Vec::new(),
                    data: vec![0; 20],
                    samples: vec![],
                },
            ],
        };
        assert_eq!(seg.byte_len(), 150);
    }

    #[test]
    fn track_samples_slice_like_segments() {
        let t = TrackSamples {
            pts_start_s: Some(0.0),
            discontinuities: Vec::new(),
            data: b"XXYYY".to_vec(),
            samples: vec![
                SampleInfo {
                    size: 2,
                    duration_s: 0.02,
                    is_sync: true,
                },
                SampleInfo {
                    size: 3,
                    duration_s: 0.02,
                    is_sync: true,
                },
            ],
        };
        let slices: Vec<&[u8]> = t.sample_slices().collect::<Result<_, _>>().unwrap();
        assert_eq!(slices, vec![b"XX".as_slice(), b"YYY".as_slice()]);
    }

    #[test]
    fn malformed_public_sample_metadata_returns_error_instead_of_panicking() {
        let track = TrackSamples {
            pts_start_s: None,
            discontinuities: Vec::new(),
            data: vec![1, 2],
            samples: vec![SampleInfo {
                size: 3,
                duration_s: 0.02,
                is_sync: true,
            }],
        };

        let error = track.sample_slices().next().unwrap().unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }
}
