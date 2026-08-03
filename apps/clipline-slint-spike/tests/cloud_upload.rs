use clipline_desktop::{
    ui_event_channel, CloudAccountOwner, CloudAccountScope, CloudUploadUpdateKind, UiEvent,
    UI_EVENT_CAPACITY,
};
use clipline_library::{
    catalog_result_channel, upload_account_owner_from_snapshot, ActiveFileRegistry, CatalogResult,
    ClipPathIdentity, CloudAccountGeneration, CloudAccountKey, DurableUploadToken,
    ExpectedResultOwner, LocalClipId, UploadEventKind, UploadEventPort, UploadGeneration,
    UploadPhase, UploadRecord, UploadServiceEvent, CATALOG_RESULT_CAPACITY, MAX_ACTIVE_UPLOAD_JOBS,
    MAX_UPLOAD_SUMMARIES,
};
use clipline_settings::{AppSettings, CloudUploadRecord, SettingsProfile, SettingsStore};
use clipline_slint_spike::cloud_upload::{
    NativeUploadEventFanout, NativeUploadRuntime, MAX_NATIVE_UPLOAD_FANOUT_SLOTS,
};
use clipline_test_utils::TestDir;

fn token(generation: u64, local_clip_id: &str) -> DurableUploadToken {
    DurableUploadToken {
        account_key: CloudAccountKey::new("account").unwrap(),
        account_generation: CloudAccountGeneration::new(7),
        upload_generation: UploadGeneration::new(generation),
        local_clip_id: LocalClipId::new(local_clip_id).unwrap(),
        source_path: ClipPathIdentity::from_text(&format!(r"C:\Clips\{local_clip_id}.mp4"))
            .unwrap(),
    }
}

fn record(generation: u64, local_clip_id: &str, received: u64) -> UploadRecord {
    let token = token(generation, local_clip_id);
    UploadRecord {
        path: format!(r"C:\Clips\{local_clip_id}.mp4"),
        token,
        client_clip_id: None,
        visibility: "private".into(),
        phase: UploadPhase::Uploading,
        upload_status: "uploading".into(),
        received_size_bytes: received,
        file_size_bytes: 1_000,
        remote_clip_id: None,
        remote_url: None,
        error: None,
        local_deleted: false,
        updated_at_unix: 1,
    }
}

fn terminal_record(generation: u64, local_clip_id: &str, received: u64) -> UploadRecord {
    let mut record = record(generation, local_clip_id, received);
    record.phase = UploadPhase::Completed;
    record.upload_status = "uploaded_private".into();
    record
}

fn account_event() -> UiEvent {
    UiEvent::CloudAccountChanged {
        generation: CloudAccountScope::new(7),
        account: Some(CloudAccountOwner::new("account", CloudAccountScope::new(7)).unwrap()),
    }
}

#[test]
fn state_barrier_remains_sticky_when_later_byte_progress_is_coalesced() {
    let (catalog_sender, catalog_receiver) = catalog_result_channel();
    let (desktop_sender, desktop_receiver) = ui_event_channel();
    desktop_sender.try_publish(account_event()).unwrap();
    let fanout = NativeUploadEventFanout::new(catalog_sender, desktop_sender);

    fanout
        .try_publish(UploadServiceEvent {
            kind: UploadEventKind::State,
            record: terminal_record(1, "clip-1", 100),
            notice: None,
        })
        .unwrap();
    fanout
        .try_publish(UploadServiceEvent {
            kind: UploadEventKind::Bytes,
            record: record(1, "clip-1", 900),
            notice: None,
        })
        .unwrap();

    let report = fanout.pump();
    assert_eq!(report.delivered, 2);
    assert_eq!(fanout.pending_slots(), 0);
    let catalog = catalog_receiver.try_recv().unwrap();
    assert!(matches!(
        catalog,
        clipline_library::CatalogResult::UploadCompleted { result, .. }
            if result.received_size_bytes == 900
    ));
    let _account = desktop_receiver.try_recv().unwrap();
    let desktop = desktop_receiver.try_recv().unwrap();
    assert!(matches!(
        desktop.event,
        UiEvent::CloudUploadProgress {
            update: CloudUploadUpdateKind::State,
            progress,
            ..
        } if progress.received_size_bytes == 900
    ));
}

#[test]
fn nonterminal_state_does_not_close_the_catalog_cancel_flow() {
    let (catalog_sender, catalog_receiver) = catalog_result_channel();
    let (desktop_sender, desktop_receiver) = ui_event_channel();
    desktop_sender.try_publish(account_event()).unwrap();
    let fanout = NativeUploadEventFanout::new(catalog_sender, desktop_sender);

    fanout
        .try_publish(UploadServiceEvent {
            kind: UploadEventKind::State,
            record: record(4, "clip-4", 10),
            notice: None,
        })
        .unwrap();
    fanout.pump();

    assert!(matches!(
        catalog_receiver.try_recv().unwrap(),
        clipline_library::CatalogResult::UploadByteProgress { .. }
    ));
    let _account = desktop_receiver.try_recv().unwrap();
    assert!(matches!(
        desktop_receiver.try_recv().unwrap().event,
        UiEvent::CloudUploadProgress {
            update: CloudUploadUpdateKind::State,
            ..
        }
    ));
}

#[test]
fn durable_large_error_and_url_are_projected_under_native_row_bounds() {
    let (catalog_sender, catalog_receiver) = catalog_result_channel();
    let (desktop_sender, desktop_receiver) = ui_event_channel();
    desktop_sender.try_publish(account_event()).unwrap();
    let fanout = NativeUploadEventFanout::new(catalog_sender, desktop_sender);
    let mut record = terminal_record(5, "clip-5", 1_000);
    record.remote_url = Some(format!(
        "https://clips.example/{}",
        "u".repeat(clipline_library::MAX_CATALOG_STRING_BYTES)
    ));
    record.error = Some("\u{00e9}".repeat(clipline_library::MAX_CATALOG_STRING_BYTES));

    fanout
        .try_publish(UploadServiceEvent {
            kind: UploadEventKind::State,
            record,
            notice: None,
        })
        .unwrap();
    fanout.pump();

    let catalog = catalog_receiver.try_recv().unwrap();
    assert!(matches!(
        catalog,
        clipline_library::CatalogResult::UploadCompleted { result, .. }
            if result.remote_url.is_none()
                && result.error.as_ref().is_some_and(|error| {
                    error.len() == clipline_library::MAX_CATALOG_STRING_BYTES
                        && error.is_char_boundary(error.len())
                })
    ));
    let _account = desktop_receiver.try_recv().unwrap();
    assert!(matches!(
        desktop_receiver.try_recv().unwrap().event,
        UiEvent::CloudUploadProgress { progress, .. }
            if progress.remote_url.is_none()
                && progress.error.as_ref().is_some_and(|error| {
                    error.len() == clipline_library::MAX_CATALOG_STRING_BYTES
                        && error.is_char_boundary(error.len())
                })
    ));
}

#[test]
fn fanout_is_bounded_and_replacement_generation_reclaims_the_old_slot() {
    let (catalog_sender, _catalog_receiver) = catalog_result_channel();
    let (desktop_sender, _desktop_receiver) = ui_event_channel();
    desktop_sender.try_publish(account_event()).unwrap();
    let fanout = NativeUploadEventFanout::new(catalog_sender, desktop_sender);

    for index in 0..MAX_NATIVE_UPLOAD_FANOUT_SLOTS {
        fanout
            .try_publish(UploadServiceEvent {
                kind: UploadEventKind::Bytes,
                record: record(1, &format!("clip-{index}"), 1),
                notice: None,
            })
            .unwrap();
    }
    assert_eq!(fanout.pending_slots(), MAX_NATIVE_UPLOAD_FANOUT_SLOTS);
    assert!(fanout
        .try_publish(UploadServiceEvent {
            kind: UploadEventKind::Bytes,
            record: record(1, "overflow", 1),
            notice: None,
        })
        .is_err());

    fanout
        .try_publish(UploadServiceEvent {
            kind: UploadEventKind::State,
            record: record(2, "clip-0", 2),
            notice: None,
        })
        .unwrap();
    assert_eq!(fanout.pending_slots(), MAX_NATIVE_UPLOAD_FANOUT_SLOTS);
}

#[test]
fn full_downstreams_retain_the_hydration_status_and_active_terminal_union() {
    let (catalog_sender, catalog_receiver) = catalog_result_channel();
    let (desktop_sender, desktop_receiver) = ui_event_channel();
    desktop_sender.try_publish(account_event()).unwrap();
    for index in 1..UI_EVENT_CAPACITY {
        desktop_sender
            .try_publish(UiEvent::UserError {
                message: format!("queue filler {index}"),
            })
            .unwrap();
    }
    for index in 0..CATALOG_RESULT_CAPACITY {
        let token = token(1, &format!("catalog-filler-{index}"));
        catalog_sender
            .try_send(
                CatalogResult::UploadRemoved {
                    token: token.clone(),
                },
                ExpectedResultOwner::Upload(token),
            )
            .unwrap();
    }

    let fanout = NativeUploadEventFanout::new(catalog_sender, desktop_sender);
    for index in 0..(MAX_UPLOAD_SUMMARIES * 2) {
        fanout
            .try_publish(UploadServiceEvent {
                kind: UploadEventKind::State,
                record: terminal_record(1, &format!("hydrated-{index}"), 1_000),
                notice: None,
            })
            .unwrap();
    }
    let report = fanout.pump();
    assert_eq!(report.delivered, 0);
    assert_eq!(report.retained, 32);

    for index in 0..MAX_ACTIVE_UPLOAD_JOBS {
        let local_clip_id = format!("active-{index}");
        fanout
            .try_publish(UploadServiceEvent {
                kind: UploadEventKind::State,
                record: record(1, &local_clip_id, 1),
                notice: None,
            })
            .unwrap();
        fanout
            .try_publish(UploadServiceEvent {
                kind: UploadEventKind::State,
                record: terminal_record(1, &local_clip_id, 1_000),
                notice: None,
            })
            .unwrap();
    }
    assert_eq!(fanout.pending_slots(), MAX_NATIVE_UPLOAD_FANOUT_SLOTS);

    while catalog_receiver.try_recv().is_some() {}
    while desktop_receiver.try_recv().is_some() {}
    for _ in 0..4 {
        fanout.pump();
    }
    assert_eq!(fanout.pending_slots(), 0);

    let mut catalog_active_terminals = 0;
    while let Some(result) = catalog_receiver.try_recv() {
        if matches!(
            result,
            CatalogResult::UploadCompleted { token, .. }
                if token.local_clip_id.as_str().starts_with("active-")
        ) {
            catalog_active_terminals += 1;
        }
    }
    assert_eq!(catalog_active_terminals, MAX_ACTIVE_UPLOAD_JOBS);

    let mut desktop_active_terminals = 0;
    while let Some(event) = desktop_receiver.try_recv() {
        if matches!(
            event.event,
            UiEvent::CloudUploadProgress {
                update: CloudUploadUpdateKind::State,
                progress,
                ..
            } if progress.local_clip_id.starts_with("active-") && progress.is_terminal()
        ) {
            desktop_active_terminals += 1;
        }
    }
    assert_eq!(desktop_active_terminals, MAX_ACTIVE_UPLOAD_JOBS);
}

#[test]
fn exact_removal_reaches_both_contracts() {
    let (catalog_sender, catalog_receiver) = catalog_result_channel();
    let (desktop_sender, desktop_receiver) = ui_event_channel();
    desktop_sender.try_publish(account_event()).unwrap();
    let fanout = NativeUploadEventFanout::new(catalog_sender, desktop_sender);
    let token = token(3, "clip-3");

    fanout.enqueue_removed(token.clone()).unwrap();
    fanout.pump();

    assert!(matches!(
        catalog_receiver.try_recv().unwrap(),
        clipline_library::CatalogResult::UploadRemoved { token: removed } if removed == token
    ));
    let _account = desktop_receiver.try_recv().unwrap();
    assert!(matches!(
        desktop_receiver.try_recv().unwrap().event,
        UiEvent::CloudUploadRemoved { generation, local_clip_id, .. }
            if generation.get() == 3 && local_clip_id == "clip-3"
    ));
}

#[test]
fn hydration_is_newest_first_bounded_and_hides_restart_orphaned_active_rows() {
    let directory = TestDir::new("slint-cloud-upload", "hydrate");
    let profile = SettingsProfile::isolated(directory.path());
    let media_root = directory.path().join("media");
    std::fs::create_dir_all(&media_root).unwrap();
    let mut settings = AppSettings {
        media_dir: media_root.display().to_string(),
        ..AppSettings::default()
    };
    settings.cloud.host_url = "https://clips.example".into();
    settings.cloud.connected_user_id = Some("user-1".into());
    settings.cloud.credential_target = Some("credential-1".into());
    for index in 0..=MAX_UPLOAD_SUMMARIES {
        let local_id = format!("completed-{index:02}");
        settings.cloud.uploads.insert(
            local_id.clone(),
            CloudUploadRecord {
                local_clip_id: local_id,
                client_clip_id: None,
                upload_generation: Some(1),
                path: media_root
                    .join(format!("completed-{index:02}.mp4"))
                    .display()
                    .to_string(),
                remote_clip_id: Some(format!("remote-{index:02}")),
                remote_url: None,
                visibility: "private".into(),
                upload_status: "uploaded_private".into(),
                error: None,
                updated_at_unix: index as u64 + 10,
            },
        );
    }
    settings.cloud.uploads.insert(
        "orphan-active".into(),
        CloudUploadRecord {
            local_clip_id: "orphan-active".into(),
            client_clip_id: None,
            upload_generation: Some(2),
            path: media_root.join("orphan.mp4").display().to_string(),
            remote_clip_id: None,
            remote_url: None,
            visibility: "private".into(),
            upload_status: "uploading".into(),
            error: None,
            updated_at_unix: u64::MAX,
        },
    );
    settings.cloud.uploads.insert(
        "restart-processing".into(),
        CloudUploadRecord {
            local_clip_id: "restart-processing".into(),
            client_clip_id: None,
            upload_generation: Some(4),
            path: media_root.join("processing.mp4").display().to_string(),
            remote_clip_id: Some("remote-processing".into()),
            remote_url: None,
            visibility: "private".into(),
            upload_status: "processing".into(),
            error: None,
            updated_at_unix: u64::MAX - 2,
        },
    );
    settings.cloud.uploads.insert(
        "legacy-oversized-path".into(),
        CloudUploadRecord {
            local_clip_id: "legacy-oversized-path".into(),
            client_clip_id: None,
            upload_generation: Some(3),
            path: "x".repeat(clipline_library::MAX_CATALOG_STRING_BYTES + 1),
            remote_clip_id: Some("remote-oversized".into()),
            remote_url: None,
            visibility: "private".into(),
            upload_status: "uploaded_private".into(),
            error: None,
            updated_at_unix: u64::MAX - 1,
        },
    );
    settings.save_to(profile.settings_path()).unwrap();
    let store = SettingsStore::open(profile);
    let owner = upload_account_owner_from_snapshot(&store.snapshot().unwrap()).unwrap();
    let (catalog_sender, _catalog_receiver) = catalog_result_channel();
    let (desktop_sender, _desktop_receiver) = ui_event_channel();
    let fanout = NativeUploadEventFanout::new(catalog_sender, desktop_sender);
    let runtime = NativeUploadRuntime::open(
        store,
        &media_root,
        ActiveFileRegistry::new(),
        std::sync::Arc::new(fanout),
    )
    .unwrap();

    let hydrated = runtime.hydrate(&owner).unwrap();
    assert_eq!(hydrated.visible.len(), MAX_UPLOAD_SUMMARIES);
    assert_eq!(
        hydrated.visible[0].token.local_clip_id.as_str(),
        "completed-16"
    );
    assert!(hydrated.visible.iter().all(|record| !matches!(
        record.token.local_clip_id.as_str(),
        "orphan-active" | "restart-processing" | "legacy-oversized-path"
    )));
    assert!(hydrated
        .status_candidates
        .iter()
        .any(|record| record.token.local_clip_id.as_str() == "restart-processing"));
}
