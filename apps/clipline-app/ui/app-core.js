// Shared app foundation (Tauri bridge, state, helpers).
// Cross-file bindings use `var` so later ui/*.js scripts share one global scope.
var { invoke, convertFileSrc } = window.__TAURI__.core;
var { listen } = window.__TAURI__.event;
var appWindow = window.__TAURI__.window.getCurrentWindow();
var $ = (id) => document.getElementById(id);
function reportFrontendDiagnostic(level, event, error) {
  const value = error instanceof Error ? error : new Error(String(error ?? "unknown frontend error"));
  invoke("log_frontend_event", {
    input: {
      level,
      event,
      message: String(value.message || value).slice(0, 8192),
      stack: String(value.stack || "").slice(0, 16384),
    },
  }).catch(() => {});
}
window.addEventListener("error", (event) => {
  reportFrontendDiagnostic("error", "window_error", event.error || event.message);
});
window.addEventListener("unhandledrejection", (event) => {
  reportFrontendDiagnostic("error", "unhandled_rejection", event.reason);
});
var afterNextPaint = () => new Promise((resolve) => {
  requestAnimationFrame(() => requestAnimationFrame(resolve));
});

var {
  fmtBytes,
  fmtLibraryStorageUsage,
  fmtDur,
  fmtTenths,
  fmtAgo,
  overlayVisible,
  OVERLAY_HIDE_MS,
  clampTime,
  percentFor,
  percentForView,
  timelineTimeView,
  clampView,
  zoomView,
  panView,
  setViewEdge,
  viewForRange,
  followView,
  snapTime,
  snapCandidates,
  frameStep,
  editPoints,
  MIN_VIEW_SPAN_S,
  DEFAULT_FOLLOW_MODE,
  SNAP_THRESHOLD_PX,
  DEFAULT_FINE_STEP_S,
  resolveTrim,
  quickTrimRange,
  trimDrag,
  slideTrim,
  trimSummary,
  nextMarker,
  prevMarker,
  markerSummary,
  playBlocks,
  playRailItem,
  playExportRange,
  playSummary,
  playActiveIndex,
  markerStyle,
  markerDigest,
  normalizeGameReviewSettings,
  gameEventActiveIndex,
  gameEventRailItem,
  playerSummaryFields,
  galleryCardPreview,
  rulerMarksRange,
  sessionGroups,
  formatClipTitle,
  clipKind,
  keyIntent,
  hotkeyFromKeyEvent,
  hotkeyFromMouseEvent,
  displayMapLayout,
  displayMapHeight,
  regionForDisplay,
  clampRegionToDisplay,
  alignRegion,
  settingDurationLabel,
  recordingQualityPreset,
  recordingQualitySummary,
  qualityIndexForBitrate,
  smoothnessPreset,
  smoothnessIndexForFps,
  outputResolutionOption,
  captureSourceLabel,
} = PlayerCore;

var video = $("video");
var stage = document.querySelector(".stage");
var stageFrame = $("stage-frame");
var currentClip = null;
var clipsCache = [];
var windowLifecycleState = WindowLifecycleCore.initialState();
var foregroundBootCompleted = false;
var foregroundBootPromise = null;
// Gallery (library home) view state.
var gallerySource = "local";
var cloudClipsCache = [];
var cloudClipsLoaded = false;
var cloudClipsLoading = false;
var cloudClipsError = "";
var cloudClipsRequestGate = CloudCore.createRequestGate();
var localClipsRequestGate = CloudCore.createRequestGate();
var localLibraryWarning = "";
var railProfileAvatarKey = "";
var railProfileAvatarRequest = 0;
var galleryFilter = "all";
var gallerySort = "new";
var galleryGroup = "smart";
var gallerySearch = "";
var galleryPageState = GalleryWindowCore.initialState();
var galleryPageTotal = 0;
// Multi-select state for the local gallery. `selectMode` makes tile body
// clicks toggle selection instead of opening the clip; `selectedClipPaths`
// survives filter/sort/group/render rebuilds because it is keyed on `clip.path`.
var selectedClipPaths = new Set();
var selectMode = false;
var posterCache = new Map();
var POSTER_UNAVAILABLE = Symbol("poster unavailable");
var POSTER_CACHE_LIMIT = 120;
var currentSettings = null;
var settingsDraft = null;
var recordingActive = false;
var recordingRequested = false;
var recorderWaitingForGame = false;
var fullSessionRecordingActive = false;
var displays = [];
var displaysLoaded = false;
var displaysLoadPromise = null;
var audioDevices = { outputs: [], inputs: [] };
var audioDevicesLoaded = false;
var audioDevicesLoadPromise = null;
var videoEncoders = [];
var videoEncodersLoaded = false;
var videoEncodersLoadPromise = null;
var gamePlugins = [];
var gamePluginSettings = {};
var customGames = [];
var gameWindows = [];
var gameWindowsScanId = 0;
var detectedGameCandidates = [];
var selectedDetectedGameIds = new Set();
var detectedGamesScanId = 0;
var activeDetectedGame = null;
var warnedElevatedGameProcesses = new Set();
var elevationRestartInFlight = false;
var captureTargetDirty = false;
// Codecs WebView2 can decode in the review player (H.264 always; HEVC/AV1
// probed at startup). Drives the playback caveat and the recorder's
// Automatic-codec policy via report_decode_support.
var decodableCodecs = ["h264"];
var regionState = { display_id: null, x: 0, y: 0, width: 1920, height: 1080 };
var regionLayout = null;
var regionDrag = null;
var regionMenuDisplayId = null;
var clipContextTarget = null;
var cloudContextTarget = null;
var gamePlayContextTarget = null;
var uploadDialogClip = null;
var selectedAudioTrackIds = new Set();
var uploadSelectedAudioTrackIds = new Set();
var currentReviewAudioKey = null;
var currentReviewAudioTrackIds = [];
var currentReviewMediaPath = null;
var reviewAudioMode = "direct";
var reviewAudioMuted = false;
var reviewAudioVolume = 1;
var activeReviewAudioSidecars = [];
var reviewAudioSidecarGeneration = 0;
var reviewAudioDriftTimer = 0;
var renamePending = false;
var DECK_STATUS_TOAST_MS = 3200;
var deckStatusToastTimer = 0;
var deckStatusActionHandler = null;
var NOTICE_TOAST_MS = 2600;
var noticeToastTimer = 0;
var pendingPostRefreshFeedback = null;
var micTestRunning = false;
var micTestSurface = "settings";
var micAudioContext = null;
var micAudioCursor = 0;
var micAudioSources = [];
var pendingUpdate = null;
var updateCheckRunning = false;
var activeHotkeyCaptureId = null;
var trimStart = 0;
var trimEnd = 0;
var simpleTrimMode = false;
var dragging = null;
var overlayTimerId = 0;
// Timeline zoom: the visible window into the clip. zoomSpan === 0 means fully
// zoomed out (the whole clip is shown); a smaller span shows [zoomStart, +span].
var zoomStart = 0;
var zoomSpan = 0;
// Active navigator drag: { mode:"pan"|"left"|"right", grab?, pointerId } or null.
var overviewDrag = null;
// Magnetic snapping for scrub/trim drags (toggle with S, hold Alt to bypass).
var snapEnabled = true;
var MIC_MONITOR_START_DELAY_S = 0.02;
var MIC_MONITOR_MAX_LATENCY_S = 0.25;
// Clicking a marker/event row starts playback this many seconds before the
// moment so its lead-up plays rather than dropping the viewer right on it.
var MARKER_LEAD_S = 1;

function setDeckStatus(message, { transient = false } = {}) {
  window.clearTimeout(deckStatusToastTimer);
  deckStatusToastTimer = 0;
  setDeckStatusAction("", null);
  $("deck-status").textContent = message;
  if (!transient || !message) return;

  deckStatusToastTimer = window.setTimeout(() => {
    if ($("deck-status").textContent === message) {
      $("deck-status").textContent = "";
      setDeckStatusAction("", null);
    }
    deckStatusToastTimer = 0;
  }, DECK_STATUS_TOAST_MS);
}

function setDeckStatusAction(label, handler) {
  deckStatusActionHandler = typeof handler === "function" ? handler : null;
  const button = $("deck-status-action");
  button.textContent = deckStatusActionHandler ? label : "";
  button.hidden = !deckStatusActionHandler;
}

function runDeckStatusAction() {
  const action = deckStatusActionHandler;
  if (!action) return;
  setDeckStatus("");
  action();
}

function setNotice(message, { transient = false } = {}) {
  window.clearTimeout(noticeToastTimer);
  noticeToastTimer = 0;
  $("notice").textContent = message;
  if (!transient || !message) return;

  noticeToastTimer = window.setTimeout(() => {
    if ($("notice").textContent === message) {
      $("notice").textContent = "";
    }
    noticeToastTimer = 0;
  }, NOTICE_TOAST_MS);
}

function showPostRefreshFeedback(feedback) {
  if (feedback.error) $("error").textContent = feedback.error;
  if (feedback.notice) setNotice(feedback.notice, { transient: true });
}

function finishPostRefreshFeedback(refreshCompleted, feedback = {}) {
  const error = String(feedback.error || "").trim();
  const notice = String(feedback.notice || "").trim();
  if (!error && !notice) return;
  feedback = { error, notice };
  if (refreshCompleted) {
    showPostRefreshFeedback(feedback);
    return;
  }
  const pending = pendingPostRefreshFeedback || {};
  pendingPostRefreshFeedback = {
    error: error || pending.error || "",
    notice: notice || pending.notice || "",
  };
}

function flushDeferredPostRefreshFeedback() {
  const feedback = pendingPostRefreshFeedback;
  pendingPostRefreshFeedback = null;
  if (feedback) showPostRefreshFeedback(feedback);
}

function clipDuration() {
  if (Number.isFinite(video.duration) && video.duration > 0) return video.duration;
  if (currentClip && Number.isFinite(currentClip.duration_s)) return currentClip.duration_s;
  if (currentClip && currentClip.markers && Number.isFinite(currentClip.markers.duration_s)) {
    return currentClip.markers.duration_s;
  }
  return 0;
}

function rawClipMarkers(clip = currentClip) {
  return clip && clip.markers && Array.isArray(clip.markers.markers)
    ? clip.markers.markers
    : [];
}

function clipPlayerSummary(clip = currentClip) {
  return clip && clip.markers ? clip.markers.player_summary : null;
}

function gameReviewSettingsForClip(clip = currentClip) {
  const gameId = clip && clip.game && clip.game.id;
  const settings = gameId && gamePluginSettings[gameId] ? gamePluginSettings[gameId] : null;
  return normalizeGameReviewSettings(settings && settings.review);
}

function gameReviewEnabledForClip(clip = currentClip) {
  return gameReviewSettingsForClip(clip).enabled;
}

function clipMarkers(clip = currentClip) {
  return PlayerCore.reviewTimelineMarkers(
    rawClipMarkers(clip),
    clipPlayerSummary(clip),
    gameReviewSettingsForClip(clip),
    pluginPresentationForClip(clip),
  );
}

function clipPlays(clip = currentClip) {
  if (!gameReviewEnabledForClip(clip)) return [];
  return clip && clip.markers && Array.isArray(clip.markers.plays)
    ? clip.markers.plays
    : [];
}

function clipMatchEventMarkers(clip = currentClip) {
  return PlayerCore.reviewMatchEventMarkers(
    rawClipMarkers(clip),
    clipPlayerSummary(clip),
    gameReviewSettingsForClip(clip),
    pluginPresentationForClip(clip),
  );
}

function clipAudioTracks(clip = currentClip) {
  return clip && clip.markers && Array.isArray(clip.markers.audio_tracks)
    ? clip.markers.audio_tracks
    : [];
}

function defaultAudioTrackIds(clip = currentClip) {
  return PlayerCore.defaultAudioTrackIds(clipAudioTracks(clip));
}

function resetSelectedAudioTracks(clip = currentClip) {
  selectedAudioTrackIds = new Set(defaultAudioTrackIds(clip));
}

function pruneSelectedAudioTracks(clip = currentClip) {
  selectedAudioTrackIds = new Set(
    PlayerCore.selectedReviewAudioTrackIds(clipAudioTracks(clip), [...selectedAudioTrackIds]),
  );
}

function selectedAudioTrackIdsForClip(clip = currentClip, selected = selectedAudioTrackIds) {
  return PlayerCore.selectedReviewAudioTrackIds(clipAudioTracks(clip), [...selected]);
}

function audioSelectionKey(clip = currentClip, selected = selectedAudioTrackIdsForClip(clip)) {
  return `${clip && clip.path ? clip.path : ""}\n${selected.join("\n")}`;
}

function audioTrackLabel(track) {
  const label = track && track.label ? String(track.label).trim() : "";
  if (label) return label;
  const index = Number.isFinite(Number(track && track.track_index)) ? Number(track.track_index) + 1 : 1;
  return `Audio ${index}`;
}

function renderAudioTrackRows(container, clip, selected, onChange, {
  rowState = PlayerCore.audioTrackRowState,
} = {}) {
  container.replaceChildren();
  const tracks = clipAudioTracks(clip);
  const selectedIds = [...selected];
  for (const track of tracks) {
    const row = document.createElement("label");
    row.className = "audio-track-row";
    const input = document.createElement("input");
    const state = rowState(track, tracks, selectedIds);
    input.type = "checkbox";
    input.checked = state.checked;
    input.indeterminate = state.indeterminate;
    input.dataset.trackId = track.id || "";
    input.addEventListener("change", () => onChange(track, input.checked));
    const label = document.createElement("span");
    label.className = "audio-track-label";
    label.textContent = audioTrackLabel(track);
    label.title = label.textContent;
    row.append(input, label);
    container.appendChild(row);
  }
}

function renderAudioTrackPanel() {
  const panel = $("audio-track-panel");
  const list = $("audio-track-list");
  const summary = $("audio-track-summary");
  const tracks = clipAudioTracks();
  panel.hidden = tracks.length === 0;
  if (!tracks.length) {
    list.replaceChildren();
    summary.textContent = "";
    return;
  }
  summary.textContent = `${PlayerCore.reviewAudioTrackSelectedRowCount(tracks, [...selectedAudioTrackIds])}/${tracks.length} selected`;
  renderAudioTrackRows(list, currentClip, selectedAudioTrackIds, (track, checked) => {
    if (!track.id) return;
    selectedAudioTrackIds = new Set(
      PlayerCore.applyReviewAudioTrackToggle(tracks, [...selectedAudioTrackIds], track.id, checked),
    );
    renderAudioTrackPanel();
    requestSelectedAudioPreview();
  }, { rowState: PlayerCore.reviewAudioTrackRowState });
}

function renderUploadAudioTracks(clip = uploadDialogClip) {
  const section = $("upload-audio-section");
  const list = $("upload-audio-list");
  const tracks = clipAudioTracks(clip);
  section.hidden = tracks.length === 0;
  if (!tracks.length) {
    list.replaceChildren();
    return;
  }
  renderAudioTrackRows(list, clip, uploadSelectedAudioTrackIds, (track, checked) => {
    if (!track.id) return;
    uploadSelectedAudioTrackIds = new Set(
      PlayerCore.applyReviewAudioTrackToggle(tracks, [...uploadSelectedAudioTrackIds], track.id, checked),
    );
    renderUploadAudioTracks(clip);
  }, { rowState: PlayerCore.reviewAudioTrackRowState });
}

function audioSelectionLabel(clip = currentClip) {
  const tracks = clipAudioTracks(clip);
  if (!tracks.length) return "";
  const selected = PlayerCore.reviewAudioTrackSelectedRowCount(tracks, [...selectedAudioTrackIds]);
  if (selected === tracks.length) return "audio: all tracks";
  if (selected === 0) return "audio: muted";
  return `audio: ${selected}/${tracks.length} tracks`;
}

function videoAspect() {
  return video.videoWidth > 0 && video.videoHeight > 0
    ? video.videoWidth / video.videoHeight
    : 16 / 9;
}

function updateStageFrame() {
  const bounds = stage.getBoundingClientRect();
  if (bounds.width <= 0 || bounds.height <= 0) return;
  const aspect = videoAspect();
  let width = bounds.width;
  let height = width / aspect;
  if (height > bounds.height) {
    height = bounds.height;
    width = height * aspect;
  }
  stageFrame.style.width = `${Math.max(1, Math.floor(width))}px`;
  stageFrame.style.height = `${Math.max(1, Math.floor(height))}px`;
}

/* ---- sidebar: status, settings, library ---- */

function captureForegroundWork() {
  return WindowLifecycleCore.captureWork(windowLifecycleState);
}

function isForegroundWorkCurrent(work) {
  return WindowLifecycleCore.isWorkCurrent(windowLifecycleState, work);
}

function requestWindowRefresh() {
  const request = WindowLifecycleCore.requestRefresh(windowLifecycleState);
  windowLifecycleState = request.state;
  return request.refreshNow;
}

async function refresh() {
  const work = captureForegroundWork();
  if (!work) {
    requestWindowRefresh();
    return false;
  }
  await Promise.all([refreshClips(null, work), refreshStorage(work)]);
  const completed = isForegroundWorkCurrent(work);
  if (completed) flushDeferredPostRefreshFeedback();
  return completed;
}

async function refreshStorage(work = captureForegroundWork()) {
  if (!work) {
    requestWindowRefresh();
    return false;
  }
  const s = await invoke("storage_status");
  if (!isForegroundWorkCurrent(work)) return false;
  const quotaGb = s.quota_bytes == null ? 0 : Number(s.quota_bytes) / (1024 * 1024 * 1024);
  $("rail-clips-count").textContent = compactCount(s.clip_count);
  $("rail-library-status").title = `${plural(s.clip_count, "clip")} in library`;
  $("gallery-storage-used").textContent =
    `· ${fmtLibraryStorageUsage(s.total_bytes, quotaGb)}`;
  return true;
}

function compactCount(count) {
  return count > 99 ? "99+" : String(count);
}

function plural(count, singular) {
  return `${count} ${singular}${count === 1 ? "" : "s"}`;
}

async function refreshMemoryUsage() {
  try {
    const s = await invoke("memory_status");
    // Compact for the 64px rail; the "RAM" caption is a CSS ::before label.
    // Headline is Clipline's own process so a regression here is visible; the
    // webview runtime and any ffmpeg child are real but not ours to allocate,
    // and blending them hides a ring regression inside Edge's noise.
    $("memory-usage").textContent = fmtBytes(s.process_private_working_set_bytes);
    const children = $("memory-children");
    if (children) {
      const bytes = s.children_private_working_set_bytes;
      children.textContent = bytes > 0 ? `+${fmtBytes(bytes)} child` : "";
      children.hidden = !(bytes > 0);
    }
  } catch (_) {
    $("memory-usage").textContent = "-- MB";
    const children = $("memory-children");
    if (children) {
      children.textContent = "";
      children.hidden = true;
    }
  }
}
