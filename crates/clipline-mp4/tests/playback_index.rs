use std::cell::{Cell, RefCell};
use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::rc::Rc;

use clipline_mp4::{
    remux_with_selected_audio_tracks, AudioTrackConfig, FragSample, HybridMp4Writer, IndexedMovie,
    PlaybackTime, PlaybackTrackConfig, TrackConfig, VideoCodecParams, VideoTrackConfig,
};

const HEVC_VPS: &[u8] = &[0x40, 0x01, 0x0c, 0x01, 0xff, 0xff, 0x01];
const HEVC_SPS: &[u8] = &[
    0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03,
    0x00, 0x1e, 0xa0, 0x10, 0x20, 0x49, 0x65, 0x95, 0x9a, 0x49, 0x32, 0xbc, 0x05, 0xa0, 0x20, 0x00,
    0x00, 0x03, 0x00, 0x20, 0x00, 0x00, 0x03, 0x03, 0xc1,
];
const HEVC_PPS: &[u8] = &[0x44, 0x01, 0xc1, 0x72, 0xb4, 0x22, 0x40];
const AV1_SEQUENCE_HEADER: &[u8] = &[
    0x0a, 0x0a, 0x00, 0x00, 0x00, 0x03, 0x37, 0xf8, 0xe3, 0x57, 0xcc, 0x02,
];

fn fixture() -> Vec<u8> {
    let tracks = vec![
        TrackConfig::Video(VideoTrackConfig::h264(
            640,
            360,
            90_000,
            vec![0x67, 0x64, 0x00, 0x1f],
            vec![0x68, 0xee, 0x3c, 0x80],
        )),
        TrackConfig::Audio(AudioTrackConfig {
            channels: 2,
            sample_rate: 48_000,
            pre_skip: 312,
        }),
        TrackConfig::Audio(AudioTrackConfig {
            channels: 1,
            sample_rate: 48_000,
            pre_skip: 312,
        }),
    ];
    let mut writer = HybridMp4Writer::new_multi(Cursor::new(Vec::new()), tracks).unwrap();
    for second in 0..3_u8 {
        let video: Vec<_> = (0..30_u8)
            .map(|frame| FragSample {
                data: vec![0, 0, 0, 2, second, frame],
                duration: 3_000,
                is_sync: frame == 0,
            })
            .collect();
        let output: Vec<_> = (0..50_u8)
            .map(|packet| FragSample {
                data: vec![0xa0, second, packet],
                duration: 960,
                is_sync: true,
            })
            .collect();
        let microphone: Vec<_> = (0..50_u8)
            .map(|packet| FragSample {
                data: vec![0xb0, second, packet],
                duration: 960,
                is_sync: true,
            })
            .collect();
        writer
            .write_fragment_multi(&[&video, &output, &microphone])
            .unwrap();
    }
    writer.finalize().unwrap().into_inner()
}

fn leading_audio_gap_fixture() -> Vec<u8> {
    let tracks = vec![
        TrackConfig::Video(VideoTrackConfig::h264(
            640,
            360,
            90_000,
            vec![0x67, 0x64, 0x00, 0x1f],
            vec![0x68, 0xee, 0x3c, 0x80],
        )),
        TrackConfig::Audio(AudioTrackConfig {
            channels: 2,
            sample_rate: 48_000,
            pre_skip: 312,
        }),
    ];
    let mut writer = HybridMp4Writer::new_multi(Cursor::new(Vec::new()), tracks).unwrap();
    let video: Vec<_> = (0..30_u8)
        .map(|frame| FragSample {
            data: vec![0, 0, 0, 2, 0, frame],
            duration: 3_000,
            is_sync: frame == 0,
        })
        .collect();
    writer.write_fragment_multi(&[&video, &[]]).unwrap();
    writer.set_track_decode_time(1, 48_000).unwrap();
    let second_video: Vec<_> = (0..30_u8)
        .map(|frame| FragSample {
            data: vec![0, 0, 0, 2, 1, frame],
            duration: 3_000,
            is_sync: frame == 0,
        })
        .collect();
    let audio: Vec<_> = (0..50_u8)
        .map(|packet| FragSample {
            data: vec![0xa0, 1, packet],
            duration: 960,
            is_sync: true,
        })
        .collect();
    writer
        .write_fragment_multi(&[&second_video, &audio])
        .unwrap();
    writer.finalize().unwrap().into_inner()
}

fn leading_video_gap_fixture() -> Vec<u8> {
    let tracks = vec![TrackConfig::Video(VideoTrackConfig::h264(
        640,
        360,
        90_000,
        vec![0x67, 0x64, 0x00, 0x1f],
        vec![0x68, 0xee, 0x3c, 0x80],
    ))];
    let mut writer = HybridMp4Writer::new_multi(Cursor::new(Vec::new()), tracks).unwrap();
    writer.set_track_decode_time(0, 90_000).unwrap();
    let video: Vec<_> = (0..30_u8)
        .map(|frame| FragSample {
            data: vec![0, 0, 0, 2, 0, frame],
            duration: 3_000,
            is_sync: frame == 0,
        })
        .collect();
    writer.write_fragment_multi(&[&video]).unwrap();
    writer.finalize().unwrap().into_inner()
}

fn single_video_fixture(codec: VideoCodecParams) -> Vec<u8> {
    let tracks = vec![TrackConfig::Video(VideoTrackConfig {
        width: 128,
        height: 72,
        timescale: 90_000,
        codec,
    })];
    let mut writer = HybridMp4Writer::new_multi(Cursor::new(Vec::new()), tracks).unwrap();
    let samples = [FragSample {
        data: vec![0, 0, 0, 1],
        duration: 3_000,
        is_sync: true,
    }];
    writer.write_fragment_multi(&[&samples]).unwrap();
    writer.finalize().unwrap().into_inner()
}

fn fourcc_offset(bytes: &[u8], fourcc: &[u8; 4]) -> usize {
    bytes
        .windows(4)
        // HybridMp4Writer's finalized mdat deliberately contains its former
        // fragmented-init bytes. The authoritative finalized tables live in
        // the trailing moov, so use the last matching box type.
        .rposition(|window| window == fourcc)
        .unwrap_or_else(|| panic!("missing {}", String::from_utf8_lossy(fourcc)))
}

fn fourcc_offsets(bytes: &[u8], fourcc: &[u8; 4]) -> Vec<usize> {
    bytes
        .windows(4)
        .enumerate()
        .filter_map(|(offset, window)| (window == fourcc).then_some(offset))
        .collect()
}

fn finalized_track_children(bytes: &[u8], fourcc: &[u8; 4]) -> Vec<clipline_mp4::walker::BoxInfo> {
    let top = clipline_mp4::walker::walk(bytes);
    let moov = top.iter().find(|item| &item.fourcc == b"moov").unwrap();
    clipline_mp4::walker::children(bytes, moov)
        .into_iter()
        .filter(|item| &item.fourcc == b"trak")
        .map(|trak| {
            clipline_mp4::walker::children(bytes, &trak)
                .into_iter()
                .find(|item| &item.fourcc == fourcc)
                .unwrap()
        })
        .collect()
}

#[test]
fn indexes_track_configuration_and_ordered_samples() {
    let movie = IndexedMovie::from_reader(Cursor::new(fixture())).unwrap();
    let index = movie.index();
    assert_eq!(index.tracks.len(), 3);
    assert_eq!(index.movie_timescale, 720_000);
    assert_eq!(index.duration_ticks, 2_160_000);

    let video = &index.tracks[0];
    assert_eq!(video.id, 1);
    assert_eq!(video.timescale, 90_000);
    match &video.config {
        PlaybackTrackConfig::H264 {
            width,
            height,
            nal_length_size,
            sps,
            pps,
        } => {
            assert_eq!((*width, *height, *nal_length_size), (640, 360, 4));
            assert_eq!(sps, &[vec![0x67, 0x64, 0x00, 0x1f]]);
            assert_eq!(pps, &[vec![0x68, 0xee, 0x3c, 0x80]]);
        }
        other => panic!("unexpected video config: {other:?}"),
    }
    assert_eq!(video.samples.len(), 90);
    assert_eq!(video.samples[0].dts, 0);
    assert_eq!(video.samples[0].pts, 0);
    assert_eq!(video.samples[0].duration, 3_000);
    assert!(video.samples[0].is_sync);
    assert_eq!(video.samples[30].dts, 90_000);
    assert!(video.samples[30].is_sync);
    assert!(video
        .samples
        .windows(2)
        .all(|pair| pair[0].offset + u64::from(pair[0].size) <= pair[1].offset));

    assert!(matches!(
        &index.tracks[1].config,
        PlaybackTrackConfig::Opus {
            channels: 2,
            sample_rate: 48_000,
            pre_skip: 312
        }
    ));
    assert!(matches!(
        &index.tracks[2].config,
        PlaybackTrackConfig::Opus {
            channels: 1,
            sample_rate: 48_000,
            pre_skip: 312
        }
    ));
}

#[test]
fn plans_mid_gop_video_and_selected_audio_preroll() {
    let movie = IndexedMovie::from_reader(Cursor::new(fixture())).unwrap();
    let plan = movie
        .seek_plan(0, &[1, 2], PlaybackTime::new(135_000, 90_000).unwrap())
        .unwrap();

    assert_eq!(plan.video_sync_sample.track_index, 0);
    assert_eq!(plan.video_sync_sample.sample_index, 30);
    assert_eq!(plan.video_preroll.track_index, 0);
    assert_eq!(plan.video_preroll.samples, 30..46);
    assert_eq!(plan.audio_preroll.len(), 2);
    assert_eq!(plan.audio_preroll[0].track_index, 1);
    assert_eq!(plan.audio_preroll[0].samples, 50..76);
    assert_eq!(plan.audio_preroll[1].track_index, 2);
    assert_eq!(plan.audio_preroll[1].samples, 50..76);
}

#[test]
fn preserves_decode_and_edit_mapped_presentation_timestamps() {
    let movie = IndexedMovie::from_reader(Cursor::new(leading_audio_gap_fixture())).unwrap();
    let first_audio = &movie.index().tracks[1].samples[0];
    assert_eq!(first_audio.dts, 0);
    assert_eq!(first_audio.pts, 48_000);

    let plan = movie
        .seek_plan(0, &[1], PlaybackTime::new(3, 2).unwrap())
        .unwrap();
    assert_eq!(plan.video_sync_sample.sample_index, 30);
    assert_eq!(plan.audio_preroll[0].samples, 0..26);
}

#[test]
fn rejects_duplicate_or_non_audio_seek_tracks() {
    let movie = IndexedMovie::from_reader(Cursor::new(fixture())).unwrap();
    let target = PlaybackTime::new(1, 1).unwrap();
    assert!(movie.seek_plan(0, &[1, 1], target).is_err());
    assert!(movie.seek_plan(0, &[0], target).is_err());
    assert!(movie.seek_plan(1, &[], target).is_err());
}

#[test]
fn seeks_exact_sync_zero_and_eof_without_rounding() {
    let movie = IndexedMovie::from_reader(Cursor::new(fixture())).unwrap();

    let zero = movie
        .seek_plan(0, &[1], PlaybackTime::new(0, 1).unwrap())
        .unwrap();
    assert_eq!(zero.video_sync_sample.sample_index, 0);
    assert_eq!(zero.video_preroll.samples, 0..1);
    assert_eq!(zero.target_time, PlaybackTime::new(0, 90_000).unwrap());

    let exact = movie
        .seek_plan(0, &[1], PlaybackTime::new(2, 1).unwrap())
        .unwrap();
    assert_eq!(exact.video_sync_sample.sample_index, 60);
    assert_eq!(exact.video_preroll.samples, 60..61);

    let past_end = movie
        .seek_plan(0, &[1], PlaybackTime::new(u64::MAX, 1).unwrap())
        .unwrap();
    assert_eq!(past_end.video_sync_sample.sample_index, 60);
    assert_eq!(past_end.video_preroll.samples, 60..90);
    assert_eq!(past_end.audio_preroll[0].samples, 100..150);
    assert_eq!(
        past_end.target_time,
        PlaybackTime::new(270_000, 90_000).unwrap()
    );
}

#[test]
fn clamps_before_a_leading_video_edit_gap_to_the_first_frame() {
    let movie = IndexedMovie::from_reader(Cursor::new(leading_video_gap_fixture())).unwrap();
    let plan = movie
        .seek_plan(0, &[], PlaybackTime::new(0, 1).unwrap())
        .unwrap();

    assert_eq!(movie.index().tracks[0].samples[0].dts, 0);
    assert_eq!(movie.index().tracks[0].samples[0].pts, 90_000);
    assert_eq!(plan.target_time, PlaybackTime::new(90_000, 90_000).unwrap());
    assert_eq!(plan.video_sync_sample.sample_index, 0);
    assert_eq!(plan.video_preroll.samples, 0..1);
}

#[test]
fn reports_a_video_track_with_no_sync_samples_at_seek_time() {
    let mut bytes = fixture();
    let stss = fourcc_offset(&bytes, b"stss");
    bytes[stss + 8..stss + 12].copy_from_slice(&0_u32.to_be_bytes());

    let movie = IndexedMovie::from_reader(Cursor::new(bytes)).unwrap();
    let error = movie
        .seek_plan(0, &[], PlaybackTime::new(1, 1).unwrap())
        .unwrap_err();
    assert!(error.to_string().contains("no video sync sample"));
}

#[test]
fn reads_into_exact_caller_buffer_and_rejects_small_buffers() {
    let mut movie = IndexedMovie::from_reader(Cursor::new(fixture())).unwrap();
    let sample = &movie.index().tracks[0].samples[31];
    let mut exact = vec![0_u8; sample.size as usize];
    let read = movie.read_sample_into(0, 31, &mut exact).unwrap();
    assert_eq!(read, exact.len());
    assert_eq!(exact, vec![0, 0, 0, 2, 1, 1]);

    let mut short = vec![0_u8; exact.len() - 1];
    assert!(movie.read_sample_into(0, 31, &mut short).is_err());
    assert!(movie.read_sample_into(9, 0, &mut exact).is_err());
    assert!(movie.read_sample_into(0, 900, &mut exact).is_err());
}

#[derive(Clone)]
struct MetadataOnlyReader {
    inner: Cursor<Vec<u8>>,
    forbidden: std::ops::Range<u64>,
    forbidden_reads: Rc<Cell<usize>>,
}

impl Read for MetadataOnlyReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let start = self.inner.position();
        let end = start.saturating_add(output.len() as u64);
        if start < self.forbidden.end && end > self.forbidden.start {
            self.forbidden_reads
                .set(self.forbidden_reads.get().saturating_add(1));
            return Err(io::Error::other("attempted to read mdat payload"));
        }
        self.inner.read(output)
    }
}

impl Seek for MetadataOnlyReader {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.inner.seek(position)
    }
}

#[test]
fn construction_does_not_read_mdat_payloads() {
    let bytes = fixture();
    let mdat = clipline_mp4::walker::walk(&bytes)
        .into_iter()
        .find(|item| &item.fourcc == b"mdat")
        .unwrap();
    let forbidden_reads = Rc::new(Cell::new(0));
    let reader = MetadataOnlyReader {
        inner: Cursor::new(bytes),
        forbidden: mdat.payload_offset..mdat.offset + mdat.size,
        forbidden_reads: forbidden_reads.clone(),
    };
    let movie = IndexedMovie::from_reader(reader).unwrap();
    assert_eq!(movie.index().tracks.len(), 3);
    assert_eq!(forbidden_reads.get(), 0);
}

#[test]
fn opens_the_production_file_backed_path() {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "clipline-playback-index-{}-{unique}.mp4",
        std::process::id()
    ));
    std::fs::write(&path, fixture()).unwrap();

    let movie = IndexedMovie::open(&path).unwrap();
    let _ = std::fs::remove_file(path);
    assert_eq!(movie.index().tracks.len(), 3);
}

#[derive(Clone)]
struct SharedReader {
    bytes: Rc<RefCell<Vec<u8>>>,
    position: u64,
}

impl Read for SharedReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let bytes = self.bytes.borrow();
        let start = usize::try_from(self.position)
            .map_err(|_| io::Error::other("reader position overflow"))?;
        let available = bytes.get(start..).unwrap_or_default();
        let count = available.len().min(output.len());
        output[..count].copy_from_slice(&available[..count]);
        self.position += count as u64;
        Ok(count)
    }
}

impl Seek for SharedReader {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let len = self.bytes.borrow().len() as i128;
        let next = match position {
            SeekFrom::Start(offset) => i128::from(offset),
            SeekFrom::End(offset) => len + i128::from(offset),
            SeekFrom::Current(offset) => i128::from(self.position) + i128::from(offset),
        };
        if !(0..=u64::MAX as i128).contains(&next) {
            return Err(io::Error::other("invalid seek"));
        }
        self.position = next as u64;
        Ok(self.position)
    }
}

#[test]
fn a_source_truncated_after_indexing_fails_the_exact_sample_read() {
    let bytes = Rc::new(RefCell::new(fixture()));
    let reader = SharedReader {
        bytes: bytes.clone(),
        position: 0,
    };
    let mut movie = IndexedMovie::from_reader(reader).unwrap();
    let sample = movie.index().tracks[0].samples[31].clone();
    bytes
        .borrow_mut()
        .truncate((sample.offset + u64::from(sample.size) - 1) as usize);

    let mut output = vec![0_u8; sample.size as usize];
    assert!(movie.read_sample_into(0, 31, &mut output).is_err());
}

#[test]
fn rejects_composition_offsets_and_hostile_sample_tables() {
    let original = fixture();

    let mut ctts = original.clone();
    let stss = fourcc_offset(&ctts, b"stss");
    ctts[stss..stss + 4].copy_from_slice(b"ctts");
    let error = IndexedMovie::from_reader(Cursor::new(ctts))
        .err()
        .expect("ctts must be rejected");
    assert!(error.to_string().contains("ctts/B-frames"), "{error}");

    let mut zero_duration = original.clone();
    let stts = fourcc_offset(&zero_duration, b"stts");
    zero_duration[stts + 16..stts + 20].copy_from_slice(&0_u32.to_be_bytes());
    assert!(IndexedMovie::from_reader(Cursor::new(zero_duration)).is_err());

    let mut outside_mdat = original.clone();
    let co64 = fourcc_offset(&outside_mdat, b"co64");
    outside_mdat[co64 + 12..co64 + 20].copy_from_slice(&0_u64.to_be_bytes());
    let error = IndexedMovie::from_reader(Cursor::new(outside_mdat))
        .err()
        .expect("non-mdat sample must be rejected");
    assert!(error.to_string().contains("inside an mdat"), "{error}");

    let mut overlapping = original.clone();
    let co64s = fourcc_offsets(&overlapping, b"co64");
    assert_eq!(co64s.len(), 3);
    let preceding_offset = overlapping[co64s[1] + 12..co64s[1] + 20].to_vec();
    overlapping[co64s[2] + 12..co64s[2] + 20].copy_from_slice(&preceding_offset);
    let error = IndexedMovie::from_reader(Cursor::new(overlapping))
        .err()
        .expect("overlapping samples must be rejected");
    assert!(error.to_string().contains("overlap"), "{error}");

    let mut zero_size = original.clone();
    let stsz = fourcc_offset(&zero_size, b"stsz");
    zero_size[stsz + 16..stsz + 20].copy_from_slice(&0_u32.to_be_bytes());
    assert!(IndexedMovie::from_reader(Cursor::new(zero_size)).is_err());

    let mut wrong_description = original.clone();
    let stsc = fourcc_offset(&wrong_description, b"stsc");
    wrong_description[stsc + 20..stsc + 24].copy_from_slice(&2_u32.to_be_bytes());
    assert!(IndexedMovie::from_reader(Cursor::new(wrong_description)).is_err());

    let mut duplicate_sync = original;
    let stss = fourcc_offset(&duplicate_sync, b"stss");
    duplicate_sync[stss + 16..stss + 20].copy_from_slice(&1_u32.to_be_bytes());
    assert!(IndexedMovie::from_reader(Cursor::new(duplicate_sync)).is_err());
}

#[test]
fn extracts_each_supported_avc_nal_length_size() {
    for (encoded, expected) in [(0xfc, 1), (0xfd, 2), (0xff, 4)] {
        let mut bytes = fixture();
        let avcc = fourcc_offset(&bytes, b"avcC");
        bytes[avcc + 8] = encoded;
        let movie = IndexedMovie::from_reader(Cursor::new(bytes)).unwrap();
        let PlaybackTrackConfig::H264 {
            nal_length_size, ..
        } = movie.index().tracks[0].config
        else {
            panic!("expected H.264")
        };
        assert_eq!(nal_length_size, expected);
    }

    let mut reserved_three = fixture();
    let avcc = fourcc_offset(&reserved_three, b"avcC");
    reserved_three[avcc + 8] = 0xfe;
    assert!(IndexedMovie::from_reader(Cursor::new(reserved_three)).is_err());
}

#[test]
fn indexes_hevc_and_av1_decoder_configuration() {
    let hevc = single_video_fixture(VideoCodecParams::Hevc {
        vps: vec![HEVC_VPS.to_vec()],
        sps: vec![HEVC_SPS.to_vec()],
        pps: vec![HEVC_PPS.to_vec()],
    });
    let movie = IndexedMovie::from_reader(Cursor::new(hevc.clone())).unwrap();
    assert!(matches!(
        &movie.index().tracks[0].config,
        PlaybackTrackConfig::Hevc {
            nal_length_size: 4,
            vps,
            sps,
            pps,
            ..
        } if vps == &[HEVC_VPS.to_vec()]
            && sps == &[HEVC_SPS.to_vec()]
            && pps == &[HEVC_PPS.to_vec()]
    ));

    let mut reserved_three = hevc;
    let hvcc = fourcc_offset(&reserved_three, b"hvcC");
    reserved_three[hvcc + 25] = (reserved_three[hvcc + 25] & !0x03) | 0x02;
    assert!(IndexedMovie::from_reader(Cursor::new(reserved_three)).is_err());

    let av1 = single_video_fixture(VideoCodecParams::Av1 {
        sequence_header_obu: AV1_SEQUENCE_HEADER.to_vec(),
    });
    let movie = IndexedMovie::from_reader(Cursor::new(av1)).unwrap();
    assert!(matches!(
        &movie.index().tracks[0].config,
        PlaybackTrackConfig::Av1 {
            sequence_header_obu,
            ..
        } if sequence_header_obu == AV1_SEQUENCE_HEADER
    ));
}

#[test]
fn rejects_duplicate_track_ids_and_more_than_thirty_two_tracks() {
    let mut duplicate_id = fixture();
    let tkhd = finalized_track_children(&duplicate_id, b"tkhd");
    assert_eq!(tkhd.len(), 3);
    let first_payload = tkhd[0].payload_offset as usize;
    let second_payload = tkhd[1].payload_offset as usize;
    assert_eq!(duplicate_id[first_payload], 0);
    assert_eq!(duplicate_id[second_payload], 0);
    let first_id = duplicate_id[first_payload + 12..first_payload + 16].to_vec();
    duplicate_id[second_payload + 12..second_payload + 16].copy_from_slice(&first_id);
    let error = IndexedMovie::from_reader(Cursor::new(duplicate_id))
        .err()
        .expect("duplicate track ids must be rejected");
    assert!(error.to_string().contains("duplicate track id"), "{error}");

    let tracks = (0..33)
        .map(|_| {
            TrackConfig::Audio(AudioTrackConfig {
                channels: 1,
                sample_rate: 48_000,
                pre_skip: 312,
            })
        })
        .collect();
    let mut writer = HybridMp4Writer::new_multi(Cursor::new(Vec::new()), tracks).unwrap();
    let packets: Vec<_> = (0..33)
        .map(|track| {
            vec![FragSample {
                data: vec![track as u8],
                duration: 960,
                is_sync: true,
            }]
        })
        .collect();
    let packet_slices: Vec<_> = packets.iter().map(Vec::as_slice).collect();
    writer.write_fragment_multi(&packet_slices).unwrap();
    let bytes = writer.finalize().unwrap().into_inner();
    let error = IndexedMovie::from_reader(Cursor::new(bytes))
        .err()
        .expect("track count cap must be enforced");
    assert!(error.to_string().contains("track count exceeds"), "{error}");
}

#[test]
fn in_memory_remux_rejects_multiple_finalized_movie_boxes() {
    let mut bytes = fixture();
    let top = clipline_mp4::walker::walk(&bytes);
    let moov = top.iter().find(|item| &item.fourcc == b"moov").unwrap();
    let moov_bytes = bytes[moov.offset as usize..(moov.offset + moov.size) as usize].to_vec();
    bytes.extend_from_slice(&moov_bytes);

    let error = remux_with_selected_audio_tracks(&bytes, &[0])
        .expect_err("multiple moov boxes must be rejected consistently");
    assert!(error.to_string().contains("multiple moov"), "{error}");
}
