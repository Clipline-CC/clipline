use clipline_games::identity::{built_in_id, GameIdentity, LEAGUE_OF_LEGENDS_ID, OSU_ID};

#[test]
fn built_in_and_custom_identities_never_overlap() {
    assert!(clipline_settings::games::validate_custom_game_id(OSU_ID).is_err());
    assert!(clipline_settings::games::validate_custom_game_id(LEAGUE_OF_LEGENDS_ID).is_err());
    assert!(clipline_settings::games::validate_custom_game_id("custom-osu-123").is_ok());
    assert!(GameIdentity::custom(OSU_ID).plugin_id().is_none());
    assert!(!GameIdentity::custom("unknown").is_built_in_plugin("unknown"));
    assert_eq!(built_in_id(OSU_ID), Some(OSU_ID));
    for plugin in clipline_games::plugin::all() {
        assert!(built_in_id(plugin.id()).is_some());
        assert!(clipline_settings::games::validate_custom_game_id(plugin.id()).is_err());
    }
}
