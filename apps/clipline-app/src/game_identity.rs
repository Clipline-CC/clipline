//! Scoped game identity and the persisted custom-game ID namespace.

use std::collections::HashSet;

pub const LEAGUE_OF_LEGENDS_ID: &str = "league_of_legends";
pub const VALORANT_ID: &str = "valorant";
pub const CS2_ID: &str = "cs2";
pub const OSU_ID: &str = "osu";

const BUILT_IN_IDS: &[&str] = &[LEAGUE_OF_LEGENDS_ID, VALORANT_ID, CS2_ID, OSU_ID];
const CUSTOM_PREFIX: &str = "custom-";
const MIGRATED_PREFIX: &str = "custom-migrated-";
const MAX_CUSTOM_ID_LEN: usize = 96;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GameIdentity {
    BuiltInPlugin(&'static str),
    Custom(String),
    DiscoveredSteam { app_id: u32, id: String },
}

impl GameIdentity {
    pub fn built_in_plugin(id: &str) -> Option<Self> {
        built_in_id(id).map(Self::BuiltInPlugin)
    }

    pub fn custom(id: impl Into<String>) -> Self {
        Self::Custom(id.into())
    }

    pub fn discovered_steam(app_id: u32) -> Self {
        Self::DiscoveredSteam {
            app_id,
            id: format!("steam-{app_id}"),
        }
    }

    pub fn id(&self) -> &str {
        match self {
            Self::BuiltInPlugin(id) => id,
            Self::Custom(id) => id,
            Self::DiscoveredSteam { id, .. } => id,
        }
    }

    pub fn plugin_id(&self) -> Option<&'static str> {
        match self {
            Self::BuiltInPlugin(id) => Some(*id),
            Self::Custom(_) => None,
            Self::DiscoveredSteam { .. } => None,
        }
    }

    pub fn is_built_in_plugin(&self, id: &str) -> bool {
        matches!(
            (self.plugin_id(), built_in_id(id)),
            (Some(actual), Some(expected)) if actual == expected
        )
    }
}

pub fn built_in_id(id: &str) -> Option<&'static str> {
    BUILT_IN_IDS
        .iter()
        .copied()
        .find(|built_in| *built_in == id)
}

pub fn validate_custom_game_id(id: &str) -> Result<(), String> {
    if built_in_id(id).is_some() {
        return Err(format!(
            "custom game id {id:?} is reserved for a built-in game"
        ));
    }
    if id.len() > MAX_CUSTOM_ID_LEN {
        return Err(format!(
            "custom game id must be at most {MAX_CUSTOM_ID_LEN} characters"
        ));
    }
    let Some(slug) = id.strip_prefix(CUSTOM_PREFIX) else {
        return Err(format!(
            "custom game id {id:?} must use the {CUSTOM_PREFIX} namespace"
        ));
    };
    if slug.is_empty()
        || slug.starts_with('-')
        || slug.ends_with('-')
        || slug.contains("--")
        || !slug
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(format!("custom game id {id:?} is not a canonical slug"));
    }
    Ok(())
}

pub fn migrated_custom_game_id(raw_id: &str, fallback_name: &str) -> String {
    let source = if raw_id.trim().is_empty() {
        fallback_name
    } else {
        raw_id
    };
    let max_slug_len = MAX_CUSTOM_ID_LEN - MIGRATED_PREFIX.len();
    let mut slug = source
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    slug.truncate(max_slug_len);
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        slug.push_str("game");
    }
    format!("{MIGRATED_PREFIX}{slug}")
}

pub fn unique_migrated_custom_game_id(
    raw_id: &str,
    fallback_name: &str,
    occupied: &mut HashSet<String>,
) -> String {
    let base = migrated_custom_game_id(raw_id, fallback_name);
    let mut candidate = base.clone();
    let mut suffix = 2_u32;
    while occupied.contains(&candidate) {
        let suffix_text = format!("-{suffix}");
        let stem_len = MAX_CUSTOM_ID_LEN - suffix_text.len();
        candidate = format!("{}{suffix_text}", &base[..base.len().min(stem_len)]);
        suffix += 1;
    }
    occupied.insert(candidate.clone());
    candidate
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_and_custom_identities_never_overlap() {
        assert!(validate_custom_game_id(OSU_ID).is_err());
        assert!(validate_custom_game_id(LEAGUE_OF_LEGENDS_ID).is_err());
        assert!(validate_custom_game_id("custom-osu-123").is_ok());
        assert!(GameIdentity::custom(OSU_ID).plugin_id().is_none());
        assert!(!GameIdentity::custom("unknown").is_built_in_plugin("unknown"));
    }

    #[test]
    fn discovered_steam_id_is_stable_and_outside_custom_namespace() {
        let identity = GameIdentity::discovered_steam(427520);
        assert_eq!(identity.id(), "steam-427520");
        assert!(identity.plugin_id().is_none());
        assert!(!identity.is_built_in_plugin("steam-427520"));
        assert!(validate_custom_game_id(identity.id()).is_err());
        assert!(validate_custom_game_id("steam-427520").is_err());
        assert_ne!(identity, GameIdentity::custom("steam-427520"));
    }
}
