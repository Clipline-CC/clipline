use std::collections::BTreeSet;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

use clipline_settings::{
    AccountGeneration, AppSettings, CloudAccountIdentity, SettingsApplyCoordinator,
    SettingsApplyPorts, SettingsPreferences, SettingsRevision, SettingsSnapshot,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Failure {
    Preflight,
    Hotkeys,
    Tray,
    Autostart,
    Recorder,
    Persistence,
}

struct PreparedRecorder {
    log: Arc<Mutex<Vec<String>>>,
    marker: String,
    committed: bool,
}

impl Drop for PreparedRecorder {
    fn drop(&mut self) {
        if !self.committed {
            self.log.lock().unwrap().push("recorder_cancel_join".into());
        }
    }
}

struct FakePorts {
    fail: Option<Failure>,
    rollback_failures: BTreeSet<&'static str>,
    log: Arc<Mutex<Vec<String>>>,
    document: AppSettings,
    hotkeys: String,
    tray: String,
    autostart: bool,
    recorder: String,
    storage: String,
    authorization_pending: bool,
    desktop: String,
    block_preflight: Option<(Sender<()>, Receiver<()>)>,
}

impl FakePorts {
    fn new(document: AppSettings, fail: Option<Failure>) -> Self {
        let marker = document.hotkey.clone();
        Self {
            fail,
            rollback_failures: BTreeSet::new(),
            log: Arc::new(Mutex::new(Vec::new())),
            document,
            hotkeys: marker.clone(),
            tray: marker.clone(),
            autostart: false,
            recorder: marker.clone(),
            storage: marker.clone(),
            authorization_pending: true,
            desktop: marker,
            block_preflight: None,
        }
    }

    fn event(&self, event: &str) {
        self.log.lock().unwrap().push(event.into());
    }

    fn events(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    fn fail_at(&self, stage: Failure) -> Result<(), String> {
        if self.fail == Some(stage) {
            Err(format!("{stage:?} failed"))
        } else {
            Ok(())
        }
    }
}

impl SettingsApplyPorts for FakePorts {
    type PreparedPreflight = String;
    type HotkeyReceipt = String;
    type TrayReceipt = String;
    type AutostartReceipt = bool;
    type PreparedRecorder = PreparedRecorder;

    fn prepare_preflight(
        &mut self,
        _baseline: &SettingsPreferences,
        candidate: &SettingsPreferences,
    ) -> Result<Self::PreparedPreflight, String> {
        self.event("preflight");
        if let Some((entered, release)) = self.block_preflight.take() {
            entered.send(()).unwrap();
            release.recv().unwrap();
        }
        self.fail_at(Failure::Preflight)?;
        Ok(candidate.hotkey.clone())
    }

    fn apply_hotkeys(
        &mut self,
        _baseline: &SettingsPreferences,
        candidate: &SettingsPreferences,
    ) -> Result<(Self::HotkeyReceipt, Vec<String>), String> {
        self.event("hotkeys");
        self.fail_at(Failure::Hotkeys)?;
        let old = std::mem::replace(&mut self.hotkeys, candidate.hotkey.clone());
        Ok((old, vec!["hotkey warning".into()]))
    }

    fn rollback_hotkeys(&mut self, receipt: Self::HotkeyReceipt) -> Result<(), String> {
        self.event("rollback_hotkeys");
        self.hotkeys = receipt;
        if self.rollback_failures.contains("hotkeys") {
            Err("hotkeys rollback failed".into())
        } else {
            Ok(())
        }
    }

    fn apply_tray_label(
        &mut self,
        _baseline: &SettingsPreferences,
        candidate: &SettingsPreferences,
    ) -> Result<Self::TrayReceipt, String> {
        self.event("tray");
        self.fail_at(Failure::Tray)?;
        Ok(std::mem::replace(&mut self.tray, candidate.hotkey.clone()))
    }

    fn rollback_tray_label(&mut self, receipt: Self::TrayReceipt) -> Result<(), String> {
        self.event("rollback_tray");
        self.tray = receipt;
        if self.rollback_failures.contains("tray") {
            Err("tray rollback failed".into())
        } else {
            Ok(())
        }
    }

    fn apply_autostart(
        &mut self,
        _baseline: bool,
        requested: bool,
    ) -> Result<(Self::AutostartReceipt, bool), String> {
        self.event("autostart");
        self.fail_at(Failure::Autostart)?;
        let old = std::mem::replace(&mut self.autostart, requested);
        Ok((old, requested))
    }

    fn rollback_autostart(&mut self, receipt: Self::AutostartReceipt) -> Result<(), String> {
        self.event("rollback_autostart");
        self.autostart = receipt;
        if self.rollback_failures.contains("autostart") {
            Err("autostart rollback failed".into())
        } else {
            Ok(())
        }
    }

    fn prepare_recorder(
        &mut self,
        candidate: &SettingsPreferences,
    ) -> Result<Self::PreparedRecorder, String> {
        self.event("recorder_prepare");
        self.fail_at(Failure::Recorder)?;
        Ok(PreparedRecorder {
            log: self.log.clone(),
            marker: candidate.hotkey.clone(),
            committed: false,
        })
    }

    fn persist_preferences(
        &mut self,
        _baseline: &SettingsPreferences,
        candidate: SettingsPreferences,
    ) -> Result<SettingsSnapshot, String> {
        self.event("persist");
        self.fail_at(Failure::Persistence)?;
        // Simulate a concurrent backend-owned update immediately before the
        // preferences merge.
        self.document.cloud.connected_username = Some("concurrent-cloud".into());
        self.document.osu.client_id = Some("12345".into());
        candidate.apply_to_document(&mut self.document)?;
        Ok(SettingsSnapshot {
            document: self.document.clone(),
            revision: SettingsRevision::INITIAL,
            account_generation: AccountGeneration::INITIAL,
            account: CloudAccountIdentity::from_settings(&self.document.cloud),
        })
    }

    fn commit_recorder(
        &mut self,
        mut prepared: Self::PreparedRecorder,
        _authoritative: &SettingsSnapshot,
    ) {
        self.event("recorder_commit");
        self.recorder.clone_from(&prepared.marker);
        prepared.committed = true;
    }

    fn commit_preflight(
        &mut self,
        prepared: Self::PreparedPreflight,
        _authoritative: &SettingsSnapshot,
    ) {
        self.event("preflight_commit");
        self.storage = prepared;
        self.authorization_pending = false;
    }

    fn publish(&mut self, authoritative: &SettingsSnapshot) {
        self.event("publish");
        self.desktop.clone_from(&authoritative.document.hotkey);
    }
}

fn documents() -> (AppSettings, SettingsPreferences, SettingsPreferences) {
    let baseline_document = AppSettings::default();
    let baseline = SettingsPreferences::from_document(&baseline_document).unwrap();
    let mut candidate_document = baseline_document.clone();
    candidate_document.hotkey = "Ctrl+F10".into();
    candidate_document.open_on_startup = !baseline_document.open_on_startup;
    candidate_document.media_dir = "C:\\Clipline-New".into();
    let candidate = SettingsPreferences::from_document(&candidate_document).unwrap();
    (baseline_document, baseline, candidate)
}

#[test]
fn success_uses_the_pinned_order_and_preserves_backend_owned_state() {
    let (document, baseline, candidate) = documents();
    let mut ports = FakePorts::new(document, None);
    let coordinator = SettingsApplyCoordinator::default();

    let success = coordinator.apply(&mut ports, baseline, candidate).unwrap();

    assert_eq!(
        ports.events(),
        [
            "preflight",
            "hotkeys",
            "tray",
            "autostart",
            "recorder_prepare",
            "persist",
            "recorder_commit",
            "preflight_commit",
            "publish",
        ]
    );
    assert_eq!(success.warnings, ["hotkey warning"]);
    assert_eq!(success.settings().hotkey, "Ctrl+F10");
    assert_eq!(
        success.settings().cloud.connected_username.as_deref(),
        Some("concurrent-cloud")
    );
    assert_eq!(success.settings().osu.client_id.as_deref(), Some("12345"));
    assert_eq!(ports.hotkeys, "Ctrl+F10");
    assert_eq!(ports.tray, "Ctrl+F10");
    assert_eq!(ports.recorder, "Ctrl+F10");
    assert_eq!(ports.storage, "Ctrl+F10");
    assert_eq!(ports.desktop, "Ctrl+F10");
    assert!(!ports.authorization_pending);
}

#[test]
fn every_fallible_boundary_leaves_the_entire_live_projection_old() {
    for failure in [
        Failure::Preflight,
        Failure::Hotkeys,
        Failure::Tray,
        Failure::Autostart,
        Failure::Recorder,
        Failure::Persistence,
    ] {
        let (document, baseline, candidate) = documents();
        let old_hotkey = document.hotkey.clone();
        let old_autostart = document.open_on_startup;
        let mut ports = FakePorts::new(document.clone(), Some(failure));
        ports.autostart = old_autostart;
        let coordinator = SettingsApplyCoordinator::default();

        let error = coordinator
            .apply(&mut ports, baseline, candidate)
            .unwrap_err();

        assert!(error.primary().contains("failed"), "{failure:?}: {error}");
        assert_eq!(ports.document, document, "{failure:?}: durable document");
        assert_eq!(ports.hotkeys, old_hotkey, "{failure:?}: hotkeys");
        assert_eq!(ports.tray, old_hotkey, "{failure:?}: tray");
        assert_eq!(ports.autostart, old_autostart, "{failure:?}: autostart");
        assert_eq!(ports.recorder, old_hotkey, "{failure:?}: recorder");
        assert_eq!(ports.storage, old_hotkey, "{failure:?}: storage");
        assert_eq!(ports.desktop, old_hotkey, "{failure:?}: desktop");
        assert!(ports.authorization_pending, "{failure:?}: authorization");
        if failure == Failure::Persistence {
            assert!(ports.events().contains(&"recorder_cancel_join".into()));
        }
    }
}

#[test]
fn persistence_failure_cancels_recorder_then_aggregates_reverse_rollback_errors() {
    let (document, baseline, candidate) = documents();
    let mut ports = FakePorts::new(document, Some(Failure::Persistence));
    ports.rollback_failures = BTreeSet::from(["autostart", "tray", "hotkeys"]);
    let coordinator = SettingsApplyCoordinator::default();

    let error = coordinator
        .apply(&mut ports, baseline, candidate)
        .unwrap_err();

    assert_eq!(
        &ports.events()[6..],
        [
            "recorder_cancel_join",
            "rollback_autostart",
            "rollback_tray",
            "rollback_hotkeys",
        ]
    );
    assert_eq!(
        error.rollback_errors(),
        [
            "restore Windows startup registration: autostart rollback failed",
            "restore tray hotkey label: tray rollback failed",
            "restore save hotkeys: hotkeys rollback failed",
        ]
    );
    assert!(error.to_string().contains("settings rollback incomplete"));
}

#[test]
fn overlapping_settings_apply_is_rejected_without_blocking_backend_transactions() {
    let coordinator = Arc::new(SettingsApplyCoordinator::default());
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let (document, baseline, candidate) = documents();
    let mut first_ports = FakePorts::new(document.clone(), None);
    first_ports.block_preflight = Some((entered_tx, release_rx));
    let first_coordinator = coordinator.clone();
    let first =
        std::thread::spawn(move || first_coordinator.apply(&mut first_ports, baseline, candidate));
    entered_rx.recv().unwrap();

    let (_, second_baseline, second_candidate) = documents();
    let mut second_ports = FakePorts::new(document, None);
    let error = coordinator
        .apply(&mut second_ports, second_baseline, second_candidate)
        .unwrap_err();
    assert_eq!(
        error.primary(),
        "another settings apply is already in progress"
    );
    assert!(second_ports.events().is_empty());

    release_tx.send(()).unwrap();
    first.join().unwrap().unwrap();
    assert!(!coordinator.is_active());
}
