// Settings: FFmpeg runtime, encoders, game detection, region editor.
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
