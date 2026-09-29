use std::io::{Read, Seek, SeekFrom};

use crate::walker::{BoxInfo, children};
use super::model::{SampleRecord, TrimError};

/// Conservative upper bound for a finalized `moov` box that this metadata
/// reader will load into memory. Real Clipline sample tables are far smaller;
/// this rejects corrupt or hostile declarations before allocation.
const MAX_FINALIZED_MOOV_BYTES: u64 = 64 * 1024 * 1024;

/// Upper bound for per-track sample metadata. At 60 FPS this still permits
/// more than 18 hours of video while preventing tiny hostile tables from
/// expanding into multi-gigabyte allocations.
const MAX_PARSED_SAMPLES: usize = 4_000_000;

pub(crate) fn parse_sample_table(
    input: &[u8],
    stbl: &BoxInfo,
    source_len: usize,
) -> Result<Vec<SampleRecord>, TrimError> {
    let stsz = require_child(input, stbl, b"stsz")?;
    let sizes = parse_stsz(input, &stsz)?;
    let stts = require_child(input, stbl, b"stts")?;
    let durations = parse_stts(input, &stts, sizes.len())?;
    let sync = match child(input, stbl, b"stss") {
        Some(stss) => parse_stss(input, &stss, sizes.len())?,
        None => vec![true; sizes.len()],
    };
    let stsc = require_child(input, stbl, b"stsc")?;
    let chunk_offsets = if let Some(co64) = child(input, stbl, b"co64") {
        parse_co64(input, &co64)?
    } else {
        let stco = require_child(input, stbl, b"stco")?;
        parse_stco(input, &stco)?
    };
    let samples_per_chunk = parse_stsc(input, &stsc, chunk_offsets.len())?;
    records_from_tables(
        source_len,
        &sizes,
        &durations,
        &sync,
        &chunk_offsets,
        &samples_per_chunk,
    )
}

fn parse_stts(
    input: &[u8],
    stts: &BoxInfo,
    expected_sample_count: usize,
) -> Result<Vec<u32>, TrimError> {
    let p = stts.payload_offset as usize;
    let count = read_u32(input, p + 4)? as usize;
    let end = box_end(stts)?;
    let mut pos = p + 8;
    validate_table_entries(count, pos, end, 8, "stts")?;
    validate_sample_count(expected_sample_count, "stts")?;
    let mut out = Vec::with_capacity(expected_sample_count);
    for _ in 0..count {
        let sample_count = read_u32(input, pos)? as usize;
        let delta = read_u32(input, pos + 4)?;
        let expanded = out
            .len()
            .checked_add(sample_count)
            .ok_or_else(|| TrimError::Corrupt("stts sample count overflow".into()))?;
        if expanded > expected_sample_count || expanded > MAX_PARSED_SAMPLES {
            return Err(TrimError::Corrupt(
                "stts sample count exceeds limit or stsz count".into(),
            ));
        }
        out.extend(std::iter::repeat_n(delta, sample_count));
        pos += 8;
    }
    if out.len() != expected_sample_count {
        return Err(TrimError::Corrupt(format!(
            "stts/stsz sample count mismatch: {} vs {expected_sample_count}",
            out.len()
        )));
    }
    Ok(out)
}

fn parse_stsz(input: &[u8], stsz: &BoxInfo) -> Result<Vec<u32>, TrimError> {
    let p = stsz.payload_offset as usize;
    let sample_size = read_u32(input, p + 4)?;
    let sample_count = read_u32(input, p + 8)? as usize;
    validate_sample_count(sample_count, "stsz")?;
    if sample_size != 0 {
        return Ok(vec![sample_size; sample_count]);
    }
    let end = box_end(stsz)?;
    let mut pos = p + 12;
    validate_table_entries(sample_count, pos, end, 4, "stsz")?;
    let mut out = Vec::with_capacity(sample_count);
    for _ in 0..sample_count {
        out.push(read_u32(input, pos)?);
        pos += 4;
    }
    Ok(out)
}

fn parse_stss(input: &[u8], stss: &BoxInfo, sample_count: usize) -> Result<Vec<bool>, TrimError> {
    let p = stss.payload_offset as usize;
    let entry_count = read_u32(input, p + 4)? as usize;
    let end = box_end(stss)?;
    let mut pos = p + 8;
    validate_table_entries(entry_count, pos, end, 4, "stss")?;
    if entry_count > sample_count {
        return Err(TrimError::Corrupt(
            "stss entry count exceeds sample count".into(),
        ));
    }
    let mut sync = vec![false; sample_count];
    for _ in 0..entry_count {
        let n = read_u32(input, pos)? as usize;
        if n == 0 || n > sample_count {
            return Err(TrimError::Corrupt("stss sample number out of range".into()));
        }
        sync[n - 1] = true;
        pos += 4;
    }
    Ok(sync)
}

fn parse_co64(input: &[u8], co64: &BoxInfo) -> Result<Vec<u64>, TrimError> {
    let p = co64.payload_offset as usize;
    let count = read_u32(input, p + 4)? as usize;
    let end = box_end(co64)?;
    let mut pos = p + 8;
    validate_sample_count(count, "co64")?;
    validate_table_entries(count, pos, end, 8, "co64")?;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        out.push(read_u64(input, pos)?);
        pos += 8;
    }
    Ok(out)
}

fn parse_stco(input: &[u8], stco: &BoxInfo) -> Result<Vec<u64>, TrimError> {
    let p = stco.payload_offset as usize;
    let count = read_u32(input, p + 4)? as usize;
    let end = box_end(stco)?;
    let mut pos = p + 8;
    validate_sample_count(count, "stco")?;
    validate_table_entries(count, pos, end, 4, "stco")?;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        out.push(read_u32(input, pos)? as u64);
        pos += 4;
    }
    Ok(out)
}

fn parse_stsc(input: &[u8], stsc: &BoxInfo, chunk_count: usize) -> Result<Vec<u32>, TrimError> {
    let p = stsc.payload_offset as usize;
    let entry_count = read_u32(input, p + 4)? as usize;
    if entry_count == 0 && chunk_count > 0 {
        return Err(TrimError::Corrupt("stsc has no entries".into()));
    }
    let end = box_end(stsc)?;
    let mut pos = p + 8;
    validate_sample_count(entry_count, "stsc")?;
    validate_table_entries(entry_count, pos, end, 12, "stsc")?;
    let mut entries = Vec::with_capacity(entry_count);
    for _ in 0..entry_count {
        let first_chunk = read_u32(input, pos)?;
        let samples_per_chunk = read_u32(input, pos + 4)?;
        if first_chunk == 0 || samples_per_chunk == 0 {
            return Err(TrimError::Corrupt("invalid stsc entry".into()));
        }
        entries.push((first_chunk, samples_per_chunk));
        pos += 12;
    }
    if chunk_count == 0 {
        return Ok(Vec::new());
    }
    if entries.first().map(|e| e.0) != Some(1) {
        return Err(TrimError::Corrupt(
            "first stsc entry must start at chunk 1".into(),
        ));
    }

    let mut out = Vec::with_capacity(chunk_count);
    let mut entry_idx = 0usize;
    for chunk_number in 1..=chunk_count as u32 {
        while entry_idx + 1 < entries.len() && entries[entry_idx + 1].0 <= chunk_number {
            entry_idx += 1;
        }
        out.push(entries[entry_idx].1);
    }
    Ok(out)
}

fn records_from_tables(
    source_len: usize,
    sizes: &[u32],
    durations: &[u32],
    sync: &[bool],
    chunk_offsets: &[u64],
    samples_per_chunk: &[u32],
) -> Result<Vec<SampleRecord>, TrimError> {
    let expected: usize = samples_per_chunk.iter().map(|&n| n as usize).sum();
    if expected != sizes.len() {
        return Err(TrimError::Corrupt(format!(
            "stsc sample count {expected} does not match stsz count {}",
            sizes.len()
        )));
    }

    let mut out = Vec::with_capacity(sizes.len());
    let mut sample_index = 0usize;
    let mut start_ticks = 0u64;
    for (&chunk_offset, &chunk_samples) in chunk_offsets.iter().zip(samples_per_chunk) {
        let mut offset = usize::try_from(chunk_offset)
            .map_err(|_| TrimError::Corrupt("chunk offset too large".into()))?;
        for _ in 0..chunk_samples {
            let size = sizes[sample_index];
            let end = offset
                .checked_add(size as usize)
                .ok_or_else(|| TrimError::Corrupt("sample offset overflow".into()))?;
            if end > source_len {
                return Err(TrimError::Corrupt(
                    "sample points outside source file".into(),
                ));
            }
            out.push(SampleRecord {
                offset,
                size,
                duration: durations[sample_index],
                is_sync: sync[sample_index],
                start_ticks,
            });
            start_ticks += durations[sample_index] as u64;
            offset = end;
            sample_index += 1;
        }
    }
    Ok(out)
}

pub(crate) fn child(input: &[u8], parent: &BoxInfo, fourcc: &[u8; 4]) -> Option<BoxInfo> {
    children(input, parent)
        .into_iter()
        .find(|b| &b.fourcc == fourcc)
}

pub(crate) fn require_child(input: &[u8], parent: &BoxInfo, fourcc: &[u8; 4]) -> Result<BoxInfo, TrimError> {
    child(input, parent, fourcc)
        .ok_or_else(|| TrimError::Unsupported(format!("missing {} box", fourcc_str(fourcc))))
}

pub(crate) fn find_box_between(
    input: &[u8],
    mut offset: usize,
    end: usize,
    fourcc: &[u8; 4],
) -> Result<Option<BoxInfo>, TrimError> {
    while offset + 8 <= end {
        let b = read_box_at(input, offset, end)?;
        let next = box_end(&b)?;
        if &b.fourcc == fourcc {
            return Ok(Some(b));
        }
        if next <= offset {
            return Err(TrimError::Corrupt("box parser made no progress".into()));
        }
        offset = next;
    }
    Ok(None)
}

pub(crate) fn read_box_at(input: &[u8], offset: usize, limit: usize) -> Result<BoxInfo, TrimError> {
    if offset + 8 > limit || offset + 8 > input.len() {
        return Err(TrimError::Corrupt("truncated box header".into()));
    }
    let size32 = read_u32(input, offset)?;
    let fourcc = read_fourcc(input, offset + 4)?;
    let large_size = if crate::box_header::uses_large_size(size32) {
        if offset + 16 > limit || offset + 16 > input.len() {
            return Err(TrimError::Corrupt("truncated largesize box header".into()));
        }
        Some(read_u64(input, offset + 8)?)
    } else {
        None
    };
    let decoded =
        crate::box_header::decode_box_header(size32, large_size, offset as u64, limit as u64)
            .map_err(|error| box_header_error(error, &fourcc))?;
    if decoded.end > input.len() as u64 {
        return Err(TrimError::Corrupt(format!(
            "invalid {} box size",
            fourcc_str(&fourcc)
        )));
    }
    Ok(BoxInfo {
        fourcc,
        offset: offset as u64,
        size: decoded.size,
        payload_offset: decoded.payload_offset,
    })
}

pub(crate) fn read_finalized_moov_bytes<R: Read + Seek>(reader: &mut R) -> Result<Vec<u8>, TrimError> {
    let file_len = reader.seek(SeekFrom::End(0))?;
    reader.seek(SeekFrom::Start(0))?;

    let mut offset = 0_u64;
    while offset < file_len {
        let top = read_top_level_box(reader, offset, file_len)?;
        if &top.fourcc == b"moov" {
            return read_box_bytes(reader, &top);
        }
        let next = top
            .offset
            .checked_add(top.size)
            .ok_or_else(|| TrimError::Corrupt("top-level box offset overflow".into()))?;
        if next <= offset {
            return Err(TrimError::Corrupt(
                "top-level box parser made no progress".into(),
            ));
        }
        offset = next;
    }

    Err(TrimError::Unsupported("missing finalized moov".into()))
}

fn read_top_level_box<R: Read + Seek>(
    reader: &mut R,
    offset: u64,
    limit: u64,
) -> Result<BoxInfo, TrimError> {
    if offset.checked_add(8).is_none_or(|end| end > limit) {
        return Err(TrimError::Corrupt("truncated box header".into()));
    }

    reader.seek(SeekFrom::Start(offset))?;
    let mut header = [0_u8; 8];
    reader.read_exact(&mut header)?;
    let size32 = u32::from_be_bytes(header[..4].try_into().unwrap());
    let fourcc = header[4..8].try_into().unwrap();

    let large_size = if crate::box_header::uses_large_size(size32) {
        if offset.checked_add(16).is_none_or(|end| end > limit) {
            return Err(TrimError::Corrupt("truncated largesize box header".into()));
        }
        let mut large = [0_u8; 8];
        reader.read_exact(&mut large)?;
        Some(u64::from_be_bytes(large))
    } else {
        None
    };
    let decoded = crate::box_header::decode_box_header(size32, large_size, offset, limit)
        .map_err(|error| box_header_error(error, &fourcc))?;

    Ok(BoxInfo {
        fourcc,
        offset,
        size: decoded.size,
        payload_offset: decoded.payload_offset,
    })
}

fn box_header_error(error: crate::box_header::BoxHeaderError, fourcc: &[u8; 4]) -> TrimError {
    match error {
        crate::box_header::BoxHeaderError::SizeOverflow => {
            TrimError::Corrupt("box size overflow".into())
        }
        crate::box_header::BoxHeaderError::MissingLargeSize
        | crate::box_header::BoxHeaderError::InvalidExtent => {
            TrimError::Corrupt(format!("invalid {} box size", fourcc_str(fourcc)))
        }
    }
}

fn read_box_bytes<R: Read + Seek>(reader: &mut R, b: &BoxInfo) -> Result<Vec<u8>, TrimError> {
    if b.size > MAX_FINALIZED_MOOV_BYTES {
        return Err(TrimError::Unsupported(format!(
            "moov box is too large to inspect ({} bytes > {} byte limit)",
            b.size, MAX_FINALIZED_MOOV_BYTES
        )));
    }
    let size = usize::try_from(b.size)
        .map_err(|_| TrimError::Unsupported("moov box is too large to inspect".into()))?;
    reader.seek(SeekFrom::Start(b.offset))?;
    let mut bytes = vec![0_u8; size];
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
}

pub(crate) fn box_end(b: &BoxInfo) -> Result<usize, TrimError> {
    let end = b
        .offset
        .checked_add(b.size)
        .ok_or_else(|| TrimError::Corrupt("box end offset overflow".into()))?;
    usize::try_from(end).map_err(|_| TrimError::Corrupt("box end offset too large".into()))
}

fn validate_sample_count(count: usize, table: &str) -> Result<(), TrimError> {
    if count > MAX_PARSED_SAMPLES {
        return Err(TrimError::Corrupt(format!(
            "{table} sample count exceeds limit of {MAX_PARSED_SAMPLES}"
        )));
    }
    Ok(())
}

pub(crate) fn validate_table_entries(
    count: usize,
    start: usize,
    end: usize,
    entry_size: usize,
    table: &str,
) -> Result<(), TrimError> {
    let byte_len = count
        .checked_mul(entry_size)
        .ok_or_else(|| TrimError::Corrupt(format!("{table} entry byte count overflow")))?;
    let required_end = start
        .checked_add(byte_len)
        .ok_or_else(|| TrimError::Corrupt(format!("{table} entry range overflow")))?;
    if required_end > end {
        return Err(TrimError::Corrupt(format!("truncated {table}")));
    }
    Ok(())
}

pub(crate) fn read_slice(input: &[u8], offset: usize, len: usize, limit: usize) -> Result<&[u8], TrimError> {
    let end = offset
        .checked_add(len)
        .ok_or_else(|| TrimError::Corrupt("slice offset overflow".into()))?;
    if end > limit {
        return Err(TrimError::Corrupt(
            "slice extends past containing box".into(),
        ));
    }
    input
        .get(offset..end)
        .ok_or_else(|| TrimError::Corrupt("slice extends past file".into()))
}

pub(crate) fn read_u16(input: &[u8], offset: usize) -> Result<u16, TrimError> {
    Ok(u16::from_be_bytes(
        input
            .get(offset..offset + 2)
            .ok_or_else(|| TrimError::Corrupt("truncated u16".into()))?
            .try_into()
            .unwrap(),
    ))
}

pub(crate) fn read_u16_bounded(
    input: &[u8],
    offset: usize,
    limit: usize,
    label: &str,
) -> Result<u16, TrimError> {
    let bytes = read_slice(input, offset, 2, limit)
        .map_err(|_| TrimError::Corrupt(format!("truncated {label}")))?;
    Ok(u16::from_be_bytes(bytes.try_into().unwrap()))
}

fn read_u32(input: &[u8], offset: usize) -> Result<u32, TrimError> {
    Ok(u32::from_be_bytes(
        input
            .get(offset..offset + 4)
            .ok_or_else(|| TrimError::Corrupt("truncated u32".into()))?
            .try_into()
            .unwrap(),
    ))
}

pub(crate) fn read_u32_bounded(
    input: &[u8],
    offset: usize,
    limit: usize,
    label: &str,
) -> Result<u32, TrimError> {
    let bytes = read_slice(input, offset, 4, limit)
        .map_err(|_| TrimError::Corrupt(format!("truncated {label}")))?;
    Ok(u32::from_be_bytes(bytes.try_into().unwrap()))
}

fn read_u64(input: &[u8], offset: usize) -> Result<u64, TrimError> {
    Ok(u64::from_be_bytes(
        input
            .get(offset..offset + 8)
            .ok_or_else(|| TrimError::Corrupt("truncated u64".into()))?
            .try_into()
            .unwrap(),
    ))
}

pub(crate) fn read_u64_bounded(
    input: &[u8],
    offset: usize,
    limit: usize,
    label: &str,
) -> Result<u64, TrimError> {
    let bytes = read_slice(input, offset, 8, limit)
        .map_err(|_| TrimError::Corrupt(format!("truncated {label}")))?;
    Ok(u64::from_be_bytes(bytes.try_into().unwrap()))
}

fn read_fourcc(input: &[u8], offset: usize) -> Result<[u8; 4], TrimError> {
    let mut out = [0u8; 4];
    out.copy_from_slice(
        input
            .get(offset..offset + 4)
            .ok_or_else(|| TrimError::Corrupt("truncated fourcc".into()))?,
    );
    Ok(out)
}

pub(crate) fn read_fourcc_bounded(
    input: &[u8],
    offset: usize,
    limit: usize,
    label: &str,
) -> Result<[u8; 4], TrimError> {
    let mut output = [0_u8; 4];
    output.copy_from_slice(
        read_slice(input, offset, 4, limit)
            .map_err(|_| TrimError::Corrupt(format!("truncated {label}")))?,
    );
    Ok(output)
}

pub(crate) fn fourcc_str(fourcc: &[u8; 4]) -> String {
    String::from_utf8_lossy(fourcc).into_owned()
}

#[cfg(test)]
mod tests {
    use super::super::model::fixtures::*;
    use super::super::model::{MediaTrackCounts, TrimError};
    use super::super::parse::{media_track_counts_reader, parse_movie_reader};
    use super::*;
    use crate::walker::{find, walk};
    use std::io::{Cursor, Read, Seek, SeekFrom};

        struct TrackingCursor {
            inner: Cursor<Vec<u8>>,
            mdat_range: std::ops::Range<u64>,
            bytes_read: usize,
            seeks: Vec<u64>,
        }

        impl TrackingCursor {
            fn new(bytes: Vec<u8>, mdat_range: std::ops::Range<u64>) -> Self {
                Self {
                    inner: Cursor::new(bytes),
                    mdat_range,
                    bytes_read: 0,
                    seeks: Vec::new(),
                }
            }
        }

        impl Read for TrackingCursor {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let position = self.inner.position();
                if position >= self.mdat_range.start && position < self.mdat_range.end {
                    return Err(std::io::Error::other(format!(
                        "reader touched skipped mdat payload at {position}"
                    )));
                }
                let read = self.inner.read(buf)?;
                self.bytes_read += read;
                Ok(read)
            }
        }

        impl Seek for TrackingCursor {
            fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
                let next = self.inner.seek(pos)?;
                self.seeks.push(next);
                Ok(next)
            }
        }

        #[test]
        fn bounded_scalar_reads_do_not_borrow_bytes_from_a_sibling_box() {
            let bytes = [0_u8, 0, 0, 1, 0, 0, 0, 2];
            assert!(read_u32_bounded(&bytes, 2, 4, "test box").is_err());
            assert!(read_u16_bounded(&bytes, 3, 4, "test box").is_err());
            assert!(read_fourcc_bounded(&bytes, 2, 4, "test box").is_err());
        }

        #[test]
        fn stts_rejects_an_excessive_expanded_sample_count() {
            let mut payload = vec![0_u8; 4];
            payload.extend(1_u32.to_be_bytes());
            payload.extend(4_000_001_u32.to_be_bytes());
            payload.extend(1_500_u32.to_be_bytes());
            let input = crate::boxes::mp4_box(*b"stts", payload);
            let info = walk(&input).remove(0);
    
            let err = parse_stts(&input, &info, 4_000_000).unwrap_err();
            assert!(err.to_string().contains("sample count exceeds limit"));
        }

        #[test]
        fn fixed_stsz_rejects_an_excessive_sample_count() {
            let mut payload = vec![0_u8; 4];
            payload.extend(1_u32.to_be_bytes());
            payload.extend(4_000_001_u32.to_be_bytes());
            let input = crate::boxes::mp4_box(*b"stsz", payload);
            let info = walk(&input).remove(0);
    
            let err = parse_stsz(&input, &info).unwrap_err();
            assert!(err.to_string().contains("sample count exceeds limit"));
        }

        #[test]
        fn offset_and_chunk_tables_reject_excessive_entry_counts() {
            fn table(fourcc: [u8; 4]) -> (Vec<u8>, BoxInfo) {
                let mut payload = vec![0_u8; 4];
                payload.extend(4_000_001_u32.to_be_bytes());
                let input = crate::boxes::mp4_box(fourcc, payload);
                let info = walk(&input).remove(0);
                (input, info)
            }
    
            let (co64, co64_info) = table(*b"co64");
            assert!(parse_co64(&co64, &co64_info)
                .unwrap_err()
                .to_string()
                .contains("sample count exceeds limit"));
    
            let (stco, stco_info) = table(*b"stco");
            assert!(parse_stco(&stco, &stco_info)
                .unwrap_err()
                .to_string()
                .contains("sample count exceeds limit"));
    
            let (stsc, stsc_info) = table(*b"stsc");
            assert!(parse_stsc(&stsc, &stsc_info, 1)
                .unwrap_err()
                .to_string()
                .contains("sample count exceeds limit"));
        }

        #[test]
        fn media_track_counts_reader_skips_top_level_mdat_and_reads_only_headers_plus_moov() {
            let fixture = clipline_two_audio_fixture();
            let top = walk(&fixture);
            let mdat = find(&top, b"mdat").expect("fixture has top-level mdat");
            let moov = find(&top, b"moov").expect("fixture has finalized moov");
            let moov_size = usize::try_from(moov.size).unwrap();
            let mut reader =
                TrackingCursor::new(fixture, mdat.payload_offset..(mdat.offset + mdat.size));
    
            let counts = media_track_counts_reader(&mut reader).unwrap();
    
            assert_eq!(counts, MediaTrackCounts { video: 1, audio: 2 });
            assert!(
                reader
                    .seeks
                    .iter()
                    .any(|offset| *offset >= mdat.offset + mdat.size),
                "expected seek past mdat payload, got {:?}",
                reader.seeks
            );
            assert!(
                reader.bytes_read <= moov_size + 128,
                "expected bounded reads, got {} bytes for moov size {moov_size}",
                reader.bytes_read
            );
        }

        #[test]
        fn movie_reader_skips_mdat_payload_while_recovering_sample_offsets() {
            let fixture = clipline_two_audio_fixture();
            let top = walk(&fixture);
            let mdat = find(&top, b"mdat").expect("fixture has top-level mdat");
            let moov = find(&top, b"moov").expect("fixture has finalized moov");
            let moov_size = usize::try_from(moov.size).unwrap();
            let mut reader =
                TrackingCursor::new(fixture, mdat.payload_offset..(mdat.offset + mdat.size));
    
            let movie = parse_movie_reader(&mut reader).unwrap();
    
            assert_eq!(movie.tracks.len(), 3);
            let first_offset = movie.tracks[0].samples[0].offset as u64;
            assert!(
                first_offset >= mdat.payload_offset && first_offset < mdat.offset + mdat.size,
                "sample offset {first_offset} should remain inside the source mdat"
            );
            assert!(reader.bytes_read <= moov_size + 128);
        }

        #[test]
        fn media_track_counts_reader_rejects_oversized_declared_moov_before_allocation() {
            struct LargeMoovReader {
                pos: u64,
                len: u64,
            }
    
            impl Read for LargeMoovReader {
                fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                    const HEADER: [u8; 8] = [0, 0, 0, 0, b'm', b'o', b'o', b'v'];
                    if self.pos >= self.len {
                        return Ok(0);
                    }
                    let mut read = 0usize;
                    while read < buf.len() && self.pos < self.len {
                        buf[read] = if self.pos < HEADER.len() as u64 {
                            HEADER[self.pos as usize]
                        } else {
                            0
                        };
                        self.pos += 1;
                        read += 1;
                    }
                    Ok(read)
                }
            }
    
            impl Seek for LargeMoovReader {
                fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
                    let next = match pos {
                        SeekFrom::Start(offset) => offset,
                        SeekFrom::End(offset) => self
                            .len
                            .checked_add_signed(offset)
                            .ok_or_else(|| std::io::Error::other("seek overflow"))?,
                        SeekFrom::Current(offset) => self
                            .pos
                            .checked_add_signed(offset)
                            .ok_or_else(|| std::io::Error::other("seek overflow"))?,
                    };
                    self.pos = next;
                    Ok(self.pos)
                }
            }
    
            let mut reader = LargeMoovReader {
                pos: 0,
                len: MAX_FINALIZED_MOOV_BYTES + 1,
            };
    
            let err = media_track_counts_reader(&mut reader).unwrap_err();
    
            assert!(
                err.to_string().contains("moov box is too large to inspect"),
                "{err}"
            );
        }

        #[test]
        fn read_top_level_box_supports_extended_size_boxes() {
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&1u32.to_be_bytes());
            bytes.extend_from_slice(b"moov");
            bytes.extend_from_slice(&24u64.to_be_bytes());
            bytes.extend_from_slice(&[0u8; 8]);
            let limit = bytes.len() as u64;
            let mut reader = Cursor::new(bytes);
    
            let b = read_top_level_box(&mut reader, 0, limit).unwrap();
    
            assert_eq!(b.fourcc, *b"moov");
            assert_eq!(b.size, 24);
            assert_eq!(b.payload_offset, 16);
        }

        #[test]
        fn read_top_level_box_treats_size_zero_as_terminal_box() {
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&8u32.to_be_bytes());
            bytes.extend_from_slice(b"ftyp");
            bytes.extend_from_slice(&0u32.to_be_bytes());
            bytes.extend_from_slice(b"mdat");
            bytes.extend_from_slice(&[0u8; 5]);
            let bytes_len = bytes.len();
            let limit = bytes.len() as u64;
            let mut reader = Cursor::new(bytes);
    
            let b = read_top_level_box(&mut reader, 8, limit).unwrap();
    
            assert_eq!(b.fourcc, *b"mdat");
            assert_eq!(b.size, (bytes_len - 8) as u64);
            assert_eq!(b.payload_offset, 16);
        }

        #[test]
        fn read_top_level_box_rejects_truncated_header() {
            let mut reader = Cursor::new(vec![0, 0, 0, 8, b'm', b'o', b'o']);
    
            let err = read_top_level_box(&mut reader, 0, 7).unwrap_err();
    
            assert!(err.to_string().contains("truncated box header"), "{err}");
        }

        #[test]
        fn read_top_level_box_rejects_truncated_extended_header() {
            let mut reader = Cursor::new({
                let mut bytes = Vec::new();
                bytes.extend_from_slice(&1u32.to_be_bytes());
                bytes.extend_from_slice(b"moov");
                bytes.extend_from_slice(&[0u8; 4]);
                bytes
            });
    
            let err = read_top_level_box(&mut reader, 0, 12).unwrap_err();
    
            assert!(
                err.to_string().contains("truncated largesize box header"),
                "{err}"
            );
        }

        #[test]
        fn read_box_bytes_rejects_truncated_payload() {
            let mut reader = Cursor::new(vec![0u8; 12]);
            let b = BoxInfo {
                fourcc: *b"moov",
                offset: 0,
                size: 16,
                payload_offset: 8,
            };
    
            let err = read_box_bytes(&mut reader, &b).unwrap_err();
    
            assert!(matches!(err, TrimError::Io(_)), "{err}");
        }

        #[test]
        fn read_top_level_box_rejects_too_small_box_size() {
            let mut reader = Cursor::new({
                let mut bytes = Vec::new();
                bytes.extend_from_slice(&4u32.to_be_bytes());
                bytes.extend_from_slice(b"moov");
                bytes
            });
    
            let err = read_top_level_box(&mut reader, 0, 8).unwrap_err();
    
            assert!(err.to_string().contains("invalid moov box size"), "{err}");
        }

        #[test]
        fn read_top_level_box_rejects_box_extent_past_file() {
            let mut reader = Cursor::new({
                let mut bytes = Vec::new();
                bytes.extend_from_slice(&32u32.to_be_bytes());
                bytes.extend_from_slice(b"moov");
                bytes.extend_from_slice(&[0u8; 8]);
                bytes
            });
    
            let err = read_top_level_box(&mut reader, 0, 16).unwrap_err();
    
            assert!(err.to_string().contains("invalid moov box size"), "{err}");
        }

        #[test]
        fn read_finalized_moov_bytes_reports_missing_moov_at_eof_without_looping() {
            let mut reader = Cursor::new({
                let mut bytes = Vec::new();
                bytes.extend_from_slice(&8u32.to_be_bytes());
                bytes.extend_from_slice(b"ftyp");
                bytes.extend_from_slice(&0u32.to_be_bytes());
                bytes.extend_from_slice(b"mdat");
                bytes.extend_from_slice(&[0u8; 3]);
                bytes
            });
    
            let err = read_finalized_moov_bytes(&mut reader).unwrap_err();
    
            assert!(err.to_string().contains("missing finalized moov"), "{err}");
        }
}
