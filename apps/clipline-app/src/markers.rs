//! League event source (ddoc section 5a): a thread with a current-thread
//! Tokio runtime polling the Live Client Data API at roughly 1 Hz and
//! forwarding anchored events to the recorder service. The League game
//! plugin owns when this source is attached to a recorder session.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use clipline_events::{EventKind, GameEvent, PlayerSummary};
use clipline_lol::{poll_once_with_continuity, EventTracker, LcuClient, LeagueQueue, LiveClient};

const POLL_INTERVAL: Duration = Duration::from_secs(1);
const RETRY_INTERVAL: Duration = Duration::from_secs(5);
const MATCH_ABSENCE_FAILURES: u32 = 6;
const MAX_QUEUE_LOOKUP_ATTEMPTS: u8 = 5;

/// What the poller tells the service: anchored events, match boundaries, and
/// a non-semantic heartbeat used only to detect a departed consumer.
pub enum PollerMsg {
    Event(GameEvent),
    PlayerSummary(PlayerSummary),
    Queue(LeagueQueue),
    MatchStarted,
    MatchEnded,
    Heartbeat,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MatchBoundary {
    Started,
    Ended,
}

#[derive(Debug, Default)]
struct PollDecision {
    before_events: Vec<MatchBoundary>,
    end_after_events: bool,
}

#[derive(Debug, Default)]
struct MatchLifecycle {
    observed_match: bool,
    match_active: bool,
    consecutive_failures: u32,
}

#[derive(Debug, Default)]
struct QueueLookupState {
    generation: u64,
    attempts: u8,
    in_flight: bool,
    resolved: bool,
}

impl QueueLookupState {
    fn reset(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.attempts = 0;
        self.in_flight = false;
        self.resolved = false;
    }

    fn begin(&mut self) -> Option<u64> {
        if self.resolved || self.in_flight || self.attempts >= MAX_QUEUE_LOOKUP_ATTEMPTS {
            return None;
        }
        self.attempts += 1;
        self.in_flight = true;
        Some(self.generation)
    }

    fn complete(&mut self, generation: u64, success: bool) -> bool {
        if generation != self.generation || !self.in_flight {
            return false;
        }
        self.in_flight = false;
        if success {
            self.resolved = true;
        }
        success
    }
}

impl MatchLifecycle {
    fn poll_succeeded(&mut self, new_match: bool, has_game_end: bool) -> PollDecision {
        self.consecutive_failures = 0;
        let mut decision = PollDecision::default();
        if !self.observed_match {
            self.observed_match = true;
            self.match_active = true;
            decision.before_events.push(MatchBoundary::Started);
        } else if new_match {
            if self.match_active {
                decision.before_events.push(MatchBoundary::Ended);
            }
            self.match_active = true;
            decision.before_events.push(MatchBoundary::Started);
        }

        if has_game_end && self.match_active {
            self.match_active = false;
            decision.end_after_events = true;
        }
        decision
    }

    fn poll_failed(&mut self) -> Option<MatchBoundary> {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        if self.match_active && self.consecutive_failures == MATCH_ABSENCE_FAILURES {
            self.match_active = false;
            Some(MatchBoundary::Ended)
        } else {
            None
        }
    }

    fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }

    fn is_active(&self) -> bool {
        self.match_active
    }
}

fn failure_backoff(consecutive_failures: u32) -> Duration {
    let shift = consecutive_failures.saturating_sub(1).min(3);
    Duration::from_secs((1_u64 << shift).min(RETRY_INTERVAL.as_secs()))
}

fn send_heartbeat(tx: &Sender<PollerMsg>) -> bool {
    tx.send(PollerMsg::Heartbeat).is_ok()
}

fn send_boundary(tx: &Sender<PollerMsg>, boundary: MatchBoundary) -> bool {
    let message = match boundary {
        MatchBoundary::Started => PollerMsg::MatchStarted,
        MatchBoundary::Ended => PollerMsg::MatchEnded,
    };
    tx.send(message).is_ok()
}

/// Forward match boundaries, then re-issue a detected Replay tag after each
/// start. The recorder clears `league_queue` on `MatchStarted` and may have
/// switched session folders, so the early spawn-time Queue write is not enough.
fn send_match_boundaries(
    tx: &Sender<PollerMsg>,
    boundaries: &[MatchBoundary],
    replay_queue: Option<&LeagueQueue>,
) -> bool {
    for boundary in boundaries {
        if !send_boundary(tx, *boundary) {
            return false;
        }
        if *boundary == MatchBoundary::Started {
            if let Some(queue) = replay_queue {
                if tx.send(PollerMsg::Queue(queue.clone())).is_err() {
                    return false;
                }
            }
        }
    }
    true
}

/// Spawn the poller. `base_url` overrides the local Live Client endpoint
/// (mock servers in tests/demos); `recording_t0` is the wall-clock twin of
/// the capture clock origin, sampled at the same time. The League executable
/// locates the local client lockfile for best-effort queue identification.
/// `process_id` is used to detect League client replays from the command line.
pub fn spawn(
    base_url: Option<String>,
    recording_t0: Instant,
    game_executable: Option<PathBuf>,
    process_id: Option<u32>,
) -> Receiver<PollerMsg> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("clipline-lol-poller".into())
        .spawn(move || {
            let replay_queue = process_id.and_then(league_replay_queue_from_pid);
            if let Some(queue) = replay_queue.clone() {
                if tx.send(PollerMsg::Queue(queue)).is_err() {
                    return;
                }
            }
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(error) => {
                    tracing::error!(event = "lol_runtime_build_failed", error = %error);
                    return;
                }
            };
            rt.block_on(async move {
                let client = match base_url {
                    Some(url) => LiveClient::new(url),
                    None => LiveClient::default_local(),
                };
                let client = match client {
                    Ok(client) => client,
                    Err(error) => {
                        tracing::error!(event = "lol_client_create_failed", error = %error);
                        return;
                    }
                };
                let mut tracker = EventTracker::default();
                let mut lifecycle = MatchLifecycle::default();
                let mut local_player = None;
                let mut queue_lookup = QueueLookupState::default();
                if replay_queue.is_some() {
                    queue_lookup.resolved = true;
                }
                let (queue_result_tx, queue_result_rx) =
                    mpsc::channel::<(u64, Result<LeagueQueue, String>)>();

                loop {
                    while let Ok((generation, result)) = queue_result_rx.try_recv() {
                        match result {
                            Ok(queue)
                                if queue_lookup.complete(generation, true)
                                    && lifecycle.is_active() =>
                            {
                                if tx.send(PollerMsg::Queue(queue)).is_err() {
                                    return;
                                }
                            }
                            Ok(_) => {}
                            Err(error) => {
                                queue_lookup.complete(generation, false);
                                tracing::debug!(
                                    event = "lol_queue_lookup_failed",
                                    error = %error,
                                );
                            }
                        }
                    }
                    // Outside a game, the endpoint normally refuses requests.
                    if local_player.is_none() {
                        match client.active_player_name().await {
                            Ok(name) => local_player = Some(name),
                            Err(_) => {
                                if !send_heartbeat(&tx) {
                                    return;
                                }
                                tokio::time::sleep(RETRY_INTERVAL).await;
                                continue;
                            }
                        }
                    }
                    let Some(player) = local_player.as_deref() else {
                        continue;
                    };

                    match poll_once_with_continuity(
                        &client,
                        &mut tracker,
                        player,
                        recording_t0,
                        0.0,
                    )
                    .await
                    {
                        Ok(batch) => {
                            let has_game_end = batch
                                .events
                                .iter()
                                .any(|event| event.kind == EventKind::GameEnd);
                            let decision = lifecycle.poll_succeeded(batch.new_match, has_game_end);
                            if decision.before_events.contains(&MatchBoundary::Started)
                                && replay_queue.is_none()
                            {
                                queue_lookup.reset();
                            }
                            if !send_match_boundaries(
                                &tx,
                                &decision.before_events,
                                replay_queue.as_ref(),
                            ) {
                                return;
                            }
                            if let Ok(Some(summary)) = client.player_summary(player).await {
                                if tx.send(PollerMsg::PlayerSummary(summary)).is_err() {
                                    return;
                                }
                            }
                            for event in batch.events {
                                if tx.send(PollerMsg::Event(event)).is_err() {
                                    return;
                                }
                            }
                            if lifecycle.is_active() {
                                if let Some(executable) = game_executable.clone() {
                                    if let Some(generation) = queue_lookup.begin() {
                                        let queue_result_tx = queue_result_tx.clone();
                                        tokio::spawn(async move {
                                            let result =
                                                match LcuClient::from_game_executable(&executable) {
                                                    Ok(client) => client
                                                        .current_queue()
                                                        .await
                                                        .map_err(|error| error.to_string()),
                                                    Err(error) => Err(error.to_string()),
                                                };
                                            let _ = queue_result_tx.send((generation, result));
                                        });
                                    }
                                }
                            }
                            if decision.end_after_events
                                && !send_boundary(&tx, MatchBoundary::Ended)
                            {
                                return;
                            }
                            tokio::time::sleep(POLL_INTERVAL).await;
                        }
                        Err(_) => {
                            if lifecycle
                                .poll_failed()
                                .is_some_and(|boundary| !send_boundary(&tx, boundary))
                            {
                                return;
                            }
                            if !send_heartbeat(&tx) {
                                return;
                            }
                            let failures = lifecycle.consecutive_failures();
                            if failures >= MATCH_ABSENCE_FAILURES {
                                // Re-acquire identity after sustained absence,
                                // but retain tracker state across the outage.
                                local_player = None;
                            }
                            tokio::time::sleep(failure_backoff(failures)).await;
                        }
                    }
                }
            });
        })
        .expect("spawn lol poller thread");
    rx
}

fn league_replay_queue_from_pid(process_id: u32) -> Option<LeagueQueue> {
    crate::windows::process_command_line(process_id)
        .filter(|command_line| clipline_lol::is_league_replay_command_line(command_line))
        .map(|_| LeagueQueue::replay())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_process_is_not_a_league_replay() {
        assert_eq!(league_replay_queue_from_pid(std::process::id()), None);
        assert_eq!(league_replay_queue_from_pid(0), None);
    }

    #[test]
    fn match_start_reemits_a_detected_replay_queue() {
        let (tx, rx) = mpsc::channel();
        let queue = LeagueQueue::replay();
        assert!(send_match_boundaries(
            &tx,
            &[MatchBoundary::Started],
            Some(&queue),
        ));
        assert!(matches!(rx.try_recv(), Ok(PollerMsg::MatchStarted)));
        match rx.try_recv() {
            Ok(PollerMsg::Queue(got)) => assert_eq!(got, queue),
            Ok(_) => panic!("expected replay queue after MatchStarted"),
            Err(_) => panic!("missing replay queue after MatchStarted"),
        }
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn match_end_does_not_reemit_replay_queue() {
        let (tx, rx) = mpsc::channel();
        assert!(send_match_boundaries(
            &tx,
            &[MatchBoundary::Ended],
            Some(&LeagueQueue::replay()),
        ));
        assert!(matches!(rx.try_recv(), Ok(PollerMsg::MatchEnded)));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn live_match_start_does_not_invent_a_replay_queue() {
        let (tx, rx) = mpsc::channel();
        assert!(send_match_boundaries(&tx, &[MatchBoundary::Started], None));
        assert!(matches!(rx.try_recv(), Ok(PollerMsg::MatchStarted)));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn transient_failures_do_not_end_or_restart_a_match() {
        let mut lifecycle = MatchLifecycle::default();
        let started = lifecycle.poll_succeeded(false, false);
        assert_eq!(started.before_events, vec![MatchBoundary::Started]);

        for _ in 0..(MATCH_ABSENCE_FAILURES - 1) {
            assert_eq!(lifecycle.poll_failed(), None);
        }

        let recovered = lifecycle.poll_succeeded(false, false);
        assert!(recovered.before_events.is_empty());
        assert!(!recovered.end_after_events);
    }

    #[test]
    fn sustained_absence_ends_once_without_forgetting_match_identity() {
        let mut lifecycle = MatchLifecycle::default();
        lifecycle.poll_succeeded(false, false);

        for _ in 0..(MATCH_ABSENCE_FAILURES - 1) {
            assert_eq!(lifecycle.poll_failed(), None);
        }
        assert_eq!(lifecycle.poll_failed(), Some(MatchBoundary::Ended));
        assert_eq!(lifecycle.poll_failed(), None, "end is emitted only once");

        let same_match = lifecycle.poll_succeeded(false, false);
        assert!(same_match.before_events.is_empty());

        let new_match = lifecycle.poll_succeeded(true, false);
        assert_eq!(new_match.before_events, vec![MatchBoundary::Started]);
    }

    #[test]
    fn explicit_game_end_closes_once_and_new_match_restarts() {
        let mut lifecycle = MatchLifecycle::default();
        lifecycle.poll_succeeded(false, false);

        let ended = lifecycle.poll_succeeded(false, true);
        assert!(ended.before_events.is_empty());
        assert!(ended.end_after_events);

        let lingering_endpoint = lifecycle.poll_succeeded(false, true);
        assert!(lingering_endpoint.before_events.is_empty());
        assert!(!lingering_endpoint.end_after_events);

        let next = lifecycle.poll_succeeded(true, false);
        assert_eq!(next.before_events, vec![MatchBoundary::Started]);
    }

    #[test]
    fn new_match_signal_ends_active_match_before_starting_next() {
        let mut lifecycle = MatchLifecycle::default();
        lifecycle.poll_succeeded(false, false);

        let next = lifecycle.poll_succeeded(true, false);
        assert_eq!(
            next.before_events,
            vec![MatchBoundary::Ended, MatchBoundary::Started]
        );
    }

    #[test]
    fn heartbeat_detects_a_dropped_receiver() {
        let (tx, rx) = mpsc::channel();
        assert!(send_heartbeat(&tx));
        assert!(matches!(rx.recv().unwrap(), PollerMsg::Heartbeat));
        drop(rx);
        assert!(!send_heartbeat(&tx));
    }

    #[test]
    fn retry_backoff_is_bounded() {
        assert_eq!(failure_backoff(1), Duration::from_secs(1));
        assert_eq!(failure_backoff(2), Duration::from_secs(2));
        assert_eq!(failure_backoff(3), Duration::from_secs(4));
        assert_eq!(failure_backoff(20), RETRY_INTERVAL);
    }

    #[test]
    fn queue_lookup_retries_failure_and_stops_after_success() {
        let mut lookup = QueueLookupState::default();
        let generation = lookup.begin().expect("first lookup starts");

        assert!(!lookup.complete(generation, false));
        let retry = lookup.begin().expect("failure permits a retry");
        assert_eq!(retry, generation);
        assert!(lookup.complete(retry, true));
        assert!(lookup.begin().is_none(), "success resolves this match");
    }

    #[test]
    fn queue_lookup_ignores_a_completion_from_the_previous_match() {
        let mut lookup = QueueLookupState::default();
        let old_generation = lookup.begin().unwrap();
        lookup.reset();

        assert!(!lookup.complete(old_generation, true));
        assert!(lookup.begin().is_some());
    }
}
