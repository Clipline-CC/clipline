use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf()
}

fn unix_day_for_iso_date(value: &str) -> i64 {
    let mut parts = value.split('-');
    let mut year: i64 = parts
        .next()
        .expect("date year")
        .parse()
        .expect("numeric year");
    let month: i64 = parts
        .next()
        .expect("date month")
        .parse()
        .expect("numeric month");
    let day: i64 = parts
        .next()
        .expect("date day")
        .parse()
        .expect("numeric day");
    assert!(parts.next().is_none(), "date must be YYYY-MM-DD: {value}");
    assert!((1..=12).contains(&month), "invalid date month: {value}");
    assert!((1..=31).contains(&day), "invalid date day: {value}");

    year -= i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn assert_not_past(value: &str, subject: &str) {
    let today = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time after epoch")
        .as_secs() as i64
        / 86_400;
    assert!(
        today <= unix_day_for_iso_date(value),
        "{subject} expired on {value}; review and update its policy"
    );
}

fn rust_sources_below(root: &Path) -> Vec<PathBuf> {
    let mut pending = vec![root.to_path_buf()];
    let mut sources = Vec::new();
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory).expect("read Rust source directory") {
            let path = entry.expect("Rust source entry").path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().and_then(|value| value.to_str()) == Some("rs") {
                sources.push(path);
            }
        }
    }
    sources
}

#[test]
fn unsafe_application_platform_helpers_live_under_the_windows_module() {
    let source_root = workspace_root().join("apps/clipline-app/src");
    let windows_root = source_root.join("windows");
    let sources = rust_sources_below(&source_root);
    for symbol in [
        "CredWriteW",
        "CredReadW",
        "CredDeleteW",
        "CredFree",
        "CREDENTIALW",
        "ShellExecuteW",
        "GetDiskFreeSpaceExW",
        "MoveFileExW",
    ] {
        let owners: Vec<_> = sources
            .iter()
            .filter(|path| fs::read_to_string(path).unwrap().contains(symbol))
            .collect();
        assert!(!owners.is_empty(), "expected a Windows owner for {symbol}");
        assert!(
            owners.iter().all(|path| path.starts_with(&windows_root)),
            "{symbol} must be confined below {} but appears in {owners:?}",
            windows_root.display()
        );
    }

    let duplicate_clocks: Vec<_> = sources
        .iter()
        .filter(|path| {
            path.file_name().and_then(|name| name.to_str()) != Some("util.rs")
                && fs::read_to_string(path).unwrap().contains("fn unix_now(")
        })
        .collect();
    assert!(
        duplicate_clocks.is_empty(),
        "Unix wall-clock helpers must be shared from util.rs: {duplicate_clocks:?}"
    );

    let duplicate_wide_terminators: Vec<_> = sources
        .iter()
        .filter(|path| !path.starts_with(&windows_root))
        .filter(|path| {
            let source = fs::read_to_string(path).unwrap();
            source.contains("chain(std::iter::once(0))") || source.contains("chain(Some(0))")
        })
        .collect();
    assert!(
        duplicate_wide_terminators.is_empty(),
        "NUL-terminated UTF-16 conversion must use windows::wide_null: {duplicate_wide_terminators:?}"
    );
}

#[test]
fn capture_diagnostics_and_snapshot_names_match_production_behavior() {
    let root = workspace_root();
    let wasapi_path = root.join("crates/clipline-capture/src/windows/wasapi.rs");
    let ffmpeg_path = root.join("crates/clipline-capture/src/ffmpeg_encoder.rs");
    let wasapi = fs::read_to_string(&wasapi_path).expect("read WASAPI source");
    let ffmpeg = fs::read_to_string(&ffmpeg_path).expect("read FFmpeg encoder source");
    let wasapi_production = wasapi
        .rsplit_once("#[cfg(test)]\nmod tests")
        .expect("WASAPI unit-test boundary")
        .0;
    let ffmpeg_production = ffmpeg
        .split_once("#[cfg(test)]\nmod tests")
        .expect("FFmpeg unit-test boundary")
        .0;

    let snapshot = wasapi_production
        .split_once("struct ProcessSnapshotEntry")
        .expect("process snapshot entry")
        .1
        .split_once('}')
        .expect("process snapshot fields")
        .0;
    assert!(snapshot.contains("image_name:"));
    assert!(
        !snapshot.contains("process_path:"),
        "ToolHelp exposes a bare executable image name, not a full path"
    );
    assert!(!wasapi_production.contains("InitPropVariantFromBuffer"));
    assert!(
        !wasapi_production.contains("eprintln!"),
        "production WASAPI diagnostics must use the typed diagnostic route"
    );
    assert!(
        !ffmpeg_production.contains("eprintln!"),
        "production FFmpeg reader diagnostics must not print ad hoc"
    );

    let app =
        fs::read_to_string(root.join("apps/clipline-app/src/app.rs")).expect("read app source");
    let install = app
        .find("install_diagnostic_handler(|event|")
        .expect("capture diagnostic handler installation");
    let builder = app.find("tauri::Builder").expect("Tauri builder");
    assert!(
        install < builder,
        "capture diagnostics must be routed before capture services can start"
    );
}

#[test]
fn dependency_and_ci_supply_chain_is_reviewable_and_audited() {
    let root = workspace_root();
    let workflows = root.join(".github/workflows");
    let mut saw_rustsec = false;

    for entry in fs::read_dir(&workflows).expect("read workflows") {
        let path = entry.expect("workflow entry").path();
        if !matches!(
            path.extension().and_then(|extension| extension.to_str()),
            Some("yml" | "yaml")
        ) {
            continue;
        }
        let workflow = fs::read_to_string(&path).expect("read workflow");
        saw_rustsec |= workflow.contains("rustsec/audit-check@");
        for line in workflow.lines() {
            let Some(spec) = line.trim().strip_prefix("- uses:") else {
                continue;
            };
            let spec = spec.trim();
            if spec.starts_with("./") {
                continue;
            }
            let (action, revision_and_comment) = spec.split_once('@').unwrap_or_else(|| {
                panic!("remote action lacks revision in {}: {line}", path.display())
            });
            let revision = revision_and_comment
                .split_whitespace()
                .next()
                .expect("action revision");
            assert!(
                revision.len() == 40 && revision.chars().all(|ch| ch.is_ascii_hexdigit()),
                "{action} must use a full commit SHA in {}: {line}",
                path.display()
            );
            assert!(
                line.contains('#'),
                "pinned action needs a readable version comment in {}: {line}",
                path.display()
            );
        }
    }
    assert!(saw_rustsec, "a pinned RustSec audit workflow is required");

    let audit_policy =
        fs::read_to_string(root.join(".cargo/audit.toml")).expect("read audit policy");
    assert!(audit_policy.contains("ignore = []"));
    for requirement in ["owner", "rationale", "expiry", "removal"] {
        assert!(
            audit_policy.to_ascii_lowercase().contains(requirement),
            "audit ignore policy must document {requirement}"
        );
    }

    let dependabot =
        fs::read_to_string(root.join(".github/dependabot.yml")).expect("read Dependabot config");
    assert!(dependabot.contains("package-ecosystem: cargo"));
    assert!(dependabot.contains("package-ecosystem: github-actions"));

    let lock = fs::read_to_string(root.join("Cargo.lock")).expect("read Cargo.lock");
    for (crate_name, minimum, advisories) in [
        ("anyhow", &[1, 0, 103][..], "RUSTSEC-2026-0190"),
        ("quick-xml", &[0, 41, 0][..], "RUSTSEC-2026-0194/0195"),
        ("quinn-proto", &[0, 11, 15][..], "RUSTSEC-2026-0185"),
    ] {
        let packages: Vec<_> = lock
            .split("[[package]]")
            .filter(|package| {
                package
                    .lines()
                    .any(|line| line.trim() == format!("name = \"{crate_name}\""))
            })
            .collect();
        assert!(!packages.is_empty(), "missing locked {crate_name} package");
        for package in packages {
            let version = package
                .lines()
                .find_map(|line| line.trim().strip_prefix("version = \"")?.strip_suffix('"'))
                .expect("package version");
            let parts: Vec<u64> = version
                .split('.')
                .map(|part| part.parse().expect("numeric package version"))
                .collect();
            assert!(
                parts.as_slice() >= minimum,
                "{crate_name} {version} is affected by {advisories}"
            );
        }
    }
}

#[test]
fn dependency_exceptions_and_fixed_runtime_are_owned_and_current() {
    let root = workspace_root();
    let lock = fs::read_to_string(root.join("Cargo.lock")).expect("read Cargo.lock");
    assert!(
        !lock.contains("name = \"audiopus\"") && !lock.contains("name = \"audiopus_sys\""),
        "the unmaintained audiopus binding must not be selected"
    );
    let opus_packages: Vec<_> = lock
        .split("[[package]]")
        .filter(|package| package.contains("name = \"shiguredo_opus\""))
        .collect();
    assert_eq!(opus_packages.len(), 1, "select one maintained Opus binding");
    assert!(
        opus_packages[0].contains("version = \"2026.1.0\"")
            && !opus_packages[0].contains("source = "),
        "use Clipline's reviewed shiguredo_opus 2026.1.0 controlled fork"
    );

    let mut reqwest_lines: Vec<_> = lock
        .split("[[package]]")
        .filter(|package| package.contains("name = \"reqwest\""))
        .map(|package| {
            let version = package
                .lines()
                .find_map(|line| line.trim().strip_prefix("version = \"")?.strip_suffix('"'))
                .expect("reqwest version");
            version.rsplit_once('.').expect("reqwest patch version").0
        })
        .collect();
    reqwest_lines.sort_unstable();
    assert_eq!(
        reqwest_lines,
        ["0.12", "0.13"],
        "only the two reviewed reqwest release lines may be selected"
    );

    let policy: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(root.join("docs/dependency-policy.json"))
            .expect("read dependency policy"),
    )
    .expect("valid dependency policy JSON");
    let exception = policy["duplicate_major_exceptions"]
        .as_array()
        .expect("duplicate-major exception array")
        .iter()
        .find(|exception| exception["package"] == "reqwest")
        .expect("reqwest duplicate-major exception");
    assert_eq!(
        exception["allowed_versions"],
        serde_json::json!(["0.12", "0.13"])
    );
    for field in ["owner", "rationale", "review_by", "remove_when"] {
        assert!(
            exception[field]
                .as_str()
                .is_some_and(|value| !value.is_empty()),
            "reqwest exception requires {field}"
        );
    }
    assert_not_past(
        exception["review_by"].as_str().expect("review date"),
        "reqwest duplicate-major exception",
    );
    let opus_fork = policy["controlled_forks"]
        .as_array()
        .expect("controlled-fork array")
        .iter()
        .find(|fork| fork["package"] == "shiguredo_opus")
        .expect("controlled Opus fork policy");
    for field in ["owner", "rationale", "review_by", "remove_when", "upstream"] {
        assert!(
            opus_fork[field]
                .as_str()
                .is_some_and(|value| !value.is_empty()),
            "controlled Opus fork requires {field}"
        );
    }
    assert_not_past(
        opus_fork["review_by"].as_str().expect("fork review date"),
        "controlled Opus fork",
    );
    let fork_build = fs::read_to_string(root.join("third-party/shiguredo_opus/build.rs"))
        .expect("read controlled Opus build script");
    for contract in [
        "windows_x86_64",
        "ubuntu-22.04_x86_64",
        "ubuntu-24.04_x86_64",
        "228e55adda46e79b7d5be1950283aa2f79f3de8b19081cb1a6ed74fa71f5f602",
        "opus.lib",
        "no reviewed prebuilt Opus hash",
    ] {
        assert!(
            fork_build.contains(contract),
            "Opus fork must retain {contract}"
        );
    }
    assert!(root.join("third-party/shiguredo_opus/LICENSE").is_file());
    assert!(root
        .join("third-party/shiguredo_opus/CLIPLINE-PATCHES.md")
        .is_file());

    let manifest: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(root.join("apps/clipline-app/webview2-fixed-runtime.json"))
            .expect("read WebView2 runtime manifest"),
    )
    .expect("valid WebView2 runtime manifest JSON");
    let version = manifest["version"].as_str().expect("runtime version");
    let architecture = manifest["architecture"]
        .as_str()
        .expect("runtime architecture");
    assert_eq!(architecture, "x64");
    assert_not_past(
        manifest["review_due_on"]
            .as_str()
            .expect("runtime review due date"),
        "WebView2 Fixed Version runtime review",
    );
    assert_eq!(manifest["max_review_age_days"], 30);
    assert!(manifest["source_url"]
        .as_str()
        .is_some_and(|url| url.starts_with("https://developer.microsoft.com/")));

    let config = fs::read_to_string(root.join("apps/clipline-app/tauri.standalone.conf.json"))
        .expect("read standalone config");
    let expected_folder =
        format!("Microsoft.WebView2.FixedVersionRuntime.{version}.{architecture}");
    assert_eq!(config.matches(&expected_folder).count(), 2);

    let verifier = fs::read_to_string(root.join("scripts/verify-webview2-runtime.ps1"))
        .expect("read WebView2 runtime verifier");
    for contract in [
        "review_due_on",
        "tauri.standalone.conf.json",
        "Test-Path",
        "RequirePayload",
        "msedgewebview2.exe",
    ] {
        assert!(
            verifier.contains(contract),
            "runtime verifier must enforce {contract}"
        );
    }
}

#[test]
fn ffmpeg_release_staging_is_pinned_allowlisted_and_attributed() {
    let root = workspace_root();
    let manifest: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(root.join("apps/clipline-app/ffmpeg-runtime.json"))
            .expect("read FFmpeg runtime manifest"),
    )
    .expect("valid FFmpeg runtime manifest JSON");

    assert_eq!(manifest["schema_version"].as_u64(), Some(1));
    let release_tag = manifest["release_tag"].as_str().expect("release tag");
    let archive_name = manifest["archive_name"].as_str().expect("archive name");
    let archive_url = manifest["archive_url"].as_str().expect("archive URL");
    let archive_sha = manifest["archive_sha256"]
        .as_str()
        .expect("archive SHA-256");
    assert!(release_tag.starts_with("autobuild-20") && !release_tag.contains("latest"));
    assert!(archive_name.ends_with("win64-lgpl-shared-8.1.zip"));
    assert!(archive_url.starts_with("https://github.com/BtbN/FFmpeg-Builds/releases/download/"));
    assert!(archive_url.contains(release_tag) && archive_url.ends_with(archive_name));
    assert!(!archive_url.contains("/latest/"));
    assert!(
        archive_sha.len() == 64 && archive_sha.chars().all(|ch| ch.is_ascii_hexdigit()),
        "FFmpeg archive requires an exact SHA-256"
    );
    assert!(manifest["version_line"]
        .as_str()
        .is_some_and(|line| line.starts_with("ffmpeg version n8.1.")));
    let forbidden_configuration = manifest["forbidden_configuration"]
        .as_array()
        .expect("forbidden FFmpeg configuration");
    for forbidden in [
        "--enable-gpl",
        "--enable-nonfree",
        "--enable-libx264",
        "--enable-libx265",
    ] {
        assert!(
            forbidden_configuration
                .iter()
                .any(|value| value.as_str() == Some(forbidden)),
            "FFmpeg manifest must reject {forbidden}"
        );
    }

    let files = manifest["allowed_files"]
        .as_array()
        .expect("FFmpeg file allowlist");
    let staged_names: Vec<_> = files
        .iter()
        .map(|file| file["staged_name"].as_str().expect("staged file name"))
        .collect();
    assert_eq!(
        staged_names,
        [
            "LICENSE.txt",
            "ffmpeg.exe",
            "avcodec-62.dll",
            "avdevice-62.dll",
            "avfilter-11.dll",
            "avformat-62.dll",
            "avutil-60.dll",
            "swresample-6.dll",
            "swscale-9.dll",
        ]
    );
    let mut unique_names = staged_names.clone();
    unique_names.sort_unstable();
    unique_names.dedup();
    assert_eq!(unique_names.len(), staged_names.len());
    for file in files {
        let archive_path = file["archive_path"].as_str().expect("archive path");
        let sha = file["sha256"].as_str().expect("file SHA-256");
        assert!(
            !archive_path.starts_with('/')
                && !archive_path.starts_with('\\')
                && !archive_path.contains("..")
        );
        assert!(file["size"].as_u64().is_some_and(|size| size > 0));
        assert!(sha.len() == 64 && sha.chars().all(|ch| ch.is_ascii_hexdigit()));
    }
    assert!(!staged_names.contains(&"ffplay.exe"));
    assert!(!staged_names.contains(&"ffprobe.exe"));

    let script = fs::read_to_string(root.join("scripts/stage-ffmpeg-resource.ps1"))
        .expect("read FFmpeg staging script");
    for contract in [
        "Get-FileHash",
        "OpenRead",
        "allowed_files",
        "PROVENANCE.json",
        "version_line",
        "forbidden_configuration",
        "manifest.schema_version",
        "Move-Item",
    ] {
        assert!(
            script.contains(contract),
            "FFmpeg staging must enforce {contract}"
        );
    }
    assert!(!script.contains("$SourceDir"));

    let verifier = fs::read_to_string(root.join("scripts/verify-ffmpeg-resource.ps1"))
        .expect("read staged FFmpeg verifier");
    for contract in [
        "allowed_files",
        "PROVENANCE.json",
        "manifest_sha256",
        "Get-FileHash",
        "version_line",
        "required_configuration",
        "forbidden_configuration",
        "manifest.schema_version",
        "Unexpected FFmpeg resource entries",
    ] {
        assert!(
            verifier.contains(contract),
            "offline FFmpeg resource verification must enforce {contract}"
        );
    }
    assert!(
        !verifier.contains("Invoke-WebRequest"),
        "the release-bundle preflight must stay offline"
    );

    let tauri_text = fs::read_to_string(root.join("apps/clipline-app/tauri.conf.json"))
        .expect("read Tauri config");
    assert_eq!(
        tauri_text.matches("\"ffmpeg/\"").count(),
        0,
        "regular installer must not embed ffmpeg/ after on-demand runtime"
    );
    let tauri: serde_json::Value =
        serde_json::from_str(&tauri_text).expect("valid Tauri configuration");
    assert!(
        tauri.pointer("/build/beforeBundleCommand").is_none(),
        "regular SKU must not run verify-ffmpeg-resource as beforeBundleCommand"
    );
    let standalone_text =
        fs::read_to_string(root.join("apps/clipline-app/tauri.standalone.conf.json"))
            .expect("read standalone Tauri config");
    assert!(
        standalone_text.contains("\"ffmpeg/\""),
        "standalone/offline SKU may still bundle the verified ffmpeg resource"
    );
    let release = fs::read_to_string(root.join("docs/release.workflow.yml"))
        .expect("read release workflow template");
    let stage_position = release
        .find(r".\scripts\stage-ffmpeg-resource.ps1")
        .expect("release workflow must stage the pinned FFmpeg runtime");
    let build_position = release
        .find("run: cargo tauri build")
        .expect("release workflow must build the Tauri app");
    assert!(
        stage_position < build_position,
        "release workflow must stage/verify FFmpeg before building SKUs that still bundle it"
    );
    let readme = fs::read_to_string(root.join("apps/clipline-app/ffmpeg/README.md"))
        .expect("read bundled FFmpeg notice");
    assert!(readme.contains("LGPL") && readme.contains("replace"));
    let notices =
        fs::read_to_string(root.join("THIRD-PARTY-NOTICES.md")).expect("read third-party notices");
    for provenance in [release_tag, "ce3c09c101", "PROVENANCE.json", "LGPL v3"] {
        assert!(
            notices.contains(provenance),
            "FFmpeg notice must retain {provenance}"
        );
    }
}

#[test]
fn divergence_prone_paths_keep_single_production_owners() {
    let root = workspace_root();
    let game_discovery = fs::read_to_string(root.join("apps/clipline-app/src/game_discovery.rs"))
        .expect("read game discovery source");
    assert!(
        !game_discovery.contains("#![allow(dead_code)]"),
        "game discovery must expose real dead-code drift to the compiler"
    );

    let app =
        fs::read_to_string(root.join("apps/clipline-app/src/app.rs")).expect("read app source");
    assert_eq!(
        app.matches("rfd::FileDialog::new()").count(),
        1,
        "folder pickers must share one dialog construction path"
    );
    assert!(app.matches("choose_folder_dialog(").count() >= 3);

    let service = fs::read_to_string(root.join("apps/clipline-app/src/service.rs"))
        .expect("read service source");
    assert!(!service.contains("to_string().contains(\"timed out\")"));
    let ffmpeg = fs::read_to_string(root.join("crates/clipline-capture/src/ffmpeg_encoder.rs"))
        .expect("read FFmpeg source");
    assert!(!ffmpeg.contains("let _ = codec"));

    let walker = fs::read_to_string(root.join("crates/clipline-mp4/src/walker.rs"))
        .expect("read MP4 walker");
    let trim = fs::read_to_string(root.join("crates/clipline-mp4/src/trim.rs"))
        .expect("read MP4 trim source");
    assert!(walker.contains("decode_box_header("));
    assert!(trim.matches("decode_box_header(").count() >= 2);
    assert!(!walker.contains("size32 == 1"));
    assert!(!trim.contains("size32 == 1"));

    let writer = fs::read_to_string(root.join("crates/clipline-mp4/src/writer.rs"))
        .expect("read MP4 writer");
    let writer_production = writer
        .split_once("#[cfg(test)]\nmod tests")
        .expect("writer unit-test boundary")
        .0;
    assert_eq!(
        writer_production
            .matches("state.record_sample(sample)?;")
            .count(),
        1,
        "all fragment transports must share one metadata commit path"
    );
    assert_eq!(
        writer_production.matches("fn record_sample(").count(),
        1,
        "sample-table accounting must remain centralized"
    );
}

#[test]
fn large_application_surfaces_delegate_to_named_domain_owners() {
    let root = workspace_root();
    let app =
        fs::read_to_string(root.join("apps/clipline-app/src/app.rs")).expect("read app shell");
    let service = fs::read_to_string(root.join("apps/clipline-app/src/service.rs"))
        .expect("read service shell");
    let library = fs::read_to_string(root.join("apps/clipline-app/src/library.rs"))
        .expect("read library shell");
    let cloud =
        fs::read_to_string(root.join("apps/clipline-app/src/cloud.rs")).expect("read cloud shell");

    for relative in [
        "apps/clipline-app/src/app/diagnostics.rs",
        "apps/clipline-app/src/app/support.rs",
        "apps/clipline-app/src/service/media_root.rs",
        "apps/clipline-app/src/library/naming.rs",
        "apps/clipline-app/src/cloud/cache_identity.rs",
    ] {
        assert!(
            root.join(relative).is_file(),
            "missing domain owner {relative}"
        );
    }
    assert!(
        app.contains("mod diagnostics;")
            && app.contains("mod support;")
            && !app.contains("struct RollingFileWriter")
    );
    assert!(
        service.contains("mod media_root;") && !service.contains("static MEDIA_ROOT_PROBE_COUNTER")
    );
    assert!(library.contains("mod naming;") && !library.contains("fn normalized_clip_file_name("));
    assert!(
        cloud.contains("mod cache_identity;")
            && !cloud.contains("fn validate_cloud_cache_component")
    );

    let presentation = fs::read_to_string(root.join("apps/clipline-app/ui/presentation-core.js"))
        .expect("read presentation core");
    let bootstrap = fs::read_to_string(root.join("apps/clipline-app/ui/bootstrap.mjs"))
        .expect("read module bootstrap");
    let index = fs::read_to_string(root.join("apps/clipline-app/ui/index.html"))
        .expect("read renderer markup");
    assert!(presentation.contains("Object.freeze({"));
    assert!(bootstrap.contains("import { PresentationCore }"));
    assert!(
        bootstrap.contains("import { PlayerCore }") && bootstrap.contains("import { CloudCore }")
    );
    assert!(index.contains("<script type=\"module\" src=\"bootstrap.mjs\"></script>"));
}

#[test]
fn private_reports_have_one_immutable_official_destination() {
    let root = workspace_root();
    let build =
        fs::read_to_string(root.join("apps/clipline-app/build.rs")).expect("read app build script");
    let support = fs::read_to_string(root.join("apps/clipline-app/src/app/support.rs"))
        .expect("read Support implementation");
    let release = fs::read_to_string(root.join("docs/release.workflow.yml"))
        .expect("read release workflow template");
    let endpoint = "https://support.dain.cafe/api/v1/reports";

    assert!(build.contains(endpoint));
    assert!(build.contains("OFFICIAL_BUG_REPORT_ENDPOINT"));
    assert!(build.contains("cargo:rustc-env=CLIPLINE_BUG_REPORT_ENDPOINT"));
    assert!(
        !support.contains(".join(\"api/v1/reports\")"),
        "the configured value is already the complete intake URL"
    );
    assert!(release.contains(&format!("CLIPLINE_BUG_REPORT_ENDPOINT: {endpoint}")));
    assert!(
        !release.contains("vars.CLIPLINE_BUG_REPORT_ENDPOINT"),
        "release builds must not redirect private reports through a mutable repository variable"
    );
}
