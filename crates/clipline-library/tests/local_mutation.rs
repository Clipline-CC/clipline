use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use clipline_library::{
    clip_sidecar_paths, compatibility_clip_kind, compatibility_clip_title,
    inferred_clip_kind_for_path, normalized_clip_file_name, normalized_clip_title,
    CreateNewFileError, FileSystemEntry, LocalLibraryRepository, MutationLease, MutationPermit,
    NoActiveMutationLease, OsuEnrichmentStatus, OsuPendingEnrichment, PlatformEffect,
    RepositoryFileSystem, RepositoryMutationFence, StandardRepositoryFileSystem,
    ACTIVE_UPLOAD_MUTATION_ERROR,
};
use clipline_shell::ReplacementRecoveryReport;
use clipline_shell::{FileIdentity, FileMutationError};
use clipline_test_utils::TestDir;

#[test]
fn normalized_title_preserves_the_shipping_validation_contract() {
    assert_eq!(
        normalized_clip_title("  Ranked win  ").unwrap(),
        "Ranked win"
    );
    assert_eq!(
        normalized_clip_title("   ").unwrap_err(),
        "clip name cannot be empty"
    );
    assert_eq!(
        normalized_clip_title("bad\nname").unwrap_err(),
        "clip name contains a control character"
    );
}

#[test]
fn normalized_file_name_adds_mp4_and_preserves_valid_text() {
    assert_eq!(
        normalized_clip_file_name("Ranked win").unwrap(),
        "Ranked win.mp4"
    );
    assert_eq!(
        normalized_clip_file_name("Ranked win.Mp4").unwrap(),
        "Ranked win.mp4"
    );
    assert_eq!(
        normalized_clip_file_name("solo.queue.vod").unwrap(),
        "solo.queue.vod.mp4"
    );
}

#[test]
fn normalized_file_name_rejects_paths_reserved_names_and_invalid_chars() {
    let cases = [
        ("", "clip name cannot be empty"),
        ("..", "clip name cannot be empty"),
        ("folder/clip", "clip name cannot contain folders"),
        (r"folder\clip", "clip name cannot contain folders"),
        (
            "bad:name",
            "clip name contains a character Windows cannot use in filenames",
        ),
        (
            "clip?",
            "clip name contains a character Windows cannot use in filenames",
        ),
        ("clip.", "clip name cannot end with a dot or space"),
        ("CON", "clip name is reserved by Windows"),
        ("LPT1.mp4", "clip name is reserved by Windows"),
    ];

    for (name, expected) in cases {
        assert_eq!(
            normalized_clip_file_name(name).unwrap_err(),
            expected,
            "unexpected error for {name:?}"
        );
    }
}

#[test]
fn inferred_kind_only_matches_generated_filename_patterns() {
    assert_eq!(
        inferred_clip_kind_for_path(Path::new("trimming-practice.mp4")),
        "replay"
    );
    assert_eq!(
        inferred_clip_kind_for_path(Path::new("obsession.mp4")),
        "replay"
    );
    assert_eq!(
        inferred_clip_kind_for_path(Path::new("clip_1_trim_001000_002000.mp4")),
        "trim"
    );
    assert_eq!(
        inferred_clip_kind_for_path(Path::new("session_1781377615.mp4")),
        "session"
    );
}

fn touch(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

fn pending(clip: &Path) -> OsuPendingEnrichment {
    OsuPendingEnrichment {
        schema_version: 1,
        clip_path: clip.display().to_string(),
        recording_start_unix: 10,
        recording_end_unix: 20,
        clip_duration_s: 10.0,
        status: OsuEnrichmentStatus::Pending,
        attempts: 0,
        pagination_ceiling_reached: false,
        title_events: Vec::new(),
        message: None,
    }
}

fn standard_repository(root: &Path) -> LocalLibraryRepository {
    LocalLibraryRepository::open(root).unwrap()
}

#[test]
fn validation_accepts_only_root_or_one_direct_session_mp4() {
    let directory = TestDir::new("clipline-library", "repository-path-acceptance");
    let root = directory.path().join("media");
    let root_clip = root.join("root.mp4");
    let session_clip = root.join("2026-08-02").join("session.mp4");
    let deep_clip = root.join("a").join("b").join("deep.mp4");
    let upper_extension = root.join("upper.MP4");
    let text = root.join("not-video.txt");
    let outside = directory.path().join("outside.mp4");
    for path in [
        &root_clip,
        &session_clip,
        &deep_clip,
        &upper_extension,
        &text,
        &outside,
    ] {
        touch(path, b"clip");
    }

    let repository = standard_repository(&root);
    let root_validated = repository
        .validate_clip_path(&root_clip.display().to_string())
        .unwrap();
    assert_eq!(
        root_validated.display_path(),
        root_clip.display().to_string()
    );
    assert_eq!(
        root_validated.canonical_path(),
        root_clip.canonicalize().unwrap()
    );
    assert!(repository
        .validate_clip_path(&session_clip.display().to_string())
        .is_ok());

    for rejected in [&deep_clip, &upper_extension, &text, &outside] {
        assert_eq!(
            repository
                .validate_clip_path(&rejected.display().to_string())
                .unwrap_err()
                .to_string(),
            "refusing to access a clip outside the clips directory"
        );
    }
}

#[test]
fn validation_rejects_a_symlink_escape_when_the_platform_can_create_one() {
    let directory = TestDir::new("clipline-library", "repository-symlink-escape");
    let root = directory.path().join("media");
    let outside = directory.path().join("outside").join("escaped.mp4");
    touch(&outside, b"outside");
    std::fs::create_dir_all(&root).unwrap();
    let link = root.join("escaped.mp4");

    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    #[cfg(windows)]
    if std::os::windows::fs::symlink_file(&outside, &link).is_err() {
        return;
    }

    assert_eq!(
        standard_repository(&root)
            .validate_clip_path(&link.display().to_string())
            .unwrap_err()
            .to_string(),
        "refusing to access a clip outside the clips directory"
    );
}

#[test]
fn validation_rejects_an_internal_file_symlink_alias() {
    let directory = TestDir::new("clipline-library", "repository-symlink-alias");
    let root = directory.path().join("media");
    let target = root.join("target.mp4");
    let alias = root.join("alias.mp4");
    touch(&target, b"target");
    if !create_file_symlink(&target, &alias) {
        return;
    }

    assert_eq!(
        standard_repository(&root)
            .validate_clip_path(&alias.display().to_string())
            .unwrap_err()
            .to_string(),
        "refusing to access a clip outside the clips directory"
    );
    assert_eq!(std::fs::read(target).unwrap(), b"target");
}

fn create_file_symlink(target: &Path, link: &Path) -> bool {
    create_file_symlink_result(target, link).is_ok()
}

fn create_file_symlink_result(target: &Path, link: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_file(target, link)
    }
}

#[test]
fn validation_rejects_an_internal_session_directory_link() {
    let directory = TestDir::new("clipline-library", "repository-session-link-alias");
    let root = directory.path().join("media");
    let session = root.join("session");
    let clip = session.join("clip.mp4");
    let alias = root.join("alias");
    touch(&clip, b"target");

    #[cfg(unix)]
    std::os::unix::fs::symlink(&session, &alias).unwrap();
    #[cfg(windows)]
    if std::os::windows::fs::symlink_dir(&session, &alias).is_err() {
        return;
    }

    assert_eq!(
        standard_repository(&root)
            .validate_clip_path(&alias.join("clip.mp4").display().to_string())
            .unwrap_err()
            .to_string(),
        "refusing to access a clip outside the clips directory"
    );
    assert_eq!(std::fs::read(clip).unwrap(), b"target");
}

#[test]
fn rename_title_and_delete_fail_closed_on_owned_sidecar_links() {
    for operation in ["rename", "title", "delete"] {
        let directory = TestDir::new("clipline-library", &format!("sidecar-link-{operation}"));
        let root = directory.path().join("media");
        let clip = root.join("session_1.mp4");
        let outside = directory.path().join("outside.txt");
        touch(&clip, b"mp4");
        touch(&outside, b"outside bytes");
        let sidecars = clip_sidecar_paths(&clip);
        let linked_sidecar = match operation {
            "rename" => sidecars.markers,
            "title" => sidecars.metadata,
            "delete" => sidecars.poster,
            _ => unreachable!(),
        };
        if !create_file_symlink(&outside, &linked_sidecar) {
            return;
        }
        let repository = standard_repository(&root);
        let validated = repository
            .validate_clip_path(&clip.display().to_string())
            .unwrap();

        let error = match operation {
            "rename" => repository.rename_file(&validated, "Renamed").unwrap_err(),
            "title" => repository.rename_title(&validated, "Title").unwrap_err(),
            "delete" => repository.delete(&validated).unwrap_err(),
            _ => unreachable!(),
        };

        assert!(
            error.to_string().contains("untrusted clip sidecar"),
            "{error}"
        );
        assert_eq!(std::fs::read(&clip).unwrap(), b"mp4");
        assert_eq!(std::fs::read(&outside).unwrap(), b"outside bytes");
        assert!(linked_sidecar.exists());
        assert!(!root.join("Renamed.mp4").exists());
        assert_no_repository_temps(&root);
    }
}

#[test]
fn mutation_revalidates_file_identity_immediately_before_use() {
    let directory = TestDir::new("clipline-library", "repository-identity-swap");
    let root = directory.path().join("media");
    let clip = root.join("session_1.mp4");
    touch(&clip, b"first identity");
    let repository = standard_repository(&root);
    let validated = repository
        .validate_clip_path(&clip.display().to_string())
        .unwrap();

    std::fs::remove_file(&clip).unwrap();
    touch(&clip, b"replacement identity");

    assert_eq!(
        repository
            .rename_file(&validated, "renamed")
            .unwrap_err()
            .to_string(),
        "clip changed since it was selected; refresh the Library and try again"
    );
    assert_eq!(std::fs::read(&clip).unwrap(), b"replacement identity");
    assert!(!root.join("renamed.mp4").exists());

    let delete_clip = root.join("delete.mp4");
    touch(&delete_clip, b"first delete identity");
    let validated_delete = repository
        .validate_clip_path(&delete_clip.display().to_string())
        .unwrap();
    std::fs::remove_file(&delete_clip).unwrap();
    touch(&delete_clip, b"replacement delete identity");
    assert_eq!(
        repository
            .delete(&validated_delete)
            .unwrap_err()
            .to_string(),
        "clip changed since it was selected; refresh the Library and try again"
    );
    assert_eq!(
        std::fs::read(delete_clip).unwrap(),
        b"replacement delete identity"
    );
}

fn write_five_file_clip(source: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let paths = clip_sidecar_paths(source);
    let files = vec![
        (source.to_path_buf(), b"mp4".to_vec()),
        (paths.markers, b"markers".to_vec()),
        (
            paths.metadata,
            br#"{"title":"Ranked win","kind":"session"}"#.to_vec(),
        ),
        (
            paths.pending_osu,
            serde_json::to_vec_pretty(&pending(source)).unwrap(),
        ),
        (paths.poster, b"poster".to_vec()),
    ];
    for (path, bytes) in &files {
        touch(path, bytes);
    }
    files
}

#[test]
fn file_rename_moves_all_five_owned_files_and_rewrites_pending_path() {
    let directory = TestDir::new("clipline-library", "repository-five-file-rename");
    let root = directory.path().join("media");
    let source = root.join("session_1.mp4");
    let target = root.join("Ranked win.mp4");
    let originals = write_five_file_clip(&source);
    let repository = standard_repository(&root);
    let validated = repository
        .validate_clip_path(&source.display().to_string())
        .unwrap();

    let renamed = repository.rename_file(&validated, "Ranked win").unwrap();

    assert_eq!(renamed.old_path, source.display().to_string());
    assert_eq!(renamed.path, target.display().to_string());
    assert_eq!(renamed.name, "Ranked win.mp4");
    assert_eq!(renamed.title.as_deref(), Some("Ranked win"));
    assert_eq!(renamed.kind, "session");
    for (source_path, _) in originals {
        assert!(!source_path.exists(), "source remains: {source_path:?}");
    }
    let target_sidecars = clip_sidecar_paths(&target);
    assert_eq!(std::fs::read(&target).unwrap(), b"mp4");
    assert_eq!(std::fs::read(target_sidecars.markers).unwrap(), b"markers");
    assert_eq!(std::fs::read(target_sidecars.poster).unwrap(), b"poster");
    let moved: OsuPendingEnrichment =
        serde_json::from_slice(&std::fs::read(target_sidecars.pending_osu).unwrap()).unwrap();
    assert_eq!(moved.clip_path, target.display().to_string());
    assert_no_repository_temps(&root);
}

#[cfg(windows)]
#[test]
fn case_only_same_object_rename_moves_every_owned_file() {
    let directory = TestDir::new("clipline-library", "repository-case-only");
    let root = directory.path().join("media");
    let source = root.join("session_1.mp4");
    let target = root.join("Session_1.mp4");
    write_five_file_clip(&source);
    let repository = standard_repository(&root);
    let validated = repository
        .validate_clip_path(&source.display().to_string())
        .unwrap();

    let renamed = repository.rename_file(&validated, "Session_1").unwrap();

    assert_eq!(renamed.path, target.display().to_string());
    let names: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    for expected in [
        "Session_1.mp4",
        "Session_1.markers.json",
        "Session_1.clipline.json",
        "Session_1.osu-enrichment.json",
        "Session_1.poster.jpg",
    ] {
        assert!(
            names.iter().any(|name| name == expected),
            "missing {expected}"
        );
    }
    assert_no_repository_temps(&root);
}

#[test]
fn every_owned_destination_collision_is_rejected_without_mutation() {
    let cases = [
        ("mp4", "a clip with that name already exists"),
        ("markers", "a marker sidecar with that name already exists"),
        (
            "metadata",
            "a clip metadata sidecar with that name already exists",
        ),
        (
            "pending",
            "an osu! enrichment sidecar with that name already exists",
        ),
        ("poster", "a poster sidecar with that name already exists"),
    ];
    for (kind, expected) in cases {
        let directory = TestDir::new("clipline-library", &format!("collision-{kind}"));
        let root = directory.path().join("media");
        let source = root.join("session_1.mp4");
        let target = root.join("Taken.mp4");
        let originals = write_five_file_clip(&source);
        let target_sidecars = clip_sidecar_paths(&target);
        let collision = match kind {
            "mp4" => target.clone(),
            "markers" => target_sidecars.markers,
            "metadata" => target_sidecars.metadata,
            "pending" => target_sidecars.pending_osu,
            "poster" => target_sidecars.poster,
            _ => unreachable!(),
        };
        touch(&collision, b"occupied");
        let repository = standard_repository(&root);
        let validated = repository
            .validate_clip_path(&source.display().to_string())
            .unwrap();

        assert_eq!(
            repository
                .rename_file(&validated, "Taken")
                .unwrap_err()
                .to_string(),
            expected
        );
        for (path, bytes) in originals {
            assert_eq!(std::fs::read(path).unwrap(), bytes);
        }
        assert_eq!(std::fs::read(collision).unwrap(), b"occupied");
        assert!(!target.exists() || kind == "mp4");
        assert_no_repository_temps(&root);
    }
}

#[test]
fn orphan_destination_sidecars_are_collisions_even_when_the_source_sidecar_is_absent() {
    let cases = [
        ("markers", "a marker sidecar with that name already exists"),
        (
            "metadata",
            "a clip metadata sidecar with that name already exists",
        ),
        (
            "pending",
            "an osu! enrichment sidecar with that name already exists",
        ),
        ("poster", "a poster sidecar with that name already exists"),
    ];
    for (kind, expected) in cases {
        let directory = TestDir::new("clipline-library", &format!("orphan-collision-{kind}"));
        let root = directory.path().join("media");
        let source = root.join("session_1.mp4");
        let target = root.join("Taken.mp4");
        touch(&source, b"source");
        let target_sidecars = clip_sidecar_paths(&target);
        let collision = match kind {
            "markers" => target_sidecars.markers,
            "metadata" => target_sidecars.metadata,
            "pending" => target_sidecars.pending_osu,
            "poster" => target_sidecars.poster,
            _ => unreachable!(),
        };
        touch(&collision, b"unrelated");
        let repository = standard_repository(&root);
        let validated = repository
            .validate_clip_path(&source.display().to_string())
            .unwrap();

        assert_eq!(
            repository
                .rename_file(&validated, "Taken")
                .unwrap_err()
                .to_string(),
            expected
        );
        assert_eq!(std::fs::read(&source).unwrap(), b"source");
        assert_eq!(std::fs::read(&collision).unwrap(), b"unrelated");
        assert_no_repository_temps(&root);
    }
}

#[test]
fn a_differently_named_hard_link_is_a_collision_not_a_case_alias() {
    let directory = TestDir::new("clipline-library", "hard-link-collision");
    let root = directory.path().join("media");
    let source = root.join("session_1.mp4");
    let target = root.join("Alias.mp4");
    touch(&source, b"source");
    std::fs::hard_link(&source, &target).unwrap();
    let repository = standard_repository(&root);
    let validated = repository
        .validate_clip_path(&source.display().to_string())
        .unwrap();

    assert_eq!(
        repository
            .rename_file(&validated, "Alias")
            .unwrap_err()
            .to_string(),
        "a clip with that name already exists"
    );
    assert_eq!(std::fs::read(&source).unwrap(), b"source");
    assert_eq!(std::fs::read(&target).unwrap(), b"source");
}

#[derive(Clone, Debug)]
struct FailureRule {
    operation: &'static str,
    from_name: Option<String>,
    to_name: Option<String>,
}

type CollisionHook = Arc<Mutex<Option<(String, Vec<u8>)>>>;

#[derive(Clone, Default)]
struct FaultFileSystem {
    standard: StandardRepositoryFileSystem,
    failures: Arc<Mutex<Vec<FailureRule>>>,
    log: Arc<Mutex<Vec<String>>>,
    required_permit: Option<Arc<AtomicBool>>,
    create_collision: CollisionHook,
    create_after_error: CollisionHook,
    rename_collision: CollisionHook,
    replace_collision: CollisionHook,
    replace_before_fence: Arc<Mutex<Option<Vec<u8>>>>,
    read_link_swap: Arc<Mutex<Option<PathBuf>>>,
    read_file_swap: CollisionHook,
    rename_source_swap: CollisionHook,
    remove_source_swap: CollisionHook,
    fail_after_move: Arc<Mutex<Option<String>>>,
}

impl FaultFileSystem {
    fn fail(&self, operation: &'static str, from: Option<&str>, to: Option<&str>) {
        self.failures.lock().unwrap().push(FailureRule {
            operation,
            from_name: from.map(str::to_owned),
            to_name: to.map(str::to_owned),
        });
    }

    fn collide_create(&self, name_fragment: &str, bytes: &[u8]) {
        *self.create_collision.lock().unwrap() = Some((name_fragment.to_owned(), bytes.to_vec()));
    }

    fn fail_create_after(&self, name_fragment: &str, bytes: &[u8]) {
        *self.create_after_error.lock().unwrap() = Some((name_fragment.to_owned(), bytes.to_vec()));
    }

    fn collide_rename(&self, target_name: &str, bytes: &[u8]) {
        *self.rename_collision.lock().unwrap() = Some((target_name.to_owned(), bytes.to_vec()));
    }

    fn collide_replace(&self, target_name: &str, bytes: &[u8]) {
        *self.replace_collision.lock().unwrap() = Some((target_name.to_owned(), bytes.to_vec()));
    }

    fn replace_before_fence(&self, bytes: &[u8]) {
        *self.replace_before_fence.lock().unwrap() = Some(bytes.to_vec());
    }

    fn swap_read_to_link(&self, target: &Path) {
        *self.read_link_swap.lock().unwrap() = Some(target.to_path_buf());
    }

    fn swap_read_file(&self, source_name: &str, bytes: &[u8]) {
        *self.read_file_swap.lock().unwrap() = Some((source_name.to_owned(), bytes.to_vec()));
    }

    fn swap_rename_source(&self, source_name: &str, bytes: &[u8]) {
        *self.rename_source_swap.lock().unwrap() = Some((source_name.to_owned(), bytes.to_vec()));
    }

    fn swap_remove_source(&self, source_name: &str, bytes: &[u8]) {
        *self.remove_source_swap.lock().unwrap() = Some((source_name.to_owned(), bytes.to_vec()));
    }

    fn fail_after_move(&self, source_name: &str) {
        *self.fail_after_move.lock().unwrap() = Some(source_name.to_owned());
    }

    fn take_fail_after_move(&self, source: &Path) -> bool {
        let mut hook = self.fail_after_move.lock().unwrap();
        let Some(name) = hook.take() else {
            return false;
        };
        if source
            .file_name()
            .is_some_and(|source| name_matches(&name, &source.to_string_lossy()))
        {
            true
        } else {
            *hook = Some(name);
            false
        }
    }

    fn operation(&self, operation: &'static str, from: &Path, to: Option<&Path>) -> io::Result<()> {
        if self
            .required_permit
            .as_ref()
            .is_some_and(|held| !held.load(Ordering::SeqCst))
        {
            return Err(io::Error::other("mutation ran without its permit"));
        }
        let from_name = from.file_name().unwrap().to_string_lossy().to_string();
        let to_name = to.map(|path| path.file_name().unwrap().to_string_lossy().to_string());
        self.log.lock().unwrap().push(format!(
            "{operation}:{from_name}->{}",
            to_name.as_deref().unwrap_or("")
        ));
        let mut failures = self.failures.lock().unwrap();
        if let Some(index) = failures.iter().position(|rule| {
            rule.operation == operation
                && rule
                    .from_name
                    .as_deref()
                    .is_none_or(|pattern| name_matches(pattern, &from_name))
                && rule.to_name.as_deref().is_none_or(|pattern| {
                    to_name
                        .as_deref()
                        .is_some_and(|name| name_matches(pattern, name))
                })
        }) {
            let rule = failures.remove(index);
            return Err(io::Error::other(format!("injected {rule:?}")));
        }
        Ok(())
    }
}

fn name_matches(pattern: &str, name: &str) -> bool {
    pattern
        .strip_prefix('*')
        .map_or_else(|| pattern == name, |fragment| name.contains(fragment))
}

impl RepositoryFileSystem for FaultFileSystem {
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        self.standard.canonicalize(path)
    }

    fn is_directory(&self, path: &Path) -> io::Result<bool> {
        self.standard.is_directory(path)
    }

    fn entry(&self, path: &Path) -> io::Result<FileSystemEntry> {
        self.standard.entry(path)
    }

    fn try_exists(&self, path: &Path) -> io::Result<bool> {
        self.standard.try_exists(path)
    }

    fn recover_pending_replacements(
        &self,
        root: &Path,
        expected_root_identity: FileIdentity,
    ) -> io::Result<ReplacementRecoveryReport> {
        self.standard
            .recover_pending_replacements(root, expected_root_identity)
    }

    fn read_bounded_if_identity(
        &self,
        path: &Path,
        expected_identity: FileIdentity,
        maximum_bytes: u64,
    ) -> io::Result<Vec<u8>> {
        if let Some(target) = self.read_link_swap.lock().unwrap().take() {
            std::fs::remove_file(path)?;
            create_file_symlink_result(&target, path)?;
        }
        if let Some((name, winner)) = self.read_file_swap.lock().unwrap().take() {
            if path
                .file_name()
                .is_some_and(|source| name_matches(&name, &source.to_string_lossy()))
            {
                std::fs::remove_file(path)?;
                std::fs::write(path, winner)?;
            } else {
                *self.read_file_swap.lock().unwrap() = Some((name, winner));
            }
        }
        self.standard
            .read_bounded_if_identity(path, expected_identity, maximum_bytes)
    }

    fn create_new_synced(
        &self,
        path: &Path,
        bytes: &[u8],
    ) -> Result<FileIdentity, CreateNewFileError> {
        self.operation("create", path, None)
            .map_err(CreateNewFileError::before_create)?;
        let create_after_error = self.create_after_error.lock().unwrap().take();
        if let Some((fragment, partial)) = create_after_error {
            if path.to_string_lossy().contains(&fragment) {
                let identity = self.standard.create_new_synced(path, &partial)?;
                return Err(CreateNewFileError::after_create(
                    io::Error::other("injected failure after create"),
                    Some(identity),
                ));
            }
            *self.create_after_error.lock().unwrap() = Some((fragment, partial));
        }
        let create_collision = self.create_collision.lock().unwrap().take();
        if let Some((fragment, winner)) = create_collision {
            if path.to_string_lossy().contains(&fragment) {
                std::fs::write(path, winner).unwrap();
                return Err(CreateNewFileError::before_create(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "injected create-new collision",
                )));
            }
            *self.create_collision.lock().unwrap() = Some((fragment, winner));
        }
        self.standard.create_new_synced(path, bytes)
    }

    fn acquire_mutation_fence(
        &self,
        path: &Path,
        source_identity: FileIdentity,
        parent_identity: FileIdentity,
    ) -> io::Result<Box<dyn RepositoryMutationFence>> {
        if let Some(replacement) = self.replace_before_fence.lock().unwrap().take() {
            std::fs::remove_file(path)?;
            std::fs::write(path, replacement)?;
        }
        let inner = self
            .standard
            .acquire_mutation_fence(path, source_identity, parent_identity)?;
        Ok(Box::new(FaultMutationFence {
            file_system: self.clone(),
            inner,
            current_path: path.to_path_buf(),
        }))
    }

    fn rename_noreplace_if_identity(
        &self,
        from: &Path,
        to: &Path,
        identity: FileIdentity,
    ) -> Result<(), FileMutationError> {
        self.operation("rename", from, Some(to))
            .map_err(FileMutationError::unchanged)?;
        let source_swap = self.rename_source_swap.lock().unwrap().take();
        if let Some((name, winner)) = source_swap {
            if from
                .file_name()
                .is_some_and(|source| name_matches(&name, &source.to_string_lossy()))
            {
                std::fs::remove_file(from).map_err(FileMutationError::unchanged)?;
                std::fs::write(from, winner).map_err(FileMutationError::unchanged)?;
            } else {
                *self.rename_source_swap.lock().unwrap() = Some((name, winner));
            }
        }
        let rename_collision = self.rename_collision.lock().unwrap().take();
        if let Some((name, winner)) = rename_collision {
            if to.file_name().is_some_and(|target| target == name.as_str()) {
                std::fs::write(to, winner).unwrap();
            } else {
                *self.rename_collision.lock().unwrap() = Some((name, winner));
            }
        }
        self.standard
            .rename_noreplace_if_identity(from, to, identity)?;
        if self.take_fail_after_move(from) {
            return Err(FileMutationError::target_or_unknown(
                io::Error::other("injected failure after committed move"),
                to,
            ));
        }
        Ok(())
    }

    fn replace_if_identities(
        &self,
        from: &Path,
        from_identity: FileIdentity,
        to: &Path,
        to_identity: FileIdentity,
    ) -> Result<(), FileMutationError> {
        self.operation("replace", from, Some(to))
            .map_err(FileMutationError::unchanged)?;
        let replace_collision = self.replace_collision.lock().unwrap().take();
        if let Some((name, winner)) = replace_collision {
            if to.file_name().is_some_and(|target| target == name.as_str()) {
                std::fs::remove_file(to).map_err(FileMutationError::unchanged)?;
                std::fs::write(to, winner).map_err(FileMutationError::unchanged)?;
            } else {
                *self.replace_collision.lock().unwrap() = Some((name, winner));
            }
        }
        self.standard
            .replace_if_identities(from, from_identity, to, to_identity)
    }

    fn remove_file_if_identity(
        &self,
        path: &Path,
        identity: FileIdentity,
    ) -> Result<(), FileMutationError> {
        self.operation("remove", path, None)
            .map_err(FileMutationError::unchanged)?;
        let source_swap = self.remove_source_swap.lock().unwrap().take();
        if let Some((name, winner)) = source_swap {
            if path
                .file_name()
                .is_some_and(|source| name_matches(&name, &source.to_string_lossy()))
            {
                std::fs::remove_file(path).map_err(FileMutationError::unchanged)?;
                std::fs::write(path, winner).map_err(FileMutationError::unchanged)?;
            } else {
                *self.remove_source_swap.lock().unwrap() = Some((name, winner));
            }
        }
        self.standard.remove_file_if_identity(path, identity)
    }
}

struct FaultMutationFence {
    file_system: FaultFileSystem,
    inner: Box<dyn RepositoryMutationFence>,
    current_path: PathBuf,
}

impl RepositoryMutationFence for FaultMutationFence {
    fn rename_noreplace(&mut self, target: &Path) -> Result<(), FileMutationError> {
        let source = self.current_path.clone();
        self.file_system
            .operation("rename", &self.current_path, Some(target))
            .map_err(FileMutationError::unchanged)?;
        self.inner.rename_noreplace(target)?;
        self.current_path = target.to_path_buf();
        if self.file_system.take_fail_after_move(&source) {
            return Err(FileMutationError::target_or_unknown(
                io::Error::other("injected failure after committed move"),
                target,
            ));
        }
        Ok(())
    }

    fn delete(&mut self) -> Result<(), FileMutationError> {
        self.file_system
            .operation("remove", &self.current_path, None)
            .map_err(FileMutationError::unchanged)?;
        self.inner.delete()
    }
}

fn fault_repository(
    root: &Path,
    file_system: FaultFileSystem,
    lease: Arc<dyn MutationLease>,
) -> LocalLibraryRepository {
    LocalLibraryRepository::with_seams(root, Arc::new(file_system), lease).unwrap()
}

fn assert_no_repository_temps(root: &Path) {
    fn walk(path: &Path, found: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, found);
            } else if path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .contains(".clipline-")
            {
                found.push(path);
            }
        }
    }
    let mut found = Vec::new();
    walk(root, &mut found);
    assert!(found.is_empty(), "repository temp files remain: {found:?}");
}

#[test]
fn every_injected_forward_move_failure_rolls_back_exact_bytes_without_temps() {
    let failures = [
        ("session_1.mp4", "Renamed.mp4"),
        ("session_1.markers.json", "Renamed.markers.json"),
        ("session_1.clipline.json", "Renamed.clipline.json"),
        (
            "session_1.osu-enrichment.json",
            "session_1.osu-enrichment.clipline-rename-backup",
        ),
        (
            "Renamed.osu-enrichment.clipline-rename-tmp",
            "Renamed.osu-enrichment.json",
        ),
        ("session_1.poster.jpg", "Renamed.poster.jpg"),
    ];
    for (from, to) in failures {
        let directory = TestDir::new("clipline-library", &format!("forward-failure-{from}"));
        let root = directory.path().join("media");
        let source = root.join("session_1.mp4");
        let originals = write_five_file_clip(&source);
        let file_system = FaultFileSystem::default();
        file_system.fail("rename", Some(from), Some(to));
        let repository = fault_repository(&root, file_system, Arc::new(NoActiveMutationLease));
        let validated = repository
            .validate_clip_path(&source.display().to_string())
            .unwrap();

        assert!(
            repository.rename_file(&validated, "Renamed").is_err(),
            "failure rule was not exercised: {from} -> {to}"
        );
        for (path, bytes) in originals {
            assert_eq!(std::fs::read(&path).unwrap(), bytes, "changed: {path:?}");
        }
        assert!(!root.join("Renamed.mp4").exists());
        assert_no_repository_temps(&root);
    }
}

#[test]
fn metadata_publish_and_backup_cleanup_failures_also_roll_back_exactly() {
    let failures = [
        FailureRule {
            operation: "create",
            from_name: Some("*clipline-write".to_string()),
            to_name: None,
        },
        FailureRule {
            operation: "replace",
            from_name: Some("*clipline-write".to_string()),
            to_name: Some("Renamed.clipline.json".to_string()),
        },
        FailureRule {
            operation: "remove",
            from_name: Some("session_1.osu-enrichment.clipline-rename-backup".to_string()),
            to_name: None,
        },
    ];
    for rule in failures {
        let directory = TestDir::new(
            "clipline-library",
            &format!("publish-failure-{}", rule.operation),
        );
        let root = directory.path().join("media");
        let source = root.join("session_1.mp4");
        let originals = write_five_file_clip(&source);
        let file_system = FaultFileSystem::default();
        file_system.failures.lock().unwrap().push(rule);
        let repository = fault_repository(&root, file_system, Arc::new(NoActiveMutationLease));
        let validated = repository
            .validate_clip_path(&source.display().to_string())
            .unwrap();

        assert!(repository.rename_file(&validated, "Renamed").is_err());
        for (path, bytes) in originals {
            assert_eq!(std::fs::read(&path).unwrap(), bytes, "changed: {path:?}");
        }
        assert!(!root.join("Renamed.mp4").exists());
        assert_no_repository_temps(&root);
    }
}

#[test]
fn pending_restore_after_create_failure_never_installs_partial_bytes() {
    let directory = TestDir::new("clipline-library", "pending-restore-after-create");
    let root = directory.path().join("media");
    let source = root.join("session_1.mp4");
    let originals = write_five_file_clip(&source);
    let pending_path = clip_sidecar_paths(&source).pending_osu;
    let file_system = FaultFileSystem::default();
    file_system.fail(
        "replace",
        Some("*clipline-write"),
        Some("Renamed.clipline.json"),
    );
    file_system.fail_create_after(
        "session_1.osu-enrichment.json.clipline-write-",
        b"partial pending bytes",
    );
    let repository = fault_repository(&root, file_system, Arc::new(NoActiveMutationLease));
    let validated = repository
        .validate_clip_path(&source.display().to_string())
        .unwrap();

    let error = repository.rename_file(&validated, "Renamed").unwrap_err();

    assert!(
        !pending_path.exists(),
        "unexpected restored bytes: {:?}",
        std::fs::read(&pending_path).ok()
    );
    assert!(
        error
            .rollback_failures()
            .iter()
            .any(|failure| failure.contains("recreate original osu! enrichment sidecar")),
        "{error}"
    );
    for (path, bytes) in originals {
        if path != pending_path {
            assert_eq!(std::fs::read(path).unwrap(), bytes);
        }
    }
    assert_no_repository_temps(&root);
}

#[test]
fn pending_restore_collision_never_installs_or_deletes_foreign_bytes() {
    let directory = TestDir::new("clipline-library", "pending-restore-collision");
    let root = directory.path().join("media");
    let source = root.join("session_1.mp4");
    let originals = write_five_file_clip(&source);
    let pending_path = clip_sidecar_paths(&source).pending_osu;
    let file_system = FaultFileSystem::default();
    file_system.fail(
        "replace",
        Some("*clipline-write"),
        Some("Renamed.clipline.json"),
    );
    file_system.collide_create(
        "session_1.osu-enrichment.json.clipline-write-",
        b"foreign pending collision",
    );
    let repository = fault_repository(&root, file_system, Arc::new(NoActiveMutationLease));
    let validated = repository
        .validate_clip_path(&source.display().to_string())
        .unwrap();

    let error = repository.rename_file(&validated, "Renamed").unwrap_err();

    assert!(
        !pending_path.exists(),
        "unexpected restored bytes: {:?}",
        std::fs::read(&pending_path).ok()
    );
    assert!(
        error
            .rollback_failures()
            .iter()
            .any(|failure| failure.contains("recreate original osu! enrichment sidecar")),
        "{error}"
    );
    for (path, bytes) in originals {
        if path != pending_path {
            assert_eq!(std::fs::read(path).unwrap(), bytes);
        }
    }
    let winner = std::fs::read_dir(&root)
        .unwrap()
        .map(Result::unwrap)
        .find(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .contains("session_1.osu-enrichment.json.clipline-write-")
        })
        .expect("foreign rollback collision must remain")
        .path();
    assert_eq!(std::fs::read(winner).unwrap(), b"foreign pending collision");
}

#[test]
fn create_new_collision_winners_are_never_removed_as_rollback() {
    let directory = TestDir::new("clipline-library", "create-collision-ownership");
    let root = directory.path().join("media");
    let clip = root.join("session_1.mp4");
    touch(&clip, b"clip");
    let file_system = FaultFileSystem::default();
    file_system.collide_create(".clipline-write-", b"metadata collision winner");
    let repository = fault_repository(&root, file_system.clone(), Arc::new(NoActiveMutationLease));
    let validated = repository
        .validate_clip_path(&clip.display().to_string())
        .unwrap();

    assert!(repository.rename_title(&validated, "Title").is_err());
    let winner = std::fs::read_dir(&root)
        .unwrap()
        .map(Result::unwrap)
        .find(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .contains(".clipline-write-")
        })
        .expect("injected collision winner must remain")
        .path();
    assert_eq!(std::fs::read(winner).unwrap(), b"metadata collision winner");
    assert_eq!(std::fs::read(&clip).unwrap(), b"clip");

    let pending_directory = TestDir::new("clipline-library", "pending-create-collision-ownership");
    let pending_root = pending_directory.path().join("media");
    let pending_clip = pending_root.join("session_1.mp4");
    let originals = write_five_file_clip(&pending_clip);
    let file_system = FaultFileSystem::default();
    file_system.collide_create(
        "Renamed.osu-enrichment.clipline-rename-tmp",
        b"pending collision winner",
    );
    let repository = fault_repository(&pending_root, file_system, Arc::new(NoActiveMutationLease));
    let validated = repository
        .validate_clip_path(&pending_clip.display().to_string())
        .unwrap();

    assert!(repository.rename_file(&validated, "Renamed").is_err());
    for (path, bytes) in originals {
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
    assert_eq!(
        std::fs::read(pending_root.join("Renamed.osu-enrichment.clipline-rename-tmp")).unwrap(),
        b"pending collision winner"
    );
}

#[test]
fn after_create_cleanup_never_removes_a_replacement_of_the_owned_temp() {
    let directory = TestDir::new("clipline-library", "after-create-cleanup-identity");
    let root = directory.path().join("media");
    let clip = root.join("session_1.mp4");
    touch(&clip, b"clip");
    let file_system = FaultFileSystem::default();
    file_system.fail_create_after(
        "session_1.clipline.json.clipline-write-",
        b"partial metadata",
    );
    file_system.swap_remove_source("*clipline-write", b"foreign temp replacement");
    let repository = fault_repository(&root, file_system, Arc::new(NoActiveMutationLease));
    let validated = repository
        .validate_clip_path(&clip.display().to_string())
        .unwrap();

    let error = repository.rename_title(&validated, "Title").unwrap_err();

    assert!(
        error
            .rollback_failures()
            .iter()
            .any(|failure| failure.contains("partial temporary metadata file")),
        "{error}"
    );
    let winner = std::fs::read_dir(&root)
        .unwrap()
        .map(Result::unwrap)
        .find(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .contains("session_1.clipline.json.clipline-write-")
        })
        .expect("foreign replacement must remain")
        .path();
    assert_eq!(std::fs::read(winner).unwrap(), b"foreign temp replacement");
    assert!(!clip_sidecar_paths(&clip).metadata.exists());
    assert_eq!(std::fs::read(clip).unwrap(), b"clip");
}

#[test]
fn rewritten_pending_sidecar_is_rechecked_after_serialization() {
    const LIMIT: usize = 8 * 1024 * 1024;

    let directory = TestDir::new("clipline-library", "pending-rewrite-bound");
    let root = directory.path().join("media");
    let clip = root.join("session_1.mp4");
    touch(&clip, b"clip");
    let mut record = pending(&clip);
    record.message = Some(String::new());
    let overhead = serde_json::to_vec(&record).unwrap().len();
    record.message = Some("x".repeat(LIMIT - overhead - 1));
    let compact = serde_json::to_vec(&record).unwrap();
    assert!(compact.len() < LIMIT);
    touch(&clip_sidecar_paths(&clip).pending_osu, &compact);
    let repository = standard_repository(&root);
    let validated = repository
        .validate_clip_path(&clip.display().to_string())
        .unwrap();

    let error = repository
        .rename_file(&validated, "A much longer renamed clip")
        .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("serialize osu! enrichment sidecar: sidecar exceeds"),
        "{error}"
    );
    assert_eq!(std::fs::read(&clip).unwrap(), b"clip");
    assert!(!root.join("A much longer renamed clip.mp4").exists());
    assert_no_repository_temps(&root);
}

#[test]
fn a_racing_destination_is_preserved_and_the_transaction_rolls_back() {
    let directory = TestDir::new("clipline-library", "rename-no-replace-race");
    let root = directory.path().join("media");
    let clip = root.join("session_1.mp4");
    let originals = write_five_file_clip(&clip);
    let file_system = FaultFileSystem::default();
    file_system.collide_rename("Renamed.markers.json", b"collision winner");
    let repository = fault_repository(&root, file_system, Arc::new(NoActiveMutationLease));
    let validated = repository
        .validate_clip_path(&clip.display().to_string())
        .unwrap();

    assert!(repository.rename_file(&validated, "Renamed").is_err());
    for (path, bytes) in originals {
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
    assert_eq!(
        std::fs::read(root.join("Renamed.markers.json")).unwrap(),
        b"collision winner"
    );
    assert!(!root.join("Renamed.mp4").exists());
    assert_no_repository_temps(&root);
}

#[test]
fn file_rename_never_moves_a_sidecar_replaced_after_preflight() {
    let directory = TestDir::new("clipline-library", "rename-sidecar-source-race");
    let root = directory.path().join("media");
    let clip = root.join("session_1.mp4");
    write_five_file_clip(&clip);
    let source_markers = clip_sidecar_paths(&clip).markers;
    let file_system = FaultFileSystem::default();
    file_system.swap_rename_source("session_1.markers.json", b"foreign markers");
    let repository = fault_repository(&root, file_system, Arc::new(NoActiveMutationLease));
    let validated = repository
        .validate_clip_path(&clip.display().to_string())
        .unwrap();

    assert!(repository.rename_file(&validated, "Renamed").is_err());

    assert_eq!(std::fs::read(&clip).unwrap(), b"mp4");
    assert_eq!(std::fs::read(&source_markers).unwrap(), b"foreign markers");
    assert!(!root.join("Renamed.mp4").exists());
    assert!(!root.join("Renamed.markers.json").exists());
    assert_no_repository_temps(&root);
}

#[test]
fn delete_never_removes_a_sidecar_replaced_after_preflight() {
    let directory = TestDir::new("clipline-library", "delete-sidecar-source-race");
    let root = directory.path().join("media");
    let clip = root.join("session_1.mp4");
    write_five_file_clip(&clip);
    let sidecars = clip_sidecar_paths(&clip);
    let file_system = FaultFileSystem::default();
    file_system.swap_remove_source("session_1.markers.json", b"foreign markers");
    let repository = fault_repository(&root, file_system, Arc::new(NoActiveMutationLease));
    let validated = repository
        .validate_clip_path(&clip.display().to_string())
        .unwrap();

    repository.delete(&validated).unwrap();

    assert!(!clip.exists());
    assert_eq!(
        std::fs::read(&sidecars.markers).unwrap(),
        b"foreign markers"
    );
    assert!(!sidecars.metadata.exists());
    assert!(!sidecars.pending_osu.exists());
    assert!(!sidecars.poster.exists());
}

#[test]
fn mutation_fence_rejects_a_replacement_after_lease_acquisition() {
    let directory = TestDir::new("clipline-library", "fence-identity-swap");
    let root = directory.path().join("media");
    let clip = root.join("session_1.mp4");
    touch(&clip, b"selected");
    let file_system = FaultFileSystem::default();
    file_system.replace_before_fence(b"replacement");
    let repository = fault_repository(&root, file_system, Arc::new(NoActiveMutationLease));
    let validated = repository
        .validate_clip_path(&clip.display().to_string())
        .unwrap();

    assert_eq!(
        repository
            .rename_file(&validated, "Renamed")
            .unwrap_err()
            .to_string(),
        "clip changed since it was selected; refresh the Library and try again"
    );
    assert_eq!(std::fs::read(&clip).unwrap(), b"replacement");
    assert!(!root.join("Renamed.mp4").exists());
}

#[test]
fn bounded_sidecar_read_refuses_a_link_swapped_in_after_entry_validation() {
    let directory = TestDir::new("clipline-library", "sidecar-read-link-race");
    let root = directory.path().join("media");
    let clip = root.join("session_1.mp4");
    write_five_file_clip(&clip);
    let outside = directory.path().join("outside-pending.json");
    touch(&outside, b"foreign outside bytes");
    let probe = root.join("symlink-capability-probe");
    if !create_file_symlink(&outside, &probe) {
        return;
    }
    std::fs::remove_file(&probe).unwrap();

    let file_system = FaultFileSystem::default();
    file_system.swap_read_to_link(&outside);
    let repository = fault_repository(&root, file_system, Arc::new(NoActiveMutationLease));
    let validated = repository
        .validate_clip_path(&clip.display().to_string())
        .unwrap();

    let error = repository.rename_file(&validated, "Renamed").unwrap_err();

    assert!(
        error.to_string().contains("read osu! enrichment sidecar"),
        "{error}"
    );
    assert_eq!(std::fs::read(&clip).unwrap(), b"mp4");
    assert_eq!(std::fs::read(&outside).unwrap(), b"foreign outside bytes");
    assert!(!root.join("Renamed.mp4").exists());
}

#[test]
fn file_rename_never_rewrites_metadata_replaced_after_preflight() {
    let directory = TestDir::new("clipline-library", "metadata-read-identity-race");
    let root = directory.path().join("media");
    let clip = root.join("session_1.mp4");
    write_five_file_clip(&clip);
    let metadata = clip_sidecar_paths(&clip).metadata;
    let file_system = FaultFileSystem::default();
    file_system.swap_read_file("session_1.clipline.json", b"foreign metadata");
    let repository = fault_repository(&root, file_system, Arc::new(NoActiveMutationLease));
    let validated = repository
        .validate_clip_path(&clip.display().to_string())
        .unwrap();

    assert!(repository.rename_file(&validated, "Renamed").is_err());

    assert_eq!(std::fs::read(&clip).unwrap(), b"mp4");
    assert_eq!(std::fs::read(&metadata).unwrap(), b"foreign metadata");
    assert!(!root.join("Renamed.mp4").exists());
    assert!(!root.join("Renamed.clipline.json").exists());
    assert_no_repository_temps(&root);
}

#[test]
fn rollback_failures_are_collected_while_every_reverse_step_is_attempted() {
    let directory = TestDir::new("clipline-library", "rollback-failures");
    let root = directory.path().join("media");
    let source = root.join("session_1.mp4");
    write_five_file_clip(&source);
    let file_system = FaultFileSystem::default();
    file_system.fail(
        "rename",
        Some("session_1.poster.jpg"),
        Some("Renamed.poster.jpg"),
    );
    file_system.fail(
        "rename",
        Some("Renamed.markers.json"),
        Some("session_1.markers.json"),
    );
    file_system.fail("rename", Some("Renamed.mp4"), Some("session_1.mp4"));
    let log = Arc::clone(&file_system.log);
    let repository = fault_repository(&root, file_system, Arc::new(NoActiveMutationLease));
    let validated = repository
        .validate_clip_path(&source.display().to_string())
        .unwrap();

    let error = repository.rename_file(&validated, "Renamed").unwrap_err();

    assert_eq!(error.rollback_failures().len(), 2);
    assert!(error.to_string().contains("rollback failures"));
    let log = log.lock().unwrap().join("\n");
    assert!(log.contains("Renamed.markers.json->session_1.markers.json"));
    assert!(log.contains("Renamed.mp4->session_1.mp4"));
    assert!(log.contains("Renamed.osu-enrichment.json->"));
    assert_no_repository_temps(&root);
}

#[test]
fn a_reported_maybe_committed_primary_move_is_journaled_and_restored() {
    let directory = TestDir::new("clipline-library", "committed-move-restored");
    let root = directory.path().join("media");
    let source = root.join("session_1.mp4");
    let target = root.join("Renamed.mp4");
    write_five_file_clip(&source);
    let file_system = FaultFileSystem::default();
    file_system.fail_after_move("session_1.mp4");
    let repository = fault_repository(&root, file_system, Arc::new(NoActiveMutationLease));
    let validated = repository
        .validate_clip_path(&source.display().to_string())
        .unwrap();

    let error = repository.rename_file(&validated, "Renamed").unwrap_err();

    assert!(error
        .to_string()
        .contains("injected failure after committed move"));
    assert_eq!(std::fs::read(&source).unwrap(), b"mp4");
    assert!(!target.exists());
    assert_no_repository_temps(&root);
}

#[test]
fn a_maybe_committed_move_with_failed_reverse_is_reported_at_its_recovery_path() {
    let directory = TestDir::new("clipline-library", "committed-move-reverse-fails");
    let root = directory.path().join("media");
    let source = root.join("session_1.mp4");
    let target = root.join("Renamed.mp4");
    write_five_file_clip(&source);
    let file_system = FaultFileSystem::default();
    file_system.fail_after_move("session_1.mp4");
    file_system.fail("rename", Some("Renamed.mp4"), Some("session_1.mp4"));
    let repository = fault_repository(&root, file_system, Arc::new(NoActiveMutationLease));
    let validated = repository
        .validate_clip_path(&source.display().to_string())
        .unwrap();

    let error = repository.rename_file(&validated, "Renamed").unwrap_err();

    assert_eq!(error.rollback_failures().len(), 1);
    assert!(error.to_string().contains("rollback failures"));
    assert!(!source.exists());
    assert_eq!(std::fs::read(&target).unwrap(), b"mp4");
}

struct AlwaysActiveLease;

impl MutationLease for AlwaysActiveLease {
    fn acquire(
        &self,
        _path: &Path,
        _identity: FileIdentity,
    ) -> Result<Box<dyn MutationPermit>, String> {
        Err(ACTIVE_UPLOAD_MUTATION_ERROR.to_string())
    }
}

struct TrackingLease {
    held: Arc<AtomicBool>,
    drops: Arc<AtomicUsize>,
}

struct TrackingPermit {
    held: Arc<AtomicBool>,
    drops: Arc<AtomicUsize>,
}

impl MutationPermit for TrackingPermit {}

impl Drop for TrackingPermit {
    fn drop(&mut self) {
        assert!(self.held.swap(false, Ordering::SeqCst));
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

impl MutationLease for TrackingLease {
    fn acquire(
        &self,
        _path: &Path,
        _identity: FileIdentity,
    ) -> Result<Box<dyn MutationPermit>, String> {
        self.held
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .map_err(|_| "mutation permit was already held".to_string())?;
        Ok(Box::new(TrackingPermit {
            held: Arc::clone(&self.held),
            drops: Arc::clone(&self.drops),
        }))
    }
}

#[test]
fn mutation_permit_is_held_through_preflight_commit_and_delete_cleanup() {
    let directory = TestDir::new("clipline-library", "permit-lifetime");
    let root = directory.path().join("media");
    let source = root.join("session_1.mp4");
    let target = root.join("Renamed.mp4");
    write_five_file_clip(&source);
    let held = Arc::new(AtomicBool::new(false));
    let drops = Arc::new(AtomicUsize::new(0));
    let file_system = FaultFileSystem {
        required_permit: Some(Arc::clone(&held)),
        ..FaultFileSystem::default()
    };
    let lease = Arc::new(TrackingLease {
        held: Arc::clone(&held),
        drops: Arc::clone(&drops),
    });
    let repository = fault_repository(&root, file_system, lease);
    let validated = repository
        .validate_clip_path(&source.display().to_string())
        .unwrap();

    repository.rename_file(&validated, "Renamed").unwrap();
    assert!(!held.load(Ordering::SeqCst));
    assert_eq!(drops.load(Ordering::SeqCst), 1);

    let renamed = repository
        .validate_clip_path(&target.display().to_string())
        .unwrap();
    repository.rename_title(&renamed, "Title").unwrap();
    assert!(!held.load(Ordering::SeqCst));
    assert_eq!(drops.load(Ordering::SeqCst), 2);
    repository.delete(&renamed).unwrap();
    assert!(!held.load(Ordering::SeqCst));
    assert_eq!(drops.load(Ordering::SeqCst), 3);
}

#[test]
fn active_upload_lease_rejects_rename_and_delete_without_mutation() {
    let directory = TestDir::new("clipline-library", "active-lease");
    let root = directory.path().join("media");
    let clip = root.join("session_1.mp4");
    let originals = write_five_file_clip(&clip);
    let repository = LocalLibraryRepository::with_seams(
        &root,
        Arc::new(StandardRepositoryFileSystem),
        Arc::new(AlwaysActiveLease),
    )
    .unwrap();
    let validated = repository
        .validate_clip_path(&clip.display().to_string())
        .unwrap();

    assert_eq!(
        repository
            .rename_file(&validated, "Renamed")
            .unwrap_err()
            .to_string(),
        ACTIVE_UPLOAD_MUTATION_ERROR
    );
    assert_eq!(
        repository
            .rename_title(&validated, "Renamed title")
            .unwrap_err()
            .to_string(),
        ACTIVE_UPLOAD_MUTATION_ERROR
    );
    assert_eq!(
        repository.delete(&validated).unwrap_err().to_string(),
        ACTIVE_UPLOAD_MUTATION_ERROR
    );
    for (path, bytes) in originals {
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
}

#[test]
fn title_rename_is_atomic_and_preserves_existing_bytes_on_replace_failure() {
    let directory = TestDir::new("clipline-library", "title-atomic");
    let root = directory.path().join("media");
    let clip = root.join("session_1.mp4");
    touch(&clip, b"mp4");
    let metadata = clip_sidecar_paths(&clip).metadata;
    let original = br#"{"title":"Old","kind":"session","future":42}"#;
    touch(&metadata, original);
    let file_system = FaultFileSystem::default();
    file_system.fail("replace", None, Some("session_1.clipline.json"));
    let repository = fault_repository(&root, file_system, Arc::new(NoActiveMutationLease));
    let validated = repository
        .validate_clip_path(&clip.display().to_string())
        .unwrap();

    assert!(repository.rename_title(&validated, "New").is_err());
    assert_eq!(std::fs::read(metadata).unwrap(), original);
    assert_no_repository_temps(&root);
}

#[test]
fn title_rename_never_overwrites_a_metadata_file_replaced_after_preflight() {
    let directory = TestDir::new("clipline-library", "title-replace-identity-race");
    let root = directory.path().join("media");
    let clip = root.join("session_1.mp4");
    touch(&clip, b"mp4");
    let metadata = clip_sidecar_paths(&clip).metadata;
    touch(&metadata, br#"{"title":"Old","kind":"session"}"#);
    let file_system = FaultFileSystem::default();
    file_system.collide_replace("session_1.clipline.json", b"foreign metadata");
    let repository = fault_repository(&root, file_system, Arc::new(NoActiveMutationLease));
    let validated = repository
        .validate_clip_path(&clip.display().to_string())
        .unwrap();

    assert!(repository.rename_title(&validated, "New").is_err());
    assert_eq!(std::fs::read(&metadata).unwrap(), b"foreign metadata");
    assert_no_repository_temps(&root);
}

#[test]
fn title_rename_updates_only_metadata_and_returns_the_compatibility_shape() {
    let directory = TestDir::new("clipline-library", "title-success");
    let root = directory.path().join("media");
    let clip = root.join("session_1.mp4");
    touch(&clip, b"mp4 bytes");
    let repository = standard_repository(&root);
    let validated = repository
        .validate_clip_path(&clip.display().to_string())
        .unwrap();

    let renamed = repository
        .rename_title(&validated, "  New title  ")
        .unwrap();

    assert_eq!(renamed.old_path, clip.display().to_string());
    assert_eq!(renamed.path, renamed.old_path);
    assert_eq!(renamed.name, "session_1.mp4");
    assert_eq!(renamed.title.as_deref(), Some("New title"));
    assert_eq!(renamed.kind, "session");
    assert_eq!(std::fs::read(&clip).unwrap(), b"mp4 bytes");
    let metadata: serde_json::Value =
        serde_json::from_slice(&std::fs::read(clip_sidecar_paths(&clip).metadata).unwrap())
            .unwrap();
    assert_eq!(metadata["title"], "New title");
    assert_eq!(metadata["kind"], "session");
    assert_no_repository_temps(&root);
}

#[test]
fn corrupt_and_oversized_metadata_are_bounded_and_treated_as_legacy_defaults() {
    for (name, bytes) in [
        ("corrupt", b"not-json".to_vec()),
        ("oversized", vec![b'x'; 64 * 1024 + 1]),
    ] {
        let directory = TestDir::new("clipline-library", &format!("metadata-{name}"));
        let root = directory.path().join("media");
        let clip = root.join("session_1.mp4");
        touch(&clip, b"mp4");
        touch(&clip_sidecar_paths(&clip).metadata, &bytes);
        let repository = standard_repository(&root);
        let validated = repository
            .validate_clip_path(&clip.display().to_string())
            .unwrap();

        let renamed = repository
            .rename_title(&validated, "Recovered title")
            .unwrap();

        assert_eq!(renamed.title.as_deref(), Some("Recovered title"));
        assert_eq!(renamed.kind, "session");
        let stored: serde_json::Value =
            serde_json::from_slice(&std::fs::read(clip_sidecar_paths(&clip).metadata).unwrap())
                .unwrap();
        assert_eq!(stored["title"], "Recovered title");
        assert_eq!(stored["kind"], "session");
    }
}

#[test]
fn compatibility_metadata_projection_is_bounded_and_falls_back_to_the_filename() {
    let directory = TestDir::new("clipline-library", "metadata-compatibility-projection");
    let root = directory.path().join("media");
    let clip = root.join("session_1.mp4");
    touch(&clip, b"mp4");
    let metadata = clip_sidecar_paths(&clip).metadata;

    touch(&metadata, br#"{"title":"  Ranked win  ","kind":"trim"}"#);
    assert_eq!(compatibility_clip_title(&clip), "Ranked win");
    assert_eq!(compatibility_clip_kind(&clip), "trim");

    touch(&metadata, &vec![b'x'; 64 * 1024 + 1]);
    assert_eq!(compatibility_clip_title(&clip), "session_1");
    assert_eq!(compatibility_clip_kind(&clip), "session");

    touch(&metadata, br#"{"kind":"future-kind"}"#);
    assert_eq!(compatibility_clip_kind(&clip), "session");
}

#[test]
fn delete_owns_exactly_four_sidecars_and_bulk_report_is_ordered_and_json_compatible() {
    let directory = TestDir::new("clipline-library", "delete-exact-bulk");
    let root = directory.path().join("media");
    let first = root.join("first.mp4");
    let second = root.join("second.mp4");
    let unrelated = root.join("first.notes.json");
    write_five_file_clip(&first);
    write_five_file_clip(&second);
    touch(&unrelated, b"keep");
    let missing = root.join("missing.mp4").display().to_string();
    let repository = standard_repository(&root);

    let report = repository
        .delete_many(&[
            first.display().to_string(),
            missing.clone(),
            second.display().to_string(),
        ])
        .unwrap();

    assert_eq!(
        report.deleted,
        vec![first.display().to_string(), second.display().to_string()]
    );
    assert_eq!(report.failed.len(), 1);
    assert_eq!(report.failed[0].0, missing);
    assert!(
        report.failed[0].1.contains("cannot find")
            || report.failed[0].1.contains("not find")
            || report.failed[0].1.contains("No such file")
    );
    assert_eq!(std::fs::read(unrelated).unwrap(), b"keep");
    for clip in [&first, &second] {
        assert!(!clip.exists());
        for sidecar in clip_sidecar_paths(clip).into_array() {
            assert!(!sidecar.exists());
        }
    }
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["deleted"].as_array().unwrap().len(), 2);
    assert_eq!(json["failed"][0][0], report.failed[0].0);
    assert_eq!(json["failed"][0][1], report.failed[0].1);
}

#[test]
fn bulk_delete_reports_validation_failures_before_ordered_mutation_failures() {
    let directory = TestDir::new("clipline-library", "delete-bulk-failure-order");
    let root = directory.path().join("media");
    let blocked = root.join("blocked.mp4");
    let kept = root.join("kept.mp4");
    write_five_file_clip(&blocked);
    write_five_file_clip(&kept);
    let missing = root.join("missing.mp4").display().to_string();
    let file_system = FaultFileSystem::default();
    file_system.fail("remove", Some("blocked.mp4"), None);
    let repository = fault_repository(&root, file_system, Arc::new(NoActiveMutationLease));

    let report = repository
        .delete_many(&[
            blocked.display().to_string(),
            missing.clone(),
            kept.display().to_string(),
        ])
        .unwrap();

    assert_eq!(report.deleted, vec![kept.display().to_string()]);
    assert_eq!(report.failed.len(), 2);
    assert_eq!(report.failed[0].0, missing);
    assert_eq!(report.failed[1].0, blocked.display().to_string());
    assert!(blocked.exists());
    for sidecar in clip_sidecar_paths(&blocked).into_array() {
        assert!(sidecar.exists(), "primary failure removed {sidecar:?}");
    }
    assert!(!kept.exists());
}

#[test]
fn bulk_delete_rejects_the_aggregate_path_budget_before_validation_or_mutation() {
    let directory = TestDir::new("clipline-library", "delete-path-budget");
    let root = directory.path().join("media");
    std::fs::create_dir_all(&root).unwrap();
    let repository = standard_repository(&root);
    let paths = vec!["x".repeat(clipline_library::MAX_MUTATION_PATH_BYTES / 2 + 1); 2];

    let error = repository.delete_many(&paths).unwrap_err().to_string();

    assert!(error.starts_with("delete.path_bytes contains "), "{error}");
    assert!(
        error.ends_with(&format!(
            "maximum is {}",
            clipline_library::MAX_MUTATION_PATH_BYTES
        )),
        "{error}"
    );
}

#[test]
fn reveal_and_open_folder_are_typed_effects_with_canonical_authority() {
    let directory = TestDir::new("clipline-library", "platform-effects");
    let root = directory.path().join("media");
    let clip = root.join("session_1.mp4");
    touch(&clip, b"mp4");
    let repository = standard_repository(&root);
    let validated = repository
        .validate_clip_path(&clip.display().to_string())
        .unwrap();

    assert_eq!(
        repository.reveal_effect(&validated).unwrap(),
        PlatformEffect::RevealClip(clip.canonicalize().unwrap())
    );
    assert_eq!(
        repository.open_folder_effect(),
        PlatformEffect::OpenFolder(root.canonicalize().unwrap())
    );
}
