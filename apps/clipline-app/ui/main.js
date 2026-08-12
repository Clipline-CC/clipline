// Bootstrap: backend events, DOM wiring, and app startup.

// Custom window chrome — registered last so handlers in review-player.js exist.
$('win-min').addEventListener('click', async () => {
  try {
    await invoke('minimize_main_window');
  } catch (e) {
    $('error').textContent = e;
  }
});
$('win-max').addEventListener('click', () => appWindow.toggleMaximize());
$('win-close').addEventListener('click', requestWindowClose);
/* ---- backend events ---- */

listen("status", (e) => {
  const s = e.payload;
  recordingActive = s.recording;
  recorderWaitingForGame = !!s.waiting_for_game;
  recordingRequested = recordingActive || recorderWaitingForGame;
  activeEncoderLabel = s.recording ? String(s.encoder || "") : "";
  fullSessionRecordingActive = Boolean(s.full_session);
  updateCaptureStatus();
});

function requestRefresh() {
  if (!requestWindowRefresh()) return;
  refresh().catch((error) => {
    $("error").textContent = String(error);
  });
}

listen("saved", (e) => {
  $("error").textContent = "";
  const s = e.payload;
  const savedKind = s.full_session ? "session" : "replay";
  setNotice(`saved ${fmtDur(s.seconds)} ${savedKind}`, { transient: true });
  requestRefresh();
});

// The sound is the real confirmation (the app is usually behind a game), so
// this is just the visible echo for anyone watching the window. Only a full
// session can name a time the user will recognise; the recorder-wide offset a
// replay bookmark carries is not where it lands in the eventual clip, so that
// case stays wordless rather than lying about the position.
listen("bookmark-added", (e) => {
  // Guard the object first: `Number(null)` is 0, which would read as 0:00.
  const session_t_s = e.payload ? e.payload.session_t_s : null;
  const at = typeof session_t_s === "number" && Number.isFinite(session_t_s);
  setNotice(at ? `bookmarked at ${fmtDur(session_t_s)}` : "bookmarked", {
    transient: true,
  });
});

listen("library-changed", () => {
  requestRefresh();
});

listen("storage-quota-full", (event) => showStorageQuotaFull(event.payload));
listen("storage-quota-resolved", () => {
  storageQuotaBlocked = false;
  storageQuotaState = null;
  recordingRequested = true;
  if ($("storage-quota-dialog").open) $("storage-quota-dialog").close();
  updateCaptureStatus();
  requestRefresh();
});

listen("osu-enrichment-updated", () => {
  requestRefresh();
});

listen("error", (e) => { $("error").textContent = e.payload; });

listen("mic-test", (e) => {
  if (!micTestRunning) return;
  const result = e.payload || {};
  playMicSamples(result.samples || []);
  const level = micMeterLevel(result);
  const peakPct = Math.round(Math.max(0, Math.min(1, Number(result.peak) || 0)) * 100);
  if (!result.sample_count) {
    setMicTestStatus("no input", 0);
  } else if (peakPct <= 1) {
    setMicTestStatus("quiet", level);
  } else {
    setMicTestStatus(`${peakPct}%`, level);
  }
});

listen("mic-test-error", (e) => {
  stopMicTestUi("error");
  $(micTestSurface === "first-run" ? "first-run-error" : "error").textContent = e.payload;
});

listen("mic-test-stopped", () => {
  if (micTestRunning) stopMicTestUi("stopped");
});

var windowLifecycleListenerReady = listen("window-lifecycle", (event) => {
  applyWindowLifecycleSnapshot(event.payload);
});

listen("ffmpeg-install", (event) => {
  applyFfmpegInstallSnapshot(event.payload);
});

listen("encoders-changed", (event) => {
  videoEncoders = Array.isArray(event.payload) ? event.payload : [];
  videoEncodersLoaded = true;
  renderVideoEncoderSelect();
  if (currentSettings) syncRecordingFields();
});

listen("game-detection", (e) => {
  activeDetectedGame = e.payload || null;
  if (activeDetectedGame?.active) {
    if (captureForegroundWork()) loadGamePlugins();
    else requestWindowRefresh();
  }
  updateCaptureStatus();
  updateGameDetectionStatus();
  maybeWarnElevatedGame(activeDetectedGame);
});

listen("cloud-upload-progress", (e) => {
  const progress = e.payload || {};
  const update = upsertCloudProgress(progress);
  if (progress.error) {
    $("error").textContent = progress.error;
  } else if (progress.upload_status === "uploading") {
    const total = Number(progress.file_size_bytes) || 0;
    const done = Number(progress.received_size_bytes) || 0;
    setDeckStatus(total > 0
      ? `cloud upload ${Math.round((done / total) * 100)}%`
      : "cloud upload in progress");
  } else if (progress.upload_status === "processing") {
    setDeckStatus("cloud upload processing");
  }
  if (update.renderRequired) renderClips();
});

/* ---- wiring ---- */

$("review-back").addEventListener("click", () => closeReview());

// Gallery (library home) controls.
$("gallery-search").addEventListener("input", (ev) => {
  gallerySearch = ev.target.value.trim().toLowerCase();
  renderClips();
});
$("gallery-source-tabs").addEventListener("click", (ev) => {
  const tab = ev.target.closest(".source-tab");
  if (!tab) return;
  gallerySource = tab.dataset.gallerySource === "cloud" ? "cloud" : "local";
  if (gallerySource === "cloud") {
    exitSelectMode();
    loadCloudClips({ force: true });
    return;
  }
  renderClips();
});
$("gallery-select-toggle").addEventListener("click", () => {
  selectMode = !selectMode;
  if (!selectMode) clearSelection();
  syncSelectionControls();
});
$("bulk-select-all").addEventListener("click", selectAllVisible);
$("bulk-clear").addEventListener("click", clearSelection);
$("bulk-delete").addEventListener("click", bulkDeleteSelected);
$("poster-runtime-install").addEventListener("click", () => {
  void installFfmpegForPosters().catch(() => {});
});
$("ffmpeg-runtime-install").addEventListener("click", () => {
  void ensureFfmpegRuntime().catch(() => {});
});
$("ffmpeg-runtime-cancel").addEventListener("click", () => {
  void cancelFfmpegRuntimeInstall().catch((error) => {
    $("error").textContent = String(error);
  });
});
$("gallery-sort").addEventListener("change", (ev) => { gallerySort = ev.target.value; renderClips(); });
$("gallery-game-type").addEventListener("change", (ev) => {
  galleryGameType = ev.target.value;
  renderClips();
});
$("gallery-group").addEventListener("change", (ev) => { galleryGroup = ev.target.value; renderClips(); });
$("gallery-filter").addEventListener("click", (ev) => {
  const chip = ev.target.closest(".g-chip");
  if (!chip) return;
  galleryFilter = chip.dataset.filter;
  for (const c of $("gallery-filter").querySelectorAll(".g-chip")) c.classList.toggle("on", c === chip);
  renderClips();
});
$("rail-status").addEventListener("click", toggleSessionRecording);
$("rail-game").addEventListener("click", toggleRecording);
$("set-capture").addEventListener("change", () => {
  captureTargetDirty = true;
  syncCaptureFields();
});
$("set-backend").addEventListener("change", syncCaptureBackendSummary);
$("set-theme").addEventListener("change", () => applyUiTheme($("set-theme").value));
for (const id of ["set-output-enabled", "set-audio-split-output", "set-mic-enabled"]) {
  $(id).addEventListener("change", syncAudioFields);
}
for (const id of ["set-output-volume", "set-mic-volume"]) {
  $(id).addEventListener("input", () => {
    syncRangeProgress($(id));
    syncAudioFields();
  });
  $(id).addEventListener("change", () => {
    syncRangeProgress($(id));
    syncAudioFields();
  });
}
$("test-mic").addEventListener("click", testMic);
$("add-custom-game").addEventListener("click", showGameWindowPicker);
$("detect-games").addEventListener("click", showDetectedGamesDialog);
$("add-detected-games").addEventListener("click", addSelectedDetectedGames);
$("cancel-detected-games").addEventListener("click", hideDetectedGamesDialog);
$("detected-games-dialog").addEventListener("close", resetDetectedGamesDialog);
$("refresh-game-windows").addEventListener("click", refreshGameWindows);
$("cancel-game-picker").addEventListener("click", hideGameWindowPicker);
$("choose-media-folder").addEventListener("click", chooseMediaFolder);
$("choose-replay-cache-folder").addEventListener("click", chooseReplayCacheFolder);
$("check-updates").addEventListener("click", () => checkForUpdates({ manual: true }));
$("update-install").addEventListener("click", installPendingUpdate);
$("update-cancel").addEventListener("click", () => {
  // Keep `pendingUpdate`: the rail button reopens this dialog from it, and the
  // background poller stops after its first find, so clearing it here would
  // leave a visible button that does nothing until the next launch. Nothing
  // reads it for the install itself — `install_update` re-resolves the update
  // server-side — so a stale payload cannot install the wrong build.
  $("update-dialog").close();
});
$("elevation-cancel").addEventListener("click", () => {
  if (!elevationRestartInFlight) $("elevation-dialog").close();
});
$("elevation-restart").addEventListener("click", restartAsAdministrator);
$("elevation-dialog").addEventListener("cancel", (ev) => {
  ev.preventDefault();
});
$("elevation-dialog").addEventListener("close", () => maybeWarnElevatedGame(activeDetectedGame));
$("set-replay-disk-enabled").addEventListener("change", syncReplayStorageFields);
$("set-replay-disk-quota").addEventListener("input", syncReplayStorageFields);
$("set-replay-disk-quota").addEventListener("change", syncReplayStorageFields);
for (const id of ["cloud-default-visibility", "cloud-delete-local-after-upload"]) {
  $(id).addEventListener("change", () => {
    $("settings-status").textContent = "cloud settings changed - save to apply";
  });
}
$("cloud-host-url").addEventListener("input", syncCloudHttpWarning);
$("cloud-host-url").addEventListener("change", syncCloudHttpWarning);
$("cloud-connect").addEventListener("click", connectCloud);
$("cloud-disconnect").addEventListener("click", disconnectCloud);
for (const id of ["set-games-auto-detect", "set-games-pause-when-empty"]) {
  $(id).addEventListener("change", updateGameDetectionStatus);
}
for (const id of [
  "set-buffer",
  "set-replay",
  "set-encoder",
  "set-output-resolution",
  "set-bitrate",
  "set-fps",
  "recording-mode-basic",
  "recording-mode-advanced",
  "set-output-width",
  "set-output-height",
  "set-custom-bitrate",
  "set-custom-fps",
]) {
  $(id).addEventListener("input", syncRecordingFields);
  $(id).addEventListener("change", syncRecordingFields);
}
document.querySelectorAll("[data-replay-preset]").forEach((button) => {
  button.addEventListener("click", () => {
    $("set-replay").value = button.dataset.replayPreset;
    syncRangeProgress($("set-replay"));
    syncRecordingFields();
  });
});
for (const id of ["set-region-width", "set-region-height", "set-region-x", "set-region-y"]) {
  $(id).addEventListener("change", () => setRegion(regionFromFields()));
  $(id).addEventListener("blur", () => setRegion(regionFromFields()));
}
$("display-map").addEventListener("contextmenu", showRegionMenu);
$("region-box").addEventListener("pointerdown", (ev) => {
  startRegionDrag(ev.target.dataset.regionResize ? "resize" : "move", ev);
});
$("region-box").addEventListener("pointermove", moveRegionDrag);
$("region-box").addEventListener("pointerup", endRegionDrag);
$("region-box").addEventListener("pointercancel", endRegionDrag);
$("region-box").addEventListener("lostpointercapture", endRegionDrag);
document.querySelectorAll("#region-align-menu button").forEach((button) => {
  button.addEventListener("click", () => {
    const display = menuDisplay();
    if (display) setRegion(alignRegion(regionState, display, button.dataset.align));
    hideRegionMenu();
  });
});
document.addEventListener("click", (ev) => {
  if (!$("capture-region-menu").contains(ev.target)) hideRegionMenu();
  if (!$("clip-context-menu").contains(ev.target)) hideClipContextMenu();
});
document.addEventListener("contextmenu", (ev) => {
  ev.preventDefault();
  hideRegionMenu();
  hideClipContextMenu();
});
$("clip-context-menu").addEventListener("contextmenu", (ev) => ev.preventDefault());
$("clip-menu-select").addEventListener("click", () => {
  const clip = clipContextTarget;
  hideClipContextMenu();
  if (clip) selectClipFromContext(clip.path);
});
$("clip-menu-play").addEventListener("click", () => {
  const entry = cloudContextTarget;
  hideClipContextMenu();
  if (entry) openCloudEntryInApp(entry);
});
$("clip-menu-open-cloud-page").addEventListener("click", () => {
  const entry = cloudContextTarget;
  hideClipContextMenu();
  if (entry) openCloudClipUrl(entry);
});
$("clip-menu-copy-cloud-link").addEventListener("click", () => {
  const entry = cloudContextTarget;
  hideClipContextMenu();
  if (entry) copyCloudUrl(entry);
});
$("clip-menu-export-play").addEventListener("click", () => {
  const target = gamePlayContextTarget;
  hideClipContextMenu();
  if (target) {
    gamePlayContextTarget = target;
    exportPlayClip().finally(() => {
      if (gamePlayContextTarget === target) gamePlayContextTarget = null;
    });
  }
});
$("clip-menu-copy").addEventListener("click", () => {
  const clip = clipContextTarget;
  hideClipContextMenu();
  if (clip) copyClipToClipboard(null, clip, true);
});
$("clip-menu-copy-shareable").addEventListener("click", () => {
  const clip = clipContextTarget;
  hideClipContextMenu();
  if (clip) copyClipToClipboard(null, clip, false);
});
$("clip-menu-upload").addEventListener("click", () => {
  const clip = clipContextTarget;
  const record = clipContextRecord();
  hideClipContextMenu();
  if (!clip) return;
  if (cloudShareUrl(record)) copyCloudUrl(record);
  else if (cloudRecordUploaded(record)) openCloudClipUrl(record);
  else openUploadDialog(clip);
});
$("clip-menu-rename").addEventListener("click", () => {
  const clip = clipContextTarget;
  hideClipContextMenu();
  if (clip) beginClipRename(clip);
});
$("clip-menu-rename-file").addEventListener("click", () => {
  const clip = clipContextTarget;
  hideClipContextMenu();
  if (clip) openRenameFileDialog(clip);
});
$("clip-menu-delete").addEventListener("click", () => {
  const clip = clipContextTarget;
  hideClipContextMenu();
  if (clip) deleteClip(clip.path);
});
window.addEventListener("resize", () => {
  renderRegionEditor();
  updateStageFrame();
  hideRegionMenu();
  hideClipContextMenu();
});
$("settings-save").addEventListener("click", async () => {
  $("settings-status").textContent = "";
  $("error").textContent = "";
  if (!syncSettingsDraftFromForm().hotkey) {
    setHotkeyStatus("Save Replay needs at least one keybind.", "error");
    $("settings-status").textContent = "Save Replay needs at least one keybind.";
    return;
  }
  try {
    const saved = await invoke("save_settings", { settings: syncSettingsDraftFromForm() });
    fillSettings(saved);
    $("settings-status").textContent = "saved";
    await refresh();
  } catch (e) {
    $("error").textContent = e;
  }
});

video.addEventListener("click", togglePlay);
video.addEventListener("play", () => {
  const current = reviewPlayheadTime();
  scheduleTrimBoundaryCheck();
  syncPlayState();
  syncGameEventRail(current);
  syncGamePlayRail(current);
  paintTimeline();
  syncReviewAudioSidecars();
  refreshReviewAudioDriftTimer();
  scheduleOverlayIdleCheck();
});
video.addEventListener("pause", () => {
  clearTrimBoundaryCheck();
  syncReviewAudioSidecars();
  refreshReviewAudioDriftTimer();
  syncPlayState();
  clearOverlayIdleCheck();
  paintTimeline();
  updateOverlay();
});
video.addEventListener("timeupdate", () => {
  const current = reviewPlayheadTime();
  if (stopAtTrimEnd(current)) return;
  maybeFollow(current);
  paintTimeline();
  syncGameEventRail(current);
  syncGamePlayRail(current);
  syncReviewAudioSidecars();
});
video.addEventListener("seeking", () => syncReviewAudioSidecars({ forceSeek: true }));
video.addEventListener("ratechange", () => syncReviewAudioSidecars());
video.addEventListener("volumechange", syncVolume);
video.addEventListener("loadedmetadata", () => {
  $("stage-note").textContent = `${video.videoWidth}x${video.videoHeight} · ${fmtDur(video.duration)}`;
  updateStageFrame();
  if (currentClip) {
    $("pmeta").textContent = `${fmtDur(video.duration)} · ${fmtMegabytes(currentClip.size_mb)} · ${currentClip.path}`;
    setTrim(0, video.duration);
    // Duration is now exact: rebuild the whole-clip navigator and re-render.
    renderOverviewMarkers();
    applyView({ start: zoomStart, span: zoomSpan });
  }
});
$("play-toggle").addEventListener("click", togglePlay);
$("seek-back").addEventListener("click", () => seekBy(-5));
$("seek-forward").addEventListener("click", () => seekBy(5));
$("prev-marker").addEventListener("click", () => jumpMarker(-1));
$("next-marker").addEventListener("click", () => jumpMarker(1));
$("game-event-rail-toggle").addEventListener("click", () => {
  setGameEventRailCollapsed(!gameEventRailCollapsed);
});
$("mute-toggle").addEventListener("click", toggleMute);
$("fullscreen-toggle").addEventListener("click", toggleReviewFullscreen);
document.addEventListener("fullscreenchange", syncReviewFullscreenState);
$("rate-select").addEventListener("change", () => {
  video.playbackRate = Number($("rate-select").value);
});
$("volume-slider").addEventListener("input", () => {
  syncRangeProgress($("volume-slider"));
  reviewAudioVolume = Number($("volume-slider").value);
  reviewAudioMuted = reviewAudioVolume === 0;
  applyReviewAudioOutput();
});

$("export-clip").addEventListener("click", exportTrim);
$("deck-status-action").addEventListener("click", runDeckStatusAction);
$("delete-clip").addEventListener("click", () => deleteClip());
$("open-folder").addEventListener("click", openFolder);
$("copy-clip").addEventListener("click", (event) => copyClipToClipboard(event));
$("rename-clip").addEventListener("click", () => beginClipRename());
$("clip-title-edit").addEventListener("submit", saveClipRename);
$("rename-cancel").addEventListener("click", cancelClipRename);
$("rename-input").addEventListener("keydown", (ev) => {
  if (ev.key !== "Escape") return;
  ev.preventDefault();
  cancelClipRename();
});
$("rename-file-save").addEventListener("click", submitRenameFileDialog);
$("rename-file-cancel").addEventListener("click", () => closeRenameFileDialog());
$("rename-file-input").addEventListener("keydown", (ev) => {
  if (ev.key === "Enter") {
    ev.preventDefault();
    submitRenameFileDialog();
  } else if (ev.key === "Escape") {
    ev.preventDefault();
    closeRenameFileDialog();
  }
});
$("upload-clip").addEventListener("click", () => {
  if (!currentClip) return;
  if (isCloudOnlyReviewClip(currentClip)) {
    const entry = {
      remote_clip_id: currentClip.cloud_remote_clip_id,
      remote_url: currentClip.cloud_remote_url || "",
      visibility: currentClip.cloud_visibility || "private",
      upload_status: currentClip.cloud_remote_url ? "uploaded_public" : "uploaded_private",
    };
    if (cloudShareUrl(entry)) copyCloudUrl(entry);
    else openCloudClipUrl(entry);
    return;
  }
  const record = clipCloudRecord(currentClip);
  if (cloudShareUrl(record)) copyCloudUrl(record);
  else if (cloudRecordUploaded(record)) openCloudClipUrl(record);
  else openUploadDialog(currentClip);
});
$("upload-confirm").addEventListener("click", submitUploadDialog);
$("upload-cancel").addEventListener("click", closeUploadDialog);
$("upload-title").addEventListener("keydown", (ev) => {
  if (ev.key !== "Enter") return;
  ev.preventDefault();
  submitUploadDialog();
});
$("upload-dialog").addEventListener("click", (ev) => {
  if (ev.target === $("upload-dialog")) closeUploadDialog();
});

$("game-plugin-settings-close").addEventListener("click", hideGamePluginSettingsDialog);
$("game-plugin-settings-dialog").addEventListener("click", (ev) => {
  if (ev.target === $("game-plugin-settings-dialog")) hideGamePluginSettingsDialog();
});
$("game-plugin-settings-dialog").addEventListener("close", () => {
  gamePluginSettingsDialogPluginId = null;
});
document.querySelectorAll("[data-game-plugin-settings-tab]").forEach((tab) => {
  tab.addEventListener("click", () => setGamePluginSettingsTab(tab.dataset.gamePluginSettingsTab));
});

$("trim-mode-toggle").addEventListener("click", () => setSimpleTrimMode(!simpleTrimMode));
$("zoom-in").addEventListener("click", () => zoomAtPlayhead(0.5));
$("zoom-out").addEventListener("click", () => zoomAtPlayhead(2));
// Plain click frames the trim selection (the editing default); Shift-click fits
// the whole clip — mirroring \ and Shift+\.
$("zoom-fit").addEventListener("click", (ev) => (ev.shiftKey ? zoomFit() : zoomToSelection()));
$("snap-toggle").addEventListener("click", toggleSnap);

// Keyboard shortcuts guide — the corner "K" keycap opens it; click the X or the
// backdrop (or press Esc, which the modal dialog handles) to close.
$("keys-close").addEventListener("click", () => $("keys-dialog").close());
$("keys-dialog").addEventListener("click", (ev) => {
  if (ev.target === $("keys-dialog")) $("keys-dialog").close();
});

$("rail-save").addEventListener("click", () => invoke("save_replay"));
$("storage-quota-manage").addEventListener("click", () => $("storage-quota-dialog").close());
$("storage-quota-folder").addEventListener("click", async () => {
  try {
    await invoke("open_media_folder");
  } catch (error) {
    $("error").textContent = String(error);
  }
});
$("storage-quota-settings").addEventListener("click", () => {
  $("storage-quota-dialog").close();
  toggleSettings(true);
  activateSettingsTab($("settings-tab-storage"), { focus: true });
});
$("storage-quota-recheck").addEventListener("click", async () => {
  const button = $("storage-quota-recheck");
  button.disabled = true;
  try {
    await invoke("recheck_storage_quota", { announce: true });
    requestRefresh();
  } catch (error) {
    $("error").textContent = String(error);
  } finally {
    button.disabled = false;
  }
});
$("storage-quota-dialog").addEventListener("click", (event) => {
  if (event.target === $("storage-quota-dialog")) $("storage-quota-dialog").close();
});
// The background poller announces the first newer build it finds. This one
// only lights the rail button: it can land ten minutes into a game, unlike the
// launch and manual checks, which the user is present for and which do open the
// dialog. A window that was closed to tray when this fired catches up through
// the launch check when it reopens.
listen("update-available", (e) => announceUpdate(e.payload));

$("rail-update").addEventListener("click", () => {
  if (pendingUpdate) showUpdateDialog(pendingUpdate);
});

$("rail-profile").addEventListener("click", openRailProfile);
$("rail-settings").addEventListener("click", () => {
  if (settingsOpen) requestSettingsClose();
  else toggleSettings(true);
});
$("settings-close").addEventListener("click", requestSettingsClose);
$("settings-page").addEventListener("input", () => syncSettingsDraftFromForm());
$("settings-page").addEventListener("change", () => syncSettingsDraftFromForm());
$("settings-page").addEventListener("pointerdown", (ev) => {
  if (ev.target === $("settings-page")) requestSettingsClose({ allowDiscard: false });
});
for (const hotkeyFieldId of HOTKEY_FIELD_IDS) {
  const field = $(hotkeyFieldId);
  field.addEventListener("focus", () => beginHotkeyCapture(hotkeyFieldId));
  field.addEventListener("click", () => beginHotkeyCapture(hotkeyFieldId));
  field.addEventListener("keydown", (ev) => recordHotkey(hotkeyFieldId, ev));
  field.addEventListener("mousedown", (ev) => recordMouseHotkey(hotkeyFieldId, ev));
  field.addEventListener("auxclick", (ev) => ev.preventDefault());
  field.addEventListener("contextmenu", (ev) => ev.preventDefault());
  field.addEventListener("paste", (ev) => ev.preventDefault());
  field.addEventListener("blur", () => {
    if (activeHotkeyCaptureId === hotkeyFieldId) endHotkeyCapture(hotkeyFieldId, "Shortcut unchanged.");
  });
}

function activateSettingsTab(tab, { focus = false } = {}) {
  syncSettingsDraftFromForm();
  document
    .querySelectorAll("#settings-tabs .tab")
    .forEach((t) => {
      t.classList.toggle("active", t === tab);
      t.setAttribute("aria-selected", String(t === tab));
      t.setAttribute("tabindex", t === tab ? "0" : "-1");
    });
  syncSettingsFooterForTab();
  document.querySelectorAll(".settings-section").forEach((section) => {
    section.hidden = section.dataset.section !== tab.dataset.tab;
  });
  renderVisibleSettingsSection();
  if (focus) tab.focus();
}

document.querySelectorAll("#settings-tabs .tab").forEach((tab) => {
  tab.addEventListener("click", () => activateSettingsTab(tab));
});

$("settings-tabs").addEventListener("keydown", (event) => {
  const tabs = [...document.querySelectorAll("#settings-tabs .tab")];
  const currentIndex = tabs.indexOf(document.activeElement);
  if (currentIndex < 0) return;
  let nextIndex;
  switch (event.key) {
    case "ArrowLeft":
      nextIndex = (currentIndex - 1 + tabs.length) % tabs.length;
      break;
    case "ArrowRight":
      nextIndex = (currentIndex + 1) % tabs.length;
      break;
    case "Home":
      nextIndex = 0;
      break;
    case "End":
      nextIndex = tabs.length - 1;
      break;
    default:
      return;
  }
  event.preventDefault();
  const nextTab = tabs[nextIndex];
  activateSettingsTab(nextTab, { focus: true });
});

$("timeline").addEventListener("pointerdown", (ev) => {
  if (ev.target === $("handle-in")) startDrag("in", ev);
  else if (ev.target === $("handle-out")) startDrag("out", ev);
  else if (ev.target === $("trim-band")) startDrag("slide", ev);
  else startDrag("scrub", ev);
});
$("timeline").addEventListener("pointermove", moveDrag);

// Scroll to zoom. Bound on the stack (covers the track and the marker band
// above it via bubbling) and the ruler below; passive:false so we can stop the
// page from scrolling instead.
document
  .querySelector(".timeline-stack")
  .addEventListener("wheel", onTimelineWheel, { passive: false });
$("ruler").addEventListener("wheel", onTimelineWheel, { passive: false });

// Navigator (whole-clip minimap): drag the box to pan, its grips to zoom.
$("overview").addEventListener("pointerdown", onOverviewPointerDown);
$("overview").addEventListener("pointermove", moveOverviewDrag);
$("overview").addEventListener("pointerup", endOverviewDrag);
$("overview").addEventListener("pointercancel", endOverviewDrag);
$("overview").addEventListener("lostpointercapture", endOverviewDrag);
$("overview").addEventListener("wheel", onOverviewWheel, { passive: false });

stage.addEventListener("pointermove", noteActivity);
stage.addEventListener("pointerdown", noteActivity);
stage.addEventListener("pointerleave", () => {
  // Leaving the stage while playing hides the bar immediately.
  lastActivityMs = -Infinity;
  updateOverlay();
});
new ResizeObserver(updateStageFrame).observe(stage);
$("timeline").addEventListener("pointerup", endDrag);
$("timeline").addEventListener("pointercancel", endDrag);
$("timeline").addEventListener("lostpointercapture", endDrag);

document.addEventListener("keydown", (ev) => {
  if (document.querySelector("dialog[open]")) return; // a dialog owns the keyboard
  if (ev.code === "Escape" && settingsOpen) {
    ev.preventDefault();
    requestSettingsClose();
    return;
  }
  if (settingsOpen) return; // player shortcuts are inert behind the page
  const tag = ev.target && ev.target.tagName;
  if (tag === "INPUT" || tag === "SELECT" || tag === "TEXTAREA") return;
  // "?" opens the shortcuts guide from anywhere in the player (clip or not).
  if (ev.code === "Slash" && ev.shiftKey) {
    ev.preventDefault();
    $("keys-dialog").showModal();
    return;
  }
  // Gallery multi-select shortcuts (only when the library view is shown).
  const galleryVisible = !$("gallery-view").hidden;
  if (galleryVisible && !currentClip && (selectMode || selectedClipPaths.size > 0)) {
    if (ev.code === "Escape") {
      ev.preventDefault();
      if (selectedClipPaths.size > 0) clearSelection();
      else exitSelectMode();
      return;
    }
    if (ev.code === "KeyA" && ev.ctrlKey && selectMode) {
      ev.preventDefault();
      selectAllVisible();
      return;
    }
  }
  if (!currentClip) return;
  if (ev.code === "Escape" && reviewFullscreenActive()) return;
  const intent = keyIntent(ev.code, ev.shiftKey);
  if (!intent) return;
  ev.preventDefault();
  noteActivity();
  switch (intent.kind) {
    case "toggle-play": togglePlay(); break;
    case "seek-by": seekBy(intent.seconds); break;
    case "step-frame": stepFrame(intent.dir); break;
    case "seek-to": seekTo(intent.seconds); break;
    case "seek-to-end": seekTo(clipDuration()); break;
    case "set-in":
      if (!legacyTimelineEnabled() && !simpleTrimMode) setSimpleTrimMode(true);
      setTrim(video.currentTime || 0, trimEnd);
      break;
    case "set-out":
      if (!legacyTimelineEnabled() && !simpleTrimMode) setSimpleTrimMode(true);
      setTrim(trimStart, video.currentTime || 0);
      break;
    case "next-marker": jumpMarker(1); break;
    case "prev-marker": jumpMarker(-1); break;
    case "next-edit": jumpEdit(1); break;
    case "prev-edit": jumpEdit(-1); break;
    case "zoom":
      if (legacyTimelineEnabled() || simpleTrimMode) zoomAtPlayhead(intent.factor);
      break;
    case "zoom-fit":
      if (legacyTimelineEnabled() || simpleTrimMode) zoomFit();
      break;
    case "zoom-selection":
      if (legacyTimelineEnabled() || simpleTrimMode) zoomToSelection();
      break;
    case "toggle-snap":
      if (legacyTimelineEnabled()) toggleSnap();
      break;
    case "toggle-fullscreen": toggleReviewFullscreen(); break;
    case "close": closeReview(); break;
  }
});

function maybeWarnElevatedGame(game) {
  const dialog = $("elevation-dialog");
  if (!game || !game.active || !game.elevated_hotkeys_blocked) {
    if (dialog.open && !elevationRestartInFlight) dialog.close();
    return;
  }
  const processInstanceId = String(game.process_instance_id || "");
  if (!processInstanceId || warnedElevatedGameProcesses.has(processInstanceId)) return;
  if (dialog.open) return;

  warnedElevatedGameProcesses.add(processInstanceId);
  $("elevation-game-name").textContent = game.name || game.exe_name || "This game";
  dialog.showModal();
}

async function restartAsAdministrator() {
  const button = $("elevation-restart");
  const cancel = $("elevation-cancel");
  const dialog = $("elevation-dialog");
  elevationRestartInFlight = true;
  button.disabled = true;
  cancel.disabled = true;
  button.textContent = "Waiting for Windows...";
  $("error").textContent = "";
  try {
    const restarted = await invoke("restart_as_administrator");
    if (!restarted) {
      button.disabled = false;
      cancel.disabled = false;
      button.textContent = "Restart as Administrator";
    }
  } catch (error) {
    button.disabled = false;
    cancel.disabled = false;
    button.textContent = "Restart as Administrator";
    $("error").textContent = String(error);
    if (!dialog.open) {
      const processInstanceId = String(
        (activeDetectedGame && activeDetectedGame.process_instance_id) || "",
      );
      if (processInstanceId) warnedElevatedGameProcesses.delete(processInstanceId);
    }
  } finally {
    elevationRestartInFlight = false;
  }
  maybeWarnElevatedGame(activeDetectedGame);
}

/* ---- boot ---- */

updateViews();
syncPlayState();
syncVolume();
syncAllRangeProgress();

function releaseBackgroundUi() {
  localClipsRequestGate.invalidate();
  cloudClipsRequestGate.invalidate();
  cloudClipsLoading = false;
  if (micTestRunning || micAudioContext) stopMicTestUi("stopped");
  releaseBackgroundSettingsUi();
  suspendReviewPlayback({ renderGallery: false });
  clearHeavyGalleryDom();
}

function startForegroundBootWork() {
  if (foregroundBootCompleted || foregroundBootPromise) return;
  const lifecycleWork = captureForegroundWork();
  if (!lifecycleWork) return;
  const pending = loadInitialSettings(lifecycleWork)
    .then((completed) => {
      if (completed && isForegroundWorkCurrent(lifecycleWork)) {
        foregroundBootCompleted = true;
      }
    })
    .catch((e) => {
      if (isForegroundWorkCurrent(lifecycleWork)) $("error").textContent = e;
    })
    .finally(() => {
      if (foregroundBootPromise !== pending) return;
      const supersededByNewForeground =
        !foregroundBootCompleted
        && !isForegroundWorkCurrent(lifecycleWork)
        && !!captureForegroundWork();
      foregroundBootPromise = null;
      if (supersededByNewForeground) startForegroundBootWork();
    });
  foregroundBootPromise = pending;
  refreshMemoryUsage();
}

function applyWindowLifecycleSnapshot(snapshot) {
  const wasKnown = windowLifecycleState.known;
  const transition = WindowLifecycleCore.applySnapshot(
    windowLifecycleState,
    snapshot,
  );
  if (!transition.accepted) return;
  windowLifecycleState = transition.state;

  if (transition.missedBackground) releaseBackgroundUi();
  if (windowLifecycleState.backgrounded) {
    if (!wasKnown || transition.enteredBackground) releaseBackgroundUi();
    return;
  }

  if (transition.enteredForeground || transition.missedBackground) {
    startForegroundBootWork();
    resumeForegroundSettingsWork();
    if (transition.refreshRequired) requestRefresh();
  }
}

async function reportFrontendReady() {
  try {
    await windowLifecycleListenerReady;
    const response = await invoke("frontend_ready");
    const warnings = response && response.warnings;
    if (Array.isArray(warnings) && warnings.length) {
      $("error").textContent = warnings.join(" ");
    }
    applyWindowLifecycleSnapshot(response && response.window_lifecycle);
  } catch (e) {
    console.warn("frontend_ready failed:", e);
  }
}
async function loadInitialSettings(lifecycleWork = captureForegroundWork()) {
  if (!lifecycleWork) return false;
  await loadGamePlugins(lifecycleWork);
  if (!isForegroundWorkCurrent(lifecycleWork)) return false;
  const needsFirstRunSetup = await invoke("needs_first_run_setup");
  if (!isForegroundWorkCurrent(lifecycleWork)) return false;
  let settings = await invoke("get_settings");
  if (!isForegroundWorkCurrent(lifecycleWork)) return false;
  // The registry Run key is the ground truth for startup. Reconcile the UI
  // in case the entry was changed externally since the last save.
  try {
    settings = { ...settings, open_on_startup: await invoke("get_autostart_status") };
  } catch (e) {
    if (!isForegroundWorkCurrent(lifecycleWork)) return false;
    console.warn("could not read autostart status:", e);
  }
  if (!isForegroundWorkCurrent(lifecycleWork)) return false;
  fillSettings(settings);
  if (needsFirstRunSetup) await openFirstRunSetup(settings);
  window.setTimeout(() => {
    if (isForegroundWorkCurrent(lifecycleWork)) {
      checkForUpdates({ manual: false });
    }
  }, 1500);
  // Custom-game icons live in settings; refresh clip badges once they load.
  if (clipsCache.length) renderClips();
  return true;
}
reportFrontendReady();
queryFfmpegRuntimeStatus().catch((error) => {
  console.warn("ffmpeg runtime status failed:", error);
});
setInterval(() => {
  if (!document.hidden && captureForegroundWork()) refreshMemoryUsage();
}, 2000);
document.addEventListener("visibilitychange", () => {
  if (!document.hidden && captureForegroundWork()) refreshMemoryUsage();
});
