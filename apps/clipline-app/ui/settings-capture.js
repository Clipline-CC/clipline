// Settings: capture targets, recording/audio fields, hotkeys, devices.
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
    $(activeHotkeyCaptureId).classList.remove("recording");
    setHotkeyStatus(activeHotkeyCaptureId, HOTKEY_IDLE_MESSAGE);
  }
  activeHotkeyCaptureId = fieldId;
  $(fieldId).classList.add("recording");
  setHotkeyStatus(fieldId, "Press an F-key, mouse button, or Ctrl/Alt/Shift plus a keyboard key - or Esc to clear.", "recording");
  syncHotkeyCapturePause();
}

function endHotkeyCapture(fieldId, message = HOTKEY_IDLE_MESSAGE, state = "") {
  if (activeHotkeyCaptureId === fieldId) activeHotkeyCaptureId = null;
  $(fieldId).classList.remove("recording");
  setHotkeyStatus(fieldId, message, state);
  syncHotkeyCapturePause();
}

function endAllHotkeyCaptures() {
  HOTKEY_FIELD_IDS.forEach((fieldId) => {
    if (activeHotkeyCaptureId === fieldId) activeHotkeyCaptureId = null;
    $(fieldId).classList.remove("recording");
    setHotkeyStatus(fieldId, HOTKEY_IDLE_MESSAGE);
  });
  activeHotkeyCaptureId = null;
  syncHotkeyCapturePause();
}

function isHotkeyRecorderFocus(el) {
  return !!(el && (HOTKEY_FIELD_IDS.includes(el.id) || el.id === "first-run-hotkey"));
}

function hotkeyCaptureShouldPause() {
  return isHotkeyRecorderFocus(document.activeElement) || firstRunHotkeyCapturing;
}

function hotkeyRecorderIsFocused(field) {
  return document.activeElement === field;
}

function focusHotkeyRecorder(field) {
  if (hotkeyRecorderIsFocused(field)) return true;
  field.focus();
  return false;
}

function hotkeyCapturePauseErrorTarget() {
  const firstRun = $("first-run-setup");
  if (firstRun && !firstRun.hidden) return $("first-run-error");
  return $("error");
}

var hotkeyCapturePauseChain = Promise.resolve();
var lastSentHotkeyCapturePause = false;

function syncHotkeyCapturePause() {
  // Blur then focus of another recorder field is one turn; wait until
  // activeElement has settled so we do not resume-then-pause the live bind.
  hotkeyCapturePauseChain = hotkeyCapturePauseChain
    .catch(() => {})
    .then(
      () =>
        new Promise((resolve) => {
          queueMicrotask(resolve);
        }),
    )
    .then(flushHotkeyCapturePause)
    .catch((error) => {
      const node = hotkeyCapturePauseErrorTarget();
      if (node) node.textContent = String(error);
    });
}

async function flushHotkeyCapturePause() {
  const active = hotkeyCaptureShouldPause();
  if (active === lastSentHotkeyCapturePause) return;
  await invoke("set_hotkey_capture_active", { active });
  lastSentHotkeyCapturePause = active;
  if (hotkeyCaptureShouldPause() !== lastSentHotkeyCapturePause) {
    return flushHotkeyCapturePause();
  }
}

function recordHotkey(fieldId, ev) {
  if (activeHotkeyCaptureId !== fieldId) beginHotkeyCapture(fieldId);
  ev.preventDefault();
  ev.stopPropagation();

  applyHotkeyCaptureResult(fieldId, hotkeyFromKeyEvent(ev));
}

function recordMouseHotkey(fieldId, ev) {
  if (ev.button === 0) return;
  ev.preventDefault();
  ev.stopPropagation();
  const field = $(fieldId);
  // An unfocused field has not paused live binds yet, and preventDefault
  // would otherwise skip focus. Arm the recorder on this press; bind the next.
  if (!focusHotkeyRecorder(field)) return;
  if (activeHotkeyCaptureId !== fieldId) beginHotkeyCapture(fieldId);

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
