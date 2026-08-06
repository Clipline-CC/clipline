// First-launch setup controller. It intentionally writes into the existing
// Settings form before using save_settings, so setup and later edits share one
// validation, persistence, hotkey, autostart, and recorder-restart path.
var firstRunStep = 0;
var firstRunCandidates = [];
var firstRunSelectedCandidateIds = new Set();

function firstRunSelectedText(id) {
  const select = $(id);
  return select.selectedOptions[0] ? select.selectedOptions[0].textContent : "";
}

function firstRunVolumeLabel(value) {
  return `${Math.round(Number(value) * 100)}%`;
}

function syncFirstRunAudioFields() {
  const overlay = $("first-run-setup");
  if (!overlay) return;
  const outputEnabled = $("first-run-output-enabled").checked;
  $("first-run-output-device").disabled = !outputEnabled;
  $("first-run-output-volume").disabled = !outputEnabled;
  $("first-run-split-output").disabled = !outputEnabled;
  const testingHere = micTestRunning && micTestSurface === "first-run";
  $("first-run-mic-device").disabled = testingHere;
  $("first-run-mic-volume").disabled = testingHere;
  $("first-run-mic-mono").disabled = testingHere;
  $("first-run-test-mic").textContent = testingHere ? "Stop testing" : "Test mic";
  $("first-run-output-volume-label").textContent = firstRunVolumeLabel(
    $("first-run-output-volume").value,
  );
  $("first-run-mic-volume-label").textContent = firstRunVolumeLabel(
    $("first-run-mic-volume").value,
  );
  syncRangeProgress($("first-run-output-volume"));
  syncRangeProgress($("first-run-mic-volume"));
}

function syncFirstRunRecordingFields() {
  const replay = Number($("first-run-replay").value);
  const resolution = outputResolutionOption($("first-run-resolution").value);
  const quality = recordingQualityPreset(
    Number($("first-run-quality").value),
    resolution.id,
  );
  const smoothness = smoothnessPreset(Number($("first-run-fps").value));
  $("first-run-replay-label").textContent =
    `Save Replay writes the last ${settingDurationLabel(replay)}.`;
  $("first-run-resolution-label").textContent =
    `${resolution.label} output, ${resolution.hint}.`;
  $("first-run-quality-label").textContent = recordingQualitySummary(quality);
  $("first-run-fps-label").textContent =
    `${smoothness.label} — ${smoothness.hint}.`;
  for (const id of [
    "first-run-replay",
    "first-run-quality",
    "first-run-fps",
  ]) syncRangeProgress($(id));
}

function renderFirstRunAudioDevices() {
  fillDeviceSelect(
    "first-run-output-device",
    audioDevices.outputs,
    "Default output device",
    null,
  );
  fillDeviceSelect(
    "first-run-mic-device",
    audioDevices.inputs,
    "Default microphone",
    null,
  );
  syncFirstRunAudioFields();
}

function renderFirstRunCaptureTargets() {
  const source = $("set-capture");
  const target = $("first-run-capture-target");
  const desired = source.value;
  target.replaceChildren();
  for (const option of source.options) {
    if (option.value === "display_region") continue;
    target.appendChild(option.cloneNode(true));
  }
  if (!target.options.length) {
    const option = document.createElement("option");
    option.value = "primary_monitor";
    option.textContent = "Primary display";
    target.appendChild(option);
  }
  target.value = Array.from(target.options).some((option) => option.value === desired)
    ? desired
    : target.options[0].value;
}

function renderFirstRunSupportedGames() {
  const root = $("first-run-supported-games");
  root.replaceChildren();
  if (!gamePlugins.length) {
    const empty = document.createElement("div");
    empty.className = "first-run-empty-state";
    empty.textContent = "No built-in game integrations are available.";
    root.appendChild(empty);
    return;
  }
  for (const plugin of gamePlugins) {
    const row = document.createElement("label");
    row.className = "first-run-supported-game";
    const icon = gameIconEl(plugin.icon, plugin.name);
    const meta = document.createElement("span");
    const name = document.createElement("strong");
    name.textContent = plugin.name;
    const summary = document.createElement("small");
    summary.textContent = plugin.summary;
    meta.append(name, summary);
    const enabled = document.createElement("input");
    enabled.type = "checkbox";
    enabled.checked = gamePluginSetting(plugin).enabled;
    enabled.dataset.firstRunPlugin = plugin.id;
    enabled.setAttribute("aria-label", `Enable ${plugin.name}`);
    row.append(icon, meta, enabled);
    root.appendChild(row);
  }
}

function renderFirstRunDetectedGames() {
  const root = $("first-run-detected-games");
  const status = $("first-run-game-scan-status");
  root.replaceChildren();
  if (!firstRunCandidates.length) {
    root.hidden = true;
    $("first-run-detected-actions").hidden = true;
    status.hidden = false;
    status.replaceChildren();
    const title = document.createElement("strong");
    title.textContent = "No new games found";
    const detail = document.createElement("span");
    detail.textContent = "You can run detection again whenever your installed games change.";
    status.append(title, detail);
    return;
  }
  status.hidden = true;
  root.hidden = false;
  $("first-run-detected-actions").hidden = false;
  for (const candidate of firstRunCandidates) {
    const key = detectedGameKey(candidate);
    const row = document.createElement("label");
    row.className = "first-run-detected-game";
    const selected = document.createElement("input");
    selected.type = "checkbox";
    selected.checked = firstRunSelectedCandidateIds.has(key);
    selected.addEventListener("change", () => {
      if (selected.checked) firstRunSelectedCandidateIds.add(key);
      else firstRunSelectedCandidateIds.delete(key);
      syncFirstRunDetectedCount();
    });
    const icon = gameIconEl(candidate.icon, candidate.name);
    const meta = document.createElement("span");
    const name = document.createElement("strong");
    name.textContent = candidate.name || "Detected game";
    const detail = document.createElement("small");
    detail.textContent = detectedGameMeta(candidate);
    meta.append(name, detail);
    row.append(selected, icon, meta);
    root.appendChild(row);
  }
  syncFirstRunDetectedCount();
}

function syncFirstRunDetectedCount() {
  const count = firstRunSelectedCandidateIds.size;
  $("first-run-detected-count").textContent = count
    ? `${count} game${count === 1 ? "" : "s"} selected`
    : "No games selected";
  $("first-run-add-games").disabled = count === 0;
}

async function detectFirstRunGames() {
  const scanButton = $("first-run-detect-games");
  const status = $("first-run-game-scan-status");
  scanButton.disabled = true;
  scanButton.textContent = "Scanning...";
  firstRunCandidates = [];
  firstRunSelectedCandidateIds.clear();
  $("first-run-detected-games").hidden = true;
  $("first-run-detected-actions").hidden = true;
  status.hidden = false;
  status.replaceChildren();
  const title = document.createElement("strong");
  title.textContent = "Scanning installed games...";
  const detail = document.createElement("span");
  detail.textContent = "This usually takes a few seconds.";
  status.append(title, detail);
  $("first-run-error").textContent = "";
  try {
    const candidates = await invoke("detect_installed_games", {
      existingCustomGames: customGames,
    });
    firstRunCandidates = Array.isArray(candidates) ? candidates : [];
    renderFirstRunDetectedGames();
  } catch (error) {
    status.hidden = true;
    $("first-run-error").textContent = String(error);
  } finally {
    scanButton.disabled = false;
    scanButton.textContent = "Detect Games";
  }
}

function addFirstRunDetectedGames() {
  const selected = firstRunCandidates.filter((candidate) =>
    firstRunSelectedCandidateIds.has(detectedGameKey(candidate)),
  );
  const usedIds = new Set(customGames.map((game) => game.id));
  const additions = selected
    .filter((candidate) =>
      !customGames.some((game) => customGameMatchesCandidate(game, candidate)))
    .map((candidate) => customGameFromDetectedCandidate(candidate, usedIds));
  if (additions.length) customGames.push(...additions);
  const selectedKeys = new Set(selected.map(detectedGameKey));
  firstRunCandidates = firstRunCandidates.filter(
    (candidate) => !selectedKeys.has(detectedGameKey(candidate)),
  );
  firstRunSelectedCandidateIds.clear();
  renderFirstRunDetectedGames();
  const added = $("first-run-added-games");
  added.hidden = false;
  added.textContent = additions.length
    ? `Added ${additions.map((game) => game.name).join(", ")}.`
    : "Those games were already added.";
}

function updateFirstRunReview() {
  const quality = recordingQualityPreset(
    Number($("first-run-quality").value),
    $("first-run-resolution").value,
  );
  const smoothness = smoothnessPreset(Number($("first-run-fps").value));
  const supported = Array.from(
    document.querySelectorAll("[data-first-run-plugin]:checked"),
  ).map((input) => {
    const plugin = gamePlugins.find((item) => item.id === input.dataset.firstRunPlugin);
    return plugin ? plugin.name : input.dataset.firstRunPlugin;
  });
  $("first-run-summary-hotkey").textContent = $("first-run-hotkey").value;
  $("first-run-summary-folder").textContent = $("first-run-media-dir").value;
  $("first-run-summary-quota").textContent = `${$("first-run-quota").value} GB`;
  $("first-run-summary-startup").textContent = $("first-run-startup").checked ? "On" : "Off";
  $("first-run-summary-output").textContent = $("first-run-output-enabled").checked
    ? `${firstRunSelectedText("first-run-output-device")} at ${firstRunVolumeLabel($("first-run-output-volume").value)}`
    : "Off";
  $("first-run-summary-input").textContent = $("first-run-mic-enabled").checked
    ? `${firstRunSelectedText("first-run-mic-device")} at ${firstRunVolumeLabel($("first-run-mic-volume").value)}`
    : "Off";
  $("first-run-summary-capture").textContent = firstRunSelectedText("first-run-capture-target");
  $("first-run-summary-pause").textContent = $("first-run-pause-no-game").checked ? "On" : "Off";
  $("first-run-summary-replay").textContent = settingDurationLabel($("first-run-replay").value);
  $("first-run-summary-resolution").textContent = firstRunSelectedText("first-run-resolution");
  $("first-run-summary-quality").textContent = `${quality.label} (${quality.bitrate} Mbps)`;
  $("first-run-summary-fps").textContent = smoothness.label;
  $("first-run-summary-supported").textContent = supported.length ? supported.join(", ") : "None";
  $("first-run-summary-other").textContent = customGames.length
    ? customGames.map((game) => game.name).join(", ")
    : "None added";
}

function validateFirstRunBasics() {
  const hotkey = $("first-run-hotkey").value.trim();
  const mediaDir = $("first-run-media-dir").value.trim();
  const quota = Number($("first-run-quota").value);
  if (!hotkey) return "Choose a replay button hotkey.";
  if (!mediaDir) return "Choose a media folder.";
  if (!Number.isFinite(quota) || quota < 1 || quota > 1000) {
    return "Disk quota must be between 1 and 1,000 GB.";
  }
  return "";
}

async function stopFirstRunMicTest() {
  if (!micTestRunning || micTestSurface !== "first-run") return;
  await invoke("stop_microphone_test").catch(() => {});
  stopMicTestUi("stopped");
}

function showFirstRunStep(step) {
  firstRunStep = Math.max(0, Math.min(3, step));
  document.querySelectorAll("[data-first-run-page]").forEach((page) => {
    page.hidden = Number(page.dataset.firstRunPage) !== firstRunStep;
  });
  document.querySelectorAll("[data-first-run-step]").forEach((item) => {
    const itemStep = Number(item.dataset.firstRunStep);
    item.classList.toggle("active", itemStep === firstRunStep);
    item.classList.toggle("complete", itemStep < firstRunStep);
    if (itemStep === firstRunStep) item.setAttribute("aria-current", "step");
    else item.removeAttribute("aria-current");
  });
  $("first-run-back").hidden = firstRunStep === 0;
  $("first-run-next").hidden = firstRunStep === 3;
  $("first-run-finish").hidden = firstRunStep !== 3;
  $("first-run-error").textContent = "";
  if (firstRunStep === 3) updateFirstRunReview();
  $("first-run-content").scrollTop = 0;
}

function applyFirstRunFormToSettings() {
  $("set-hotkey").value = $("first-run-hotkey").value.trim();
  $("set-hotkey-2").value = "";
  $("set-media-dir").value = $("first-run-media-dir").value.trim();
  $("set-quota").value = $("first-run-quota").value;
  $("set-open-on-startup").checked = $("first-run-startup").checked;
  $("set-output-enabled").checked = $("first-run-output-enabled").checked;
  $("set-audio-split-output").checked = $("first-run-split-output").checked;
  $("set-output-device").value = $("first-run-output-device").value;
  $("set-output-volume").value = $("first-run-output-volume").value;
  $("set-mic-enabled").checked = $("first-run-mic-enabled").checked;
  $("set-mic-device").value = $("first-run-mic-device").value;
  $("set-mic-volume").value = $("first-run-mic-volume").value;
  $("set-mic-mono").checked = $("first-run-mic-mono").checked;
  const target = $("first-run-capture-target").value;
  if (Array.from($("set-capture").options).some((option) => option.value === target)) {
    $("set-capture").value = target;
    captureTargetDirty = true;
    syncCaptureFields();
  }
  $("set-games-auto-detect").checked = true;
  $("set-games-pause-when-empty").checked = $("first-run-pause-no-game").checked;
  $("set-buffer").value = $("first-run-replay").value;
  $("set-replay").value = $("first-run-replay").value;
  $("set-output-resolution").value = $("first-run-resolution").value;
  $("set-bitrate").value = $("first-run-quality").value;
  $("set-fps").value = $("first-run-fps").value;
  $("recording-mode-basic").checked = true;
  $("recording-mode-advanced").checked = false;
  for (const plugin of gamePlugins) {
    const input = document.querySelector(`[data-first-run-plugin="${plugin.id}"]`);
    gamePluginSettings[plugin.id] = normalizeGamePluginSettings({
      ...gamePluginSetting(plugin),
      enabled: input ? input.checked : gamePluginSetting(plugin).enabled,
    }, plugin);
  }
  renderGamePlugins();
  renderCustomGames();
  syncAudioFields();
  syncRecordingFields();
  return readSettings();
}

function closeFirstRunSetup() {
  $("first-run-setup").hidden = true;
  const app = document.querySelector(".app");
  app.inert = false;
  app.removeAttribute("aria-hidden");
}

async function finishFirstRunSetup() {
  const finish = $("first-run-finish");
  finish.disabled = true;
  finish.textContent = "Starting...";
  $("first-run-back").disabled = true;
  $("first-run-error").textContent = "";
  await stopFirstRunMicTest();
  try {
    const settings = applyFirstRunFormToSettings();
    const saved = await invoke("save_settings", { settings });
    fillSettings(saved);
    closeFirstRunSetup();
    try {
      recordingRequested = await invoke("set_recording", { recording: true });
      updateCaptureStatus();
      await refresh();
    } catch (error) {
      $("error").textContent = `Setup was saved, but recording could not start: ${error}`;
    }
  } catch (error) {
    $("first-run-error").textContent = String(error);
    finish.disabled = false;
    finish.textContent = "Start Clipline";
    $("first-run-back").disabled = false;
  }
}

async function openFirstRunSetup(settings) {
  const app = document.querySelector(".app");
  app.inert = true;
  app.setAttribute("aria-hidden", "true");
  $("first-run-setup").hidden = false;
  $("first-run-media-dir").value = settings.media_dir || "";
  renderFirstRunSupportedGames();
  showFirstRunStep(0);
  await Promise.all([ensureDisplaysLoaded(), ensureAudioDevicesLoaded()]);
  renderFirstRunCaptureTargets();
  renderFirstRunAudioDevices();
  syncFirstRunRecordingFields();
  $("first-run-next").focus();
}

$("first-run-browse").addEventListener("click", async () => {
  $("first-run-error").textContent = "";
  try {
    const selected = await invoke("choose_media_folder");
    if (selected) $("first-run-media-dir").value = selected;
  } catch (error) {
    $("first-run-error").textContent = String(error);
  }
});

$("first-run-hotkey").addEventListener("focus", () => {
  $("first-run-hotkey").classList.add("recording");
});
$("first-run-hotkey").addEventListener("blur", () => {
  $("first-run-hotkey").classList.remove("recording");
});
$("first-run-hotkey").addEventListener("keydown", (event) => {
  if (event.key === "Tab") return;
  event.preventDefault();
  event.stopPropagation();
  const result = hotkeyFromKeyEvent(event);
  if (result.kind === "captured") {
    $("first-run-hotkey").value = result.value;
    $("first-run-error").textContent = "";
    $("first-run-hotkey").blur();
  } else if (result.kind === "invalid") {
    $("first-run-error").textContent = result.message;
  } else if (result.kind === "cancel") {
    $("first-run-hotkey").blur();
  }
});
$("first-run-hotkey").addEventListener("mousedown", (event) => {
  if (event.button === 0) return;
  event.preventDefault();
  const result = hotkeyFromMouseEvent(event);
  if (result.kind === "captured") {
    $("first-run-hotkey").value = result.value;
    $("first-run-error").textContent = "";
    $("first-run-hotkey").blur();
  } else {
    $("first-run-error").textContent = result.message;
  }
});

for (const id of ["first-run-output-enabled", "first-run-split-output"]) {
  $(id).addEventListener("change", syncFirstRunAudioFields);
}
for (const id of ["first-run-output-volume", "first-run-mic-volume"]) {
  $(id).addEventListener("input", syncFirstRunAudioFields);
}
for (const id of [
  "first-run-replay",
  "first-run-resolution",
  "first-run-quality",
  "first-run-fps",
]) $(id).addEventListener("input", syncFirstRunRecordingFields);

$("first-run-test-mic").addEventListener("click", () => testMic("first-run"));
$("first-run-detect-games").addEventListener("click", detectFirstRunGames);
$("first-run-add-games").addEventListener("click", addFirstRunDetectedGames);
$("first-run-back").addEventListener("click", async () => {
  await stopFirstRunMicTest();
  showFirstRunStep(firstRunStep - 1);
});
$("first-run-next").addEventListener("click", async () => {
  if (firstRunStep === 0) {
    const error = validateFirstRunBasics();
    if (error) {
      $("first-run-error").textContent = error;
      return;
    }
  }
  await stopFirstRunMicTest();
  showFirstRunStep(firstRunStep + 1);
});
$("first-run-finish").addEventListener("click", finishFirstRunSetup);
