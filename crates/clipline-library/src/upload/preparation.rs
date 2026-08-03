//! Bounded, identity-fenced preparation of durable upload payloads.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

use crate::protocol::CreateUploadRequest;
use crate::{
    client_clip_id_for_payload, compatibility_clip_kind, compatibility_clip_title,
    load_marker_sidecar_with_probe, ClipGame, DurableUploadToken, GameIdentityResolver,
    KnownGameIdentityResolver, Mp4LegacyAudioTrackProbe, OwnedUploadTemp, ParsedMarkerSidecar,
    PreparedUploadPayload, UploadCancellation, UploadIntent, UploadPreparationPort,
    UploadRequestError, UploadSourceLease, UploadWorkError, MAX_CATALOG_STRING_BYTES,
};

const HASH_BUFFER_BYTES: usize = 1024 * 1024;
const MAX_SESSION_GAME_BYTES: u64 = 64 * 1024;

/// Shipping-compatible upload preparation backed by the already-held source
/// lease. All path-based reads are no-follow and are checked against the
/// lease identity before their results can be published.
#[derive(Debug, Clone, Copy, Default)]
pub struct StandardUploadPreparation;

impl UploadPreparationPort for StandardUploadPreparation {
    fn prepare<'a>(
        &'a self,
        source: &'a UploadSourceLease,
        intent: &'a UploadIntent,
        cancellation: &'a UploadCancellation,
    ) -> crate::UploadFuture<'a, Result<PreparedUploadPayload, UploadWorkError>> {
        Box::pin(async move {
            check_cancellation(cancellation)?;
            let normalized = NormalizedIntent::new(intent, source.canonical_path())?;
            let path = source.canonical_path().to_path_buf();
            let identity = source.identity();
            let token = source.token().clone();
            let task_cancellation = cancellation.clone();

            let prepared = tokio::task::spawn_blocking(move || {
                prepare_blocking(path, identity, token, normalized, task_cancellation)
            })
            .await
            .map_err(|error| {
                UploadWorkError::failed(format!("upload preparation task failed: {error}"))
            })??;

            check_cancellation(cancellation)?;
            // The service retains `source` across this await. Re-check the
            // captured logical fence before converting the blocking result to
            // the public payload owner.
            if prepared.token != *source.token()
                || prepared.source_path != source.canonical_path()
                || prepared.source_identity != source.identity()
            {
                return Err(UploadWorkError::failed(
                    "upload source changed while preparing its payload",
                ));
            }

            let payload = match prepared.owner {
                PreparedOwner::Original => PreparedUploadPayload::original(
                    source,
                    prepared.request,
                    prepared.description,
                    prepared.client_clip_id,
                )?,
                PreparedOwner::Owned(temp) => PreparedUploadPayload::owned(
                    temp,
                    prepared.request,
                    prepared.description,
                    prepared.client_clip_id,
                    source.token(),
                )?,
            };
            check_cancellation(cancellation)?;
            Ok(payload)
        })
    }
}

#[derive(Debug)]
struct NormalizedIntent {
    title: String,
    description: Option<String>,
    visibility: String,
    audio_track_ids: Option<Vec<String>>,
}

impl NormalizedIntent {
    fn new(intent: &UploadIntent, source: &Path) -> Result<Self, UploadWorkError> {
        intent
            .validate_for_path(&source.to_string_lossy())
            .map_err(request_error)?;
        let title = intent
            .title
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| compatibility_clip_title(source));
        let title_units = title.encode_utf16().count();
        if title_units > crate::MAX_UPLOAD_TITLE_UTF16 {
            return Err(request_error(UploadRequestError::TitleTooLong {
                actual: title_units,
                maximum: crate::MAX_UPLOAD_TITLE_UTF16,
            }));
        }
        let description = intent
            .description
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
        Ok(Self {
            title,
            description,
            visibility: intent.visibility.clone(),
            audio_track_ids: intent.audio_track_ids.clone(),
        })
    }
}

enum PreparedOwner {
    Original,
    Owned(OwnedUploadTemp),
}

struct BlockingPreparation {
    owner: PreparedOwner,
    request: CreateUploadRequest,
    description: Option<String>,
    client_clip_id: crate::ClientClipId,
    token: DurableUploadToken,
    source_path: PathBuf,
    source_identity: clipline_shell::FileIdentity,
}

fn prepare_blocking(
    source_path: PathBuf,
    source_identity: clipline_shell::FileIdentity,
    token: DurableUploadToken,
    intent: NormalizedIntent,
    cancellation: UploadCancellation,
) -> Result<BlockingPreparation, UploadWorkError> {
    check_cancellation(&cancellation)?;
    let source_metadata = verify_source(&source_path, source_identity)?;
    let markers = load_marker_sidecar_with_probe(&source_path, &Mp4LegacyAudioTrackProbe)
        .map_err(|error| UploadWorkError::failed(format!("read clip marker sidecar: {error}")))?;
    check_cancellation(&cancellation)?;

    let audio_plan = audio_plan(markers.as_ref(), intent.audio_track_ids.as_deref())?;
    let game = read_session_game(&source_path).or_else(|| marker_game(markers.as_ref()));
    let duration_ms = clip_duration_ms(&source_path, markers.as_ref());
    let source_type = compatibility_clip_kind(&source_path);
    check_cancellation(&cancellation)?;

    let (owner, file_size_bytes, checksum_sha256) = match audio_plan {
        AudioPlan::Original => {
            let mut file = open_verified(&source_path, source_identity)?;
            let (size, checksum) = checksum_file(&mut file, &cancellation)?;
            verify_source(&source_path, source_identity)?;
            (PreparedOwner::Original, size, checksum)
        }
        AudioPlan::Remux(indices) => prepare_owned_payload(
            &source_path,
            source_identity,
            &indices,
            false,
            &cancellation,
        )?,
        AudioPlan::Mix(indices) => {
            prepare_owned_payload(&source_path, source_identity, &indices, true, &cancellation)?
        }
    };
    if file_size_bytes == 0 {
        return Err(UploadWorkError::failed("prepared upload payload is empty"));
    }
    check_cancellation(&cancellation)?;

    let client_clip_id = client_clip_id_for_payload(&token.local_clip_id, &checksum_sha256)
        .map_err(|error| UploadWorkError::failed(error.to_string()))?;
    let request = CreateUploadRequest {
        client_clip_id: Some(client_clip_id.as_str().to_owned()),
        title: intent.title,
        description: None,
        game_name: game.as_ref().map(|game| game.name.clone()),
        game_id: game.as_ref().map(|game| game.id.clone()),
        game_executable: None,
        source_type: Some(source_type),
        recorded_at: source_metadata.modified().ok().map(DateTime::<Utc>::from),
        duration_ms,
        file_size_bytes,
        checksum_sha256,
        container: "mp4".into(),
        video_codec: None,
        audio_codec: None,
        width: None,
        height: None,
        fps: None,
        visibility: Some(intent.visibility),
        markers: None,
    };
    Ok(BlockingPreparation {
        owner,
        request,
        description: intent.description,
        client_clip_id,
        token,
        source_path,
        source_identity,
    })
}

fn prepare_owned_payload(
    source_path: &Path,
    source_identity: clipline_shell::FileIdentity,
    indices: &[u32],
    mix: bool,
    cancellation: &UploadCancellation,
) -> Result<(PreparedOwner, u64, String), UploadWorkError> {
    let mut temp = OwnedUploadTemp::create_near(source_path)
        .map_err(|error| UploadWorkError::failed(error.to_string()))?;
    check_cancellation(cancellation)?;
    if mix {
        clipline_mp4::remux_with_mixed_audio_track_into_file(
            source_path,
            temp.file_mut()
                .map_err(|error| UploadWorkError::failed(error.to_string()))?,
            indices,
        )
    } else {
        clipline_mp4::remux_with_selected_audio_tracks_into_file(
            source_path,
            temp.file_mut()
                .map_err(|error| UploadWorkError::failed(error.to_string()))?,
            indices,
        )
    }
    .map_err(|error| UploadWorkError::failed(format!("prepare upload audio: {error}")))?;
    check_cancellation(cancellation)?;
    verify_source(source_path, source_identity)?;
    let (size, checksum) = checksum_file(
        temp.file_mut()
            .map_err(|error| UploadWorkError::failed(error.to_string()))?,
        cancellation,
    )?;
    Ok((PreparedOwner::Owned(temp), size, checksum))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AudioPlan {
    Original,
    Remux(Vec<u32>),
    Mix(Vec<u32>),
}

fn audio_plan(
    markers: Option<&ParsedMarkerSidecar>,
    selection: Option<&[String]>,
) -> Result<AudioPlan, UploadWorkError> {
    let Some(selection) = selection else {
        return Ok(AudioPlan::Original);
    };
    let tracks = markers
        .map(|parsed| parsed.markers().audio_tracks.as_slice())
        .unwrap_or_default();
    if tracks.is_empty() && !selection.is_empty() {
        return Err(UploadWorkError::failed(
            "this clip has no selectable audio track metadata",
        ));
    }

    let selected: BTreeSet<&str> = selection.iter().map(String::as_str).collect();
    if selected.len() != selection.len() {
        return Err(UploadWorkError::failed(
            "audio track selection contains duplicates",
        ));
    }
    if let Some(unknown) = selection
        .iter()
        .find(|id| !tracks.iter().any(|track| track.id == id.as_str()))
    {
        return Err(UploadWorkError::failed(format!(
            "unknown audio track {unknown:?}"
        )));
    }
    let indices: Vec<u32> = tracks
        .iter()
        .filter(|track| selected.contains(track.id.as_str()))
        .map(|track| track.track_index)
        .collect();
    let unique_indices: BTreeSet<u32> = indices.iter().copied().collect();
    if unique_indices.len() != indices.len() {
        return Err(UploadWorkError::failed(
            "selected audio track metadata aliases an MP4 track",
        ));
    }
    if indices.len() > 1 {
        Ok(AudioPlan::Mix(indices))
    } else {
        Ok(AudioPlan::Remux(indices))
    }
}

fn verify_source(
    path: &Path,
    expected: clipline_shell::FileIdentity,
) -> Result<std::fs::Metadata, UploadWorkError> {
    let file = open_verified(path, expected)?;
    file.metadata()
        .map_err(|error| UploadWorkError::failed(format!("read upload source metadata: {error}")))
}

fn open_verified(
    path: &Path,
    expected: clipline_shell::FileIdentity,
) -> Result<File, UploadWorkError> {
    let file = clipline_shell::open_regular_file_nofollow(path)
        .map_err(|error| UploadWorkError::failed(format!("open upload source: {error}")))?;
    let actual = clipline_shell::opened_file_identity(&file)
        .map_err(|error| UploadWorkError::failed(format!("identify upload source: {error}")))?;
    if actual != expected {
        return Err(UploadWorkError::failed(
            "upload source was replaced after its lease was acquired",
        ));
    }
    Ok(file)
}

fn checksum_file(
    file: &mut File,
    cancellation: &UploadCancellation,
) -> Result<(u64, String), UploadWorkError> {
    file.seek(SeekFrom::Start(0))
        .map_err(|error| UploadWorkError::failed(format!("seek upload payload: {error}")))?;
    let mut hash = Sha256::new();
    let mut bytes = 0_u64;
    let mut buffer = vec![0_u8; HASH_BUFFER_BYTES];
    loop {
        check_cancellation(cancellation)?;
        let read = file
            .read(&mut buffer)
            .map_err(|error| UploadWorkError::failed(format!("read upload payload: {error}")))?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
        bytes = bytes
            .checked_add(read as u64)
            .ok_or_else(|| UploadWorkError::failed("upload payload size overflowed"))?;
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|error| UploadWorkError::failed(format!("rewind upload payload: {error}")))?;
    Ok((bytes, format!("{:x}", hash.finalize())))
}

fn clip_duration_ms(path: &Path, markers: Option<&ParsedMarkerSidecar>) -> Option<i64> {
    clipline_mp4::movie_duration_s_file(path)
        .ok()
        .flatten()
        .or_else(|| markers.map(|parsed| parsed.markers().duration_s))
        .map(|seconds| (seconds * 1000.0).round())
        .filter(|milliseconds| milliseconds.is_finite() && *milliseconds >= 0.0)
        .map(|milliseconds| milliseconds as i64)
}

fn marker_game(markers: Option<&ParsedMarkerSidecar>) -> Option<ClipGame> {
    let game = markers?.markers().markers.first()?.event.game_id;
    KnownGameIdentityResolver.resolve(game)
}

fn read_session_game(source: &Path) -> Option<ClipGame> {
    let path = source.parent()?.join("clipline-session.json");
    let file = clipline_shell::open_regular_file_nofollow(&path).ok()?;
    let metadata = file.metadata().ok()?;
    if metadata.len() > MAX_SESSION_GAME_BYTES {
        return None;
    }
    let capacity = usize::try_from(metadata.len()).ok()?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(capacity).ok()?;
    file.take(MAX_SESSION_GAME_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_SESSION_GAME_BYTES {
        return None;
    }
    let mut game: ClipGame = serde_json::from_slice(&bytes).ok()?;
    game.id = game.id.trim().to_owned();
    game.name = game.name.trim().to_owned();
    if game.id.is_empty()
        || game.name.is_empty()
        || game.id.len() > MAX_CATALOG_STRING_BYTES
        || game.name.len() > MAX_CATALOG_STRING_BYTES
    {
        return None;
    }
    Some(game)
}

fn check_cancellation(cancellation: &UploadCancellation) -> Result<(), UploadWorkError> {
    if cancellation.is_canceled() {
        Err(UploadWorkError::Canceled)
    } else {
        Ok(())
    }
}

fn request_error(error: UploadRequestError) -> UploadWorkError {
    UploadWorkError::failed(error.to_string())
}
