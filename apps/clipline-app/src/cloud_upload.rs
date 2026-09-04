//! Upload transport selection for Clipline Cloud.
//!
//! The server-proxy upload path remains the baseline. Direct-to-S3 multipart
//! uploads are used only when discovery and the create-upload response both
//! advertise the required capability.

use std::{
    collections::{hash_map::DefaultHasher, BTreeSet, HashMap},
    fs::{File, OpenOptions},
    hash::{Hash, Hasher},
    os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
    path::Path,
    sync::{Mutex, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use clipline_cloud_api::{
    types::{DirectPartUploadAckRequest, DirectPartUploadUrlResponse},
    CloudApiError, CloudApiResult, CloudClient, CreateUploadRequest, CreateUploadResponse,
    DiscoveryResponse, PartUploadResponse, UploadProgressResponse,
};
use reqwest::{header, StatusCode};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;
use windows_sys::Win32::{
    Foundation::HANDLE,
    Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_SHARE_READ,
    },
};

const DIRECT_PUT_MAX_ATTEMPTS: usize = 3;
const PROXY_PUT_MAX_ATTEMPTS: usize = 3;
const DIRECT_PUT_BACKOFF_BASE: Duration = Duration::from_millis(250);
const DIRECT_PUT_BACKOFF_MAX: Duration = Duration::from_secs(30);
const MAX_UPLOAD_PART_BYTES: u64 = 64 * 1024 * 1024;
const MAX_CONCURRENT_UPLOADS: usize = 2;
static UPLOAD_PERMITS: tokio::sync::Semaphore =
    tokio::sync::Semaphore::const_new(MAX_CONCURRENT_UPLOADS);
static ACTIVE_UPLOAD_SOURCES: OnceLock<Mutex<HashMap<UploadSourceIdentity, usize>>> =
    OnceLock::new();
const ACTIVE_UPLOAD_MUTATION_ERROR: &str = "clip is uploading; wait for the upload to finish";

#[path = "cloud_upload/types.rs"]
mod types;
#[path = "cloud_upload/source_lease.rs"]
mod source_lease;
#[path = "cloud_upload/create.rs"]
mod create;
#[path = "cloud_upload/entry.rs"]
mod entry;
#[path = "cloud_upload/single_put.rs"]
mod single_put;
#[path = "cloud_upload/proxy.rs"]
mod proxy;
#[path = "cloud_upload/direct.rs"]
mod direct;
#[path = "cloud_upload/parts.rs"]
mod parts;
#[path = "cloud_upload/http.rs"]
mod http;
#[path = "cloud_upload/retry.rs"]
mod retry;

#[path = "cloud_upload/test_support.rs"]
#[cfg(test)]
mod test_support;

pub(crate) use types::*;
pub(crate) use source_lease::*;
pub(crate) use create::*;
pub(crate) use entry::*;
pub(crate) use single_put::*;
pub(crate) use proxy::*;
pub(crate) use direct::*;
pub(crate) use parts::*;
pub(crate) use http::*;
pub(crate) use retry::*;

#[cfg(test)]
pub(crate) use test_support::*;
