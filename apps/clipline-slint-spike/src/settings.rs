//! Process-owned settings bootstrap for the native candidate.

use std::path::{Path, PathBuf};

use clipline_settings::{
    SettingsPathResolver, SettingsProfile, SettingsProfileError, SettingsSnapshot, SettingsStore,
    SettingsTransaction, SettingsTransactionError,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CandidateSettingsProfile {
    SharedProduction,
    Isolated(PathBuf),
}

impl CandidateSettingsProfile {
    pub fn from_isolated_path(path: Option<&Path>) -> Self {
        path.map_or(Self::SharedProduction, |path| {
            Self::Isolated(path.to_path_buf())
        })
    }
}

struct InstalledProfileResolver;

impl SettingsPathResolver for InstalledProfileResolver {
    fn resolve_settings_profile(&self) -> SettingsProfile {
        SettingsProfile::installed()
    }
}

pub struct CandidateSettings {
    store: SettingsStore,
}

impl CandidateSettings {
    pub fn open(profile: CandidateSettingsProfile) -> Result<Self, SettingsProfileError> {
        Self::open_with_resolver(profile, &InstalledProfileResolver)
    }

    pub fn open_with_resolver(
        profile: CandidateSettingsProfile,
        production_resolver: &dyn SettingsPathResolver,
    ) -> Result<Self, SettingsProfileError> {
        let store = match profile {
            CandidateSettingsProfile::SharedProduction => {
                SettingsStore::open_resolved(production_resolver)
            }
            CandidateSettingsProfile::Isolated(root) => {
                SettingsStore::open(SettingsProfile::try_isolated(root)?)
            }
        };
        Ok(Self { store })
    }

    pub fn snapshot(&self) -> Result<SettingsSnapshot, SettingsTransactionError> {
        self.store.snapshot()
    }

    pub fn transact(
        &self,
        transaction: SettingsTransaction,
    ) -> Result<SettingsSnapshot, SettingsTransactionError> {
        self.store.transact(transaction)
    }

    pub fn store(&self) -> &SettingsStore {
        &self.store
    }
}
