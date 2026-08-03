use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clipline_library::{
    catalog_result_channel, local_clip_id_for_source, ActiveFileRegistry, CatalogEffect,
    CatalogOperationOwner, CatalogResult, CatalogRevision, ClipDetailRequest, ClipPathIdentity,
    CloudAccountGeneration, CloudAccountKey, DurableUploadToken, ExpectedResultOwner,
    ForegroundGeneration, LegacyAudioTrackProbe, LocalLibraryRepository, PlaybackSourceLease,
    RequestGeneration, ResolvedLocalClip, UploadGeneration, ValidatedClipPath,
    WindowAttachmentGeneration, WindowWorkToken, ACTIVE_UPLOAD_MUTATION_ERROR,
    MAX_FOREGROUND_MESSAGE_BYTES,
};
use clipline_slint_spike::catalog::{
    CatalogEffectExecutor, CatalogEffectHandler, CatalogResultWake, CatalogRevealPort,
    CatalogReviewPort, LocalCatalogEffectHandler,
};
use clipline_test_utils::TestDir;

#[derive(Debug, Clone, Copy)]
struct NoAudio;

impl LegacyAudioTrackProbe for NoAudio {
    fn audio_track_count(&self, _clip_path: &Path) -> Result<usize, String> {
        Ok(0)
    }
}

#[derive(Default)]
struct RecordingReveal(Mutex<Vec<PathBuf>>);

impl RecordingReveal {
    fn targets(&self) -> Vec<PathBuf> {
        self.0.lock().unwrap().clone()
    }
}

impl CatalogRevealPort for RecordingReveal {
    fn reveal(&self, target: &Path) -> Result<(), String> {
        self.0.lock().unwrap().push(target.to_path_buf());
        Ok(())
    }
}

#[derive(Default)]
struct RecordingReview(Mutex<Option<(WindowWorkToken, PathBuf, PlaybackSourceLease)>>);

impl RecordingReview {
    fn take(&self) -> Option<(WindowWorkToken, PathBuf, PlaybackSourceLease)> {
        self.0.lock().unwrap().take()
    }
}

impl CatalogReviewPort for RecordingReview {
    fn open(
        &self,
        token: WindowWorkToken,
        source: ValidatedClipPath,
        lease: PlaybackSourceLease,
    ) -> Result<(), String> {
        *self.0.lock().unwrap() = Some((token, source.canonical_path().to_path_buf(), lease));
        Ok(())
    }
}

#[derive(Default)]
struct NoopWake;

impl CatalogResultWake for NoopWake {
    fn wake(&self) {}
}

fn window(request: u64) -> WindowWorkToken {
    WindowWorkToken {
        attachment: WindowAttachmentGeneration::new(11),
        foreground: ForegroundGeneration::new(12),
        request: RequestGeneration::new(request),
    }
}

fn resolved(path: &Path) -> ResolvedLocalClip {
    let text = path.display().to_string();
    ResolvedLocalClip::new(ClipPathIdentity::from_text(&text).unwrap(), text).unwrap()
}

fn fixture(name: &str) -> (TestDir, PathBuf) {
    let directory = TestDir::new("clipline-slint-catalog-local", name);
    let root = directory.path().join("media");
    std::fs::create_dir_all(&root).unwrap();
    (directory, root)
}

#[test]
fn refresh_and_detail_publish_exact_bounded_owners() {
    let (_directory, root) = fixture("refresh-detail");
    let clip = root.join("one.mp4");
    std::fs::write(&clip, b"fixture").unwrap();
    let handler = LocalCatalogEffectHandler::open_with_ports(
        &root,
        ActiveFileRegistry::new(),
        Arc::new(NoAudio),
        Arc::new(RecordingReveal::default()),
    )
    .unwrap();
    let token = window(13);
    let revision = CatalogRevision::new(14);

    let completion = handler
        .execute(CatalogEffect::RefreshLocal { token, revision })
        .unwrap()
        .unwrap();
    assert_eq!(completion.expected, ExpectedResultOwner::Window(token));
    match completion.result {
        CatalogResult::LocalIndex(index) => {
            assert_eq!(index.token, token);
            assert_eq!(index.revision, revision);
            assert!(!index.truncated);
            assert_eq!(index.items.len(), 1);
            assert_eq!(index.items[0].path, clip.display().to_string());
            assert!(index.items[0].file_identity.is_some());
        }
        other => panic!("expected local index, got {other:?}"),
    }

    let target = resolved(&clip);
    let request = ClipDetailRequest::new(target.identity.clone(), token);
    let completion = handler
        .execute(CatalogEffect::LoadClipDetail {
            token,
            request: request.clone(),
            target,
            title: "One".into(),
            description: "Detail".into(),
        })
        .unwrap()
        .unwrap();
    assert_eq!(
        completion.expected,
        ExpectedResultOwner::Detail(request.owner().clone())
    );
    match completion.result {
        CatalogResult::ClipDetail(detail) => {
            assert!(detail.matches_request(&request));
            assert_eq!(detail.detail().upload().title(), "One");
        }
        other => panic!("expected clip detail, got {other:?}"),
    }
}

#[test]
fn opening_review_transfers_the_exact_playback_lease_until_session_release() {
    let (_directory, root) = fixture("review-lease");
    let clip = root.join("review.mp4");
    std::fs::write(&clip, b"fixture").unwrap();
    let registry = ActiveFileRegistry::new();
    let review = Arc::new(RecordingReview::default());
    let handler = LocalCatalogEffectHandler::open_with_review_port(
        &root,
        registry,
        Arc::new(NoAudio),
        Arc::new(RecordingReveal::default()),
        review.clone(),
    )
    .unwrap();
    let token = window(19);
    let target = resolved(&clip);

    assert!(handler
        .execute(CatalogEffect::OpenLocalReview {
            token,
            target: target.clone(),
        })
        .unwrap()
        .is_none());

    let (actual_token, actual_path, lease) = review.take().expect("review owns the source lease");
    assert_eq!(actual_token, token);
    assert_eq!(actual_path, clip.canonicalize().unwrap());
    assert_eq!(lease.owner(), token);
    assert!(handler
        .execute(CatalogEffect::RenameTitle {
            token,
            target: target.clone(),
            title: "blocked while playing".into(),
        })
        .err()
        .expect("playback lease blocks mutation")
        .contains(ACTIVE_UPLOAD_MUTATION_ERROR));

    drop(lease);
    assert!(handler
        .execute(CatalogEffect::RenameTitle {
            token,
            target,
            title: "released".into(),
        })
        .unwrap()
        .is_some());
}

#[test]
fn stale_catalog_identity_cannot_delete_a_replacement_at_the_same_path() {
    let (_directory, root) = fixture("same-path-replacement");
    let clip = root.join("replace-me.mp4");
    std::fs::write(&clip, b"original").unwrap();
    let repository = LocalLibraryRepository::open(&root).unwrap();
    let original = repository
        .validate_clip_path(&clip.display().to_string())
        .unwrap();
    let target = ResolvedLocalClip::with_file_identity(
        original.comparison_identity().clone(),
        original.display_path(),
        Some(original.file_identity()),
    )
    .unwrap();
    std::fs::remove_file(&clip).unwrap();
    std::fs::write(&clip, b"replacement").unwrap();

    let handler = LocalCatalogEffectHandler::open_with_ports(
        &root,
        ActiveFileRegistry::new(),
        Arc::new(NoAudio),
        Arc::new(RecordingReveal::default()),
    )
    .unwrap();
    let token = window(25);
    let completion = handler
        .execute(CatalogEffect::Delete {
            token,
            targets: vec![target],
        })
        .unwrap()
        .unwrap();
    match completion.result {
        CatalogResult::DeleteCompleted { report, .. } => {
            assert!(report.deleted.is_empty());
            assert_eq!(report.failed.len(), 1);
            assert!(report.failed[0].1.contains("replaced"));
        }
        other => panic!("expected delete completion, got {other:?}"),
    }
    assert_eq!(std::fs::read(&clip).unwrap(), b"replacement");
}

#[test]
fn rename_reveal_and_delete_revalidate_the_repository_target() {
    let (_directory, root) = fixture("mutations");
    let original = root.join("original.mp4");
    std::fs::write(&original, b"fixture").unwrap();
    let reveal = Arc::new(RecordingReveal::default());
    let handler = LocalCatalogEffectHandler::open_with_ports(
        &root,
        ActiveFileRegistry::new(),
        Arc::new(NoAudio),
        reveal.clone(),
    )
    .unwrap();
    let token = window(21);

    let completion = handler
        .execute(CatalogEffect::RenameTitle {
            token,
            target: resolved(&original),
            title: "Named clip".into(),
        })
        .unwrap()
        .unwrap();
    assert_eq!(completion.expected, ExpectedResultOwner::Window(token));
    assert!(matches!(
        completion.result,
        CatalogResult::RenameCompleted { .. }
    ));

    let completion = handler
        .execute(CatalogEffect::RenameFile {
            token,
            target: resolved(&original),
            file_name: "renamed.mp4".into(),
        })
        .unwrap()
        .unwrap();
    let renamed = match completion.result {
        CatalogResult::RenameCompleted {
            token: actual,
            result,
        } => {
            assert_eq!(actual, token);
            result
        }
        other => panic!("expected rename completion, got {other:?}"),
    };
    let renamed_path = PathBuf::from(&renamed.path);
    assert!(renamed_path.is_file());

    assert!(handler
        .execute(CatalogEffect::Reveal {
            token,
            target: resolved(&renamed_path),
        })
        .unwrap()
        .is_none());
    assert_eq!(reveal.targets(), vec![renamed_path.canonicalize().unwrap()]);

    let missing_path = root.join("missing.mp4");
    let completion = handler
        .execute(CatalogEffect::Delete {
            token,
            targets: vec![resolved(&renamed_path), resolved(&missing_path)],
        })
        .unwrap()
        .unwrap();
    assert_eq!(completion.expected, ExpectedResultOwner::Window(token));
    match completion.result {
        CatalogResult::DeleteCompleted {
            token: actual,
            report,
        } => {
            assert_eq!(actual, token);
            assert_eq!(report.deleted, vec![renamed.path]);
            assert_eq!(report.failed.len(), 1);
            assert_eq!(report.failed[0].0, missing_path.display().to_string());
        }
        other => panic!("expected delete completion, got {other:?}"),
    }
    assert!(!renamed_path.exists());
}

#[test]
fn shared_upload_registry_blocks_mutation_and_executor_bounds_the_exact_failure() {
    let (_directory, root) = fixture("shared-registry");
    let clip = root.join("leased.mp4");
    std::fs::write(&clip, b"fixture").unwrap();
    let registry = ActiveFileRegistry::new();
    let source = LocalLibraryRepository::open(&root)
        .unwrap()
        .validate_clip_path(&clip.display().to_string())
        .unwrap();
    let upload_token = DurableUploadToken {
        account_key: CloudAccountKey::new("account").unwrap(),
        account_generation: CloudAccountGeneration::new(1),
        upload_generation: UploadGeneration::new(2),
        local_clip_id: local_clip_id_for_source(source.file_identity()),
        source_path: source.comparison_identity().clone(),
    };
    let _lease = registry.acquire_upload(&source, upload_token).unwrap();
    let handler = Arc::new(
        LocalCatalogEffectHandler::open_with_ports(
            &root,
            registry,
            Arc::new(NoAudio),
            Arc::new(RecordingReveal::default()),
        )
        .unwrap(),
    );
    let (sender, receiver) = catalog_result_channel();
    let executor = CatalogEffectExecutor::start(handler, sender, Arc::new(NoopWake)).unwrap();
    let token = window(31);
    let target = resolved(&clip);
    executor
        .try_submit(CatalogEffect::RenameTitle {
            token,
            target: target.clone(),
            title: "blocked".into(),
        })
        .unwrap();

    let result = receiver.wait_recv(Duration::from_secs(2)).unwrap().unwrap();
    match result {
        CatalogResult::OperationFailed { owner, message } => {
            assert_eq!(
                owner,
                CatalogOperationOwner::RenameTitle {
                    token,
                    target: target.identity,
                }
            );
            assert!(message.contains(ACTIVE_UPLOAD_MUTATION_ERROR));
            assert!(message.len() <= MAX_FOREGROUND_MESSAGE_BYTES);
        }
        other => panic!("expected exact operation failure, got {other:?}"),
    }
    executor.shutdown().unwrap();
}
