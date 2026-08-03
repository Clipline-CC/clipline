use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use clipline_shell::FileMutationFence;
use clipline_shell::{file_identity, open_regular_file_nofollow, replace_file_if_identities};

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(case: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "clipline-shell-{case}-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn identity_checked_replace_publishes_synced_sibling_and_removes_old_target() {
    let directory = TestDirectory::new("checked-replace");
    let target = directory.path().join("metadata.json");
    let replacement = directory.path().join("metadata.tmp");
    std::fs::write(&target, b"old").unwrap();
    std::fs::write(&replacement, b"new").unwrap();
    let target_identity = file_identity(&target).unwrap();
    let replacement_identity = file_identity(&replacement).unwrap();

    replace_file_if_identities(&replacement, replacement_identity, &target, target_identity)
        .unwrap();

    assert_eq!(std::fs::read(&target).unwrap(), b"new");
    assert!(!replacement.exists());
    assert!(std::fs::read_dir(directory.path()).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".clipline-replace-old-")
    }));
}

#[test]
fn identity_checked_replace_preserves_a_target_changed_after_validation() {
    let directory = TestDirectory::new("checked-replace-race");
    let target = directory.path().join("metadata.json");
    let replacement = directory.path().join("metadata.tmp");
    let foreign = directory.path().join("foreign.tmp");
    std::fs::write(&target, b"old").unwrap();
    std::fs::write(&replacement, b"new").unwrap();
    std::fs::write(&foreign, b"foreign").unwrap();
    let selected_target_identity = file_identity(&target).unwrap();
    let replacement_identity = file_identity(&replacement).unwrap();
    std::fs::remove_file(&target).unwrap();
    std::fs::rename(&foreign, &target).unwrap();
    assert_ne!(file_identity(&target).unwrap(), selected_target_identity);

    assert!(replace_file_if_identities(
        &replacement,
        replacement_identity,
        &target,
        selected_target_identity,
    )
    .is_err());

    assert_eq!(std::fs::read(&target).unwrap(), b"foreign");
    assert_eq!(std::fs::read(&replacement).unwrap(), b"new");
}

#[test]
fn nofollow_open_rejects_a_final_file_link() {
    let directory = TestDirectory::new("nofollow-link");
    let target = directory.path().join("target.json");
    let link = directory.path().join("link.json");
    std::fs::write(&target, b"outside").unwrap();

    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, &link).unwrap();
    #[cfg(windows)]
    if std::os::windows::fs::symlink_file(&target, &link).is_err() {
        return;
    }

    assert!(open_regular_file_nofollow(&link).is_err());
    assert_eq!(std::fs::read(target).unwrap(), b"outside");
}

#[cfg(unix)]
#[test]
fn fenced_rename_rejects_a_source_link_swapped_in_after_acquisition() {
    let directory = TestDirectory::new("fenced-rename-source-link-race");
    let source = directory.path().join("selected.mp4");
    let destination = directory.path().join("renamed.mp4");
    let outside = directory.path().join("foreign.mp4");
    std::fs::write(&source, b"selected").unwrap();
    std::fs::write(&outside, b"foreign").unwrap();
    let mut fence = FileMutationFence::acquire(
        &source,
        file_identity(&source).unwrap(),
        file_identity(directory.path()).unwrap(),
    )
    .unwrap();
    std::fs::remove_file(&source).unwrap();
    std::os::unix::fs::symlink(&outside, &source).unwrap();

    let error = fence.rename_noreplace(&destination).unwrap_err();

    assert!(!error.may_have_moved());
    assert!(source.symlink_metadata().unwrap().file_type().is_symlink());
    assert!(!destination.exists());
    assert_eq!(std::fs::read(outside).unwrap(), b"foreign");
}

#[cfg(unix)]
#[test]
fn fenced_delete_rejects_a_source_link_swapped_in_after_acquisition() {
    let directory = TestDirectory::new("fenced-delete-source-link-race");
    let source = directory.path().join("selected.mp4");
    let outside = directory.path().join("foreign.mp4");
    std::fs::write(&source, b"selected").unwrap();
    std::fs::write(&outside, b"foreign").unwrap();
    let mut fence = FileMutationFence::acquire(
        &source,
        file_identity(&source).unwrap(),
        file_identity(directory.path()).unwrap(),
    )
    .unwrap();
    std::fs::remove_file(&source).unwrap();
    std::os::unix::fs::symlink(&outside, &source).unwrap();

    let error = fence.delete().unwrap_err();

    assert!(!error.may_have_moved());
    assert!(source.symlink_metadata().unwrap().file_type().is_symlink());
    assert_eq!(std::fs::read(outside).unwrap(), b"foreign");
    assert!(std::fs::read_dir(directory.path()).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".clipline-delete-")
    }));
}
