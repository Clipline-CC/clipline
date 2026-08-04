//! Transaction ordering for applying UI-owned settings to live runtime state.
//!
//! Every operation through durable preference publication is fallible and owns
//! an exact rollback receipt. Recorder and detector activation, prepared storage/media
//! authorization, and desktop publication happen only after persistence and
//! therefore form an infallible commit tail.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use thiserror::Error;

use crate::{AppSettings, SettingsPreferences, SettingsSnapshot};

/// Narrow application boundary consumed by [`apply_settings`].
///
/// A frontend adapter may implement every port on one struct, but each method
/// retains one responsibility and every reversible mutation returns an owned
/// receipt. Implementations must not retain controller/UI locks across calls.
pub trait SettingsApplyPorts {
    type PreparedPreflight;
    type HotkeyReceipt;
    type TrayReceipt;
    type AutostartReceipt;
    type PreparedDetector;
    type PreparedRecorder;

    fn prepare_preflight(
        &mut self,
        baseline: &SettingsPreferences,
        candidate: &SettingsPreferences,
    ) -> Result<Self::PreparedPreflight, String>;

    fn apply_hotkeys(
        &mut self,
        baseline: &SettingsPreferences,
        candidate: &SettingsPreferences,
    ) -> Result<(Self::HotkeyReceipt, Vec<String>), String>;

    fn rollback_hotkeys(&mut self, receipt: Self::HotkeyReceipt) -> Result<(), String>;

    fn apply_tray_label(
        &mut self,
        baseline: &SettingsPreferences,
        candidate: &SettingsPreferences,
    ) -> Result<Self::TrayReceipt, String>;

    fn rollback_tray_label(&mut self, receipt: Self::TrayReceipt) -> Result<(), String>;

    /// Apply the release/debug-specific startup policy and return the exact
    /// value that must be persisted with its rollback receipt.
    fn apply_autostart(
        &mut self,
        baseline: bool,
        requested: bool,
    ) -> Result<(Self::AutostartReceipt, bool), String>;

    fn rollback_autostart(&mut self, receipt: Self::AutostartReceipt) -> Result<(), String>;

    /// Reserve the exact next detector configuration without activating it.
    /// Dropping the receipt must cancel that reservation without changing the
    /// currently committed detector generation or configuration.
    fn prepare_detector(
        &mut self,
        candidate: &SettingsPreferences,
    ) -> Result<Self::PreparedDetector, String>;

    /// Validate and create the replacement recorder behind a closed start
    /// latch. Dropping the returned value must cancel and join that worker.
    fn prepare_recorder(
        &mut self,
        candidate: &SettingsPreferences,
    ) -> Result<Self::PreparedRecorder, String>;

    /// Atomically compare the UI baseline and merge the replacement into the
    /// latest durable document. Backend-owned Cloud/osu! state must survive.
    fn persist_preferences(
        &mut self,
        baseline: &SettingsPreferences,
        candidate: SettingsPreferences,
    ) -> Result<SettingsSnapshot, String>;

    /// Begin the already-created recorder and publish its sender/generation.
    /// This is deliberately infallible: durable preferences already exist.
    fn commit_recorder(
        &mut self,
        prepared: Self::PreparedRecorder,
        authoritative: &SettingsSnapshot,
    );

    /// Activate the already-reserved detector configuration. This is an
    /// infallible commit-tail operation after durable preferences exist.
    fn commit_detector(
        &mut self,
        prepared: Self::PreparedDetector,
        authoritative: &SettingsSnapshot,
    );

    /// Publish storage quota/root and consume exact media authorization.
    /// All validation and allocation happened in `prepare_preflight`.
    fn commit_preflight(
        &mut self,
        prepared: Self::PreparedPreflight,
        authoritative: &SettingsSnapshot,
    );

    /// Reconcile the compact desktop/frontend snapshot without a fallible
    /// operation after the durable commit point.
    fn publish(&mut self, authoritative: &SettingsSnapshot);
}

#[derive(Debug)]
pub struct SettingsApplySuccess {
    pub snapshot: SettingsSnapshot,
    pub warnings: Vec<String>,
}

impl SettingsApplySuccess {
    pub fn settings(&self) -> &AppSettings {
        &self.snapshot.document
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("{primary}{rollback_suffix}")]
pub struct SettingsApplyError {
    primary: String,
    rollback_errors: Vec<String>,
    rollback_suffix: String,
}

impl SettingsApplyError {
    fn new(primary: String, rollback_errors: Vec<String>) -> Self {
        let rollback_suffix = if rollback_errors.is_empty() {
            String::new()
        } else {
            format!(
                "; settings rollback incomplete: {}",
                rollback_errors.join(", ")
            )
        };
        Self {
            primary,
            rollback_errors,
            rollback_suffix,
        }
    }

    pub fn primary(&self) -> &str {
        &self.primary
    }

    pub fn rollback_errors(&self) -> &[String] {
        &self.rollback_errors
    }
}

/// Process-wide owner of the settings-apply transaction.
///
/// The lease serializes Settings windows without blocking independent Cloud,
/// upload, profile, or osu! transactions on the store's commit gate.
const SETTINGS_IDLE: u8 = 0;
const SETTINGS_APPLYING: u8 = 1;
const SETTINGS_QUIESCED: u8 = 2;

#[derive(Debug, Clone)]
pub struct SettingsApplyCoordinator {
    state: Arc<AtomicU8>,
}

impl Default for SettingsApplyCoordinator {
    fn default() -> Self {
        Self {
            state: Arc::new(AtomicU8::new(SETTINGS_IDLE)),
        }
    }
}

impl SettingsApplyCoordinator {
    pub fn apply<P: SettingsApplyPorts>(
        &self,
        ports: &mut P,
        baseline: SettingsPreferences,
        candidate: SettingsPreferences,
    ) -> Result<SettingsApplySuccess, SettingsApplyError> {
        let _lease = self.try_acquire()?;
        apply_settings(ports, baseline, candidate)
    }

    /// Run a settings-document publication while excluding a concurrent
    /// Settings apply. Shutdown uses this to prevent its final persistence
    /// pass from restoring the pre-apply preferences during the durable
    /// persist-to-runtime-commit window.
    pub fn with_exclusive<T>(
        &self,
        operation: impl FnOnce() -> T,
    ) -> Result<T, SettingsApplyError> {
        let _lease = self.try_acquire()?;
        Ok(operation())
    }

    pub fn is_active(&self) -> bool {
        self.state.load(Ordering::Acquire) == SETTINGS_APPLYING
    }

    /// Exclude new Settings applies across a reversible application shutdown.
    /// The returned owned guard may outlive a frontend state borrow.
    pub fn quiesce(&self) -> Result<SettingsApplyQuiescence, SettingsApplyError> {
        self.state
            .compare_exchange(
                SETTINGS_IDLE,
                SETTINGS_QUIESCED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| {
                SettingsApplyError::new(
                    "another settings apply is already in progress".into(),
                    Vec::new(),
                )
            })?;
        Ok(SettingsApplyQuiescence {
            state: Arc::clone(&self.state),
            resume_on_drop: true,
        })
    }

    fn try_acquire(&self) -> Result<SettingsApplyLease<'_>, SettingsApplyError> {
        self.state
            .compare_exchange(
                SETTINGS_IDLE,
                SETTINGS_APPLYING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| {
                SettingsApplyError::new(
                    "another settings apply is already in progress".into(),
                    Vec::new(),
                )
            })?;
        Ok(SettingsApplyLease { state: &self.state })
    }
}

struct SettingsApplyLease<'a> {
    state: &'a AtomicU8,
}

impl Drop for SettingsApplyLease<'_> {
    fn drop(&mut self) {
        self.state.store(SETTINGS_IDLE, Ordering::Release);
    }
}

pub struct SettingsApplyQuiescence {
    state: Arc<AtomicU8>,
    resume_on_drop: bool,
}

impl SettingsApplyQuiescence {
    /// Keep Settings admission closed because process exit is now inevitable.
    pub fn commit_shutdown(mut self) {
        self.resume_on_drop = false;
    }
}

impl Drop for SettingsApplyQuiescence {
    fn drop(&mut self) {
        if self.resume_on_drop {
            let _ = self.state.compare_exchange(
                SETTINGS_QUIESCED,
                SETTINGS_IDLE,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
    }
}

/// Apply one normalized preference draft with exact old-or-new semantics.
fn apply_settings<P: SettingsApplyPorts>(
    ports: &mut P,
    baseline: SettingsPreferences,
    candidate: SettingsPreferences,
) -> Result<SettingsApplySuccess, SettingsApplyError> {
    let baseline = baseline
        .normalized()
        .map_err(|error| SettingsApplyError::new(error, Vec::new()))?;
    let mut candidate = candidate
        .normalized()
        .map_err(|error| SettingsApplyError::new(error, Vec::new()))?;
    let prepared_preflight = ports
        .prepare_preflight(&baseline, &candidate)
        .map_err(|error| SettingsApplyError::new(error, Vec::new()))?;

    let (hotkey_receipt, warnings) = ports
        .apply_hotkeys(&baseline, &candidate)
        .map_err(|error| SettingsApplyError::new(error, Vec::new()))?;
    let tray_receipt = match ports.apply_tray_label(&baseline, &candidate) {
        Ok(receipt) => receipt,
        Err(primary) => {
            let rollback = rollback_hotkeys(ports, hotkey_receipt);
            return Err(SettingsApplyError::new(primary, rollback));
        }
    };
    let (autostart_receipt, persisted_autostart) =
        match ports.apply_autostart(baseline.open_on_startup, candidate.open_on_startup) {
            Ok(applied) => applied,
            Err(primary) => {
                let rollback = rollback_tray_and_hotkeys(ports, tray_receipt, hotkey_receipt);
                return Err(SettingsApplyError::new(primary, rollback));
            }
        };
    candidate.open_on_startup = persisted_autostart;

    let prepared_detector = match ports.prepare_detector(&candidate) {
        Ok(prepared) => prepared,
        Err(primary) => {
            let rollback =
                rollback_live_effects(ports, autostart_receipt, tray_receipt, hotkey_receipt);
            return Err(SettingsApplyError::new(primary, rollback));
        }
    };

    let prepared_recorder = match ports.prepare_recorder(&candidate) {
        Ok(prepared) => prepared,
        Err(primary) => {
            drop(prepared_detector);
            let rollback =
                rollback_live_effects(ports, autostart_receipt, tray_receipt, hotkey_receipt);
            return Err(SettingsApplyError::new(primary, rollback));
        }
    };

    let authoritative = match ports.persist_preferences(&baseline, candidate) {
        Ok(settings) => settings,
        Err(primary) => {
            // Cancel and join the latched worker before reversing older OS
            // effects. This preserves the transaction's exact reverse order.
            drop(prepared_recorder);
            drop(prepared_detector);
            let rollback =
                rollback_live_effects(ports, autostart_receipt, tray_receipt, hotkey_receipt);
            return Err(SettingsApplyError::new(primary, rollback));
        }
    };

    ports.commit_recorder(prepared_recorder, &authoritative);
    ports.commit_detector(prepared_detector, &authoritative);
    ports.commit_preflight(prepared_preflight, &authoritative);
    ports.publish(&authoritative);
    Ok(SettingsApplySuccess {
        snapshot: authoritative,
        warnings,
    })
}

fn rollback_hotkeys<P: SettingsApplyPorts>(
    ports: &mut P,
    hotkeys: P::HotkeyReceipt,
) -> Vec<String> {
    ports
        .rollback_hotkeys(hotkeys)
        .err()
        .map(|error| vec![format!("restore save hotkeys: {error}")])
        .unwrap_or_default()
}

fn rollback_tray_and_hotkeys<P: SettingsApplyPorts>(
    ports: &mut P,
    tray: P::TrayReceipt,
    hotkeys: P::HotkeyReceipt,
) -> Vec<String> {
    let mut errors = Vec::new();
    if let Err(error) = ports.rollback_tray_label(tray) {
        errors.push(format!("restore tray hotkey label: {error}"));
    }
    errors.extend(rollback_hotkeys(ports, hotkeys));
    errors
}

fn rollback_live_effects<P: SettingsApplyPorts>(
    ports: &mut P,
    autostart: P::AutostartReceipt,
    tray: P::TrayReceipt,
    hotkeys: P::HotkeyReceipt,
) -> Vec<String> {
    let mut errors = Vec::new();
    if let Err(error) = ports.rollback_autostart(autostart) {
        errors.push(format!("restore Windows startup registration: {error}"));
    }
    errors.extend(rollback_tray_and_hotkeys(ports, tray, hotkeys));
    errors
}
