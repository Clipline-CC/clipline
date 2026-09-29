use std::io::{Read, Seek, SeekFrom};

use crate::walker::{BoxInfo, children, find, walk};
use crate::{AudioTrackConfig, TrackConfig, VideoCodecParams, VideoTrackConfig};
use super::model::{
    MediaTrackCounts, MediaVideoCodec, ParsedMovie, ParsedTrack, SampleRecord, TrimError,
    rescale_ticks,
};
use super::tables::{
    box_end, child, find_box_between, fourcc_str, parse_sample_table, read_box_at,
    read_finalized_moov_bytes, read_fourcc_bounded, read_slice, read_u16, read_u16_bounded,
    read_u32_bounded, read_u64_bounded, require_child, validate_table_entries,
};

pub(crate) fn media_track_counts_reader<R: Read + Seek>(
    reader: &mut R,
) -> Result<MediaTrackCounts, TrimError> {
    let moov = read_finalized_moov_bytes(reader)?;
    finalized_movie_track_counts(&moov)
}

pub(crate) fn finalized_movie_track_counts(input: &[u8]) -> Result<MediaTrackCounts, TrimError> {
    let top = walk(input);
    let moov = find(&top, b"moov")
        .ok_or_else(|| TrimError::Unsupported("missing finalized moov".into()))?
        .clone();
    let moov_children = children(input, &moov);
    if find(&moov_children, b"mvex").is_some() {
        return Err(TrimError::Unsupported(
            "fragmented/unfinalized files are not trim-ready".into(),
        ));
    }

    let mut counts = MediaTrackCounts { video: 0, audio: 0 };
    for trak in moov_children.iter().filter(|b| &b.fourcc == b"trak") {
        match parse_track_cfg(input, trak)? {
            TrackConfig::Video(_) => counts.video += 1,
            TrackConfig::Audio(_) => counts.audio += 1,
        }
    }
    if counts.video == 0 && counts.audio == 0 {
        return Err(TrimError::Unsupported("no tracks found".into()));
    }
    Ok(counts)
}

pub(crate) fn finalized_movie_video_codecs(input: &[u8]) -> Result<Vec<MediaVideoCodec>, TrimError> {
    let top = walk(input);
    let moov = find(&top, b"moov")
        .ok_or_else(|| TrimError::Unsupported("missing finalized moov".into()))?
        .clone();
    let moov_children = children(input, &moov);
    if find(&moov_children, b"mvex").is_some() {
        return Err(TrimError::Unsupported(
            "fragmented/unfinalized files are not trim-ready".into(),
        ));
    }

    let mut codecs = Vec::new();
    for trak in moov_children.iter().filter(|b| &b.fourcc == b"trak") {
        let TrackConfig::Video(video) = parse_track_cfg(input, trak)? else {
            continue;
        };
        codecs.push(match video.codec {
            VideoCodecParams::H264 { .. } => MediaVideoCodec::H264,
            VideoCodecParams::Hevc { .. } => MediaVideoCodec::Hevc,
            VideoCodecParams::Av1 { .. } => MediaVideoCodec::Av1,
        });
    }
    Ok(codecs)
}

pub(crate) fn parse_movie(input: &[u8]) -> Result<ParsedMovie, TrimError> {
    parse_movie_with_source_len(input, input.len())
}

fn parse_movie_with_source_len(input: &[u8], source_len: usize) -> Result<ParsedMovie, TrimError> {
    let top = walk(input);
    let moov = find(&top, b"moov")
        .ok_or_else(|| TrimError::Unsupported("missing finalized moov".into()))?
        .clone();
    let moov_children = children(input, &moov);
    if find(&moov_children, b"mvex").is_some() {
        return Err(TrimError::Unsupported(
            "fragmented/unfinalized files are not trim-ready".into(),
        ));
    }
    let mvhd = find(&moov_children, b"mvhd")
        .ok_or_else(|| TrimError::Unsupported("missing mvhd".into()))?;
    let movie_timescale = parse_header_timescale(input, mvhd, "mvhd")?;

    let tracks: Vec<ParsedTrack> = moov_children
        .iter()
        .filter(|b| &b.fourcc == b"trak")
        .map(|trak| parse_track(input, trak, source_len, movie_timescale))
        .collect::<Result<_, _>>()?;
    if tracks.is_empty() {
        return Err(TrimError::Unsupported("no tracks found".into()));
    }
    Ok(ParsedMovie { tracks })
}

pub(crate) fn parse_movie_reader<R: Read + Seek>(reader: &mut R) -> Result<ParsedMovie, TrimError> {
    let source_len = usize::try_from(reader.seek(SeekFrom::End(0))?)
        .map_err(|_| TrimError::Unsupported("source file is too large to address".into()))?;
    let moov = read_finalized_moov_bytes(reader)?;
    parse_movie_with_source_len(&moov, source_len)
}

fn parse_track(
    input: &[u8],
    trak: &BoxInfo,
    source_len: usize,
    movie_timescale: u32,
) -> Result<ParsedTrack, TrimError> {
    let cfg = parse_track_cfg(input, trak)?;
    let mdia = require_child(input, trak, b"mdia")?;
    let mdhd = require_child(input, &mdia, b"mdhd")?;
    let timescale = parse_mdhd_timescale(input, &mdhd)?;
    let minf = require_child(input, &mdia, b"minf")?;
    let stbl = require_child(input, &minf, b"stbl")?;
    let samples = parse_sample_table(input, &stbl, source_len)?;
    let samples = apply_track_edit_list(input, trak, samples, timescale, movie_timescale)?;
    if samples.is_empty() {
        return Err(TrimError::Unsupported("track has no samples".into()));
    }
    Ok(ParsedTrack {
        cfg,
        timescale,
        samples,
    })
}

#[derive(Debug, Clone, Copy)]
struct ParsedEdit {
    duration_movie_ts: u64,
    media_time: i64,
}

fn apply_track_edit_list(
    input: &[u8],
    trak: &BoxInfo,
    samples: Vec<SampleRecord>,
    track_timescale: u32,
    movie_timescale: u32,
) -> Result<Vec<SampleRecord>, TrimError> {
    let Some(edts) = child(input, trak, b"edts") else {
        return Ok(samples);
    };
    let elst = require_child(input, &edts, b"elst")?;
    let edits = parse_elst(input, &elst)?;
    if edits.is_empty() {
        return Err(TrimError::Corrupt("empty elst".into()));
    }

    let mut output = Vec::with_capacity(samples.len());
    let mut presentation_cursor_movie = 0_u64;
    let mut previous_media_end = 0_u64;
    let mut saw_media = false;
    for edit in edits {
        let presentation_start =
            rescale_ticks(presentation_cursor_movie, movie_timescale, track_timescale)?;
        presentation_cursor_movie = presentation_cursor_movie
            .checked_add(edit.duration_movie_ts)
            .ok_or_else(|| TrimError::Corrupt("edit-list duration overflow".into()))?;
        if edit.media_time == -1 {
            continue;
        }
        let media_start = u64::try_from(edit.media_time)
            .map_err(|_| TrimError::Unsupported("negative edit-list media time".into()))?;
        if saw_media && media_start < previous_media_end {
            return Err(TrimError::Unsupported(
                "overlapping or backward edit-list media ranges".into(),
            ));
        }
        let first = samples
            .iter()
            .position(|sample| sample.start_ticks == media_start)
            .ok_or_else(|| {
                TrimError::Unsupported("edit-list media time begins within a sample".into())
            })?;
        let duration_scaled = u128::from(edit.duration_movie_ts) * u128::from(track_timescale);
        let duration_scale = u128::from(movie_timescale);
        let mut copied = 0_usize;
        for sample in samples.iter().skip(first) {
            let relative_start = sample
                .start_ticks
                .checked_sub(media_start)
                .ok_or_else(|| TrimError::Unsupported("backward edit-list media range".into()))?;
            if u128::from(relative_start) * duration_scale >= duration_scaled {
                break;
            }
            let mut mapped = sample.clone();
            mapped.start_ticks = presentation_start
                .checked_add(relative_start)
                .ok_or_else(|| TrimError::Corrupt("mapped sample time overflow".into()))?;
            output.push(mapped);
            copied += 1;
        }
        if copied == 0 {
            return Err(TrimError::Unsupported(
                "edit-list media segment contains no complete sample start".into(),
            ));
        }
        previous_media_end = samples[first + copied - 1]
            .start_ticks
            .checked_add(u64::from(samples[first + copied - 1].duration))
            .ok_or_else(|| TrimError::Corrupt("sample end overflow".into()))?;
        let presented_media = previous_media_end
            .checked_sub(media_start)
            .ok_or_else(|| TrimError::Unsupported("backward edit-list media range".into()))?;
        if u128::from(presented_media) * u128::from(movie_timescale) != duration_scaled {
            return Err(TrimError::Unsupported(
                "edit-list media segment must end on a sample boundary".into(),
            ));
        }
        saw_media = true;
    }
    if output.is_empty() {
        return Err(TrimError::Unsupported(
            "edit list presents no track samples".into(),
        ));
    }
    Ok(output)
}

fn parse_elst(input: &[u8], elst: &BoxInfo) -> Result<Vec<ParsedEdit>, TrimError> {
    let p = elst.payload_offset as usize;
    let end = box_end(elst)?;
    let version = *input
        .get(p)
        .filter(|_| p < end)
        .ok_or_else(|| TrimError::Corrupt("truncated elst".into()))?;
    let entry_size = match version {
        0 => 12,
        1 => 20,
        _ => return Err(TrimError::Unsupported("unknown elst version".into())),
    };
    let count = read_u32_bounded(input, p + 4, end, "elst")? as usize;
    validate_table_entries(count, p + 8, end, entry_size, "elst")?;
    let mut edits = Vec::with_capacity(count);
    let mut pos = p + 8;
    for _ in 0..count {
        let (duration_movie_ts, media_time, rate_offset) = if version == 1 {
            (
                read_u64_bounded(input, pos, end, "elst")?,
                read_u64_bounded(input, pos + 8, end, "elst")? as i64,
                pos + 16,
            )
        } else {
            (
                u64::from(read_u32_bounded(input, pos, end, "elst")?),
                i64::from(read_u32_bounded(input, pos + 4, end, "elst")? as i32),
                pos + 8,
            )
        };
        if read_u32_bounded(input, rate_offset, end, "elst")? != 0x0001_0000 {
            return Err(TrimError::Unsupported(
                "edit-list media rates other than 1.0 are unsupported".into(),
            ));
        }
        if duration_movie_ts == 0 {
            return Err(TrimError::Corrupt("zero-duration edit-list entry".into()));
        }
        edits.push(ParsedEdit {
            duration_movie_ts,
            media_time,
        });
        pos += entry_size;
    }
    Ok(edits)
}

fn parse_track_cfg(input: &[u8], trak: &BoxInfo) -> Result<TrackConfig, TrimError> {
    let mdia = require_child(input, trak, b"mdia")?;
    let mdhd = require_child(input, &mdia, b"mdhd")?;
    let timescale = parse_mdhd_timescale(input, &mdhd)?;
    let hdlr = require_child(input, &mdia, b"hdlr")?;
    let handler = parse_hdlr(input, &hdlr)?;
    let minf = require_child(input, &mdia, b"minf")?;
    let stbl = require_child(input, &minf, b"stbl")?;
    let stsd = require_child(input, &stbl, b"stsd")?;
    parse_stsd(input, &stsd, handler, timescale)
}

fn parse_mdhd_timescale(input: &[u8], mdhd: &BoxInfo) -> Result<u32, TrimError> {
    parse_header_timescale(input, mdhd, "mdhd")
}

fn parse_header_timescale(input: &[u8], header: &BoxInfo, label: &str) -> Result<u32, TrimError> {
    let p = header.payload_offset as usize;
    let end = box_end(header)?;
    let version = *input
        .get(p)
        .filter(|_| p < end)
        .ok_or_else(|| TrimError::Corrupt(format!("truncated {label}")))?;
    let ts_off = match version {
        0 => p + 12,
        1 => p + 20,
        _ => return Err(TrimError::Unsupported(format!("unknown {label} version"))),
    };
    let timescale = read_u32_bounded(input, ts_off, end, label)?;
    if timescale == 0 {
        return Err(TrimError::Corrupt(format!("zero {label} timescale")));
    }
    Ok(timescale)
}

fn parse_hdlr(input: &[u8], hdlr: &BoxInfo) -> Result<[u8; 4], TrimError> {
    let p = hdlr.payload_offset as usize;
    read_fourcc_bounded(input, p + 8, box_end(hdlr)?, "hdlr")
}

fn parse_stsd(
    input: &[u8],
    stsd: &BoxInfo,
    handler: [u8; 4],
    timescale: u32,
) -> Result<TrackConfig, TrimError> {
    let p = stsd.payload_offset as usize;
    let stsd_end = box_end(stsd)?;
    let entry_count = read_u32_bounded(input, p + 4, stsd_end, "stsd")?;
    if entry_count != 1 {
        return Err(TrimError::Unsupported(
            "expected exactly one sample description".into(),
        ));
    }
    let entry = read_box_at(input, p + 8, stsd_end)?;
    match &handler {
        b"vide" => parse_video_stsd(input, &entry, timescale),
        b"soun" => parse_audio_stsd(input, &entry),
        _ => Err(TrimError::Unsupported(format!(
            "unsupported handler {}",
            fourcc_str(&handler)
        ))),
    }
}

fn parse_video_stsd(
    input: &[u8],
    entry: &BoxInfo,
    timescale: u32,
) -> Result<TrackConfig, TrimError> {
    let p = entry.payload_offset as usize;
    let entry_end = box_end(entry)?;
    if p + 78 > entry_end {
        return Err(TrimError::Corrupt("truncated visual sample entry".into()));
    }
    let width = read_u16(input, p + 24)?;
    let height = read_u16(input, p + 26)?;
    // The codec configuration box follows the 78-byte VisualSampleEntry
    // shell, which is identical for avc1/hvc1/av01.
    let codec = match &entry.fourcc {
        b"avc1" => {
            let avcc = find_box_between(input, p + 78, entry_end, b"avcC")?
                .ok_or_else(|| TrimError::Unsupported("missing avcC".into()))?;
            let (sps, pps) = parse_avcc(input, &avcc)?;
            VideoCodecParams::H264 { sps, pps }
        }
        b"hvc1" | b"hev1" => {
            let hvcc = find_box_between(input, p + 78, entry_end, b"hvcC")?
                .ok_or_else(|| TrimError::Unsupported("missing hvcC".into()))?;
            let (vps, sps, pps) = parse_hvcc(input, &hvcc)?;
            VideoCodecParams::Hevc { vps, sps, pps }
        }
        b"av01" => {
            let av1c = find_box_between(input, p + 78, entry_end, b"av1C")?
                .ok_or_else(|| TrimError::Unsupported("missing av1C".into()))?;
            let sequence_header_obu = parse_av1c(input, &av1c)?;
            VideoCodecParams::Av1 {
                sequence_header_obu,
            }
        }
        other => {
            return Err(TrimError::Unsupported(format!(
                "unsupported video sample entry {}",
                fourcc_str(other)
            )))
        }
    };
    Ok(TrackConfig::Video(VideoTrackConfig {
        width,
        height,
        timescale,
        codec,
    }))
}

fn parse_audio_stsd(input: &[u8], entry: &BoxInfo) -> Result<TrackConfig, TrimError> {
    if &entry.fourcc != b"Opus" {
        return Err(TrimError::Unsupported(format!(
            "unsupported audio sample entry {}",
            fourcc_str(&entry.fourcc)
        )));
    }
    let p = entry.payload_offset as usize;
    let entry_end = box_end(entry)?;
    if p + 28 > entry_end {
        return Err(TrimError::Corrupt("truncated Opus sample entry".into()));
    }
    let channels = read_u16(input, p + 16)?;
    let dops = find_box_between(input, p + 28, entry_end, b"dOps")?
        .ok_or_else(|| TrimError::Unsupported("missing dOps".into()))?;
    let dp = dops.payload_offset as usize;
    let dops_end = box_end(&dops)?;
    let pre_skip = read_u16_bounded(input, dp + 2, dops_end, "dOps")?;
    let sample_rate = read_u32_bounded(input, dp + 4, dops_end, "dOps")?;
    Ok(TrackConfig::Audio(AudioTrackConfig {
        channels,
        sample_rate,
        pre_skip,
    }))
}

type H264ParamSets = (Vec<Vec<u8>>, Vec<Vec<u8>>);

fn parse_avcc(input: &[u8], avcc: &BoxInfo) -> Result<H264ParamSets, TrimError> {
    let p = avcc.payload_offset as usize;
    let end = box_end(avcc)?;
    if p + 7 > end {
        return Err(TrimError::Corrupt("truncated avcC".into()));
    }
    let sps_count = input[p + 5] & 0x1f;
    if sps_count == 0 {
        return Err(TrimError::Unsupported("avcC has no SPS".into()));
    }
    let mut pos = p + 6;
    let mut sps = Vec::with_capacity(sps_count as usize);
    for _ in 0..sps_count {
        let len = read_u16_bounded(input, pos, end, "avcC")? as usize;
        pos += 2;
        let data = read_slice(input, pos, len, end)?.to_vec();
        pos += len;
        sps.push(data);
    }
    let pps_count = *input
        .get(pos)
        .ok_or_else(|| TrimError::Corrupt("truncated avcC PPS count".into()))?;
    pos += 1;
    if pps_count == 0 {
        return Err(TrimError::Unsupported("avcC has no PPS".into()));
    }
    let mut pps = Vec::with_capacity(pps_count as usize);
    for _ in 0..pps_count {
        let pps_len = read_u16_bounded(input, pos, end, "avcC")? as usize;
        pos += 2;
        pps.push(read_slice(input, pos, pps_len, end)?.to_vec());
        pos += pps_len;
    }
    Ok((sps, pps))
}

/// (VPS, SPS, PPS) raw NAL units recovered from an `hvcC` record.
type HevcParamSets = (Vec<Vec<u8>>, Vec<Vec<u8>>, Vec<Vec<u8>>);

/// Recover every VPS/SPS/PPS NAL from an `hvcC` NAL-array section.
fn parse_hvcc(input: &[u8], hvcc: &BoxInfo) -> Result<HevcParamSets, TrimError> {
    let p = hvcc.payload_offset as usize;
    let end = box_end(hvcc)?;
    // The fixed configuration prefix is 22 bytes; numOfArrays is the 23rd.
    if p + 23 > end {
        return Err(TrimError::Corrupt("truncated hvcC".into()));
    }
    let num_arrays = input[p + 22];
    let mut pos = p + 23;
    let mut vps = Vec::new();
    let mut sps = Vec::new();
    let mut pps = Vec::new();
    for _ in 0..num_arrays {
        let nal_type = *input
            .get(pos)
            .ok_or_else(|| TrimError::Corrupt("truncated hvcC array header".into()))?
            & 0x3F;
        pos += 1;
        let num_nalus = read_u16_bounded(input, pos, end, "hvcC")?;
        pos += 2;
        for _ in 0..num_nalus {
            let len = read_u16_bounded(input, pos, end, "hvcC")? as usize;
            pos += 2;
            let data = read_slice(input, pos, len, end)?.to_vec();
            pos += len;
            match nal_type {
                32 => vps.push(data),
                33 => sps.push(data),
                34 => pps.push(data),
                _ => {}
            }
        }
    }
    if vps.is_empty() || sps.is_empty() || pps.is_empty() {
        Err(TrimError::Unsupported("hvcC missing VPS/SPS/PPS".into()))
    } else {
        Ok((vps, sps, pps))
    }
}

/// The `av1C` configOBUs payload is the sequence-header OBU verbatim.
fn parse_av1c(input: &[u8], av1c: &BoxInfo) -> Result<Vec<u8>, TrimError> {
    let p = av1c.payload_offset as usize;
    let end = box_end(av1c)?;
    // 4-byte fixed configuration record, then configOBUs.
    if p + 4 > end {
        return Err(TrimError::Corrupt("truncated av1C".into()));
    }
    let obu = read_slice(input, p + 4, end - (p + 4), end)?.to_vec();
    if obu.is_empty() {
        return Err(TrimError::Unsupported("av1C has no configOBUs".into()));
    }
    Ok(obu)
}

#[cfg(test)]
mod tests {
    use super::super::model::fixtures::*;
    use super::*;
    use crate::HybridMp4Writer;
    use std::io::Cursor;

        #[test]
        fn rejects_unfinalized_or_missing_sample_tables() {
            let mut w = HybridMp4Writer::new_multi(Cursor::new(Vec::new()), tracks()).unwrap();
            let v = video_gop(0);
            let a = audio_packets(0);
            w.write_fragment_multi(&[&v, &a]).unwrap();
            let fragmented = w.into_inner().into_inner();
    
            assert!(parse_movie(&fragmented).is_err());
        }
}
