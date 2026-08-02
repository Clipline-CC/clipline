use std::path::Path;

use serde::{Deserialize, Deserializer, Serialize};

pub const MAX_CATALOG_IDENTITY_BYTES: usize = 16 * 1024;

/// A comparison-only clip path key.
///
/// The original spelling is retained separately for display and reconciliation;
/// every filesystem operation must use a separately canonicalized, containment-
/// validated path. This value trims surrounding whitespace, folds absolute Windows
/// drive/UNC paths, and leaves every other path case- and slash-sensitive, matching
/// the shipping JavaScript.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ClipPathIdentity(String);

impl ClipPathIdentity {
    #[must_use]
    pub fn from_path(path: &Path) -> Option<Self> {
        Self::from_text(&path.to_string_lossy())
    }

    #[must_use]
    pub fn from_text(path: &str) -> Option<Self> {
        let text = path.trim();
        if text.is_empty() || text.len() > MAX_CATALOG_IDENTITY_BYTES {
            return None;
        }

        let mut normalized = text.replace('/', "\\");
        let lower = normalized.to_ascii_lowercase();
        if lower.starts_with(r"\\?\unc\") {
            normalized = format!(r"\\{}", &normalized[8..]);
        } else if lower.starts_with(r"\\?\") {
            normalized = normalized[4..].to_owned();
        }

        let is_drive_absolute = normalized.as_bytes().get(1) == Some(&b':')
            && normalized.as_bytes().get(2) == Some(&b'\\')
            && normalized
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphabetic);
        if is_drive_absolute || normalized.starts_with(r"\\") {
            Some(Self(format!("windows:{}", normalized.to_lowercase())))
        } else {
            Some(Self(format!("exact:{text}")))
        }
    }

    #[must_use]
    pub fn same(left: &str, right: &str) -> bool {
        match (Self::from_text(left), Self::from_text(right)) {
            (Some(left), Some(right)) => left == right,
            _ => false,
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn from_serialized_key(key: String) -> Option<Self> {
        if key.is_empty() || key.len() > MAX_CATALOG_IDENTITY_BYTES {
            return None;
        }
        let raw = key
            .strip_prefix("windows:")
            .or_else(|| key.strip_prefix("exact:"))?;
        Self::from_text(raw).filter(|identity| identity.0 == key)
    }
}

impl<'de> Deserialize<'de> for ClipPathIdentity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let key = String::deserialize(deserializer)?;
        Self::from_serialized_key(key)
            .ok_or_else(|| serde::de::Error::custom("invalid clip path identity"))
    }
}

impl AsRef<str> for ClipPathIdentity {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl std::fmt::Display for ClipPathIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}
