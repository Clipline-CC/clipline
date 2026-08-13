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

function updateGamePluginSummary(plugin) {
  const summary = document.querySelector(`[data-game-plugin-summary="${plugin.id}"]`);
  if (summary) summary.textContent = gamePluginSummary(plugin);
}

function renderGamePluginModeControl(plugin, settings) {
  const control = document.createElement("div");
  control.className = "segmented-control game-profile-mode";
  control.setAttribute("role", "radiogroup");
  control.setAttribute("aria-label", `${plugin.name} recording mode`);
  [
    ["replays_only", "Replays only"],
    ["full_session", "Full session"],
  ].forEach(([value, label]) => {
    const option = document.createElement("label");
    const input = document.createElement("input");
    input.type = "radio";
    input.name = `game-plugin-mode-${plugin.id}`;
    input.value = value;
    input.checked = settings.recording_mode === value;
    input.addEventListener("change", () => {
      if (input.checked) {
        gamePluginSettings[plugin.id] = normalizeGamePluginSettings({
          ...gamePluginSetting(plugin),
          recording_mode: value,
        }, plugin);
        updateGamePluginSummary(plugin);
        updateGameDetectionStatus();
        syncGamePluginSettingsDraft();
      }
    });
    const text = document.createElement("span");
    text.textContent = label;
    option.append(input, text);
    control.appendChild(option);
  });
  return control;
}

const GAME_REVIEW_GROUPS = [
  {
    id: "match_events",
    label: "Match events",
    options: [
      ["user_kills", "User kills"],
      ["user_deaths", "User deaths"],
      ["user_assists", "User assists"],
      ["team_kills", "Ally kills"],
      ["team_deaths", "Ally deaths"],
      ["enemy_kills", "Enemy kills"],
      ["enemy_deaths", "Enemy deaths"],
      ["objectives", "Objectives"],
      ["turrets", "Structures"],
    ],
  },
  {
    id: "timeline_markers",
    label: "Timeline markers",
    options: [
      ["user_kills", "User kills"],
      ["user_deaths", "User deaths"],
      ["user_assists", "User assists"],
      ["objectives", "Objectives"],
      ["turrets", "Structures"],
    ],
  },
];

const GAME_REVIEW_OPTION_GROUPS = {
  match_events: [
    {
      label: "Your events",
      keys: ["user_kills", "user_deaths", "user_assists"],
    },
    {
      label: "Team fights",
      keys: ["team_kills", "team_deaths", "enemy_kills", "enemy_deaths"],
    },
    {
      label: "Map events",
      keys: ["objectives", "turrets"],
    },
  ],
  timeline_markers: [
    {
      label: "Your markers",
      keys: ["user_kills", "user_deaths", "user_assists"],
    },
    {
      label: "Map markers",
      keys: ["objectives", "turrets"],
    },
  ],
};

const GAME_PLUGIN_SETTINGS_TAB_DEFINITIONS = {
  general: { label: "General" },
  match_events: { label: "Match events", requiresEventMarkers: true },
  timeline_markers: { label: "Timeline markers", requiresEventMarkers: true },
  osu_account: { label: "Account", pluginIds: ["osu"] },
  osu_plays: { label: "Plays", pluginIds: ["osu"] },
};

const GAME_PLUGIN_SETTINGS_TABS = Object.keys(GAME_PLUGIN_SETTINGS_TAB_DEFINITIONS);

function gamePluginSettingsTabs(plugin) {
  if (!plugin) return ["general"];
  return GAME_PLUGIN_SETTINGS_TABS.filter((tab) => {
    const definition = GAME_PLUGIN_SETTINGS_TAB_DEFINITIONS[tab];
    if (definition.requiresEventMarkers && !plugin.event_markers) return false;
    return !definition.pluginIds || definition.pluginIds.includes(plugin.id);
  });
}

function gamePluginReviewGroupDefinition(groupId) {
  return GAME_REVIEW_GROUPS.find((group) => group.id === groupId) || GAME_REVIEW_GROUPS[0];
}

function gamePluginReviewOptionLabel(group, key) {
  const option = group.options.find(([optionKey]) => optionKey === key);
  return option ? option[1] : key;
}

function syncGamePluginReviewControls(plugin) {
  const settings = gamePluginSetting(plugin);
  const reviewEnabled = settings.review.enabled;
  const master = document.querySelector(`[data-game-plugin-review-enabled="${plugin.id}"]`);
  if (master) master.checked = reviewEnabled;
  const groups = document.querySelectorAll(`[data-game-plugin-review-group="${plugin.id}"]`);
  groups.forEach((group) => {
    const groupName = group.dataset.reviewGroup;
    const groupEnabled = Boolean(settings.review[groupName] && settings.review[groupName].enabled);
    group.classList.toggle("disabled", !reviewEnabled || !groupEnabled);
    group.querySelectorAll("input").forEach((input) => {
      if (input.dataset.reviewKey === "enabled") {
        input.disabled = !reviewEnabled;
      } else {
        input.disabled = !reviewEnabled || !groupEnabled;
      }
    });
  });
}

function updateGamePluginReviewSetting(plugin) {
  const existing = gamePluginSetting(plugin);
  gamePluginSettings[plugin.id] = normalizeGamePluginSettings({
    ...existing,
    review: readGamePluginReviewSettings(plugin, existing.review),
  }, plugin);
  syncGamePluginReviewControls(plugin);
  syncGamePluginSettingsDraft();
  refreshReviewForSettingsChange();
}

function renderReviewCheckbox(plugin, groupId, key, labelText, checked) {
  const label = document.createElement("label");
  label.className = "check-line";
  const input = document.createElement("input");
  input.type = "checkbox";
  input.checked = checked;
  input.dataset.gamePluginReviewSetting = plugin.id;
  input.dataset.reviewGroup = groupId;
  input.dataset.reviewKey = key;
  input.addEventListener("change", () => updateGamePluginReviewSetting(plugin));
  const text = document.createElement("span");
  text.textContent = labelText;
  label.append(input, text);
  return label;
}

function renderGamePluginOptionGroup(plugin, group, groupSettings, optionGroup) {
  const section = document.createElement("section");
  section.className = "game-review-option-group";
  const title = document.createElement("strong");
  title.textContent = optionGroup.label;

  const list = document.createElement("div");
  list.className = "game-review-option-list";
  for (const key of optionGroup.keys) {
    list.appendChild(renderReviewCheckbox(
      plugin,
      group.id,
      key,
      gamePluginReviewOptionLabel(group, key),
      groupSettings[key],
    ));
  }

  section.append(title, list);
  return section;
}

function renderGamePluginSettingsButton(plugin) {
  const button = document.createElement("button");
  button.type = "button";
  button.className = "game-profile-settings";
  button.dataset.gamePluginSettings = plugin.id;
  button.textContent = "Settings";
  button.setAttribute("aria-label", `${plugin.name} settings`);
  button.addEventListener("click", () => showGamePluginSettingsDialog(plugin));
  return button;
}

function renderGamePluginReviewGroup(plugin, groupId, review) {
  const group = gamePluginReviewGroupDefinition(groupId);
  const groupSettings = review[group.id];
  const section = document.createElement("section");
  section.className = "game-review-group";
  section.dataset.gamePluginReviewGroup = plugin.id;
  section.dataset.reviewGroup = group.id;

  const head = document.createElement("label");
  head.className = "check-line game-review-master-card game-review-group-head";
  const enabled = document.createElement("input");
  enabled.type = "checkbox";
  enabled.checked = groupSettings.enabled;
  enabled.dataset.gamePluginReviewSetting = plugin.id;
  enabled.dataset.reviewGroup = group.id;
  enabled.dataset.reviewKey = "enabled";
  enabled.addEventListener("change", () => updateGamePluginReviewSetting(plugin));
  const title = document.createElement("strong");
  title.textContent = group.label;
  head.append(enabled, title);

  const groups = document.createElement("div");
  groups.className = "game-review-option-groups";
  for (const optionGroup of GAME_REVIEW_OPTION_GROUPS[group.id] || []) {
    groups.appendChild(renderGamePluginOptionGroup(
      plugin,
      group,
      groupSettings,
      optionGroup,
    ));
  }

  section.append(head, groups);
  return section;
}

function renderGamePluginSettingsGeneralTab(plugin, settings) {
  const review = normalizeGamePluginReviewSettings(settings.review);
  const root = document.createElement("div");
  root.className = "game-plugin-settings-panel";

  const modeSection = document.createElement("section");
  modeSection.className = "game-plugin-settings-section";
  const modeTitle = document.createElement("strong");
  modeTitle.textContent = "Recording";
  modeSection.append(modeTitle, renderGamePluginModeControl(plugin, settings));

  root.append(modeSection);

  if (plugin.id === "osu") {
    const playsSection = document.createElement("section");
    playsSection.className = "game-plugin-settings-section";
    const playsTitle = document.createElement("strong");
    playsTitle.textContent = "Play blocks";
    const playsHint = document.createElement("span");
    playsHint.className = "hint";
    playsHint.textContent = "Recent submitted plays are fetched after a full-session recording is saved.";
    playsSection.append(playsTitle, playsHint);
    root.append(playsSection);
    return root;
  }

  const reviewSection = document.createElement("section");
  reviewSection.className = "game-plugin-settings-section";
  const master = document.createElement("label");
  master.className = "check-line game-review-master-card game-review-master";
  const masterInput = document.createElement("input");
  masterInput.type = "checkbox";
  masterInput.checked = review.enabled;
  masterInput.dataset.gamePluginReviewEnabled = plugin.id;
  masterInput.addEventListener("change", () => updateGamePluginReviewSetting(plugin));
  const masterText = document.createElement("span");
  masterText.textContent = "Show League match details";
  master.append(masterInput, masterText);
  reviewSection.append(master);

  if (plugin.id === "league_of_legends") {
    const gateSection = document.createElement("section");
    gateSection.className = "game-plugin-settings-section";
    const gateTitle = document.createElement("strong");
    gateTitle.textContent = "Record game types";
    const gateHint = document.createElement("span");
    gateHint.className = "hint";
    gateHint.textContent =
      "Automatic recording is skipped for unchecked game types. Manual recording always works.";
    gateSection.append(gateTitle, gateHint);
    for (const [key, label] of LEAGUE_MODE_RECORD_LABELS) {
      const check = document.createElement("label");
      check.className = "check-line";
      const input = document.createElement("input");
      input.type = "checkbox";
      input.checked = (leagueModeSettings || {})[key] !== false;
      input.dataset.leagueModeRecord = key;
      input.addEventListener("change", updateLeagueModeSetting);
      const text = document.createElement("span");
      text.textContent = label;
      check.append(input, text);
      gateSection.append(check);
    }
    root.append(gateSection);
  }

  root.append(reviewSection);
  return root;
}

function osuApiConnectionLabel(status = osuApiSettings()) {
  const name = status.username || status.last_connected_username;
  if (status.configured) return name ? `Connected as ${name}` : "Connected";
  if (status.secret_present) return "Saved; test the connection";
  if (status.client_id || status.user || status.credential_target) return "Client secret needed";
  return "Not configured";
}

function updateOsuApiSettingsFromStatus(status) {
  const next = {
    ...osuApiSettings(),
    client_id: status.client_id || null,
    user: status.user || null,
    credential_target: status.credential_target || null,
    last_connected_username: status.username || null,
  };
  if (currentSettings) currentSettings.osu = next;
  if (settingsDraft) settingsDraft.osu = { ...next };
}

function renderOsuApiField(labelText, input) {
  const label = document.createElement("label");
  label.className = "osu-api-field";
  const labelSpan = document.createElement("span");
  labelSpan.textContent = labelText;
  label.append(labelSpan, input);
  return label;
}

function osuApiRequestFromInputs(clientIdInput, clientSecretInput, userInput) {
  return {
    client_id: clientIdInput.value.trim(),
    client_secret: clientSecretInput.value.trim() || null,
    user: userInput.value.trim(),
  };
}

function syncOsuApiInputsFromStatus(clientIdInput, clientSecretInput, userInput, status) {
  if (status.client_id) clientIdInput.value = status.client_id;
  if (status.user) userInput.value = status.user;
  clientSecretInput.value = "";
  clientSecretInput.placeholder = status.secret_present
    ? "Leave blank to keep saved secret"
    : "Paste client secret";
}

async function saveOsuApiSettingsFromInputs(clientIdInput, clientSecretInput, userInput, status) {
  status.textContent = "Saving...";
  const result = await invoke("save_osu_api_settings", {
    request: osuApiRequestFromInputs(clientIdInput, clientSecretInput, userInput),
  });
  updateOsuApiSettingsFromStatus(result);
  syncOsuApiInputsFromStatus(clientIdInput, clientSecretInput, userInput, result);
  status.textContent = osuApiConnectionLabel(result);
  syncSettingsDraftFromForm();
  return result;
}

async function refreshOsuApiStatus(status) {
  try {
    const result = await invoke("osu_api_status");
    updateOsuApiSettingsFromStatus(result);
    status.textContent = osuApiConnectionLabel(result);
  } catch (e) {
    status.textContent = String(e);
  }
}

function renderOsuAccountSettingsTab() {
  const root = document.createElement("div");
  root.className = "game-plugin-settings-panel osu-account-panel";

  const accountSection = document.createElement("section");
  accountSection.className = "game-plugin-settings-section";
  const heading = document.createElement("div");
  heading.className = "osu-api-heading";
  const title = document.createElement("strong");
  title.textContent = "Account";
  const guide = document.createElement("button");
  guide.type = "button";
  guide.className = "osu-guide-button";
  guide.title = "Open osu! API setup guide";
  guide.setAttribute("aria-label", "Open osu! API setup guide");
  guide.textContent = "?";
  guide.addEventListener("click", async () => {
    try {
      await invoke("open_osu_api_setup_guide");
    } catch (e) {
      $("error").textContent = String(e);
    }
  });
  heading.append(title, guide);

  const osu = osuApiSettings();
  const hint = document.createElement("span");
  hint.className = "hint";
  hint.textContent = "Use your own osu! OAuth app. The client secret stays in Windows Credential Manager.";

  const fields = document.createElement("div");
  fields.className = "osu-api-fields";
  const clientId = document.createElement("input");
  clientId.type = "text";
  clientId.inputMode = "numeric";
  clientId.autocomplete = "off";
  clientId.placeholder = "Client ID";
  clientId.value = osu.client_id || "";
  const secret = document.createElement("input");
  secret.type = "password";
  secret.autocomplete = "off";
  secret.placeholder = osu.credential_target ? "Leave blank to keep saved secret" : "Paste client secret";
  const user = document.createElement("input");
  user.type = "text";
  user.autocomplete = "username";
  user.placeholder = "osu! User ID or Username";
  user.value = osu.user || "";
  fields.append(
    renderOsuApiField("Client ID", clientId),
    renderOsuApiField("Client Secret", secret),
    renderOsuApiField("osu! User ID or Username", user)
  );

  const actions = document.createElement("div");
  actions.className = "osu-account-actions";
  const save = document.createElement("button");
  save.type = "button";
  save.textContent = "Save";
  const test = document.createElement("button");
  test.type = "button";
  test.className = "primary";
  test.textContent = "Test osu! API connection";
  const status = document.createElement("span");
  status.className = "hint";
  status.textContent = osuApiConnectionLabel(osu);
  save.addEventListener("click", async () => {
    $("error").textContent = "";
    save.disabled = true;
    test.disabled = true;
    try {
      await saveOsuApiSettingsFromInputs(clientId, secret, user, status);
    } catch (e) {
      status.textContent = String(e);
    } finally {
      save.disabled = false;
      test.disabled = false;
    }
  });
  test.addEventListener("click", async () => {
    $("error").textContent = "";
    save.disabled = true;
    test.disabled = true;
    try {
      await saveOsuApiSettingsFromInputs(clientId, secret, user, status);
      status.textContent = "Testing...";
      const result = await invoke("test_osu_api_connection");
      updateOsuApiSettingsFromStatus(result.status);
      syncOsuApiInputsFromStatus(clientId, secret, user, result.status);
      const missing = result.pagination_ceiling_reached ? "; some plays may be missing" : "";
      status.textContent = `Connected. Recent scores: ${result.score_count}, failed: ${result.failed_count}${missing}`;
      await refresh();
    } catch (e) {
      status.textContent = String(e);
    } finally {
      save.disabled = false;
      test.disabled = false;
    }
  });
  actions.append(save, test, status);
  refreshOsuApiStatus(status);

  accountSection.append(heading, hint, fields, actions);
  root.append(accountSection);
  return root;
}

function renderOsuPlaysSettingsTab() {
  const root = document.createElement("div");
  root.className = "game-plugin-settings-panel";

  const playsSection = document.createElement("section");
  playsSection.className = "game-plugin-settings-section";
  const title = document.createElement("strong");
  title.textContent = "Plays";
  const list = document.createElement("div");
  list.className = "osu-play-settings-list";
  [
    "Recent submitted plays are fetched after a full-session recording is saved.",
    "Failed plays stay visible when osu! returns them; retries only appear if they were submitted.",
    "Some plays may be missing if osu!'s recent-score list reaches the 500 score ceiling.",
    "v1 tracks osu!standard only.",
  ].forEach((text) => {
    const item = document.createElement("div");
    item.textContent = text;
    list.appendChild(item);
  });
  playsSection.append(title, list);
  root.append(playsSection);
  return root;
}

function renderGamePluginSettingsMatchEventsTab(plugin, settings) {
  const root = document.createElement("div");
  root.className = "game-plugin-settings-panel";
  root.appendChild(renderGamePluginReviewGroup(
    plugin,
    "match_events",
    normalizeGamePluginReviewSettings(settings.review),
  ));
  return root;
}

function renderGamePluginSettingsTimelineMarkersTab(plugin, settings) {
  const root = document.createElement("div");
  root.className = "game-plugin-settings-panel";
  root.appendChild(renderGamePluginReviewGroup(
    plugin,
    "timeline_markers",
    normalizeGamePluginReviewSettings(settings.review),
  ));
  return root;
}

function renderGamePluginSettingsDialog(plugin = gamePluginSettingsDialogPlugin()) {
  if (!plugin) return;
  const settings = gamePluginSetting(plugin);
  const availableTabs = gamePluginSettingsTabs(plugin);
  const tab = availableTabs.includes(gamePluginSettingsDialogTab)
    ? gamePluginSettingsDialogTab
    : "general";
  gamePluginSettingsDialogTab = tab;
  gamePluginSettings[plugin.id] = settings;

  $("game-plugin-settings-title").textContent = `${plugin.name} settings`;
  $("game-plugin-settings-subtitle").textContent = "";
  document.querySelectorAll("[data-game-plugin-settings-tab]").forEach((button) => {
    button.hidden = !availableTabs.includes(button.dataset.gamePluginSettingsTab);
    const active = button.dataset.gamePluginSettingsTab === tab;
    button.classList.toggle("active", active);
    button.setAttribute("aria-selected", String(active));
    button.tabIndex = active ? 0 : -1;
  });

  const body = $("game-plugin-settings-body");
  if (tab === "match_events") {
    body.replaceChildren(renderGamePluginSettingsMatchEventsTab(plugin, settings));
  } else if (tab === "timeline_markers") {
    body.replaceChildren(renderGamePluginSettingsTimelineMarkersTab(plugin, settings));
  } else if (tab === "osu_account") {
    body.replaceChildren(renderOsuAccountSettingsTab(plugin, settings));
  } else if (tab === "osu_plays") {
    body.replaceChildren(renderOsuPlaysSettingsTab(plugin, settings));
  } else {
    body.replaceChildren(renderGamePluginSettingsGeneralTab(plugin, settings));
  }
  syncGamePluginReviewControls(plugin);
}

function showGamePluginSettingsDialog(plugin, tab = "general") {
  gamePluginSettingsDialogPluginId = plugin.id;
  const availableTabs = gamePluginSettingsTabs(plugin);
  gamePluginSettingsDialogTab = availableTabs.includes(tab) ? tab : "general";
  renderGamePluginSettingsDialog(plugin);
  const dialog = $("game-plugin-settings-dialog");
  if (!dialog.open) dialog.showModal();
}

function hideGamePluginSettingsDialog() {
  const dialog = $("game-plugin-settings-dialog");
  if (dialog.open) dialog.close();
  else gamePluginSettingsDialogPluginId = null;
}

function setGamePluginSettingsTab(tab) {
  const plugin = gamePluginSettingsDialogPlugin();
  if (!gamePluginSettingsTabs(plugin).includes(tab)) return;
  syncGamePluginSettingsDraft();
  gamePluginSettingsDialogTab = tab;
  renderGamePluginSettingsDialog();
}

function syncGamePluginCatalog(nextPlugins) {
  gamePlugins = Array.isArray(nextPlugins) ? nextPlugins : [];
  renderGamePlugins();
  if (gamePluginSettingsDialogPluginId && !gamePluginSettingsDialogPlugin()) {
    hideGamePluginSettingsDialog();
  } else if (gamePluginSettingsDialogPluginId) {
    renderGamePluginSettingsDialog();
  }
  updateGameDetectionStatus();
  if (clipsCache.length) renderClips();
  if (currentClip) {
    renderGameEventRail(currentClip);
    renderGameMetadataPanel(currentClip);
  }
}

function renderGamePlugins() {
  const root = $("supported-games");
  root.replaceChildren();
  if (!gamePlugins.length) {
    const empty = document.createElement("div");
    empty.className = "hint";
    empty.textContent = "no supported games available";
    root.appendChild(empty);
    syncSettingsChangeIndicators();
    return;
  }

  for (const plugin of gamePlugins) {
    const settings = gamePluginSetting(plugin);
    gamePluginSettings[plugin.id] = settings;

    const row = document.createElement("div");
    row.className = "game-profile supported";
    row.dataset.gamePluginId = plugin.id;
    row.dataset.settingsKey = `games.plugins.${plugin.id}`;

    const enabled = document.createElement("label");
    enabled.className = "check-line";
    const checkbox = document.createElement("input");
    checkbox.type = "checkbox";
    checkbox.checked = settings.enabled;
    checkbox.dataset.gamePluginEnabled = plugin.id;
    checkbox.addEventListener("change", () => {
      gamePluginSettings[plugin.id] = {
        ...gamePluginSetting(plugin),
        enabled: checkbox.checked,
      };
      updateGamePluginSummary(plugin);
      updateGameDetectionStatus();
      syncGamePluginSettingsDraft();
    });
    enabled.appendChild(checkbox);

    const icon = gameIconEl(plugin.icon, plugin.name);

    const meta = document.createElement("div");
    meta.className = "game-profile-meta";
    const name = document.createElement("strong");
    name.textContent = plugin.name;
    const summary = document.createElement("span");
    summary.dataset.gamePluginSummary = plugin.id;
    summary.textContent = gamePluginSummary(plugin, settings);
    meta.append(name, summary);

    row.append(
      enabled,
      icon,
      meta,
      renderGamePluginSettingsButton(plugin)
    );
    root.appendChild(row);
  }
  syncSettingsChangeIndicators();
}

function displayCaptureValue(display) {
  return `display:${display.id}`;
}

function displayForCaptureValue(value) {
  if (!String(value || "").startsWith("display:")) return null;
  const id = String(value).slice("display:".length);
  return displays.find((display) => display.id === id) || null;
}

function isFullDisplayRegion(region, display) {
  return !!region && !!display
    && region.display_id === display.id
    && Number(region.x) === display.x
    && Number(region.y) === display.y
    && Number(region.width) === display.width
    && Number(region.height) === display.height;
}

function captureSettingsValue(settings = settingsFormSource()) {
  if (settings && settings.capture_mode === "display_region") {
    const display = displays.find((item) => isFullDisplayRegion(settings.capture_region, item));
    return display ? displayCaptureValue(display) : "display_region";
  }
  const display = primaryDisplay();
  return display ? displayCaptureValue(display) : "primary_monitor";
}

function displayLabel(display) {
  const primary = display.is_primary ? " (primary)" : "";
  return `${display.name}${primary} - ${display.width}x${display.height}`;
}

function renderCaptureTargetSelect() {
  const select = $("set-capture");
  const desired = captureSettingsValue();
  select.replaceChildren();
  if (displays.length) {
    for (const display of displays) {
      const option = document.createElement("option");
      option.value = displayCaptureValue(display);
      option.textContent = displayLabel(display);
      select.appendChild(option);
    }
  } else {
    const option = document.createElement("option");
    option.value = "primary_monitor";
    option.textContent = "Primary display";
    select.appendChild(option);
  }
  const region = document.createElement("option");
  region.value = "display_region";
  region.textContent = "SET REGION";
  select.appendChild(region);
  select.value = Array.from(select.options).some((option) => option.value === desired)
    ? desired
    : captureSettingsValue({ capture_mode: "primary_monitor" });
  syncCaptureFields();
  if (settingsIndicatorBaseline) refreshSettingsBaselineIfClean();
}

function selectedCaptureSettings() {
  const display = displayForCaptureValue($("set-capture").value);
  if (display) {
    return {
      capture_mode: "display_region",
      capture_region: regionForDisplay(display),
    };
  }
  return {
    capture_mode: $("set-capture").value === "display_region" ? "display_region" : "primary_monitor",
    capture_region: regionState,
  };
}

function syncCaptureFields() {
  const display = displayForCaptureValue($("set-capture").value);
  if (display) {
    regionState = regionForDisplay(display);
  }
  const isEditableRegion = $("set-capture").value === "display_region";
  $("capture-region-editor").hidden = !isEditableRegion;
  if (isEditableRegion) renderRegionEditor();
  syncCaptureBackendSummary();
  updateCaptureStatus();
}

function syncCaptureBackendSummary() {
  const summary = $("backend-summary");
  if (!summary) return;
  if ($("set-backend").value === "desktop_duplication") {
    summary.textContent =
      "Removes the Windows 10 capture border for displays and regions. Display/region only (not single windows); the mouse cursor may be missing on some systems. Falls back to Windows Graphics Capture if unavailable.";
  } else {
    summary.textContent =
      "Windows Graphics Capture works everywhere, including single windows. On Windows 10 it may show a yellow capture border.";
  }
}

function syncRecordingFields() {
  const replay = Number($("set-replay").value);
  $("set-buffer").value = replay;
  const encoder = selectedVideoEncoder();
  const outputResolution = outputResolutionOption($("set-output-resolution").value);
  const quality = recordingQualityPreset(Number($("set-bitrate").value), outputResolution.id);
  const smoothness = smoothnessPreset(Number($("set-fps").value));
  syncRangeProgress($("set-replay"));
  syncRangeProgress($("set-bitrate"));
  syncRangeProgress($("set-fps"));
  $("replay-summary").textContent = `Save Replay writes the last ${settingDurationLabel(replay)}.`;
  $("replay-summary").className = "setting-summary";
  const encoderSummary = $("encoder-summary");
  if (encoder.id === "auto") {
    encoderSummary.textContent =
      "Clipline records H.264 when available for broad playback compatibility.";
    encoderSummary.classList.remove("warn");
  } else {
    const caveat = PlayerCore.encoderCodecCaveat(encoder.codec, decodableCodecs);
    encoderSummary.textContent = caveat || `${encoder.name} is used for new recordings.`;
    encoderSummary.classList.toggle("warn", Boolean(caveat));
  }
  $("output-resolution-summary").textContent =
    outputResolution.id === "source"
      ? "Uses the captured size, capped only when needed for encoder compatibility."
      : `${outputResolution.label} output, ${outputResolution.hint}.`;
  $("quality-summary").textContent = recordingQualitySummary(quality);
  $("fps-summary").textContent = `${smoothness.label} - ${smoothness.hint}.`;
  syncRecordingModeFields();
  syncReplayStorageFields();
}

function syncRecordingModeFields() {
  const advanced = isAdvancedRecordingMode();
  $("recording-basic-fields").hidden = advanced;
  $("recording-advanced-fields").hidden = !advanced;
  for (const id of ["set-output-resolution", "set-bitrate", "set-fps"]) {
    $(id).disabled = advanced;
  }
  for (const id of ["set-output-width", "set-output-height", "set-custom-bitrate", "set-custom-fps"]) {
    $(id).disabled = !advanced;
  }
}

function syncReplayStorageFields() {
  const enabled = $("set-replay-disk-enabled").checked;
  const fields = $("replay-disk-fields");
  fields.hidden = !enabled;
  const bitrate = currentRecordingBitrateMbps();
  const gbPerHour = bitrate * 1_000_000 / 8 * 3600 / (1000 ** 3);
  $("replay-disk-estimate").textContent =
    `${bitrate} Mbps: about ${gbPerHour.toFixed(bitrate >= 40 ? 0 : 1)} GB/hour written while recording.`;
  for (const id of ["set-replay-disk-dir", "choose-replay-cache-folder", "set-replay-disk-quota", "set-replay-disk-ack"]) {
    $(id).disabled = !enabled;
  }
}

function volumeLabel(value) {
  const pct = Math.round(Math.max(0, Math.min(2, Number(value) || 0)) * 100);
  return `${pct}%`;
}

function syncRangeProgress(input) {
  const min = Number(input.min || 0);
  const max = Number(input.max || 100);
  const value = Number(input.value || min);
  const pct = max > min ? ((value - min) / (max - min)) * 100 : 0;
  input.style.setProperty("--range-progress", `${Math.max(0, Math.min(100, pct)).toFixed(2)}%`);
}

function syncAllRangeProgress() {
  document.querySelectorAll("input[type='range']").forEach(syncRangeProgress);
}

function selectedDeviceId(id) {
  const value = $(id).value;
  return value ? value : null;
}

function fillDeviceSelect(id, devices, defaultLabel, selectedId) {
  const select = $(id);
  const selected = selectedId || "";
  select.replaceChildren();
  const def = document.createElement("option");
  def.value = "";
  def.textContent = defaultLabel;
  select.appendChild(def);
  for (const device of devices) {
    const opt = document.createElement("option");
    opt.value = device.id;
    opt.textContent = device.name + (device.is_default ? " (default)" : "");
    select.appendChild(opt);
  }
  if (selected && !devices.some((device) => device.id === selected)) {
    const stale = document.createElement("option");
    stale.value = selected;
    stale.textContent = "Unavailable device";
    select.appendChild(stale);
  }
  select.value = selected;
}

function renderAudioDeviceSelects() {
  const audio = settingsFormSource().audio || defaultAudioSettings();
  fillDeviceSelect("set-output-device", audioDevices.outputs, "Default output device", audio.output_device_id);
  fillDeviceSelect("set-mic-device", audioDevices.inputs, "Default microphone", audio.mic_device_id);
  if (settingsIndicatorBaseline) refreshSettingsBaselineIfClean();
}

function renderVideoEncoderSelect() {
  const select = $("set-encoder");
  const selected = settingsFormSource().video_encoder || "auto";
  select.replaceChildren();
  const automatic = document.createElement("option");
  automatic.value = "auto";
  automatic.textContent = "Automatic (recommended)";
  select.appendChild(automatic);
  for (const encoder of videoEncoders) {
    const opt = document.createElement("option");
    opt.value = encoder.id;
    const caveat = PlayerCore.encoderCodecCaveat(encoder.codec, decodableCodecs);
    opt.textContent = caveat ? `${encoder.name} (limited playback)` : encoder.name;
    select.appendChild(opt);
  }
  if (selected !== "auto" && !videoEncoders.some((encoder) => encoder.id === selected)) {
    const stale = document.createElement("option");
    stale.value = selected;
    stale.textContent = "Unavailable encoder";
    select.appendChild(stale);
  }
  select.value = selected;
}

function selectedVideoEncoder() {
  const id = $("set-encoder").value || "auto";
  if (id === "auto") return { id, name: "Automatic (recommended)" };
  return videoEncoders.find((encoder) => encoder.id === id) || { id, name: "Unavailable encoder" };
}

function syncAudioFields() {
  const outputEnabled = $("set-output-enabled").checked;
  $("set-output-device").disabled = !outputEnabled;
  $("set-output-volume").disabled = !outputEnabled;
  $("set-audio-split-output").disabled = !outputEnabled;
  const testingHere = micTestRunning && micTestSurface === "settings";
  $("set-mic-device").disabled = testingHere;
  $("set-mic-volume").disabled = testingHere;
  $("set-mic-mono").disabled = testingHere;
  $("test-mic").disabled = false;
  $("test-mic").textContent = testingHere ? "Stop testing" : "Test mic";
  syncRangeProgress($("set-output-volume"));
  syncRangeProgress($("set-mic-volume"));
  $("output-volume-summary").textContent = volumeLabel($("set-output-volume").value);
  $("mic-volume-summary").textContent = volumeLabel($("set-mic-volume").value);
  if (typeof syncFirstRunAudioFields === "function") syncFirstRunAudioFields();
}

function setMicTestStatus(message, level = 0) {
  const firstRun = micTestSurface === "first-run";
  const status = $(firstRun ? "first-run-mic-test-status" : "mic-test-status");
  const meter = $(firstRun ? "first-run-mic-meter-fill" : "mic-meter-fill");
  if (status) status.textContent = message;
  if (meter) meter.style.width = `${Math.round(Math.max(0, Math.min(1, level)) * 100)}%`;
}

function micMeterLevel(result) {
  const peak = Math.max(0, Number(result.peak) || 0);
  const rms = Math.max(0, Number(result.rms) || 0);
  return Math.min(1, Math.sqrt(Math.max(peak, rms * 3)));
}

function ensureMicAudioContext() {
  const AudioContextCtor = window.AudioContext || window.webkitAudioContext;
  if (!AudioContextCtor) throw new Error("Web Audio is unavailable");
  if (!micAudioContext || micAudioContext.state === "closed") {
    micAudioContext = new AudioContextCtor({ sampleRate: 48000 });
  }
  return micAudioContext;
}

async function startMicPlayback() {
  const ctx = ensureMicAudioContext();
  if (ctx.state === "suspended") await ctx.resume();
  micAudioCursor = ctx.currentTime + 0.04;
}

function stopMicPlayback() {
  for (const source of micAudioSources) {
    try {
      source.stop();
    } catch (_) {
      // Already ended.
    }
  }
  micAudioSources = [];
  micAudioCursor = 0;
}

function playMicSamples(samples) {
  if (!micTestRunning || !samples || samples.length < 2) return;
  const ctx = ensureMicAudioContext();
  const frames = Math.floor(samples.length / 2);
  const buffer = ctx.createBuffer(2, frames, 48000);
  const left = buffer.getChannelData(0);
  const right = buffer.getChannelData(1);
  for (let i = 0; i < frames; i += 1) {
    left[i] = Math.max(-1, Math.min(1, samples[i * 2] / 32768));
    right[i] = Math.max(-1, Math.min(1, samples[i * 2 + 1] / 32768));
  }
  const source = ctx.createBufferSource();
  source.buffer = buffer;
  source.connect(ctx.destination);
  const nextStart = ctx.currentTime + MIC_MONITOR_START_DELAY_S;
  if (
    !micAudioCursor ||
    micAudioCursor < nextStart ||
    micAudioCursor - ctx.currentTime > MIC_MONITOR_MAX_LATENCY_S
  ) {
    micAudioCursor = nextStart;
  }
  const startAt = micAudioCursor;
  source.start(startAt);
  micAudioCursor = startAt + buffer.duration;
  micAudioSources.push(source);
  source.onended = () => {
    micAudioSources = micAudioSources.filter((item) => item !== source);
  };
}

function stopMicTestUi(message = "stopped") {
  micTestRunning = false;
  stopMicPlayback();
  const context = micAudioContext;
  micAudioContext = null;
  if (context && context.state !== "closed") {
    context.close().catch(() => {});
  }
  syncAudioFields();
  setMicTestStatus(message, 0);
}

async function testMic(surface = "settings") {
  surface = surface === "first-run" ? "first-run" : "settings";
  const error = $(surface === "first-run" ? "first-run-error" : "error");
  error.textContent = "";
  if (micTestRunning) {
    try {
      await invoke("stop_microphone_test");
    } catch (e) {
      error.textContent = e;
    }
    stopMicTestUi("stopped");
    return;
  }

  const lifecycleWork = captureForegroundWork();
  if (!lifecycleWork) return;
  micTestSurface = surface;
  micTestRunning = true;
  syncAudioFields();
  setMicTestStatus("listening", 0);
  try {
    await startMicPlayback();
    if (!isForegroundWorkCurrent(lifecycleWork)) {
      stopMicTestUi("stopped");
      return;
    }
    await invoke("start_microphone_test", {
      deviceId: selectedDeviceId(surface === "first-run" ? "first-run-mic-device" : "set-mic-device"),
      volume: Number($(surface === "first-run" ? "first-run-mic-volume" : "set-mic-volume").value),
      mono: $(surface === "first-run" ? "first-run-mic-mono" : "set-mic-mono").checked,
    });
    if (!isForegroundWorkCurrent(lifecycleWork)) {
      await invoke("stop_microphone_test").catch(() => {});
      stopMicTestUi("stopped");
    }
  } catch (e) {
    if (!isForegroundWorkCurrent(lifecycleWork)) {
      stopMicTestUi("stopped");
      return;
    }
    stopMicTestUi("error");
    error.textContent = e;
  }
}

function updateCaptureStatus() {
  const source =
    activeDetectedGame && activeDetectedGame.active
      ? `Game: ${activeDetectedGame.name}`
      : fallbackCaptureSourceLabel(currentSettings || { capture_mode: "primary_monitor" });
  const bufferReadyTitle = activeEncoderLabel
    ? `Replay buffer ready · ${activeEncoderLabel}`
    : "Replay buffer ready";
  renderRailGame();
  $("rail-status").classList.toggle("stopped", !fullSessionRecordingActive || storageQuotaBlocked);
  $("rail-status").classList.toggle("blocked", storageQuotaBlocked);
  $("rail-status").setAttribute("aria-pressed", String(fullSessionRecordingActive));
  $("rail-status").setAttribute("aria-disabled", String(storageQuotaBlocked));
  $("rail-status").title = storageQuotaBlocked
    ? "Recording disabled — storage quota full"
    : fullSessionRecordingActive
      ? "Stop recording"
      : `Start ${source} recording`;
  $("rail-status-text").textContent = storageQuotaBlocked
    ? "Full"
    : fullSessionRecordingActive
      ? "Rec"
      : "Record";
  $("rail-dot").className = `dot${fullSessionRecordingActive ? " on" : ""}`;

  $("rail-game").classList.toggle("active", recordingActive && !storageQuotaBlocked);
  $("rail-game").classList.toggle("stopped", !recordingRequested || storageQuotaBlocked);
  $("rail-game").classList.toggle("waiting", recorderWaitingForGame);
  $("rail-game").classList.toggle("blocked", storageQuotaBlocked);
  $("rail-game").setAttribute("aria-pressed", String(recordingRequested));
  $("rail-game").setAttribute("aria-disabled", String(storageQuotaBlocked));
  $("rail-game").title = storageQuotaBlocked
    ? "Replay buffer disabled — storage quota full"
    : recordingActive
      ? bufferReadyTitle
      : recorderWaitingForGame
        ? "Stop waiting for a game"
        : `Start ${source} replay buffer`;
  $("rail-game").setAttribute("aria-label", $("rail-game").title);
  $("rail-save").disabled = storageQuotaBlocked || !recordingActive;
}

function saveHotkeyLabel() {
  return (currentSettings && currentSettings.hotkey) || $("set-hotkey").value || "F6";
}

function saveSecondaryHotkeyLabel() {
  return (currentSettings && currentSettings.hotkey_secondary) || "";
}

function updateHotkeyLabels(hotkey = saveHotkeyLabel(), secondary = saveSecondaryHotkeyLabel()) {
  const label = String(hotkey || "F6");
  // The rail stays compact with the primary keybind; tooltips list both.
  const full = secondary ? `${label} / ${secondary}` : label;
  $("rail-hotkey").textContent = label;
  $("rail-hotkey").title = `Save Replay: ${full}`;
  $("rail-save").title = `Save Replay (${full})`;
}

function fallbackCaptureSourceLabel(settings) {
  if (settings && settings.capture_mode === "display_region") {
    const display = displays.find((item) => isFullDisplayRegion(settings.capture_region, item));
    if (display) return `Display: ${display.name}`;
  }
  return captureSourceLabel(settings);
}


async function toggleRecording() {
  if (storageQuotaBlocked) {
    showStorageQuotaFull(storageQuotaState);
    return;
  }
  const next = !recordingRequested;
  $("rail-game").disabled = true;
  try {
    recordingRequested = await invoke("set_recording", { recording: next });
    if (!recordingRequested) {
      recordingActive = false;
      recorderWaitingForGame = false;
    }
    updateCaptureStatus();
  } catch (e) {
    $("error").textContent = e;
  } finally {
    $("rail-game").disabled = false;
  }
}

async function toggleSessionRecording() {
  if (storageQuotaBlocked) {
    showStorageQuotaFull(storageQuotaState);
    return;
  }
  const next = !fullSessionRecordingActive;
  $("rail-status").disabled = true;
  try {
    const requested = await invoke("set_session_recording", { recording: next });
    fullSessionRecordingActive = Boolean(requested);
    updateCaptureStatus();
  } catch (e) {
    $("error").textContent = e;
  } finally {
    $("rail-status").disabled = false;
  }
}

// All shortcut fields share capture and conflict handling. Esc clears a field;
// at least one Save Replay keybind must remain, while recording is optional.
const HOTKEY_FIELD_IDS = [
  "set-hotkey",
  "set-hotkey-2",
  "set-recording-hotkey",
  "set-recording-hotkey-2",
  "set-bookmark-hotkey",
  "set-bookmark-hotkey-2",
];
const HOTKEY_IDLE_MESSAGE = "Click a field to record a shortcut. Esc clears it.";

function hotkeyStatusId(fieldId) {
  if (fieldId.startsWith("set-recording-hotkey")) return "recording-hotkey-status";
  if (fieldId.startsWith("set-bookmark-hotkey")) return "bookmark-hotkey-status";
  return "hotkey-status";
}

function setHotkeyStatus(fieldId, message, state = "") {
  const status = $(hotkeyStatusId(fieldId));
  status.textContent = message;
  status.dataset.state = state;
}

function beginHotkeyCapture(fieldId) {
  if (activeHotkeyCaptureId && activeHotkeyCaptureId !== fieldId) {
    endHotkeyCapture(activeHotkeyCaptureId);
  }
  activeHotkeyCaptureId = fieldId;
  $(fieldId).classList.add("recording");
  setHotkeyStatus(fieldId, "Press an F-key, mouse button, or Ctrl/Alt/Shift plus a keyboard key - or Esc to clear.", "recording");
}

function endHotkeyCapture(fieldId, message = HOTKEY_IDLE_MESSAGE, state = "") {
  if (activeHotkeyCaptureId === fieldId) activeHotkeyCaptureId = null;
  $(fieldId).classList.remove("recording");
  setHotkeyStatus(fieldId, message, state);
}

function endAllHotkeyCaptures() {
  HOTKEY_FIELD_IDS.forEach((fieldId) => endHotkeyCapture(fieldId));
}

function recordHotkey(fieldId, ev) {
  if (activeHotkeyCaptureId !== fieldId) beginHotkeyCapture(fieldId);
  ev.preventDefault();
  ev.stopPropagation();

  applyHotkeyCaptureResult(fieldId, hotkeyFromKeyEvent(ev));
}

function recordMouseHotkey(fieldId, ev) {
  if (ev.button === 0) return;
  if (activeHotkeyCaptureId !== fieldId) beginHotkeyCapture(fieldId);
  ev.preventDefault();
  ev.stopPropagation();

  applyHotkeyCaptureResult(fieldId, hotkeyFromMouseEvent(ev));
}

function applyHotkeyCaptureResult(fieldId, result) {
  switch (result.kind) {
    case "captured":
      if (HOTKEY_FIELD_IDS.some((other) => other !== fieldId && $(other).value === result.value)) {
        setHotkeyStatus(fieldId, "Already used by another Clipline action.", "error");
        break;
      }
      $(fieldId).value = result.value;
      endHotkeyCapture(fieldId, "Ready to save.", "ready");
      syncSettingsDraftFromForm();
      break;
    case "pending":
      setHotkeyStatus(fieldId, result.message, "recording");
      break;
    case "cancel":
      // Esc clears the keybind; the other field can still hold one.
      $(fieldId).value = "";
      endHotkeyCapture(fieldId, "Keybind cleared. Ready to save.", "ready");
      syncSettingsDraftFromForm();
      $(fieldId).blur();
      break;
    case "invalid":
      setHotkeyStatus(fieldId, result.message, "error");
      break;
  }
}

function primaryDisplay() {
  return displays.find((d) => d.is_primary) || displays[0] || null;
}

function activeDisplay() {
  return displays.find((d) => d.id === regionState.display_id) || primaryDisplay();
}

function menuDisplay() {
  return displays.find((d) => d.id === regionMenuDisplayId) || activeDisplay();
}

function setRegion(next) {
  const display = displays.find((d) => d.id === next.display_id) || activeDisplay();
  regionState = display
    ? clampRegionToDisplay({ ...next, display_id: display.id }, display)
    : {
        display_id: next.display_id ?? null,
        x: Math.round(next.x || 0),
        y: Math.round(next.y || 0),
        width: Math.max(2, Math.round(next.width || 2)),
        height: Math.max(2, Math.round(next.height || 2)),
      };
  renderRegionEditor();
  if (typeof settingsOpen !== "undefined" && settingsOpen) syncSettingsDraftFromForm();
}

async function loadDisplays(lifecycleWork = captureForegroundWork()) {
  if (!lifecycleWork) return false;
  try {
    const nextDisplays = await invoke("list_displays");
    if (!isForegroundWorkCurrent(lifecycleWork)) return false;
    displays = nextDisplays;
    displaysLoaded = true;
    if (!regionState.display_id && displays.length) {
      regionState = regionForDisplay(primaryDisplay());
    }
    renderCaptureTargetSelect();
    renderRegionEditor();
    return true;
  } catch (e) {
    if (!isForegroundWorkCurrent(lifecycleWork)) return false;
    $("region-display-label").textContent = "display list unavailable";
    $("error").textContent = e;
    return true;
  }
}

async function ensureDisplaysLoaded(lifecycleWork = captureForegroundWork()) {
  if (!lifecycleWork) return false;
  if (displaysLoaded) return true;
  if (!displaysLoadPromise) {
    const pending = loadDisplays(lifecycleWork).finally(() => {
      if (displaysLoadPromise === pending) displaysLoadPromise = null;
    });
    displaysLoadPromise = pending;
  }
  const completed = await displaysLoadPromise;
  if (
    !completed
    && !displaysLoaded
    && isForegroundWorkCurrent(lifecycleWork)
  ) {
    return ensureDisplaysLoaded(lifecycleWork);
  }
  return completed;
}

async function loadAudioDevices(lifecycleWork = captureForegroundWork()) {
  if (!lifecycleWork) return false;
  try {
    const nextAudioDevices = await invoke("list_audio_devices");
    if (!isForegroundWorkCurrent(lifecycleWork)) return false;
    audioDevices = nextAudioDevices;
    audioDevicesLoaded = true;
    renderAudioDeviceSelects();
    return true;
  } catch (e) {
    if (!isForegroundWorkCurrent(lifecycleWork)) return false;
    $("error").textContent = e;
    return true;
  }
}

async function ensureAudioDevicesLoaded(lifecycleWork = captureForegroundWork()) {
  if (!lifecycleWork) return false;
  if (audioDevicesLoaded) return true;
  if (!audioDevicesLoadPromise) {
    const pending = loadAudioDevices(lifecycleWork).finally(() => {
      if (audioDevicesLoadPromise === pending) audioDevicesLoadPromise = null;
    });
    audioDevicesLoadPromise = pending;
  }
  const completed = await audioDevicesLoadPromise;
  if (
    !completed
    && !audioDevicesLoaded
    && isForegroundWorkCurrent(lifecycleWork)
  ) {
    return ensureAudioDevicesLoaded(lifecycleWork);
  }
  return completed;
}

// Probe which codecs this WebView2 can actually decode and report them so
// Automatic recording never produces a clip the review player can't show.
function probeDecodableCodecs() {
  const probe = document.createElement("video");
  const supported = ["h264"];
  for (const { codec, mime } of PlayerCore.videoDecodeProbes()) {
    const verdict = probe.canPlayType(mime);
    if (verdict === "probably" || verdict === "maybe") supported.push(codec);
  }
  decodableCodecs = supported;
}

var ffmpegRuntimeSnapshot = null;
var ffmpegRuntimeInstallPromise = null;
var ffmpegRuntimeRetry = null;

function ffmpegInstallPhase(snapshot = ffmpegRuntimeSnapshot) {
  return String(snapshot?.state?.phase || "idle");
}

function ffmpegRuntimeUnavailable(error) {
  return String(error || "").toLowerCase().includes("ffmpeg is not available");
}

function applyFfmpegInstallSnapshot(snapshot) {
  if (!snapshot) return;
  ffmpegRuntimeSnapshot = snapshot;
  const phase = ffmpegInstallPhase(snapshot);
  const state = snapshot.state || {};
  const managed = snapshot.discovery === "managed_verified";
  const active = ["checking", "downloading", "verifying", "publishing"].includes(phase);
  const cancellable = ["checking", "downloading", "verifying"].includes(phase);
  const total = Number(state.total) || 0;
  const bytes = Number(state.bytes) || 0;
  const percent = total > 0 ? Math.min(100, Math.round((bytes / total) * 100)) : 0;
  const status = $("ffmpeg-runtime-status");
  const progress = $("ffmpeg-runtime-progress");
  const install = $("ffmpeg-runtime-install");
  const cancel = $("ffmpeg-runtime-cancel");
  const posterInstall = $("poster-runtime-install");

  if (phase === "checking") status.textContent = "Checking...";
  else if (phase === "downloading") status.textContent = `Downloading... ${percent}%`;
  else if (phase === "verifying") status.textContent = "Verifying downloaded files...";
  else if (phase === "publishing") status.textContent = "Finishing installation...";
  else if (phase === "failed") status.textContent = `Install failed: ${state.message || "unknown error"}`;
  else if (phase === "cancelled") status.textContent = "Install cancelled";
  else if (managed) status.textContent = "Installed and verified";
  else if (snapshot.discovery === "external_unmanaged") {
    status.textContent = "External FFmpeg found; managed component not installed";
  } else status.textContent = "Not installed";

  progress.hidden = phase !== "downloading";
  progress.value = percent;
  install.hidden = active || managed;
  install.textContent = phase === "failed" || phase === "cancelled"
    ? "Retry Install / Repair"
    : "Install / Repair";
  cancel.hidden = !cancellable;
  posterInstall.disabled = active;
  posterInstall.textContent = active
    ? phase === "downloading" ? `Installing... ${percent}%` : "Installing..."
    : "Install FFmpeg";
}

async function queryFfmpegRuntimeStatus() {
  const snapshot = await invoke("ffmpeg_runtime_status");
  applyFfmpegInstallSnapshot(snapshot);
  return snapshot;
}

async function ensureFfmpegRuntime(retry = null) {
  if (typeof retry === "function") ffmpegRuntimeRetry = retry;
  if (ffmpegRuntimeInstallPromise) return ffmpegRuntimeInstallPromise;

  ffmpegRuntimeInstallPromise = invoke("ensure_ffmpeg_runtime")
    .then((snapshot) => {
      applyFfmpegInstallSnapshot(snapshot);
      if (snapshot.discovery === "managed_verified") {
        const pendingRetry = ffmpegRuntimeRetry;
        ffmpegRuntimeRetry = null;
        if (pendingRetry) return Promise.resolve(pendingRetry()).then(() => snapshot);
      }
      return snapshot;
    })
    .catch(async (error) => {
      ffmpegRuntimeRetry = null;
      try {
        await queryFfmpegRuntimeStatus();
      } catch (_) {
        // Keep the original install error.
      }
      if (ffmpegInstallPhase() !== "cancelled") $("error").textContent = String(error);
      throw error;
    })
    .finally(() => {
      ffmpegRuntimeInstallPromise = null;
    });
  return ffmpegRuntimeInstallPromise;
}

async function cancelFfmpegRuntimeInstall() {
  $("ffmpeg-runtime-cancel").disabled = true;
  $("ffmpeg-runtime-status").textContent = "Cancelling...";
  try {
    const snapshot = await invoke("cancel_ffmpeg_runtime_install");
    applyFfmpegInstallSnapshot(snapshot);
    if (["checking", "downloading", "verifying"].includes(ffmpegInstallPhase(snapshot))) {
      $("ffmpeg-runtime-status").textContent = "Cancelling...";
    }
  } finally {
    $("ffmpeg-runtime-cancel").disabled = false;
  }
}

function installFfmpegForPosters() {
  return ensureFfmpegRuntime(retryUnavailablePosters);
}

async function loadVideoEncoders(lifecycleWork = captureForegroundWork()) {
  if (!lifecycleWork) return false;
  probeDecodableCodecs();
  try {
    await invoke("report_decode_support", { codecs: decodableCodecs });
  } catch (e) {
    // Reporting is best-effort; the recorder defaults to H.264-safe Automatic.
  }
  if (!isForegroundWorkCurrent(lifecycleWork)) return false;
  try {
    const nextVideoEncoders = await invoke("probe_encoders");
    if (!isForegroundWorkCurrent(lifecycleWork)) return false;
    videoEncoders = nextVideoEncoders;
    videoEncodersLoaded = true;
  } catch (e) {
    if (!isForegroundWorkCurrent(lifecycleWork)) return false;
    videoEncoders = [];
    $("error").textContent = e;
  }
  renderVideoEncoderSelect();
  if (currentSettings) syncRecordingFields();
  return true;
}

async function ensureVideoEncodersLoaded(lifecycleWork = captureForegroundWork()) {
  if (!lifecycleWork) return false;
  if (videoEncodersLoaded) return true;
  if (!videoEncodersLoadPromise) {
    const pending = loadVideoEncoders(lifecycleWork).finally(() => {
      if (videoEncodersLoadPromise === pending) videoEncodersLoadPromise = null;
    });
    videoEncodersLoadPromise = pending;
  }
  const completed = await videoEncodersLoadPromise;
  if (
    !completed
    && !videoEncodersLoaded
    && isForegroundWorkCurrent(lifecycleWork)
  ) {
    return ensureVideoEncodersLoaded(lifecycleWork);
  }
  return completed;
}

async function loadGamePlugins(lifecycleWork = captureForegroundWork()) {
  if (!lifecycleWork) {
    requestWindowRefresh();
    return false;
  }
  try {
    const plugins = await invoke("list_game_plugins");
    if (!isForegroundWorkCurrent(lifecycleWork)) return false;
    syncGamePluginCatalog(plugins);
    return true;
  } catch (e) {
    if (!isForegroundWorkCurrent(lifecycleWork)) return false;
    gamePlugins = [];
    $("error").textContent = e;
    renderGamePlugins();
    return true;
  }
}

function gameNameFromWindow(win) {
  const exe = String(win.exe_name || "").replace(/\.exe$/i, "");
  return exe || String(win.title || "Custom game").trim() || "Custom game";
}

function customGameId(name) {
  const slug = String(name || "game")
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 28) || "game";
  return `custom-${slug}-${Date.now()}`;
}

function uniqueCustomGameId(name, usedIds = new Set(customGames.map((game) => game.id))) {
  for (const plugin of gamePlugins) usedIds.add(plugin.id);
  const baseId = customGameId(name);
  let candidateId = baseId;
  let suffix = 2;
  while (usedIds.has(candidateId)) {
    candidateId = `${baseId}-${suffix}`;
    suffix += 1;
  }
  usedIds.add(candidateId);
  return candidateId;
}

function detectedGameKey(candidate) {
  return String(candidate.id_hint || candidate.process_path || candidate.exe_name || candidate.name || "");
}

function detectedGameSourceLabel(candidate) {
  switch (candidate.source) {
    case "steam":
      return "Steam";
    case "running_window":
      return "Running";
    case "steam_and_running_window":
      return "Steam · Running";
    default:
      return "Installed";
  }
}

function detectedGameMeta(candidate) {
  const parts = [detectedGameSourceLabel(candidate)];
  if (candidate.exe_name) parts.push(candidate.exe_name);
  if (candidate.window_title) parts.push(candidate.window_title);
  if (!candidate.window_title && candidate.install_dir) parts.push(candidate.install_dir);
  if (!candidate.window_title && !candidate.install_dir && candidate.steam_app_id) {
    parts.push(`Steam app ${candidate.steam_app_id}`);
  }
  return parts.join(" · ");
}

function customGameMatchKey(game) {
  const path = String(game.process_path || "")
    .trim()
    .replaceAll("/", "\\")
    .replace(/\\+$/, "")
    .toLowerCase();
  const exe = String(game.exe_name || "").trim().toLowerCase();
  const title = String(game.window_title || "").trim().toLowerCase();
  return path || exe || title ? JSON.stringify([path, exe, title]) : "";
}

function customGameRuleMatchesCandidate(game, candidate) {
  const gameKey = customGameMatchKey(game);
  return !!gameKey && gameKey === customGameMatchKey(candidate);
}

function customGameMatchesCandidate(game, candidate) {
  const gamePath = String(game.process_path || "").toLowerCase();
  const candidatePath = String(candidate.process_path || "").toLowerCase();
  if (gamePath && candidatePath) return gamePath === candidatePath;
  if (
    game.exe_name &&
    candidate.exe_name &&
    String(game.exe_name).toLowerCase() === String(candidate.exe_name).toLowerCase()
  ) {
    return true;
  }
  const gameName = String(game.name || "").toLowerCase();
  const candidateName = String(candidate.name || "").toLowerCase();
  return !!gameName && !!candidateName && gameName === candidateName;
}

function gameRecordingModeControl(game, index) {
  const control = document.createElement("div");
  control.className = "segmented-control custom-game-mode";
  control.setAttribute("role", "radiogroup");
  control.setAttribute("aria-label", `${game.name} recording mode`);
  const selectedMode = normalizeGameRecordingMode(game.recording_mode);
  [
    ["replays_only", "Replays only"],
    ["full_session", "Full session"],
  ].forEach(([value, label]) => {
    const option = document.createElement("label");
    const input = document.createElement("input");
    input.type = "radio";
    input.name = `custom-game-recording-mode-${index}`;
    input.value = value;
    input.checked = selectedMode === value;
    input.addEventListener("change", () => {
      if (input.checked) {
        customGames[index] = { ...customGames[index], recording_mode: value };
      }
    });
    const text = document.createElement("span");
    text.textContent = label;
    option.append(input, text);
    control.appendChild(option);
  });
  return control;
}

function renderDetectedGames() {
  const root = $("detected-games-list");
  root.replaceChildren();
  const addable = detectedGameCandidates.filter(
    (candidate) => !customGames.some((game) => customGameMatchesCandidate(game, candidate)),
  );
  const addableKeys = new Set(addable.map(detectedGameKey));
  selectedDetectedGameIds = new Set([...selectedDetectedGameIds].filter((key) => addableKeys.has(key)));
  $("add-detected-games").disabled = selectedDetectedGameIds.size === 0;
  if (!addable.length) {
    const empty = document.createElement("div");
    empty.className = "hint";
    empty.textContent = "no new games found";
    root.appendChild(empty);
    return;
  }
  for (const candidate of addable) {
    const key = detectedGameKey(candidate);
    const row = document.createElement("label");
    row.className = "detected-game";

    const check = document.createElement("span");
    check.className = "check-line";
    const checkbox = document.createElement("input");
    checkbox.type = "checkbox";
    checkbox.checked = selectedDetectedGameIds.has(key);
    checkbox.addEventListener("change", () => {
      if (checkbox.checked) {
        selectedDetectedGameIds.add(key);
      } else {
        selectedDetectedGameIds.delete(key);
      }
      $("add-detected-games").disabled = selectedDetectedGameIds.size === 0;
    });
    check.appendChild(checkbox);

    const icon = gameIconEl(candidate.icon, candidate.name);
    const meta = document.createElement("div");
    meta.className = "detected-game-meta";
    const name = document.createElement("strong");
    name.textContent = candidate.name || "Detected game";
    const info = document.createElement("span");
    info.textContent = detectedGameMeta(candidate);
    meta.append(name, info);
    row.append(check, icon, meta);
    root.appendChild(row);
  }
}

function renderCustomGames() {
  const root = $("custom-games");
  root.replaceChildren();
  if (!customGames.length) {
    const empty = document.createElement("div");
    empty.className = "hint";
    empty.textContent = "no custom games saved";
    root.appendChild(empty);
    syncSettingsChangeIndicators();
    return;
  }
  customGames.forEach((game, index) => {
    const row = document.createElement("div");
    row.className = "custom-game";
    row.dataset.settingsKey = `games.custom_games.${game.id}`;

    const enabled = document.createElement("label");
    enabled.className = "check-line";
    const checkbox = document.createElement("input");
    checkbox.type = "checkbox";
    checkbox.checked = game.enabled;
    checkbox.addEventListener("change", () => {
      customGames[index] = { ...customGames[index], enabled: checkbox.checked };
    });
    enabled.appendChild(checkbox);

    const icon = gameIconEl(game.icon, game.name);

    const meta = document.createElement("div");
    meta.className = "custom-game-meta";
    const name = document.createElement("strong");
    name.textContent = game.name;
    const info = document.createElement("span");
    info.textContent =
      `${game.exe_name || "window title"} · ${game.window_title || game.process_path || "custom rule"}`;
    meta.append(name, info);

    const remove = document.createElement("button");
    remove.type = "button";
    remove.className = "custom-game-remove";
    remove.title = "Remove custom game";
    remove.textContent = "×";
    remove.addEventListener("click", () => {
      customGames.splice(index, 1);
      renderCustomGames();
      syncSettingsDraftFromForm();
    });

    row.append(enabled, icon, meta, gameRecordingModeControl(game, index), remove);
    root.appendChild(row);
  });
  syncSettingsChangeIndicators();
}

function renderGameWindows() {
  const root = $("game-window-list");
  root.replaceChildren();
  if (!gameWindows.length) {
    const empty = document.createElement("div");
    empty.className = "hint";
    empty.textContent = "no running windows found";
    root.appendChild(empty);
    return;
  }
  for (const win of gameWindows) {
    const row = document.createElement("button");
    row.type = "button";
    row.className = "game-window";
    const title = document.createElement("strong");
    title.textContent = win.title;
    const meta = document.createElement("span");
    meta.textContent =
      `${win.exe_name || "unknown process"} · PID ${win.process_id}` +
      (win.exe_path ? ` · ${win.exe_path}` : "");
    row.append(title, meta);
    row.addEventListener("click", () => addCustomGameFromWindow(win));
    root.appendChild(row);
  }
}

async function refreshGameWindows() {
  const lifecycleWork = captureForegroundWork();
  if (!lifecycleWork) return false;
  const scanId = ++gameWindowsScanId;
  $("error").textContent = "";
  $("game-window-list").replaceChildren();
  const loading = document.createElement("div");
  loading.className = "hint";
  loading.textContent = "scanning running windows…";
  $("game-window-list").appendChild(loading);
  try {
    const windows = await invoke("list_game_windows");
    if (
      !isForegroundWorkCurrent(lifecycleWork)
      || scanId !== gameWindowsScanId
      || !$("game-window-picker-dialog").open
    ) return false;
    gameWindows = windows;
    renderGameWindows();
    return true;
  } catch (e) {
    if (
      !isForegroundWorkCurrent(lifecycleWork)
      || scanId !== gameWindowsScanId
      || !$("game-window-picker-dialog").open
    ) return false;
    $("error").textContent = e;
    gameWindows = [];
    renderGameWindows();
    return true;
  }
}

async function showDetectedGamesDialog() {
  const lifecycleWork = captureForegroundWork();
  if (!lifecycleWork) return;
  $("error").textContent = "";
  if (!$("detected-games-dialog").open) $("detected-games-dialog").showModal();
  const scanId = ++detectedGamesScanId;
  selectedDetectedGameIds = new Set();
  detectedGameCandidates = [];
  $("add-detected-games").disabled = true;
  $("detected-games-list").replaceChildren();
  const loading = document.createElement("div");
  loading.className = "hint";
  loading.textContent = "scanning installed games...";
  $("detected-games-list").appendChild(loading);
  try {
    const candidates = await invoke("detect_installed_games", { existingCustomGames: customGames });
    if (
      !isForegroundWorkCurrent(lifecycleWork)
      || scanId !== detectedGamesScanId
      || !$("detected-games-dialog").open
    ) return;
    detectedGameCandidates = candidates;
    renderDetectedGames();
  } catch (e) {
    if (
      !isForegroundWorkCurrent(lifecycleWork)
      || scanId !== detectedGamesScanId
      || !$("detected-games-dialog").open
    ) return;
    $("error").textContent = e;
    detectedGameCandidates = [];
    renderDetectedGames();
  }
}

function resetDetectedGamesDialog() {
  detectedGamesScanId += 1;
  detectedGameCandidates = [];
  selectedDetectedGameIds = new Set();
  $("add-detected-games").disabled = true;
  $("detected-games-list").replaceChildren();
}

function hideDetectedGamesDialog() {
  if ($("detected-games-dialog").open) {
    $("detected-games-dialog").close();
  } else {
    resetDetectedGamesDialog();
  }
}

function customGameFromDetectedCandidate(candidate, usedIds) {
  const name = candidate.name || "Detected game";
  return normalizeCustomGame({
    id: uniqueCustomGameId(name, usedIds),
    name,
    enabled: true,
    exe_name: candidate.exe_name || "",
    process_path: candidate.process_path || null,
    window_title: candidate.window_title || "",
    recording_mode: "replays_only",
    icon: candidate.icon || null,
  });
}

function addSelectedDetectedGames() {
  const selected = detectedGameCandidates.filter((candidate) =>
    selectedDetectedGameIds.has(detectedGameKey(candidate)),
  );
  const usedIds = new Set(customGames.map((game) => game.id));
  const additions = selected
    .filter((candidate) => !customGames.some((game) => customGameMatchesCandidate(game, candidate)))
    .map((candidate) => customGameFromDetectedCandidate(candidate, usedIds));
  if (!additions.length) {
    renderDetectedGames();
    return;
  }
  customGames.push(...additions);
  hideDetectedGamesDialog();
  renderCustomGames();
  updateGameDetectionStatus();
  syncSettingsDraftFromForm();
  $("settings-status").textContent =
    additions.length === 1
      ? "custom game added - save to apply"
      : `${additions.length} custom games added - save to apply`;
}

async function showGameWindowPicker() {
  if (!$("game-window-picker-dialog").open) $("game-window-picker-dialog").showModal();
  await refreshGameWindows();
}

function hideGameWindowPicker() {
  gameWindowsScanId += 1;
  gameWindows = [];
  $("game-window-list").replaceChildren();
  if ($("game-window-picker-dialog").open) $("game-window-picker-dialog").close();
}

async function addCustomGameFromWindow(win) {
  const lifecycleWork = captureForegroundWork();
  if (!lifecycleWork) return;
  const scanId = gameWindowsScanId;
  const name = gameNameFromWindow(win);
  if (customGames.some((game) => customGameRuleMatchesCandidate(game, {
    name,
    exe_name: win.exe_name || "",
    process_path: win.exe_path || null,
    window_title: win.title || "",
  }))) {
    hideGameWindowPicker();
    $("settings-status").textContent = "game is already added";
    return;
  }
  // Pull the executable's icon now, while we still have its path. Best-effort:
  // a missing path or icon just leaves the game with the placeholder glyph.
  let icon = null;
  if (win.exe_path) {
    try {
      icon = await invoke("extract_window_icon", { processId: win.process_id });
    } catch (e) {
      icon = null;
    }
  }
  if (
    !isForegroundWorkCurrent(lifecycleWork)
    || scanId !== gameWindowsScanId
    || !$("game-window-picker-dialog").open
  ) return;
  customGames.push(normalizeCustomGame({
    id: customGameId(name),
    name,
    enabled: true,
    exe_name: win.exe_name || "",
    process_path: win.exe_path || null,
    window_title: win.title || "",
    recording_mode: "replays_only",
    icon,
  }));
  hideGameWindowPicker();
  renderCustomGames();
  syncSettingsDraftFromForm();
  $("settings-status").textContent = "custom game added - save to apply";
}

function releaseBackgroundSettingsUi() {
  gameWindowsScanId += 1;
  gameWindows = [];
  $("game-window-list").replaceChildren();
  if ($("game-window-picker-dialog").open) {
    $("game-window-picker-dialog").close();
  }

  detectedGamesScanId += 1;
  detectedGameCandidates = [];
  selectedDetectedGameIds = new Set();
  $("detected-games-list").replaceChildren();
  $("add-detected-games").disabled = true;
  if ($("detected-games-dialog").open) {
    $("detected-games-dialog").close();
  }
}

function updateGameDetectionStatus() {
  const detectionEnabled = $("set-games-auto-detect").checked;
  $("set-games-pause-when-empty").disabled = !detectionEnabled;
  if (activeDetectedGame && activeDetectedGame.active) {
    $("game-detection-status").textContent =
      `Active: ${activeDetectedGame.name} · ${activeDetectedGame.window_title}`;
  } else {
    if (!detectionEnabled) {
      $("game-detection-status").textContent = "Game detection is off.";
      return;
    }
    const enabledPlugins = gamePlugins.filter((plugin) => gamePluginSetting(plugin).enabled);
    if (enabledPlugins.length) {
      const names = enabledPlugins.map((plugin) => plugin.name).join(", ");
      $("game-detection-status").textContent = `Waiting for: ${names}.`;
    } else if (customGames.length) {
      $("game-detection-status").textContent = "No saved custom game is active.";
    } else {
      $("game-detection-status").textContent = "Enable a supported game or add a running game window, then save.";
    }
  }
}

function updateRegionFields() {
  $("set-region-width").value = regionState.width;
  $("set-region-height").value = regionState.height;
  $("set-region-x").value = regionState.x;
  $("set-region-y").value = regionState.y;
  const display = activeDisplay();
  $("region-display-label").textContent = display
    ? `${display.name} · ${display.width}x${display.height} at ${display.x}, ${display.y}`
    : "no displays";
  $("region-size-label").textContent = `${regionState.width}x${regionState.height}`;
}

function renderDisplayMenu() {
  const menu = $("region-display-menu");
  menu.replaceChildren();
  for (const display of displays) {
    const item = document.createElement("button");
    item.type = "button";
    item.textContent = display.name + (display.is_primary ? " (primary)" : "");
    item.addEventListener("click", () => {
      hideRegionMenu();
      setRegion(regionForDisplay(display));
    });
    menu.appendChild(item);
  }
}

function renderRegionEditor() {
  const editor = $("capture-region-editor");
  if (editor.hidden) return;
  const map = $("display-map");
  const inner = $("display-map-inner");
  const box = $("region-box");
  inner.querySelectorAll(".display-tile").forEach((node) => node.remove());
  if (!displays.length) {
    updateRegionFields();
    box.hidden = true;
    return;
  }
  const display = activeDisplay();
  if (display) {
    regionState = clampRegionToDisplay(regionState, display);
  }
  const mapWidth = Math.max(320, map.clientWidth);
  const mapHeight = displayMapHeight(displays, mapWidth, 10);
  map.style.height = `${mapHeight}px`;
  regionLayout = displayMapLayout(displays, mapWidth, mapHeight, 10);
  inner.style.width = "100%";
  inner.style.height = "100%";

  for (const item of regionLayout.displays) {
    const displayInfo = displays.find((d) => d.id === item.id);
    const tile = document.createElement("button");
    tile.type = "button";
    tile.className =
      "display-tile" +
      (displayInfo && displayInfo.is_primary ? " primary" : "") +
      (displayInfo && displayInfo.id === regionState.display_id ? " active" : "");
    tile.style.left = `${item.left}px`;
    tile.style.top = `${item.top}px`;
    tile.style.width = `${item.width}px`;
    tile.style.height = `${item.height}px`;
    tile.addEventListener("click", () => {
      if (displayInfo) setRegion({ ...regionState, display_id: displayInfo.id });
    });
    tile.addEventListener("contextmenu", (ev) => showRegionMenu(ev, displayInfo && displayInfo.id));
    const label = document.createElement("span");
    label.textContent = displayInfo ? displayInfo.name : item.id;
    tile.appendChild(label);
    inner.insertBefore(tile, box);
  }

  const bounds = regionLayout.bounds;
  const scale = regionLayout.scale;
  box.hidden = false;
  box.style.left = `${10 + (regionState.x - bounds.x) * scale}px`;
  box.style.top = `${10 + (regionState.y - bounds.y) * scale}px`;
  box.style.width = `${regionState.width * scale}px`;
  box.style.height = `${regionState.height * scale}px`;
  updateRegionFields();
  renderDisplayMenu();
}

function regionFromFields() {
  return {
    display_id: regionState.display_id,
    x: Number($("set-region-x").value),
    y: Number($("set-region-y").value),
    width: Number($("set-region-width").value),
    height: Number($("set-region-height").value),
  };
}

function startRegionDrag(kind, ev) {
  if (!regionLayout || !activeDisplay()) return;
  regionDrag = {
    kind,
    startX: ev.clientX,
    startY: ev.clientY,
    region: { ...regionState },
  };
  $("region-box").setPointerCapture(ev.pointerId);
  ev.preventDefault();
  ev.stopPropagation();
}

function moveRegionDrag(ev) {
  if (!regionDrag || !regionLayout) return;
  const dx = Math.round((ev.clientX - regionDrag.startX) / regionLayout.scale);
  const dy = Math.round((ev.clientY - regionDrag.startY) / regionLayout.scale);
  const base = regionDrag.region;
  if (regionDrag.kind === "resize") {
    setRegion({
      ...base,
      width: base.width + dx,
      height: base.height + dy,
    });
  } else {
    setRegion({
      ...base,
      x: base.x + dx,
      y: base.y + dy,
    });
  }
}

function endRegionDrag() {
  regionDrag = null;
}

function showRegionMenu(ev, displayId = null) {
  ev.preventDefault();
  ev.stopPropagation();
  hideClipContextMenu();
  regionMenuDisplayId = displayId || (activeDisplay() && activeDisplay().id);
  renderDisplayMenu();
  const menu = $("capture-region-menu");
  menu.hidden = false;
  positionContextMenu(menu, ev.clientX, ev.clientY);
}

function hideRegionMenu() {
  $("capture-region-menu").hidden = true;
  regionMenuDisplayId = null;
}

function positionContextMenu(menu, x, y) {
  menu.style.left = "0px";
  menu.style.top = "0px";
  const width = menu.offsetWidth || 160;
  const height = menu.offsetHeight || 80;
  const left = Math.min(Math.max(6, x), Math.max(6, window.innerWidth - width - 6));
  const top = Math.min(Math.max(6, y), Math.max(6, window.innerHeight - height - 6));
  menu.style.left = `${left}px`;
  menu.style.top = `${top}px`;
}
