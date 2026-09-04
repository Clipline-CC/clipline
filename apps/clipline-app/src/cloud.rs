//! Clipline Cloud desktop integration: connection state, OS credential storage,
//! and per-clip uploads through the first-party API client.

#[path = "cloud/cache_identity.rs"]
mod cache_identity;
use cache_identity::validate_cloud_cache_component;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, UNIX_EPOCH};

use base64::{engine::general_purpose, Engine as _};
use chrono::{DateTime, Utc};
use clipline_cloud_api::types::{CreateDeviceTokenRequest, CreateDeviceTokenResponse};
use clipline_cloud_api::{
    sha256_hex, ClipDetailResponse, ClipSummaryResponse, CloudApiError, CloudClient,
    CreateUploadRequest, DiscoveryResponse, ListClipsRequest, MeResponse, UpdateVisibilityRequest,
};
use clipline_events::ClipMarkers;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, Runtime};
use tokio::io::AsyncWriteExt;

use crate::app::RuntimeState;
use crate::library::{validate_clip_path, StorageSettings};
use crate::settings::{normalize_cloud_visibility, CloudSettings, CloudUploadRecord};
use crate::util::unix_now;
use crate::windows::CredentialStore;

const DEFAULT_DEVICE_NAME: &str = "Clipline Desktop";
const READY_POLL_ATTEMPTS: usize = 30;
const READY_POLL_DELAY: Duration = Duration::from_secs(1);
const READY_MEDIA_PROBE_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const READY_MEDIA_PROBE_TIMEOUT: Duration = Duration::from_secs(15);
const CLOUD_LIBRARY_PAGE_SIZE: i64 = 100;
const CLOUD_LIBRARY_MAX_PAGES: i64 = 100;
const CLOUD_LIBRARY_MAX_CLIPS: usize = 10_000;
const CLOUD_UPLOAD_PROGRESS_EVENT: &str = "cloud-upload-progress";
const REMOTE_NOT_FOUND_SYNC_MARKER: &str = "remote clip not found during status sync";
const MAX_AVATAR_BYTES: usize = 2 * 1024 * 1024;
const CLOUD_CACHE_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const UPLOAD_PAYLOAD_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const UPLOAD_PAYLOAD_PREFIX: &str = "clipline-upload-";
const CLOUD_THUMBNAIL_MAX_BYTES: u64 = 10 * 1024 * 1024;
const CLOUD_MEDIA_FALLBACK_MAX_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const CLOUD_MEDIA_HARD_MAX_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const CLOUD_MEDIA_SIZE_SLACK_BYTES: u64 = 64 * 1024 * 1024;
const CLOUD_CREDENTIALS: CredentialStore = CredentialStore::new("cloud token");
const CLOUD_CACHE_QUOTA_BYTES: u64 = 10 * 1024 * 1024 * 1024;
const CLOUD_CACHE_FREE_SPACE_FLOOR_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const CLOUD_CACHE_TEMP_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const CLOUD_CACHE_PLAYBACK_LEASE: Duration = Duration::from_secs(24 * 60 * 60);
static CLOUD_CACHE_TMP_COUNTER: AtomicU64 = AtomicU64::new(0);
static CLOUD_CACHE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
static CLOUD_CACHE_LEASES: OnceLock<Mutex<BTreeMap<PathBuf, Instant>>> = OnceLock::new();
static CLOUD_USER_AVATAR_CACHE: OnceLock<Mutex<Option<CachedCloudUserAvatar>>> = OnceLock::new();

#[path = "cloud/types.rs"]
mod types;
#[path = "cloud/connection.rs"]
mod connection;
#[path = "cloud/library_sync.rs"]
mod library_sync;
#[path = "cloud/assets.rs"]
mod assets;
#[path = "cloud/avatar.rs"]
mod avatar;
#[path = "cloud/cache.rs"]
mod cache;
#[path = "cloud/cache_legacy.rs"]
mod cache_legacy;
#[path = "cloud/upload_command.rs"]
mod upload_command;
#[path = "cloud/upload_request.rs"]
mod upload_request;
#[path = "cloud/records.rs"]
mod records;
#[path = "cloud/readiness.rs"]
mod readiness;

#[path = "cloud/test_support.rs"]
#[cfg(test)]
mod test_support;

pub(crate) use types::*;
pub(crate) use connection::*;
pub(crate) use library_sync::*;
pub(crate) use assets::*;
pub(crate) use avatar::*;
pub(crate) use cache::*;
pub(crate) use cache_legacy::*;
pub(crate) use upload_command::*;
pub(crate) use upload_request::*;
pub(crate) use records::*;
pub(crate) use readiness::*;

#[cfg(test)]
pub(crate) use test_support::*;
