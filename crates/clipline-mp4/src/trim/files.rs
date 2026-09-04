use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{HybridMp4Writer, SourceSample, TrackConfig};
use super::audio_mix::mix_selected_opus_audio_tracks_to_spool;
use super::model::{SampleRecord, TrimError, TrimInfo, select_trim_range, validate_range};
use super::parse::parse_movie_reader;
use super::{
    selected_audio_index_set, selected_audio_tracks, write_timed_source_samples,
    write_timed_source_samples_from_sources,
};

const TEMP_FILE_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn trim_keyframe_aligned_file(
    source: &Path,
    target: &Path,
    start_s: f64,
    end_s: f64,
) -> Result<TrimInfo, TrimError> {
    validate_range(start_s, end_s)?;
    reject_same_file(source, target)?;
    let mut source_file = File::open(source)?;
    let movie = parse_movie_reader(&mut source_file)?;
    let selection = select_trim_range(&movie, start_s, end_s)?;
    let mut per_track: Vec<Vec<SourceSample>> = Vec::with_capacity(movie.tracks.len());
    let mut starts: Vec<Vec<u64>> = Vec::with_capacity(movie.tracks.len());
    for (idx, track) in movie.tracks.iter().enumerate() {
        let records: Vec<&SampleRecord> = if idx == selection.video_idx {
            track.samples[selection.start_idx..selection.end_idx]
                .iter()
                .collect()
        } else {
            track
                .samples
                .iter()
                .filter(|sample| selection.contains_start(sample.start_ticks, track.timescale))
                .collect()
        };
        starts.push(
            records
                .iter()
                .map(|sample| selection.rebase_start(sample.start_ticks, track.timescale))
                .collect::<Result<_, _>>()?,
        );
        per_track.push(
            records
                .into_iter()
                .map(SampleRecord::to_source_sample)
                .collect(),
        );
    }

    let tracks: Vec<TrackConfig> = movie.tracks.iter().map(|t| t.cfg.clone()).collect();
    write_file_atomically(target, |target_file| {
        let mut writer = HybridMp4Writer::new_multi(target_file, tracks)?;
        write_timed_source_samples(&mut writer, &mut source_file, &per_track, &starts)?;
        Ok(writer.finalize()?)
    })?;
    Ok(selection.info(start_s, end_s))
}

pub fn remux_with_selected_audio_tracks_file(
    source: &Path,
    target: &Path,
    selected_audio_track_indices: &[u32],
) -> Result<(), TrimError> {
    reject_same_file(source, target)?;
    let mut source_file = File::open(source)?;
    let movie = parse_movie_reader(&mut source_file)?;
    let selected = selected_audio_index_set(&movie, selected_audio_track_indices)?;

    let mut tracks = Vec::new();
    let mut per_track = Vec::new();
    let mut starts = Vec::new();
    let mut audio_index = 0_usize;
    for track in &movie.tracks {
        let keep = match track.cfg {
            TrackConfig::Video(_) => true,
            TrackConfig::Audio(_) => {
                let keep = selected.contains(&audio_index);
                audio_index += 1;
                keep
            }
        };
        if keep {
            tracks.push(track.cfg.clone());
            starts.push(
                track
                    .samples
                    .iter()
                    .map(|sample| sample.start_ticks)
                    .collect(),
            );
            per_track.push(
                track
                    .samples
                    .iter()
                    .map(SampleRecord::to_source_sample)
                    .collect::<Vec<_>>(),
            );
        }
    }

    write_file_atomically(target, |target_file| {
        let mut writer = HybridMp4Writer::new_multi(target_file, tracks)?;
        write_timed_source_samples(&mut writer, &mut source_file, &per_track, &starts)?;
        Ok(writer.finalize()?)
    })
}

pub fn remux_with_mixed_audio_track_file(
    source: &Path,
    target: &Path,
    selected_audio_track_indices: &[u32],
) -> Result<(), TrimError> {
    reject_same_file(source, target)?;
    let mut source_file = File::open(source)?;
    let movie = parse_movie_reader(&mut source_file)?;
    let selected = selected_audio_index_set(&movie, selected_audio_track_indices)?;
    if selected.is_empty() {
        return remux_with_selected_audio_tracks_file(source, target, selected_audio_track_indices);
    }

    let selected_audio = selected_audio_tracks(&movie, &selected);
    let mut spool = OwnedTempFile::create_near(target, "mix")?;
    let mixed = mix_selected_opus_audio_tracks_to_spool(
        &mut source_file,
        &selected_audio,
        spool.file_mut(),
    )?;
    spool.file_mut().flush()?;

    let mut tracks = Vec::new();
    let mut per_track = Vec::new();
    let mut starts = Vec::new();
    for track in &movie.tracks {
        if matches!(track.cfg, TrackConfig::Video(_)) {
            tracks.push(track.cfg.clone());
            starts.push(
                track
                    .samples
                    .iter()
                    .map(|sample| sample.start_ticks)
                    .collect(),
            );
            per_track.push(
                track
                    .samples
                    .iter()
                    .map(SampleRecord::to_source_sample)
                    .collect::<Vec<_>>(),
            );
        }
    }
    if !mixed.samples.is_empty() {
        tracks.push(TrackConfig::Audio(mixed.cfg));
        per_track.push(mixed.samples);
        starts.push(mixed.start_ticks);
    }

    let video_sources = tracks
        .iter()
        .filter(|track| matches!(track, TrackConfig::Video(_)))
        .count();
    let mut sources = (0..video_sources)
        .map(|_| File::open(source))
        .collect::<Result<Vec<_>, _>>()?;
    if tracks
        .last()
        .is_some_and(|track| matches!(track, TrackConfig::Audio(_)))
    {
        sources.push(File::open(spool.path())?);
    }

    write_file_atomically(target, |target_file| {
        let mut writer = HybridMp4Writer::new_multi(target_file, tracks)?;
        let mut source_refs: Vec<&mut dyn crate::writer::ReadSeek> = sources
            .iter_mut()
            .map(|file| file as &mut dyn crate::writer::ReadSeek)
            .collect();
        write_timed_source_samples_from_sources(
            &mut writer,
            &mut source_refs,
            &per_track,
            &starts,
        )?;
        Ok(writer.finalize()?)
    })
}

fn reject_same_file(source: &Path, target: &Path) -> Result<(), TrimError> {
    let source_canonical = std::fs::canonicalize(source)?;
    let same_identity = target.exists() && files_have_same_identity(source, target)?;
    let same_path = std::fs::canonicalize(target)
        .is_ok_and(|target_canonical| source_canonical == target_canonical);
    if same_identity || same_path {
        return Err(TrimError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "MP4 source and target must be different files",
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn files_have_same_identity(source: &Path, target: &Path) -> std::io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let source = std::fs::metadata(source)?;
    let target = std::fs::metadata(target)?;
    Ok(source.dev() == target.dev() && source.ino() == target.ino())
}

#[cfg(windows)]
fn files_have_same_identity(source: &Path, target: &Path) -> std::io::Result<bool> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };

    fn identity(path: &Path) -> std::io::Result<(u32, u64)> {
        let file = File::open(path)?;
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        let result =
            unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut info) };
        if result == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
        Ok((info.dwVolumeSerialNumber, index))
    }

    Ok(identity(source)? == identity(target)?)
}

#[cfg(not(any(unix, windows)))]
fn files_have_same_identity(source: &Path, target: &Path) -> std::io::Result<bool> {
    Ok(std::fs::canonicalize(source)? == std::fs::canonicalize(target)?)
}

struct OwnedTempFile {
    path: PathBuf,
    file: Option<File>,
}

impl OwnedTempFile {
    fn create_near(target: &Path, purpose: &str) -> Result<Self, TrimError> {
        let file_name = target.file_name().ok_or_else(|| {
            TrimError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "MP4 target must include a file name",
            ))
        })?;
        if let Some(parent) = target.parent() {
            prune_abandoned_transform_temps(parent);
        }
        for _ in 0..128 {
            let suffix = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
            let mut temp_name = file_name.to_os_string();
            temp_name.push(format!(
                ".clipline-tmp-{purpose}-{}-{suffix}.tmp",
                std::process::id()
            ));
            let path = target.with_file_name(temp_name);
            match OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => {
                    return Ok(Self {
                        path,
                        file: Some(file),
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(TrimError::Io(error)),
            }
        }
        Err(TrimError::Io(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not allocate a unique MP4 temporary file",
        )))
    }

    fn file_mut(&mut self) -> &mut File {
        self.file.as_mut().expect("owned temp file is open")
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn take_file(&mut self) -> File {
        self.file.take().expect("owned temp file is open")
    }

    fn disarm(mut self) {
        self.file.take();
        self.path.clear();
    }
}

fn prune_abandoned_transform_temps(directory: &Path) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        let is_transform_temp = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.contains(".clipline-tmp-") && name.ends_with(".tmp"));
        let abandoned = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= TEMP_FILE_MAX_AGE);
        if is_transform_temp && abandoned {
            let _ = std::fs::remove_file(path);
        }
    }
}

impl Drop for OwnedTempFile {
    fn drop(&mut self) {
        self.file.take();
        if !self.path.as_os_str().is_empty() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn write_file_atomically(
    target: &Path,
    write: impl FnOnce(File) -> Result<File, TrimError>,
) -> Result<(), TrimError> {
    let mut temp = OwnedTempFile::create_near(target, "output")?;
    let file = temp.take_file();
    let file = write(file)?;
    file.sync_all()?;
    drop(file);
    replace_file(temp.path(), target)?;
    temp.disarm();
    Ok(())
}

#[cfg(windows)]
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let from_w: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to_w: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    let result = unsafe {
        MoveFileExW(
            from_w.as_ptr(),
            to_w.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::rename(from, to)
}

#[cfg(test)]
mod tests {
    use super::super::model::fixtures::*;
    use super::super::parse::parse_movie;
    use super::super::{remux_with_selected_audio_tracks, trim_keyframe_aligned};
    use super::*;

        #[test]
        fn file_trim_matches_in_memory_trim_output() {
            let input = clipline_fixture();
            let (expected, expected_info) = trim_keyframe_aligned(&input, 0.4, 1.2).unwrap();
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir = std::env::temp_dir().join(format!(
                "clipline-trim-file-{}-{unique}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let source = dir.join("source.mp4");
            let target = dir.join("target.mp4");
            std::fs::write(&source, &input).unwrap();
    
            let info = trim_keyframe_aligned_file(&source, &target, 0.4, 1.2).unwrap();
            let actual = std::fs::read(&target).unwrap();
            let _ = std::fs::remove_dir_all(&dir);
    
            assert_eq!(info, expected_info);
            assert_eq!(actual, expected);
        }

        #[test]
        fn file_trim_rejects_same_source_and_target_without_truncating() {
            let input = clipline_fixture();
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir = std::env::temp_dir().join(format!(
                "clipline-trim-same-file-{}-{unique}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let source = dir.join("source.mp4");
            std::fs::write(&source, &input).unwrap();
    
            let err = trim_keyframe_aligned_file(&source, &source, 0.4, 1.2).unwrap_err();
            let after = std::fs::read(&source).unwrap();
            let _ = std::fs::remove_dir_all(&dir);
    
            assert!(matches!(
                err,
                TrimError::Io(ref e) if e.kind() == std::io::ErrorKind::InvalidInput
            ));
            assert_eq!(after, input);
        }

        #[test]
        fn file_trim_rejects_hard_link_target_without_truncating_source() {
            let input = clipline_fixture();
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir = std::env::temp_dir().join(format!(
                "clipline-trim-hard-link-{}-{unique}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let source = dir.join("source.mp4");
            let target = dir.join("target.mp4");
            std::fs::write(&source, &input).unwrap();
            std::fs::hard_link(&source, &target).unwrap();
    
            let err = trim_keyframe_aligned_file(&source, &target, 0.4, 1.2).unwrap_err();
            let source_after = std::fs::read(&source).unwrap();
            let target_after = std::fs::read(&target).unwrap();
            let _ = std::fs::remove_dir_all(&dir);
    
            assert!(matches!(
                err,
                TrimError::Io(ref e) if e.kind() == std::io::ErrorKind::InvalidInput
            ));
            assert_eq!(source_after, input);
            assert_eq!(target_after, input);
        }

        #[test]
        fn file_remux_matches_in_memory_output_and_preserves_existing_target_on_error() {
            let input = clipline_two_audio_fixture();
            let expected = remux_with_selected_audio_tracks(&input, &[1]).unwrap();
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir = std::env::temp_dir().join(format!(
                "clipline-remux-file-{}-{unique}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let source = dir.join("source.mp4");
            let target = dir.join("target.mp4");
            std::fs::write(&source, &input).unwrap();
    
            remux_with_selected_audio_tracks_file(&source, &target, &[1]).unwrap();
            assert_eq!(std::fs::read(&target).unwrap(), expected);
    
            let sentinel = b"existing target must survive";
            std::fs::write(&target, sentinel).unwrap();
            let err = remux_with_selected_audio_tracks_file(&source, &target, &[2]).unwrap_err();
            assert!(err
                .to_string()
                .contains("outside the clip's 2 audio tracks"));
            assert_eq!(std::fs::read(&target).unwrap(), sentinel);
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn atomic_file_transform_preserves_target_and_cleans_partial_on_late_failure() {
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir = std::env::temp_dir().join(format!(
                "clipline-atomic-transform-{}-{unique}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let target = dir.join("target.mp4");
            std::fs::write(&target, b"previous complete clip").unwrap();
    
            let error = write_file_atomically(&target, |mut temporary| {
                temporary.write_all(b"partial replacement")?;
                Err(TrimError::Io(std::io::Error::other(
                    "injected finalize failure",
                )))
            })
            .unwrap_err();
            let leftovers = std::fs::read_dir(&dir)
                .unwrap()
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect::<Vec<_>>();
    
            assert!(error.to_string().contains("injected finalize failure"));
            assert_eq!(std::fs::read(&target).unwrap(), b"previous complete clip");
            assert_eq!(leftovers, vec!["target.mp4"]);
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn abandoned_transform_temp_prune_is_scoped_and_age_gated() {
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir = std::env::temp_dir().join(format!(
                "clipline-transform-prune-{}-{unique}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let abandoned = dir.join("clip.mp4.clipline-tmp-output-1-1.tmp");
            let active = dir.join("clip.mp4.clipline-tmp-output-1-2.tmp");
            let unrelated = dir.join("editor.tmp");
            for path in [&abandoned, &active, &unrelated] {
                std::fs::write(path, b"temp").unwrap();
            }
            File::options()
                .write(true)
                .open(&abandoned)
                .unwrap()
                .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1))
                .unwrap();
    
            prune_abandoned_transform_temps(&dir);
    
            assert!(!abandoned.exists());
            assert!(active.exists());
            assert!(unrelated.exists());
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn file_audio_mix_streams_video_and_emits_audible_mixed_track() {
            let input = clipline_two_real_opus_audio_fixture();
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir =
                std::env::temp_dir().join(format!("clipline-mix-file-{}-{unique}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let source = dir.join("source.mp4");
            let target = dir.join("target.mp4");
            std::fs::write(&source, input).unwrap();
    
            remux_with_mixed_audio_track_file(&source, &target, &[0, 1]).unwrap();
            let out = std::fs::read(&target).unwrap();
            let movie = parse_movie(&out).unwrap();
            let leftovers = std::fs::read_dir(&dir)
                .unwrap()
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| name.contains("clipline-tmp"))
                .collect::<Vec<_>>();
            let _ = std::fs::remove_dir_all(&dir);
    
            assert_eq!(movie.tracks.len(), 2);
            assert!(decoded_audible_audio_rms(&out) > 0.10);
            assert!(leftovers.is_empty(), "leftover temp files: {leftovers:?}");
        }

        #[test]
        fn file_audio_mix_handles_staggered_track_starts() {
            let input = clipline_staggered_opus_audio_fixture();
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir = std::env::temp_dir().join(format!(
                "clipline-staggered-mix-file-{}-{unique}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let source = dir.join("source.mp4");
            let target = dir.join("target.mp4");
            std::fs::write(&source, input).unwrap();
    
            remux_with_mixed_audio_track_file(&source, &target, &[0, 1]).unwrap();
            let out = std::fs::read(&target).unwrap();
            let movie = parse_movie(&out).unwrap();
            let audio = movie
                .tracks
                .iter()
                .find(|track| matches!(track.cfg, TrackConfig::Audio(_)))
                .unwrap();
            let _ = std::fs::remove_dir_all(&dir);
    
            assert!(audio.samples.windows(2).all(|samples| {
                samples[1].start_ticks >= samples[0].start_ticks + u64::from(samples[0].duration)
            }));
            assert!(decoded_audible_audio_rms(&out) > 0.02);
        }
}
