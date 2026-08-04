//! Compatibility adapter over the framework-neutral settings document/store.

pub use clipline_settings::*;

pub mod cloud {
    #[allow(unused_imports)]
    pub use clipline_settings::cloud::*;
}

pub mod games {
    #[allow(unused_imports)]
    pub use clipline_settings::games::*;
}

pub mod hotkey {
    #[allow(unused_imports)]
    pub use clipline_settings::hotkey::*;
}

pub mod osu {
    #[allow(unused_imports)]
    pub use clipline_settings::osu::*;
}

pub mod persistence {
    pub use clipline_settings::persistence::*;
}

pub mod types {
    #[allow(unused_imports)]
    pub use clipline_settings::types::*;
}

pub mod validation {
    pub use clipline_settings::validation::*;
}

pub use clipline_recorder::AppSettingsServiceExt;
