use std::ffi::OsString;
use std::fmt;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpikeScenario {
    Interactive,
    ReviewIdle,
    ReviewPlaying,
    ScrubStorm,
    RevealClose100,
}

impl SpikeScenario {
    fn parse(value: &str) -> Result<Self, OptionsError> {
        match value {
            "interactive" => Ok(Self::Interactive),
            "review-idle" => Ok(Self::ReviewIdle),
            "review-playing" => Ok(Self::ReviewPlaying),
            "scrub-storm" => Ok(Self::ScrubStorm),
            "reveal-close-100" => Ok(Self::RevealClose100),
            _ => Err(OptionsError::InvalidScenario(value.to_owned())),
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::ReviewIdle => "review-idle",
            Self::ReviewPlaying => "review-playing",
            Self::ScrubStorm => "scrub-storm",
            Self::RevealClose100 => "reveal-close-100",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpikeOptions {
    pub fixture: Option<PathBuf>,
    pub renderer: String,
    pub cpu_frame_diagnostic: bool,
    pub exit_after_ready: bool,
    pub autostart: bool,
    pub scenario: SpikeScenario,
    pub marker_path: Option<PathBuf>,
    pub stop_path: Option<PathBuf>,
    pub telemetry_path: Option<PathBuf>,
    pub settings_profile: Option<PathBuf>,
}

impl Default for SpikeOptions {
    fn default() -> Self {
        Self {
            fixture: None,
            renderer: "winit-software".to_owned(),
            cpu_frame_diagnostic: false,
            exit_after_ready: false,
            autostart: false,
            scenario: SpikeScenario::Interactive,
            marker_path: None,
            stop_path: None,
            telemetry_path: None,
            settings_profile: None,
        }
    }
}

impl SpikeOptions {
    pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Self, OptionsError> {
        let mut options = Self::default();
        let mut args = args.into_iter();
        let _program = args.next();
        while let Some(argument) = args.next() {
            let argument = argument.to_string_lossy();
            match argument.as_ref() {
                "--fixture" => {
                    options.fixture = Some(PathBuf::from(next_value(&mut args, "--fixture")?))
                }
                "--renderer" => {
                    let renderer = next_value(&mut args, "--renderer")?
                        .to_string_lossy()
                        .into_owned();
                    if renderer != "winit-software" {
                        return Err(OptionsError::UnsupportedRenderer(renderer));
                    }
                    options.renderer = renderer;
                }
                "--cpu-frame-diagnostic" => options.cpu_frame_diagnostic = true,
                "--exit-after-ready" => options.exit_after_ready = true,
                "--autostart" => options.autostart = true,
                "--scenario" => {
                    options.scenario = SpikeScenario::parse(
                        &next_value(&mut args, "--scenario")?.to_string_lossy(),
                    )?;
                }
                "--marker-path" => {
                    options.marker_path =
                        Some(PathBuf::from(next_value(&mut args, "--marker-path")?));
                }
                "--stop-path" => {
                    options.stop_path = Some(PathBuf::from(next_value(&mut args, "--stop-path")?));
                }
                "--telemetry-path" => {
                    options.telemetry_path =
                        Some(PathBuf::from(next_value(&mut args, "--telemetry-path")?));
                }
                "--settings-profile" => {
                    options.settings_profile =
                        Some(PathBuf::from(next_value(&mut args, "--settings-profile")?));
                }
                "--help" | "-h" => return Err(OptionsError::HelpRequested),
                _ => return Err(OptionsError::UnknownArgument(argument.into_owned())),
            }
        }
        if options.scenario != SpikeScenario::Interactive && options.fixture.is_none() {
            return Err(OptionsError::FixtureRequired(options.scenario));
        }
        Ok(options)
    }

    pub const fn usage() -> &'static str {
        "clipline-slint-spike [--fixture <mp4>] [--renderer winit-software] \
         [--cpu-frame-diagnostic] [--exit-after-ready] [--scenario interactive|review-idle|review-playing|scrub-storm|reveal-close-100] \
         [--autostart] [--marker-path <jsonl>] [--stop-path <file>] [--telemetry-path <json>] \
         [--settings-profile <isolated-directory>]"
    }
}

fn next_value(
    args: &mut impl Iterator<Item = OsString>,
    option: &'static str,
) -> Result<OsString, OptionsError> {
    args.next().ok_or(OptionsError::MissingValue(option))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OptionsError {
    HelpRequested,
    UnknownArgument(String),
    MissingValue(&'static str),
    UnsupportedRenderer(String),
    InvalidScenario(String),
    FixtureRequired(SpikeScenario),
}

impl fmt::Display for OptionsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for OptionsError {}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReadinessMarker<'a> {
    schema_version: u8,
    kind: &'a str,
    timestamp_utc: String,
    detail: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LifecycleMarker<'a> {
    schema_version: u8,
    kind: &'a str,
    timestamp_utc: String,
    detail: &'a str,
    lifecycle: &'a serde_json::Value,
}

fn marker_write_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

pub fn write_marker(path: &PathBuf, kind: &str, detail: &str) -> Result<(), std::io::Error> {
    let marker = ReadinessMarker {
        schema_version: 1,
        kind,
        timestamp_utc: utc_timestamp(SystemTime::now()),
        detail,
    };
    append_marker(path, &marker)
}

pub fn write_lifecycle_marker(
    path: &PathBuf,
    kind: &str,
    detail: &str,
    lifecycle: &serde_json::Value,
) -> Result<(), std::io::Error> {
    let marker = LifecycleMarker {
        schema_version: 1,
        kind,
        timestamp_utc: utc_timestamp(SystemTime::now()),
        detail,
        lifecycle,
    };
    append_marker(path, &marker)
}

fn append_marker(path: &PathBuf, marker: &impl Serialize) -> Result<(), std::io::Error> {
    let mut record = serde_json::to_vec(marker)?;
    record.push(b'\n');
    let _guard = marker_write_lock()
        .lock()
        .map_err(|_| std::io::Error::other("marker write lock poisoned"))?;
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(&record)?;
    file.flush()
}

fn utc_timestamp(now: SystemTime) -> String {
    let duration = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    let seconds = i64::try_from(duration.as_secs()).unwrap_or(i64::MAX);
    let days = seconds.div_euclid(86_400);
    let seconds_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = seconds_of_day % 3_600 / 60;
    let second = seconds_of_day % 60;
    format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{:03}Z",
        duration.subsec_millis()
    )
}

fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let days = days_since_epoch + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::{utc_timestamp, UNIX_EPOCH};
    use std::time::Duration;

    #[test]
    fn timestamp_is_frontend_marker_compatible() {
        let timestamp = utc_timestamp(UNIX_EPOCH + Duration::from_millis(1_719_843_845_678));
        assert_eq!(timestamp, "2024-07-01T14:24:05.678Z");
    }
}
