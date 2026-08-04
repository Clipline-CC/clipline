//! Scoped game identity and the persisted custom-game ID namespace.

pub use clipline_settings::games::{
    BUILT_IN_GAME_IDS, CS2_ID, LEAGUE_OF_LEGENDS_ID, OSU_ID, VALORANT_ID,
};

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
    BUILT_IN_GAME_IDS
        .iter()
        .copied()
        .find(|built_in| *built_in == id)
}
