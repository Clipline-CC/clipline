//! Stable, traversal-free identities for the framework-neutral Cloud cache.

use std::ffi::{OsStr, OsString};

use crate::{CloudAccountGeneration, CloudAccountKey};
use sha2::{Digest, Sha256};

pub const CLOUD_CACHE_NAMESPACE_HEX_BYTES: usize = 16;
pub const MAX_REMOTE_CLIP_ID_BYTES: usize = 128;

/// A cache namespace derived by the account adapter from the stable account key.
///
/// The shipping compatibility adapter already derives this value from the first
/// 16 hexadecimal characters of SHA-256(host + account). Keeping the validated
/// namespace in the fence lets this neutral module avoid inventing a second hash
/// implementation while still making the disk authority explicit.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CloudCacheNamespace(String);

impl CloudCacheNamespace {
    pub fn new(value: impl Into<String>) -> Result<Self, CacheIdentityError> {
        let value = value.into();
        if value.len() != CLOUD_CACHE_NAMESPACE_HEX_BYTES
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(CacheIdentityError::InvalidNamespace);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Derives the compatibility namespace without exposing the account key on disk.
    pub fn derive(host_url: &str, stable_account: &str) -> Result<Self, CacheIdentityError> {
        let key = format!(
            "{}|{}",
            host_url.trim().trim_end_matches('/'),
            stable_account.trim()
        );
        if key == "|" {
            return Err(CacheIdentityError::InvalidNamespaceSource);
        }
        let digest = Sha256::digest(key.as_bytes());
        Self::new(format!("{:x}", digest)[..CLOUD_CACHE_NAMESPACE_HEX_BYTES].to_owned())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CloudAccountFence {
    pub account_key: CloudAccountKey,
    pub account_generation: CloudAccountGeneration,
    pub cache_namespace: CloudCacheNamespace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CloudAssetKind {
    Thumbnail,
    Media,
}

impl CloudAssetKind {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Thumbnail => "thumbnail",
            Self::Media => "media",
        }
    }

    #[must_use]
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Thumbnail => "jpg",
            Self::Media => "mp4",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CloudAssetKey {
    remote_clip_id: String,
    kind: CloudAssetKind,
    version: u64,
}

impl CloudAssetKey {
    pub fn new(
        remote_clip_id: impl Into<String>,
        kind: CloudAssetKind,
        version: u64,
    ) -> Result<Self, CacheIdentityError> {
        let remote_clip_id = remote_clip_id.into();
        validate_component(&remote_clip_id, MAX_REMOTE_CLIP_ID_BYTES)?;
        Ok(Self {
            remote_clip_id,
            kind,
            version,
        })
    }

    #[must_use]
    pub fn remote_clip_id(&self) -> &str {
        &self.remote_clip_id
    }

    #[must_use]
    pub const fn kind(&self) -> CloudAssetKind {
        self.kind
    }

    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }

    #[must_use]
    pub fn file_name(&self) -> OsString {
        OsString::from(format!(
            "{}-{}-{}.{}",
            self.remote_clip_id,
            self.kind.label(),
            self.version,
            self.kind.extension()
        ))
    }

    #[must_use]
    pub fn marker_name(&self) -> OsString {
        let mut name = self.file_name();
        name.push(".ok");
        name
    }

    #[must_use]
    pub fn owns_asset_name(name: &OsStr) -> bool {
        let Some(name) = name.to_str() else {
            return false;
        };
        let Some((stem, extension)) = name.rsplit_once('.') else {
            return false;
        };
        let kind = match extension {
            "jpg" => CloudAssetKind::Thumbnail,
            "mp4" => CloudAssetKind::Media,
            _ => return false,
        };
        let Some((prefix, version)) = stem.rsplit_once('-') else {
            return false;
        };
        let suffix = format!("-{}", kind.label());
        let Some(remote_clip_id) = prefix.strip_suffix(&suffix) else {
            return false;
        };
        validate_component(remote_clip_id, MAX_REMOTE_CLIP_ID_BYTES).is_ok()
            && !version.is_empty()
            && version.bytes().all(|byte| byte.is_ascii_digit())
            && version.parse::<u64>().is_ok()
    }

    #[must_use]
    pub fn owns_marker_name(name: &OsStr) -> bool {
        name.to_str()
            .and_then(|name| name.strip_suffix(".ok"))
            .is_some_and(|asset| Self::owns_asset_name(OsStr::new(asset)))
    }

    #[must_use]
    pub fn owns_temp_name(name: &OsStr) -> bool {
        let Some(name) = name.to_str() else {
            return false;
        };
        let mut parts = name.rsplitn(4, '.');
        let Some("tmp") = parts.next() else {
            return false;
        };
        let Some(counter) = parts.next() else {
            return false;
        };
        let Some(pid) = parts.next() else {
            return false;
        };
        let Some(owned) = parts.next() else {
            return false;
        };
        !counter.is_empty()
            && counter.bytes().all(|byte| byte.is_ascii_digit())
            && !pid.is_empty()
            && pid.bytes().all(|byte| byte.is_ascii_digit())
            && (Self::owns_asset_name(OsStr::new(owned))
                || Self::owns_marker_name(OsStr::new(owned)))
    }
}

fn validate_component(value: &str, max_bytes: usize) -> Result<(), CacheIdentityError> {
    if value.is_empty()
        || value.len() > max_bytes
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(CacheIdentityError::InvalidComponent);
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CacheIdentityError {
    #[error("cloud cache namespace must be exactly 16 lowercase hexadecimal bytes")]
    InvalidNamespace,
    #[error("cloud cache namespace source is empty")]
    InvalidNamespaceSource,
    #[error("cloud cache identity contains unsupported characters or is too long")]
    InvalidComponent,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owned_name_grammar_is_exact() {
        let key = CloudAssetKey::new("remote-1_A", CloudAssetKind::Media, 42).unwrap();
        assert_eq!(key.file_name(), "remote-1_A-media-42.mp4");
        assert!(CloudAssetKey::owns_asset_name(&key.file_name()));
        assert!(CloudAssetKey::owns_marker_name(&key.marker_name()));
        assert!(CloudAssetKey::owns_temp_name(OsStr::new(
            "remote-1_A-media-42.mp4.123.9.tmp"
        )));
        for foreign in [
            "../remote-media-1.mp4",
            "remote-media-x.mp4",
            "remote-video-1.mp4",
            "remote-media-1.exe",
            "editor.tmp",
        ] {
            assert!(!CloudAssetKey::owns_asset_name(OsStr::new(foreign)));
            assert!(!CloudAssetKey::owns_temp_name(OsStr::new(foreign)));
        }
    }

    #[test]
    fn namespace_derivation_matches_the_legacy_compatibility_key() {
        let namespace =
            CloudCacheNamespace::derive("https://clips.example/", " user-1 ").expect("namespace");
        let digest = Sha256::digest(b"https://clips.example|user-1");
        assert_eq!(
            namespace.as_str(),
            &format!("{digest:x}")[..CLOUD_CACHE_NAMESPACE_HEX_BYTES]
        );
        assert!(CloudCacheNamespace::derive("/", "").is_err());
    }
}
