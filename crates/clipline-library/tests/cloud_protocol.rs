use clipline_library::protocol::*;

#[test]
fn base_url_preserves_prefix_and_plain_http_policy() {
    let prefixed = CloudApiBase::parse("https://clips.example/clipline", false).unwrap();
    assert_eq!(prefixed.as_str(), "https://clips.example/clipline/");
    assert_eq!(
        prefixed.api_url("api/v1/uploads").unwrap().as_str(),
        "https://clips.example/clipline/api/v1/uploads"
    );
    assert!(matches!(
        CloudApiBase::parse("http://clips.example", true),
        Err(CloudProtocolError::PlainHttpPublicHost)
    ));
    assert!(matches!(
        CloudApiBase::parse("http://192.168.1.20", false),
        Err(CloudProtocolError::PlainHttpRequiresConfirmation)
    ));
    assert!(CloudApiBase::parse("http://192.168.1.20", true).is_ok());
    assert!(CloudApiBase::parse("http://[::1]", true).is_ok());
    assert!(CloudApiBase::parse("https://user@clips.example", false).is_err());
}

#[test]
fn resource_and_authenticated_template_urls_are_segment_safe_and_same_origin() {
    let base = CloudApiBase::parse("https://clips.example/prefix", false).unwrap();
    assert_eq!(
        base.clip_url("clip-1", Some("visibility"))
            .unwrap()
            .as_str(),
        "https://clips.example/prefix/api/v1/clips/clip-1/visibility"
    );
    assert!(base.clip_url("../foreign", None).is_err());
    assert_eq!(
        base.authenticated_upload_url("api/v1/u/{part_number}", 7)
            .unwrap()
            .as_str(),
        "https://clips.example/prefix/api/v1/u/7"
    );
    assert!(base
        .authenticated_upload_url("https://objects.example/part/7", 7)
        .is_err());
    assert!(base
        .authenticated_upload_url("https://clips.example:8443/part/7", 7)
        .is_err());
    assert!(base
        .api_url("https://foreign.example/api/v1/clips")
        .is_err());
    assert!(base.api_url("//foreign.example/api/v1/clips").is_err());
    assert!(base.api_url("../api/v1/clips").is_err());
}

#[test]
fn discovery_requires_exact_product_and_api_version() {
    let mut discovery = DiscoveryResponse {
        name: EXPECTED_DISCOVERY_NAME.into(),
        api_version: SUPPORTED_API_VERSION.into(),
        server_version: "1.2.3".into(),
        min_client_version: "0.1.0".into(),
        public_url: "https://clips.example".into(),
        features: DiscoveryFeatures {
            single_put_upload: true,
            chunked_upload: true,
            direct_s3_upload: false,
            public_sharing: true,
            clip_markers: true,
            max_upload_size_bytes: 42,
        },
    };
    validate_discovery(&discovery).unwrap();
    discovery.name = "Imposter".into();
    assert_eq!(
        validate_discovery(&discovery),
        Err(CloudProtocolError::InvalidDiscovery)
    );
    discovery.name = EXPECTED_DISCOVERY_NAME.into();
    discovery.api_version = "v2".into();
    assert_eq!(
        validate_discovery(&discovery),
        Err(CloudProtocolError::UnsupportedApiVersion("v2".into()))
    );
}

#[test]
fn absent_direct_upload_feature_defaults_false_and_upload_json_is_exact() {
    let discovery: DiscoveryFeatures = serde_json::from_value(serde_json::json!({
        "single_put_upload": true,
        "chunked_upload": true,
        "public_sharing": true,
        "clip_markers": true,
        "max_upload_size_bytes": 100
    }))
    .unwrap();
    assert!(!discovery.direct_s3_upload);

    let request = CreateUploadRequest {
        client_clip_id: Some("local-1".into()),
        title: "Ranked".into(),
        description: Some("desc".into()),
        game_name: Some("osu!".into()),
        game_id: Some("osu".into()),
        game_executable: None,
        source_type: Some("replay".into()),
        recorded_at: None,
        duration_ms: Some(1_500),
        file_size_bytes: 2_048,
        checksum_sha256: sha256_hex(b"payload"),
        container: "mp4".into(),
        video_codec: None,
        audio_codec: None,
        width: None,
        height: None,
        fps: None,
        visibility: Some("private".into()),
        markers: None,
    };
    let value = serde_json::to_value(request).unwrap();
    assert_eq!(value["client_clip_id"], "local-1");
    assert_eq!(value["description"], "desc");
    assert_eq!(value["markers"], serde_json::Value::Null);
    assert_eq!(value["container"], "mp4");
}

#[test]
fn protocol_error_preserves_not_found_classification() {
    assert!(CloudProtocolError::Api {
        status: reqwest::StatusCode::NOT_FOUND,
        message: "missing".into(),
    }
    .is_not_found());
    assert!(!CloudProtocolError::Api {
        status: reqwest::StatusCode::CONFLICT,
        message: "conflict".into(),
    }
    .is_not_found());
}
