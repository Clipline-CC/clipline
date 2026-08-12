//! Marker accumulation during a recording and extraction into saved clips
//! (ddoc §5: normalized events land on the recording timeline; saved clips
//! carry the markers inside their window, re-based to clip time).

use serde::{Deserialize, Serialize};

use crate::schema::{GameEvent, GameId};

/// All anchored events of the current recording session, in arrival order.
#[derive(Debug, Default)]
pub struct MarkerLog {
    events: Vec<GameEvent>, // every entry has recording_offset_s = Some
    /// User-placed bookmark offsets on the recording timeline, in arrival order.
    bookmarks: Vec<f64>,
    retained_from_s: Option<f64>,
}

/// One marker inside a saved clip, `t_s` seconds from clip start.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipMarker {
    pub t_s: f64,
    #[serde(flatten)]
    pub event: GameEvent,
}

/// One user-placed bookmark inside a saved clip, `t_s` seconds from clip start.
/// A struct rather than a bare offset so an optional label can be added later
/// without migrating existing sidecars.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ClipBookmark {
    pub t_s: f64,
}

/// One player in a game adapter's match summary roster.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayerParticipant {
    pub player_name: String,
    pub champion_name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub team: String,
}

/// One summoner spell in a game adapter's match summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayerSummonerSpell {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub asset_key: String,
}

/// One item in a game adapter's match summary build.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayerItem {
    pub id: u32,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<u32>,
}

/// Per-player match summary shown in library rows when a game adapter can
/// provide it. Extra participant fields are optional so older sidecars remain
/// readable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayerSummary {
    pub champion_name: String,
    pub kills: u32,
    pub deaths: u32,
    pub assists: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creep_score: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub game_time_s: Option<u32>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub player_name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub team: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub participants: Vec<PlayerParticipant>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub summoner_spells: Vec<PlayerSummonerSpell>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<PlayerItem>,
}

/// One user-facing audio stream inside a saved clip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipAudioTrack {
    /// Stable id for UI/upload selection, e.g. "output" or "microphone".
    pub id: String,
    /// Zero-based audio-track index in the MP4, excluding the video track.
    pub track_index: u32,
    pub label: String,
    /// Machine-readable source kind for future process/game audio.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

/// One score/play interval inside a saved clip.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClipPlay {
    pub game_id: GameId,
    pub source: String,
    pub external_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub beatmap_id: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub beatmapset_id: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cover_url: Option<String>,
    pub title: String,
    pub artist: String,
    pub difficulty: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mapper: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub star_rating: Option<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mods: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rank: Option<String>,
    pub passed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accuracy: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_combo: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_score: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pp: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    pub ended_at: String,
    pub derived_start: bool,
    pub t_start_s: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t_end_s: Option<f64>,
}

/// The `<clip>.markers.json` sidecar document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipMarkers {
    /// Recording-timeline range the clip covers.
    pub recording_start_s: f64,
    pub duration_s: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub player_summary: Option<PlayerSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub audio_tracks: Vec<ClipAudioTrack>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plays: Vec<ClipPlay>,
    pub markers: Vec<ClipMarker>,
    /// User-placed bookmarks, ordered by time. Absent from clips without any,
    /// and defaulted so sidecars written before bookmarks existed still read.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bookmarks: Vec<ClipBookmark>,
}

impl MarkerLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Unanchored events (no recording offset yet) are dropped — they
    /// cannot be placed on the timeline.
    pub fn push(&mut self, event: GameEvent) {
        let Some(offset_s) = event.recording_offset_s else {
            return;
        };
        if self
            .retained_from_s
            .is_some_and(|retained_from_s| offset_s < retained_from_s)
        {
            return;
        }
        self.events.push(event);
    }

    /// Record a user-placed bookmark at `offset_s` on the recording timeline.
    /// Like `push`, a bookmark behind the retained media front is dropped: the
    /// recorder can no longer put that moment in a clip.
    pub fn push_bookmark(&mut self, offset_s: f64) {
        if !offset_s.is_finite() {
            return;
        }
        if self
            .retained_from_s
            .is_some_and(|retained_from_s| offset_s < retained_from_s)
        {
            return;
        }
        self.bookmarks.push(offset_s);
    }

    /// Discard markers that the recorder can no longer include in a future
    /// replay. The caller supplies the actual oldest retained media timestamp,
    /// including keyframe coverage and encoder lag, rather than deriving a
    /// cutoff from wall time.
    pub fn retain_from_recording_offset(&mut self, retained_from_s: f64) {
        if !retained_from_s.is_finite() {
            return;
        }
        let retained_from_s = self
            .retained_from_s
            .map_or(retained_from_s, |current| current.max(retained_from_s));
        self.retained_from_s = Some(retained_from_s);
        self.events.retain(|event| {
            event
                .recording_offset_s
                .is_some_and(|offset_s| offset_s >= retained_from_s)
        });
        self.bookmarks
            .retain(|offset_s| *offset_s >= retained_from_s);
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Markers and bookmarks within [start, end), re-based to clip time.
    pub fn clip_markers(&self, start_s: f64, end_s: f64) -> ClipMarkers {
        let markers = self
            .events
            .iter()
            .filter_map(|e| {
                let off = e.recording_offset_s?;
                (off >= start_s && off < end_s).then(|| ClipMarker {
                    t_s: off - start_s,
                    event: e.clone(),
                })
            })
            .collect();
        // Bookmarks arrive in press order, which is already time order; sorting
        // keeps the sidecar contract explicit for the review timeline.
        let mut bookmarks = self
            .bookmarks
            .iter()
            .filter(|off| **off >= start_s && **off < end_s)
            .map(|off| ClipBookmark { t_s: off - start_s })
            .collect::<Vec<_>>();
        bookmarks.sort_by(|a, b| a.t_s.total_cmp(&b.t_s));
        ClipMarkers {
            recording_start_s: start_s,
            duration_s: end_s - start_s,
            player_summary: None,
            audio_tracks: Vec::new(),
            plays: Vec::new(),
            markers,
            bookmarks,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::EventKind;

    fn ev(kind: EventKind, offset_s: f64) -> GameEvent {
        GameEvent {
            game_id: GameId::LeagueOfLegends,
            kind,
            actor: "Dain".into(),
            victim: None,
            assisters: Vec::new(),
            subtype: None,
            game_time_s: 0.0,
            recording_offset_s: Some(offset_s),
            importance: 5,
            involves_local_player: true,
        }
    }

    #[test]
    fn clip_markers_filters_and_rebases_to_clip_start() {
        let mut log = MarkerLog::new();
        log.push(ev(EventKind::ChampionKill, 10.0));
        log.push(ev(EventKind::DragonKill, 70.0));
        log.push(ev(EventKind::BaronKill, 130.0));
        let clip = log.clip_markers(60.0, 120.0);
        assert_eq!(
            clip.markers.len(),
            1,
            "only the dragon is inside the window"
        );
        assert!(
            (clip.markers[0].t_s - 10.0).abs() < 1e-9,
            "70s − 60s clip start"
        );
        assert_eq!(clip.markers[0].event.kind, EventKind::DragonKill);
        assert!((clip.duration_s - 60.0).abs() < 1e-9);
    }

    #[test]
    fn unanchored_events_are_ignored() {
        let mut log = MarkerLog::new();
        let mut e = ev(EventKind::ChampionKill, 0.0);
        e.recording_offset_s = None;
        log.push(e);
        assert_eq!(log.clip_markers(0.0, 100.0).markers.len(), 0);
        assert_eq!(log.len(), 0);
    }

    #[test]
    fn boundary_inclusive_start_exclusive_end() {
        let mut log = MarkerLog::new();
        log.push(ev(EventKind::ChampionKill, 60.0));
        log.push(ev(EventKind::Ace, 120.0));
        let clip = log.clip_markers(60.0, 120.0);
        assert_eq!(clip.markers.len(), 1);
        assert_eq!(clip.markers[0].event.kind, EventKind::ChampionKill);
    }

    #[test]
    fn replay_pruning_uses_actual_oldest_media_and_keeps_keyframe_lead_in() {
        let mut log = MarkerLog::new();
        log.push(ev(EventKind::ChampionKill, 0.0));
        log.push(ev(EventKind::DragonKill, 8.0));
        log.push(ev(EventKind::BaronKill, 9.5));
        log.push(ev(EventKind::Ace, 12.0));

        // A nominal ten-second cutoff at t=20 would be 10.0, but the actual
        // saved MP4 begins at its covering keyframe at 8.0.
        log.retain_from_recording_offset(8.0);

        let clip = log.clip_markers(0.0, 20.0);
        let kinds: Vec<_> = clip
            .markers
            .iter()
            .map(|marker| marker.event.kind)
            .collect();
        assert_eq!(
            kinds,
            [EventKind::DragonKill, EventKind::BaronKill, EventKind::Ace]
        );
        assert_eq!(log.len(), 3);
    }

    #[test]
    fn replay_log_does_not_retain_late_events_before_the_media_front() {
        let mut log = MarkerLog::new();
        log.retain_from_recording_offset(60.0);
        log.push(ev(EventKind::Ace, 120.0));
        log.push(ev(EventKind::ChampionKill, 20.0));

        assert_eq!(log.len(), 1);
        assert_eq!(
            log.clip_markers(0.0, 121.0).markers[0].event.kind,
            EventKind::Ace
        );
    }

    #[test]
    fn replay_media_front_prunes_without_new_markers_and_never_moves_backward() {
        let mut log = MarkerLog::new();
        log.push(ev(EventKind::ChampionKill, 0.0));
        log.push(ev(EventKind::DragonKill, 30.0));

        log.retain_from_recording_offset(60.0);
        log.retain_from_recording_offset(50.0);
        log.push(ev(EventKind::Ace, 55.0));

        assert!(log.is_empty());
    }

    #[test]
    fn full_session_log_keeps_markers_older_than_the_replay_window() {
        let mut log = MarkerLog::new();
        log.push(ev(EventKind::ChampionKill, 0.0));
        log.push(ev(EventKind::Ace, 120.0));

        assert_eq!(log.len(), 2);
    }

    #[test]
    fn bookmarks_filter_and_rebase_to_clip_start() {
        let mut log = MarkerLog::new();
        log.push_bookmark(10.0);
        log.push_bookmark(70.0);
        log.push_bookmark(120.0);

        let clip = log.clip_markers(60.0, 120.0);

        assert_eq!(
            clip.bookmarks,
            [ClipBookmark { t_s: 10.0 }],
            "inclusive start, exclusive end, re-based to clip time"
        );
    }

    #[test]
    fn bookmarks_are_ordered_by_time() {
        let mut log = MarkerLog::new();
        log.push_bookmark(30.0);
        log.push_bookmark(10.0);
        log.push_bookmark(20.0);

        let times: Vec<_> = log
            .clip_markers(0.0, 60.0)
            .bookmarks
            .iter()
            .map(|bookmark| bookmark.t_s)
            .collect();

        assert_eq!(times, [10.0, 20.0, 30.0]);
    }

    #[test]
    fn non_finite_bookmarks_are_ignored() {
        let mut log = MarkerLog::new();
        log.push_bookmark(f64::NAN);
        log.push_bookmark(f64::INFINITY);

        assert!(log.clip_markers(0.0, 60.0).bookmarks.is_empty());
    }

    #[test]
    fn replay_pruning_discards_bookmarks_behind_the_media_front() {
        let mut log = MarkerLog::new();
        log.push_bookmark(5.0);
        log.push_bookmark(12.0);

        log.retain_from_recording_offset(8.0);
        // A press that arrives late but names a moment already gone stays gone.
        log.push_bookmark(6.0);

        assert_eq!(
            log.clip_markers(0.0, 20.0).bookmarks,
            [ClipBookmark { t_s: 12.0 }]
        );
    }

    #[test]
    fn full_session_log_keeps_bookmarks_older_than_the_replay_window() {
        let mut log = MarkerLog::new();
        log.push_bookmark(0.0);
        log.push_bookmark(600.0);

        assert_eq!(log.clip_markers(0.0, 900.0).bookmarks.len(), 2);
    }

    #[test]
    fn sidecar_round_trips_bookmarks_and_omits_them_when_empty() {
        let mut log = MarkerLog::new();
        log.push_bookmark(65.0);
        let clip = log.clip_markers(60.0, 120.0);

        let json = serde_json::to_string_pretty(&clip).unwrap();
        let back: ClipMarkers = serde_json::from_str(&json).unwrap();
        assert_eq!(back.bookmarks, [ClipBookmark { t_s: 5.0 }]);

        let empty = MarkerLog::new().clip_markers(0.0, 10.0);
        let empty_json = serde_json::to_string(&empty).unwrap();
        assert!(
            !empty_json.contains("bookmarks"),
            "clips without bookmarks gain no sidecar field: {empty_json}"
        );
    }

    #[test]
    fn sidecar_written_before_bookmarks_existed_still_reads() {
        let json = r#"{
          "recording_start_s": 0.0,
          "duration_s": 1.0,
          "markers": []
        }"#;

        let back: ClipMarkers = serde_json::from_str(json).unwrap();

        assert!(back.bookmarks.is_empty());
    }

    #[test]
    fn sidecar_serializes_round_trip() {
        let mut log = MarkerLog::new();
        log.push(ev(EventKind::ChampionKill, 65.0));
        let mut clip = log.clip_markers(60.0, 120.0);
        clip.player_summary = Some(PlayerSummary {
            champion_name: "Nautilus".into(),
            kills: 3,
            deaths: 4,
            assists: 23,
            creep_score: Some(187),
            game_time_s: Some(1800),
            player_name: String::new(),
            team: String::new(),
            participants: Vec::new(),
            summoner_spells: Vec::new(),
            items: Vec::new(),
        });
        clip.audio_tracks = vec![ClipAudioTrack {
            id: "output".into(),
            track_index: 0,
            label: "Output Audio".into(),
            kind: Some("output".into()),
        }];
        let json = serde_json::to_string_pretty(&clip).unwrap();
        let back: ClipMarkers = serde_json::from_str(&json).unwrap();
        assert_eq!(back.markers.len(), 1);
        assert_eq!(back.audio_tracks, clip.audio_tracks);
        assert!((back.markers[0].t_s - 5.0).abs() < 1e-9);
        assert_eq!(
            back.player_summary.as_ref().map(|summary| (
                summary.champion_name.as_str(),
                summary.kills,
                summary.deaths,
                summary.assists,
                summary.creep_score,
                summary.game_time_s
            )),
            Some(("Nautilus", 3, 4, 23, Some(187), Some(1800)))
        );
    }

    #[test]
    fn sidecar_without_player_summary_still_round_trips() {
        let json = r#"{
          "recording_start_s": 0.0,
          "duration_s": 1.0,
          "markers": []
        }"#;
        let back: ClipMarkers = serde_json::from_str(json).unwrap();
        assert!(back.player_summary.is_none());
        assert!(back.audio_tracks.is_empty());
        assert!(back.markers.is_empty());
        assert!(back.plays.is_empty());
    }

    #[test]
    fn sidecar_serializes_osu_play_round_trip() {
        let clip = ClipMarkers {
            recording_start_s: 120.0,
            duration_s: 180.0,
            player_summary: None,
            audio_tracks: Vec::new(),
            markers: Vec::new(),
            bookmarks: Vec::new(),
            plays: vec![ClipPlay {
                game_id: GameId::Osu,
                source: "osu_api".into(),
                external_id: "solo-score:42".into(),
                url: Some("https://osu.ppy.sh/scores/42".into()),
                beatmap_id: Some(123),
                beatmapset_id: Some(456),
                cover_url: Some("https://assets.ppy.sh/beatmaps/456/covers/list.jpg".into()),
                title: "Everything will freeze".into(),
                artist: "UNDEAD CORPORATION".into(),
                difficulty: "Time Freeze".into(),
                mapper: Some("Ekoro".into()),
                star_rating: Some(6.42),
                mods: vec!["DT".into(), "HD".into()],
                rank: Some("A".into()),
                passed: false,
                accuracy: Some(0.9345),
                max_combo: Some(789),
                total_score: Some(1234567),
                pp: None,
                started_at: None,
                ended_at: "2026-06-30T23:55:00+00:00".into(),
                derived_start: true,
                t_start_s: 5.0,
                t_end_s: Some(95.0),
            }],
        };

        let json = serde_json::to_string_pretty(&clip).unwrap();
        let back: ClipMarkers = serde_json::from_str(&json).unwrap();

        assert_eq!(back.plays, clip.plays);
    }

    #[test]
    fn player_summary_defaults_missing_participant_data() {
        let json = r#"{
          "champion_name": "Nautilus",
          "kills": 3,
          "deaths": 4,
          "assists": 23
        }"#;

        let summary: PlayerSummary = serde_json::from_str(json).unwrap();

        assert_eq!(summary.champion_name, "Nautilus");
        assert_eq!((summary.kills, summary.deaths, summary.assists), (3, 4, 23));
        assert_eq!(summary.creep_score, None);
        assert_eq!(summary.game_time_s, None);
        assert!(summary.player_name.is_empty());
        assert!(summary.team.is_empty());
        assert!(summary.participants.is_empty());
        assert!(summary.summoner_spells.is_empty());
        assert!(summary.items.is_empty());
    }
}
