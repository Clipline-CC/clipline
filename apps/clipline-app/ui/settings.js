// Settings page: form I/O, capture region, devices, games.
var gamePluginSettingsDialogPluginId = null;
var gamePluginSettingsDialogTab = "general";
var activeEncoderLabel = "";
// Live League game-type recording toggles; survives dialog open/close because
// the checkbox inputs are removed from the DOM when the dialog closes.
var leagueModeSettings = null;

const LEAGUE_MODE_RECORD_LABELS = [
  ["record_ranked_solo_duo", "Ranked Solo/Duo"],
  ["record_ranked_flex", "Ranked Flex"],
  ["record_normal", "Normal"],
  ["record_aram", "ARAM"],
  ["record_arena", "Arena"],
  ["record_custom", "Custom"],
  ["record_replay", "Replay"],
  ["record_other", "Other"],
  ["record_unknown", "Unknown (client lookup failed)"],
];

function defaultLeagueModeSettings() {
  return {
    record_ranked_solo_duo: true,
    record_ranked_flex: true,
    record_normal: true,
    record_aram: true,
    record_arena: true,
    record_custom: true,
    record_replay: true,
    record_other: true,
    record_unknown: true,
  };
}

function readLeagueModeSettingInputs() {
  const next = { ...defaultLeagueModeSettings(), ...(leagueModeSettings || {}) };
  for (const [key] of LEAGUE_MODE_RECORD_LABELS) {
    const input = document.querySelector(`[data-league-mode-record="${key}"]`);
    if (input) next[key] = input.checked;
  }
  return next;
}

function updateLeagueModeSetting() {
  leagueModeSettings = readLeagueModeSettingInputs();
  syncSettingsDraftFromForm();
}

function cloneSettings(settings) {
  return settings ? JSON.parse(JSON.stringify(settings)) : null;
}

var settingsDiscardWarningArmed = false;
var settingsIndicatorBaseline = null;

function stableSettingsSnapshot(value) {
  if (Array.isArray(value)) {
    return value.map(stableSettingsSnapshot);
  }
  if (value && typeof value === "object") {
    return Object.keys(value)
      .sort()
      .reduce((out, key) => {
        out[key] = stableSettingsSnapshot(value[key]);
        return out;
      }, {});
  }
  return value;
}

function stripEphemeralSettingsState(value) {
  const stable = stableSettingsSnapshot(value ?? null);
  if (!stable || typeof stable !== "object" || Array.isArray(stable)) return stable;
  if (!stable.cloud || typeof stable.cloud !== "object" || Array.isArray(stable.cloud)) return stable;
  const out = { ...stable };
  const cloud = { ...out.cloud };
  delete cloud.uploads;
  out.cloud = cloud;
  return out;
}

function settingsSnapshot(value) {
  return JSON.stringify(stripEphemeralSettingsState(value));
}

function settingsBaselineForComparison() {
  return settingsIndicatorBaseline || currentSettings;
}

function settingsHaveUnsavedChanges() {
  return settingsSnapshot(settingsDraft) !== settingsSnapshot(settingsBaselineForComparison());
}

function settingsValueAtPath(source, path) {
  return String(path || "")
    .split(".")
    .filter(Boolean)
    .reduce((value, key) => {
      if (value == null) return undefined;
      if (Array.isArray(value)) return value.find((item) => item && String(item.id) === key);
      return value[key];
    }, source);
}

function settingKeyChanged(path, draft, baseline) {
  return settingsSnapshot(settingsValueAtPath(draft, path))
    !== settingsSnapshot(settingsValueAtPath(baseline, path));
}

function settingsNodeKeys(node) {
  return String(node.dataset.settingsKey || "")
    .split(/\s+/)
    .filter(Boolean);
}

function syncSettingsChangeIndicators() {
  const draft = settingsDraft || {};
  const baseline = settingsBaselineForComparison() || {};
  const dirtyTabs = new Set();
  document.querySelectorAll("#settings-page [data-settings-key]").forEach((node) => {
    const changed = settingsNodeKeys(node).some((key) => settingKeyChanged(key, draft, baseline));
    node.classList.toggle("setting-changed", changed);
    const section = node.closest(".settings-section");
    if (changed && section && section.dataset.section) dirtyTabs.add(section.dataset.section);
  });
  document.querySelectorAll("#settings-tabs .tab").forEach((tab) => {
    const changed = dirtyTabs.has(tab.dataset.tab);
    tab.classList.toggle("settings-tab-changed", changed);
    if (changed) tab.setAttribute("aria-label", `${tab.textContent.trim()} has unsaved changes`);
    else tab.removeAttribute("aria-label");
  });
}

function resetSettingsBaselineFromForm() {
  settingsIndicatorBaseline = readSettings();
  settingsDraft = cloneSettings(settingsIndicatorBaseline);
  syncSettingsDirtyState({ resetDiscard: true });
}

function refreshSettingsBaselineIfClean() {
  if (settingsHaveUnsavedChanges()) {
    syncSettingsDirtyState();
    return;
  }
  resetSettingsBaselineFromForm();
}

function resetSettingsDiscardWarning() {
  settingsDiscardWarningArmed = false;
  $("settings-discard-warning").hidden = true;
  $("settings-save").classList.remove("settings-save-glow");
  $("settings-popup-shell").classList.remove("settings-shake");
}

function syncSettingsDirtyState({ resetDiscard = false } = {}) {
  const dirty = settingsHaveUnsavedChanges();
  if (resetDiscard || !dirty) resetSettingsDiscardWarning();
  $("settings-close").textContent = dirty ? "Discard Changes" : "Close";
  $("settings-close").classList.toggle("settings-discard", dirty);
  $("settings-save").classList.toggle("settings-save-glow", dirty && settingsDiscardWarningArmed);
  syncSettingsChangeIndicators();
  syncSettingsFooterForTab(dirty);
  return dirty;
}

function syncSettingsFooterForTab(dirty = settingsHaveUnsavedChanges()) {
  const activeTab = document.querySelector("#settings-tabs .tab.active");
  const onSupport = activeTab && activeTab.dataset.tab === "support";
  const view = SupportCore.view("idle", {
    uploadAvailable: false,
    settingsDirty: dirty,
  });
  $("settings-save").hidden = Boolean(onSupport && !view.settingsSaveVisible);
  $("settings-save").textContent = onSupport ? view.settingsSaveLabel : "Save Settings";
}

function showSettingsDiscardWarning() {
  settingsDiscardWarningArmed = true;
  $("settings-discard-warning").textContent = "Careful--your changes aren't saved.";
  $("settings-discard-warning").hidden = false;
  $("settings-save").classList.add("settings-save-glow");
  const shell = $("settings-popup-shell");
  shell.classList.remove("settings-shake");
  void shell.offsetWidth;
  shell.classList.add("settings-shake");
}

function settingsFormSource() {
  return settingsDraft || currentSettings || {};
}

function syncSettingsDraftFromForm({ resetDiscard = true } = {}) {
  settingsDraft = readSettings();
  syncSettingsDirtyState({ resetDiscard });
  return settingsDraft;
}

// Booth (default) needs no attribute; alternate palettes use [data-theme]
// override blocks in styles.css.
function applyUiTheme(theme) {
  if (theme === "booth") delete document.documentElement.dataset.theme;
  else document.documentElement.dataset.theme = theme;
}

function fillSettings(s) {
  const audio = { ...defaultAudioSettings(), ...(s.audio || {}) };
  const replayStorage = { ...defaultReplayStorageSettings(), ...(s.replay_storage || {}) };
  const games = { ...defaultGameSettings(), ...(s.games || {}) };
  const cloud = { ...defaultCloudSettings(), ...(s.cloud || {}) };
  const osu = { ...defaultOsuApiSettings(), ...(s.osu || {}) };
  const league = { ...defaultLeagueModeSettings(), ...(s.league || {}) };
  const replay = Math.min(120, Math.max(5, Number(s.replay_window_s) || 60));
  cloud.uploads = { ...(cloud.uploads || {}) };
  gamePluginSettings = normalizeGamePluginSettingsMap(games.plugins || {});
  customGames = (games.custom_games || []).map(normalizeCustomGame);
  currentSettings = {
    ...s,
    audio,
    replay_storage: replayStorage,
    cloud,
    osu,
    league,
    games: {
      ...games,
      plugins: { ...gamePluginSettings },
      custom_games: customGames.map((game) => ({ ...game })),
    },
  };
  leagueModeSettings = { ...league };
  settingsDraft = cloneSettings(currentSettings);
  regionState = s.capture_region ?? regionState;
  captureTargetDirty = false;
  renderCaptureTargetSelect();
  $("set-games-auto-detect").checked = !!games.auto_detect;
  $("set-games-pause-when-empty").checked = !!games.pause_when_no_game;
  $("set-output-enabled").checked = !!audio.output_enabled;
  $("set-audio-split-output").checked = audio.split_output_by_process === true;
  $("set-output-volume").value = String(Number.isFinite(audio.output_volume) ? audio.output_volume : 1);
  $("set-mic-enabled").checked = !!audio.mic_enabled;
  $("set-mic-volume").value = String(Number.isFinite(audio.mic_volume) ? audio.mic_volume : 1);
  $("set-mic-mono").checked = (audio.mic_channels || "mono") === "mono";
  $("set-buffer").value = replay;
  $("set-replay").value = replay;
  $("set-backend").value = s.capture_backend || "auto";
  $("set-encoder").value = s.video_encoder || "auto";
  $("set-output-resolution").value = outputResolutionOption(s.output_resolution).id;
  $("set-bitrate").value = s.video_quality
    ? PlayerCore.qualityIndexForId(s.video_quality)
    : qualityIndexForBitrate(s.bitrate_mbps, $("set-output-resolution").value);
  $("set-fps").value = smoothnessIndexForFps(s.fps);
  const advanced = {
    ...advancedRecordingFromPresetControls(),
    ...(s.advanced_recording || {}),
  };
  $("recording-mode-basic").checked = !advanced.enabled;
  $("recording-mode-advanced").checked = !!advanced.enabled;
  $("set-output-width").value = String(advanced.output_width);
  $("set-output-height").value = String(advanced.output_height);
  $("set-custom-bitrate").value = String(advanced.bitrate_mbps);
  $("set-custom-fps").value = String(advanced.fps);
  $("set-quota").value = s.disk_quota_gb;
  $("set-auto-delete-when-over-quota").checked = !!s.auto_delete_when_over_quota;
  $("set-media-dir").value = s.media_dir ?? "";
  $("set-replay-disk-enabled").checked = replayStorage.mode === "disk";
  $("set-replay-disk-dir").value = replayStorage.disk_dir || "";
  $("set-replay-disk-quota").value = replayStorage.disk_quota_gb ?? 2;
  $("set-replay-disk-ack").checked = !!replayStorage.disk_acknowledged;
  $("set-hotkey").value = s.hotkey;
  $("set-hotkey-2").value = s.hotkey_secondary || "";
  $("set-recording-hotkey").value = s.recording_hotkey || "";
  $("set-recording-hotkey-2").value = s.recording_hotkey_secondary || "";
  $("set-bookmark-hotkey").value = s.bookmark_hotkey || "";
  $("set-bookmark-hotkey-2").value = s.bookmark_hotkey_secondary || "";
  updateHotkeyLabels(s.hotkey, s.hotkey_secondary || "");
  $("set-open-on-startup").checked = !!s.open_on_startup;
  $("set-close-to-tray").checked = s.close_to_tray !== false;
  $("set-minimize-to-tray").checked = !!s.minimize_to_tray;
  $("set-legacy-timeline-editor").checked = !!s.legacy_timeline_editor;
  $("set-theme").value = s.ui_theme || "booth";
  applyUiTheme(s.ui_theme);
  $("set-update-channel").value = s.update_channel || "nightly";
  fillCloudSettings(cloud);
  endAllHotkeyCaptures();
  syncCaptureFields();
  renderAudioDeviceSelects();
  renderVideoEncoderSelect();
  syncAudioFields();
  syncRecordingFields();
  syncReplayStorageFields();
  renderGamePlugins();
  renderCustomGames();
  updateGameDetectionStatus();
  updateCaptureStatus();
  syncUploadClipButton();
  applyTimelineEditorPreference();
  renderClips();
  resetSettingsBaselineFromForm();
}

function readSettings() {
  const replay = Number($("set-replay").value);
  const capture = selectedCaptureSettings();
  const source = settingsFormSource();
  const preserveLegacyWindow =
    !captureTargetDirty
    && source.capture_mode === "window_title"
    && String(source.window_title || "").trim().length > 0;
  return {
    capture_mode: preserveLegacyWindow ? "window_title" : capture.capture_mode,
    capture_backend: $("set-backend").value,
    window_title: preserveLegacyWindow ? source.window_title : "",
    capture_region: preserveLegacyWindow
      ? (source.capture_region || capture.capture_region)
      : capture.capture_region,
    games: {
      auto_detect: $("set-games-auto-detect").checked,
      pause_when_no_game: $("set-games-pause-when-empty").checked,
      plugins: readGamePluginSettings(),
      custom_games: customGames.map((game) => ({ ...game })),
    },
    audio: {
      output_enabled: $("set-output-enabled").checked,
      output_device_id: selectedDeviceId("set-output-device"),
      output_volume: Number($("set-output-volume").value),
      split_output_by_process: $("set-audio-split-output").checked,
      mic_enabled: $("set-mic-enabled").checked,
      mic_device_id: selectedDeviceId("set-mic-device"),
      mic_volume: Number($("set-mic-volume").value),
      mic_channels: $("set-mic-mono").checked ? "mono" : "stereo",
    },
    // Persisted for compatibility with older settings files. Runtime retention
    // derives from replay_window_s.
    buffer_seconds: replay,
    replay_window_s: replay,
    video_encoder: $("set-encoder").value,
    output_resolution: outputResolutionOption($("set-output-resolution").value).id,
    video_quality: recordingQualityPreset(Number($("set-bitrate").value)).id,
    bitrate_mbps: recordingQualityPreset(
      Number($("set-bitrate").value),
      $("set-output-resolution").value
    ).bitrate,
    fps: smoothnessPreset(Number($("set-fps").value)).fps,
    advanced_recording: readAdvancedRecordingSettings(),
    disk_quota_gb: Number($("set-quota").value),
    auto_delete_when_over_quota: $("set-auto-delete-when-over-quota").checked,
    media_dir: $("set-media-dir").value.trim(),
    replay_storage: {
      mode: $("set-replay-disk-enabled").checked ? "disk" : "memory",
      disk_dir: $("set-replay-disk-dir").value.trim(),
      disk_quota_gb: Number($("set-replay-disk-quota").value),
      disk_acknowledged: $("set-replay-disk-ack").checked,
    },
    ...readHotkeySettings(),
    open_on_startup: $("set-open-on-startup").checked,
    close_to_tray: $("set-close-to-tray").checked,
    minimize_to_tray: $("set-minimize-to-tray").checked,
    legacy_timeline_editor: $("set-legacy-timeline-editor").checked,
    ui_theme: $("set-theme").value,
    update_channel: $("set-update-channel").value,
    cloud: readCloudSettings(),
    osu: readOsuApiSettings(),
    league: { ...leagueModeSettings },
  };
}

// Either field may be cleared with Esc, so the first non-empty keybind is
// promoted to the primary slot; the backend rejects an empty primary.
function readHotkeySettings() {
  const keybinds = ["set-hotkey", "set-hotkey-2"]
    .map((fieldId) => $(fieldId).value.trim())
    .filter(Boolean);
  const recordingKeybinds = ["set-recording-hotkey", "set-recording-hotkey-2"]
    .map((fieldId) => $(fieldId).value.trim())
    .filter(Boolean);
  const bookmarkKeybinds = ["set-bookmark-hotkey", "set-bookmark-hotkey-2"]
    .map((fieldId) => $(fieldId).value.trim())
    .filter(Boolean);
  return {
    hotkey: keybinds[0] || "",
    hotkey_secondary: keybinds[1] || null,
    recording_hotkey: recordingKeybinds[0] || null,
    recording_hotkey_secondary: recordingKeybinds[1] || null,
    // Always sent, so clearing the field is persisted as null rather than
    // reverting to the default keybind on the next load.
    bookmark_hotkey: bookmarkKeybinds[0] || null,
    bookmark_hotkey_secondary: bookmarkKeybinds[1] || null,
  };
}

function defaultAudioSettings() {
  return {
    output_enabled: true,
    output_device_id: null,
    output_volume: 1,
    split_output_by_process: false,
    mic_enabled: false,
    mic_device_id: null,
    mic_volume: 1,
    mic_channels: "mono",
  };
}

function outputBoundsForResolution(id) {
  switch (outputResolutionOption(id).id) {
    case "480p": return { width: 854, height: 480 };
    case "720p": return { width: 1280, height: 720 };
    case "1080p": return { width: 1920, height: 1080 };
    case "1440p": return { width: 2560, height: 1440 };
    case "source": return { width: 2560, height: 16384 };
    default: return { width: 2560, height: 16384 };
  }
}

function numberFieldValue(id, fallback, { integer = false } = {}) {
  const value = Number($(id).value);
  if (!Number.isFinite(value)) return fallback;
  return integer ? Math.round(value) : value;
}

function advancedRecordingFromPresetControls() {
  const bounds = outputBoundsForResolution($("set-output-resolution").value);
  const quality = recordingQualityPreset(Number($("set-bitrate").value), $("set-output-resolution").value);
  const smoothness = smoothnessPreset(Number($("set-fps").value));
  return {
    enabled: false,
    output_width: bounds.width,
    output_height: bounds.height,
    bitrate_mbps: quality.bitrate,
    fps: smoothness.fps,
  };
}

function isAdvancedRecordingMode() {
  return $("recording-mode-advanced").checked;
}

function readAdvancedRecordingSettings() {
  const fallback = advancedRecordingFromPresetControls();
  return {
    enabled: isAdvancedRecordingMode(),
    output_width: numberFieldValue("set-output-width", fallback.output_width, { integer: true }),
    output_height: numberFieldValue("set-output-height", fallback.output_height, { integer: true }),
    bitrate_mbps: numberFieldValue("set-custom-bitrate", fallback.bitrate_mbps),
    fps: numberFieldValue("set-custom-fps", fallback.fps, { integer: true }),
  };
}

function currentRecordingBitrateMbps() {
  if (isAdvancedRecordingMode()) {
    return numberFieldValue(
      "set-custom-bitrate",
      recordingQualityPreset(Number($("set-bitrate").value), $("set-output-resolution").value).bitrate
    );
  }
  return recordingQualityPreset(Number($("set-bitrate").value), $("set-output-resolution").value).bitrate;
}

function defaultReplayStorageSettings() {
  return {
    mode: "memory",
    disk_dir: "",
    disk_quota_gb: 2,
    disk_acknowledged: false,
  };
}

function defaultGameSettings() {
  return {
    auto_detect: true,
    pause_when_no_game: false,
    plugins: {},
    custom_games: [],
  };
}

function defaultCloudSettings() {
  return {
    host_url: "",
    public_url: null,
    connected_user_id: null,
    connected_username: null,
    connected_display_name: null,
    credential_target: null,
    default_visibility: "private",
    delete_local_after_upload: false,
    auto_upload_rules: false,
    uploads: {},
  };
}

function defaultOsuApiSettings() {
  return {
    client_id: null,
    user: null,
    credential_target: null,
    last_connected_username: null,
  };
}

function osuApiSettings() {
  return currentSettings && currentSettings.osu ? currentSettings.osu : defaultOsuApiSettings();
}

function readOsuApiSettings() {
  const source = settingsFormSource();
  return {
    ...defaultOsuApiSettings(),
    ...(source.osu || {}),
  };
}

function defaultGamePluginSettings(plugin) {
  return {
    enabled: plugin ? plugin.default_enabled !== false : true,
    recording_mode: normalizeGameRecordingMode(
      plugin && plugin.default_recording_mode ? plugin.default_recording_mode : "full_session"
    ),
    review: defaultGamePluginReviewSettings(plugin),
  };
}

function defaultGamePluginReviewSettings(plugin = null) {
  return normalizeGamePluginReviewSettings(
    plugin && plugin.default_review ? plugin.default_review : null
  );
}

function normalizeGamePluginReviewSettings(settings) {
  return PlayerCore.normalizeGameReviewSettings(settings);
}

function normalizeGameRecordingMode(mode) {
  return mode === "full_session" ? "full_session" : "replays_only";
}

function normalizeGamePluginId(raw) {
  return String(raw || "")
    .trim()
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "_")
    .replace(/^_+|_+$/g, "");
}

function normalizeGamePluginSettings(settings, plugin = null) {
  const defaults = defaultGamePluginSettings(plugin);
  return {
    enabled: settings && Object.prototype.hasOwnProperty.call(settings, "enabled")
      ? settings.enabled !== false
      : defaults.enabled,
    recording_mode: normalizeGameRecordingMode(
      settings && settings.recording_mode ? settings.recording_mode : defaults.recording_mode
    ),
    review: normalizeGamePluginReviewSettings(settings && settings.review ? settings.review : defaults.review),
  };
}

function normalizeGamePluginSettingsMap(settings) {
  const out = {};
  for (const [id, value] of Object.entries(settings || {})) {
    const cleanId = normalizeGamePluginId(id);
    if (cleanId) out[cleanId] = normalizeGamePluginSettings(value);
  }
  return out;
}

function normalizeCustomGame(game) {
  return {
    id: String(game.id || `custom-${Date.now()}`),
    legacy_ids: Array.isArray(game.legacy_ids) ? game.legacy_ids.map(String) : [],
    name: String(game.name || game.exe_name || game.window_title || "Custom game").trim(),
    enabled: game.enabled !== false,
    exe_name: String(game.exe_name || "").trim(),
    process_path: game.process_path ? String(game.process_path).trim() : null,
    window_title: String(game.window_title || "").trim(),
    recording_mode: normalizeGameRecordingMode(game.recording_mode),
    icon: game.icon ? String(game.icon) : null,
  };
}

function selectedRecordingMode(name, fallback = "replays_only") {
  const input = document.querySelector(`input[name="${name}"]:checked`);
  return input ? normalizeGameRecordingMode(input.value) : normalizeGameRecordingMode(fallback);
}

function setRecordingMode(name, mode) {
  const normalized = normalizeGameRecordingMode(mode);
  document.querySelectorAll(`input[name="${name}"]`).forEach((input) => {
    input.checked = input.value === normalized;
  });
}

function gamePluginSetting(plugin) {
  return normalizeGamePluginSettings(gamePluginSettings[plugin.id], plugin);
}

function syncGamePluginSettingsDraft() {
  if (currentSettings || settingsDraft) {
    settingsDraft = readSettings();
    syncSettingsDirtyState({ resetDiscard: true });
  }
}

function gamePluginSettingsDialogPlugin() {
  if (!gamePluginSettingsDialogPluginId) return null;
  return gamePlugins.find((plugin) => plugin.id === gamePluginSettingsDialogPluginId) || null;
}

function gamePluginReviewInputs(plugin) {
  return Array.from(document.querySelectorAll(`[data-game-plugin-review-setting="${plugin.id}"]`));
}

function readGamePluginReviewSettings(plugin, fallback) {
  const review = normalizeGamePluginReviewSettings(fallback);
  const master = document.querySelector(`[data-game-plugin-review-enabled="${plugin.id}"]`);
  const next = normalizeGamePluginReviewSettings({
    ...review,
    enabled: master ? master.checked : review.enabled,
  });
  for (const input of gamePluginReviewInputs(plugin)) {
    const group = input.dataset.reviewGroup;
    const key = input.dataset.reviewKey;
    if (!next[group] || !Object.prototype.hasOwnProperty.call(next[group], key)) continue;
    next[group][key] = input.checked;
  }
  return next;
}

function readGamePluginSettings() {
  const source = settingsFormSource();
  const next = {
    ...normalizeGamePluginSettingsMap(
      source.games ? source.games.plugins : {}
    ),
  };
  for (const plugin of gamePlugins) {
    const existing = gamePluginSetting(plugin);
    const checkbox = document.querySelector(`[data-game-plugin-enabled="${plugin.id}"]`);
    next[plugin.id] = normalizeGamePluginSettings({
      enabled: checkbox ? checkbox.checked : existing.enabled,
      recording_mode: selectedRecordingMode(
        `game-plugin-mode-${plugin.id}`,
        existing.recording_mode
      ),
      review: readGamePluginReviewSettings(plugin, existing.review),
    }, plugin);
  }
  gamePluginSettings = next;
  return { ...gamePluginSettings };
}

function gamePluginSummary(plugin, settings = gamePluginSetting(plugin)) {
  if (!settings.enabled) {
    return `Disabled. ${plugin.name} will not change capture or start session recordings.`;
  }
  if (settings.recording_mode === "full_session") {
    return "Full-session recording starts when the match window appears. Takes priority over matching custom games.";
  }
  return "Replay capture switches to the match window without saving a full session. Takes priority over matching custom games.";
}

function refreshReviewForSettingsChange() {
  if (clipsCache.length) renderClips();
  if (!currentClip) return;
  if (typeof renderOverviewMarkers === "function") renderOverviewMarkers();
  if (typeof renderMarkers === "function") renderMarkers();
  renderGameEventRail(currentClip);
  renderGamePlayRail(currentClip);
  renderGameMetadataPanel(currentClip);
}
