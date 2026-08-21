//! Behavioral tests for the pure review-player logic in `ui/player-core.js`.
//!
//! The file is evaluated in Boa (pure-Rust JS interpreter), so these run on both
//! CI OSes with no Node/npm toolchain. `player-core.js` must stay DOM-free and
//! Tauri-free for this to work — that constraint is the point.

use boa_engine::{Context, Source};
use std::fs;
use std::path::Path;

fn player_core_context() -> Context {
    let mut context = Context::default();
    for name in ["presentation-core.js", "player-core.js"] {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("ui").join(name);
        let source =
            fs::read_to_string(path).unwrap_or_else(|error| panic!("read {name}: {error}"));
        context
            .eval(Source::from_bytes(&source))
            .unwrap_or_else(|error| {
                panic!("{name} evaluates without DOM or Tauri globals: {error}")
            });
    }
    context
}

#[test]
fn shared_presentation_normalizes_names_labels_and_dates() {
    let mut ctx = player_core_context();
    assert_eq!(
        eval_json(
            &mut ctx,
            "['clip.MP4','clip.mov','clip.MKV','clip.webm','clip.avi'].map(PresentationCore.clipNameStem)"
        ),
        "[\"clip\",\"clip\",\"clip\",\"clip\",\"clip.avi\"]"
    );
    assert_eq!(
        eval(
            &mut ctx,
            "PresentationCore.markerKindLabel('multiKill', '')"
        ),
        "multi Kill"
    );
    assert_eq!(
        eval(
            &mut ctx,
            "PresentationCore.markerKindLabel('multiKill', 'Spree')"
        ),
        "Spree"
    );
    assert_eq!(
        eval(&mut ctx, "PresentationCore.formatClipTitle(6, 18, 0, 5)"),
        "Jul 18 · 12:05 AM"
    );
}

/// Evaluate an expression and return its string conversion ("null" for null).
fn eval(context: &mut Context, expr: &str) -> String {
    let value = context
        .eval(Source::from_bytes(expr))
        .unwrap_or_else(|err| panic!("eval `{expr}`: {err}"));
    value
        .to_string(context)
        .unwrap_or_else(|err| panic!("stringify `{expr}`: {err}"))
        .to_std_string_escaped()
}

/// Evaluate an expression through JSON.stringify for structural comparison.
fn eval_json(context: &mut Context, expr: &str) -> String {
    eval(context, &format!("JSON.stringify({expr})"))
}

#[test]
fn fmt_dur_carries_seconds_into_minutes() {
    let mut ctx = player_core_context();
    // Splitting minutes before rounding renders 59.6 as "0:60".
    assert_eq!(eval(&mut ctx, "PlayerCore.fmtDur(59.6)"), "1:00");
    assert_eq!(eval(&mut ctx, "PlayerCore.fmtDur(0)"), "0:00");
    assert_eq!(eval(&mut ctx, "PlayerCore.fmtDur(90)"), "1:30");
    assert_eq!(eval(&mut ctx, "PlayerCore.fmtDur(605.4)"), "10:05");
    assert_eq!(eval(&mut ctx, "PlayerCore.fmtDur(NaN)"), "?");
    assert_eq!(eval(&mut ctx, "PlayerCore.fmtDur(null)"), "?");
}

#[test]
fn encoder_caveat_only_warns_for_undecodable_non_h264_codecs() {
    let mut ctx = player_core_context();
    // H.264 always plays — never a caveat, regardless of the decodable set.
    assert_eq!(
        eval(&mut ctx, "PlayerCore.encoderCodecCaveat('h264', [])"),
        "null"
    );
    // HEVC/AV1 warn when not in the decodable set...
    assert!(eval(&mut ctx, "PlayerCore.encoderCodecCaveat('hevc', ['h264'])").contains("HEVC"));
    assert!(eval(&mut ctx, "PlayerCore.encoderCodecCaveat('av1', ['h264'])").contains("AV1"));
    // ...and stay quiet once the player reports it can decode them.
    assert_eq!(
        eval(
            &mut ctx,
            "PlayerCore.encoderCodecCaveat('av1', ['h264','av1'])"
        ),
        "null"
    );
}

#[test]
fn video_decode_probes_cover_hevc_and_av1_mp4_profiles() {
    let mut ctx = player_core_context();
    let codecs = eval_json(&mut ctx, "PlayerCore.videoDecodeProbes().map(p => p.codec)");
    assert_eq!(codecs, "[\"hevc\",\"av1\"]");
    // Each probe is a concrete mp4 codec query for canPlayType.
    assert!(eval(&mut ctx, "PlayerCore.videoDecodeProbes()[0].mime").contains("hvc1"));
    assert!(eval(&mut ctx, "PlayerCore.videoDecodeProbes()[1].mime").contains("av01"));
}

#[test]
fn clip_paths_match_legacy_windows_canonical_forms() {
    let mut ctx = player_core_context();
    assert_eq!(
        eval(
            &mut ctx,
            r#"PlayerCore.sameClipPath('D:\\Videos\\Clipline\\clip.mp4', '\\\\?\\D:\\Videos\\Clipline\\clip.mp4')"#,
        ),
        "true"
    );
    assert_eq!(
        eval(
            &mut ctx,
            r#"PlayerCore.sameClipPath('d:/videos/clipline/CLIP.mp4', 'D:\\Videos\\Clipline\\clip.mp4')"#,
        ),
        "true"
    );
    assert_eq!(
        eval(
            &mut ctx,
            "PlayerCore.sameClipPath('/Clips/clip.mp4', '/clips/clip.mp4')",
        ),
        "false"
    );
}

#[test]
fn clip_file_label_prefers_name_and_never_leaks_folders() {
    let mut ctx = player_core_context();
    // The recorded file name wins when present.
    assert_eq!(
        eval(
            &mut ctx,
            r#"PlayerCore.clipFileLabel({ name: 'Ranked win.mp4', path: 'C:\\Users\\me\\Videos\\Clipline\\Ranked win.mp4' })"#,
        ),
        "Ranked win.mp4"
    );
    // Without a name, only the final path segment is shown — never the folders.
    assert_eq!(
        eval(
            &mut ctx,
            r#"PlayerCore.clipFileLabel({ path: 'C:\\Users\\me\\Videos\\Clipline\\clip.mp4' })"#,
        ),
        "clip.mp4"
    );
    // Long (\\?\) device paths and forward slashes degrade to the file name too.
    assert_eq!(
        eval(
            &mut ctx,
            r#"PlayerCore.clipFileLabel({ path: '\\\\?\\C:\\secret\\folders\\clip.mp4' })"#,
        ),
        "clip.mp4"
    );
    assert_eq!(
        eval(&mut ctx, r#"PlayerCore.clipFileLabel({ path: 'C:/Clips/clip.mp4' })"#),
        "clip.mp4"
    );
    assert_eq!(eval(&mut ctx, "PlayerCore.clipFileLabel(null)"), "");
}

#[test]
fn cloud_library_entries_filter_sort_and_mark_local_availability() {
    let mut ctx = player_core_context();
    let entries = eval_json(
        &mut ctx,
        r#"PlayerCore.cloudLibraryEntries({
          old: {
            local_clip_id: 'old',
            path: 'C:/Clips/old clip.mp4',
            remote_url: 'https://clips.example.com/old',
            visibility: 'public',
            upload_status: 'uploaded_public',
            updated_at_unix: 10
          },
          pending: {
            local_clip_id: 'pending',
            path: 'C:/Clips/pending clip.mp4',
            remote_url: 'https://clips.example.com/pending',
            visibility: 'private',
            upload_status: 'processing',
            updated_at_unix: 30
          },
          localGone: {
            local_clip_id: 'gone',
            path: 'C:/Clips/gone clip.mp4',
            remote_url: 'https://clips.example.com/gone',
            visibility: 'unlisted',
            upload_status: 'uploaded_processing',
            updated_at_unix: 20
          },
          failed: {
            local_clip_id: 'failed',
            path: 'C:/Clips/failed clip.mp4',
            remote_url: 'https://clips.example.com/failed',
            upload_status: 'failed',
            updated_at_unix: 40
          },
          localOnly: {
            local_clip_id: 'local',
            path: 'C:/Clips/local only.mp4',
            upload_status: 'not_uploaded',
            updated_at_unix: 50
          }
        }, [
          { path: 'C:/Clips/pending clip.mp4' },
          { path: 'C:/Clips/old clip.mp4' }
        ])"#,
    );

    assert_eq!(
        entries,
        r#"[{"local_clip_id":"pending","path":"C:/Clips/pending clip.mp4","title":"pending clip","remote_url":"https://clips.example.com/pending","visibility":"private","upload_status":"processing","updated_at_unix":30,"local_available":true},{"local_clip_id":"gone","path":"C:/Clips/gone clip.mp4","title":"gone clip","remote_url":"https://clips.example.com/gone","visibility":"unlisted","upload_status":"uploaded_processing","updated_at_unix":20,"local_available":false},{"local_clip_id":"old","path":"C:/Clips/old clip.mp4","title":"old clip","remote_url":"https://clips.example.com/old","visibility":"public","upload_status":"uploaded_public","updated_at_unix":10,"local_available":true}]"#
    );
}

#[test]
fn cloud_library_entries_match_legacy_windows_canonical_paths() {
    let mut ctx = player_core_context();
    assert_eq!(
        eval(
            &mut ctx,
            r#"PlayerCore.cloudLibraryEntries({
              legacy: {
                local_clip_id: 'legacy',
                path: '\\\\?\\D:\\Videos\\Clipline\\clip.mp4',
                remote_url: 'https://clips.example.com/legacy',
                visibility: 'public',
                upload_status: 'uploaded_public',
                updated_at_unix: 10
              }
            }, [{ path: 'D:\\Videos\\Clipline\\clip.mp4' }])[0].local_available"#,
        ),
        "true"
    );
}

#[test]
fn authoritative_cloud_library_hides_deleted_upload_history_but_keeps_active_uploads() {
    let mut ctx = player_core_context();
    let entries = eval_json(
        &mut ctx,
        r#"PlayerCore.cloudLibraryEntries({
          deleted: {
            local_clip_id: 'deleted',
            path: 'C:/Clips/deleted.mp4',
            remote_clip_id: 'missing-remote',
            remote_url: 'https://clips.example.com/missing-remote',
            upload_status: 'uploaded_public',
            updated_at_unix: 10
          },
          active: {
            local_clip_id: 'active',
            path: 'C:/Clips/active.mp4',
            remote_clip_id: 'processing-remote',
            remote_url: 'https://clips.example.com/processing-remote',
            upload_status: 'uploaded_processing',
            updated_at_unix: 20
          }
        }, [], [], true)"#,
    );

    assert_eq!(
        entries,
        r#"[{"local_clip_id":"active","path":"C:/Clips/active.mp4","title":"active","remote_url":"https://clips.example.com/processing-remote","visibility":"public","upload_status":"uploaded_processing","updated_at_unix":20,"local_available":false,"remote_clip_id":"processing-remote"}]"#
    );
}

#[test]
fn cloud_library_entries_prefer_authoritative_cloud_list() {
    let mut ctx = player_core_context();
    let entries = eval_json(
        &mut ctx,
        r#"PlayerCore.cloudLibraryEntries({
          localKnown: {
            local_clip_id: 'localKnown',
            path: 'C:/Clips/local known.mp4',
            remote_clip_id: 'remote-known-old',
            remote_url: 'https://clips.example.com/old-known',
            visibility: 'private',
            upload_status: 'uploaded_private',
            updated_at_unix: 10
          },
          localOnlyHistory: {
            local_clip_id: 'localOnlyHistory',
            path: 'C:/Clips/local history.mp4',
            remote_clip_id: 'remote-history',
            remote_url: 'https://clips.example.com/history',
            visibility: 'public',
            upload_status: 'uploaded_public',
            updated_at_unix: 20
          }
        }, [
          { path: 'C:/Clips/local known.mp4' },
          { path: 'C:/Clips/local history.mp4' }
        ], [
          {
            remote_clip_id: 'remote-known',
            local_clip_id: 'localKnown',
            title: 'Server Known',
            remote_url: '',
            visibility: 'private',
            upload_status: 'uploaded_private',
            updated_at_unix: 40
          },
          {
            remote_clip_id: 'remote-cloud-only',
            local_clip_id: null,
            title: 'Other Device',
            remote_url: 'https://clips.example.com/clip/remote-cloud-only',
            visibility: 'unlisted',
            upload_status: 'uploaded_public',
            updated_at_unix: 30
          }
        ])"#,
    );

    assert_eq!(
        entries,
        r#"[{"local_clip_id":"localKnown","path":"C:/Clips/local known.mp4","title":"Server Known","remote_url":"","visibility":"private","upload_status":"uploaded_private","updated_at_unix":40,"local_available":true,"remote_clip_id":"remote-known"},{"local_clip_id":"","path":"","title":"Other Device","remote_url":"https://clips.example.com/clip/remote-cloud-only","visibility":"unlisted","upload_status":"uploaded_public","updated_at_unix":30,"local_available":false,"remote_clip_id":"remote-cloud-only"},{"local_clip_id":"localOnlyHistory","path":"C:/Clips/local history.mp4","title":"local history","remote_url":"https://clips.example.com/history","visibility":"public","upload_status":"uploaded_public","updated_at_unix":20,"local_available":true,"remote_clip_id":"remote-history"}]"#
    );
}

#[test]
fn cloud_library_entries_keep_private_uploads_without_share_urls() {
    let mut ctx = player_core_context();
    let entries = eval_json(
        &mut ctx,
        r#"PlayerCore.cloudLibraryEntries({
          privateClip: {
            local_clip_id: 'privateClip',
            path: 'C:/Clips/private.mp4',
            remote_clip_id: 'remote-private',
            remote_url: null,
            visibility: 'private',
            upload_status: 'uploaded_private',
            updated_at_unix: 50
          }
        }, [{ path: 'C:/Clips/private.mp4' }])"#,
    );

    assert_eq!(
        entries,
        r#"[{"local_clip_id":"privateClip","path":"C:/Clips/private.mp4","title":"private","remote_url":"","visibility":"private","upload_status":"uploaded_private","updated_at_unix":50,"local_available":true,"remote_clip_id":"remote-private"}]"#
    );
}

#[test]
fn fmt_tenths_keeps_a_tenth_and_carries() {
    let mut ctx = player_core_context();
    assert_eq!(eval(&mut ctx, "PlayerCore.fmtTenths(0)"), "0:00.0");
    assert_eq!(eval(&mut ctx, "PlayerCore.fmtTenths(7.34)"), "0:07.3");
    assert_eq!(eval(&mut ctx, "PlayerCore.fmtTenths(59.97)"), "1:00.0");
    assert_eq!(eval(&mut ctx, "PlayerCore.fmtTenths(75.25)"), "1:15.3");
    assert_eq!(eval(&mut ctx, "PlayerCore.fmtTenths(NaN)"), "?");
}

#[test]
fn fmt_bytes_switches_units_at_a_gigabyte() {
    let mut ctx = player_core_context();
    assert_eq!(
        eval(&mut ctx, "PlayerCore.fmtBytes(5 * 1024 * 1024)"),
        "5.0 MB"
    );
    assert_eq!(
        eval(&mut ctx, "PlayerCore.fmtBytes(1536 * 1024 * 1024)"),
        "1.5 GB"
    );
}

#[test]
fn clip_size_switches_from_megabytes_to_gigabytes() {
    let mut ctx = player_core_context();
    assert_eq!(
        eval(&mut ctx, "PlayerCore.fmtMegabytes(1559.7)"),
        "1.5 GB"
    );
}

#[test]
fn library_storage_usage_formats_used_bytes_and_quota() {
    let mut ctx = player_core_context();
    assert_eq!(
        eval(
            &mut ctx,
            "PlayerCore.fmtLibraryStorageUsage(8.4 * 1024 * 1024 * 1024, 100)"
        ),
        "8.4 GB / 100 GB"
    );
    assert_eq!(
        eval(
            &mut ctx,
            "PlayerCore.fmtLibraryStorageUsage(512 * 1024 * 1024, 0)"
        ),
        "512.0 MB / no limit"
    );
    assert_eq!(
        eval(
            &mut ctx,
            "PlayerCore.fmtLibraryStorageUsage(10 * 1024 * 1024, 0.01)"
        ),
        "10.0 MB / 0.01 GB"
    );
}

#[test]
fn setting_duration_labels_are_human_readable() {
    let mut ctx = player_core_context();
    assert_eq!(
        eval(&mut ctx, "PlayerCore.settingDurationLabel(45)"),
        "45 sec"
    );
    assert_eq!(
        eval(&mut ctx, "PlayerCore.settingDurationLabel(60)"),
        "1 min"
    );
    assert_eq!(
        eval(&mut ctx, "PlayerCore.settingDurationLabel(90)"),
        "1 min 30 sec"
    );
    assert_eq!(
        eval(&mut ctx, "PlayerCore.settingDurationLabel(120)"),
        "2 min"
    );
}

#[test]
fn recording_quality_labels_include_bitrate_amounts() {
    let mut ctx = player_core_context();
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.recordingQualityPreset(0)"),
        r#"{"id":"compact","label":"Compact","hint":"smaller files","bitrate":6}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.recordingQualityPreset(1)"),
        r#"{"id":"balanced","label":"Balanced","hint":"good default","bitrate":12}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.recordingQualityPreset(2)"),
        r#"{"id":"sharp","label":"Sharp","hint":"more detail","bitrate":24}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.recordingQualityPreset(3)"),
        r#"{"id":"maximum","label":"Maximum","hint":"largest files","bitrate":40}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.recordingQualityPreset(1, '720p')"),
        r#"{"id":"balanced","label":"Balanced","hint":"good default","bitrate":5}"#
    );
    assert_eq!(eval(&mut ctx, "PlayerCore.qualityIndexForBitrate(13)"), "1");
    assert_eq!(eval(&mut ctx, "PlayerCore.qualityIndexForBitrate(35)"), "3");
    assert_eq!(eval(&mut ctx, "PlayerCore.qualityIndexForId('sharp')"), "2");
    assert_eq!(
        eval(
            &mut ctx,
            "PlayerCore.recordingQualitySummary(PlayerCore.recordingQualityPreset(2))"
        ),
        "Sharp quality - more detail. 24 Mbps."
    );
}

#[test]
fn smoothness_slider_maps_to_valid_fps_values() {
    let mut ctx = player_core_context();
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.smoothnessPreset(0)"),
        r#"{"fps":30,"label":"30 FPS","hint":"lighter on the PC"}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.smoothnessPreset(1)"),
        r#"{"fps":60,"label":"60 FPS","hint":"good default for most games"}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.smoothnessPreset(2)"),
        r#"{"fps":90,"label":"90 FPS","hint":"smoother for high-refresh play"}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.smoothnessPreset(3)"),
        r#"{"fps":120,"label":"120 FPS","hint":"best for high-refresh footage"}"#
    );
    assert_eq!(eval(&mut ctx, "PlayerCore.smoothnessIndexForFps(60)"), "1");
    assert_eq!(eval(&mut ctx, "PlayerCore.smoothnessIndexForFps(90)"), "2");
    assert_eq!(eval(&mut ctx, "PlayerCore.smoothnessIndexForFps(120)"), "3");
}

#[test]
fn output_resolution_options_have_stable_ids_and_fallback() {
    let mut ctx = player_core_context();
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.outputResolutionOption('1080p')"),
        r#"{"id":"1080p","label":"1080p","hint":"up to 1920 x 1080"}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.outputResolutionOption('unknown')"),
        r#"{"id":"source","label":"Source","hint":"uses the captured size"}"#
    );
}

#[test]
fn capture_source_labels_are_sidebar_friendly() {
    let mut ctx = player_core_context();
    assert_eq!(
        eval(
            &mut ctx,
            "PlayerCore.captureSourceLabel({ capture_mode: 'primary_monitor' })"
        ),
        "Desktop"
    );
    assert_eq!(
        eval(
            &mut ctx,
            "PlayerCore.captureSourceLabel({ capture_mode: 'window_title', window_title: 'League of Legends' })"
        ),
        "Window: League of Legends"
    );
    assert_eq!(
        eval(
            &mut ctx,
            "PlayerCore.captureSourceLabel({ capture_mode: 'display_region' })"
        ),
        "Display region"
    );
}

#[test]
fn fmt_ago_is_pure_in_now() {
    let mut ctx = player_core_context();
    assert_eq!(eval(&mut ctx, "PlayerCore.fmtAgo(1000, 958)"), "just now");
    assert_eq!(
        eval(&mut ctx, "PlayerCore.fmtAgo(1000, 940)"),
        "1 minute ago"
    );
    assert_eq!(
        eval(&mut ctx, "PlayerCore.fmtAgo(1000, 700)"),
        "5 minutes ago"
    );
    assert_eq!(
        eval(&mut ctx, "PlayerCore.fmtAgo(11800, 1000)"),
        "3 hours ago"
    );
    assert_eq!(
        eval(&mut ctx, "PlayerCore.fmtAgo(1000000, 827200)"),
        "2 days ago"
    );
    assert_eq!(
        eval(&mut ctx, "PlayerCore.fmtAgo(2000000, 185600)"),
        "3 weeks ago"
    );
    // Clock skew must not produce negative ages.
    assert_eq!(eval(&mut ctx, "PlayerCore.fmtAgo(1000, 1005)"), "just now");
}

#[test]
fn clamp_time_and_percent_respect_duration() {
    let mut ctx = player_core_context();
    assert_eq!(eval(&mut ctx, "PlayerCore.clampTime(-3, 10)"), "0");
    assert_eq!(eval(&mut ctx, "PlayerCore.clampTime(12, 10)"), "10");
    // Unknown duration must not clamp the high side to zero.
    assert_eq!(eval(&mut ctx, "PlayerCore.clampTime(5, 0)"), "5");
    assert_eq!(eval(&mut ctx, "PlayerCore.percentFor(5, 10)"), "50");
    assert_eq!(eval(&mut ctx, "PlayerCore.percentFor(15, 10)"), "100");
    assert_eq!(eval(&mut ctx, "PlayerCore.percentFor(3, 0)"), "0");
}

#[test]
fn relative_seek_accumulates_from_logical_target() {
    let mut ctx = player_core_context();
    assert_eq!(
        eval(&mut ctx, "PlayerCore.relativeSeekTarget(5, 10, 5, 60)"),
        "15"
    );
    assert_eq!(
        eval(&mut ctx, "PlayerCore.relativeSeekTarget(58, null, 5, 60)"),
        "60"
    );
    assert_eq!(
        eval(
            &mut ctx,
            "[5, 5, 5, 5, 5].reduce((target, delta) => PlayerCore.relativeSeekTarget(0, target, delta, 60), null)"
        ),
        "25"
    );
}

#[test]
fn logical_seek_survives_early_seeked_and_source_swap() {
    let mut ctx = player_core_context();
    let result = eval_json(
        &mut ctx,
        r#"
        (() => {
          let state = PlayerCore.createLogicalSeekState();
          state = PlayerCore.beginSourceAssignment(state, 1, 10, 60);
          let decision = PlayerCore.metadataSeekDecision(state, 1, 60);
          state = decision.state;
          state = PlayerCore.seekedDecision(state, 1, 10, 60).state;
          for (const delta of [5, 5, 5, 5, 5]) {
            const target = PlayerCore.relativeSeekTarget(10, state.targetTime, delta, 60);
            state = PlayerCore.requestLogicalSeek(state, target, 60);
          }
          state = PlayerCore.beginSourceAssignment(state, 2, 0, 60);
          const early = PlayerCore.seekedDecision(state, 2, 0, 60);
          const metadata = PlayerCore.metadataSeekDecision(early.state, 2, 60);
          const prior = PlayerCore.seekedDecision(metadata.state, 2, 30, 60);
          const arrived = PlayerCore.seekedDecision(prior.state, 2, 35, 60);
          return {
            targetAfterEarlyEvent: early.state.targetTime,
            earlyApply: early.applyTime,
            metadataApply: metadata.applyTime,
            priorApply: prior.applyTime,
            confirmed: arrived.confirmed,
            finalTarget: arrived.state.targetTime,
          };
        })()
        "#,
    );

    assert_eq!(
        result,
        r#"{"targetAfterEarlyEvent":35,"earlyApply":null,"metadataApply":35,"priorApply":35,"confirmed":true,"finalTarget":null}"#
    );
}

#[test]
fn logical_seek_ignores_invalid_requests_and_clamps_when_metadata_arrives() {
    let mut ctx = player_core_context();
    assert_eq!(
        eval_json(
            &mut ctx,
            r#"
            (() => {
              let state = PlayerCore.createLogicalSeekState();
              state = PlayerCore.requestLogicalSeek(state, 75, 0);
              state = PlayerCore.requestLogicalSeek(state, NaN, 0);
              const metadata = PlayerCore.metadataSeekDecision(
                PlayerCore.beginSourceAssignment(state, 7, 0, 60),
                7,
                60,
              );
              return {
                applyTime: metadata.applyTime,
                logical: PlayerCore.logicalPlaybackTime(metadata.state, 0, 60),
              };
            })()
            "#,
        ),
        r#"{"applyTime":60,"logical":60}"#
    );
}

#[test]
fn timeline_time_maps_pointer_x_to_clip_time() {
    let mut ctx = player_core_context();
    assert_eq!(
        eval(&mut ctx, "PlayerCore.timelineTime(150, 100, 200, 10)"),
        "2.5"
    );
    assert_eq!(
        eval(&mut ctx, "PlayerCore.timelineTime(50, 100, 200, 10)"),
        "0"
    );
    assert_eq!(
        eval(&mut ctx, "PlayerCore.timelineTime(400, 100, 200, 10)"),
        "10"
    );
}

#[test]
fn resolve_trim_clamps_and_keeps_minimum_gap() {
    let mut ctx = player_core_context();
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.resolveTrim(2, 8, 10)"),
        r#"{"start":2,"end":8}"#
    );
    // Non-finite inputs fall back to the full clip.
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.resolveTrim(NaN, NaN, 10)"),
        r#"{"start":0,"end":10}"#
    );
    // Inverted bounds push the out point forward by the minimum gap.
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.resolveTrim(5, 3, 10)"),
        r#"{"start":5,"end":5.1}"#
    );
    // At end-of-clip the gap is taken from the in point instead.
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.resolveTrim(12, 0, 10)"),
        r#"{"start":9.9,"end":10}"#
    );
    // Zero-duration (metadata not loaded yet) stays sane.
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.resolveTrim(0, 0, 0)"),
        r#"{"start":0,"end":0}"#
    );
}

#[test]
fn trim_drag_keeps_handles_apart() {
    let mut ctx = player_core_context();
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.trimDrag('in', 9.95, 2, 10, 10)"),
        r#"{"start":9.9,"end":10}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.trimDrag('out', 1, 2, 8, 10)"),
        r#"{"start":2,"end":2.1}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.trimDrag('in', 4, 2, 10, 10)"),
        r#"{"start":4,"end":10}"#
    );
}

#[test]
fn slide_trim_moves_the_selection_and_clamps() {
    let mut ctx = player_core_context();
    // Mid-clip slide preserves the length.
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.slideTrim(10, 30, 15, 100)"),
        r#"{"start":15,"end":35}"#
    );
    // Sliding past either end clamps while keeping the length.
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.slideTrim(70, 90, 95, 100)"),
        r#"{"start":80,"end":100}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.slideTrim(10, 30, -5, 100)"),
        r#"{"start":0,"end":20}"#
    );
}

#[test]
fn trim_summary_reports_range_and_length() {
    let mut ctx = player_core_context();
    assert_eq!(
        eval(&mut ctx, "PlayerCore.trimSummary(2, 18.4)"),
        "keeps 0:02.0 \u{2013} 0:18.4 \u{b7} 16.4 s"
    );
    assert_eq!(
        eval(&mut ctx, "PlayerCore.trimSummary(0, 0)"),
        "keeps 0:00.0 \u{2013} 0:00.0 \u{b7} 0.0 s"
    );
}

#[test]
fn marker_navigation_skips_nearby_and_wraps() {
    let mut ctx = player_core_context();
    ctx.eval(Source::from_bytes(
        "const M = [{ t_s: 1 }, { t_s: 5 }, { t_s: 9 }];",
    ))
    .expect("define markers");
    assert_eq!(eval(&mut ctx, "PlayerCore.nextMarker(M, 4.9).t_s"), "5");
    // A marker within the epsilon of the playhead is "current", not "next".
    assert_eq!(eval(&mut ctx, "PlayerCore.nextMarker(M, 4.96).t_s"), "9");
    assert_eq!(eval(&mut ctx, "PlayerCore.nextMarker(M, 9.5).t_s"), "1");
    assert_eq!(eval(&mut ctx, "PlayerCore.prevMarker(M, 5).t_s"), "1");
    assert_eq!(eval(&mut ctx, "PlayerCore.prevMarker(M, 0.5).t_s"), "9");
    assert_eq!(eval_json(&mut ctx, "PlayerCore.nextMarker([], 0)"), "null");
    assert_eq!(eval_json(&mut ctx, "PlayerCore.prevMarker([], 0)"), "null");
}

#[test]
fn game_event_active_index_honors_clicked_event_during_lead_in() {
    let mut ctx = player_core_context();
    ctx.eval(Source::from_bytes(
        "const M = [{ t_s: 72 }, { t_s: 140 }, { t_s: 161 }];",
    ))
    .expect("define markers");
    assert_eq!(
        eval(&mut ctx, "PlayerCore.gameEventActiveIndex(M, 158)"),
        "1"
    );
    assert_eq!(
        eval(&mut ctx, "PlayerCore.gameEventActiveIndex(M, 158, 2)"),
        "2"
    );
    assert_eq!(
        eval(&mut ctx, "PlayerCore.gameEventActiveIndex(M, 161.2, 2)"),
        "2"
    );
    assert_eq!(
        eval(&mut ctx, "PlayerCore.gameEventActiveIndex(M, 158, 99)"),
        "1"
    );
    assert_eq!(
        eval(&mut ctx, "PlayerCore.gameEventActiveIndex([], 158, 2)"),
        "-1"
    );
}

#[test]
fn review_marker_filters_apply_per_surface_game_settings() {
    let mut ctx = player_core_context();
    ctx.eval(Source::from_bytes(
        r#"
        const REVIEW_SUMMARY = {
          player_name: 'Dain',
          champion_name: 'Nautilus',
          team: 'ORDER',
          participants: [
            { player_name: 'Dain', champion_name: 'Nautilus', team: 'ORDER' },
            { player_name: 'Ally Bot', champion_name: 'Ezreal', team: 'ORDER' },
            { player_name: 'Enemy Mid', champion_name: 'Ahri', team: 'CHAOS' },
            { player_name: 'Enemy Jungle', champion_name: 'Zed', team: 'CHAOS' }
          ]
        };
        const REVIEW_MARKERS = [
          { id: 'local-kill', kind: 'ChampionKill', actor: 'Dain', victim: 'Enemy Mid', involves_local_player: true },
          { id: 'local-assist', kind: 'ChampionAssist', actor: 'Ally Bot', victim: 'Enemy Mid', assisters: ['Dain'], involves_local_player: true },
          { id: 'local-death', kind: 'ChampionDeath', actor: 'Enemy Jungle', victim: 'Dain', involves_local_player: true },
          { id: 'team-kill', kind: 'ChampionKill', actor: 'Ally Bot', victim: 'Enemy Jungle', involves_local_player: false },
          { id: 'enemy-kill', kind: 'ChampionKill', actor: 'Enemy Mid', victim: 'Ally Bot', involves_local_player: false },
          { id: 'dragon', kind: 'DragonKill', actor: 'Ally Bot', involves_local_player: false },
          { id: 'herald', kind: 'HeraldKill', actor: 'Enemy Mid', involves_local_player: false },
          { id: 'turret', kind: 'TurretKilled', actor: 'Ally Bot', involves_local_player: false },
          { id: 'inhib', kind: 'InhibKilled', actor: 'Ally Bot', involves_local_player: false },
          { id: 'first-blood', kind: 'FirstBlood', actor: 'Dain', victim: 'Enemy Mid', involves_local_player: true },
          { id: 'noise', kind: 'MinionsSpawning', actor: '', involves_local_player: false }
        ];
        const REVIEW_SETTINGS = {
          enabled: true,
          match_events: {
            enabled: true,
            user_kills: true,
            user_deaths: true,
            user_assists: true,
            team_kills: false,
            team_deaths: false,
            enemy_kills: true,
            enemy_deaths: false,
            objectives: true,
            turrets: true
          },
          timeline_markers: {
            enabled: true,
            user_kills: false,
            user_deaths: true,
            user_assists: true,
            objectives: true,
            turrets: false
          }
        };
        "#,
    ))
    .expect("define review marker filter inputs");

    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.reviewMatchEventMarkers(REVIEW_MARKERS, REVIEW_SUMMARY, REVIEW_SETTINGS).map(m => m.id)"
        ),
        r#"["local-kill","local-assist","local-death","enemy-kill","dragon","herald","turret","inhib"]"#
    );
    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.reviewTimelineMarkers(REVIEW_MARKERS, REVIEW_SUMMARY, REVIEW_SETTINGS).map(m => m.id)"
        ),
        r#"["local-assist","local-death","dragon","herald"]"#
    );
    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.reviewMatchEventMarkers(REVIEW_MARKERS, REVIEW_SUMMARY, { ...REVIEW_SETTINGS, enabled: false })"
        ),
        "[]"
    );
    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.reviewTimelineMarkers(REVIEW_MARKERS, REVIEW_SUMMARY, { ...REVIEW_SETTINGS, timeline_markers: { ...REVIEW_SETTINGS.timeline_markers, enabled: false } })"
        ),
        "[]"
    );
}

#[test]
fn user_bookmarks_join_the_timeline_regardless_of_game_review_filters() {
    let mut ctx = player_core_context();
    ctx.eval(Source::from_bytes(
        r#"
        const GAME_MARKERS = [
          { t_s: 5, kind: 'DragonKill', actor: 'Dain' },
          { t_s: 40, kind: 'BaronKill', actor: 'Dain' }
        ];
        const BOOKMARKS = [{ t_s: 20 }, { t_s: 1 }];
        "#,
    ))
    .expect("fixtures evaluate");

    // Merged into one time-ordered list the timeline surfaces can consume.
    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.withBookmarks(GAME_MARKERS, BOOKMARKS).map(m => [m.t_s, m.kind])"
        ),
        r#"[[1,"Bookmark"],[5,"DragonKill"],[20,"Bookmark"],[40,"BaronKill"]]"#
    );
    // The whole point: a clip with no game, or with game review switched off,
    // still shows what the user marked by hand.
    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.withBookmarks(PlayerCore.reviewTimelineMarkers(GAME_MARKERS, null, { enabled: false }), BOOKMARKS).map(m => m.t_s)"
        ),
        "[1,20]"
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.withBookmarks([], [])"),
        "[]"
    );
    // Bookmarks get their own category, so no game's marker settings can hide
    // them and they never borrow a kill/objective color.
    assert_eq!(
        eval(&mut ctx, "PlayerCore.markerStyle('Bookmark').cls"),
        "bookmark"
    );
    assert_eq!(
        eval(
            &mut ctx,
            "PlayerCore.markerDigest(PlayerCore.withBookmarks([], BOOKMARKS))"
        ),
        "2 bookmarks"
    );
    assert_eq!(
        eval_json(
            &mut ctx,
            "[PlayerCore.isBookmarkMarker({ kind: 'Bookmark' }), PlayerCore.isBookmarkMarker({ kind: 'DragonKill' }), PlayerCore.isBookmarkMarker(null)]"
        ),
        "[true,false,false]"
    );
}

#[test]
fn bookmark_markers_drop_unusable_offsets_and_duplicates() {
    let mut ctx = player_core_context();

    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.bookmarkMarkers([{ t_s: 3 }, { t_s: 'x' }, { t_s: -1 }, {}, null, { t_s: 3 }]).map(m => m.t_s)"
        ),
        "[3]"
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.bookmarkMarkers(undefined)"),
        "[]"
    );
}

#[test]
fn review_marker_filters_honor_profile_declared_categories() {
    // A future supported game (CS2-shaped) whose kind names player-core has no
    // built-in knowledge of: its profile's marker_kinds categories alone must
    // opt events into the review surfaces.
    let mut ctx = player_core_context();
    ctx.eval(Source::from_bytes(
        r#"
        const CS_PRESENTATION = {
          marker_kinds: {
            PlayerKill: { category: 'kill' },
            PlayerDeath: { category: 'death' },
            BombPlanted: { category: 'objective' },
            RoundStart: { category: 'info' }
          }
        };
        const CS_SUMMARY = {
          player_name: 'Dain',
          team: 'CT',
          participants: [
            { player_name: 'Dain', team: 'CT' },
            { player_name: 'Rival', team: 'T' }
          ]
        };
        const CS_MARKERS = [
          { id: 'kill', kind: 'PlayerKill', actor: 'Dain', victim: 'Rival', involves_local_player: true },
          { id: 'death', kind: 'PlayerDeath', actor: 'Rival', victim: 'Dain', involves_local_player: true },
          { id: 'plant', kind: 'BombPlanted', actor: 'Rival', involves_local_player: false },
          { id: 'round', kind: 'RoundStart', actor: '', involves_local_player: false }
        ];
        "#,
    ))
    .expect("define profile-driven filter inputs");

    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.reviewTimelineMarkers(CS_MARKERS, CS_SUMMARY, null, CS_PRESENTATION).map(m => m.id)"
        ),
        r#"["kill","death","plant"]"#
    );
    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.reviewMatchEventMarkers(CS_MARKERS, CS_SUMMARY, null, CS_PRESENTATION).map(m => m.id)"
        ),
        r#"["kill","death","plant"]"#
    );
    // Without the profile the kinds are unknown -> info category -> filtered out.
    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.reviewTimelineMarkers(CS_MARKERS, CS_SUMMARY, null, null)"
        ),
        "[]"
    );
}

#[test]
fn marker_count_pluralizes() {
    let mut ctx = player_core_context();
    assert_eq!(eval(&mut ctx, "PlayerCore.markerSummary([])"), "no markers");
    assert_eq!(
        eval(&mut ctx, "PlayerCore.markerSummary([{ t_s: 1 }])"),
        "1 marker"
    );
    assert_eq!(
        eval(
            &mut ctx,
            "PlayerCore.markerSummary([{ t_s: 1 }, { t_s: 2 }])"
        ),
        "2 markers"
    );
}

#[test]
fn osu_play_blocks_format_intervals_and_details() {
    let mut ctx = player_core_context();
    ctx.eval(Source::from_bytes(
        r#"
        const OSU_PLAYS = [
          {
            external_id: 'score-2',
            artist: 'Camellia',
            title: "Exit This Earth's Atomosphere",
            difficulty: 'Extra',
            cover_url: 'https://assets.ppy.sh/beatmaps/1/covers/list.jpg',
            star_rating: 6.54,
            mods: ['HD', 'DT'],
            rank: 'A',
            passed: false,
            accuracy: 0.9876,
            derived_start: true,
            t_start_s: 10,
            t_end_s: 50
          },
          {
            external_id: 'score-3',
            artist: 'xi',
            title: 'Blue Zenith',
            difficulty: 'FOUR DIMENSIONS',
            cover_url: 'https://assets.ppy.sh/beatmaps/2/covers/list.jpg',
            star_rating: 5.43,
            mods: ['CL'],
            rank: 'S',
            passed: true,
            accuracy: 0.9912,
            pp: 321.4,
            derived_start: false,
            t_start_s: 60,
            t_end_s: 120
          }
        ];
        "#,
    ))
    .expect("define osu plays");

    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.playBlocks(OSU_PLAYS, 200).map(p => ({ id: p.externalId, left: p.leftPct, width: p.widthPct, title: p.title, details: p.details, estimated: p.estimated, incomplete: p.incomplete }))"
        ),
        r#"[{"id":"score-2","left":5,"width":20,"title":"Camellia - Exit This Earth's Atomosphere [Extra]","details":"Incomplete · A · 98.76% · +HDDT","estimated":true,"incomplete":true},{"id":"score-3","left":30,"width":30,"title":"xi - Blue Zenith [FOUR DIMENSIONS]","details":"Passed · S · 99.12% · 321pp","estimated":false,"incomplete":false}]"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.playRailItem(OSU_PLAYS[0])"),
        r#"{"title":"Camellia - Exit This Earth's Atomosphere [Extra]","artistTitle":"Camellia - Exit This Earth's Atomosphere","difficulty":"Extra","mods":"+HDDT","starRating":"6.54★","coverUrl":"https://assets.ppy.sh/beatmaps/1/covers/list.jpg","rank":"A","pp":"","accuracy":"98.76%","meta":"A ▸ Incomplete ▸ 98.76%","time":"0:10.0-0:50.0"}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.playRailItem(OSU_PLAYS[1])"),
        r#"{"title":"xi - Blue Zenith [FOUR DIMENSIONS]","artistTitle":"xi - Blue Zenith","difficulty":"FOUR DIMENSIONS","mods":"","starRating":"5.43★","coverUrl":"https://assets.ppy.sh/beatmaps/2/covers/list.jpg","rank":"S","pp":"321pp","accuracy":"99.12%","meta":"S ▸ 321pp ▸ 99.12%","time":"1:00.0-2:00.0"}"#
    );
    assert_eq!(
        eval(
            &mut ctx,
            "PlayerCore.playRailItem({ artist: 'ZUN', title: 'Necro-Fantasia', difficulty: 'Hard', beatmapset_id: 456, rank: 'B', accuracy: 0.95, t_start_s: 1 }).coverUrl"
        ),
        "https://assets.ppy.sh/beatmaps/456/covers/list.jpg"
    );
    assert_eq!(
        eval(&mut ctx, "PlayerCore.playRailItem({ artist: 'ZUN', title: 'Necro-Fantasia', difficulty: 'Hard', rank: 'B', accuracy: 0.95, t_start_s: 1, t_end_s: 30 }).meta"),
        "B ▸ Incomplete ▸ 95.00%"
    );
    assert_eq!(
        eval(&mut ctx, "PlayerCore.playRailItem({ artist: 'ZUN', title: 'Necro-Fantasia', difficulty: 'Hard', passed: true, rank: 'S', accuracy: 0.99, t_start_s: 1, t_end_s: 30 }).meta"),
        "S ▸ 99.00%"
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.playBlocks([{ passed: true, rank: 'S', accuracy: 0.99, t_start_s: 1, t_end_s: 30 }], 60).map(p => ({ details: p.details, incomplete: p.incomplete }))"),
        r#"[{"details":"Passed · S · 99.00%","incomplete":false}]"#
    );
    assert_eq!(
        eval(&mut ctx, "PlayerCore.playRailItem({ artist: 'ZUN', title: 'Necro-Fantasia', difficulty: 'Hard', rank: 'F', accuracy: 0.95, t_start_s: 1, t_end_s: 30 }).meta"),
        "F ▸ 95.00%"
    );
    assert_eq!(
        eval(&mut ctx, "PlayerCore.playRailItem({ artist: 'ZUN', title: 'Necro-Fantasia', difficulty: 'Hard', t_start_s: 1, t_end_s: 30 }).meta"),
        "Incomplete"
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.playExportRange(OSU_PLAYS[0])"),
        r#"{"start":10,"end":50}"#
    );
    assert_eq!(
        eval(
            &mut ctx,
            "PlayerCore.playExportRange({ t_start_s: 4 }) === null"
        ),
        "true"
    );
    assert_eq!(
        eval(&mut ctx, "PlayerCore.playSummary(OSU_PLAYS)"),
        "2 submitted plays"
    );
    assert_eq!(
        eval(
            &mut ctx,
            "PlayerCore.playResultSummary([{ passed: true, rank: 'S' }, { passed: false, rank: 'A' }, { passed: false, rank: 'B' }, { passed: false, rank: 'F' }])"
        ),
        "1 pass · 2 incomplete · 1 fail"
    );
    assert_eq!(
        eval(&mut ctx, "PlayerCore.playActiveIndex(OSU_PLAYS, 75)"),
        "1"
    );
    assert_eq!(
        eval(
            &mut ctx,
            "PlayerCore.playActiveIndex([{ t_start_s: 10, t_end_s: 70 }, { t_start_s: 45, t_end_s: 90 }], 50)"
        ),
        "1"
    );
    assert_eq!(
        eval(&mut ctx, "PlayerCore.playActiveIndex(OSU_PLAYS, 45, 1)"),
        "1"
    );
}

#[test]
fn osu_gallery_card_preview_summarizes_play_sidecar() {
    let mut ctx = player_core_context();
    ctx.eval(Source::from_bytes(
        r#"
        const OSU_CARD_CLIP = {
          markers: {
            plays: [
              { external_id: 'score-1', passed: true, t_start_s: 3, t_end_s: 40 },
              { external_id: 'score-2', passed: false, rank: 'F', t_start_s: 50, t_end_s: 80 }
            ]
          }
        };
        const OSU_PRESENTATION = {
          gallery: {
            summary: 'osu_set_plays',
            card: { title: 'osu_session_summary' }
          }
        };
        "#,
    ))
    .expect("define osu gallery preview");

    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.galleryCardPreview(OSU_CARD_CLIP, 'session', 'Jun 30 · 8:20 PM', OSU_PRESENTATION)"
        ),
        r#"{"title":"2 submitted plays","titleSource":"summary","summary":"1 pass · 1 fail"}"#
    );
}

#[test]
fn osu_gallery_card_preview_uses_clip_name_for_non_session_exports() {
    let mut ctx = player_core_context();
    ctx.eval(Source::from_bytes(
        r#"
        const OSU_EXPORT_PRESENTATION = {
          gallery: {
            summary: 'osu_set_plays',
            card: { title: 'osu_session_summary' }
          }
        };
        "#,
    ))
    .expect("define osu export gallery preview");

    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.galleryCardPreview({ name: 'I MY ME MINE - Trouble.mp4', markers: {} }, 'replay', 'Jul 1 · 1:20 AM', OSU_EXPORT_PRESENTATION)"
        ),
        r#"{"title":"I MY ME MINE - Trouble","titleSource":"clip","summary":""}"#
    );
}

#[test]
fn split_output_defaults_to_fallback_while_master_toggles_process_tracks() {
    let mut ctx = player_core_context();
    let model = eval_json(
        &mut ctx,
        r#"
        (() => {
          const tracks = [
            { id: 'output', kind: 'output', label: 'Output Audio' },
            { id: 'process:1', kind: 'process_output', label: 'Game' },
            { id: 'process:2', kind: 'process_output', label: 'Discord' },
            { id: 'microphone', kind: 'microphone', label: 'Microphone' },
          ];
          const defaults = PlayerCore.defaultAudioTrackIds(tracks);
          const normalizedDefaults = PlayerCore.selectedAudioTrackIds(tracks, defaults);
          const afterOff = PlayerCore.applyAudioTrackToggle(tracks, defaults, 'output', false);
          const afterOn = PlayerCore.applyAudioTrackToggle(tracks, afterOff, 'output', true);
          return {
            defaults,
            normalizedDefaults,
            outputDefault: PlayerCore.audioTrackRowState(tracks[0], tracks, defaults),
            afterOff,
            afterOn,
            outputOn: PlayerCore.audioTrackRowState(tracks[0], tracks, afterOn),
            outputPartial: PlayerCore.audioTrackRowState(tracks[0], tracks, ['process:1', 'microphone']),
            effectiveAfterOn: PlayerCore.selectedAudioTrackIds(tracks, afterOn),
          };
        })()
        "#,
    );

    assert_eq!(
        model,
        r#"{"defaults":["output","microphone"],"normalizedDefaults":["output","microphone"],"outputDefault":{"checked":true,"indeterminate":false},"afterOff":["microphone"],"afterOn":["process:1","process:2","microphone"],"outputOn":{"checked":true,"indeterminate":false},"outputPartial":{"checked":false,"indeterminate":true},"effectiveAfterOn":["process:1","process:2","microphone"]}"#
    );
}

#[test]
fn normal_output_track_remains_directly_selectable() {
    let mut ctx = player_core_context();
    let model = eval_json(
        &mut ctx,
        r#"
        (() => {
          const tracks = [
            { id: 'output', kind: 'output', label: 'Output Audio' },
            { id: 'microphone', kind: 'microphone', label: 'Microphone' },
          ];
          const defaults = PlayerCore.defaultAudioTrackIds(tracks);
          const afterOff = PlayerCore.applyAudioTrackToggle(tracks, defaults, 'output', false);
          return {
            defaults,
            afterOff,
            outputOff: PlayerCore.audioTrackRowState(tracks[0], tracks, afterOff),
            effectiveAfterOff: PlayerCore.selectedAudioTrackIds(tracks, afterOff),
          };
        })()
        "#,
    );

    assert_eq!(
        model,
        r#"{"defaults":["output","microphone"],"afterOff":["microphone"],"outputOff":{"checked":false,"indeterminate":false},"effectiveAfterOff":["microphone"]}"#
    );
}

#[test]
fn multi_track_default_selection_requires_preview() {
    let mut ctx = player_core_context();
    let model = eval_json(
        &mut ctx,
        r#"
        (() => {
          const splitTracks = [
            { id: 'output', kind: 'output', label: 'Output Audio' },
            { id: 'process:1', kind: 'process_output', label: 'Game' },
            { id: 'microphone', kind: 'microphone', label: 'Microphone' },
          ];
          const normalTracks = [
            { id: 'output', kind: 'output', label: 'Output Audio' },
            { id: 'microphone', kind: 'microphone', label: 'Microphone' },
          ];
          const singleTrack = [
            { id: 'output', kind: 'output', label: 'Output Audio' },
          ];
          return {
            splitDefault: PlayerCore.selectionNeedsPreview(splitTracks, PlayerCore.defaultAudioTrackIds(splitTracks)),
            normalDefault: PlayerCore.selectionNeedsPreview(normalTracks, PlayerCore.defaultAudioTrackIds(normalTracks)),
            normalPartial: PlayerCore.selectionNeedsPreview(normalTracks, ['microphone']),
            singleDefault: PlayerCore.selectionNeedsPreview(singleTrack, PlayerCore.defaultAudioTrackIds(singleTrack)),
          };
        })()
        "#,
    );

    assert_eq!(
        model,
        r#"{"splitDefault":true,"normalDefault":true,"normalPartial":true,"singleDefault":false}"#
    );
}

#[test]
fn review_audio_defaults_to_direct_fallback_and_explicit_tracks_need_preview() {
    let mut ctx = player_core_context();
    let model = eval_json(
        &mut ctx,
        r#"
        (() => {
          const tracks = [
            { id: 'output', kind: 'output', label: 'Output Audio' },
            { id: 'process:1', kind: 'process_output', label: 'Game' },
            { id: 'microphone', kind: 'microphone', label: 'Microphone' },
          ];
          const fallback = PlayerCore.directPlaybackAudioTrackIds(tracks);
          const process = PlayerCore.applyReviewAudioTrackToggle(
            tracks, fallback, 'process:1', true,
          );
          const withMic = PlayerCore.applyReviewAudioTrackToggle(
            tracks, process, 'microphone', true,
          );
          const restored = PlayerCore.applyReviewAudioTrackToggle(
            tracks, withMic, 'output', true,
          );
          const muted = PlayerCore.applyReviewAudioTrackToggle(
            tracks, fallback, 'output', false,
          );
          return {
            fallback,
            fallbackRow: PlayerCore.reviewAudioTrackRowState(tracks[0], tracks, fallback),
            fallbackNeedsPreview: PlayerCore.reviewSelectionNeedsPreview(tracks, fallback),
            process,
            withMic,
            restored,
            muted,
            expandedNeedsPreview: PlayerCore.reviewSelectionNeedsPreview(tracks, withMic),
          };
        })()
        "#,
    );

    assert_eq!(
        model,
        r#"{"fallback":["output"],"fallbackRow":{"checked":true,"indeterminate":false},"fallbackNeedsPreview":false,"process":["process:1"],"withMic":["process:1","microphone"],"restored":["output","microphone"],"muted":[],"expandedNeedsPreview":true}"#
    );
}

#[test]
fn review_direct_audio_fallback_uses_stream_zero_not_marker_order() {
    let mut ctx = player_core_context();
    let model = eval_json(
        &mut ctx,
        r#"
        (() => {
          const reordered = [
            { id: 'microphone', kind: 'microphone', track_index: 1 },
            { id: 'output', kind: 'output', track_index: 0 },
          ];
          const legacy = [
            { id: 'legacy-output', kind: 'output' },
            { id: 'legacy-microphone', kind: 'microphone' },
          ];
          return {
            reordered: PlayerCore.directPlaybackAudioTrackIds(reordered),
            reorderedMicNeedsPreview: PlayerCore.reviewSelectionNeedsPreview(
              reordered,
              ['microphone'],
            ),
            legacy: PlayerCore.directPlaybackAudioTrackIds(legacy),
          };
        })()
        "#,
    );

    assert_eq!(
        model,
        r#"{"reordered":["output"],"reorderedMicNeedsPreview":true,"legacy":["legacy-output"]}"#
    );
}

#[test]
fn key_intents_cover_the_documented_shortcuts() {
    let mut ctx = player_core_context();
    for code in ["Space", "KeyK"] {
        assert_eq!(
            eval_json(&mut ctx, &format!("PlayerCore.keyIntent('{code}', false)")),
            r#"{"kind":"toggle-play"}"#
        );
    }
    // Arrows jump 5s (Shift 1s); J/L step frames.
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.keyIntent('ArrowLeft', false)"),
        r#"{"kind":"seek-by","seconds":-5}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.keyIntent('ArrowRight', false)"),
        r#"{"kind":"seek-by","seconds":5}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.keyIntent('ArrowLeft', true)"),
        r#"{"kind":"seek-by","seconds":-1}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.keyIntent('ArrowRight', true)"),
        r#"{"kind":"seek-by","seconds":1}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.keyIntent('KeyJ', false)"),
        r#"{"kind":"step-frame","dir":-1}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.keyIntent('KeyL', false)"),
        r#"{"kind":"step-frame","dir":1}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.keyIntent('KeyJ', true)"),
        r#"{"kind":"step-frame","dir":-1}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.keyIntent('KeyL', true)"),
        r#"{"kind":"step-frame","dir":1}"#
    );
    // ,/. are a fixed 0.1s nudge regardless of Shift.
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.keyIntent('Comma', false)"),
        r#"{"kind":"seek-by","seconds":-0.1}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.keyIntent('Period', false)"),
        r#"{"kind":"seek-by","seconds":0.1}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.keyIntent('KeyI', false)"),
        r#"{"kind":"set-in"}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.keyIntent('KeyO', false)"),
        r#"{"kind":"set-out"}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.keyIntent('KeyM', false)"),
        r#"{"kind":"next-marker"}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.keyIntent('KeyM', true)"),
        r#"{"kind":"prev-marker"}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.keyIntent('Escape', false)"),
        r#"{"kind":"close"}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.keyIntent('KeyZ', false)"),
        "null"
    );
    // Per-frame stepping lives on J/L.
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.keyIntent('KeyJ', false)"),
        r#"{"kind":"step-frame","dir":-1}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.keyIntent('KeyL', false)"),
        r#"{"kind":"step-frame","dir":1}"#
    );
    // Zoom controls.
    for code in ["Equal", "NumpadAdd"] {
        assert_eq!(
            eval_json(&mut ctx, &format!("PlayerCore.keyIntent('{code}', false)")),
            r#"{"kind":"zoom","factor":0.5}"#
        );
    }
    for code in ["Minus", "NumpadSubtract"] {
        assert_eq!(
            eval_json(&mut ctx, &format!("PlayerCore.keyIntent('{code}', false)")),
            r#"{"kind":"zoom","factor":2}"#
        );
    }
    // \ fits the trim selection (the editing default); Shift+\ fits the whole clip.
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.keyIntent('Backslash', false)"),
        r#"{"kind":"zoom-selection"}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.keyIntent('Backslash', true)"),
        r#"{"kind":"zoom-fit"}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.keyIntent('KeyZ', true)"),
        r#"{"kind":"zoom-fit"}"#
    );
    // Jump controls.
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.keyIntent('Home', false)"),
        r#"{"kind":"seek-to","seconds":0}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.keyIntent('End', false)"),
        r#"{"kind":"seek-to-end"}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.keyIntent('ArrowUp', false)"),
        r#"{"kind":"prev-edit"}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.keyIntent('ArrowDown', false)"),
        r#"{"kind":"next-edit"}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.keyIntent('KeyS', false)"),
        r#"{"kind":"toggle-snap"}"#
    );
}

#[test]
fn hotkey_recorder_formats_modifier_function_keys() {
    let mut ctx = player_core_context();
    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.hotkeyFromKeyEvent({ code: 'F10', ctrlKey: false, altKey: false, shiftKey: false })"
        ),
        r#"{"kind":"captured","value":"F10"}"#
    );
    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.hotkeyFromKeyEvent({ code: 'F9', ctrlKey: true, altKey: true, shiftKey: false })"
        ),
        r#"{"kind":"captured","value":"Ctrl+Alt+F9"}"#
    );
    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.hotkeyFromKeyEvent({ key: 'F13', ctrlKey: false, altKey: true, shiftKey: true })"
        ),
        r#"{"kind":"captured","value":"Alt+Shift+F13"}"#
    );
}

#[test]
fn hotkey_recorder_formats_modified_keyboard_keys() {
    let mut ctx = player_core_context();
    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.hotkeyFromKeyEvent({ code: 'KeyG', ctrlKey: true, altKey: false, shiftKey: false })"
        ),
        r#"{"kind":"captured","value":"Ctrl+G"}"#
    );
    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.hotkeyFromKeyEvent({ code: 'ArrowLeft', ctrlKey: false, altKey: true, shiftKey: true })"
        ),
        r#"{"kind":"captured","value":"Alt+Shift+ArrowLeft"}"#
    );
    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.hotkeyFromKeyEvent({ code: 'Digit1', ctrlKey: true, altKey: false, shiftKey: false })"
        ),
        r#"{"kind":"captured","value":"Ctrl+1"}"#
    );
    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.hotkeyFromKeyEvent({ code: 'Slash', ctrlKey: true, altKey: false, shiftKey: false })"
        ),
        r#"{"kind":"captured","value":"Ctrl+Slash"}"#
    );
}

#[test]
fn hotkey_recorder_formats_mouse_buttons() {
    let mut ctx = player_core_context();
    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.hotkeyFromMouseEvent({ button: 1, ctrlKey: true, altKey: false, shiftKey: false })"
        ),
        r#"{"kind":"captured","value":"Ctrl+Middle"}"#
    );
    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.hotkeyFromMouseEvent({ button: 3, ctrlKey: true, altKey: false, shiftKey: false })"
        ),
        r#"{"kind":"captured","value":"Ctrl+Mouse4"}"#
    );
    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.hotkeyFromMouseEvent({ button: 4, ctrlKey: false, altKey: true, shiftKey: true })"
        ),
        r#"{"kind":"captured","value":"Alt+Shift+Mouse5"}"#
    );
    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.hotkeyFromMouseEvent({ button: 0, ctrlKey: false, altKey: false, shiftKey: false })"
        ),
        r#"{"kind":"invalid","message":"Use middle, Mouse4, or Mouse5 as a mouse shortcut."}"#
    );
    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.hotkeyFromMouseEvent({ button: 1, ctrlKey: false, altKey: false, shiftKey: false })"
        ),
        r#"{"kind":"captured","value":"Middle"}"#
    );
}

#[test]
fn hotkey_recorder_reports_pending_cancel_and_invalid_inputs() {
    let mut ctx = player_core_context();
    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.hotkeyFromKeyEvent({ code: 'ControlLeft', ctrlKey: true })"
        ),
        r#"{"kind":"pending","message":"Now press an F-key, mouse button, or keyboard key."}"#
    );
    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.hotkeyFromKeyEvent({ code: 'Escape' })"
        ),
        r#"{"kind":"cancel"}"#
    );
    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.hotkeyFromKeyEvent({ code: 'F10', ctrlKey: false, altKey: false, shiftKey: false })"
        ),
        r#"{"kind":"captured","value":"F10"}"#
    );
    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.hotkeyFromKeyEvent({ code: 'F12', ctrlKey: false, altKey: false, shiftKey: false })"
        ),
        r#"{"kind":"invalid","message":"F12 is reserved by Windows for debuggers."}"#
    );
    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.hotkeyFromKeyEvent({ code: 'KeyS', ctrlKey: true, altKey: false, shiftKey: false })"
        ),
        r#"{"kind":"captured","value":"Ctrl+S"}"#
    );
    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.hotkeyFromKeyEvent({ code: 'KeyS', ctrlKey: false, altKey: false, shiftKey: false })"
        ),
        r#"{"kind":"invalid","message":"Use Ctrl, Alt, or Shift with this key."}"#
    );
    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.hotkeyFromKeyEvent({ code: 'Tab', ctrlKey: false, altKey: true, shiftKey: false })"
        ),
        r#"{"kind":"invalid","message":"That shortcut is reserved by Windows."}"#
    );
}

#[test]
fn hotkey_recorder_captures_printscreen_bare_and_modified() {
    let mut ctx = player_core_context();
    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.hotkeyFromKeyEvent({ code: 'PrintScreen', ctrlKey: false, altKey: false, shiftKey: false })"
        ),
        r#"{"kind":"captured","value":"PrintScreen"}"#
    );
    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.hotkeyFromKeyEvent({ code: 'PrintScreen', ctrlKey: true, altKey: false, shiftKey: false })"
        ),
        r#"{"kind":"captured","value":"Ctrl+PrintScreen"}"#
    );
    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.hotkeyFromKeyEvent({ code: 'PrintScreen', ctrlKey: false, altKey: true, shiftKey: false })"
        ),
        r#"{"kind":"captured","value":"Alt+PrintScreen"}"#
    );
}

#[test]
fn marker_styles_map_kinds_to_categories() {
    let mut ctx = player_core_context();
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.markerStyle('ChampionKill')"),
        r#"{"glyph":"✕","cls":"kill"}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.markerStyle('ChampionDeath')"),
        r#"{"glyph":"✕","cls":"death"}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.markerStyle('ChampionAssist')"),
        r#"{"glyph":"+","cls":"assist"}"#
    );
    // FirstBlood is an annotation that rides along with its ChampionKill, so it
    // lives in the spree category — sharing "kill" would double-count it in
    // digests and render it twice on the filtered review surfaces.
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.markerStyle('FirstBlood')"),
        r#"{"glyph":"★","cls":"spree"}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.markerStyle('Multikill')"),
        r#"{"glyph":"★","cls":"spree"}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.markerStyle('BaronKill')"),
        r#"{"glyph":"◆","cls":"objective"}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.markerStyle('TurretKilled')"),
        r#"{"glyph":"▣","cls":"structure"}"#
    );
    // Unknown / future kinds degrade to the info dot, never crash.
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.markerStyle('GameStart')"),
        r#"{"glyph":"•","cls":"info"}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.markerStyle('SomethingNew')"),
        r#"{"glyph":"•","cls":"info"}"#
    );
}

#[test]
fn marker_styles_accept_injected_plugin_presentation() {
    let mut ctx = player_core_context();
    ctx.eval(Source::from_bytes(
        r#"const P = {
          marker_kinds: {
            ChampionKill: { category: 'hero', glyph: '!' },
            DragonKill: { category: 'objective' }
          },
          marker_categories: {
            hero: { singular: 'hero play', plural: 'hero plays', glyph: '!' },
            objective: { singular: 'map objective', plural: 'map objectives', glyph: '◆' }
          }
        };"#,
    ))
    .expect("define presentation");

    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.markerStyle('ChampionKill', P)"),
        r#"{"glyph":"!","cls":"hero"}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.markerStyle('SomethingNew', P)"),
        r#"{"glyph":"•","cls":"info"}"#
    );
    assert_eq!(
        eval(
            &mut ctx,
            "PlayerCore.markerDigest([{ kind: 'ChampionKill' }, { kind: 'ChampionKill' }, { kind: 'DragonKill' }, { kind: 'ChampionAssist' }], P)"
        ),
        "2 hero plays · 1 map objective · 1 assist"
    );
}

#[test]
fn marker_presentation_ignores_inherited_keys_and_rejects_unsafe_images() {
    let mut ctx = player_core_context();
    ctx.eval(Source::from_bytes(
        r#"
        const inheritedKinds = Object.create({
          constructor: { category: 'kill', icon: 'assets/markers/kill.png' },
          SneakyKind: { category: 'kill' }
        });
        inheritedKinds.RealKind = { category: 'objective' };
        const inheritedCategories = Object.create({
          constructor: { singular: 'forged', plural: 'forged', glyph: '!' }
        });
        inheritedCategories.objective = { singular: 'objective', plural: 'objectives', glyph: 'O' };
        const INHERITED_PRESENTATION = {
          marker_kinds: inheritedKinds,
          marker_categories: inheritedCategories
        };
        const OWN_DANGEROUS_PRESENTATION = JSON.parse(
          '{"marker_kinds":{"__proto__":{"category":"kill"},"constructor":{"category":"kill"}}}'
        );
        "#,
    ))
    .expect("define inherited presentation maps");

    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.markerStyle('SneakyKind', INHERITED_PRESENTATION)"
        ),
        r#"{"glyph":"•","cls":"info"}"#
    );
    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.markerStyle('RealKind', INHERITED_PRESENTATION)"
        ),
        r#"{"glyph":"O","cls":"objective"}"#
    );
    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.markerKindConfig('constructor', INHERITED_PRESENTATION)"
        ),
        "{}"
    );
    assert_eq!(
        eval_json(
            &mut ctx,
            "[PlayerCore.markerKindConfig('__proto__', OWN_DANGEROUS_PRESENTATION), PlayerCore.markerKindConfig('constructor', OWN_DANGEROUS_PRESENTATION)]"
        ),
        "[{},{}]"
    );

    assert_eq!(
        eval(
            &mut ctx,
            "PlayerCore.safeMarkerImage('assets/markers/kill.png')"
        ),
        "assets/markers/kill.png"
    );
    assert_eq!(
        eval(
            &mut ctx,
            "PlayerCore.safeMarkerImage('data:image/png;base64,a2lsbA==')"
        ),
        "data:image/png;base64,a2lsbA=="
    );
    for unsafe_image in [
        "assets/markers/../kill.png",
        "https://tracker.example/marker.png",
        "assets/markers/kill.png\");color:red/*",
        "data:image/svg+xml,<svg onload=alert(1)>",
    ] {
        let encoded = serde_json::to_string(unsafe_image).expect("JSON image fixture");
        assert_eq!(
            eval(&mut ctx, &format!("PlayerCore.safeMarkerImage({encoded})")),
            ""
        );
    }
}

#[test]
fn ruler_marks_pick_nice_steps() {
    let mut ctx = player_core_context();
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.rulerMarks(22, 8).map(m => m.t)"),
        "[0,5,10,15,20]"
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.rulerMarks(22, 8).map(m => m.label)"),
        r#"["0:00","0:05","0:10","0:15","0:20"]"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.rulerMarks(1200, 8).map(m => m.t)"),
        "[0,300,600,900,1200]"
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.rulerMarks(6, 8).map(m => m.t)"),
        "[0,1,2,3,4,5,6]"
    );
    assert_eq!(eval_json(&mut ctx, "PlayerCore.rulerMarks(0, 8)"), "[]");
}

#[test]
fn ruler_marks_range_covers_a_zoomed_window() {
    let mut ctx = player_core_context();
    // A 20s window at 30s: 5s steps, only the marks that fall inside the window.
    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.rulerMarksRange(30, 20, 8).map(m => m.t)"
        ),
        "[30,35,40,45,50]"
    );
    // A start off the step grid rounds up to the first mark in view.
    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.rulerMarksRange(32, 20, 8).map(m => m.t)"
        ),
        "[35,40,45,50]"
    );
    // Starting at zero matches the whole-clip ruler.
    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.rulerMarksRange(0, 22, 8).map(m => m.t)"
        ),
        "[0,5,10,15,20]"
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.rulerMarksRange(0, 0, 8)"),
        "[]"
    );
}

#[test]
fn timeline_view_window_maps_positions_and_pointer() {
    let mut ctx = player_core_context();
    // percentForView locates a time within the visible window, unclamped so the
    // caller can clip content that lies outside 0–100%.
    assert_eq!(
        eval(&mut ctx, "PlayerCore.percentForView(15, 10, 20)"),
        "25"
    );
    assert_eq!(
        eval(&mut ctx, "PlayerCore.percentForView(5, 10, 20)"),
        "-25"
    );
    assert_eq!(eval(&mut ctx, "PlayerCore.percentForView(5, 0, 0)"), "0");

    // timelineTimeView maps pointer x into the window, then clamps to the clip.
    assert_eq!(
        eval(
            &mut ctx,
            "PlayerCore.timelineTimeView(150, 100, 200, 10, 20, 60)"
        ),
        "15"
    );
    assert_eq!(
        eval(
            &mut ctx,
            "PlayerCore.timelineTimeView(50, 100, 200, 10, 20, 60)"
        ),
        "10"
    );

    // clampView pins the window inside [0, duration]; span 0 means whole clip.
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.clampView(0, 0, 60)"),
        r#"{"start":0,"span":60}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.clampView(55, 20, 60)"),
        r#"{"start":40,"span":20}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.clampView(-5, 100, 60)"),
        r#"{"start":0,"span":60}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.clampView(0, 20, 0)"),
        r#"{"start":0,"span":0}"#
    );
}

#[test]
fn quick_trim_range_centers_on_playhead_and_clamps_to_clip() {
    let mut ctx = player_core_context();

    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.quickTrimRange(50, 120)"),
        r#"{"start":35,"end":65}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.quickTrimRange(4, 120)"),
        r#"{"start":0,"end":30}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.quickTrimRange(118, 120)"),
        r#"{"start":90,"end":120}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.quickTrimRange(8, 12)"),
        r#"{"start":0,"end":12}"#
    );
}

#[test]
fn trim_playback_stops_at_the_out_point_without_constraining_scrubbing() {
    let mut ctx = player_core_context();

    assert_eq!(
        eval(
            &mut ctx,
            "PlayerCore.trimPlaybackStopTime(true, false, 8, 8)"
        ),
        "8"
    );
    assert_eq!(
        eval(
            &mut ctx,
            "PlayerCore.trimPlaybackStopTime(true, false, 8.2, 8)"
        ),
        "8"
    );
    for expression in [
        "PlayerCore.trimPlaybackStopTime(true, false, 7.9, 8)",
        "PlayerCore.trimPlaybackStopTime(false, false, 8.2, 8)",
        "PlayerCore.trimPlaybackStopTime(true, true, 8.2, 8)",
        "PlayerCore.trimPlaybackStopTime(true, false, NaN, 8)",
    ] {
        assert_eq!(eval(&mut ctx, expression), "null");
    }
}

#[test]
fn zoom_view_keeps_the_anchor_time_fixed() {
    let mut ctx = player_core_context();
    // Zoom in to half span, anchored mid-window: the center time stays put.
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.zoomView(0, 60, 60, 0.5, 0.5, 1)"),
        r#"{"start":15,"span":30}"#
    );
    // Anchored at the left edge keeps the left time pinned.
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.zoomView(0, 60, 60, 0, 0.5, 1)"),
        r#"{"start":0,"span":30}"#
    );
    // Zooming back out never exceeds the clip and re-pins to the start.
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.zoomView(15, 30, 60, 0.5, 4, 1)"),
        r#"{"start":0,"span":60}"#
    );
    // The span floor caps how far in you can zoom.
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.zoomView(0, 60, 60, 0, 0.001, 1)"),
        r#"{"start":0,"span":1}"#
    );
    // Zero duration is a no-op window.
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.zoomView(0, 0, 0, 0.5, 0.5, 1)"),
        r#"{"start":0,"span":0}"#
    );
}

#[test]
fn pan_view_slides_and_clamps_the_window() {
    let mut ctx = player_core_context();
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.panView(10, 20, 100, 5)"),
        r#"{"start":15,"span":20}"#
    );
    // Panning past either end stops at the edge.
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.panView(10, 20, 100, -50)"),
        r#"{"start":0,"span":20}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.panView(90, 20, 100, 50)"),
        r#"{"start":80,"span":20}"#
    );
    // Zoomed out (span 0) is a no-op: the whole clip stays in view.
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.panView(0, 0, 100, 30)"),
        r#"{"start":0,"span":100}"#
    );
}

#[test]
fn set_view_edge_moves_one_boundary() {
    let mut ctx = player_core_context();
    // Drag the left edge in: the right edge stays, the span shrinks.
    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.setViewEdge(10, 40, 100, 'left', 20, 1)"
        ),
        r#"{"start":20,"span":30}"#
    );
    // Drag the right edge out: the left edge stays.
    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.setViewEdge(10, 40, 100, 'right', 80, 1)"
        ),
        r#"{"start":10,"span":70}"#
    );
    // The min span floors how far an edge can close.
    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.setViewEdge(10, 40, 100, 'left', 49.5, 1)"
        ),
        r#"{"start":49,"span":1}"#
    );
    // Edges clamp to the clip bounds.
    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.setViewEdge(10, 40, 100, 'right', 200, 1)"
        ),
        r#"{"start":10,"span":90}"#
    );
}

#[test]
fn view_for_range_frames_the_selection() {
    let mut ctx = player_core_context();
    // A mid-clip range gets symmetric padding.
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.viewForRange(20, 40, 100, 0.05, 1)"),
        r#"{"start":19,"span":22}"#
    );
    // A zero-width selection floors to the min span, recentered on the point.
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.viewForRange(50, 50, 100, 0.05, 1)"),
        r#"{"start":49.5,"span":1}"#
    );
    // The padded span never exceeds the clip.
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.viewForRange(0, 100, 100, 0.05, 1)"),
        r#"{"start":0,"span":100}"#
    );
    // Zero duration is a no-op window.
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.viewForRange(0, 10, 0, 0.05, 1)"),
        r#"{"start":0,"span":0}"#
    );
}

#[test]
fn follow_view_pages_and_centers() {
    let mut ctx = player_core_context();
    // Page: no change while the playhead is inside the window.
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.followView(10, 20, 100, 15, 'page')"),
        r#"{"start":10,"span":20}"#
    );
    // Page: re-page when the playhead leaves the right / left edge.
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.followView(10, 20, 100, 35, 'page')"),
        r#"{"start":33,"span":20}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.followView(50, 20, 100, 40, 'page')"),
        r#"{"start":38,"span":20}"#
    );
    // Smooth keeps the playhead centered.
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.followView(0, 20, 100, 50, 'smooth')"),
        r#"{"start":40,"span":20}"#
    );
    // None leaves the window alone.
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.followView(10, 20, 100, 99, 'none')"),
        r#"{"start":10,"span":20}"#
    );
    // Zoomed out never follows.
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.followView(0, 0, 100, 50, 'page')"),
        r#"{"start":0,"span":100}"#
    );
}

#[test]
fn snap_time_snaps_within_a_pixel_tolerance() {
    let mut ctx = player_core_context();
    // 8px / 100 px-per-s = 0.08s tolerance; 0.05s away snaps.
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.snapTime(10.05, [10, 20], 100, 8)"),
        r#"{"t":10,"snapped":true,"target":10}"#
    );
    // Beyond tolerance, the time passes through untouched.
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.snapTime(10.2, [10, 20], 100, 8)"),
        r#"{"t":10.2,"snapped":false,"target":null}"#
    );
    // Picks the closer of two candidates.
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.snapTime(10.6, [10, 11], 100, 80)"),
        r#"{"t":11,"snapped":true,"target":11}"#
    );
    // No candidates / non-positive scale never snap.
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.snapTime(5, [], 100, 8)"),
        r#"{"t":5,"snapped":false,"target":null}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.snapTime(5, [1, 2], 0, 8)"),
        r#"{"t":5,"snapped":false,"target":null}"#
    );
    // Tolerance scales with pixels-per-second (more zoom => tighter snap).
    assert_eq!(
        eval(&mut ctx, "PlayerCore.snapTime(10.1, [10], 50, 8).snapped"),
        "true"
    );
    assert_eq!(
        eval(&mut ctx, "PlayerCore.snapTime(10.1, [10], 200, 8).snapped"),
        "false"
    );
}

#[test]
fn snap_candidates_collect_and_exclude() {
    let mut ctx = player_core_context();
    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.snapCandidates(100, [{t_s:30}], 50, 10, 90)"
        ),
        "[0,10,30,50,90,100]"
    );
    // The moving element (here the in-edge and the playhead) is excluded.
    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.snapCandidates(100, [{t_s:30}], 50, 10, 90, ['in','playhead'])"
        ),
        "[0,30,90,100]"
    );
    // Duplicates collapse and the list stays sorted.
    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.snapCandidates(100, [{t_s:0},{t_s:100}], 0, 0, 100)"
        ),
        "[0,100]"
    );
}

#[test]
fn frame_step_falls_back_when_fps_unknown() {
    let mut ctx = player_core_context();
    assert_eq!(
        eval(&mut ctx, "Math.round(PlayerCore.frameStep(60) * 1e6)"),
        "16667"
    );
    // 0 / NaN / negative fps all fall back to the 1/30 default.
    assert_eq!(
        eval(&mut ctx, "Math.round(PlayerCore.frameStep(0) * 1e6)"),
        "33333"
    );
    assert_eq!(
        eval(&mut ctx, "Math.round(PlayerCore.frameStep(NaN) * 1e6)"),
        "33333"
    );
    assert_eq!(eval(&mut ctx, "PlayerCore.frameStep(-5, 0.5)"), "0.5");
}

#[test]
fn edit_points_are_sorted_unique_stops() {
    let mut ctx = player_core_context();
    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.editPoints([{t_s:30}], 10, 90, 100).map(p => p.t_s)"
        ),
        "[0,10,30,90,100]"
    );
}

#[test]
fn clip_titles_use_twelve_hour_time() {
    let mut ctx = player_core_context();
    assert_eq!(
        eval(&mut ctx, "PlayerCore.formatClipTitle(5, 11, 22, 25)"),
        "Jun 11 · 10:25 PM"
    );
    assert_eq!(
        eval(&mut ctx, "PlayerCore.formatClipTitle(0, 1, 0, 5)"),
        "Jan 1 · 12:05 AM"
    );
    assert_eq!(
        eval(&mut ctx, "PlayerCore.formatClipTitle(11, 31, 12, 0)"),
        "Dec 31 · 12:00 PM"
    );
}

#[test]
fn clip_kind_distinguishes_trims_from_replays() {
    let mut ctx = player_core_context();
    assert_eq!(
        eval(&mut ctx, "PlayerCore.clipKind('clip_20260612_212006.mp4')"),
        "replay"
    );
    assert_eq!(
        eval(
            &mut ctx,
            "PlayerCore.clipKind('clip_20260612_212006_trim_001000_002000.mp4')"
        ),
        "trim"
    );
    assert_eq!(
        eval(&mut ctx, "PlayerCore.clipKind('session_1781377615.mp4')"),
        "session"
    );
    assert_eq!(eval(&mut ctx, "PlayerCore.clipKind('')"), "replay");
}

#[test]
fn clip_kind_prefers_backend_kind_for_renamed_clips() {
    let mut ctx = player_core_context();
    assert_eq!(
        eval(
            &mut ctx,
            "PlayerCore.clipKind({ name: 'Ranked win.mp4', kind: 'session' })"
        ),
        "session"
    );
    assert_eq!(
        eval(
            &mut ctx,
            "PlayerCore.clipKind({ name: 'Ranked win.mp4', kind: 'trim' })"
        ),
        "trim"
    );
}

#[test]
fn gallery_card_preview_prefers_custom_title() {
    let mut ctx = player_core_context();
    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.galleryCardPreview({ name: 'session_123.mp4', title: 'Ranked win vs Lux', markers: {} }, 'session', 'Jul 2 · 7:30 PM')"
        ),
        r#"{"title":"Ranked win vs Lux","titleSource":"clip","summary":""}"#
    );
}

#[test]
fn marker_digest_collapses_categories() {
    let mut ctx = player_core_context();
    ctx.eval(Source::from_bytes(
        "const D = [{ kind: 'ChampionKill' }, { kind: 'ChampionKill' }, { kind: 'DragonKill' }];",
    ))
    .expect("define markers");
    assert_eq!(
        eval(&mut ctx, "PlayerCore.markerDigest(D)"),
        "2 kills · 1 objective"
    );
    assert_eq!(
        eval(&mut ctx, "PlayerCore.markerDigest([{ kind: 'Multikill' }])"),
        "1 spree"
    );
    assert_eq!(
        eval(
            &mut ctx,
            "PlayerCore.markerDigest([{ kind: 'ChampionDeath' }, { kind: 'ChampionDeath' }])"
        ),
        "2 deaths"
    );
    assert_eq!(eval(&mut ctx, "PlayerCore.markerDigest([])"), "");
}

#[test]
fn player_summary_label_formats_champion_kda() {
    let mut ctx = player_core_context();
    assert_eq!(
        eval(
            &mut ctx,
            "PlayerCore.playerSummaryLabel({ champion_name: 'Nautilus', kills: 3, deaths: 4, assists: 23 })"
        ),
        "Nautilus | 3/4/23"
    );
    assert_eq!(
        eval(
            &mut ctx,
            "PlayerCore.playerSummaryLabel({ champion_name: '  Ahri ', kills: '2', deaths: null, assists: -1 })"
        ),
        "Ahri | 2/0/0"
    );
    assert_eq!(eval(&mut ctx, "PlayerCore.playerSummaryLabel(null)"), "");
    assert_eq!(
        eval(
            &mut ctx,
            "PlayerCore.playerSummaryLabel({ champion_name: '   ', kills: 1, deaths: 2, assists: 3 })"
        ),
        ""
    );
}

#[test]
fn player_summary_fields_format_declarative_metadata() {
    let mut ctx = player_core_context();
    ctx.eval(Source::from_bytes(
        r#"
        const SUMMARY = { champion_name: 'Nautilus', kills: 3, deaths: 4, assists: 23 };
        const FIELDS = [
          {
            type: 'portrait',
            source: 'player_summary.champion_name',
            label: 'Champion',
            asset_template: 'assets/champions/{assetKey}.png'
          },
          { type: 'champion', source: 'player_summary.champion_name', label: 'Champion' },
          { type: 'kda', label: 'K/D/A' },
          { type: 'stat', source: 'player_summary.kills', label: 'Kills' },
          { type: 'stat', source: 'player_summary.creep_score', label: 'CS' }
        ];
        "#,
    ))
    .expect("define summary fields");

    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.playerSummaryFields(SUMMARY, FIELDS)"),
        r#"[{"type":"portrait","label":"Champion","value":"Nautilus","assetKey":"nautilus","asset":"assets/champions/nautilus.png"},{"type":"champion","label":"Champion","value":"Nautilus"},{"type":"kda","label":"K/D/A","value":"3/4/23"},{"type":"stat","label":"Kills","value":"3"}]"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.playerSummaryFields(null, FIELDS)"),
        "[]"
    );
}

#[test]
fn player_summary_fields_resolve_data_dragon_portraits() {
    let mut ctx = player_core_context();
    ctx.eval(Source::from_bytes(
        r#"
        const FIELDS = [{
          type: 'portrait',
          source: 'player_summary.champion_name',
          label: 'Champion',
          asset_provider: 'riot_data_dragon_champion_square',
          asset_key_format: 'data_dragon_champion',
          asset_aliases: { wukong: 'MonkeyKing' }
        }];
        const OPTIONS = { data_dragon: { version: '16.13.1' } };
        "#,
    ))
    .expect("define data dragon fields");

    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.playerSummaryFields({ champion_name: 'Nautilus', kills: 3, deaths: 4, assists: 23 }, FIELDS, OPTIONS)"
        ),
        r#"[{"type":"portrait","label":"Champion","value":"Nautilus","assetKey":"Nautilus","asset":"https://ddragon.leagueoflegends.com/cdn/16.13.1/img/champion/Nautilus.png"}]"#
    );
    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.playerSummaryFields({ champion_name: 'Wukong', kills: 1, deaths: 0, assists: 2 }, FIELDS, OPTIONS)"
        ),
        r#"[{"type":"portrait","label":"Champion","value":"Wukong","assetKey":"MonkeyKing","asset":"https://ddragon.leagueoflegends.com/cdn/16.13.1/img/champion/MonkeyKing.png"}]"#
    );
}

#[test]
fn player_summary_fields_format_rich_league_metadata() {
    let mut ctx = player_core_context();
    ctx.eval(Source::from_bytes(
        r#"
        const SUMMARY = {
          champion_name: "Vel'Koz",
          kills: 11,
          deaths: 19,
          assists: 34,
          summoner_spells: [
            { name: 'Ignite', asset_key: 'SummonerDot' },
            { name: 'Flash', asset_key: 'SummonerFlash' }
          ],
          items: [
            { id: 1056, name: "Doran's Ring" },
            { id: 3020, name: "Sorcerer's Shoes" },
            { id: 6655, name: "Luden's Companion" },
            { id: 3089, name: "Rabadon's Deathcap" }
          ]
        };
        const FIELDS = [
          {
            type: 'summoner_spells',
            source: 'player_summary.summoner_spells',
            label: 'Summoner spells',
            asset_provider: 'riot_data_dragon_summoner_spell'
          },
          { type: 'kda', secondary: 'kda_ratio' },
          {
            type: 'item_build',
            source: 'player_summary.items',
            label: 'Build',
            asset_provider: 'riot_data_dragon_item'
          }
        ];
        const OPTIONS = { data_dragon: { version: '16.13.1' } };
        "#,
    ))
    .expect("define rich summary fields");

    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.playerSummaryFields(SUMMARY, FIELDS, OPTIONS)"
        ),
        r#"[{"type":"summoner_spells","label":"Summoner spells","items":[{"value":"Ignite","assetKey":"SummonerDot","asset":"https://ddragon.leagueoflegends.com/cdn/16.13.1/img/spell/SummonerDot.png"},{"value":"Flash","assetKey":"SummonerFlash","asset":"https://ddragon.leagueoflegends.com/cdn/16.13.1/img/spell/SummonerFlash.png"}]},{"type":"kda","label":"","value":"11/19/34","secondary":"2.37 KDA"},{"type":"item_build","label":"Build","items":[{"value":"Doran's Ring","assetKey":"1056","asset":"https://ddragon.leagueoflegends.com/cdn/16.13.1/img/item/1056.png"},{"value":"Sorcerer's Shoes","assetKey":"3020","asset":"https://ddragon.leagueoflegends.com/cdn/16.13.1/img/item/3020.png"},{"value":"Luden's Companion","assetKey":"6655","asset":"https://ddragon.leagueoflegends.com/cdn/16.13.1/img/item/6655.png"},{"value":"Rabadon's Deathcap","assetKey":"3089","asset":"https://ddragon.leagueoflegends.com/cdn/16.13.1/img/item/3089.png"}]}]"#
    );
}

#[test]
fn player_summary_fields_maps_hexflash_sidecar_to_flash_asset() {
    let mut ctx = player_core_context();
    ctx.eval(Source::from_bytes(
        r#"
        const SUMMARY = {
          summoner_spells: [
            { name: 'Ignite', asset_key: 'SummonerDot' },
            {
              name: 'Hexflash',
              asset_key: 'SummonerFlashPerksHextechFlashtraptionV2'
            }
          ]
        };
        const FIELDS = [
          {
            type: 'summoner_spells',
            source: 'player_summary.summoner_spells',
            label: 'Summoner spells',
            asset_provider: 'riot_data_dragon_summoner_spell'
          }
        ];
        const OPTIONS = { data_dragon: { version: '16.13.1' } };
        "#,
    ))
    .expect("define hexflash summary fields");

    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.playerSummaryFields(SUMMARY, FIELDS, OPTIONS)"
        ),
        r#"[{"type":"summoner_spells","label":"Summoner spells","items":[{"value":"Ignite","assetKey":"SummonerDot","asset":"https://ddragon.leagueoflegends.com/cdn/16.13.1/img/spell/SummonerDot.png"},{"value":"Hexflash","assetKey":"SummonerFlash","asset":"https://ddragon.leagueoflegends.com/cdn/16.13.1/img/spell/SummonerFlash.png"}]}]"#
    );
}

#[test]
fn gallery_card_preview_uses_declarative_title_and_icon() {
    let mut ctx = player_core_context();
    ctx.eval(Source::from_bytes(
        r#"
        const CARD_CLIP = {
          markers: {
            player_summary: {
              champion_name: "Vel'Koz",
              kills: 11,
              deaths: 19,
              assists: 34,
              creep_score: 204,
              game_time_s: 1800
            }
          }
        };
        const CARD_PRESENTATION = {
          data_dragon: { version: '16.13.1' },
          gallery: {
            summary: 'player_summary_kda',
            card: {
              title: 'summary_for_full_session',
              title_format: {
                type: 'player_summary_stats',
                separator: ' | ',
                stats: [
                  { type: 'kda' },
                  { type: 'cs_per_min', label: 'CS/min' }
                ]
              },
              icon: {
                type: 'portrait',
                source: 'player_summary.champion_name',
                label: 'Champion',
                asset_provider: 'riot_data_dragon_champion_square',
                asset_key_format: 'data_dragon_champion',
                asset_aliases: { "vel'koz": 'Velkoz' }
              }
            }
          }
        };
        "#,
    ))
    .expect("define gallery card preview");

    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.galleryCardPreview(CARD_CLIP, 'session', 'Jun 28 · 12:15 PM', CARD_PRESENTATION, { data_dragon: CARD_PRESENTATION.data_dragon })"
        ),
        r#"{"title":"11/19/34 | 6.8 CS/min","titleSource":"summary","summary":"Vel'Koz | 11/19/34","icon":{"type":"portrait","url":"https://ddragon.leagueoflegends.com/cdn/16.13.1/img/champion/Velkoz.png","label":"Vel'Koz"}}"#
    );
    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.galleryCardPreview(CARD_CLIP, 'replay', 'Jun 28 · 12:15 PM', CARD_PRESENTATION, { data_dragon: CARD_PRESENTATION.data_dragon })"
        ),
        r#"{"title":"Jun 28 · 12:15 PM","titleSource":"clip","summary":"Vel'Koz | 11/19/34","icon":{"type":"portrait","url":"https://ddragon.leagueoflegends.com/cdn/16.13.1/img/champion/Velkoz.png","label":"Vel'Koz"}}"#
    );
    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.galleryCardPreview({ markers: { player_summary: { champion_name: \"Vel'Koz\", kills: 11, deaths: 19, assists: 34 } } }, 'session', 'Jun 28 · 12:15 PM', CARD_PRESENTATION, { data_dragon: CARD_PRESENTATION.data_dragon })"
        ),
        r#"{"title":"11/19/34","titleSource":"summary","summary":"Vel'Koz | 11/19/34","icon":{"type":"portrait","url":"https://ddragon.leagueoflegends.com/cdn/16.13.1/img/champion/Velkoz.png","label":"Vel'Koz"}}"#
    );
}

#[test]
fn gallery_card_preview_accepts_plugin_asset_icons() {
    let mut ctx = player_core_context();
    ctx.eval(Source::from_bytes(
        r#"
        const ASSET_PRESENTATION = {
          gallery: {
            card: {
              title: 'clip',
              icon: {
                type: 'asset',
                src: 'data:image/png;base64,plugin-logo',
                label: 'Arena logo'
              }
            }
          }
        };
        "#,
    ))
    .expect("define gallery card asset icon");

    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.galleryCardPreview({ name: 'Named clip', markers: {} }, 'trim', 'Custom title', ASSET_PRESENTATION)"
        ),
        r#"{"title":"Named clip","titleSource":"clip","summary":"","icon":{"type":"asset","url":"data:image/png;base64,plugin-logo","label":"Arena logo"}}"#
    );
}

#[test]
fn game_event_rail_item_formats_duel_with_data_dragon_portraits() {
    let mut ctx = player_core_context();
    ctx.eval(Source::from_bytes(
        r#"
        const RAIL_SUMMARY = {
          player_name: 'dain',
          team: 'ORDER',
          participants: [
            { player_name: 'dain', champion_name: 'Nautilus', team: 'ORDER' },
            { player_name: 'Soupmaster', champion_name: 'Ahri', team: 'CHAOS' }
          ]
        };
        const RAIL_PRESENTATION = {
          marker_kinds: {
            ChampionKill: {
              category: 'kill',
              icon: 'data:image/png;base64,kill-icon',
              rail: { layout: 'duel', allegiance: 'friendly' }
            }
          }
        };
        const RAIL_OPTIONS = { data_dragon: { version: '16.13.1' } };
        "#,
    ))
    .expect("define rail inputs");

    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.gameEventRailItem({ kind: 'ChampionKill', actor: 'dain', victim: 'Soupmaster', t_s: 162 }, RAIL_SUMMARY, RAIL_PRESENTATION, RAIL_OPTIONS)"
        ),
        r#"{"layout":"duel","kind":"ChampionKill","category":"kill","allegiance":"friendly","label":"Champion Kill","text":"Champion Kill · dain","icon":"data:image/png;base64,kill-icon","actor":{"name":"dain","champion":"Nautilus","team":"ORDER","assetKey":"Nautilus","asset":"https://ddragon.leagueoflegends.com/cdn/16.13.1/img/champion/Nautilus.png","initials":"DA","local":true},"victim":{"name":"Soupmaster","champion":"Ahri","team":"CHAOS","assetKey":"Ahri","asset":"https://ddragon.leagueoflegends.com/cdn/16.13.1/img/champion/Ahri.png","initials":"SO","local":false}}"#
    );
}

#[test]
fn game_event_rail_item_marks_deaths_enemy() {
    let mut ctx = player_core_context();
    ctx.eval(Source::from_bytes(
        r#"
        const DEATH_SUMMARY = {
          player_name: 'dain',
          team: 'ORDER',
          participants: [
            { player_name: 'dain', champion_name: 'Nautilus', team: 'ORDER' },
            { player_name: 'Kcrystal', champion_name: 'Zed', team: 'CHAOS' }
          ]
        };
        const DEATH_PRESENTATION = {
          marker_kinds: {
            ChampionDeath: {
              category: 'death',
              icon: 'data:image/png;base64,death-icon',
              rail: { layout: 'duel', allegiance: 'enemy' }
            }
          }
        };
        const DEATH_OPTIONS = { data_dragon: { version: '16.13.1' } };
        "#,
    ))
    .expect("define death rail inputs");

    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.gameEventRailItem({ kind: 'ChampionDeath', actor: 'Kcrystal', victim: 'dain', t_s: 160 }, DEATH_SUMMARY, DEATH_PRESENTATION, DEATH_OPTIONS)"
        ),
        r#"{"layout":"duel","kind":"ChampionDeath","category":"death","allegiance":"enemy","label":"Champion Death","text":"Champion Death · Kcrystal","icon":"data:image/png;base64,death-icon","actor":{"name":"Kcrystal","champion":"Zed","team":"CHAOS","assetKey":"Zed","asset":"https://ddragon.leagueoflegends.com/cdn/16.13.1/img/champion/Zed.png","initials":"KC","local":false},"victim":{"name":"dain","champion":"Nautilus","team":"ORDER","assetKey":"Nautilus","asset":"https://ddragon.leagueoflegends.com/cdn/16.13.1/img/champion/Nautilus.png","initials":"DA","local":true}}"#
    );
}

#[test]
fn game_event_rail_item_prefers_event_rail_icons_over_timeline_marker_icons() {
    let mut ctx = player_core_context();
    ctx.eval(Source::from_bytes(
        r#"
        const RAIL_ICON_SUMMARY = {
          player_name: 'dain',
          team: 'ORDER',
          participants: [
            { player_name: 'dain', champion_name: 'Nautilus', team: 'ORDER' },
            { player_name: 'Kcrystal', champion_name: 'Zed', team: 'CHAOS' }
          ]
        };
        const RAIL_ICON_PRESENTATION = {
          marker_kinds: {
            ChampionDeath: {
              category: 'death',
              icon: 'data:image/png;base64,timeline-death',
              rail: { layout: 'duel', allegiance: 'enemy' }
            }
          },
          event_rail: {
            icons: {
              ChampionDeath: 'data:image/png;base64,rail-death'
            }
          }
        };
        "#,
    ))
    .expect("define event rail icon inputs");

    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.gameEventRailItem({ kind: 'ChampionDeath', actor: 'Kcrystal', victim: 'dain', t_s: 160 }, RAIL_ICON_SUMMARY, RAIL_ICON_PRESENTATION, {})"
        ),
        r#"{"layout":"duel","kind":"ChampionDeath","category":"death","allegiance":"enemy","label":"Champion Death","text":"Champion Death · Kcrystal","icon":"data:image/png;base64,rail-death","actor":{"name":"Kcrystal","champion":"Zed","team":"CHAOS","assetKey":"Zed","initials":"KC","local":false},"victim":{"name":"dain","champion":"Nautilus","team":"ORDER","assetKey":"Nautilus","initials":"DA","local":true}}"#
    );
}

#[test]
fn game_event_rail_item_uses_manifest_rail_layout_and_allegiance() {
    let mut ctx = player_core_context();
    ctx.eval(Source::from_bytes(
        r#"
        const CUSTOM_RAIL_SUMMARY = {
          player_name: 'dain',
          team: 'ORDER',
          participants: [
            { player_name: 'dain', champion_name: 'Nautilus', team: 'ORDER' },
            { player_name: 'Kcrystal', champion_name: 'Zed', team: 'CHAOS' }
          ]
        };
        const CUSTOM_RAIL_PRESENTATION = {
          marker_kinds: {
            CustomElimination: {
              category: 'kill',
              icon: 'data:image/png;base64,custom-icon',
              rail: {
                layout: 'duel',
                allegiance: 'actor_team'
              }
            }
          }
        };
        "#,
    ))
    .expect("define custom rail inputs");

    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.gameEventRailItem({ kind: 'CustomElimination', actor: 'Kcrystal', victim: 'dain', t_s: 160 }, CUSTOM_RAIL_SUMMARY, CUSTOM_RAIL_PRESENTATION, {})"
        ),
        r#"{"layout":"duel","kind":"CustomElimination","category":"kill","allegiance":"enemy","label":"Custom Elimination","text":"Custom Elimination · Kcrystal","icon":"data:image/png;base64,custom-icon","actor":{"name":"Kcrystal","champion":"Zed","team":"CHAOS","assetKey":"Zed","initials":"KC","local":false},"victim":{"name":"dain","champion":"Nautilus","team":"ORDER","assetKey":"Nautilus","initials":"DA","local":true}}"#
    );
}

#[test]
fn game_event_rail_item_formats_actor_objectives_with_portrait_and_icon() {
    let mut ctx = player_core_context();
    ctx.eval(Source::from_bytes(
        r#"
        const OBJECTIVE_SUMMARY = {
          player_name: 'dain',
          team: 'ORDER',
          participants: [
            { player_name: 'dain', champion_name: 'Nautilus', team: 'ORDER' },
            { player_name: 'Jinmee', champion_name: 'Ezreal', team: 'ORDER' }
          ]
        };
        const OBJECTIVE_PRESENTATION = {
          marker_kinds: {
            TurretKilled: {
              category: 'structure',
              icon: 'data:image/png;base64,turret-icon',
              rail: { layout: 'actor_event', allegiance: 'actor_team' }
            }
          }
        };
        const OBJECTIVE_OPTIONS = { data_dragon: { version: '16.13.1' } };
        "#,
    ))
    .expect("define objective rail inputs");

    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.gameEventRailItem({ kind: 'TurretKilled', actor: 'Jinmee', t_s: 445 }, OBJECTIVE_SUMMARY, OBJECTIVE_PRESENTATION, OBJECTIVE_OPTIONS)"
        ),
        r#"{"layout":"actor_event","kind":"TurretKilled","category":"structure","allegiance":"friendly","label":"Turret Killed","text":"Turret Killed · Jinmee","icon":"data:image/png;base64,turret-icon","actor":{"name":"Jinmee","champion":"Ezreal","team":"ORDER","assetKey":"Ezreal","asset":"https://ddragon.leagueoflegends.com/cdn/16.13.1/img/champion/Ezreal.png","initials":"JI","local":false}}"#
    );
}

#[test]
fn game_event_rail_item_formats_herald_with_event_rail_icon() {
    let mut ctx = player_core_context();
    ctx.eval(Source::from_bytes(
        r#"
        const HERALD_SUMMARY = {
          player_name: 'dain',
          team: 'ORDER',
          participants: [
            { player_name: 'dain', champion_name: 'Nautilus', team: 'ORDER' },
            { player_name: 'Jinmee', champion_name: 'Ezreal', team: 'ORDER' }
          ]
        };
        const HERALD_PRESENTATION = {
          marker_kinds: {
            HeraldKill: {
              category: 'objective',
              rail: { layout: 'actor_event', allegiance: 'actor_team' }
            }
          },
          event_rail: {
            icons: {
              HeraldKill: 'data:image/png;base64,herald-icon'
            }
          }
        };
        const HERALD_OPTIONS = { data_dragon: { version: '16.13.1' } };
        "#,
    ))
    .expect("define herald rail inputs");

    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.gameEventRailItem({ kind: 'HeraldKill', actor: 'Jinmee', t_s: 812 }, HERALD_SUMMARY, HERALD_PRESENTATION, HERALD_OPTIONS)"
        ),
        r#"{"layout":"actor_event","kind":"HeraldKill","category":"objective","allegiance":"friendly","label":"Herald Kill","text":"Herald Kill · Jinmee","icon":"data:image/png;base64,herald-icon","actor":{"name":"Jinmee","champion":"Ezreal","team":"ORDER","assetKey":"Ezreal","asset":"https://ddragon.leagueoflegends.com/cdn/16.13.1/img/champion/Ezreal.png","initials":"JI","local":false}}"#
    );
}

#[test]
fn game_event_rail_item_keeps_objective_icon_when_actor_is_not_a_participant() {
    let mut ctx = player_core_context();
    ctx.eval(Source::from_bytes(
        r#"
        const MINION_OBJECTIVE_SUMMARY = {
          player_name: 'dain',
          team: 'ORDER',
          participants: [
            { player_name: 'dain', champion_name: 'Nautilus', team: 'ORDER' }
          ]
        };
        const MINION_OBJECTIVE_PRESENTATION = {
          marker_kinds: {
            TurretKilled: {
              category: 'structure',
              icon: 'data:image/png;base64,turret-icon',
              rail: { layout: 'actor_event', allegiance: 'actor_team' }
            }
          },
          event_rail: {
            actor_icons: [
              { prefix: 'Minion_T100', name: 'Minion', asset: 'data:image/png;base64,minion-blue' },
              { prefix: 'Minion_T200', name: 'Minion', asset: 'data:image/png;base64,minion-red' }
            ]
          }
        };
        "#,
    ))
    .expect("define minion objective rail inputs");

    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.gameEventRailItem({ kind: 'TurretKilled', actor: 'Minion_T200LS29N0', t_s: 931 }, MINION_OBJECTIVE_SUMMARY, MINION_OBJECTIVE_PRESENTATION, {})"
        ),
        r#"{"layout":"actor_event","kind":"TurretKilled","category":"structure","allegiance":"neutral","label":"Turret Killed","text":"Turret Killed · Minion_T200LS29N0","icon":"data:image/png;base64,turret-icon","actor":{"name":"Minion","asset":"data:image/png;base64,minion-red","initials":"MI","local":false}}"#
    );
}

#[test]
fn game_event_rail_item_falls_back_without_participants() {
    let mut ctx = player_core_context();
    ctx.eval(Source::from_bytes(
        r#"
        const FALLBACK_PRESENTATION = {
          marker_kinds: {
            ChampionKill: {
              category: 'kill',
              icon: 'data:image/png;base64,kill-icon',
              rail: { layout: 'duel', allegiance: 'friendly' }
            }
          }
        };
        "#,
    ))
    .expect("define fallback rail inputs");

    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.gameEventRailItem({ kind: 'ChampionKill', actor: 'dain', victim: 'Soupmaster', t_s: 162 }, null, FALLBACK_PRESENTATION, {})"
        ),
        r#"{"layout":"text","kind":"ChampionKill","category":"kill","allegiance":"friendly","label":"Champion Kill","text":"Champion Kill · dain","icon":"data:image/png;base64,kill-icon"}"#
    );
}

#[test]
fn session_groups_bucket_and_sort_by_newest() {
    let mut ctx = player_core_context();
    ctx.eval(Source::from_bytes(
        r#"const C = [
            { path: "c.mp4", session: "2026-06-12 14-30", modified_unix: 100 },
            { path: "a.mp4", session: null, modified_unix: 50 },
            { path: "d.mp4", session: "2026-06-12 14-30", modified_unix: 200 },
            { path: "b.mp4", session: "2026-06-11 09-00", modified_unix: 150 },
        ];"#,
    ))
    .expect("define clips");
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.sessionGroups(C).map(g => g.label)"),
        r#"["2026-06-12 14-30","2026-06-11 09-00","Earlier"]"#
    );
    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.sessionGroups(C)[0].clips.map(c => c.path)"
        ),
        r#"["d.mp4","c.mp4"]"#
    );
    assert_eq!(eval_json(&mut ctx, "PlayerCore.sessionGroups([])"), "[]");
}

#[test]
fn fullscreen_key_maps_to_review_fullscreen() {
    let mut ctx = player_core_context();
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.keyIntent('KeyF', false)"),
        r#"{"kind":"toggle-fullscreen"}"#
    );
}

#[test]
fn overlay_pins_while_paused_and_fades_when_idle() {
    let mut ctx = player_core_context();
    // Paused: always visible, no matter how stale the activity.
    assert_eq!(
        eval(&mut ctx, "PlayerCore.overlayVisible(true, 999999)"),
        "true"
    );
    // Playing with fresh pointer activity: visible.
    assert_eq!(
        eval(&mut ctx, "PlayerCore.overlayVisible(false, 500)"),
        "true"
    );
    // Playing and idle past the threshold: hidden.
    assert_eq!(
        eval(&mut ctx, "PlayerCore.overlayVisible(false, 2500)"),
        "false"
    );
    assert_eq!(eval(&mut ctx, "PlayerCore.OVERLAY_HIDE_MS"), "2000");
}

#[test]
fn shared_constants_are_exposed() {
    let mut ctx = player_core_context();
    assert_eq!(eval(&mut ctx, "PlayerCore.MIN_TRIM_GAP_S"), "0.1");
    assert_eq!(eval(&mut ctx, "PlayerCore.MARKER_EPSILON_S"), "0.05");
    assert_eq!(eval(&mut ctx, "PlayerCore.MIN_VIEW_SPAN_S"), "1");
    assert_eq!(eval(&mut ctx, "PlayerCore.DEFAULT_FOLLOW_MODE"), "page");
    assert_eq!(eval(&mut ctx, "PlayerCore.SNAP_THRESHOLD_PX"), "8");
    assert_eq!(
        eval(&mut ctx, "Math.round(PlayerCore.DEFAULT_FINE_STEP_S * 1e6)"),
        "16667"
    );
}

#[test]
fn display_bounds_cover_the_virtual_desktop() {
    let mut ctx = player_core_context();
    ctx.eval(Source::from_bytes(
        r#"const DISPLAYS = [
            { id: 'DISPLAY1', name: 'Display 1', x: -1280, y: 120, width: 1280, height: 720, is_primary: false },
            { id: 'DISPLAY2', name: 'Display 2', x: 0, y: 0, width: 1920, height: 1080, is_primary: true },
        ];"#,
    ))
    .expect("define displays");

    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.displayBounds(DISPLAYS)"),
        r#"{"x":-1280,"y":0,"width":3200,"height":1080}"#
    );
}

#[test]
fn display_map_layout_fits_displays_into_viewport() {
    let mut ctx = player_core_context();
    ctx.eval(Source::from_bytes(
        r#"const DISPLAYS = [
            { id: 'DISPLAY1', name: 'Display 1', x: 0, y: 0, width: 1920, height: 1080, is_primary: true },
            { id: 'DISPLAY2', name: 'Display 2', x: 1920, y: 0, width: 2560, height: 1440, is_primary: false },
        ];
        const LAYOUT = PlayerCore.displayMapLayout(DISPLAYS, 500, 250, 10);"#,
    ))
    .expect("define layout");

    assert_eq!(
        eval_json(&mut ctx, "LAYOUT.bounds"),
        r#"{"x":0,"y":0,"width":4480,"height":1440}"#
    );
    assert_eq!(eval(&mut ctx, "Math.round(LAYOUT.scale * 1000)"), "107");
    assert_eq!(eval(&mut ctx, "Math.round(LAYOUT.displays[1].left)"), "216");
    assert_eq!(
        eval(&mut ctx, "Math.round(LAYOUT.displays[1].width)"),
        "274"
    );
}

#[test]
fn display_map_height_scales_with_virtual_desktop_shape() {
    let mut ctx = player_core_context();
    ctx.eval(Source::from_bytes(
        r#"const WIDE_STACK = [
            { id: 'DISPLAY2', name: 'Display 2', x: 0, y: 0, width: 5120, height: 1440, is_primary: true },
            { id: 'DISPLAY1', name: 'Display 1', x: 1920, y: 1440, width: 1920, height: 1080, is_primary: false },
        ];
        const SINGLE = [
            { id: 'DISPLAY1', name: 'Display 1', x: 0, y: 0, width: 1920, height: 1080, is_primary: true },
        ];"#,
    ))
    .expect("define displays");

    assert_eq!(
        eval(
            &mut ctx,
            "Math.round(PlayerCore.displayMapHeight(SINGLE, 620))"
        ),
        "358"
    );
    assert_eq!(
        eval(
            &mut ctx,
            "Math.round(PlayerCore.displayMapHeight(WIDE_STACK, 620))"
        ),
        "315"
    );
}

#[test]
fn audio_preview_queue_serializes_and_coalesces_to_latest_request() {
    let mut ctx = player_core_context();
    assert_eq!(
        eval_json(
            &mut ctx,
            r#"
            (() => {
              const one = { clipPath: 'a.mp4', trackIds: ['mic'], selectionKey: 'one', sourceGeneration: 4 };
              const two = { clipPath: 'a.mp4', trackIds: ['game'], selectionKey: 'two', sourceGeneration: 4 };
              const three = { clipPath: 'b.mp4', trackIds: ['output'], selectionKey: 'three', sourceGeneration: 5 };
              let state = PlayerCore.emptyAudioPreviewQueue();
              const first = PlayerCore.queueAudioPreviewRequest(state, one);
              state = first.state;
              state = PlayerCore.queueAudioPreviewRequest(state, two).state;
              state = PlayerCore.queueAudioPreviewRequest(state, three).state;
              const finished = PlayerCore.finishAudioPreviewRequest(state, first.start.revision, true);
              return {
                firstStart: first.start.selectionKey,
                firstApply: finished.apply,
                nextStart: finished.start.selectionKey,
                active: finished.state.active.selectionKey,
              };
            })()
            "#,
        ),
        r#"{"firstStart":"one","firstApply":null,"nextStart":"three","active":"three"}"#
    );
}

#[test]
fn audio_preview_queue_applies_only_current_success_and_cancel_keeps_worker_slot() {
    let mut ctx = player_core_context();
    assert_eq!(
        eval_json(
            &mut ctx,
            r#"
            (() => {
              const request = { clipPath: 'a.mp4', trackIds: ['mic'], selectionKey: 'mic', sourceGeneration: 2 };
              const queued = PlayerCore.queueAudioPreviewRequest(PlayerCore.emptyAudioPreviewQueue(), request);
              const applied = PlayerCore.finishAudioPreviewRequest(queued.state, queued.start.revision, true);
              const second = PlayerCore.queueAudioPreviewRequest(PlayerCore.emptyAudioPreviewQueue(), request);
              const cancelled = PlayerCore.cancelAudioPreviewRequest(second.state);
              const ignored = PlayerCore.finishAudioPreviewRequest(cancelled, second.start.revision, true);
              return {
                apply: applied.apply.selectionKey,
                activeAfterApply: applied.state.active,
                cancelledStillActive: cancelled.active.selectionKey,
                applyAfterCancel: ignored.apply,
              };
            })()
            "#,
        ),
        r#"{"apply":"mic","activeAfterApply":null,"cancelledStillActive":"mic","applyAfterCancel":null}"#
    );
}

#[test]
fn audio_sidecar_sync_keeps_requested_rate_and_hard_seeks_discontinuities() {
    let mut ctx = player_core_context();
    assert_eq!(
        eval_json(
            &mut ctx,
            r#"
            (() => ({
              forced: PlayerCore.audioSidecarSyncDecision(
                { currentTime: 12.5, playbackRate: 1.5, paused: false, ended: false },
                { currentTime: 12.49 },
                { forceSeek: true },
              ),
              inSync: PlayerCore.audioSidecarSyncDecision(
                { currentTime: 10, playbackRate: 1, paused: false, ended: false },
                { currentTime: 9.99 },
              ),
              behind: PlayerCore.audioSidecarSyncDecision(
                { currentTime: 10, playbackRate: 1, paused: false, ended: false },
                { currentTime: 9.9 },
              ),
              ahead: PlayerCore.audioSidecarSyncDecision(
                { currentTime: 10, playbackRate: 1, paused: false, ended: false },
                { currentTime: 10.1 },
              ),
              discontinuity: PlayerCore.audioSidecarSyncDecision(
                { currentTime: 10, playbackRate: 1, paused: false, ended: false },
                { currentTime: 9.49 },
              ),
              pausedDrift: PlayerCore.audioSidecarSyncDecision(
                { currentTime: 10, playbackRate: 1, paused: true, ended: false },
                { currentTime: 9.9 },
              ),
              invalidSidecar: PlayerCore.audioSidecarSyncDecision(
                { currentTime: 4, playbackRate: 0, paused: true, ended: false },
                { currentTime: NaN },
              ),
              invalidVideo: PlayerCore.audioSidecarSyncDecision(
                { currentTime: Infinity, playbackRate: NaN, paused: false, ended: false },
                { currentTime: 3 },
                { forceSeek: true },
              ),
              ended: PlayerCore.audioSidecarSyncDecision(
                { currentTime: 8, playbackRate: 2, paused: false, ended: true },
                { currentTime: 8 },
              ),
            }))()
            "#,
        ),
        r#"{"forced":{"seekTime":12.5,"playbackRate":1.5,"shouldPlay":true},"inSync":{"seekTime":null,"playbackRate":1,"shouldPlay":true},"behind":{"seekTime":null,"playbackRate":1,"shouldPlay":true},"ahead":{"seekTime":null,"playbackRate":1,"shouldPlay":true},"discontinuity":{"seekTime":10,"playbackRate":1,"shouldPlay":true},"pausedDrift":{"seekTime":10,"playbackRate":1,"shouldPlay":false},"invalidSidecar":{"seekTime":4,"playbackRate":1,"shouldPlay":false},"invalidVideo":{"seekTime":null,"playbackRate":1,"shouldPlay":true},"ended":{"seekTime":null,"playbackRate":2,"shouldPlay":false}}"#
    );
}

#[test]
fn audio_sidecar_sync_does_not_restart_a_track_that_ends_before_video() {
    let mut ctx = player_core_context();
    assert_eq!(
        eval_json(
            &mut ctx,
            r#"
            (() => ({
              exhausted: PlayerCore.audioSidecarSyncDecision(
                { currentTime: 29.8, playbackRate: 1, paused: false, ended: false },
                { currentTime: 29.64, duration: 29.64, ended: true },
              ),
              seekBack: PlayerCore.audioSidecarSyncDecision(
                { currentTime: 10, playbackRate: 1, paused: false, ended: false },
                { currentTime: 29.64, duration: 29.64, ended: true },
                { forceSeek: true },
              ),
            }))()
            "#,
        ),
        r#"{"exhausted":{"seekTime":null,"playbackRate":1,"shouldPlay":false},"seekBack":{"seekTime":10,"playbackRate":1,"shouldPlay":true}}"#
    );
}

#[test]
fn audio_sidecar_output_routes_only_the_selected_logical_source() {
    let mut ctx = player_core_context();
    assert_eq!(
        eval_json(
            &mut ctx,
            r#"
            (() => ({
              direct: PlayerCore.reviewAudioOutputDecision('direct', false, 0.6),
              sidecars: PlayerCore.reviewAudioOutputDecision('sidecars', false, 1.4),
              muted: PlayerCore.reviewAudioOutputDecision('muted', false, -0.2),
              userMuted: PlayerCore.reviewAudioOutputDecision('direct', true, 0.8),
              zeroVolume: PlayerCore.reviewAudioOutputDecision('sidecars', false, 0),
              unknown: PlayerCore.reviewAudioOutputDecision('other', false, 0.5),
            }))()
            "#,
        ),
        r#"{"direct":{"videoMuted":false,"sidecarMuted":true,"volume":0.6},"sidecars":{"videoMuted":true,"sidecarMuted":false,"volume":1},"muted":{"videoMuted":true,"sidecarMuted":true,"volume":0},"userMuted":{"videoMuted":true,"sidecarMuted":true,"volume":0.8},"zeroVolume":{"videoMuted":true,"sidecarMuted":true,"volume":0},"unknown":{"videoMuted":true,"sidecarMuted":true,"volume":0.5}}"#
    );
}

#[test]
fn region_helpers_set_align_and_clamp_to_display() {
    let mut ctx = player_core_context();
    ctx.eval(Source::from_bytes(
        r#"const DISPLAY = { id: 'DISPLAY2', name: 'Display 2', x: 1920, y: -120, width: 2560, height: 1440, is_primary: false };
        const FULL = PlayerCore.regionForDisplay(DISPLAY);
        const SMALL = { display_id: 'DISPLAY2', x: 2000, y: 0, width: 800, height: 450 };"#,
    ))
    .expect("define display and region");

    assert_eq!(
        eval_json(&mut ctx, "FULL"),
        r#"{"display_id":"DISPLAY2","x":1920,"y":-120,"width":2560,"height":1440}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.alignRegion(SMALL, DISPLAY, 'left')"),
        r#"{"display_id":"DISPLAY2","x":1920,"y":0,"width":800,"height":450}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.alignRegion(SMALL, DISPLAY, 'right')"),
        r#"{"display_id":"DISPLAY2","x":3680,"y":0,"width":800,"height":450}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.alignRegion(SMALL, DISPLAY, 'top')"),
        r#"{"display_id":"DISPLAY2","x":2000,"y":-120,"width":800,"height":450}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.alignRegion(SMALL, DISPLAY, 'bottom')"),
        r#"{"display_id":"DISPLAY2","x":2000,"y":870,"width":800,"height":450}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "PlayerCore.alignRegion(SMALL, DISPLAY, 'center')"),
        r#"{"display_id":"DISPLAY2","x":2800,"y":375,"width":800,"height":450}"#
    );
    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.clampRegionToDisplay({ display_id: 'DISPLAY2', x: 1000, y: -900, width: 4000, height: 2000 }, DISPLAY)"
        ),
        r#"{"display_id":"DISPLAY2","x":1920,"y":-120,"width":2560,"height":1440}"#
    );
    assert_eq!(
        eval_json(
            &mut ctx,
            "PlayerCore.clampRegionToDisplay({ display_id: 'DISPLAY2', x: 2000, y: 0, width: 801, height: 451 }, DISPLAY)"
        ),
        r#"{"display_id":"DISPLAY2","x":2000,"y":0,"width":800,"height":450}"#
    );
}

// --- Settlement vs user reposition (plan 2026-07-26-review-audio-settlement) ---

/// A `seeked` from the initial source assignment must not be treated as a user
/// reposition. Traced on a warm cache, the settlement's forced sidecar seek
/// re-seeked already-audible sidecars backward ~20 ms for no correction: the drift
/// was far inside `AUDIO_SIDECAR_HARD_SEEK_TOLERANCE_S` and the element landed
/// back where it started. That was the audible repeat at the start of every clip.
#[test]
fn settlement_seeks_are_distinguished_from_user_repositions() {
    let mut ctx = player_core_context();

    assert_eq!(
        eval(
            &mut ctx,
            "PlayerCore.beginSourceAssignment(PlayerCore.createLogicalSeekState(), 1, 0, 30).targetSource"
        ),
        "assignment"
    );
    assert_eq!(
        eval(
            &mut ctx,
            "PlayerCore.requestLogicalSeek(PlayerCore.beginSourceAssignment(PlayerCore.createLogicalSeekState(), 1, 0, 30), 12, 30).targetSource"
        ),
        "user"
    );
    assert_eq!(
        eval_json(
            &mut ctx,
            "(() => { let s = PlayerCore.beginSourceAssignment(PlayerCore.createLogicalSeekState(), 1, 0, 30); s = PlayerCore.metadataSeekDecision(s, 1, 30).state; const d = PlayerCore.seekedDecision(s, 1, 0, 30); return { confirmed: d.confirmed, source: d.confirmedSource }; })()"
        ),
        "{\"confirmed\":true,\"source\":\"assignment\"}"
    );
    assert_eq!(
        eval_json(
            &mut ctx,
            "(() => { let s = PlayerCore.beginSourceAssignment(PlayerCore.createLogicalSeekState(), 1, 0, 30); s = PlayerCore.metadataSeekDecision(s, 1, 30).state; s = PlayerCore.requestLogicalSeek(s, 12, 30); const d = PlayerCore.seekedDecision(s, 1, 12, 30); return { confirmed: d.confirmed, source: d.confirmedSource }; })()"
        ),
        "{\"confirmed\":true,\"source\":\"user\"}"
    );
}

/// `beginSourceAssignment` deliberately carries a pending logical target across a
/// source replacement. If that target came from the user, its provenance must
/// survive, or a replacement silently downgrades a real reposition to settlement
/// and skips the realignment the user asked for.
#[test]
fn source_replacement_preserves_user_seek_provenance() {
    let mut ctx = player_core_context();
    assert_eq!(
        eval(
            &mut ctx,
            "(() => { let s = PlayerCore.requestLogicalSeek(PlayerCore.createLogicalSeekState(), 12, 60); s = PlayerCore.beginSourceAssignment(s, 2, 0, 60); return s.targetSource; })()"
        ),
        "user"
    );
    assert_eq!(
        eval(
            &mut ctx,
            "(() => { let s = PlayerCore.requestLogicalSeek(PlayerCore.createLogicalSeekState(), 12, 60); s = PlayerCore.beginSourceAssignment(s, 2, 0, 60); return s.targetTime; })()"
        ),
        "12"
    );
}

/// A late `seeked` for a superseded target must not confirm the newer one: it
/// re-applies the current target and leaves provenance intact, so only the real
/// arrival confirms it.
#[test]
fn a_stale_seek_completion_cannot_confirm_a_newer_user_target() {
    let mut ctx = player_core_context();
    assert_eq!(
        eval_json(
            &mut ctx,
            "(() => { let s = PlayerCore.beginSourceAssignment(PlayerCore.createLogicalSeekState(), 1, 0, 30); s = PlayerCore.metadataSeekDecision(s, 1, 30).state; s = PlayerCore.requestLogicalSeek(s, 12, 30); const d = PlayerCore.seekedDecision(s, 1, 0, 30); return { confirmed: d.confirmed, source: d.confirmedSource, applyTime: d.applyTime, targetSource: d.state.targetSource }; })()"
        ),
        "{\"confirmed\":false,\"source\":null,\"applyTime\":12,\"targetSource\":\"user\"}"
    );
    assert_eq!(
        eval_json(
            &mut ctx,
            "(() => { let s = PlayerCore.beginSourceAssignment(PlayerCore.createLogicalSeekState(), 1, 0, 30); s = PlayerCore.metadataSeekDecision(s, 1, 30).state; s = PlayerCore.requestLogicalSeek(s, 12, 30); s = PlayerCore.seekedDecision(s, 1, 0, 30).state; const d = PlayerCore.seekedDecision(s, 1, 12, 30); return { confirmed: d.confirmed, source: d.confirmedSource }; })()"
        ),
        "{\"confirmed\":true,\"source\":\"user\"}"
    );
}

/// Only a user reposition forces past the drift tolerance. Settlement falls
/// through to the ordinary tolerance path, which — measured — assigned no seek at
/// all for the ~20 ms it was "correcting".
#[test]
fn only_user_repositions_force_a_sidecar_realignment() {
    let mut ctx = player_core_context();
    assert_eq!(
        eval(&mut ctx, "PlayerCore.sidecarRealignmentForced('user')"),
        "true"
    );
    assert_eq!(
        eval(&mut ctx, "PlayerCore.sidecarRealignmentForced('assignment')"),
        "false"
    );
    // Nothing confirmed: an incidental `seeked` forces nothing.
    assert_eq!(
        eval(&mut ctx, "PlayerCore.sidecarRealignmentForced(null)"),
        "false"
    );
}
