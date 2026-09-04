use std::io;

use crate::boxes::{full_box, Payload};
use crate::fragment::FragSampleInfo;
use crate::init::{
    audio_trak_with_tables_and_edits, video_trak_with_tables_and_edits, EditListEntry,
    TrackConfig, MOVIE_TIMESCALE,
};

use super::invalid_config;

#[derive(Debug, Clone, Copy)]
struct TimelineRun {
    presentation_start: u64,
    media_start: u64,
    duration: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DurationRun {
    sample_count: u32,
    sample_delta: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SampleToChunkRun {
    first_chunk: u32,
    samples_per_chunk: u32,
}

#[derive(Debug, Default, PartialEq, Eq)]
enum SyncSampleTable {
    #[default]
    All,
    Listed(Vec<u32>),
}

/// Per-track bookkeeping for the final moov.
pub(super) struct TrackState {
    cfg: TrackConfig,
    pub(super) next_decode_time: u64,
    media_duration: u64,
    sample_count: u32,
    sizes: Vec<u32>,
    duration_runs: Vec<DurationRun>,
    sync_samples: SyncSampleTable,
    /// Absolute offset of the first sample byte in each non-empty fragment for
    /// this track. Sample counts are aggregated online in `stsc_runs`.
    chunk_offsets: Vec<u64>,
    stsc_runs: Vec<SampleToChunkRun>,
    timeline_runs: Vec<TimelineRun>,
}

impl TrackState {
    pub(crate) fn new(cfg: TrackConfig) -> Self {
        Self {
            cfg,
            next_decode_time: 0,
            media_duration: 0,
            sample_count: 0,
            sizes: Vec::new(),
            duration_runs: Vec::new(),
            sync_samples: SyncSampleTable::default(),
            chunk_offsets: Vec::new(),
            stsc_runs: Vec::new(),
            timeline_runs: Vec::new(),
        }
    }
}

pub(crate) fn rescale_duration(duration: u64, source_timescale: u32, target_timescale: u32) -> u64 {
    let scaled = duration as u128 * target_timescale as u128 / source_timescale as u128;
    scaled.min(u64::MAX as u128) as u64
}

impl TrackState {
    pub(super) fn absorb_subframe_video_gap(&mut self, decode_time: u64) -> io::Result<bool> {
        if !matches!(&self.cfg, TrackConfig::Video(_)) || decode_time <= self.next_decode_time {
            return Ok(false);
        }
        let Some(last_duration_run) = self.duration_runs.last().copied() else {
            return Ok(false);
        };
        let gap = decode_time - self.next_decode_time;
        if gap >= u64::from(last_duration_run.sample_delta) {
            return Ok(false);
        }

        let gap = u32::try_from(gap)
            .map_err(|_| invalid_config("subframe video gap exceeds sample duration field"))?;
        let extended_delta = last_duration_run
            .sample_delta
            .checked_add(gap)
            .ok_or_else(|| invalid_config("video sample duration overflow"))?;
        let media_duration = self
            .media_duration
            .checked_add(u64::from(gap))
            .ok_or_else(|| invalid_config("track duration overflow"))?;
        let timeline_duration = self
            .timeline_runs
            .last()
            .ok_or_else(|| invalid_config("video duration run has no presentation run"))?
            .duration
            .checked_add(u64::from(gap))
            .ok_or_else(|| invalid_config("track presentation duration overflow"))?;

        let last_duration_run = self
            .duration_runs
            .pop()
            .expect("last duration run was checked above");
        if last_duration_run.sample_count > 1 {
            self.duration_runs.push(DurationRun {
                sample_count: last_duration_run.sample_count - 1,
                sample_delta: last_duration_run.sample_delta,
            });
        }
        match self.duration_runs.last_mut() {
            Some(previous) if previous.sample_delta == extended_delta => {
                previous.sample_count += 1;
            }
            _ => self.duration_runs.push(DurationRun {
                sample_count: 1,
                sample_delta: extended_delta,
            }),
        }
        self.media_duration = media_duration;
        self.timeline_runs
            .last_mut()
            .expect("presentation run was checked above")
            .duration = timeline_duration;
        self.next_decode_time = decode_time;
        Ok(true)
    }

    pub(super) fn duration_media_ts(&self) -> u64 {
        self.media_duration
    }

    pub(super) fn duration_movie_ts(&self) -> u64 {
        rescale_duration(
            self.presentation_end(),
            self.cfg.timescale(),
            MOVIE_TIMESCALE,
        )
    }

    fn presentation_end(&self) -> u64 {
        self.timeline_runs
            .last()
            .and_then(|run| run.presentation_start.checked_add(run.duration))
            .unwrap_or(0)
    }

    pub(super) fn record_run(&mut self, mut durations: impl Iterator<Item = u32>) -> io::Result<()> {
        let duration = durations.try_fold(0_u64, |total, duration| {
            total
                .checked_add(u64::from(duration))
                .ok_or_else(|| invalid_config("track duration overflow"))
        })?;
        let presentation_end = self
            .next_decode_time
            .checked_add(duration)
            .ok_or_else(|| invalid_config("track decode time overflow"))?;
        let media_start = self.duration_media_ts();
        if media_start > i64::MAX as u64 {
            return Err(invalid_config("track media time exceeds edit-list range"));
        }
        if let Some(previous) = self.timeline_runs.last_mut() {
            let previous_presentation_end = previous
                .presentation_start
                .checked_add(previous.duration)
                .ok_or_else(|| invalid_config("track presentation duration overflow"))?;
            let previous_media_end = previous
                .media_start
                .checked_add(previous.duration)
                .ok_or_else(|| invalid_config("track media duration overflow"))?;
            if previous_presentation_end == self.next_decode_time
                && previous_media_end == media_start
            {
                previous.duration = previous
                    .duration
                    .checked_add(duration)
                    .ok_or_else(|| invalid_config("track duration overflow"))?;
                return Ok(());
            }
        }
        self.timeline_runs.push(TimelineRun {
            presentation_start: self.next_decode_time,
            media_start,
            duration,
        });
        debug_assert_eq!(presentation_end, self.next_decode_time + duration);
        Ok(())
    }

    pub(super) fn record_chunk(&mut self, offset: u64, sample_count: usize) -> io::Result<()> {
        let samples_per_chunk = u32::try_from(sample_count)
            .map_err(|_| invalid_config("chunk sample count exceeds MP4 table range"))?;
        if samples_per_chunk == 0 {
            return Err(invalid_config(
                "MP4 chunks must contain at least one sample",
            ));
        }
        let first_chunk = u32::try_from(
            self.chunk_offsets
                .len()
                .checked_add(1)
                .ok_or_else(|| invalid_config("track chunk count overflow"))?,
        )
        .map_err(|_| invalid_config("track chunk count exceeds MP4 table range"))?;

        if self
            .stsc_runs
            .last()
            .is_none_or(|run| run.samples_per_chunk != samples_per_chunk)
        {
            self.stsc_runs.push(SampleToChunkRun {
                first_chunk,
                samples_per_chunk,
            });
        }
        self.chunk_offsets.push(offset);
        Ok(())
    }

    pub(super) fn record_sample(&mut self, sample: &FragSampleInfo) -> io::Result<()> {
        let sample_number = self
            .sample_count
            .checked_add(1)
            .ok_or_else(|| invalid_config("track sample count exceeds MP4 table range"))?;
        let next_decode_time = self
            .next_decode_time
            .checked_add(u64::from(sample.duration))
            .ok_or_else(|| invalid_config("track decode time overflow"))?;
        let media_duration = self
            .media_duration
            .checked_add(u64::from(sample.duration))
            .ok_or_else(|| invalid_config("track duration overflow"))?;

        match self.duration_runs.last_mut() {
            Some(run) if run.sample_delta == sample.duration => {
                run.sample_count = run
                    .sample_count
                    .checked_add(1)
                    .ok_or_else(|| invalid_config("duration run sample count overflow"))?;
            }
            _ => self.duration_runs.push(DurationRun {
                sample_count: 1,
                sample_delta: sample.duration,
            }),
        }
        match (&mut self.sync_samples, sample.is_sync) {
            (SyncSampleTable::All, true) | (SyncSampleTable::Listed(_), false) => {}
            (table @ SyncSampleTable::All, false) => {
                *table = SyncSampleTable::Listed((1..sample_number).collect());
            }
            (SyncSampleTable::Listed(sync_samples), true) => {
                sync_samples.push(sample_number);
            }
        }
        self.sizes.push(sample.size);
        self.sample_count = sample_number;
        self.next_decode_time = next_decode_time;
        self.media_duration = media_duration;
        Ok(())
    }

    pub(super) fn edit_list(&self) -> Vec<EditListEntry> {
        let mut entries = Vec::new();
        let mut presentation_cursor_movie = 0_u64;
        for run in &self.timeline_runs {
            let run_start_movie = rescale_duration(
                run.presentation_start,
                self.cfg.timescale(),
                MOVIE_TIMESCALE,
            );
            let run_end_movie = rescale_duration(
                run.presentation_start.saturating_add(run.duration),
                self.cfg.timescale(),
                MOVIE_TIMESCALE,
            );
            if run_start_movie > presentation_cursor_movie {
                entries.push(EditListEntry {
                    duration_movie_ts: run_start_movie - presentation_cursor_movie,
                    media_time: -1,
                });
            }
            let duration_movie = run_end_movie.saturating_sub(run_start_movie);
            if duration_movie > 0 {
                entries.push(EditListEntry {
                    duration_movie_ts: duration_movie,
                    media_time: i64::try_from(run.media_start)
                        .expect("record_run validates edit-list media time"),
                });
            }
            presentation_cursor_movie = run_end_movie;
        }
        if entries.len() == 1
            && entries[0].media_time == 0
            && self
                .timeline_runs
                .first()
                .is_some_and(|run| run.presentation_start == 0)
        {
            Vec::new()
        } else {
            entries
        }
    }

    pub(super) fn trak(&self, track_id: u32) -> Vec<u8> {
        let mut tail = self.stts();
        if let Some(stss) = self.stss() {
            tail.extend(stss);
        }
        tail.extend(self.stsc());
        tail.extend(self.stsz());
        tail.extend(self.co64());
        let media = self.duration_media_ts();
        let duration_movie = self.duration_movie_ts();
        let edits = self.edit_list();
        match &self.cfg {
            TrackConfig::Video(v) => {
                video_trak_with_tables_and_edits(v, track_id, duration_movie, media, tail, &edits)
            }
            TrackConfig::Audio(a) => {
                audio_trak_with_tables_and_edits(a, track_id, duration_movie, media, tail, &edits)
            }
        }
    }

    fn stts(&self) -> Vec<u8> {
        let mut p = Payload::new();
        p.u32(self.duration_runs.len() as u32);
        for run in &self.duration_runs {
            p.u32(run.sample_count).u32(run.sample_delta);
        }
        full_box(*b"stts", 0, 0, p.into_vec())
    }

    /// None when every sample is sync (spec: absent stss ⇒ all sync).
    fn stss(&self) -> Option<Vec<u8>> {
        let SyncSampleTable::Listed(sync_samples) = &self.sync_samples else {
            return None;
        };
        let mut p = Payload::new();
        p.u32(sync_samples.len() as u32);
        for &sample_number in sync_samples {
            p.u32(sample_number);
        }
        Some(full_box(*b"stss", 0, 0, p.into_vec()))
    }

    fn stsc(&self) -> Vec<u8> {
        let mut p = Payload::new();
        p.u32(self.stsc_runs.len() as u32);
        for run in &self.stsc_runs {
            p.u32(run.first_chunk).u32(run.samples_per_chunk).u32(1); // sample_description_index
        }
        full_box(*b"stsc", 0, 0, p.into_vec())
    }

    fn stsz(&self) -> Vec<u8> {
        let mut p = Payload::new();
        p.u32(0).u32(self.sample_count);
        for &s in &self.sizes {
            p.u32(s);
        }
        full_box(*b"stsz", 0, 0, p.into_vec())
    }

    fn co64(&self) -> Vec<u8> {
        let mut p = Payload::new();
        p.u32(self.chunk_offsets.len() as u32);
        for &offset in &self.chunk_offsets {
            p.u64(offset);
        }
        full_box(*b"co64", 0, 0, p.into_vec())
    }
}

#[cfg(test)]
pub(crate) mod support {
    use super::*;
    use crate::fragment::FragSample;
    use crate::init::{AudioTrackConfig, VideoTrackConfig};

    pub(crate) fn video_cfg() -> VideoTrackConfig {
        VideoTrackConfig::h264(
            64,
            64,
            90_000,
            vec![0x67, 0x64, 0x00, 0x0A, 0xAC],
            vec![0x68, 0xEE, 0x38, 0x80],
        )
    }

    pub(crate) fn audio_cfg() -> AudioTrackConfig {
        AudioTrackConfig {
            channels: 2,
            sample_rate: 48_000,
            pre_skip: 312,
        }
    }

    pub(crate) fn gop(start: u32) -> Vec<FragSample> {
        (0..3)
            .map(|i| FragSample {
                data: format!("sample-{:04}", start + i).into_bytes(),
                duration: 3000,
                is_sync: i == 0,
            })
            .collect()
    }

    pub(crate) fn all_sync_gop() -> Vec<FragSample> {
        (0..4)
            .map(|_| FragSample {
                data: vec![0xAA; 10],
                duration: 960,
                is_sync: true,
            })
            .collect()
    }

    pub(crate) fn read_u32_at(bytes: &[u8], offset: usize) -> u32 {
        u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap())
    }

    pub(crate) fn read_i32_at(bytes: &[u8], offset: usize) -> i32 {
        i32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap())
    }

    pub(crate) fn legacy_stsc(chunks: &[(u64, u32)]) -> Vec<u8> {
        let mut runs: Vec<(u32, u32)> = Vec::new();
        for (index, &(_, samples_per_chunk)) in chunks.iter().enumerate() {
            match runs.last() {
                Some(&(_, previous)) if previous == samples_per_chunk => {}
                _ => runs.push((index as u32 + 1, samples_per_chunk)),
            }
        }
        let mut payload = Payload::new();
        payload.u32(runs.len() as u32);
        for (first_chunk, samples_per_chunk) in runs {
            payload.u32(first_chunk).u32(samples_per_chunk).u32(1);
        }
        full_box(*b"stsc", 0, 0, payload.into_vec())
    }

    pub(crate) fn legacy_co64(chunks: &[(u64, u32)]) -> Vec<u8> {
        let mut payload = Payload::new();
        payload.u32(chunks.len() as u32);
        for &(offset, _) in chunks {
            payload.u64(offset);
        }
        full_box(*b"co64", 0, 0, payload.into_vec())
    }



    pub(crate) fn make_track_state(cfg: TrackConfig, samples: &[(u32, u32, bool)]) -> TrackState {
        let mut state = TrackState::new(cfg);
        if !samples.is_empty() {
            state
                .record_run(samples.iter().map(|(_, duration, _)| *duration))
                .unwrap();
            for &(size, duration, is_sync) in samples {
                state
                    .record_sample(&FragSampleInfo {
                        size,
                        duration,
                        is_sync,
                    })
                    .unwrap();
            }
        }
        state
    }
}

#[cfg(test)]
mod tests {
    use super::support::*;
    use super::*;
    use crate::boxes::{full_box, Payload};
    use crate::init::MOVIE_TIMESCALE;
    use crate::walker::walk;

    #[test]
    fn stts_run_length_encodes_equal_durations() {
        let state = make_track_state(
            TrackConfig::Video(video_cfg()),
            &[
                (100, 3000, true),
                (100, 3000, false),
                (100, 3000, false),
                (100, 6000, true),
                (100, 6000, false),
            ],
        );
        let stts = state.stts();
        // full_box header: 12 bytes; payload: entry_count(4) + 2 runs * 8
        let boxes = walk(&stts);
        assert_eq!(&boxes[0].fourcc, b"stts");
        let p = boxes[0].payload_offset as usize;
        let entry_count = u32::from_be_bytes(stts[p + 4..p + 8].try_into().unwrap());
        assert_eq!(entry_count, 2, "two distinct duration runs");
        // run 1: count=3, delta=3000
        assert_eq!(
            u32::from_be_bytes(stts[p + 8..p + 12].try_into().unwrap()),
            3
        );
        assert_eq!(
            u32::from_be_bytes(stts[p + 12..p + 16].try_into().unwrap()),
            3000
        );
        // run 2: count=2, delta=6000
        assert_eq!(
            u32::from_be_bytes(stts[p + 16..p + 20].try_into().unwrap()),
            2
        );
        assert_eq!(
            u32::from_be_bytes(stts[p + 20..p + 24].try_into().unwrap()),
            6000
        );
    }

    #[test]
    fn duration_runs_are_aggregated_while_samples_arrive() {
        let mut samples = vec![(100, 3000, false); 100_000];
        samples.extend([(100, 6000, false); 3]);

        let state = make_track_state(TrackConfig::Video(video_cfg()), &samples);

        assert_eq!(
            state.duration_runs,
            vec![
                DurationRun {
                    sample_count: 100_000,
                    sample_delta: 3000,
                },
                DurationRun {
                    sample_count: 3,
                    sample_delta: 6000,
                },
            ],
            "long constant-rate sessions should retain one entry per duration run, not one per sample"
        );
    }

    #[test]
    fn online_duration_runs_emit_the_same_stts_bytes_as_per_sample_tables() {
        let durations = [3000, 3000, 3001, 3001, 3001, 2999, 3000, 3000];
        let samples: Vec<_> = durations
            .iter()
            .copied()
            .map(|duration| (100, duration, false))
            .collect();
        let state = make_track_state(TrackConfig::Video(video_cfg()), &samples);

        let mut expected_payload = Payload::new();
        expected_payload
            .u32(4)
            .u32(2)
            .u32(3000)
            .u32(3)
            .u32(3001)
            .u32(1)
            .u32(2999)
            .u32(2)
            .u32(3000);
        let expected = full_box(*b"stts", 0, 0, expected_payload.into_vec());

        assert_eq!(state.stts(), expected);
    }

    #[test]
    fn stss_none_when_all_sync() {
        let state = make_track_state(
            TrackConfig::Audio(audio_cfg()),
            &[(50, 960, true), (50, 960, true), (50, 960, true)],
        );
        assert!(state.stss().is_none(), "all-sync track omits stss per spec");
    }

    #[test]
    fn stss_lists_1_based_sync_sample_numbers() {
        let state = make_track_state(
            TrackConfig::Video(video_cfg()),
            &[
                (100, 3000, true),
                (80, 3000, false),
                (80, 3000, false),
                (100, 3000, true),
            ],
        );
        let stss = state.stss().expect("should have stss");
        let boxes = walk(&stss);
        let p = boxes[0].payload_offset as usize;
        let n = u32::from_be_bytes(stss[p + 4..p + 8].try_into().unwrap());
        assert_eq!(n, 2);
        assert_eq!(
            u32::from_be_bytes(stss[p + 8..p + 12].try_into().unwrap()),
            1,
            "first sync at sample 1"
        );
        assert_eq!(
            u32::from_be_bytes(stss[p + 12..p + 16].try_into().unwrap()),
            4,
            "second sync at sample 4"
        );
    }

    #[test]
    fn sync_storage_keeps_only_one_based_sync_sample_numbers() {
        let mut samples = vec![(80, 3000, false); 100_000];
        samples[0].2 = true;
        samples[50_000].2 = true;
        samples[99_999].2 = true;

        let state = make_track_state(TrackConfig::Video(video_cfg()), &samples);

        assert_eq!(state.sample_count, 100_000);
        assert_eq!(
            state.sync_samples,
            SyncSampleTable::Listed(vec![1, 50_001, 100_000])
        );
    }

    #[test]
    fn all_sync_tracks_do_not_store_one_index_per_sample() {
        let samples = vec![(80, 960, true); 100_000];

        let state = make_track_state(TrackConfig::Audio(audio_cfg()), &samples);

        assert_eq!(state.sample_count, 100_000);
        assert_eq!(state.sync_samples, SyncSampleTable::All);
    }

    #[test]
    fn online_sync_indexes_emit_the_same_stss_bytes_as_per_sample_flags() {
        let samples = [
            (100, 3000, true),
            (80, 3000, false),
            (80, 3000, false),
            (100, 3000, true),
            (80, 3000, false),
        ];
        let state = make_track_state(TrackConfig::Video(video_cfg()), &samples);

        let mut expected_payload = Payload::new();
        expected_payload.u32(2).u32(1).u32(4);
        let expected = full_box(*b"stss", 0, 0, expected_payload.into_vec());

        assert_eq!(state.stss(), Some(expected));
    }

    #[test]
    fn stsc_run_length_encodes_chunk_sizes() {
        let mut state = make_track_state(TrackConfig::Video(video_cfg()), &[]);
        // 3 chunks: first two have 3 samples, third has 2
        state.record_chunk(0, 3).unwrap();
        state.record_chunk(100, 3).unwrap();
        state.record_chunk(200, 2).unwrap();
        let stsc = state.stsc();
        let boxes = walk(&stsc);
        let p = boxes[0].payload_offset as usize;
        let entry_count = u32::from_be_bytes(stsc[p + 4..p + 8].try_into().unwrap());
        assert_eq!(entry_count, 2, "two distinct runs");
        // run 1: first_chunk=1, samples_per_chunk=3
        assert_eq!(
            u32::from_be_bytes(stsc[p + 8..p + 12].try_into().unwrap()),
            1
        );
        assert_eq!(
            u32::from_be_bytes(stsc[p + 12..p + 16].try_into().unwrap()),
            3
        );
        // run 2: first_chunk=3, samples_per_chunk=2
        assert_eq!(
            u32::from_be_bytes(stsc[p + 20..p + 24].try_into().unwrap()),
            3
        );
        assert_eq!(
            u32::from_be_bytes(stsc[p + 24..p + 28].try_into().unwrap()),
            2
        );
    }

    #[test]
    fn stable_long_session_keeps_one_sample_to_chunk_run() {
        let mut state = make_track_state(TrackConfig::Video(video_cfg()), &[]);

        for chunk in 0..100_000_u64 {
            state.record_chunk(4_096 + chunk * 1_000_000, 300).unwrap();
        }

        assert_eq!(state.chunk_offsets.len(), 100_000);
        assert_eq!(
            state.stsc_runs,
            vec![SampleToChunkRun {
                first_chunk: 1,
                samples_per_chunk: 300,
            }],
            "constant fragment cadence should retain one stsc run, not one sample count per chunk"
        );
    }

    #[test]
    fn online_chunk_tables_are_byte_identical_to_finalize_time_encoding() {
        let legacy_chunks = [
            (1_000, 3),
            (5_000, 3),
            (9_000, 2),
            (12_000, 2),
            (15_000, 2),
            (21_000, 5),
            (30_000, 3),
        ];
        let mut state = make_track_state(TrackConfig::Video(video_cfg()), &[]);
        for &(offset, samples_per_chunk) in &legacy_chunks {
            state
                .record_chunk(offset, samples_per_chunk as usize)
                .unwrap();
        }

        assert_eq!(
            state.stsc_runs,
            vec![
                SampleToChunkRun {
                    first_chunk: 1,
                    samples_per_chunk: 3,
                },
                SampleToChunkRun {
                    first_chunk: 3,
                    samples_per_chunk: 2,
                },
                SampleToChunkRun {
                    first_chunk: 6,
                    samples_per_chunk: 5,
                },
                SampleToChunkRun {
                    first_chunk: 7,
                    samples_per_chunk: 3,
                },
            ]
        );
        assert_eq!(state.stsc(), legacy_stsc(&legacy_chunks));
        assert_eq!(state.co64(), legacy_co64(&legacy_chunks));
    }

    #[test]
    fn stsz_lists_every_sample_size() {
        let state = make_track_state(
            TrackConfig::Video(video_cfg()),
            &[(100, 3000, true), (80, 3000, false), (90, 3000, false)],
        );
        let stsz = state.stsz();
        let boxes = walk(&stsz);
        let p = boxes[0].payload_offset as usize;
        let sample_size = u32::from_be_bytes(stsz[p + 4..p + 8].try_into().unwrap());
        assert_eq!(sample_size, 0, "variable size mode");
        let count = u32::from_be_bytes(stsz[p + 8..p + 12].try_into().unwrap());
        assert_eq!(count, 3);
        assert_eq!(
            u32::from_be_bytes(stsz[p + 12..p + 16].try_into().unwrap()),
            100
        );
        assert_eq!(
            u32::from_be_bytes(stsz[p + 16..p + 20].try_into().unwrap()),
            80
        );
        assert_eq!(
            u32::from_be_bytes(stsz[p + 20..p + 24].try_into().unwrap()),
            90
        );
    }

    #[test]
    fn co64_lists_chunk_offsets() {
        let mut state = make_track_state(TrackConfig::Video(video_cfg()), &[]);
        state.record_chunk(1000, 3).unwrap();
        state.record_chunk(5000, 2).unwrap();
        let co64 = state.co64();
        let boxes = walk(&co64);
        let p = boxes[0].payload_offset as usize;
        let count = u32::from_be_bytes(co64[p + 4..p + 8].try_into().unwrap());
        assert_eq!(count, 2);
        assert_eq!(
            u64::from_be_bytes(co64[p + 8..p + 16].try_into().unwrap()),
            1000
        );
        assert_eq!(
            u64::from_be_bytes(co64[p + 16..p + 24].try_into().unwrap()),
            5000
        );
    }

    #[test]
    fn duration_media_ts_sums_all_durations() {
        let state = make_track_state(
            TrackConfig::Video(video_cfg()),
            &[(100, 3000, true), (80, 3000, false), (90, 3000, false)],
        );
        assert_eq!(state.duration_media_ts(), 9000);
    }

    #[test]
    fn duration_movie_ts_scales_to_movie_timescale() {
        let state = make_track_state(
            TrackConfig::Video(video_cfg()),
            // 90_000 ticks at media timescale 90_000 = 1 second.
            &[(100, 90_000, true)],
        );
        assert_eq!(state.duration_movie_ts(), MOVIE_TIMESCALE as u64);
    }
}
