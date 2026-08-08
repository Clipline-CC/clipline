use clipline_lol::{LcuClient, LeagueQueue, LeagueQueueCategory, LiveClient};
use httpmock::prelude::*;
use serde_json::json;

#[tokio::test]
async fn fetches_and_parses_all_three_endpoints() {
    let server = MockServer::start();

    server.mock(|when, then| {
        when.method(GET).path("/liveclientdata/eventdata");
        then.status(200).json_body(json!({
            "Events": [
                { "EventID": 0, "EventName": "GameStart", "EventTime": 0.05 }
            ]
        }));
    });
    server.mock(|when, then| {
        when.method(GET).path("/liveclientdata/activeplayername");
        then.status(200).json_body(json!("Me#NA1"));
    });
    server.mock(|when, then| {
        when.method(GET).path("/liveclientdata/gamestats");
        then.status(200).json_body(json!({
            "gameMode": "CLASSIC", "gameTime": 123.5, "mapName": "Map11"
        }));
    });
    server.mock(|when, then| {
        when.method(GET).path("/liveclientdata/playerlist");
        then.status(200).json_body(json!([
            {
                "summonerName": "Me",
                "riotId": "Me#NA1",
                "championName": "Nautilus",
                "scores": { "kills": 3, "deaths": 4, "assists": 23, "creepScore": 187 }
            }
        ]));
    });

    let client = LiveClient::new(server.base_url()).unwrap();
    let data = client.event_data().await.unwrap();
    assert_eq!(data.events.len(), 1);
    assert_eq!(client.active_player_name().await.unwrap(), "Me#NA1");
    assert!((client.game_time_s().await.unwrap() - 123.5).abs() < 1e-9);
    let summary = client.player_summary("Me#NA1").await.unwrap().unwrap();
    assert_eq!(summary.champion_name, "Nautilus");
    assert_eq!((summary.kills, summary.deaths, summary.assists), (3, 4, 23));
    assert_eq!(summary.creep_score, Some(187));
    assert_eq!(summary.game_time_s, Some(123));
}

#[tokio::test]
async fn connection_refused_is_an_error_not_a_panic() {
    // Nothing listens on this port.
    let client = LiveClient::new("http://127.0.0.1:9").unwrap();
    assert!(client.event_data().await.is_err());
}

#[tokio::test]
async fn lcu_gameflow_session_returns_authenticated_queue_tag() {
    let server = MockServer::start();
    let request = server.mock(|when, then| {
        when.method(GET)
            .path("/lol-gameflow/v1/session")
            .header("authorization", "Basic cmlvdDp0b2tlbg==");
        then.status(200).json_body(json!({
            "gameData": { "queue": { "id": 420 } }
        }));
    });

    let client = LcuClient::new(server.base_url(), "token").unwrap();
    let queue = client.current_queue().await.unwrap();

    request.assert();
    assert_eq!(queue, LeagueQueue::from_id(420));
    assert_eq!(queue.category, LeagueQueueCategory::RankedSoloDuo);
    assert_eq!(queue.label, "Ranked Solo/Duo");
}

#[test]
fn common_queue_ids_map_to_stable_user_categories() {
    let cases = [
        (420, LeagueQueueCategory::RankedSoloDuo, "Ranked Solo/Duo"),
        (440, LeagueQueueCategory::RankedFlex, "Ranked Flex"),
        (400, LeagueQueueCategory::Normal, "Normal Draft"),
        (490, LeagueQueueCategory::Normal, "Quickplay"),
        (450, LeagueQueueCategory::Aram, "ARAM"),
        (1700, LeagueQueueCategory::Arena, "Arena"),
        (0, LeagueQueueCategory::Custom, "Custom"),
        (2300, LeagueQueueCategory::Other, "Brawl"),
        (999_999, LeagueQueueCategory::Other, "Other"),
    ];

    for (id, category, label) in cases {
        let queue = LeagueQueue::from_id(id);
        assert_eq!(queue.id, id);
        assert_eq!(queue.category, category);
        assert_eq!(queue.label, label);
    }
}
