use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::LeagueQueue;

const MAX_LCU_JSON_BYTES: usize = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum LcuError {
    #[error("read League client lockfile: {0}")]
    ReadLockfile(#[from] std::io::Error),
    #[error("invalid League client lockfile: {0}")]
    InvalidLockfile(String),
    #[error("refusing non-loopback League client URL: {0}")]
    NotLoopback(String),
    #[error("League client request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("League client response is invalid: {0}")]
    InvalidResponse(String),
    #[error("League client JSON is invalid: {0}")]
    InvalidJson(#[from] serde_json::Error),
}

pub struct LcuClient {
    base: reqwest::Url,
    http: reqwest::Client,
    token: String,
}

impl std::fmt::Debug for LcuClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LcuClient")
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct GameflowSession {
    #[serde(rename = "gameData")]
    game_data: GameData,
}

#[derive(Deserialize)]
struct GameData {
    queue: QueueData,
}

#[derive(Deserialize)]
struct QueueData {
    id: i64,
}

impl LcuClient {
    pub fn new(base: impl Into<String>, token: impl Into<String>) -> Result<Self, LcuError> {
        let raw = base.into();
        let mut base = reqwest::Url::parse(&raw).map_err(|_| LcuError::NotLoopback(raw.clone()))?;
        let loopback = base
            .host_str()
            .and_then(|host| host.parse::<IpAddr>().ok())
            .is_some_and(|ip| ip.is_loopback());
        if !matches!(base.scheme(), "http" | "https")
            || !base.username().is_empty()
            || base.password().is_some()
            || !loopback
        {
            return Err(LcuError::NotLoopback(raw));
        }
        base.set_query(None);
        base.set_fragment(None);
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        let token = token.into();
        if token.is_empty() {
            return Err(LcuError::InvalidLockfile(
                "missing authentication token".into(),
            ));
        }
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .danger_accept_invalid_certs(true)
            .connect_timeout(Duration::from_secs(1))
            .read_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(2))
            .build()?;
        Ok(Self { base, http, token })
    }

    pub fn from_game_executable(game_executable: &Path) -> Result<Self, LcuError> {
        let lockfile = league_lockfile_path(game_executable).ok_or_else(|| {
            LcuError::InvalidLockfile(format!(
                "cannot derive install root from {}",
                game_executable.display()
            ))
        })?;
        let contents = std::fs::read_to_string(lockfile)?;
        let connection = parse_lockfile(&contents)?;
        Self::new(
            format!("{}://127.0.0.1:{}", connection.protocol, connection.port),
            connection.token,
        )
    }

    pub async fn current_queue(&self) -> Result<LeagueQueue, LcuError> {
        let url = self
            .base
            .join("lol-gameflow/v1/session")
            .map_err(|error| LcuError::InvalidResponse(format!("build endpoint URL: {error}")))?;
        let mut response = self
            .http
            .get(url)
            .basic_auth("riot", Some(&self.token))
            .send()
            .await?
            .error_for_status()?;
        if response
            .content_length()
            .is_some_and(|length| length > MAX_LCU_JSON_BYTES as u64)
        {
            return Err(LcuError::InvalidResponse("body exceeds 1 MiB".into()));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if body.len().saturating_add(chunk.len()) > MAX_LCU_JSON_BYTES {
                return Err(LcuError::InvalidResponse("body exceeds 1 MiB".into()));
            }
            body.extend_from_slice(&chunk);
        }
        let session: GameflowSession = serde_json::from_slice(&body)?;
        let queue_id = u32::try_from(session.game_data.queue.id).unwrap_or(0);
        Ok(LeagueQueue::from_id(queue_id))
    }
}

pub fn league_lockfile_path(game_executable: &Path) -> Option<PathBuf> {
    Some(game_executable.parent()?.parent()?.join("lockfile"))
}

struct LockfileConnection<'a> {
    port: u16,
    token: &'a str,
    protocol: &'a str,
}

fn parse_lockfile(contents: &str) -> Result<LockfileConnection<'_>, LcuError> {
    let parts: Vec<_> = contents.trim().split(':').collect();
    if parts.len() != 5 || parts[0].is_empty() || parts[3].is_empty() {
        return Err(LcuError::InvalidLockfile("expected five fields".into()));
    }
    parts[1]
        .parse::<u32>()
        .map_err(|_| LcuError::InvalidLockfile("invalid process id".into()))?;
    let port = parts[2]
        .parse::<u16>()
        .map_err(|_| LcuError::InvalidLockfile("invalid port".into()))?;
    if !matches!(parts[4], "http" | "https") {
        return Err(LcuError::InvalidLockfile("invalid protocol".into()));
    }
    Ok(LockfileConnection {
        port,
        token: parts[3],
        protocol: parts[4],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_lockfile_shape_without_exposing_process_fields() {
        let parsed = parse_lockfile("LeagueClient:1234:5678:secret:https").unwrap();
        assert_eq!(parsed.port, 5678);
        assert_eq!(parsed.token, "secret");
        assert_eq!(parsed.protocol, "https");
    }

    #[test]
    fn derives_lockfile_beside_league_game_folder() {
        let root = PathBuf::from("League of Legends");
        let executable = root.join("Game").join("League of Legends.exe");
        assert_eq!(
            league_lockfile_path(&executable),
            Some(root.join("lockfile"))
        );
    }

    #[test]
    fn rejects_malformed_or_non_loopback_configuration() {
        assert!(parse_lockfile("missing-fields").is_err());
        assert!(LcuClient::new("https://example.com:2999", "secret").is_err());
    }
}
