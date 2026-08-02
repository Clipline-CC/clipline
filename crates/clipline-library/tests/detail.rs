use clipline_library::{
    ClipDetail, ClipDetailAudioTrack, ClipDetailRequest, ClipDetailResult, ClipPathIdentity,
    ForegroundGeneration, MarkerTick, RequestGeneration, UploadDialogSummary,
    WindowAttachmentGeneration, WindowWorkToken, MAX_CLIP_DETAIL_AUDIO_TRACKS,
    MAX_CLIP_DETAIL_MARKERS, MAX_CLIP_DETAIL_SIDECAR_BYTES, MAX_CLIP_DETAIL_STRING_BYTES,
    MAX_UPLOAD_DESCRIPTION_UTF16, MAX_UPLOAD_TITLE_UTF16,
};

fn window(request: u64) -> WindowWorkToken {
    WindowWorkToken {
        attachment: WindowAttachmentGeneration::new(3),
        foreground: ForegroundGeneration::new(5),
        request: RequestGeneration::new(request),
    }
}

fn request(path: &str, request: u64) -> ClipDetailRequest {
    ClipDetailRequest::new(ClipPathIdentity::from_text(path).unwrap(), window(request))
}

fn upload_summary() -> UploadDialogSummary {
    UploadDialogSummary::new(
        "Round win",
        "A close finish",
        "2 kills / 1 objective",
        "2/2 audio tracks",
    )
    .unwrap()
}

fn audio_track(id: &str, label: &str) -> ClipDetailAudioTrack {
    ClipDetailAudioTrack::new(id, label).unwrap()
}

fn detail() -> ClipDetail {
    ClipDetail::new(
        4096,
        vec![
            MarkerTick::new(0.0).unwrap(),
            MarkerTick::new(4.25).unwrap(),
        ],
        "2 kills / 1 objective",
        vec![
            audio_track("output", "Desktop audio"),
            audio_track("microphone", "Microphone"),
        ],
        upload_summary(),
    )
    .unwrap()
}

#[test]
fn request_and_result_own_the_exact_item_and_window_token() {
    let initial_request = request(r"C:\Clips\One.mp4", 8);
    let result = ClipDetailResult::new(&initial_request, detail());

    assert_eq!(result.owner(), initial_request.owner());
    assert!(result.matches_request(&initial_request));
    assert_eq!(result.detail().marker_ticks().len(), 2);
    assert_eq!(result.detail().audio_tracks().len(), 2);

    let replacement_item = request(r"C:\Clips\Two.mp4", 8);
    let replacement_request = request(r"C:\Clips\One.mp4", 9);
    assert_ne!(result.owner(), replacement_item.owner());
    assert_ne!(result.owner(), replacement_request.owner());
    assert!(!result.matches_request(&replacement_item));
    assert!(!result.matches_request(&replacement_request));
}

#[test]
fn exact_utf16_upload_limits_accept_surrogate_pair_boundaries() {
    let surrogate_pair = "\u{1F600}";
    let title = surrogate_pair.repeat(MAX_UPLOAD_TITLE_UTF16 / 2);
    let description = surrogate_pair.repeat(MAX_UPLOAD_DESCRIPTION_UTF16 / 2);
    let summary = UploadDialogSummary::new(&title, &description, "markers", "audio").unwrap();
    assert_eq!(summary.title(), title);
    assert_eq!(summary.description(), description);

    let over_title = format!("{title}{surrogate_pair}");
    let over_description = format!("{description}{surrogate_pair}");
    assert!(UploadDialogSummary::new(over_title, "", "", "").is_err());
    assert!(UploadDialogSummary::new("", over_description, "", "").is_err());
}

#[test]
fn marker_ticks_must_be_finite_and_nonnegative() {
    for invalid in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.001] {
        assert!(MarkerTick::new(invalid).is_err(), "accepted {invalid:?}");
    }
    assert_eq!(MarkerTick::new(-0.0).unwrap().seconds(), 0.0);

    let raw = serde_json::to_value(detail()).unwrap();
    let mut invalid = raw.clone();
    invalid["marker_ticks"] = serde_json::json!([-1.0]);
    assert!(serde_json::from_value::<ClipDetail>(invalid).is_err());
}

#[test]
fn hostile_deserialization_cannot_bypass_collection_or_sidecar_bounds() {
    let raw = serde_json::to_value(detail()).unwrap();

    let mut oversized_sidecar = raw.clone();
    oversized_sidecar["sidecar_bytes"] = serde_json::json!(MAX_CLIP_DETAIL_SIDECAR_BYTES + 1);
    assert!(serde_json::from_value::<ClipDetail>(oversized_sidecar).is_err());

    let mut too_many_markers = raw.clone();
    too_many_markers["marker_ticks"] = serde_json::Value::Array(
        (0..=MAX_CLIP_DETAIL_MARKERS)
            .map(|index| serde_json::json!(index as f64))
            .collect(),
    );
    assert!(serde_json::from_value::<ClipDetail>(too_many_markers).is_err());

    let mut too_many_tracks = raw;
    too_many_tracks["audio_tracks"] = serde_json::Value::Array(
        (0..=MAX_CLIP_DETAIL_AUDIO_TRACKS)
            .map(|index| serde_json::json!({ "id": format!("track-{index}"), "label": "Audio" }))
            .collect(),
    );
    assert!(serde_json::from_value::<ClipDetail>(too_many_tracks).is_err());
}

#[test]
fn hostile_deserialization_cannot_bypass_string_budget_or_duplicate_track_check() {
    let raw = serde_json::to_value(detail()).unwrap();

    let mut excessive_strings = raw.clone();
    excessive_strings["marker_digest"] =
        serde_json::json!("x".repeat(MAX_CLIP_DETAIL_STRING_BYTES + 1));
    assert!(serde_json::from_value::<ClipDetail>(excessive_strings).is_err());

    let mut duplicate_tracks = raw;
    duplicate_tracks["audio_tracks"] = serde_json::json!([
        { "id": "same", "label": "Desktop" },
        { "id": "same", "label": "Microphone" }
    ]);
    assert!(serde_json::from_value::<ClipDetail>(duplicate_tracks).is_err());

    assert!(
        serde_json::from_value::<ClipDetailAudioTrack>(serde_json::json!({
            "id": "",
            "label": "Audio"
        }))
        .is_err()
    );
}

#[test]
fn valid_detail_round_trips_without_weakening_ownership_or_bounds() {
    let request = request(r"\\Server\Share\Clip.mp4", 12);
    let result = ClipDetailResult::new(&request, detail());
    let json = serde_json::to_string(&result).unwrap();
    let decoded: ClipDetailResult = serde_json::from_str(&json).unwrap();

    assert_eq!(decoded, result);
    assert!(decoded.matches_request(&request));
    assert_eq!(decoded.detail().sidecar_bytes(), 4096);
    assert_eq!(decoded.detail().marker_digest(), "2 kills / 1 objective");
    assert_eq!(decoded.detail().upload().title(), "Round win");
}
