use std::io::Write;
use std::sync::Arc;

use clipline_library::{
    client_clip_id_for_payload, clip_sidecar_paths, local_clip_id_for_source, ActiveFileRegistry,
    CloudAccountGeneration, CloudAccountKey, DurableUploadToken, ForegroundGeneration, LocalClipId,
    LocalLibraryRepository, MutationLease, OwnedUploadTemp, RequestGeneration,
    StandardRepositoryFileSystem, UploadGeneration, UploadOwnershipError,
    WindowAttachmentGeneration, WindowWorkToken, ACTIVE_UPLOAD_MUTATION_ERROR,
};
use clipline_test_utils::TestDir;

fn token(
    source: &clipline_library::ValidatedClipPath,
    account: &str,
    account_generation: u64,
    upload_generation: u64,
    local_clip_id: &str,
) -> DurableUploadToken {
    DurableUploadToken {
        account_key: CloudAccountKey::new(account).unwrap(),
        account_generation: CloudAccountGeneration::new(account_generation),
        upload_generation: UploadGeneration::new(upload_generation),
        local_clip_id: LocalClipId::new(local_clip_id).unwrap(),
        source_path: source.comparison_identity().clone(),
    }
}

fn fixture(
    name: &str,
) -> (
    TestDir,
    ActiveFileRegistry,
    LocalLibraryRepository,
    clipline_library::ValidatedClipPath,
) {
    let directory = TestDir::new("clipline-upload-source", name);
    let root = directory.path().join("media");
    std::fs::create_dir_all(&root).unwrap();
    let clip = root.join("clip.mp4");
    std::fs::write(&clip, b"original mp4 bytes").unwrap();
    let registry = ActiveFileRegistry::new();
    let repository = LocalLibraryRepository::with_seams(
        &root,
        Arc::new(StandardRepositoryFileSystem),
        Arc::new(registry.clone()),
    )
    .unwrap();
    let validated = repository
        .validate_clip_path(&clip.display().to_string())
        .unwrap();
    (directory, registry, repository, validated)
}

#[test]
fn source_and_payload_identities_are_split_before_preparation() {
    let (directory, _registry, repository, source) = fixture("split-identities");
    let stable = local_clip_id_for_source(source.file_identity());

    let alias = directory.path().join("media").join("alias.mp4");
    std::fs::hard_link(source.canonical_path(), &alias).unwrap();
    let alias = repository
        .validate_clip_path(&alias.display().to_string())
        .unwrap();
    assert_eq!(stable, local_clip_id_for_source(alias.file_identity()));

    let first_payload = client_clip_id_for_payload(
        &stable,
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    )
    .unwrap();
    assert_eq!(
        first_payload,
        client_clip_id_for_payload(
            &stable,
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        )
        .unwrap()
    );
    assert_ne!(
        first_payload,
        client_clip_id_for_payload(
            &stable,
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        )
        .unwrap()
    );
    assert!(client_clip_id_for_payload(&stable, "not-a-sha256").is_err());

    std::fs::remove_file(source.canonical_path()).unwrap();
    std::fs::write(source.canonical_path(), b"replacement mp4 bytes").unwrap();
    let replacement = repository
        .validate_clip_path(source.canonical_path().to_string_lossy().as_ref())
        .unwrap();
    assert_ne!(
        stable,
        local_clip_id_for_source(replacement.file_identity())
    );
}

fn mutation_error(result: Result<Box<dyn clipline_library::MutationPermit>, String>) -> String {
    match result {
        Ok(_) => panic!("mutation unexpectedly acquired an active upload source"),
        Err(error) => error,
    }
}

#[test]
fn exact_upload_token_is_current_only_for_its_registered_generation() {
    let (_directory, registry, _repository, source) = fixture("exact-token");
    let current = token(&source, "account-a", 7, 11, "local-1");
    let stale_generation = token(&source, "account-a", 7, 10, "local-1");

    let lease = registry.acquire_upload(&source, current.clone()).unwrap();
    assert!(registry.is_current(&current));
    assert!(registry.is_identity_active(source.file_identity()));
    assert!(!registry.is_current(&stale_generation));
    drop(lease);
    assert!(!registry.is_current(&current));
    assert!(!registry.is_identity_active(source.file_identity()));
}

#[test]
fn token_source_identity_must_match_the_validated_original() {
    let directory = TestDir::new("clipline-upload-source", "token-source");
    let root = directory.path().join("media");
    std::fs::create_dir_all(&root).unwrap();
    let first = root.join("first.mp4");
    let second = root.join("second.mp4");
    std::fs::write(&first, b"first").unwrap();
    std::fs::write(&second, b"second").unwrap();
    let registry = ActiveFileRegistry::new();
    let repository = LocalLibraryRepository::with_seams(
        &root,
        Arc::new(StandardRepositoryFileSystem),
        Arc::new(registry.clone()),
    )
    .unwrap();
    let first = repository
        .validate_clip_path(&first.display().to_string())
        .unwrap();
    let second = repository
        .validate_clip_path(&second.display().to_string())
        .unwrap();
    let wrong = token(&first, "account-a", 1, 1, "local-1");

    assert_eq!(
        registry.acquire_upload(&second, wrong).unwrap_err(),
        UploadOwnershipError::SourceTokenMismatch
    );
}

#[test]
fn same_account_generation_and_local_id_is_rejected_before_a_second_lease() {
    let (_directory, registry, _repository, source) = fixture("duplicate-owner");
    let first = token(&source, "account-a", 3, 4, "local-1");
    let newer = token(&source, "account-a", 3, 5, "local-1");

    let lease = registry.acquire_upload(&source, first).unwrap();
    assert_eq!(
        registry.acquire_upload(&source, newer).unwrap_err(),
        UploadOwnershipError::DuplicateUpload
    );
    drop(lease);
}

#[test]
fn two_distinct_upload_readers_refcount_one_file_identity() {
    let (_directory, registry, _repository, source) = fixture("reader-refcount");
    let first = registry
        .acquire_upload(&source, token(&source, "account-a", 1, 1, "local-a"))
        .unwrap();
    let second = registry
        .acquire_upload(&source, token(&source, "account-a", 1, 2, "local-b"))
        .unwrap();

    assert_eq!(
        mutation_error(registry.acquire(source.canonical_path(), source.file_identity())),
        ACTIVE_UPLOAD_MUTATION_ERROR
    );
    drop(first);
    assert_eq!(
        mutation_error(registry.acquire(source.canonical_path(), source.file_identity())),
        ACTIVE_UPLOAD_MUTATION_ERROR
    );
    drop(second);
    let mutation = registry
        .acquire(source.canonical_path(), source.file_identity())
        .unwrap();
    drop(mutation);
}

#[test]
fn playback_reader_blocks_mutation_until_the_live_source_lease_drops() {
    let (_directory, registry, _repository, source) = fixture("playback-reader");
    let owner = WindowWorkToken {
        attachment: WindowAttachmentGeneration::new(7),
        foreground: ForegroundGeneration::new(8),
        request: RequestGeneration::new(9),
    };
    let playback = registry.acquire_playback(&source, owner).unwrap();
    assert_eq!(playback.owner(), owner);
    assert_eq!(playback.identity(), source.file_identity());
    assert!(registry.is_identity_active(source.file_identity()));
    assert_eq!(
        registry.acquire_playback(&source, owner).unwrap_err(),
        UploadOwnershipError::DuplicatePlayback
    );
    assert_eq!(
        mutation_error(registry.acquire(source.canonical_path(), source.file_identity())),
        ACTIVE_UPLOAD_MUTATION_ERROR
    );
    drop(playback);
    assert!(!registry.is_identity_active(source.file_identity()));
    let mutation = registry
        .acquire(source.canonical_path(), source.file_identity())
        .unwrap();
    drop(mutation);
}

#[test]
fn hard_link_alias_cannot_bypass_the_upload_registry() {
    let (directory, registry, _repository, source) = fixture("hard-link");
    let alias = directory.path().join("media").join("alias.mp4");
    std::fs::hard_link(source.canonical_path(), &alias).unwrap();
    let alias_identity = clipline_shell::file_identity(&alias).unwrap();

    let lease = registry
        .acquire_upload(&source, token(&source, "account-a", 1, 1, "local-a"))
        .unwrap();
    assert_eq!(alias_identity, source.file_identity());
    assert_eq!(
        mutation_error(registry.acquire(&alias, alias_identity)),
        ACTIVE_UPLOAD_MUTATION_ERROR
    );
    drop(lease);
    let mutation = registry.acquire(&alias, alias_identity).unwrap();
    drop(mutation);
}

#[test]
fn mutation_and_upload_admission_are_atomic_for_one_identity() {
    let (_directory, registry, _repository, source) = fixture("mutation-first");
    let mutation = registry
        .acquire(source.canonical_path(), source.file_identity())
        .unwrap();

    assert_eq!(
        registry
            .acquire_upload(&source, token(&source, "account-a", 1, 1, "local-a"))
            .unwrap_err(),
        UploadOwnershipError::MutationActive
    );
    drop(mutation);
    let upload = registry
        .acquire_upload(&source, token(&source, "account-a", 1, 1, "local-a"))
        .unwrap();
    drop(upload);
}

#[test]
fn repository_mutations_observe_the_shared_upload_owner() {
    let (_directory, registry, repository, source) = fixture("repository-integration");
    let lease = registry
        .acquire_upload(&source, token(&source, "account-a", 1, 1, "local-a"))
        .unwrap();

    assert_eq!(
        repository.delete(&source).unwrap_err().to_string(),
        ACTIVE_UPLOAD_MUTATION_ERROR
    );
    assert!(source.canonical_path().exists());
    drop(lease);
    repository.delete(&source).unwrap();
    assert!(!source.canonical_path().exists());
}

#[test]
fn delete_transition_remains_exclusive_while_closing_the_reader_handle() {
    let (_directory, registry, _repository, source) = fixture("delete-transition");
    let upload_token = token(&source, "account-a", 1, 1, "local-a");
    let lease = registry
        .acquire_upload(&source, upload_token.clone())
        .unwrap();
    let delete = lease.into_delete_permit().unwrap();

    assert!(registry.is_current(&upload_token));
    assert_eq!(
        mutation_error(registry.acquire(source.canonical_path(), source.file_identity())),
        ACTIVE_UPLOAD_MUTATION_ERROR
    );
    delete.delete_source_if_current().unwrap();
    assert!(!source.canonical_path().exists());
    drop(delete);
    assert!(!registry.is_current(&upload_token));

    let mutation = registry
        .acquire(source.canonical_path(), source.file_identity())
        .unwrap();
    drop(mutation);
}

#[test]
fn upload_delete_uses_repository_preflight_for_primary_and_all_sidecars() {
    let (_directory, registry, repository, source) = fixture("delete-sidecars");
    let sidecars = clip_sidecar_paths(source.canonical_path()).into_array();
    for (index, sidecar) in sidecars.iter().enumerate() {
        std::fs::write(sidecar, format!("sidecar-{index}")).unwrap();
    }
    let upload_token = token(&source, "account-a", 1, 1, "local-a");
    let lease = registry
        .acquire_upload(&source, upload_token.clone())
        .unwrap();
    let delete = lease.into_delete_permit().unwrap();

    delete
        .delete_clip_and_sidecars_if_current(&repository)
        .unwrap();

    assert!(!source.canonical_path().exists());
    assert!(sidecars.iter().all(|sidecar| !sidecar.exists()));
    assert!(registry.is_current(&upload_token));
    drop(delete);
    assert!(!registry.is_current(&upload_token));
}

#[test]
fn upload_delete_preserves_a_replacement_and_its_existing_sidecars() {
    let (_directory, registry, repository, source) = fixture("delete-replaced");
    let sidecars = clip_sidecar_paths(source.canonical_path()).into_array();
    for (index, sidecar) in sidecars.iter().enumerate() {
        std::fs::write(sidecar, format!("sidecar-{index}")).unwrap();
    }
    let lease = registry
        .acquire_upload(&source, token(&source, "account-a", 1, 1, "local-a"))
        .unwrap();
    let delete = lease.into_delete_permit().unwrap();
    std::fs::remove_file(source.canonical_path()).unwrap();
    std::fs::write(source.canonical_path(), b"foreign replacement").unwrap();

    let error = delete
        .delete_clip_and_sidecars_if_current(&repository)
        .unwrap_err();

    assert!(error.to_string().to_ascii_lowercase().contains("changed"));
    assert_eq!(
        std::fs::read(source.canonical_path()).unwrap(),
        b"foreign replacement"
    );
    assert!(sidecars.iter().all(|sidecar| sidecar.exists()));
}

#[test]
fn delete_transition_rejects_other_readers_without_releasing_them() {
    let (_directory, registry, _repository, source) = fixture("delete-readers");
    let first = registry
        .acquire_upload(&source, token(&source, "account-a", 1, 1, "local-a"))
        .unwrap();
    let second = registry
        .acquire_upload(&source, token(&source, "account-a", 1, 2, "local-b"))
        .unwrap();

    assert_eq!(
        first.into_delete_permit().unwrap_err(),
        UploadOwnershipError::OtherReadersActive
    );
    assert_eq!(
        mutation_error(registry.acquire(source.canonical_path(), source.file_identity())),
        ACTIVE_UPLOAD_MUTATION_ERROR
    );
    let delete = second.into_delete_permit().unwrap();
    drop(delete);
}

#[cfg(windows)]
#[test]
fn windows_source_handle_denies_foreign_write_and_delete_until_drop() {
    let (_directory, registry, _repository, source) = fixture("windows-kernel-lease");
    let lease = registry
        .acquire_upload(&source, token(&source, "account-a", 1, 1, "local-a"))
        .unwrap();

    assert!(std::fs::OpenOptions::new()
        .write(true)
        .open(source.canonical_path())
        .is_err());
    assert!(std::fs::remove_file(source.canonical_path()).is_err());
    drop(lease);
    std::fs::OpenOptions::new()
        .write(true)
        .open(source.canonical_path())
        .unwrap();
}

#[test]
fn owned_upload_temp_drop_removes_only_the_exact_created_file() {
    let directory = TestDir::new("clipline-upload-source", "owned-temp");
    let source = directory.path().join("clip.mp4");
    std::fs::write(&source, b"source").unwrap();
    let mut temp = OwnedUploadTemp::create_near(&source).unwrap();
    let path = temp.path().to_path_buf();
    temp.file_mut().unwrap().write_all(b"payload").unwrap();
    temp.seal().unwrap();
    temp.verify_current().unwrap();
    drop(temp);

    assert!(!path.exists());
    assert_eq!(std::fs::read(&source).unwrap(), b"source");
}

#[test]
fn owned_upload_temp_preserves_a_foreign_replacement() {
    let directory = TestDir::new("clipline-upload-source", "owned-temp-replacement");
    let source = directory.path().join("clip.mp4");
    std::fs::write(&source, b"source").unwrap();
    let mut temp = OwnedUploadTemp::create_near(&source).unwrap();
    let path = temp.path().to_path_buf();
    temp.file_mut().unwrap().write_all(b"payload").unwrap();
    temp.seal().unwrap();

    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, b"foreign replacement").unwrap();
    assert_eq!(
        temp.verify_current().unwrap_err(),
        UploadOwnershipError::SourceChanged
    );
    drop(temp);

    assert_eq!(std::fs::read(&path).unwrap(), b"foreign replacement");
}
