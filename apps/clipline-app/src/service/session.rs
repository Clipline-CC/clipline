//! Full-session recording lifecycle and clip saving.
use super::*;

pub(super) fn recover_abandoned_recordings(clips_dir: &Path, events: &Sender<Event>) {
    static RECOVERED_THIS_PROCESS: AtomicBool = AtomicBool::new(false);
    if !RECOVERED_THIS_PROCESS.swap(true, Ordering::AcqRel) {
        match recover_recording_files(clips_dir) {
            Ok(report) => {
                if !report.recovered.is_empty() {
                    warn_user(
                        events,
                        format!(
                            "recovered {} unfinished full-session recording(s)",
                            report.recovered.len()
                        ),
                    );
                }
                if report.deleted_empty > 0 {
                    warn_user(
                        events,
                        format!(
                            "cleaned up {} empty unfinished full-session recording(s)",
                            report.deleted_empty
                        ),
                    );
                }
            }
            Err(e) => warn_user(events, format!("recover unfinished recordings: {e}")),
        }
    }
    if let Err(error) = sweep_emptied_session_dirs(clips_dir) {
        warn_user(events, format!("clean empty session folders: {error}"));
    }
}

pub(super) struct RecorderFinishContext<'a> {
    pub(super) marker_log: &'a MarkerLog,
    pub(super) player_summary: Option<&'a PlayerSummary>,
    pub(super) audio_tracks: &'a [ClipAudioTrack],
    pub(super) clips_dir: &'a Path,
    pub(super) opts: &'a ServiceOptions,
    pub(super) events: &'a Sender<Event>,
}

pub(super) fn shutdown_recorder(
    rec: &mut LiveRecorder,
    full_session: &mut Option<FullSessionRecording>,
    ctx: RecorderFinishContext<'_>,
) -> Option<String> {
    match rec.finish_stream() {
        Ok(()) => {
            let _ = finish_full_session_recording(rec, full_session, &ctx);
            None
        }
        Err(e) => {
            let message = format!("finish: {e}");
            warn_user(ctx.events, message.clone());
            preserve_full_session_recording(
                rec,
                full_session,
                ctx.events,
                ctx.clips_dir,
                "full session could not finish cleanly and was kept for recovery",
            );
            Some(message)
        }
    }
}

pub(super) fn finalize_runtime_failure(primary: String, finalize: impl FnOnce() -> Option<String>) -> String {
    match finalize() {
        Some(finish) => format!("{primary}; additionally, {finish}"),
        None => primary,
    }
}

/// Sidecar that records which game a session folder belongs to, so the
/// library can show its icon. Written once per folder; custom-game clips have
/// no markers, so this is their only game link.
#[cfg(test)]
pub(super) const SESSION_META_FILE: &str = "clipline-session.json";

pub(super) fn write_session_game_meta(
    session_dir: &Path,
    active_game: Option<&ActiveGame>,
    league_queue: Option<&LeagueQueue>,
) {
    let Some(game) = active_game else { return };
    let mut doc = serde_json::json!({ "id": game.identity.id(), "name": game.name });
    if let Some(queue) = league_queue {
        doc["queue"] = serde_json::json!(queue);
    }
    match serde_json::to_string(&doc) {
        Ok(json) => {
            if let Err(e) = write_session_metadata(
                session_dir,
                json.as_bytes(),
                league_queue.is_some(),
            ) {
                tracing::warn!(event = "session_game_metadata_write_failed", error = %e);
            }
        }
        Err(e) => tracing::warn!(event = "session_game_metadata_serialize_failed", error = %e),
    }
}

pub(super) fn begin_full_session_recording(
    rec: &mut LiveRecorder,
    clips_dir: &Path,
    session_label: &str,
    mode: RecordingMode,
    active_game: Option<&ActiveGame>,
    events: &Sender<Event>,
) -> Option<FullSessionRecording> {
    if mode != RecordingMode::FullSession {
        return None;
    }

    let session_dir = clips_dir.join(session_label);
    let stamp = unix_now_u64();
    let (final_path, temp_path, file) =
        match reserve_full_session_path_at(&session_dir, "session", stamp) {
            Ok(reservation) => reservation,
            Err(e) => {
                warn_user(
                    events,
                    format!(
                        "full-session recording unavailable; reserve path in {session_dir:?}: {e}"
                    ),
                );
                return None;
            }
        };
    // Reservation creates the recording file under the session lock, then its
    // ownership marker before session metadata.
    write_session_game_meta(&session_dir, active_game, None);
    if let Err(e) = rec.start_full_session(file) {
        handle_full_session_finish_error(
            &temp_path,
            events,
            &format!("full-session recording unavailable; start writer: {e}"),
        );
        cleanup_discarded_session(&temp_path, clips_dir);
        return None;
    }
    Some(FullSessionRecording {
        final_path,
        temp_path,
        wall_start_unix: unix_now_i64(),
    })
}

pub(super) fn finish_full_session_recording(
    rec: &mut LiveRecorder,
    recording: &mut Option<FullSessionRecording>,
    ctx: &RecorderFinishContext<'_>,
) -> Option<StorageStatus> {
    let recording = recording.take()?;
    match rec.finish_full_session() {
        Ok(Some(summary)) if summary.duration_s.is_finite() && summary.duration_s <= 0.0 => {
            handle_full_session_finish_error(
                &recording.temp_path,
                ctx.events,
                "full session ended before any footage was written",
            );
            cleanup_discarded_session(&recording.temp_path, ctx.clips_dir);
            None
        }
        Ok(Some(summary)) => {
            let seconds = if summary.duration_s.is_finite() {
                summary.duration_s
            } else {
                warn_user(
                    ctx.events,
                    "full session duration was invalid; keeping the recording with an unknown duration"
                        .into(),
                );
                0.0
            };
            if !rename_finalized_session(&recording, ctx.events) {
                cleanup_discarded_session(&recording.temp_path, ctx.clips_dir);
                return None;
            }
            let markers = write_marker_sidecar(
                ctx.events,
                ctx.marker_log,
                &recording.final_path,
                summary.start_s,
                summary.end_s,
                ctx.player_summary,
                ctx.audio_tracks,
            );
            Some(emit_saved_clip(
                ctx.events,
                ctx.clips_dir,
                &recording.final_path,
                seconds,
                SavedClipMeta {
                    markers,
                    full_session: true,
                    recording_start_unix: Some(recording.wall_start_unix),
                    recording_end_unix: Some(unix_now_i64()),
                },
                ctx.opts,
            ))
        }
        Ok(None) => {
            handle_full_session_finish_error(
                &recording.temp_path,
                ctx.events,
                "full session ended before any footage was written",
            );
            cleanup_discarded_session(&recording.temp_path, ctx.clips_dir);
            None
        }
        Err(error) => {
            handle_full_session_finish_error(&recording.temp_path, ctx.events, &error.to_string());
            cleanup_discarded_session(&recording.temp_path, ctx.clips_dir);
            None
        }
    }
}

pub(super) fn handle_full_session_finish_error(temp_path: &Path, events: &Sender<Event>, error: &str) {
    match std::fs::metadata(temp_path) {
        Ok(metadata) if metadata.is_file() && metadata.len() == 0 => {
            remove_discarded_clip(temp_path);
            warn_user(events, format!("finish full session: {error}"));
        }
        Ok(_) => warn_user(
            events,
            format!("finish full session: {error}; recoverable recording kept at {temp_path:?}"),
        ),
        Err(metadata_error) if metadata_error.kind() == std::io::ErrorKind::NotFound => {
            let _ = remove_clip_ownership_marker(temp_path);
            warn_user(events, format!("finish full session: {error}"));
        }
        Err(metadata_error) => warn_user(
            events,
            format!(
                "finish full session: {error}; could not inspect {temp_path:?} ({metadata_error}), so it was kept for recovery"
            ),
        ),
    }
}

pub(super) fn rename_finalized_session(recording: &FullSessionRecording, events: &Sender<Event>) -> bool {
    match std::fs::rename(&recording.temp_path, &recording.final_path) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && recording.final_path.is_file() => {
            true
        }
        Err(error) => {
            let recovery = if recording.temp_path.is_file() {
                format!("; recoverable recording kept at {:?}", recording.temp_path)
            } else {
                String::new()
            };
            warn_user(
                events,
                format!(
                    "finalize full session {:?} -> {:?}: {error}{recovery}",
                    recording.temp_path, recording.final_path,
                ),
            );
            false
        }
    }
}

pub(super) fn preserve_full_session_recording(
    rec: &mut LiveRecorder,
    recording: &mut Option<FullSessionRecording>,
    events: &Sender<Event>,
    clips_dir: &Path,
    reason: &str,
) {
    let Some(recording) = recording.take() else {
        return;
    };
    if let Err(e) = rec.finish_full_session() {
        warn_user(events, format!("stop full-session writer: {e}"));
    }
    handle_full_session_finish_error(&recording.temp_path, events, reason);
    cleanup_discarded_session(&recording.temp_path, clips_dir);
}

pub(super) fn remove_discarded_clip(path: &Path) {
    let _ = std::fs::remove_file(path);
    let _ = remove_clip_ownership_marker(path);
}

pub(super) fn cleanup_discarded_session(path: &Path, media_root: &Path) {
    if let Some(session_dir) = path.parent() {
        let _ = remove_emptied_session_dir_after_clip(session_dir, media_root);
    }
}

pub(super) struct FullSessionRecording {
    pub(super) final_path: PathBuf,
    pub(super) temp_path: PathBuf,
    pub(super) wall_start_unix: i64,
}

pub(super) struct FullSessionQuotaCheck {
    pub(super) event: Option<Event>,
    /// Present when auto-delete ran and the cached library baseline should move.
    pub(super) new_baseline_bytes: Option<u64>,
}

pub(super) fn full_session_quota_check(
    events: &Sender<Event>,
    clips_dir: &Path,
    recording: &FullSessionRecording,
    saved_media_baseline_bytes: Option<u64>,
    quota_bytes: Option<u64>,
    required_bytes: u64,
    auto_delete: bool,
) -> FullSessionQuotaCheck {
    let Some(baseline) = saved_media_baseline_bytes else {
        return FullSessionQuotaCheck {
            event: None,
            new_baseline_bytes: None,
        };
    };
    let active_bytes = match std::fs::metadata(&recording.temp_path) {
        Ok(metadata) => metadata.len(),
        Err(error) => {
            tracing::warn!(
                event = "full_session_quota_inspection_failed",
                path = ?recording.temp_path,
                error = %error,
            );
            return FullSessionQuotaCheck {
                event: None,
                new_baseline_bytes: None,
            };
        }
    };
    let total_bytes = baseline.saturating_add(active_bytes);
    if !quota_would_be_exceeded(total_bytes, quota_bytes, required_bytes) {
        return FullSessionQuotaCheck {
            event: None,
            new_baseline_bytes: None,
        };
    }
    if auto_delete {
        // Inventory after cleanup includes the active recording, which
        // enforce_quota never deletes.
        if let Some((cleaned, _)) =
            make_room_for_quota(events, clips_dir, quota_bytes, required_bytes, None)
        {
            let new_baseline_bytes = Some(cleaned.total_bytes.saturating_sub(active_bytes));
            if !quota_would_be_exceeded(cleaned.total_bytes, quota_bytes, required_bytes) {
                return FullSessionQuotaCheck {
                    event: None,
                    new_baseline_bytes,
                };
            }
            return FullSessionQuotaCheck {
                event: storage_quota_event_for_usage(
                    cleaned.total_bytes,
                    quota_bytes,
                    required_bytes,
                ),
                new_baseline_bytes,
            };
        }
    }
    FullSessionQuotaCheck {
        event: storage_quota_event_for_usage(total_bytes, quota_bytes, required_bytes),
        new_baseline_bytes: None,
    }
}

pub(super) fn unique_media_path(session_dir: &Path, prefix: &str) -> PathBuf {
    unique_media_path_at(session_dir, prefix, unix_now_u64())
}

pub(super) fn unique_media_path_at(session_dir: &Path, prefix: &str, stamp: u64) -> PathBuf {
    for attempt in 0u32..1024 {
        let name = if attempt == 0 {
            format!("{prefix}_{stamp}.mp4")
        } else {
            format!("{prefix}_{stamp}_{attempt}.mp4")
        };
        let candidate = session_dir.join(name);
        let marker_exists =
            clip_ownership_marker_path(&candidate).is_ok_and(|marker| marker.exists());
        if !candidate.exists() && !marker_exists {
            return candidate;
        }
    }
    let fallback = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    session_dir.join(format!("{prefix}_{fallback}.mp4"))
}

pub(super) fn reserve_full_session_path_at(
    session_dir: &Path,
    prefix: &str,
    stamp: u64,
) -> std::io::Result<(PathBuf, PathBuf, std::fs::File)> {
    reserve_full_session_path_at_with(session_dir, prefix, stamp, |_, temp_path| {
        reserve_session_recording_file(temp_path)
    })
}

pub(super) fn reserve_full_session_path_at_with<F>(
    session_dir: &Path,
    prefix: &str,
    stamp: u64,
    mut reserve_temp: F,
) -> std::io::Result<(PathBuf, PathBuf, std::fs::File)>
where
    F: FnMut(&Path, &Path) -> std::io::Result<std::fs::File>,
{
    for attempt in 0u32..1024 {
        let name = if attempt == 0 {
            format!("{prefix}_{stamp}.mp4")
        } else {
            format!("{prefix}_{stamp}_{attempt}.mp4")
        };
        let final_path = session_dir.join(name);
        if final_path.try_exists()? || clip_ownership_marker_path(&final_path)?.try_exists()? {
            continue;
        }
        let temp_path = final_path.with_extension("mp4.recording");
        match reserve_temp(&final_path, &temp_path) {
            Ok(file) => match final_path.try_exists() {
                Ok(false) => match ensure_clip_owned(&temp_path) {
                    Ok(true) => match final_path.try_exists() {
                        Ok(false) => return Ok((final_path, temp_path, file)),
                        Ok(true) => {
                            drop(file);
                            remove_discarded_clip(&temp_path);
                            continue;
                        }
                        Err(check_error) => {
                            drop(file);
                            remove_discarded_clip(&temp_path);
                            return Err(check_error);
                        }
                    },
                    Ok(false) => {
                        drop(file);
                        std::fs::remove_file(&temp_path)?;
                        continue;
                    }
                    Err(marker_error) => {
                        drop(file);
                        let _ = std::fs::remove_file(&temp_path);
                        return Err(marker_error);
                    }
                },
                Ok(true) => {
                    drop(file);
                    std::fs::remove_file(&temp_path)?;
                    continue;
                }
                Err(check_error) => {
                    drop(file);
                    if let Err(cleanup_error) = std::fs::remove_file(&temp_path) {
                        return Err(std::io::Error::new(
                            check_error.kind(),
                            format!(
                                "inspect reserved final path {final_path:?}: {check_error}; \
                                 remove reservation {temp_path:?}: {cleanup_error}"
                            ),
                        ));
                    }
                    return Err(check_error);
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!("no free {prefix}_{stamp} full-session path after 1024 attempts"),
    ))
}

pub(super) fn write_marker_sidecar(
    events: &Sender<Event>,
    marker_log: &MarkerLog,
    path: &Path,
    start_s: f64,
    end_s: f64,
    player_summary: Option<&PlayerSummary>,
    audio_tracks: &[ClipAudioTrack],
) -> usize {
    let mut clip = marker_log.clip_markers(start_s, end_s);
    clip.markers.retain(|m| is_review_event(&m.event));
    clip.player_summary = player_summary.cloned();
    clip.audio_tracks = audio_tracks.to_vec();
    // User-placed bookmarks are markers to the review timeline, so they count
    // toward the clip's marker total and keep the sidecar worth writing.
    let markers = clip.markers.len() + clip.bookmarks.len();
    if markers == 0
        && clip.player_summary.is_none()
        && clip.audio_tracks.is_empty()
        && clip.plays.is_empty()
    {
        return 0;
    }
    match serde_json::to_string_pretty(&clip) {
        Ok(json) => {
            if let Err(e) = std::fs::write(path.with_extension("markers.json"), json) {
                warn_user(events, format!("write marker sidecar for {path:?}: {e}"));
            }
        }
        Err(e) => warn_user(
            events,
            format!("serialize marker sidecar for {path:?}: {e}"),
        ),
    }
    markers
}

pub(super) struct SavedClipMeta {
    pub(super) markers: usize,
    pub(super) full_session: bool,
    pub(super) recording_start_unix: Option<i64>,
    pub(super) recording_end_unix: Option<i64>,
}

pub(super) fn emit_saved_clip(
    events: &Sender<Event>,
    clips_dir: &Path,
    path: &Path,
    seconds: f64,
    meta: SavedClipMeta,
    opts: &ServiceOptions,
) -> StorageStatus {
    let fallback_status = || StorageStatus {
        clip_count: 0,
        total_bytes: 0,
        quota_bytes: opts.disk_quota_bytes,
    };
    let status = if opts.auto_delete_when_over_quota {
        match crate::gc::enforce_quota_with_clip_policy(
            clips_dir,
            opts.disk_quota_bytes,
            Some(path),
        ) {
            Ok(report) => report.status,
            Err(e) => {
                warn_user(events, format!("storage cleanup: {e}"));
                match storage_status(clips_dir, opts.disk_quota_bytes) {
                    Ok(status) => status,
                    Err(status_error) => {
                        warn_user(events, format!("storage status: {status_error}"));
                        fallback_status()
                    }
                }
            }
        }
    } else {
        match storage_status(clips_dir, opts.disk_quota_bytes) {
            Ok(status) => status,
            Err(e) => {
                warn_user(events, format!("storage status: {e}"));
                fallback_status()
            }
        }
    };

    let _ = events.send(Event::Saved {
        path: path.display().to_string(),
        seconds,
        recording_start_unix: meta.recording_start_unix,
        recording_end_unix: meta.recording_end_unix,
        markers: meta.markers,
        full_session: meta.full_session,
        storage_total_bytes: status.total_bytes,
        storage_quota_bytes: status.quota_bytes,
        storage_over_quota: status.is_over_quota(),
    });
    status
}

pub(super) fn save(
    rec: &Recorder<impl CaptureEngine, impl Encoder>,
    path: &Path,
    window_s: f64,
    active_game: Option<&ActiveGame>,
    league_queue: Option<&LeagueQueue>,
) -> Result<(f64, f64), String> {
    let marker_created = ensure_session_clip_owned(path)
        .map_err(|e| format!("mark Clipline-owned clip {path:?}: {e}"))?;
    if let Some(session_dir) = path.parent() {
        write_session_game_meta(session_dir, active_game, league_queue);
    }
    let saved_from = rec
        .save_window_bounds(window_s, None)
        .map(|(start, _)| start);
    let result = (|| {
        let file = std::fs::File::create(path).map_err(|e| format!("create {path:?}: {e}"))?;
        let (_, end) = rec
            .save_replay(file, window_s, None)
            .map_err(|e| format!("save: {e}"))?;
        Ok((end, end - saved_from.unwrap_or(end)))
    })();
    if result.is_err() && marker_created {
        let _ = remove_clip_ownership_marker(path);
    }
    result
}

pub(super) fn crop_for_region(
    region: &CaptureRegion,
    display: &clipline_capture::windows::display::DisplayInfo,
) -> Result<CropRect, String> {
    if region.width < 2 || region.height < 2 {
        return Err("capture region must be at least 2x2 pixels".into());
    }
    let local_x = region.x - display.x;
    let local_y = region.y - display.y;
    if local_x < 0
        || local_y < 0
        || local_x as i64 + region.width as i64 > display.width as i64
        || local_y as i64 + region.height as i64 > display.height as i64
    {
        return Err(format!(
            "capture region must fit inside {} ({}x{} at {}, {})",
            display.name, display.width, display.height, display.x, display.y
        ));
    }
    Ok(CropRect {
        x: local_x as u32,
        y: local_y as u32,
        width: region.width,
        height: region.height,
    })
}

pub(super) fn crop_for_region_or_full_display(
    region: &CaptureRegion,
    display: &clipline_capture::windows::display::DisplayInfo,
    recovered_display: bool,
) -> Result<(CropRect, bool), String> {
    if region.width < 2 || region.height < 2 {
        return Err("capture region must be at least 2x2 pixels".into());
    }
    if !recovered_display {
        if let Some(crop) = rebased_full_display_crop(region, display) {
            return Ok((crop, false));
        }
        if let Ok(crop) = crop_for_region(region, display) {
            return Ok((crop, false));
        }
        if let Some(crop) = clamped_region_crop(region, display)? {
            return Ok((crop, true));
        }
    }
    Ok((
        CropRect {
            x: 0,
            y: 0,
            width: display.width,
            height: display.height,
        },
        true,
    ))
}

pub(super) fn rebased_full_display_crop(
    region: &CaptureRegion,
    display: &clipline_capture::windows::display::DisplayInfo,
) -> Option<CropRect> {
    if region.width == display.width && region.height == display.height {
        Some(CropRect {
            x: 0,
            y: 0,
            width: display.width,
            height: display.height,
        })
    } else {
        None
    }
}

pub(super) fn clamped_region_crop(
    region: &CaptureRegion,
    display: &clipline_capture::windows::display::DisplayInfo,
) -> Result<Option<CropRect>, String> {
    if region.width < 2 || region.height < 2 {
        return Err("capture region must be at least 2x2 pixels".into());
    }
    let region_left = region.x as i64;
    let region_top = region.y as i64;
    let region_right = region_left + region.width as i64;
    let region_bottom = region_top + region.height as i64;
    let display_left = display.x as i64;
    let display_top = display.y as i64;
    let display_right = display_left + display.width as i64;
    let display_bottom = display_top + display.height as i64;

    let left = region_left.max(display_left);
    let top = region_top.max(display_top);
    let right = region_right.min(display_right);
    let bottom = region_bottom.min(display_bottom);
    let width = right - left;
    let height = bottom - top;
    if width < 2 || height < 2 {
        return Ok(None);
    }
    Ok(Some(CropRect {
        x: (left - display_left) as u32,
        y: (top - display_top) as u32,
        width: width as u32,
        height: height as u32,
    }))
}

pub(super) fn capture_display_recovery_warning(
    region: &CaptureRegion,
    display: &clipline_capture::windows::display::DisplayInfo,
    recovered_display: bool,
    recovered_crop: bool,
) -> Option<String> {
    if !recovered_display && !recovered_crop {
        return None;
    }
    let configured = region
        .display_id
        .as_deref()
        .unwrap_or("the configured display");
    let fallback = if recovered_display {
        format!("using full display {}", display.name)
    } else {
        format!("using the visible part of the region on {}", display.name)
    };
    Some(format!(
        "capture target {configured} is no longer available or no longer fits; {fallback}. Open Settings and save your capture source to update it."
    ))
}

pub(super) fn warn_capture_display_recovery(
    events: &Sender<Event>,
    region: &CaptureRegion,
    display: &clipline_capture::windows::display::DisplayInfo,
    recovered_display: bool,
    recovered_crop: bool,
) {
    if let Some(message) =
        capture_display_recovery_warning(region, display, recovered_display, recovered_crop)
    {
        tracing::warn!(event = "capture_display_recovered", message = %message);
        warn_user(events, message);
    }
}

/// Session label from the local wall clock (folder names should match what
/// the user's file explorer shows, not UTC).
pub(super) fn local_session_label(league_match: bool) -> String {
    use chrono::{Datelike, Local, Timelike};
    let now = Local::now();
    session_label(
        now.year(),
        now.month(),
        now.day(),
        now.hour(),
        now.minute(),
        league_match,
    )
}
