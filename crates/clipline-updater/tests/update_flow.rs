use std::cell::RefCell;
use std::fmt;
use std::io::Read as _;
use std::path::Path;
use std::rc::Rc;

use clipline_test_utils::TestDir;
use clipline_updater::download::DownloadTelemetry;
use clipline_updater::manifest::{
    installer_filename, parse_update_manifest, UpdateChannel, UpdatePolicy, UpdateVariant,
};
use clipline_updater::{
    install_verified, verify_download, verify_download_with_key, InstallerLauncher,
    PreparedInstallerHandoff, UpdateOperationError, UpdateOperationGate, UpdateOperationKind,
    UpdateOperationSnapshot, UpdateShutdown, VerificationError, CLIPLINE_MINISIGN_PUBLIC_KEY,
};
use reqwest::Url;
use sha2::{Digest, Sha256};

const FIXTURE_BYTES: &[u8] = b"test";
const FIXTURE_KEY: &str = include_str!("fixtures/known-good-public-key.pub");
const FIXTURE_SIGNATURE: &str = include_str!("fixtures/known-good-signature.b64");
const BAD_SIGNATURE: &str = include_str!("fixtures/known-bad-signature.b64");

#[test]
fn update_operations_are_single_owner_cancellable_and_release_on_drop() {
    let gate = UpdateOperationGate::new();
    let check = gate
        .begin(UpdateOperationKind::Check)
        .expect("first operation owns the gate");
    let first = gate.snapshot().expect("active operation snapshot");
    assert_eq!(first.kind, UpdateOperationKind::Check);
    assert!(!first.cancelled);

    assert!(matches!(
        gate.begin(UpdateOperationKind::Install),
        Err(UpdateOperationError::Busy {
            active: UpdateOperationKind::Check,
            requested: UpdateOperationKind::Install,
        })
    ));
    assert!(gate.cancel_active());
    assert!(check
        .cancellation()
        .load(std::sync::atomic::Ordering::Acquire));
    assert!(gate.snapshot().expect("cancelled snapshot").cancelled);

    drop(check);
    assert!(gate.snapshot().is_none());
    let install = gate
        .begin(UpdateOperationKind::Install)
        .expect("drop releases the operation gate");
    assert!(install.id() > first.id);
    install.commit_exit();
    assert_eq!(
        gate.snapshot()
            .expect("committed exit keeps gate owned")
            .kind,
        UpdateOperationKind::Install
    );
}

#[test]
fn shutdown_can_cancel_and_boundedly_wait_for_the_active_update() {
    let gate = UpdateOperationGate::new();
    let operation = gate
        .begin(UpdateOperationKind::Install)
        .expect("install operation");
    let id = operation.id();

    std::thread::scope(|scope| {
        scope.spawn(move || {
            while !operation
                .cancellation()
                .load(std::sync::atomic::Ordering::Acquire)
            {
                std::thread::yield_now();
            }
            drop(operation);
        });
        let quiesced = gate
            .quiesce_and_wait(std::time::Duration::from_secs(1))
            .expect("active update releases after cancellation");
        assert_eq!(
            quiesced.completed(),
            Some(UpdateOperationSnapshot {
                id,
                kind: UpdateOperationKind::Install,
                cancelled: true,
            })
        );
        assert!(matches!(
            gate.begin(UpdateOperationKind::Check),
            Err(UpdateOperationError::Quiescing {
                requested: UpdateOperationKind::Check,
            })
        ));
        drop(quiesced);
    });
    assert!(gate.snapshot().is_none());
    assert!(gate.begin(UpdateOperationKind::Check).is_ok());
}

fn telemetry(path: &Path, bytes: &[u8]) -> DownloadTelemetry {
    DownloadTelemetry {
        destination: path.to_path_buf(),
        final_url: Url::parse(
            "https://release-assets.githubusercontent.com/github-production-release-asset/1/test",
        )
        .expect("fixture URL"),
        declared_content_length: Some(u64::try_from(bytes.len()).expect("fixture length")),
        bytes_written: u64::try_from(bytes.len()).expect("fixture length"),
        sha256: Sha256::digest(bytes).into(),
        redirects_followed: 1,
    }
}

fn telemetry_for_file(path: &Path) -> DownloadTelemetry {
    let mut file = std::fs::File::open(path).expect("open fixture file");
    let mut hasher = Sha256::new();
    let mut bytes_written = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).expect("read fixture file");
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        bytes_written += u64::try_from(read).expect("read length");
    }
    DownloadTelemetry {
        destination: path.to_path_buf(),
        final_url: Url::parse(
            "https://release-assets.githubusercontent.com/github-production-release-asset/1/test",
        )
        .expect("fixture URL"),
        declared_content_length: Some(bytes_written),
        bytes_written,
        sha256: hasher.finalize().into(),
        redirects_followed: 1,
    }
}

fn verified_fixture(dir: &TestDir) -> clipline_updater::VerifiedInstaller {
    let path = dir.path().join("installer.part");
    std::fs::write(&path, FIXTURE_BYTES).expect("write fixture");
    verify_download_with_key(
        telemetry(&path, FIXTURE_BYTES),
        FIXTURE_SIGNATURE.trim(),
        "test",
        FIXTURE_KEY,
    )
    .expect("known-good fixture verifies")
}

#[test]
fn known_good_fixture_verifies_and_drop_cleans_the_owned_download() {
    let dir = TestDir::new("clipline-updater", "known-good");
    let verified = verified_fixture(&dir);
    let path = verified.path().to_path_buf();
    assert_eq!(verified.release_filename(), "test");
    assert!(path.exists());
    drop(verified);
    assert!(!path.exists());
}

#[test]
fn tamper_wrong_key_signature_name_and_truncation_fail_before_launch() {
    type InvalidCase = (
        &'static str,
        &'static [u8],
        &'static str,
        &'static str,
        fn(&VerificationError) -> bool,
    );
    let cases: &[InvalidCase] = &[
        (
            "tamper",
            b"tEst\n",
            FIXTURE_SIGNATURE,
            FIXTURE_KEY,
            |error| matches!(error, VerificationError::InvalidSignature),
        ),
        (
            "wrong-key",
            FIXTURE_BYTES,
            FIXTURE_SIGNATURE,
            CLIPLINE_MINISIGN_PUBLIC_KEY,
            |error| matches!(error, VerificationError::InvalidSignature),
        ),
        (
            "wrong-signature",
            FIXTURE_BYTES,
            BAD_SIGNATURE,
            FIXTURE_KEY,
            |error| {
                matches!(
                    error,
                    VerificationError::SignatureEncoding | VerificationError::InvalidSignature
                )
            },
        ),
        (
            "truncated",
            b"tes",
            FIXTURE_SIGNATURE,
            FIXTURE_KEY,
            |error| matches!(error, VerificationError::InvalidSignature),
        ),
    ];

    for (name, bytes, signature, key, expected) in cases {
        let dir = TestDir::new("clipline-updater", name);
        let path = dir.path().join("installer.part");
        std::fs::write(&path, bytes).expect("write case");
        let error =
            verify_download_with_key(telemetry(&path, bytes), signature.trim(), "test", key)
                .expect_err("invalid fixture must fail closed");
        assert!(expected(&error), "unexpected {name} error: {error}");
        assert!(!path.exists(), "failed {name} must clean its owned file");
    }

    for crossed_name in [
        "Clipline_1.0.0_x64-setup.exe",
        "Clipline_1.0.0_x64-standalone-setup.exe",
    ] {
        let dir = TestDir::new("clipline-updater", crossed_name);
        let path = dir.path().join("installer.part");
        std::fs::write(&path, FIXTURE_BYTES).expect("write named case");
        let error = verify_download_with_key(
            telemetry(&path, FIXTURE_BYTES),
            FIXTURE_SIGNATURE.trim(),
            crossed_name,
            FIXTURE_KEY,
        )
        .expect_err("renamed or crossed variant must fail");
        assert!(matches!(error, VerificationError::FilenameMismatch));
        assert!(!path.exists());
    }
}

#[test]
fn production_entry_point_uses_the_embedded_clipline_key() {
    let dir = TestDir::new("clipline-updater", "production-key");
    let path = dir.path().join("installer.part");
    std::fs::write(&path, FIXTURE_BYTES).expect("write fixture");
    let error = verify_download(
        telemetry(&path, FIXTURE_BYTES),
        FIXTURE_SIGNATURE.trim(),
        "test",
    )
    .expect_err("a different release key must fail");
    assert!(matches!(error, VerificationError::InvalidSignature));
    assert!(!path.exists());
}

#[test]
fn current_production_release_verifies_when_external_oracles_are_available() {
    let (Some(manifest_path), Some(installer_path)) = (
        std::env::var_os("CLIPLINE_UPDATER_PRODUCTION_MANIFEST"),
        std::env::var_os("CLIPLINE_UPDATER_PRODUCTION_INSTALLER"),
    ) else {
        eprintln!("SKIP: production updater oracle paths are not configured");
        return;
    };
    let manifest_bytes = std::fs::read(manifest_path).expect("read production manifest");
    let policy = UpdatePolicy::new(
        semver::Version::new(0, 1, 42),
        UpdateChannel::Nightly,
        UpdateVariant::Regular,
    );
    let manifest = parse_update_manifest(&manifest_bytes, &policy).expect("production manifest");
    let dir = TestDir::new("clipline-updater", "production-oracle");
    let copy = dir.path().join("production-installer.part");
    std::fs::copy(installer_path, &copy).expect("copy production installer");
    let filename = installer_filename(&manifest.version, policy.variant);
    let verified = verify_download(
        telemetry_for_file(&copy),
        &manifest.target.signature,
        &filename,
    )
    .expect("the published installer must verify with Clipline's embedded key");
    assert_eq!(verified.release_filename(), filename);
    assert_eq!(verified.telemetry().bytes_written, 54_315_070);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FakeError(&'static str);

impl fmt::Display for FakeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for FakeError {}

#[derive(Clone)]
struct FakeLauncher {
    log: Rc<RefCell<Vec<&'static str>>>,
    fail_prepare: bool,
    fail_commit: bool,
}

struct FakePrepared {
    log: Rc<RefCell<Vec<&'static str>>>,
    fail_commit: bool,
    committed: bool,
}

impl Drop for FakePrepared {
    fn drop(&mut self) {
        if !self.committed {
            self.log.borrow_mut().push("abort-installer");
        }
    }
}

impl PreparedInstallerHandoff for FakePrepared {
    type Receipt = u32;
    type Error = FakeError;

    fn commit(mut self) -> Result<Self::Receipt, Self::Error> {
        self.log.borrow_mut().push("launch-installer");
        if self.fail_commit {
            return Err(FakeError("commit"));
        }
        self.committed = true;
        Ok(42)
    }
}

impl InstallerLauncher for FakeLauncher {
    type Prepared = FakePrepared;

    fn prepare(
        &mut self,
        installer: clipline_updater::VerifiedInstaller,
    ) -> Result<Self::Prepared, FakeError> {
        self.log.borrow_mut().push("prepare-installer");
        if self.fail_prepare {
            return Err(FakeError("prepare"));
        }
        drop(installer);
        Ok(FakePrepared {
            log: self.log.clone(),
            fail_commit: self.fail_commit,
            committed: false,
        })
    }
}

struct FakeShutdown {
    log: Rc<RefCell<Vec<&'static str>>>,
    fail_at: Option<&'static str>,
}

impl FakeShutdown {
    fn step(&mut self, name: &'static str) -> Result<(), FakeError> {
        self.log.borrow_mut().push(name);
        if self.fail_at == Some(name) {
            Err(FakeError(name))
        } else {
            Ok(())
        }
    }
}

impl UpdateShutdown for FakeShutdown {
    type Error = FakeError;

    fn publish_durable_state(&mut self) -> Result<(), Self::Error> {
        self.step("durable")
    }

    fn stop_window_media(&mut self) -> Result<(), Self::Error> {
        self.step("window-media")
    }

    fn stop_recorder(&mut self) -> Result<(), Self::Error> {
        self.step("recorder")
    }

    fn flush_diagnostics(&mut self) -> Result<(), Self::Error> {
        self.step("diagnostics")
    }

    fn request_exit(&mut self) -> Result<(), Self::Error> {
        self.step("exit")
    }
}

#[test]
fn verified_install_orders_preparation_shutdown_launch_and_exit() {
    let dir = TestDir::new("clipline-updater", "install-order");
    let log = Rc::new(RefCell::new(Vec::new()));
    let mut launcher = FakeLauncher {
        log: log.clone(),
        fail_prepare: false,
        fail_commit: false,
    };
    let mut shutdown = FakeShutdown {
        log: log.clone(),
        fail_at: None,
    };
    let receipt = install_verified(&mut launcher, &mut shutdown, verified_fixture(&dir))
        .expect("install flow");
    assert_eq!(receipt, 42);
    assert_eq!(
        *log.borrow(),
        [
            "prepare-installer",
            "durable",
            "window-media",
            "recorder",
            "diagnostics",
            "launch-installer",
            "exit",
        ]
    );
}

#[test]
fn launch_or_service_failure_never_requests_exit() {
    for (name, fail_prepare, fail_at, expected) in [
        ("launch-failure", true, None, vec!["prepare-installer"]),
        (
            "service-failure",
            false,
            Some("recorder"),
            vec![
                "prepare-installer",
                "durable",
                "window-media",
                "recorder",
                "abort-installer",
            ],
        ),
    ] {
        let dir = TestDir::new("clipline-updater", name);
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut launcher = FakeLauncher {
            log: log.clone(),
            fail_prepare,
            fail_commit: false,
        };
        let mut shutdown = FakeShutdown {
            log: log.clone(),
            fail_at,
        };
        install_verified(&mut launcher, &mut shutdown, verified_fixture(&dir))
            .expect_err("failure stays in app");
        assert_eq!(*log.borrow(), expected);
        assert!(!log.borrow().contains(&"exit"));
    }
}
