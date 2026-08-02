use clipline_updater::manifest::{
    parse_update_manifest, validate_release_download_url, ManifestErrorKind, UpdateChannel,
    UpdatePolicy, UpdateVariant, MAX_MANIFEST_BYTES,
};
use semver::Version;
use serde_json::{json, Value};
use url::Url;

const CURRENT: &str = "0.1.43";
const NEXT: &str = "0.1.44";
const REGULAR_URL: &str =
    "https://github.com/dain98/clipline/releases/download/nightly/Clipline_0.1.44_x64-setup.exe";
const STANDALONE_URL: &str =
    "https://github.com/dain98/clipline/releases/download/nightly/Clipline_0.1.44_x64-standalone-setup.exe";

fn policy(channel: UpdateChannel, variant: UpdateVariant) -> UpdatePolicy {
    UpdatePolicy::new(Version::parse(CURRENT).unwrap(), channel, variant)
}

fn manifest(version: &str, url: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "version": version,
        "notes": "A bounded test update.",
        "pub_date": "2026-08-02T12:34:56Z",
        "platforms": {
            "windows-x86_64": {
                "signature": "trusted updater signature",
                "url": url
            }
        }
    }))
    .unwrap()
}

fn set(value: &mut Value, pointer: &str, replacement: Value) -> Vec<u8> {
    *value.pointer_mut(pointer).unwrap() = replacement;
    serde_json::to_vec(value).unwrap()
}

#[test]
fn parses_current_regular_and_standalone_manifest_shapes() {
    let regular = parse_update_manifest(
        &manifest(NEXT, REGULAR_URL),
        &policy(UpdateChannel::Nightly, UpdateVariant::Regular),
    )
    .unwrap();
    assert_eq!(regular.version, Version::parse(NEXT).unwrap());
    assert_eq!(regular.notes, "A bounded test update.");
    assert_eq!(regular.pub_date, "2026-08-02T12:34:56Z");
    assert_eq!(regular.target.url.as_str(), REGULAR_URL);
    assert_eq!(regular.target.signature, "trusted updater signature");

    let standalone = parse_update_manifest(
        &manifest(NEXT, STANDALONE_URL),
        &policy(UpdateChannel::Nightly, UpdateVariant::Standalone),
    )
    .unwrap();
    assert_eq!(standalone.target.url.as_str(), STANDALONE_URL);
}

#[test]
fn enforces_the_raw_256_kib_limit_before_parsing() {
    let mut under: Value = serde_json::from_slice(&manifest(NEXT, REGULAR_URL)).unwrap();
    let base_size = serde_json::to_vec(&under).unwrap().len();
    *under.pointer_mut("/notes").unwrap() =
        Value::String("n".repeat(MAX_MANIFEST_BYTES - base_size - 2));
    let under = serde_json::to_vec(&under).unwrap();
    assert!(under.len() <= MAX_MANIFEST_BYTES);
    parse_update_manifest(
        &under,
        &policy(UpdateChannel::Nightly, UpdateVariant::Regular),
    )
    .unwrap();

    let mut over = manifest(NEXT, REGULAR_URL);
    over.resize(MAX_MANIFEST_BYTES + 1, b' ');
    let error = parse_update_manifest(
        &over,
        &policy(UpdateChannel::Nightly, UpdateVariant::Regular),
    )
    .unwrap_err();
    assert_eq!(error.kind(), ManifestErrorKind::TooLarge);
}

#[test]
fn rejects_duplicate_and_unknown_keys_at_every_schema_level() {
    let duplicate_root = format!(
        r#"{{"version":"{NEXT}","version":"0.1.45","notes":"n","pub_date":"2026-08-02T12:34:56Z","platforms":{{"windows-x86_64":{{"signature":"s","url":"{REGULAR_URL}"}}}}}}"#
    );
    let duplicate_target = format!(
        r#"{{"version":"{NEXT}","notes":"n","pub_date":"2026-08-02T12:34:56Z","platforms":{{"windows-x86_64":{{"signature":"s","signature":"other","url":"{REGULAR_URL}"}}}}}}"#
    );
    let unknown_root = format!(
        r#"{{"version":"{NEXT}","notes":"n","pub_date":"2026-08-02T12:34:56Z","unexpected":true,"platforms":{{"windows-x86_64":{{"signature":"s","url":"{REGULAR_URL}"}}}}}}"#
    );
    let unknown_platform = format!(
        r#"{{"version":"{NEXT}","notes":"n","pub_date":"2026-08-02T12:34:56Z","platforms":{{"windows-x86_64":{{"signature":"s","url":"{REGULAR_URL}"}},"linux-x86_64":{{"signature":"s","url":"https://example.invalid"}}}}}}"#
    );

    for body in [
        duplicate_root.as_bytes(),
        duplicate_target.as_bytes(),
        unknown_root.as_bytes(),
        unknown_platform.as_bytes(),
    ] {
        let error = parse_update_manifest(
            body,
            &policy(UpdateChannel::Nightly, UpdateVariant::Regular),
        )
        .unwrap_err();
        assert_eq!(error.kind(), ManifestErrorKind::InvalidJson);
    }
}

#[test]
fn rejects_unknown_or_missing_windows_platform() {
    let body = json!({
        "version": NEXT,
        "notes": "n",
        "pub_date": "2026-08-02T12:34:56Z",
        "platforms": {
            "windows-aarch64": { "signature": "s", "url": REGULAR_URL }
        }
    });
    let error = parse_update_manifest(
        &serde_json::to_vec(&body).unwrap(),
        &policy(UpdateChannel::Nightly, UpdateVariant::Regular),
    )
    .unwrap_err();
    assert_eq!(error.kind(), ManifestErrorKind::InvalidJson);
}

#[test]
fn rejects_non_https_and_unapproved_origins_or_url_features() {
    for url in [
        "http://github.com/dain98/clipline/releases/download/nightly/Clipline_0.1.44_x64-setup.exe",
        "https://example.com/dain98/clipline/releases/download/nightly/Clipline_0.1.44_x64-setup.exe",
        "https://github.com:444/dain98/clipline/releases/download/nightly/Clipline_0.1.44_x64-setup.exe",
        "https://github.com/dain98/clipline/releases/download/nightly/Clipline_0.1.44_x64-setup.exe?token=secret",
        "https://github.com/dain98/clipline/releases/download/nightly/Clipline_0.1.44_x64-setup.exe#fragment",
    ] {
        let error = parse_update_manifest(
            &manifest(NEXT, url),
            &policy(UpdateChannel::Nightly, UpdateVariant::Regular),
        )
        .unwrap_err();
        assert_eq!(error.kind(), ManifestErrorKind::InvalidDownloadUrl);
    }
}

#[test]
fn rejects_channel_crossing_and_stable_prereleases() {
    let stable_asset =
        "https://github.com/dain98/clipline/releases/download/v0.1.44/Clipline_0.1.44_x64-setup.exe";
    let nightly_policy = policy(UpdateChannel::Nightly, UpdateVariant::Regular);
    let error = parse_update_manifest(&manifest(NEXT, stable_asset), &nightly_policy).unwrap_err();
    assert_eq!(error.kind(), ManifestErrorKind::ChannelMismatch);

    let error = parse_update_manifest(
        &manifest(NEXT, REGULAR_URL),
        &policy(UpdateChannel::Stable, UpdateVariant::Regular),
    )
    .unwrap_err();
    assert_eq!(error.kind(), ManifestErrorKind::ChannelMismatch);

    let prerelease_url = "https://github.com/dain98/clipline/releases/download/v0.1.44-beta.1/Clipline_0.1.44-beta.1_x64-setup.exe";
    let error = parse_update_manifest(
        &manifest("0.1.44-beta.1", prerelease_url),
        &policy(UpdateChannel::Stable, UpdateVariant::Regular),
    )
    .unwrap_err();
    assert_eq!(error.kind(), ManifestErrorKind::ChannelMismatch);
}

#[test]
fn accepts_the_exact_stable_versioned_release_policy() {
    let stable_asset =
        "https://github.com/dain98/clipline/releases/download/v0.1.44/Clipline_0.1.44_x64-setup.exe";
    let parsed = parse_update_manifest(
        &manifest(NEXT, stable_asset),
        &policy(UpdateChannel::Stable, UpdateVariant::Regular),
    )
    .unwrap();
    assert_eq!(parsed.target.url.as_str(), stable_asset);
}

#[test]
fn rejects_regular_and_standalone_manifest_crossing() {
    let error = parse_update_manifest(
        &manifest(NEXT, STANDALONE_URL),
        &policy(UpdateChannel::Nightly, UpdateVariant::Regular),
    )
    .unwrap_err();
    assert_eq!(error.kind(), ManifestErrorKind::VariantMismatch);

    let error = parse_update_manifest(
        &manifest(NEXT, REGULAR_URL),
        &policy(UpdateChannel::Nightly, UpdateVariant::Standalone),
    )
    .unwrap_err();
    assert_eq!(error.kind(), ManifestErrorKind::VariantMismatch);
}

#[test]
fn rejects_invalid_versions_dates_and_missing_signatures() {
    let base: Value = serde_json::from_slice(&manifest(NEXT, REGULAR_URL)).unwrap();
    let cases = [
        (
            set(&mut base.clone(), "/version", json!("version-next")),
            ManifestErrorKind::InvalidVersion,
        ),
        (
            set(
                &mut base.clone(),
                "/pub_date",
                json!("2026-02-29T12:34:56Z"),
            ),
            ManifestErrorKind::InvalidPublicationDate,
        ),
        (
            set(
                &mut base.clone(),
                "/platforms/windows-x86_64/signature",
                json!("  "),
            ),
            ManifestErrorKind::MissingSignature,
        ),
    ];
    for (body, expected) in cases {
        let error = parse_update_manifest(
            &body,
            &policy(UpdateChannel::Nightly, UpdateVariant::Regular),
        )
        .unwrap_err();
        assert_eq!(error.kind(), expected);
    }
}

#[test]
fn accepts_only_a_strictly_newer_semver() {
    for version in ["0.1.43", "0.1.42", "0.1.43+rebuilt"] {
        let body = manifest(version, REGULAR_URL);
        let error = parse_update_manifest(
            &body,
            &policy(UpdateChannel::Nightly, UpdateVariant::Regular),
        )
        .unwrap_err();
        assert_eq!(error.kind(), ManifestErrorKind::VersionNotNewer);
    }
}

#[test]
fn redirect_targets_outside_the_exact_manifest_asset_policy_are_rejected() {
    let version = Version::parse(NEXT).unwrap();
    let approved = Url::parse(REGULAR_URL).unwrap();
    validate_release_download_url(
        &approved,
        &version,
        UpdateChannel::Nightly,
        UpdateVariant::Regular,
    )
    .unwrap();

    for redirect in [
        "https://release-assets.githubusercontent.com/github-production-release-asset/1/unrelated",
        "https://github.com/dain98/other/releases/download/nightly/Clipline_0.1.44_x64-setup.exe",
        "https://github.com/dain98/clipline/releases/download/nightly/Clipline_0.1.45_x64-setup.exe",
    ] {
        let error = validate_release_download_url(
            &Url::parse(redirect).unwrap(),
            &version,
            UpdateChannel::Nightly,
            UpdateVariant::Regular,
        )
        .unwrap_err();
        assert!(matches!(
            error.kind(),
            ManifestErrorKind::InvalidDownloadUrl | ManifestErrorKind::VariantMismatch
        ));
    }
}

#[test]
fn parse_failures_never_echo_the_response_body_or_url() {
    let secret = "raw-response-secret-token";
    let body = format!(r#"{{"{secret}":"{secret}"}}"#);
    let error = parse_update_manifest(
        body.as_bytes(),
        &policy(UpdateChannel::Nightly, UpdateVariant::Regular),
    )
    .unwrap_err();
    assert!(!error.to_string().contains(secret));
    assert!(!format!("{error:?}").contains(secret));

    let body = manifest(NEXT, &format!("https://example.com/{secret}"));
    let error = parse_update_manifest(
        &body,
        &policy(UpdateChannel::Nightly, UpdateVariant::Regular),
    )
    .unwrap_err();
    assert!(!error.to_string().contains(secret));
    assert!(!format!("{error:?}").contains(secret));
}

#[test]
fn channel_variant_endpoints_remain_distinct() {
    assert_eq!(
        UpdateChannel::Nightly.manifest_endpoint(UpdateVariant::Regular),
        "https://github.com/dain98/clipline/releases/download/nightly/latest.json"
    );
    assert_eq!(
        UpdateChannel::Nightly.manifest_endpoint(UpdateVariant::Standalone),
        "https://github.com/dain98/clipline/releases/download/nightly/latest-standalone.json"
    );
    assert_eq!(
        UpdateChannel::Stable.manifest_endpoint(UpdateVariant::Regular),
        "https://github.com/dain98/clipline/releases/latest/download/latest.json"
    );
    assert_eq!(
        UpdateChannel::Stable.manifest_endpoint(UpdateVariant::Standalone),
        "https://github.com/dain98/clipline/releases/latest/download/latest-standalone.json"
    );
}
