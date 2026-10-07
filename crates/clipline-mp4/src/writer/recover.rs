//! Finalize a Hybrid MP4 whose writer died before `finalize()` (ddoc §10).
//!
//! The fragmented layout is ftyp / free placeholder / init moov / moof+mdat…
//! Recovery rebuilds the writer's state from that layout and runs the same
//! `finalize()`. Every complete box must re-encode to the exact bytes on disk,
//! so recovery only ever completes files this writer produced.

use std::fs::{File, OpenOptions};
use std::io::{self, Cursor, Read, Seek, SeekFrom, Write};
use std::path::Path;

use super::track_state::TrackState;
use super::{validate_track_configs, HybridMp4Writer};
use crate::fragment::{fragment_moof_multi, mdat_header, FragSampleInfo, TrackRunInfo};
use crate::init::{free_placeholder, ftyp, moov_init_multi, TrackConfig};
use crate::trim::parse_track_cfg;
use crate::walker::{children, walk};

/// trun sample_flags the writer emits; anything else was not written by it.
const FLAG_SYNC: u32 = 0x0200_0000;
const FLAG_NON_SYNC: u32 = 0x0101_0000;
/// Far above anything the writer emits; bounds reads of untrusted sizes.
const MAX_INIT_MOOV_BYTES: u64 = 16 << 20;
const MAX_MOOF_BYTES: u64 = 64 << 20;

/// What [`finalize_interrupted_recording`] found and did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptedRecording {
    /// Not an unfinalized Hybrid MP4; left untouched.
    NotInterrupted,
    /// An unfinalized Hybrid MP4 with no complete fragment; left untouched.
    Empty,
    /// Finalized in place from its complete fragments.
    Finalized {
        fragments: u32,
        /// Bytes after the last complete fragment that were cut off: a torn
        /// fragment or the moov of an interrupted `finalize()`.
        discarded_tail_bytes: u64,
    },
}

/// Storage that recovery can truncate and flush to stable media.
pub trait RecoveryTarget: Read + Write + Seek {
    fn set_len(&mut self, len: u64) -> io::Result<()>;
    fn sync_data(&mut self) -> io::Result<()>;
}

impl RecoveryTarget for File {
    fn set_len(&mut self, len: u64) -> io::Result<()> {
        File::set_len(self, len)
    }

    fn sync_data(&mut self) -> io::Result<()> {
        File::sync_data(self)
    }
}

impl RecoveryTarget for Cursor<Vec<u8>> {
    fn set_len(&mut self, len: u64) -> io::Result<()> {
        let len = usize::try_from(len)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "length exceeds memory"))?;
        self.get_mut().resize(len, 0);
        Ok(())
    }

    fn sync_data(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Finalize an interrupted Hybrid MP4 in place, exactly as the writer's own
/// `finalize()` would have for the same complete fragments. Files that aren't
/// unfinalized Hybrid MP4s are left untouched; a complete fragment this
/// writer could not have produced is an `InvalidData` error and also leaves
/// the file untouched.
pub fn finalize_interrupted_recording<T: RecoveryTarget>(
    target: &mut T,
) -> io::Result<InterruptedRecording> {
    let Some(header) = probe(target)? else {
        return Ok(InterruptedRecording::NotInterrupted);
    };
    let file_len = target.seek(SeekFrom::End(0))?;
    let ftyp = ftyp();

    // Everything is read and validated before the first byte is written.
    let init = match read_box(target, HEADER_LEN, file_len, MAX_INIT_MOOV_BYTES)? {
        // Died while writing the init moov: nothing was recorded.
        BoxRead::Torn(None | Some(MOOV)) => return Ok(InterruptedRecording::Empty),
        BoxRead::Complete(init) if init.fourcc == MOOV => init,
        _ => return Err(invalid_data("expected the init moov after the placeholder")),
    };
    let tracks = parse_init_moov(&init.bytes)?;
    let track_count = tracks.len();
    let mut writer = HybridMp4Writer {
        w: &mut *target,
        tracks: tracks.into_iter().map(TrackState::new).collect(),
        free_offset: ftyp.len() as u64,
        next_sequence: 1,
    };

    let mut end = HEADER_LEN + init.len;
    let mut fragments = 0_u32;
    while let Some(fragment) = read_fragment(writer.w, end, file_len, track_count, writer.next_sequence)? {
        let mut infos = vec![Vec::new(); track_count];
        for run in fragment.runs {
            let track_index = run.track_id as usize - 1;
            writer
                .set_track_decode_time(track_index, run.base_decode_time)
                .map_err(replay_error)?;
            infos[track_index] = run.samples;
        }
        writer
            .record_fragment(fragment.payload_start, &infos)
            .map_err(replay_error)?;
        fragments += 1;
        end = fragment.end;
    }
    if fragments == 0 {
        return Ok(InterruptedRecording::Empty);
    }
    if let Header::Flipped { moov_offset } = header {
        if moov_offset != end {
            return Err(invalid_data("finalized header does not match the fragments"));
        }
    }

    let discarded_tail_bytes = file_len - end;
    if discarded_tail_bytes > 0 {
        writer.w.set_len(end)?;
        writer.w.sync_data()?;
    }
    writer.w.seek(SeekFrom::Start(end))?;
    writer.finalize_with_sync(|w| w.sync_data())?;
    Ok(InterruptedRecording::Finalized {
        fragments,
        discarded_tail_bytes,
    })
}

/// [`finalize_interrupted_recording`] for a file on disk. A file that only
/// looks interrupted is opened for writing, and on Windows only while no
/// other process can write to or delete it: a live recorder is refused, not
/// truncated underneath.
pub fn finalize_interrupted_recording_file(path: &Path) -> io::Result<InterruptedRecording> {
    if !is_interrupted_recording(&mut File::open(path)?)? {
        return Ok(InterruptedRecording::NotInterrupted);
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x1;
        options.share_mode(FILE_SHARE_READ);
    }
    finalize_interrupted_recording(&mut options.open(path)?)
}

/// Whether `r` looks like a Hybrid MP4 whose `finalize()` never completed.
/// Reads at most two small headers, so it is cheap enough for every owned MP4.
pub fn is_interrupted_recording<R: Read + Seek>(r: &mut R) -> io::Result<bool> {
    Ok(probe(r)?.is_some())
}

const HEADER_LEN: u64 = 44;

enum Header {
    /// The free placeholder: `finalize()` never started its header flip.
    Placeholder,
    /// The header was flipped but the moov it points at never fully landed.
    Flipped { moov_offset: u64 },
}

fn probe<R: Read + Seek>(r: &mut R) -> io::Result<Option<Header>> {
    let file_len = r.seek(SeekFrom::End(0))?;
    if file_len < HEADER_LEN {
        return Ok(None);
    }
    let head = read_at(r, 0, HEADER_LEN)?;
    let (ftyp_bytes, rest) = head.split_at(ftyp().len());
    if ftyp_bytes != ftyp() {
        return Ok(None);
    }
    if rest == free_placeholder() {
        return Ok(Some(Header::Placeholder));
    }
    if rest[..8] != [0, 0, 0, 1, b'm', b'd', b'a', b't'] {
        return Ok(None);
    }
    let span = u64::from_be_bytes(rest[8..].try_into().expect("16-byte mdat header"));
    let Some(moov_offset) = span.checked_add(ftyp_bytes.len() as u64) else {
        return Ok(None);
    };
    if moov_offset > file_len {
        return Ok(None);
    }
    match read_box_header(r, moov_offset, file_len)? {
        Some((MOOV, len)) if len <= file_len - moov_offset => Ok(None),
        _ => Ok(Some(Header::Flipped { moov_offset })),
    }
}

/// Rebuild the track list, insisting it re-encodes to the same init moov so
/// the final moov describes the tracks exactly as the writer would have.
fn parse_init_moov(moov: &[u8]) -> io::Result<Vec<TrackConfig>> {
    let top = walk(moov);
    let [moov_box] = top.as_slice() else {
        return Err(invalid_data("init moov is not a single box"));
    };
    let tracks = children(moov, moov_box)
        .iter()
        .filter(|child| &child.fourcc == b"trak")
        .map(|trak| parse_track_cfg(moov, trak).map_err(|e| invalid_data(e.to_string())))
        .collect::<io::Result<Vec<_>>>()?;
    validate_track_configs(&tracks).map_err(|e| invalid_data(e.to_string()))?;
    if moov_init_multi(&tracks) != moov {
        return Err(invalid_data("init moov does not match the tracks it describes"));
    }
    Ok(tracks)
}

struct Fragment {
    runs: Vec<Run>,
    payload_start: u64,
    end: u64,
}

struct Run {
    track_id: u32,
    base_decode_time: u64,
    samples: Vec<FragSampleInfo>,
}

/// The complete fragment at `pos`, or `None` at the end of the complete
/// fragments: end of file, a torn fragment, or an interrupted finalize's moov.
fn read_fragment<R: Read + Seek>(
    r: &mut R,
    pos: u64,
    file_len: u64,
    track_count: usize,
    sequence: u32,
) -> io::Result<Option<Fragment>> {
    // Only what a crash can leave behind may end the fragments: a torn moof,
    // the moov of an interrupted finalize(), or a zero-filled tail. Anything
    // else is unexplained data that recovery must not truncate.
    let moof = match read_box(r, pos, file_len, MAX_MOOF_BYTES)? {
        BoxRead::Complete(b) if b.fourcc == *b"moof" => b,
        BoxRead::Complete(b) if b.fourcc == *b"moov" => return Ok(None),
        BoxRead::Torn(None | Some(MOOF | MOOV)) => return Ok(None),
        _ if is_zero_fill(r, pos, file_len)? => return Ok(None),
        _ => return Err(invalid_data(format!("unexpected data after fragment at byte {pos}"))),
    };
    let runs = parse_moof(&moof.bytes, track_count)?;
    let infos: Vec<TrackRunInfo<'_>> = runs
        .iter()
        .map(|run| TrackRunInfo {
            track_id: run.track_id,
            base_decode_time: run.base_decode_time,
            samples: &run.samples,
        })
        .collect();
    let rebuilt = fragment_moof_multi(sequence, &infos).map_err(|e| invalid_data(e.to_string()))?;
    if rebuilt != moof.bytes {
        return Err(invalid_data(format!("fragment at byte {pos} was not written by this writer")));
    }

    let payload_len = runs
        .iter()
        .flat_map(|run| &run.samples)
        .map(|sample| u64::from(sample.size))
        .sum::<u64>();
    let mdat = mdat_header(payload_len);
    let mdat_start = pos + moof.len;
    let payload_start = mdat_start + mdat.len() as u64;
    let Some(end) = payload_start.checked_add(payload_len).filter(|end| *end <= file_len) else {
        return Ok(None);
    };
    if read_at(r, mdat_start, mdat.len() as u64)? != mdat {
        return Err(invalid_data(format!("fragment at byte {pos} has an unexpected mdat header")));
    }
    Ok(Some(Fragment {
        runs,
        payload_start,
        end,
    }))
}

/// Parse the trafs of one moof. Exactness is checked by re-encoding, so this
/// only needs to extract values and reject what can't be re-encoded.
fn parse_moof(moof: &[u8], track_count: usize) -> io::Result<Vec<Run>> {
    let top = walk(moof);
    let [moof_box] = top.as_slice() else {
        return Err(invalid_data("moof is not a single box"));
    };
    let mut runs = Vec::new();
    for traf in children(moof, moof_box).iter().filter(|b| &b.fourcc == b"traf") {
        let parts = children(moof, traf);
        let [tfhd, tfdt, trun] = parts.as_slice() else {
            return Err(invalid_data("traf must hold tfhd, tfdt and trun"));
        };
        let track_id = read_u32(moof, tfhd.payload_offset + 4)?;
        if track_id == 0 || track_id as usize > track_count {
            return Err(invalid_data(format!("fragment names unknown track {track_id}")));
        }
        let base_decode_time = read_u64(moof, tfdt.payload_offset + 4)?;
        let sample_count = read_u32(moof, trun.payload_offset + 4)?;
        let mut samples = Vec::new();
        for index in 0..u64::from(sample_count) {
            let entry = trun.payload_offset + 12 + index * 12;
            let flags = read_u32(moof, entry + 8)?;
            if flags != FLAG_SYNC && flags != FLAG_NON_SYNC {
                return Err(invalid_data("fragment has unknown sample flags"));
            }
            samples.push(FragSampleInfo {
                duration: read_u32(moof, entry)?,
                size: read_u32(moof, entry + 4)?,
                is_sync: flags == FLAG_SYNC,
            });
        }
        runs.push(Run {
            track_id,
            base_decode_time,
            samples,
        });
    }
    if runs.is_empty() {
        return Err(invalid_data("fragment has no tracks"));
    }
    Ok(runs)
}

struct ReadBox {
    fourcc: [u8; 4],
    len: u64,
    bytes: Vec<u8>,
}

const MOOF: [u8; 4] = *b"moof";
const MOOV: [u8; 4] = *b"moov";

enum BoxRead {
    Complete(ReadBox),
    /// Runs past the end of the file; the fourcc when the header survived.
    Torn(Option<[u8; 4]>),
    /// Not a plausible box header.
    Foreign,
}

fn read_box<R: Read + Seek>(r: &mut R, pos: u64, file_len: u64, max_len: u64) -> io::Result<BoxRead> {
    let Some((fourcc, len)) = read_box_header(r, pos, file_len)? else {
        return Ok(BoxRead::Torn(None));
    };
    if len == u64::MAX {
        return Ok(BoxRead::Torn(Some(fourcc)));
    }
    if len < 8 || len > max_len {
        return Ok(BoxRead::Foreign);
    }
    if len > file_len - pos {
        return Ok(BoxRead::Torn(Some(fourcc)));
    }
    Ok(BoxRead::Complete(ReadBox {
        fourcc,
        len,
        bytes: read_at(r, pos, len)?,
    }))
}

/// The fourcc and total size of the box at `pos`, or `None` when fewer than
/// 8 bytes remain. A large-size header cut off by the end of the file reports
/// `u64::MAX`, which is longer than any file.
fn read_box_header<R: Read + Seek>(
    r: &mut R,
    pos: u64,
    file_len: u64,
) -> io::Result<Option<([u8; 4], u64)>> {
    let remaining = file_len - pos;
    if remaining < 8 {
        return Ok(None);
    }
    let head = read_at(r, pos, 8)?;
    let fourcc: [u8; 4] = head[4..8].try_into().expect("8-byte header");
    let len = match u32::from_be_bytes(head[..4].try_into().expect("8-byte header")) {
        1 if remaining < 16 => u64::MAX,
        1 => u64::from_be_bytes(read_at(r, pos + 8, 8)?.try_into().expect("8-byte size")),
        len => u64::from(len),
    };
    Ok(Some((fourcc, len)))
}

/// A crash can leave a zero-filled tail where the OS extended the file
/// before the data landed.
fn is_zero_fill<R: Read + Seek>(r: &mut R, pos: u64, file_len: u64) -> io::Result<bool> {
    r.seek(SeekFrom::Start(pos))?;
    let mut chunk = vec![0; 64 * 1024];
    let mut remaining = file_len - pos;
    while remaining > 0 {
        let n = remaining.min(chunk.len() as u64) as usize;
        r.read_exact(&mut chunk[..n])?;
        if chunk[..n].iter().any(|&byte| byte != 0) {
            return Ok(false);
        }
        remaining -= n as u64;
    }
    Ok(true)
}

fn read_at<R: Read + Seek>(r: &mut R, pos: u64, len: u64) -> io::Result<Vec<u8>> {
    let len = usize::try_from(len).map_err(|_| invalid_data("box exceeds memory"))?;
    r.seek(SeekFrom::Start(pos))?;
    let mut bytes = vec![0; len];
    r.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn read_u32(bytes: &[u8], offset: u64) -> io::Result<u32> {
    let offset = offset as usize;
    bytes
        .get(offset..offset + 4)
        .map(|b| u32::from_be_bytes(b.try_into().expect("4 bytes")))
        .ok_or_else(|| invalid_data("box is too short"))
}

fn read_u64(bytes: &[u8], offset: u64) -> io::Result<u64> {
    let offset = offset as usize;
    bytes
        .get(offset..offset + 8)
        .map(|b| u64::from_be_bytes(b.try_into().expect("8 bytes")))
        .ok_or_else(|| invalid_data("box is too short"))
}

fn replay_error(error: io::Error) -> io::Error {
    invalid_data(format!("fragments cannot be replayed: {error}"))
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::super::track_state::support::{all_sync_gop, audio_cfg, gop, read_u32_at, video_cfg};
    use super::*;
    use crate::fragment::FragSample;
    use crate::init::{AudioTrackConfig, TrackConfig, VideoCodecParams, VideoTrackConfig};
    use crate::trim::fixtures::{AV1_SEQ_OBU, HEVC_PPS, HEVC_SPS, HEVC_VPS};
    use crate::HybridMp4Writer;

    fn session_tracks() -> Vec<TrackConfig> {
        vec![TrackConfig::Video(video_cfg()), TrackConfig::Audio(audio_cfg())]
    }

    /// Writes the first `fragments` of a scripted session that exercises
    /// absorbed sub-frame video gaps, explicit audio gaps and single-track
    /// fragments, then either finalizes or abandons it like a killed process.
    fn session(fragments: usize, finalize: bool) -> Vec<u8> {
        let mut w = HybridMp4Writer::new_multi(Cursor::new(Vec::new()), session_tracks()).unwrap();
        for step in 0..fragments {
            match step {
                0 => w.write_fragment_multi(&[&gop(0), &all_sync_gop()]).unwrap(),
                1 => {
                    w.set_track_decode_time(0, 9_001).unwrap();
                    w.write_fragment_multi(&[&gop(3), &all_sync_gop()]).unwrap();
                }
                2 => {
                    let audio_end = w.track_decode_time(1).unwrap();
                    w.set_track_decode_time(1, audio_end + 48_000).unwrap();
                    w.write_fragment_multi(&[&gop(6), &all_sync_gop()]).unwrap();
                }
                3 => w.write_fragment_multi(&[&gop(9), &[]]).unwrap(),
                4 => w.write_fragment_multi(&[&[], &all_sync_gop()]).unwrap(),
                _ => unreachable!("the scripted session has five fragments"),
            }
        }
        let writer = w;
        if finalize {
            writer.finalize().unwrap().into_inner()
        } else {
            writer.into_inner().into_inner()
        }
    }

    fn recover(bytes: Vec<u8>) -> (io::Result<InterruptedRecording>, Vec<u8>) {
        let mut target = Cursor::new(bytes);
        let outcome = finalize_interrupted_recording(&mut target);
        (outcome, target.into_inner())
    }

    fn finalized(fragments: u32, discarded_tail_bytes: u64) -> InterruptedRecording {
        InterruptedRecording::Finalized {
            fragments,
            discarded_tail_bytes,
        }
    }

    #[test]
    fn recovers_to_the_same_bytes_as_a_clean_finalize() {
        for fragments in 1..=5 {
            let (outcome, bytes) = recover(session(fragments, false));
            assert_eq!(outcome.unwrap(), finalized(fragments as u32, 0), "{fragments} fragments");
            assert!(bytes == session(fragments, true), "{fragments} fragments");
        }
    }

    #[test]
    fn cuts_a_torn_last_fragment() {
        let whole = session(5, false);
        let complete = session(4, false).len();
        let moof_len = read_u32_at(&whole, complete) as usize;
        let cuts = [
            complete + 3,            // inside the moof header
            complete + 20,           // inside the moof
            complete + moof_len + 4, // inside the mdat header
            whole.len() - 1,         // inside the sample payload
        ];
        for cut in cuts {
            let (outcome, bytes) = recover(whole[..cut].to_vec());
            assert_eq!(outcome.unwrap(), finalized(4, (cut - complete) as u64), "cut at {cut}");
            assert!(bytes == session(4, true), "cut at {cut}");
        }
    }

    #[test]
    fn recovers_a_file_killed_during_finalize() {
        let killed = session(5, false);
        let clean = session(5, true);
        // finalize() appends the moov, then flips the header; die in between.
        let moov = &clean[killed.len()..];
        for appended in [moov.len(), moov.len() / 2] {
            let torn = [killed.as_slice(), &moov[..appended]].concat();
            let (outcome, bytes) = recover(torn);
            assert_eq!(outcome.unwrap(), finalized(5, appended as u64));
            assert!(bytes == clean, "{appended} moov bytes appended");
        }
    }

    #[test]
    fn recovers_from_a_cut_inside_any_fragment() {
        let whole = session(5, false);
        for complete in 0..5 {
            // Cut inside fragment `complete + 1`, e.g. one whose tfdt would
            // otherwise have stretched the preceding sample.
            let cut = session(complete + 1, false).len() - 1;
            let (outcome, bytes) = recover(whole[..cut].to_vec());
            if complete == 0 {
                assert_eq!(outcome.unwrap(), InterruptedRecording::Empty);
                continue;
            }
            let discarded = (cut - session(complete, false).len()) as u64;
            assert_eq!(outcome.unwrap(), finalized(complete as u32, discarded));
            assert!(bytes == session(complete, true), "{complete} complete fragments");
        }
    }

    #[test]
    fn recovers_a_flipped_header_whose_moov_never_landed() {
        let killed = session(5, false);
        let clean = session(5, true);
        let moov_len = clean.len() - killed.len();
        assert!(!is_interrupted_recording(&mut Cursor::new(clean.clone())).unwrap());
        for landed in [0, 7, moov_len / 2, moov_len - 1] {
            let torn = clean[..killed.len() + landed].to_vec();
            assert!(is_interrupted_recording(&mut Cursor::new(torn.clone())).unwrap());
            let (outcome, bytes) = recover(torn);
            assert_eq!(outcome.unwrap(), finalized(5, landed as u64));
            assert!(bytes == clean, "{landed} moov bytes landed");
        }
    }

    #[test]
    fn recovers_sessions_whose_middle_track_skips_fragments() {
        let write = |finalize: bool| {
            let tracks = vec![
                TrackConfig::Video(video_cfg()),
                TrackConfig::Audio(audio_cfg()),
                TrackConfig::Audio(AudioTrackConfig {
                    channels: 1,
                    ..audio_cfg()
                }),
            ];
            let mut w = HybridMp4Writer::new_multi(Cursor::new(Vec::new()), tracks).unwrap();
            w.write_fragment_multi(&[&gop(0), &[], &all_sync_gop()]).unwrap();
            w.write_fragment_multi(&[&gop(3), &all_sync_gop(), &all_sync_gop()]).unwrap();
            w.write_fragment_multi(&[&gop(6), &[], &all_sync_gop()]).unwrap();
            if finalize {
                w.finalize().unwrap().into_inner()
            } else {
                w.into_inner().into_inner()
            }
        };
        let (outcome, bytes) = recover(write(false));
        assert_eq!(outcome.unwrap(), finalized(3, 0));
        assert!(bytes == write(true));
    }

    #[test]
    fn rejects_an_init_moov_whose_tracks_do_not_round_trip() {
        let killed = session(3, false);
        let dops = killed.windows(4).position(|w| w == b"dOps").unwrap();
        let rate = dops + 4 + 4; // dOps payload: version, channels, pre-skip, rate
        for tampered_rate in [0_u32, 24_000] {
            let mut input = killed.clone();
            input[rate..rate + 4].copy_from_slice(&tampered_rate.to_be_bytes());
            let (outcome, bytes) = recover(input.clone());
            assert_eq!(outcome.unwrap_err().kind(), io::ErrorKind::InvalidData, "{tampered_rate}");
            assert!(bytes == input, "rejected input must stay untouched");
        }
    }

    /// Counts bytes read so tests can bound recovery's I/O.
    struct CountingCursor {
        inner: Cursor<Vec<u8>>,
        bytes_read: u64,
    }

    impl Read for CountingCursor {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = self.inner.read(buf)?;
            self.bytes_read += n as u64;
            Ok(n)
        }
    }

    impl Write for CountingCursor {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.inner.write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Seek for CountingCursor {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            self.inner.seek(pos)
        }
    }

    impl RecoveryTarget for CountingCursor {
        fn set_len(&mut self, len: u64) -> io::Result<()> {
            self.inner.set_len(len)
        }

        fn sync_data(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn recovery_reads_headers_and_skips_sample_payloads() {
        let big_samples = |finalize: bool| {
            let mut w = HybridMp4Writer::new(Cursor::new(Vec::new()), video_cfg()).unwrap();
            for _ in 0..8 {
                let samples: Vec<FragSample> = (0..3)
                    .map(|i| FragSample {
                        data: vec![0xAB; 256 * 1024],
                        duration: 3000,
                        is_sync: i == 0,
                    })
                    .collect();
                w.write_fragment(&samples).unwrap();
            }
            if finalize {
                w.finalize().unwrap().into_inner()
            } else {
                w.into_inner().into_inner()
            }
        };
        let killed = big_samples(false);
        let mut target = CountingCursor {
            inner: Cursor::new(killed.clone()),
            bytes_read: 0,
        };
        assert_eq!(finalize_interrupted_recording(&mut target).unwrap(), finalized(8, 0));
        assert!(
            target.bytes_read < 16 * 1024,
            "read {} of {} bytes",
            target.bytes_read,
            killed.len()
        );

        let mut clean = CountingCursor {
            inner: Cursor::new(big_samples(true)),
            bytes_read: 0,
        };
        assert!(!is_interrupted_recording(&mut clean).unwrap());
        assert!(clean.bytes_read <= 64, "probe read {} bytes", clean.bytes_read);
    }

    #[test]
    fn file_recovery_finalizes_on_disk_and_refuses_a_file_still_being_written() {
        let path = std::env::temp_dir().join(format!(
            "clipline-mp4-recover-{}-{:?}.mp4",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&path, session(3, false)).unwrap();

        #[cfg(windows)]
        {
            let live_writer = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
            assert!(finalize_interrupted_recording_file(&path).is_err());
            drop(live_writer);
            assert!(std::fs::read(&path).unwrap() == session(3, false));
        }

        assert_eq!(finalize_interrupted_recording_file(&path).unwrap(), finalized(3, 0));
        assert!(std::fs::read(&path).unwrap() == session(3, true));
        assert_eq!(
            finalize_interrupted_recording_file(&path).unwrap(),
            InterruptedRecording::NotInterrupted
        );
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn cuts_a_zero_filled_tail_but_refuses_unexplained_data() {
        let killed = session(4, false);
        let zero_filled = [killed.as_slice(), &[0; 4096]].concat();
        let (outcome, bytes) = recover(zero_filled);
        assert_eq!(outcome.unwrap(), finalized(4, 4096));
        assert!(bytes == session(4, true));

        let mut junk_box = Vec::from(64_u32.to_be_bytes());
        junk_box.extend(b"junk");
        junk_box.resize(64, 0x5A);
        for tail in [junk_box.clone(), junk_box[..32].to_vec(), b"garbage!".to_vec()] {
            let input = [killed.as_slice(), &tail].concat();
            let (outcome, bytes) = recover(input.clone());
            assert_eq!(outcome.unwrap_err().kind(), io::ErrorKind::InvalidData);
            assert!(bytes == input, "unexplained data must not be truncated");
        }
    }

    #[test]
    fn reports_a_recording_without_fragments_as_empty() {
        let killed = session(0, false);
        let (outcome, bytes) = recover(killed.clone());
        assert_eq!(outcome.unwrap(), InterruptedRecording::Empty);
        assert!(bytes == killed);
    }

    #[test]
    fn leaves_finalized_and_foreign_files_untouched() {
        for input in [
            session(3, true),
            b"definitely not an mp4".to_vec(),
            Vec::new(),
        ] {
            let (outcome, bytes) = recover(input.clone());
            assert_eq!(outcome.unwrap(), InterruptedRecording::NotInterrupted);
            assert!(bytes == input);
        }
    }

    #[test]
    fn recovery_is_idempotent() {
        let (_, recovered) = recover(session(5, false));
        let (outcome, bytes) = recover(recovered.clone());
        assert_eq!(outcome.unwrap(), InterruptedRecording::NotInterrupted);
        assert!(bytes == recovered);
    }

    #[test]
    fn rejects_complete_data_this_writer_could_not_have_written() {
        let killed = session(3, false);
        let last_trun = killed.windows(4).rposition(|w| w == b"trun").unwrap();
        let avc1 = killed.windows(4).position(|w| w == b"avc1").unwrap();
        let mut unknown_trun_flags = killed.clone();
        unknown_trun_flags[last_trun + 7] |= 0x04; // first-sample-flags-present
        let mut unknown_sample_entry = killed.clone();
        unknown_sample_entry[avc1..avc1 + 4].copy_from_slice(b"xvid");

        for input in [unknown_trun_flags, unknown_sample_entry] {
            let (outcome, bytes) = recover(input.clone());
            assert_eq!(outcome.unwrap_err().kind(), io::ErrorKind::InvalidData);
            assert!(bytes == input, "rejected input must stay untouched");
        }
    }

    #[test]
    fn recovers_hevc_and_av1_sessions() {
        for codec in [
            VideoCodecParams::Hevc {
                vps: vec![HEVC_VPS.to_vec()],
                sps: vec![HEVC_SPS.to_vec()],
                pps: vec![HEVC_PPS.to_vec()],
            },
            VideoCodecParams::Av1 {
                sequence_header_obu: AV1_SEQ_OBU.to_vec(),
            },
        ] {
            let write = |finalize: bool| {
                let cfg = VideoTrackConfig {
                    codec: codec.clone(),
                    ..video_cfg()
                };
                let mut writer = HybridMp4Writer::new(Cursor::new(Vec::new()), cfg).unwrap();
                writer.write_fragment(&gop(0)).unwrap();
                writer.write_fragment(&gop(3)).unwrap();
                if finalize {
                    writer.finalize().unwrap().into_inner()
                } else {
                    writer.into_inner().into_inner()
                }
            };
            let (outcome, bytes) = recover(write(false));
            assert_eq!(outcome.unwrap(), finalized(2, 0), "{codec:?}");
            assert!(bytes == write(true), "{codec:?}");
        }
    }

    #[test]
    fn sample_bytes_are_never_rewritten() {
        let killed = session(5, false);
        let (outcome, bytes) = recover(killed.clone());
        assert_eq!(outcome.unwrap(), finalized(5, 0));
        // Only the 16-byte placeholder changes; everything else is appended.
        assert_eq!(bytes[..28], killed[..28]);
        assert_eq!(bytes[44..killed.len()], killed[44..]);
    }
}
