//! Scoped game identity and the persisted custom-game ID namespace.

pub const LEAGUE_OF_LEGENDS_ID: &str = "league_of_legends";
pub const VALORANT_ID: &str = "valorant";
pub const CS2_ID: &str = "cs2";
pub const OSU_ID: &str = "osu";

const BUILT_IN_IDS: &[&str] = &[LEAGUE_OF_LEGENDS_ID, VALORANT_ID, CS2_ID, OSU_ID];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GameIdentity {
    BuiltInPlugin(&'static str),
    Custom(String),
}

impl GameIdentity {
    pub fn built_in_plugin(id: &str) -> Option<Self> {
        built_in_id(id).map(Self::BuiltInPlugin)
    }

    pub fn custom(id: impl Into<String>) -> Self {
        Self::Custom(id.into())
    }

    pub fn id(&self) -> &str {
        match self {
            Self::BuiltInPlugin(id) => id,
            Self::Custom(id) => id,
        }
    }

    pub fn plugin_id(&self) -> Option<&'static str> {
        match self {
            Self::BuiltInPlugin(id) => Some(*id),
            Self::Custom(_) => None,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_and_custom_identities_never_overlap() {
        assert!(clipline_settings::games::validate_custom_game_id(OSU_ID).is_err());
        assert!(clipline_settings::games::validate_custom_game_id(LEAGUE_OF_LEGENDS_ID).is_err());
        assert!(clipline_settings::games::validate_custom_game_id("custom-osu-123").is_ok());
        assert!(GameIdentity::custom(OSU_ID).plugin_id().is_none());
        assert!(!GameIdentity::custom("unknown").is_built_in_plugin("unknown"));
    }
}
