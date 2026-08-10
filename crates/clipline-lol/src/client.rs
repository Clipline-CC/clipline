use std::net::IpAddr;
use std::time::Duration;

use serde::Deserialize;

use clipline_events::{PlayerItem, PlayerParticipant, PlayerSummary, PlayerSummonerSpell};

use crate::normalize::PlayerNameIdentity;
use crate::raw::{EventData, PlayerItemEntry, PlayerListEntry, PlayerSummonerSpellEntry};

/// Riot's local Live Client Data endpoint (ddoc §5a).
const DEFAULT_BASE: &str = "https://127.0.0.1:2999";
const MAX_LIVE_CLIENT_JSON_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("live client request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("refusing to disable certificate validation for non-loopback URL: {0}")]
    NotLoopback(String),
    #[error("live client response is invalid: {0}")]
    InvalidResponse(String),
    #[error("live client JSON is invalid: {0}")]
    InvalidJson(#[from] serde_json::Error),
}

/// Client for the League Live Client Data API. The real endpoint serves a
/// self-signed Riot cert over loopback, so certificate validation is
/// disabled only after the base is pinned to a numeric loopback address.
pub struct LiveClient {
    base: reqwest::Url,
    http: reqwest::Client,
}

impl std::fmt::Debug for LiveClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveClient")
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct GameStats {
    #[serde(rename = "gameTime")]
    game_time: f64,
}

impl LiveClient {
    pub fn new(base: impl Into<String>) -> Result<Self, Error> {
        let base = base.into();
        let base = normalize_loopback_base(&base)?;
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .danger_accept_invalid_certs(true)
            .connect_timeout(Duration::from_secs(1))
            .read_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(2))
            .build()?;
        Ok(Self { base, http })
    }

    /// Client against the real local game endpoint.
    pub fn default_local() -> Result<Self, Error> {
        Self::new(DEFAULT_BASE)
    }

    pub async fn event_data(&self) -> Result<EventData, Error> {
        self.get_json("/liveclientdata/eventdata").await
    }

    pub async fn player_list(&self) -> Result<Vec<PlayerListEntry>, Error> {
        self.get_json("/liveclientdata/playerlist").await
    }

    /// Riot returns the active player's name as a bare JSON string.
    pub async fn active_player_name(&self) -> Result<String, Error> {
        self.get_json("/liveclientdata/activeplayername").await
    }

    pub async fn player_summary(&self, local_player: &str) -> Result<Option<PlayerSummary>, Error> {
        let players = self.player_list().await?;
        let game_time_s = self.game_time_s().await.ok();
        Ok(player_summary_from_list_with_game_time(
            &players,
            local_player,
            game_time_s,
        ))
    }

    /// Current game clock in seconds, from `gamestats.gameTime` (ddoc §5).
    pub async fn game_time_s(&self) -> Result<f64, Error> {
        let stats: GameStats = self.get_json("/liveclientdata/gamestats").await?;
        Ok(stats.game_time)
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, Error> {
        let url = self
            .base
            .join(path.trim_start_matches('/'))
            .map_err(|error| Error::InvalidResponse(format!("build endpoint URL: {error}")))?;
        let mut response = self.http.get(url).send().await?.error_for_status()?;
        if response
            .content_length()
            .is_some_and(|length| length > MAX_LIVE_CLIENT_JSON_BYTES as u64)
        {
            return Err(Error::InvalidResponse(format!(
                "body exceeds {} bytes",
                MAX_LIVE_CLIENT_JSON_BYTES
            )));
        }
        let mut body = Vec::with_capacity(
            response
                .content_length()
                .unwrap_or(0)
                .min(MAX_LIVE_CLIENT_JSON_BYTES as u64) as usize,
        );
        while let Some(chunk) = response.chunk().await? {
            let total = body
                .len()
                .checked_add(chunk.len())
                .ok_or_else(|| Error::InvalidResponse("body size overflow".to_string()))?;
            if total > MAX_LIVE_CLIENT_JSON_BYTES {
                return Err(Error::InvalidResponse(format!(
                    "body exceeds {} bytes",
                    MAX_LIVE_CLIENT_JSON_BYTES
                )));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(serde_json::from_slice(&body)?)
    }
}

fn normalize_loopback_base(base: &str) -> Result<reqwest::Url, Error> {
    let reject = || Error::NotLoopback(base.to_string());
    let mut url = reqwest::Url::parse(base).map_err(|_| reject())?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(reject());
    }

    let host = url.host_str().ok_or_else(&reject)?;
    if host.eq_ignore_ascii_case("localhost") {
        url.set_host(Some("127.0.0.1")).map_err(|_| reject())?;
        return Ok(url);
    }

    let numeric_host = host
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(host);
    let numeric = numeric_host.parse::<IpAddr>().map_err(|_| reject())?;
    if !numeric.is_loopback() {
        return Err(reject());
    }
    Ok(url)
}

/// Returns `Some(true)` only when a base can be normalized to a numeric
/// loopback endpoint, `Some(false)` for other URLs, and `None` for garbage.
#[cfg(test)]
fn is_loopback_url(base: &str) -> Option<bool> {
    match reqwest::Url::parse(base) {
        Ok(_) => Some(normalize_loopback_base(base).is_ok()),
        Err(_) => None,
    }
}

fn normalized_game_time_s(game_time_s: Option<f64>) -> Option<u32> {
    let seconds = game_time_s?;
    if !seconds.is_finite() || seconds < 0.0 || seconds > u32::MAX as f64 {
        return None;
    }
    Some(seconds.floor() as u32)
}

fn summoner_spell_asset_key(spell: &PlayerSummonerSpellEntry) -> String {
    let raw = spell.raw_display_name.trim();
    if let Some((_, after_prefix)) = raw.split_once("SummonerSpell_") {
        let key = after_prefix.split('_').next().unwrap_or_default();
        if key.starts_with("Summoner")
            && key.len() > "Summoner".len()
            && key.chars().all(|ch| ch.is_ascii_alphanumeric())
        {
            // Hextech Flashtraption replaces Flash and has no Data Dragon spell art.
            if key.starts_with("SummonerFlash") {
                return "SummonerFlash".to_string();
            }
            return key.to_string();
        }
    }

    match spell
        .display_name
        .trim()
        .to_ascii_lowercase()
        .replace(|ch: char| !ch.is_ascii_alphanumeric(), "")
        .as_str()
    {
        "barrier" => "SummonerBarrier",
        "clarity" => "SummonerMana",
        "cleanse" => "SummonerBoost",
        "dash" => "SummonerSnowball",
        "exhaust" => "SummonerExhaust",
        "flash" | "hexflash" | "hextechflashtraption" => "SummonerFlash",
        "ghost" => "SummonerHaste",
        "heal" => "SummonerHeal",
        "ignite" => "SummonerDot",
        "mark" => "SummonerSnowball",
        "smite" => "SummonerSmite",
        "teleport" => "SummonerTeleport",
        _ => "",
    }
    .to_string()
}

fn summoner_spell_summary(spell: &PlayerSummonerSpellEntry) -> Option<PlayerSummonerSpell> {
    let name = spell.display_name.trim();
    if name.is_empty() {
        return None;
    }
    Some(PlayerSummonerSpell {
        name: name.to_string(),
        asset_key: summoner_spell_asset_key(spell),
    })
}

fn item_summary(item: &PlayerItemEntry) -> Option<PlayerItem> {
    if item.item_id == 0 {
        return None;
    }
    let name = item.display_name.trim();
    Some(PlayerItem {
        id: item.item_id,
        name: if name.is_empty() {
            item.item_id.to_string()
        } else {
            name.to_string()
        },
        slot: item.slot,
    })
}

pub fn player_summary_from_list_with_game_time(
    players: &[PlayerListEntry],
    local_player: &str,
    game_time_s: Option<f64>,
) -> Option<PlayerSummary> {
    let local_identity = PlayerNameIdentity::new(local_player);
    if local_identity.is_empty() {
        return None;
    }
    let player = players
        .iter()
        .find(|player| {
            local_identity.exact_match(&player.summoner_name)
                || player
                    .riot_id
                    .as_deref()
                    .is_some_and(|riot_id| local_identity.exact_match(riot_id))
        })
        .or_else(|| {
            players.iter().find(|player| {
                let preferred_name = player
                    .riot_id
                    .as_deref()
                    .filter(|riot_id| PlayerNameIdentity::new(riot_id).has_tagline())
                    .unwrap_or(&player.summoner_name);
                local_identity.matches(preferred_name)
            })
        })?;
    let champion_name = player.champion_name.trim();
    if champion_name.is_empty() {
        return None;
    }
    let participants = players
        .iter()
        .filter_map(|player| {
            let player_name = player.summoner_name.trim();
            let champion_name = player.champion_name.trim();
            if player_name.is_empty() || champion_name.is_empty() {
                return None;
            }
            Some(PlayerParticipant {
                player_name: player_name.to_string(),
                champion_name: champion_name.to_string(),
                team: player.team.trim().to_string(),
            })
        })
        .collect();
    let summoner_spells = [
        player.summoner_spells.one.as_ref(),
        player.summoner_spells.two.as_ref(),
    ]
    .into_iter()
    .flatten()
    .filter_map(summoner_spell_summary)
    .collect();
    let mut items: Vec<_> = player.items.iter().filter_map(item_summary).collect();
    items.sort_by_key(|item| item.slot.unwrap_or(u32::MAX));

    Some(PlayerSummary {
        champion_name: champion_name.to_string(),
        kills: player.scores.kills,
        deaths: player.scores.deaths,
        assists: player.scores.assists,
        creep_score: player.scores.creep_score,
        game_time_s: normalized_game_time_s(game_time_s),
        player_name: player.summoner_name.trim().to_string(),
        team: player.team.trim().to_string(),
        participants,
        summoner_spells,
        items,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raw::{PlayerListEntry, PlayerScores, PlayerSummonerSpells};
    use httpmock::prelude::*;

    #[test]
    fn is_loopback_url_accepts_loopback_variants() {
        assert_eq!(is_loopback_url("https://127.0.0.1:2999"), Some(true));
        assert_eq!(is_loopback_url("https://127.0.0.1"), Some(true));
        assert_eq!(is_loopback_url("http://localhost:1234"), Some(true));
        assert_eq!(is_loopback_url("https://[::1]:2999"), Some(true));
    }

    #[test]
    fn is_loopback_url_rejects_remote_hosts() {
        assert_eq!(is_loopback_url("https://example.com"), Some(false));
        assert_eq!(is_loopback_url("https://192.168.1.1:2999"), Some(false));
        assert_eq!(is_loopback_url("https://10.0.0.1"), Some(false));
    }

    #[test]
    fn is_loopback_url_returns_none_for_garbage() {
        assert_eq!(is_loopback_url("not a url"), None);
    }

    #[test]
    fn new_rejects_non_loopback_url() {
        let err = LiveClient::new("https://example.com:2999").unwrap_err();
        assert!(matches!(err, Error::NotLoopback(_)));
    }

    #[test]
    fn loopback_base_normalization_eliminates_name_resolution_and_url_tricks() {
        assert_eq!(
            normalize_loopback_base("https://127.0.0.1:2999")
                .unwrap()
                .as_str(),
            "https://127.0.0.1:2999/"
        );
        assert_eq!(
            normalize_loopback_base("https://[::1]:2999")
                .unwrap()
                .as_str(),
            "https://[::1]:2999/"
        );
        assert_eq!(
            normalize_loopback_base("http://localhost:1234")
                .unwrap()
                .as_str(),
            "http://127.0.0.1:1234/"
        );

        for rejected in [
            "https://example.com:2999",
            "https://192.168.1.1:2999",
            "ftp://127.0.0.1:2999",
            "https://user:pass@127.0.0.1:2999",
            "https://127.0.0.1:2999/prefix",
            "https://127.0.0.1:2999?query=1",
            "https://127.0.0.1:2999/#fragment",
            "not a url",
        ] {
            assert!(
                normalize_loopback_base(rejected).is_err(),
                "must reject {rejected}"
            );
        }
    }

    #[test]
    fn live_client_builder_explicitly_disables_redirects_and_proxies() {
        let source = include_str!("client.rs");
        assert!(source.contains(".redirect(reqwest::redirect::Policy::none())"));
        assert!(source.contains(".no_proxy()"));
    }

    #[tokio::test]
    async fn live_client_does_not_follow_loopback_redirects() {
        let target = MockServer::start();
        let redirected = target.mock(|when, then| {
            when.method(GET).path("/redirected");
            then.status(200).json_body_obj(&serde_json::json!({
                "Events": []
            }));
        });
        let source = MockServer::start();
        source.mock(|when, then| {
            when.method(GET).path("/liveclientdata/eventdata");
            then.status(302)
                .header("Location", format!("{}/redirected", target.base_url()));
        });

        let client = LiveClient::new(source.base_url()).unwrap();
        client
            .event_data()
            .await
            .expect_err("redirect response must not reach its target");
        redirected.assert_hits(0);
    }

    #[tokio::test]
    async fn live_client_rejects_oversized_json_before_deserialization() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/liveclientdata/eventdata");
            then.status(200)
                .header("content-type", "application/json")
                .body("x".repeat(MAX_LIVE_CLIENT_JSON_BYTES + 1));
        });
        let client = LiveClient::new(server.base_url()).unwrap();

        let error = client.event_data().await.unwrap_err();

        assert!(matches!(error, Error::InvalidResponse(_)));
        assert!(error.to_string().contains("exceeds"));
    }

    fn player(
        summoner_name: &str,
        riot_id: Option<&str>,
        champion_name: &str,
        kills: u32,
        deaths: u32,
        assists: u32,
    ) -> PlayerListEntry {
        PlayerListEntry {
            summoner_name: summoner_name.into(),
            riot_id: riot_id.map(str::to_string),
            champion_name: champion_name.into(),
            team: String::new(),
            items: Vec::new(),
            summoner_spells: PlayerSummonerSpells::default(),
            scores: PlayerScores {
                kills,
                deaths,
                assists,
                creep_score: None,
            },
        }
    }

    #[test]
    fn player_summary_matches_summoner_name_or_riot_id() {
        let players = [
            player("Someone", Some("Someone#NA1"), "Ahri", 1, 2, 3),
            player("dain", Some("Dain#NA1"), "Nautilus", 3, 4, 23),
        ];

        let by_riot_id =
            player_summary_from_list_with_game_time(&players, "dain#NA1", None).unwrap();
        assert_eq!(by_riot_id.champion_name, "Nautilus");
        assert_eq!(
            (by_riot_id.kills, by_riot_id.deaths, by_riot_id.assists),
            (3, 4, 23)
        );

        let by_summoner =
            player_summary_from_list_with_game_time(&players, " DAIN ", None).unwrap();
        assert_eq!(by_summoner.champion_name, "Nautilus");
    }

    #[test]
    fn player_summary_prefers_exact_riot_id_over_same_name_fallback() {
        let players = [
            player("dain", Some("Dain#EUW"), "Ahri", 1, 2, 3),
            player("dain", Some(" dAiN # nA1 "), "Nautilus", 3, 4, 23),
        ];

        let summary = player_summary_from_list_with_game_time(&players, "Dain#NA1", None).unwrap();

        assert_eq!(summary.champion_name, "Nautilus");
        assert_eq!((summary.kills, summary.deaths, summary.assists), (3, 4, 23));
    }

    #[test]
    fn player_summary_carries_participants_and_team() {
        let players: Vec<PlayerListEntry> = serde_json::from_str(
            r#"[
              {
                "summonerName": "dain",
                "riotId": "Dain#NA1",
                "championName": "Nautilus",
                "team": "ORDER",
                "scores": { "kills": 3, "deaths": 4, "assists": 23, "creepScore": 187 }
              },
              {
                "summonerName": "Soupmaster",
                "riotId": "Soup#NA1",
                "championName": "Ahri",
                "team": "CHAOS",
                "scores": { "kills": 7, "deaths": 2, "assists": 4, "creepScore": 120 }
              }
            ]"#,
        )
        .unwrap();

        let summary =
            player_summary_from_list_with_game_time(&players, "dain#NA1", Some(1800.4)).unwrap();

        assert_eq!(summary.player_name, "dain");
        assert_eq!(summary.creep_score, Some(187));
        assert_eq!(summary.game_time_s, Some(1800));
        assert_eq!(summary.team, "ORDER");
        assert_eq!(summary.participants.len(), 2);
        assert_eq!(summary.participants[0].player_name, "dain");
        assert_eq!(summary.participants[0].champion_name, "Nautilus");
        assert_eq!(summary.participants[0].team, "ORDER");
        assert_eq!(summary.participants[1].player_name, "Soupmaster");
        assert_eq!(summary.participants[1].champion_name, "Ahri");
        assert_eq!(summary.participants[1].team, "CHAOS");
    }

    #[test]
    fn player_summary_carries_summoner_spells_and_item_build() {
        let players: Vec<PlayerListEntry> = serde_json::from_str(
            r#"[
              {
                "summonerName": "dain",
                "riotId": "Dain#NA1",
                "championName": "Vel'Koz",
                "team": "ORDER",
                "summonerSpells": {
                  "summonerSpellOne": {
                    "displayName": "Ignite",
                    "rawDisplayName": "GeneratedTip_SummonerSpell_SummonerDot_DisplayName"
                  },
                  "summonerSpellTwo": {
                    "displayName": "Flash",
                    "rawDisplayName": "GeneratedTip_SummonerSpell_SummonerFlash_DisplayName"
                  }
                },
                "items": [
                  { "itemID": 1056, "displayName": "Doran's Ring", "slot": 0 },
                  { "itemID": 3020, "displayName": "Sorcerer's Shoes", "slot": 1 },
                  { "itemID": 6655, "displayName": "Luden's Companion", "slot": 2 }
                ],
                "scores": { "kills": 11, "deaths": 19, "assists": 34, "creepScore": 204 }
              }
            ]"#,
        )
        .unwrap();

        let summary = player_summary_from_list_with_game_time(&players, "dain#NA1", None).unwrap();
        let value = serde_json::to_value(summary).unwrap();

        assert_eq!(value["summoner_spells"][0]["name"], "Ignite");
        assert_eq!(value["summoner_spells"][0]["asset_key"], "SummonerDot");
        assert_eq!(value["summoner_spells"][1]["name"], "Flash");
        assert_eq!(value["summoner_spells"][1]["asset_key"], "SummonerFlash");
        assert_eq!(value["items"][0]["id"], 1056);
        assert_eq!(value["items"][0]["name"], "Doran's Ring");
        assert_eq!(value["items"][0]["slot"], 0);
        assert_eq!(value["items"][2]["id"], 6655);
    }

    #[test]
    fn player_summary_maps_hexflash_to_flash_asset() {
        let players: Vec<PlayerListEntry> = serde_json::from_str(
            r#"[
              {
                "summonerName": "dain",
                "riotId": "Dain#NA1",
                "championName": "Pyke",
                "team": "ORDER",
                "summonerSpells": {
                  "summonerSpellOne": {
                    "displayName": "Ignite",
                    "rawDisplayName": "GeneratedTip_SummonerSpell_SummonerDot_DisplayName"
                  },
                  "summonerSpellTwo": {
                    "displayName": "Hexflash",
                    "rawDisplayName": "GeneratedTip_SummonerSpell_SummonerFlashPerksHextechFlashtraptionV2_DisplayName"
                  }
                },
                "scores": { "kills": 1, "deaths": 0, "assists": 2, "creepScore": 12 }
              }
            ]"#,
        )
        .unwrap();

        let summary = player_summary_from_list_with_game_time(&players, "dain#NA1", None).unwrap();
        let value = serde_json::to_value(summary).unwrap();

        assert_eq!(value["summoner_spells"][1]["name"], "Hexflash");
        assert_eq!(value["summoner_spells"][1]["asset_key"], "SummonerFlash");
    }

    #[test]
    fn player_summary_ignores_missing_local_player_or_champion() {
        let players = [player("dain", Some("Dain#NA1"), "", 3, 4, 23)];

        assert!(player_summary_from_list_with_game_time(&players, "other", None).is_none());
        assert!(player_summary_from_list_with_game_time(&players, "dain", None).is_none());
    }
}
