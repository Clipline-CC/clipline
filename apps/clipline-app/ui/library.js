// Local/cloud gallery, clip cards, multi-select.
const CAPTURE_MONITOR_ICON =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="3.5" y="4.5" width="17" height="12" rx="1.5"/><path d="M9 20h6M10.5 16.5 10 20M13.5 16.5 14 20"/></svg>';
const CAPTURE_REGION_ICON =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M8 3H4a1 1 0 0 0-1 1v4M16 3h4a1 1 0 0 1 1 1v4M3 16v4a1 1 0 0 0 1 1h4"/><rect x="7" y="7" width="10" height="10" rx="1" stroke-dasharray="2.2 2.2"/><path d="m15 14 5.5 2.2-2.2.9 1.2 2.8-1.7.7-1.2-2.8-1.8 1.7z" fill="currentColor" stroke="none"/></svg>';

// Resolve the icon for the game currently being captured. The detected-game
// payload carries no plugin id, so match a custom game by exe/window/name, then
// fall back to a plugin by name; { url: null } means "known game, no icon".
function activeGameIcon() {
  const g = activeDetectedGame;
  if (!g || !g.active) return null;
  const exe = (g.exe_name || "").toLowerCase();
  const custom = customGames.find((c) =>
    (c.exe_name && exe && c.exe_name.toLowerCase() === exe) ||
    (c.window_title && g.window_title && c.window_title === g.window_title) ||
    (c.name && g.name && c.name === g.name));
  if (custom && custom.icon) return { url: custom.icon, label: custom.name || g.name };
  const plugin = gamePlugins.find((p) => p.name === g.name);
  if (plugin && plugin.icon) return { url: plugin.icon, label: plugin.name };
  return { url: null, label: g.name };
}

function railGamePlaceholder() {
  const ph = document.createElement("div");
  ph.className = "placeholder";
  ph.innerHTML = GENERIC_GAME_ICON; // static markup, safe
  return ph;
}

function captureTargetIcon() {
  const game = activeGameIcon();
  if (game) return game;
  const settings = currentSettings || { capture_mode: "primary_monitor" };
  const fullDisplay = settings.capture_mode === "display_region"
    && displays.some((display) => isFullDisplayRegion(settings.capture_region, display));
  const region = settings.capture_mode === "display_region" && !fullDisplay;
  return {
    url: null,
    label: fallbackCaptureSourceLabel(settings),
    markup: region ? CAPTURE_REGION_ICON : CAPTURE_MONITOR_ICON,
  };
}

// Show what the replay buffer is capturing. The surrounding button owns the
// active/off state and toggles the existing recorder action.
function renderRailGame() {
  const host = $("rail-game");
  if (!host) return;
  const icon = captureTargetIcon();
  const iconKey = icon.url ? `url:${icon.url}` : `markup:${icon.markup || "game"}`;
  if (railCaptureTargetIconKey === iconKey && host.childElementCount) return;
  railCaptureTargetIconKey = iconKey;
  host.replaceChildren();
  if (icon.url) {
    const img = document.createElement("img");
    img.src = icon.url;
    img.alt = "";
    img.addEventListener("error", () => img.replaceWith(railGamePlaceholder()));
    host.appendChild(img);
  } else if (icon.markup) {
    const fallback = document.createElement("span");
    fallback.className = "placeholder source-icon";
    fallback.innerHTML = icon.markup; // static markup, safe
    host.appendChild(fallback);
  } else {
    host.appendChild(railGamePlaceholder());
  }
}

function clipDisplayTitle(clip) {
  const title = String(clip && clip.title || "").trim();
  return title || PresentationCore.clipNameStem(clip && clip.name);
}

const CLOUD_POSTER_CACHE_PREFIX = "cloud-thumb:";
const POSTER_UNAVAILABLE_RETRY_MS = 30_000;
var posterUnavailableUntil = new Map();
var posterRuntimeWarningReported = false;
var localClipPathIndexSource = null;
var localClipPathIndex = new Set();

function posterCacheGet(key) {
  if (posterCache.get(key) === POSTER_UNAVAILABLE) {
    const retryAt = posterUnavailableUntil.get(key);
    if (!Number.isFinite(retryAt) || Date.now() >= retryAt) {
      posterCacheDelete(key);
      return undefined;
    }
  }
  // Map insertion order gives us a tiny LRU without retaining image elements
  // or decoded bitmaps: the cache owns URL strings / the unavailable sentinel.
  return GalleryWindowCore.cacheGet(posterCache, key);
}

function posterCacheSet(key, value) {
  if (!key) return;
  const evicted = GalleryWindowCore.cacheSet(
    posterCache,
    key,
    value,
    POSTER_CACHE_LIMIT,
  );
  for (const evictedKey of evicted) posterUnavailableUntil.delete(evictedKey);
  if (value === POSTER_UNAVAILABLE) {
    posterUnavailableUntil.set(key, Date.now() + POSTER_UNAVAILABLE_RETRY_MS);
  } else {
    posterUnavailableUntil.delete(key);
  }
}

function posterCacheDelete(key) {
  posterCache.delete(key);
  posterUnavailableUntil.delete(key);
}

function clearCloudPosterCache() {
  for (const key of [...posterCache.keys()]) {
    if (String(key).startsWith(CLOUD_POSTER_CACHE_PREFIX)) {
      posterCacheDelete(key);
    }
  }
}

function localClipPaths(clips) {
  const source = Array.isArray(clips) ? clips : [];
  if (source === localClipPathIndexSource) return localClipPathIndex;
  const paths = new Set();
  for (const clip of source) {
    const key = GalleryWindowCore.clipPathKey(clip && clip.path);
    if (key) paths.add(key);
  }
  localClipPathIndexSource = source;
  localClipPathIndex = paths;
  return paths;
}

function pruneLocalPosterCache(clips) {
  const paths = localClipPaths(clips);
  for (const key of [...posterCache.keys()]) {
    if (String(key).startsWith(CLOUD_POSTER_CACHE_PREFIX)) continue;
    if (!paths.has(GalleryWindowCore.clipPathKey(key))) {
      posterCacheDelete(key);
    }
  }
}

function pruneCloudPosterCache(entries) {
  const valid = new Set(
    (Array.isArray(entries) ? entries : [])
      .filter((entry) => entry && entry.remote_clip_id)
      .map((entry) => cloudThumbnailKey(entry)),
  );
  for (const key of [...posterCache.keys()]) {
    if (
      String(key).startsWith(CLOUD_POSTER_CACHE_PREFIX)
      && !valid.has(key)
    ) {
      posterCacheDelete(key);
    }
  }
}

function replacePosterCachePath(oldPath, newPath) {
  if (!oldPath || !newPath || oldPath === newPath || !posterCache.has(oldPath)) return;
  const cached = posterCacheGet(oldPath);
  posterCacheDelete(oldPath);
  posterCacheSet(newPath, cached);
}

function replaceClipInCache(oldPath, renamed) {
  invalidateLocalClipsRefresh();
  clipsCache = clipsCache.map((clip) => (clip.path === oldPath ? renamed : clip));
  if (oldPath !== renamed.path && selectedClipPaths.delete(oldPath)) {
    selectedClipPaths.add(renamed.path);
  }
  replacePosterCachePath(oldPath, renamed.path);
}

function invalidateLocalClipsRefresh() {
  localClipsRequestGate.invalidate();
}

function applyLocalLibraryWarnings(warnings) {
  const error = $("error");
  if (localLibraryWarning && error.textContent === localLibraryWarning) {
    error.textContent = "";
  }
  const normalized = Array.isArray(warnings)
    ? warnings.map((warning) => String(warning || "").trim()).filter(Boolean)
    : [];
  localLibraryWarning = normalized.join(" ");
  if (localLibraryWarning) {
    error.textContent = localLibraryWarning;
  }
}

async function refreshClips(
  preferredCurrentPath = null,
  lifecycleWork = captureForegroundWork(),
) {
  if (!lifecycleWork) {
    requestWindowRefresh();
    return false;
  }
  const request = localClipsRequestGate.begin("local-library");
  const isCurrent = () => (
    isForegroundWorkCurrent(lifecycleWork)
    && localClipsRequestGate.isCurrent(request, "local-library")
  );
  let freshClips;
  let result;
  try {
    result = await invoke("list_clips");
    freshClips = Array.isArray(result.clips) ? result.clips : [];
  } catch (error) {
    if (!isCurrent()) return false;
    throw error;
  }
  if (!isCurrent()) return false;
  applyLocalLibraryWarnings(result.warnings);
  clipsCache = freshClips;
  pruneLocalPosterCache(clipsCache);
  const availablePaths = localClipPaths(clipsCache);
  selectedClipPaths = new Set(
    [...selectedClipPaths].filter((path) =>
      availablePaths.has(GalleryWindowCore.clipPathKey(path))),
  );
  if (currentClip) {
    const currentPath = preferredCurrentPath || currentClip.path;
    const fresh = clipsCache.find((clip) => PlayerCore.sameClipPath(clip.path, currentPath));
    if (fresh) {
      currentClip = { ...fresh, path: currentPath };
      pruneSelectedAudioTracks(fresh);
      $("pname").textContent = clipDisplayTitle(fresh) || fresh.name;
      renderAudioTrackPanel();
    } else {
      closeReview();
    }
  }
  // Keep the screenshots Gallery current while it is open. It must render
  // after (not before) renderClips: the Library pass bumps the poster-work
  // generation, which would otherwise invalidate the Gallery's queued jobs
  // and leave thumbnails blank until a manual reload.
  renderClips();
  renderScreenshotsAfterLibraryPass();
  return true;
}
// Leading icon per clip kind. Static markup (no clip data) — innerHTML is safe.
const CLIP_KIND_ICONS = {
  replay:
    '<svg viewBox="0 0 24 24"><path d="M7 2v11h3v9l7-12h-4l4-8z"/></svg>',
  session:
    '<svg viewBox="0 0 24 24"><path d="M3 5h18v14H3V5zM5 6v2h2v-2zM9 6v2h2v-2zM13 6v2h2v-2zM17 6v2h2v-2zM5 16v2h2v-2zM9 16v2h2v-2zM13 16v2h2v-2zM17 16v2h2v-2z"/></svg>',
  trim:
    '<svg viewBox="0 0 24 24"><path d="M9.64 7.64c.23-.5.36-1.05.36-1.64 0-2.21-1.79-4-4-4S2 3.79 2 6s1.79 4 4 4c.59 0 1.14-.13 1.64-.36L10 12l-2.36 2.36C7.14 14.13 6.59 14 6 14c-2.21 0-4 1.79-4 4s1.79 4 4 4 4-1.79 4-4c0-.59-.13-1.14-.36-1.64L12 14l7 7h3v-1L9.64 7.64zM6 8c-1.1 0-2-.89-2-2s.9-2 2-2 2 .89 2 2-.9 2-2 2zm0 12c-1.1 0-2-.89-2-2s.9-2 2-2 2 .89 2 2-.9 2-2 2zm6-7.5c-.28 0-.5-.22-.5-.5s.22-.5.5-.5.5.22.5.5-.22.5-.5.5zM19 3l-6 6 2 2 7-7V3z"/></svg>',
  screenshot:
    '<svg viewBox="0 0 24 24"><path d="M9 3 7.2 5H4a2 2 0 0 0-2 2v11a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2V7a2 2 0 0 0-2-2h-3.2L15 3H9zm3 5.5A4.5 4.5 0 1 1 7.5 13 4.5 4.5 0 0 1 12 8.5zm0 2A2.5 2.5 0 1 0 14.5 13 2.5 2.5 0 0 0 12 10.5z"/></svg>',
};
const CLIP_KIND_LABELS = {
  replay: "Buffered replay",
  session: "Full session",
  trim: "Trimmed export",
  screenshot: "Screenshot",
};
const CLOUD_VISIBILITY_ICONS = {
  public:
    '<svg viewBox="0 0 24 24"><circle cx="12" cy="12" r="9"/><path d="M3 12h18M12 3c2.4 2.5 3.6 5.5 3.6 9s-1.2 6.5-3.6 9c-2.4-2.5-3.6-5.5-3.6-9S9.6 5.5 12 3z"/></svg>',
  unlisted:
    '<svg viewBox="0 0 24 24"><path d="M10 13a5 5 0 0 0 7.1.5l2.4-2.4a5 5 0 0 0-7.1-7.1L11 5.4"/><path d="M14 11a5 5 0 0 0-7.1-.5L4.5 12.9a5 5 0 0 0 7.1 7.1L13 18.6"/></svg>',
  private:
    '<svg viewBox="0 0 24 24"><rect x="5" y="10" width="14" height="10" rx="2"/><path d="M8 10V7a4 4 0 0 1 8 0v3"/></svg>',
};
const CLOUD_VISIBILITY_LABELS = {
  public: "Public cloud clip",
  unlisted: "Unlisted cloud clip",
  private: "Private cloud clip",
};

// Neutral fallback when a game has no extractable/bundled icon. Static markup.
const GENERIC_GAME_ICON =
  '<svg viewBox="0 0 24 24"><path d="M3 5h18a1 1 0 0 1 1 1v9a1 1 0 0 1-1 1h-7l1 2h2v2H6v-2h2l1-2H3a1 1 0 0 1-1-1V6a1 1 0 0 1 1-1zm1 2v7h16V7H4z"/></svg>';

// A game-icon element: an <img> for a real icon (a plugin's bundled URL or an
// extracted data URL), falling back to a neutral glyph when absent or broken.
function gameIconEl(iconUrl, label) {
  if (iconUrl) {
    const img = document.createElement("img");
    img.className = "game-icon";
    img.src = iconUrl;
    img.alt = "";
    if (label) img.title = label;
    img.addEventListener("error", () => img.replaceWith(gamePlaceholderEl()));
    return img;
  }
  return gamePlaceholderEl();
}
function gamePlaceholderEl() {
  const el = document.createElement("div");
  el.className = "game-icon placeholder";
  el.innerHTML = GENERIC_GAME_ICON; // static markup, safe
  return el;
}

function customGameForRecordedGame(recordedGame) {
  if (!recordedGame || !recordedGame.id) return null;
  const exact = customGames.find((custom) => custom.id === recordedGame.id);
  if (exact) return exact;
  return customGames.find((custom) =>
    custom.name === recordedGame.name &&
    Array.isArray(custom.legacy_ids) && custom.legacy_ids.includes(recordedGame.id)
  ) || null;
}

// Resolve a clip's recorded game to an icon, reusing the icons shown in
// settings. Migrated custom records win by their old id plus name so an old
// built-in collision keeps its custom icon without gaining plugin behavior.
function clipGameIcon(clip) {
  const g = clip && clip.game;
  if (!g || !g.id) return null;
  const custom = customGameForRecordedGame(g);
  if (custom && custom.icon) return { url: custom.icon, label: custom.name };
  const plugin = gamePlugins.find((p) => p.id === g.id);
  if (plugin && plugin.icon) return { url: plugin.icon, label: plugin.name };
  return null;
}

function pluginForGameId(gameId) {
  return gamePlugins.find((plugin) => plugin.id === gameId) || null;
}

function pluginForClip(clip) {
  const gameId = clip && clip.game && clip.game.id;
  if (clip && customGameForRecordedGame(clip.game)) return null;
  return gameId ? pluginForGameId(gameId) : null;
}

function pluginPresentationForClip(clip) {
  if (!gameReviewEnabledForClip(clip)) return null;
  const plugin = pluginForClip(clip);
  return plugin && plugin.presentation ? plugin.presentation : null;
}

function currentPluginPresentation() {
  return pluginPresentationForClip(currentClip);
}

function pluginGalleryPolicy(clip) {
  const presentation = pluginPresentationForClip(clip);
  return presentation && presentation.gallery ? presentation.gallery : null;
}

function markerDisplayLabel(marker, presentation) {
  const kind = marker && marker.kind ? marker.kind : "Other";
  const configured = PlayerCore.markerKindConfig(kind, presentation);
  const label = PresentationCore.markerKindLabel(kind, configured && configured.label);
  const actor = marker && marker.actor ? ` · ${marker.actor}` : "";
  return `${fmtDur(marker.t_s)} ${label}${actor}`;
}

function markerEventText(marker, presentation) {
  const kind = marker && marker.kind ? marker.kind : "Other";
  const configured = PlayerCore.markerKindConfig(kind, presentation);
  const label = PresentationCore.markerKindLabel(kind, configured && configured.label);
  const actor = marker && marker.actor ? ` · ${marker.actor}` : "";
  return `${label}${actor}`;
}

function gameEventPortrait(slot) {
  const root = document.createElement("span");
  root.className = "game-event-participant";
  const portrait = document.createElement("span");
  portrait.className = "game-event-portrait";
  portrait.title = slot.champion ? `${slot.champion} · ${slot.name}` : slot.name;
  if (slot.asset) {
    const img = document.createElement("img");
    img.src = slot.asset;
    img.alt = slot.champion || slot.name;
    img.addEventListener("error", () => {
      img.remove();
      portrait.textContent = slot.initials || "?";
    }, { once: true });
    portrait.appendChild(img);
  } else {
    portrait.textContent = slot.initials || "?";
  }
  const name = document.createElement("span");
  name.className = "game-event-name";
  name.textContent = slot.name || slot.champion || "?";
  root.append(portrait, name);
  return root;
}

function gameEventIcon(view, marker, presentation) {
  const icon = document.createElement("span");
  icon.className = "game-event-kind-icon";
  icon.title = view.label || markerDisplayLabel(marker, presentation);
  if (view.icon) {
    const img = document.createElement("img");
    img.src = view.icon;
    img.alt = "";
    img.setAttribute("aria-hidden", "true");
    img.addEventListener("error", () => {
      img.remove();
      icon.textContent = markerStyle(marker.kind, presentation).glyph;
    }, { once: true });
    icon.appendChild(img);
  } else {
    icon.textContent = markerStyle(marker.kind, presentation).glyph;
  }
  return icon;
}

var activeGameEventIndex = -1;
var selectedGameEventIndex = -1;
var selectedGameEventTime = null;
var gameEventRailCollapsed = false;
var gameEventRows = [];
var activeGamePlayIndex = -1;
var selectedGamePlayIndex = -1;
var selectedGamePlayPending = false;
var selectedGamePlayStart = null;
var selectedGamePlayEnd = null;
var gamePlayRows = [];

function eventRailPolicy(clip) {
  const presentation = pluginPresentationForClip(clip);
  return presentation && presentation.event_rail ? presentation.event_rail : null;
}

function playRailPolicy(clip) {
  const presentation = pluginPresentationForClip(clip);
  return presentation && presentation.play_rail ? presentation.play_rail : null;
}

function metadataPanelPolicy(clip) {
  const presentation = pluginPresentationForClip(clip);
  return presentation && presentation.metadata_panel ? presentation.metadata_panel : null;
}

function syncReviewSideRailLayout() {
  const eventRail = $("game-event-rail");
  const playRail = $("game-play-rail");
  const reviewBody = eventRail
    ? eventRail.closest(".review-body")
    : (playRail ? playRail.closest(".review-body") : null);
  if (!reviewBody) return;
  const eventVisible = eventRail && !eventRail.hidden;
  const playVisible = playRail && !playRail.hidden;
  reviewBody.classList.toggle("has-event-rail", Boolean(eventVisible || playVisible));
  reviewBody.classList.toggle(
    "event-rail-collapsed",
    Boolean(eventVisible && !playVisible && gameEventRailCollapsed),
  );
}

function clearGameEventSelection() {
  selectedGameEventIndex = -1;
  selectedGameEventTime = null;
}

function selectGameEvent(index, markerTime) {
  selectedGameEventIndex = index;
  selectedGameEventTime = Number.isFinite(markerTime) ? markerTime : null;
}

function selectedGameEventIndexForTime(currentTime) {
  if (selectedGameEventIndex < 0 || selectedGameEventTime == null) return -1;
  if (currentTime >= selectedGameEventTime - 0.15) {
    clearGameEventSelection();
    return -1;
  }
  return selectedGameEventIndex;
}

function clearGamePlaySelection() {
  selectedGamePlayIndex = -1;
  selectedGamePlayPending = false;
  selectedGamePlayStart = null;
  selectedGamePlayEnd = null;
}

function selectGamePlay(index, playStart, playEnd) {
  selectedGamePlayIndex = index;
  selectedGamePlayPending = true;
  selectedGamePlayStart = Number.isFinite(playStart) ? playStart : null;
  selectedGamePlayEnd = Number.isFinite(playEnd) ? playEnd : selectedGamePlayStart;
}

function selectedGamePlayIndexForTime(currentTime, options = {}) {
  if (selectedGamePlayIndex < 0 || selectedGamePlayStart == null) return -1;
  const t = Number(currentTime);
  const start = Number(selectedGamePlayStart);
  const end = Number.isFinite(selectedGamePlayEnd) ? Number(selectedGamePlayEnd) : start;
  const inSelectedPlay = Number.isFinite(t)
    && Number.isFinite(start)
    && t >= start - 0.15
    && t <= Math.max(start, end) + 0.15;
  if (options.keepGamePlaySelection || selectedGamePlayPending) {
    if (!options.keepGamePlaySelection) {
      if (inSelectedPlay) selectedGamePlayPending = false;
    }
    return selectedGamePlayIndex;
  }
  if (inSelectedPlay) return selectedGamePlayIndex;
  clearGamePlaySelection();
  return -1;
}

function renderGameEventRail(clip = currentClip) {
  const rail = $("game-event-rail");
  const title = $("game-event-rail-title");
  const summary = $("game-event-rail-summary");
  const list = $("game-event-list");
  const presentation = pluginPresentationForClip(clip);
  const eventRail = eventRailPolicy(clip);
  const markers = clipMatchEventMarkers(clip);
  activeGameEventIndex = -1;
  clearGameEventSelection();
  if (!eventRail || !eventRail.enabled || !markers.length) {
    rail.hidden = true;
    rail.classList.remove("is-collapsed");
    syncReviewSideRailLayout();
    title.textContent = "";
    summary.textContent = "";
    list.replaceChildren();
    gameEventRows = [];
    return;
  }
  title.textContent = eventRail.title || (clip && clip.game ? `${clip.game.name} events` : "Game events");
  summary.textContent = markerSummary(markers);
  list.replaceChildren();
  gameEventRows = [];
  const playerSummary = clip && clip.markers ? clip.markers.player_summary : null;
  markers.forEach((marker, index) => {
    const item = document.createElement("li");
    const button = document.createElement("button");
    const view = gameEventRailItem(marker, playerSummary, presentation, {
      data_dragon: presentation && presentation.data_dragon,
    });
    button.type = "button";
    button.setAttribute("data-game-event-index", String(index));
    button.setAttribute("data-game-event-time", String(marker.t_s || 0));
    button.className = `marker-${view.category} game-event-row-${view.allegiance || "neutral"}`;
    const time = document.createElement("span");
    time.className = "game-event-time";
    time.textContent = fmtDur(marker.t_s || 0);
    button.title = markerDisplayLabel(marker, presentation);
    if (view.layout === "duel" && view.actor && view.victim) {
      button.classList.add("game-event-duel");
      button.append(
        time,
        gameEventPortrait(view.actor),
        gameEventIcon(view, marker, presentation),
        gameEventPortrait(view.victim),
      );
    } else if (view.layout === "actor_event") {
      const icon = gameEventIcon(view, marker, presentation);
      icon.classList.add("game-event-objective-icon");
      button.classList.add("game-event-actor-event");
      if (view.actor) {
        button.append(
          time,
          gameEventPortrait(view.actor),
          icon,
        );
      } else {
        const label = document.createElement("span");
        label.className = "game-event-label";
        label.textContent = view.text || markerEventText(marker, presentation);
        button.append(time, label, icon);
      }
    } else {
      const label = document.createElement("span");
      label.className = "game-event-label";
      label.textContent = view.text || markerEventText(marker, presentation);
      button.append(time, label);
    }
    button.addEventListener("click", () => {
      const markerTime = marker.t_s || 0;
      selectGameEvent(index, markerTime);
      seekTo(markerTime - MARKER_LEAD_S, { keepGameEventSelection: true });
      video.play().catch(() => syncPlayState());
    });
    gameEventRows.push(button);
    item.appendChild(button);
    list.appendChild(item);
  });
  rail.hidden = false;
  syncReviewSideRailLayout();
  setGameEventRailCollapsed(gameEventRailCollapsed);
}

function setGameEventRailCollapsed(collapsed) {
  gameEventRailCollapsed = Boolean(collapsed);
  const rail = $("game-event-rail");
  const toggle = $("game-event-rail-toggle");
  if (!rail) return;
  rail.classList.toggle("is-collapsed", gameEventRailCollapsed);
  syncReviewSideRailLayout();
  if (toggle) {
    const label = gameEventRailCollapsed ? "Expand match events" : "Collapse match events";
    toggle.title = label;
    toggle.setAttribute("aria-label", label);
    toggle.setAttribute("aria-expanded", gameEventRailCollapsed ? "false" : "true");
  }
  if (!gameEventRailCollapsed) {
    syncGameEventRail(video.currentTime || 0, { force: true });
  }
}

function syncGameEventRail(currentTime = video.currentTime || 0, options = {}) {
  const rail = $("game-event-rail");
  if (!rail || rail.hidden || rail.classList.contains("is-collapsed")) return;
  if (!gameEventRows.length) return;
  const markers = clipMatchEventMarkers();
  const selectedIndex = selectedGameEventIndexForTime(currentTime);
  const next = gameEventActiveIndex(markers, currentTime, selectedIndex);
  if (next === activeGameEventIndex && !options.force) return;
  activeGameEventIndex = next;
  gameEventRows.forEach((row) => {
    const active = Number(row.dataset.gameEventIndex) === next;
    row.classList.toggle("active", active);
    row.setAttribute("aria-current", active ? "true" : "false");
    if (active) row.scrollIntoView({ block: "nearest", inline: "nearest" });
  });
}

function renderGamePlayRail(clip = currentClip) {
  const rail = $("game-play-rail");
  const title = $("game-play-rail-title");
  const summary = $("game-play-rail-summary");
  const list = $("game-play-list");
  const playRail = playRailPolicy(clip);
  const plays = clipPlays(clip);
  activeGamePlayIndex = -1;
  clearGamePlaySelection();
  if (!rail || !title || !summary || !list) return;
  if (!playRail || !playRail.enabled || !plays.length) {
    rail.hidden = true;
    title.textContent = "Set plays";
    summary.textContent = "";
    list.replaceChildren();
    gamePlayRows = [];
    syncReviewSideRailLayout();
    return;
  }

  const duration = clip && Number.isFinite(clip.duration_s)
    ? clip.duration_s
    : (clip && clip.markers && Number.isFinite(clip.markers.duration_s) ? clip.markers.duration_s : 0);
  title.textContent = playRail.title || "Set plays";
  summary.textContent = playSummary(plays);
  list.replaceChildren();
  gamePlayRows = [];
  playBlocks(plays, duration).forEach((play, index) => {
    const view = playRailItem(play.play);
    const item = document.createElement("li");
    const button = document.createElement("button");
    button.type = "button";
    button.setAttribute("data-game-play-index", String(index));
    button.setAttribute("data-game-play-time", String(play.start || 0));
    button.title = [view.title, view.meta, view.time].filter(Boolean).join("\n");
    const thumbnail = document.createElement("span");
    thumbnail.className = "game-play-thumb";
    if (view.coverUrl) {
      const image = document.createElement("img");
      image.src = view.coverUrl;
      image.alt = "";
      image.loading = "lazy";
      image.decoding = "async";
      thumbnail.appendChild(image);
    } else {
      thumbnail.textContent = "osu!";
    }
    const text = document.createElement("span");
    text.className = "game-play-body";
    const playTitleEl = document.createElement("span");
    playTitleEl.className = "game-play-title";
    const song = document.createElement("span");
    song.className = "game-play-song";
    song.textContent = view.artistTitle || view.title;
    playTitleEl.appendChild(song);
    if (view.difficulty) {
      const difficulty = document.createElement("span");
      difficulty.className = "game-play-difficulty";
      difficulty.textContent = `[${view.difficulty}]`;
      playTitleEl.appendChild(difficulty);
    }
    if (view.mods) {
      const mods = document.createElement("span");
      mods.className = "game-play-mods";
      mods.textContent = view.mods;
      playTitleEl.appendChild(mods);
    }
    if (view.starRating) {
      const starRating = document.createElement("span");
      starRating.className = "game-play-stars";
      starRating.textContent = view.starRating;
      playTitleEl.appendChild(starRating);
    }
    const meta = document.createElement("span");
    meta.className = "game-play-meta";
    meta.textContent = view.meta || view.time;
    text.append(playTitleEl, meta);
    button.append(thumbnail, text);
    button.addEventListener("click", () => {
      selectGamePlay(index, play.start, play.end);
      seekTo(play.start, { keepGamePlaySelection: true });
      video.play().catch(() => syncPlayState());
    });
    button.addEventListener("contextmenu", (ev) => {
      const exportRange = playExportRange(play.play);
      showGamePlayContextMenu(ev, {
        title: view.artistTitle || view.title,
        range: exportRange ? { start: play.start, end: play.end } : null,
      });
    });
    gamePlayRows.push(button);
    item.appendChild(button);
    list.appendChild(item);
  });
  rail.hidden = false;
  syncReviewSideRailLayout();
  syncGamePlayRail(video.currentTime || 0, { force: true });
}

function syncGamePlayRail(currentTime = video.currentTime || 0, options = {}) {
  const rail = $("game-play-rail");
  if (!rail || rail.hidden) return;
  const selectedIndex = selectedGamePlayIndexForTime(currentTime, options);
  const next = playActiveIndex(clipPlays(), currentTime, selectedIndex);
  if (next === activeGamePlayIndex && !options.force) return;
  activeGamePlayIndex = next;
  gamePlayRows.forEach((row) => {
    const active = Number(row.dataset.gamePlayIndex) === next;
    row.classList.toggle("active", active);
    row.setAttribute("aria-current", active ? "true" : "false");
    if (active) row.scrollIntoView({ block: "nearest", inline: "nearest" });
  });
  document.querySelectorAll(".play-block").forEach((block) => {
    const active = Number(block.dataset.gamePlayIndex) === next;
    block.classList.toggle("active", active);
    block.setAttribute("aria-current", active ? "true" : "false");
  });
}

function metadataIconFallbackText(value) {
  const letters = String(value || "").match(/[A-Za-z0-9]/g) || [];
  return (letters.slice(0, 2).join("").toUpperCase() || "?").slice(0, 2);
}

function renderMetadataIcon(entry, className) {
  const icon = document.createElement("span");
  icon.className = className;
  icon.title = entry.value || "";
  icon.setAttribute("aria-label", entry.value || "Metadata icon");
  if (entry.asset) {
    const img = document.createElement("img");
    img.src = entry.asset;
    img.alt = entry.value || "";
    img.addEventListener("error", () => {
      img.remove();
      icon.textContent = metadataIconFallbackText(entry.value || entry.assetKey);
    }, { once: true });
    icon.appendChild(img);
  } else {
    icon.textContent = metadataIconFallbackText(entry.value || entry.assetKey);
  }
  return icon;
}

function renderMetadataIconList(field) {
  const list = document.createElement("div");
  list.className = `game-metadata-icons ${field.type}`;
  list.setAttribute("aria-label", field.label || field.type);
  for (const entry of field.items || []) {
    list.appendChild(renderMetadataIcon(entry, "game-metadata-icon"));
  }
  return list;
}

function renderGameMetadataPanel(clip = currentClip) {
  const panel = $("game-metadata-panel");
  const fieldsRoot = $("game-metadata-fields");
  if (!clip) {
    panel.hidden = true;
    fieldsRoot.replaceChildren();
    return;
  }
  const presentation = pluginPresentationForClip(clip);
  const metadataPanel = metadataPanelPolicy(clip);
  const summary = clip && clip.markers ? clip.markers.player_summary : null;
  const fields = metadataPanel && metadataPanel.fields
    ? playerSummaryFields(summary, metadataPanel.fields, {
      data_dragon: presentation && presentation.data_dragon,
    })
    : [];
  if (!metadataPanel || !metadataPanel.enabled || !fields.length) {
    panel.hidden = true;
    fieldsRoot.replaceChildren();
    return;
  }
  fieldsRoot.replaceChildren();
  for (const field of fields) {
    if (field.type === "portrait") {
      const portrait = document.createElement("div");
      portrait.className = "game-metadata-portrait";
      portrait.title = field.value;
      if (field.asset) {
        const img = document.createElement("img");
        img.src = field.asset;
        img.alt = field.value;
        img.addEventListener("error", () => {
          img.remove();
          portrait.textContent = String(field.value || "?").slice(0, 2).toUpperCase();
        }, { once: true });
        portrait.appendChild(img);
      } else {
        portrait.textContent = String(field.value || "?").slice(0, 2).toUpperCase();
      }
      fieldsRoot.appendChild(portrait);
      continue;
    }
    if (field.type === "summoner_spells" || field.type === "item_build") {
      fieldsRoot.appendChild(renderMetadataIconList(field));
      continue;
    }
    const item = document.createElement("div");
    item.className = `game-metadata-field ${field.type}`;
    if (field.label) {
      const label = document.createElement("strong");
      label.textContent = field.label;
      item.appendChild(label);
    }
    const value = document.createElement("span");
    value.textContent = field.value;
    item.appendChild(value);
    if (field.secondary) {
      const secondary = document.createElement("small");
      secondary.textContent = field.secondary;
      item.appendChild(secondary);
    }
    fieldsRoot.appendChild(item);
  }
  panel.hidden = false;
}

function clipGalleryCardPreview(clip, kind, fallbackTitle) {
  const presentation = pluginPresentationForClip(clip);
  return galleryCardPreview(
    clip,
    kind,
    fallbackTitle,
    presentation,
    { data_dragon: presentation && presentation.data_dragon },
  );
}

function cloudVisibilityEl(record) {
  const visibility = clipCloudVisibility(record);
  if (!visibility) return null;
  const el = document.createElement("span");
  el.className = `clip-cloud-visibility ${visibility}`;
  el.title = CLOUD_VISIBILITY_LABELS[visibility];
  el.setAttribute("aria-label", CLOUD_VISIBILITY_LABELS[visibility]);
  el.innerHTML = CLOUD_VISIBILITY_ICONS[visibility]; // static markup, safe
  return el;
}

const CLOUD_CARD_ICON =
  '<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M7.2 18h10.2a4.1 4.1 0 0 0 .4-8.2A6.2 6.2 0 0 0 5.9 8.1 5 5 0 0 0 7.2 18zm.2-2a3 3 0 0 1-.5-5.9l.8-.1.3-.8A4.2 4.2 0 0 1 16 10.4l.2 1.2 1.2.1A2.1 2.1 0 0 1 17.4 16H7.4z"/></svg>';

function cloudClipCard(entry) {
  const el = document.createElement("article");
  el.className = "card cloud-card";
  el.title = entry.title;

  const thumb = document.createElement("div");
  thumb.className = "card-thumb";
  thumb.style.cssText = thumbGradient({ name: entry.title, session: entry.remote_clip_id });
  const placeholder = document.createElement("span");
  placeholder.className = "cloud-card-placeholder";
  placeholder.innerHTML = CLOUD_CARD_ICON; // static markup, safe
  thumb.appendChild(placeholder);
  observeCloudThumbnail(entry, thumb);

  const play = document.createElement("div");
  play.className = "card-play";
  play.innerHTML = '<svg viewBox="0 0 24 24"><path d="M8 5v14l11-7z"/></svg>'; // static markup, safe

  const kindChip = document.createElement("span");
  kindChip.className = "card-kind session";
  kindChip.title = "Cloud clip";
  kindChip.innerHTML =
    '<svg viewBox="0 0 24 24"><path d="M7.2 18h10.2a4.1 4.1 0 0 0 .4-8.2A6.2 6.2 0 0 0 5.9 8.1 5 5 0 0 0 7.2 18z"/></svg>';
  const kindLabel = document.createElement("span");
  kindLabel.className = "card-kind-label";
  kindLabel.textContent = "Cloud";
  kindChip.appendChild(kindLabel);
  thumb.appendChild(kindChip);

  const meta = document.createElement("div");
  meta.className = "card-meta";
  const nameRow = document.createElement("div");
  nameRow.className = "card-name";
  const name = document.createElement("span");
  name.className = "t";
  name.textContent = entry.title;
  nameRow.appendChild(name);
  const visibility = cloudVisibilityEl(entry);
  if (visibility) nameRow.appendChild(visibility);

  const info = document.createElement("div");
  info.className = "card-sub";
  const updated = entry.updated_at_unix ? fmtAgo(Date.now() / 1000, entry.updated_at_unix) : "";
  const parts = [cloudStatusLabel(entry.upload_status)];
  if (updated) parts.push(updated);
  parts.push(entry.local_available ? "local copy available" : "cloud only");
  info.textContent = parts.join(" · ");

  const localState = document.createElement("div");
  localState.className = "cloud-local-state";
  localState.textContent = entry.remote_url || "No public share link";
  localState.title = entry.remote_url || "This clip is not publicly shareable.";

  meta.append(nameRow, info, localState);
  thumb.appendChild(play);
  el.append(thumb, meta);
  el.addEventListener("click", () => openCloudEntryInApp(entry));
  el.addEventListener("contextmenu", (ev) => showCloudClipContextMenu(ev, entry));
  return el;
}

/* ---- screenshot lightbox ---- */
// The Gallery owns navigation state (sorted list + current index) and
// overrides these hooks; these fallbacks keep the lightbox working when a
// single screenshot is opened outside the Gallery context.
var lightboxNavigation = null;

function openScreenshotLightbox(clip, navigation = null) {
  const dialog = document.getElementById("screenshot-lightbox");
  const img = document.getElementById("screenshot-lightbox-img");
  if (!dialog || !img) return;
  lightboxNavigation = navigation;
  img.src = convertFileSrc(clip.path);
  img.alt = clipDisplayTitle(clip) || clip.name;
  updateLightboxNav();
  dialog.showModal();
}

function updateLightboxNav() {
  const prev = document.getElementById("lightbox-prev");
  const next = document.getElementById("lightbox-next");
  const counter = document.getElementById("lightbox-counter");
  const nav = lightboxNavigation;
  if (!prev || !next || !counter || !nav) return;
  prev.disabled = nav.index <= 0;
  next.disabled = nav.index >= nav.items.length - 1;
  counter.textContent =
    nav.items.length > 1 ? `${nav.index + 1} / ${nav.items.length}` : "";
}

function stepScreenshotLightbox(delta) {
  const nav = lightboxNavigation;
  const dialog = document.getElementById("screenshot-lightbox");
  if (!nav || !dialog || !dialog.open) return;
  const nextIndex = nav.index + delta;
  if (nextIndex < 0 || nextIndex >= nav.items.length) return;
  nav.index = nextIndex;
  openScreenshotLightbox(nav.items[nextIndex], nav);
}
(function initScreenshotLightbox() {
  const dialog = document.getElementById("screenshot-lightbox");
  if (!dialog) return;
  // Clicking the dimmed backdrop closes; clicking the photo keeps it open.
  // Esc closes natively via the dialog cancel event.
  dialog.addEventListener("click", (ev) => {
    if (ev.target === dialog) dialog.close();
  });
  document.getElementById("lightbox-prev")?.addEventListener("click", () => {
    stepScreenshotLightbox(-1);
  });
  document.getElementById("lightbox-next")?.addEventListener("click", () => {
    stepScreenshotLightbox(1);
  });
  dialog.addEventListener("keydown", (ev) => {
    if (ev.key === "ArrowLeft") {
      ev.preventDefault();
      stepScreenshotLightbox(-1);
    } else if (ev.key === "ArrowRight") {
      ev.preventDefault();
      stepScreenshotLightbox(1);
    }
  });
})();

// Clip names come from disk; build rows with textContent, never innerHTML.
const CARD_KIND_LABELS = {
  replay: "Replay",
  session: "Session",
  trim: "Trim",
  screenshot: "Screenshot",
};
// Marker categories → tint var, matching the timeline glyph colors.
const MARKER_CATEGORY_TICK_VARS = {
  kill: "--mc-kill",
  assist: "--mc-assist",
  death: "--mc-death",
  spree: "--mc-spree",
  objective: "--mc-objective",
  structure: "--mc-structure",
  info: "--mc-info",
};
const MARKER_TICK_VARS = {
  ChampionKill: "--mc-kill", FirstBlood: "--mc-kill",
  ChampionAssist: "--mc-assist",
  ChampionDeath: "--mc-death",
  Multikill: "--mc-spree", Ace: "--mc-spree",
  DragonKill: "--mc-objective", HeraldKill: "--mc-objective", BaronKill: "--mc-objective",
  TurretKilled: "--mc-structure", InhibKilled: "--mc-structure", FirstBrick: "--mc-structure",
};

// Stable gradient placeholder per clip, shown until the poster loads (and the
// fallback if poster extraction fails).
function thumbGradient(c) {
  const key = (c.name || "") + (c.session || "");
  let h = 0;
  for (let i = 0; i < key.length; i++) h = (h * 31 + key.charCodeAt(i)) % 360;
  return `--g1:hsl(${h} 30% 18%); --g2:hsl(${(h + 38) % 360} 34% 8%);`;
}

function insertThumbMedia(thumb, media) {
  const firstOverlay = thumb.querySelector(".card-play, .card-kind, .card-dur, .card-markers, .card-del");
  thumb.insertBefore(media, firstOverlay || null);
}

function makePosterImg(url, onError = null) {
  const img = document.createElement("img");
  img.className = "card-thumb-img";
  img.src = url;
  img.alt = "";
  img.addEventListener("error", () => {
    img.remove();
    if (onError) onError();
  });
  return img;
}

function markPosterUnavailable(path) {
  posterCacheSet(path, POSTER_UNAVAILABLE);
}

function showPosterRuntimeWarning(error) {
  const warning = $("poster-runtime-warning");
  if (warning) warning.hidden = false;
  if (posterRuntimeWarningReported) return;
  posterRuntimeWarningReported = true;
  reportFrontendDiagnostic("warn", "poster_ffmpeg_unavailable", error);
}

function clearPosterRuntimeWarning() {
  const warning = $("poster-runtime-warning");
  if (warning) warning.hidden = true;
}

function retryUnavailablePosters() {
  for (const [key, value] of [...posterCache.entries()]) {
    if (
      value === POSTER_UNAVAILABLE
      && !String(key).startsWith(CLOUD_POSTER_CACHE_PREFIX)
    ) {
      posterCacheDelete(key);
    }
  }
  clearPosterRuntimeWarning();
  posterRuntimeWarningReported = false;
  renderClips();
}

// Lazily fetch + cache a clip's poster, then drop it into its card thumbnail.
// The backend caches the JPEG, so repeat calls are cheap after the first.
function loadCardPoster(path, thumb) {
  const lifecycleWork = captureForegroundWork();
  if (!lifecycleWork) return Promise.resolve();
  const cached = posterCacheGet(path);
  if (cached === POSTER_UNAVAILABLE) return Promise.resolve();
  if (cached) {
    if (thumb.isConnected && !thumb.querySelector(".card-thumb-img")) {
      insertThumbMedia(thumb, makePosterImg(cached, () => markPosterUnavailable(path)));
    }
    return Promise.resolve();
  }
  return invoke("clip_poster", { path })
    .then((posterPath) => {
      if (!isForegroundWorkCurrent(lifecycleWork)) return;
      if (!localClipPaths(clipsCache).has(GalleryWindowCore.clipPathKey(path))) return;
      if (!posterPath) {
        markPosterUnavailable(path);
        return;
      }
      const url = convertFileSrc(posterPath);
      posterCacheSet(path, url);
      if (thumb.isConnected && !thumb.querySelector(".card-thumb-img")) {
        insertThumbMedia(thumb, makePosterImg(url, () => markPosterUnavailable(path)));
      }
    })
    .catch((error) => {
      if (!isForegroundWorkCurrent(lifecycleWork)) return;
      markPosterUnavailable(path);
      if (GalleryWindowCore.posterRuntimeUnavailable(error)) {
        showPosterRuntimeWarning(error);
      }
    });
}

// Extracting a poster is an ffmpeg spawn, so we only request one once its card
// scrolls near the viewport — otherwise a library of hundreds of clips would
// queue an extraction for every clip on the first render and peg CPU/disk.
var posterQueue = new WeakMap();
var cloudThumbnailInflight = new Map();
var posterWorkQueue = [];
var posterWorkActive = 0;
var posterRenderGeneration = 0;
const POSTER_WORK_LIMIT = 2;

function pumpPosterWork() {
  while (posterWorkActive < POSTER_WORK_LIMIT && posterWorkQueue.length) {
    const job = posterWorkQueue.shift();
    if (
      job.generation !== posterRenderGeneration
      || !job.thumb.isConnected
      || !captureForegroundWork()
    ) {
      continue;
    }
    posterWorkActive += 1;
    const work = job.request.type === "local-poster"
      ? loadCardPoster(job.request.path, job.thumb)
      : loadCloudThumbnail(job.request.entry, job.thumb);
    Promise.resolve(work)
      .catch(() => {})
      .finally(() => {
        posterWorkActive -= 1;
        pumpPosterWork();
      });
  }
}

function queuePosterWork(request, thumb) {
  if (!request || !thumb || !thumb.isConnected) return;
  posterWorkQueue.push({
    request,
    thumb,
    generation: posterRenderGeneration,
  });
  pumpPosterWork();
}

var posterObserver =
  typeof IntersectionObserver === "function"
    ? new IntersectionObserver(
        (entries, obs) => {
          for (const entry of entries) {
            if (!entry.isIntersecting) continue;
            const thumb = entry.target;
            obs.unobserve(thumb);
            const request = posterQueue.get(thumb);
            posterQueue.delete(thumb);
            queuePosterWork(request, thumb);
          }
        },
        { rootMargin: "400px 0px" },
      )
    : null;

// Request a clip's poster when its thumbnail nears the viewport — or right away
// when IntersectionObserver is unavailable.
function observePoster(path, thumb) {
  if (!captureForegroundWork()) return;
  const request = { type: "local-poster", path };
  if (!posterObserver) {
    Promise.resolve().then(() => queuePosterWork(request, thumb));
    return;
  }
  posterQueue.set(thumb, request);
  posterObserver.observe(thumb);
}

function clipCard(c) {
  const el = document.createElement("article");
  const selected = selectedClipPaths.has(c.path);
  el.className = "card"
    + (currentClip && currentClip.path === c.path ? " active" : "")
    + (selected ? " selected" : "");
  el.dataset.clipPath = c.path;
  el.title = clipDisplayTitle(c) || c.name;
  const cloudRecord = clipCloudRecord(c);
  const uploadBusy = cloudRecord
    && ["queued", "uploading", "processing", "retrying"].includes(cloudRecord.upload_status);

  const kind = clipKind(c);
  const when = new Date(c.modified_unix * 1000);
  const markers = clipMarkers(c);
  const presentation = pluginPresentationForClip(c);
  const duration = Number.isFinite(c.duration_s)
    ? c.duration_s
    : (c.markers ? c.markers.duration_s : NaN);
  const fallbackTitle = formatClipTitle(
    when.getMonth(), when.getDate(), when.getHours(), when.getMinutes());
  const cardPreview = clipGalleryCardPreview(c, kind, fallbackTitle);
  const cardTitleUsesSummary = cardPreview.titleSource === "summary";
  const cardTitle = cardPreview.title || fallbackTitle;

  // Thumbnail: gradient placeholder + lazily-loaded poster, with the kind chip,
  // a hover delete, a play glyph, the duration, and marker ticks layered on.
  const thumb = document.createElement("div");
  thumb.className = "card-thumb";
  thumb.style.cssText = thumbGradient(c);
  // Cached and uncached posters share the same viewport gate so revisiting a
  // page does not immediately decode every image in that page.
  observePoster(c.path, thumb);

  const play = document.createElement("div");
  play.className = "card-play";
  play.innerHTML = '<svg viewBox="0 0 24 24"><path d="M8 5v14l11-7z"/></svg>'; // static markup, safe

  const kindChip = document.createElement("span");
  kindChip.className = "card-kind " + kind;
  kindChip.title = CLIP_KIND_LABELS[kind];
  kindChip.innerHTML = CLIP_KIND_ICONS[kind]; // static markup, safe
  const kindLabel = document.createElement("span");
  kindLabel.className = "card-kind-label";
  kindLabel.textContent = CARD_KIND_LABELS[kind];
  kindChip.appendChild(kindLabel);

  const del = document.createElement("button");
  del.className = "card-del";
  del.title = "Delete clip";
  // Static markup, no clip data — innerHTML is safe here.
  del.innerHTML =
    '<svg viewBox="0 0 24 24"><path d="M9 3v1H4v2h16V4h-5V3H9zM6 8v11a2 2 0 0 0 2 2h8a2 2 0 0 0 2-2V8H6zm3 2h2v9H9v-9zm4 0h2v9h-2v-9z"/></svg>';

  thumb.append(play, kindChip, del);

  if (Number.isFinite(duration)) {
    const dur = document.createElement("span");
    dur.className = "card-dur";
    dur.textContent = fmtDur(duration);
    thumb.appendChild(dur);
  }

  if (Number.isFinite(duration) && duration > 0 && markers.length) {
    const strip = document.createElement("div");
    strip.className = "card-markers";
    for (const m of markers) {
      const tick = document.createElement("i");
      tick.style.left = Math.max(0, Math.min(100, (m.t_s / duration) * 100)) + "%";
      const style = markerStyle(m.kind, presentation);
      const tint = MARKER_CATEGORY_TICK_VARS[style.cls] || MARKER_TICK_VARS[m.kind];
      if (tint) tick.style.setProperty("--mc", `var(${tint})`);
      strip.appendChild(tick);
    }
    thumb.appendChild(strip);
  }

  const meta = document.createElement("div");
  meta.className = "card-meta";
  const nameRow = document.createElement("div");
  nameRow.className = "card-name";
  const previewIcon = cardPreview.icon && cardPreview.icon.url ? cardPreview.icon : null;
  const game = clipGameIcon(c);
  const cardIcon = previewIcon || (game ? { type: "game", url: game.url, label: game.label } : null);
  if (cardIcon) {
    const gi = document.createElement("img");
    gi.className = "card-game-ico" + (cardIcon.type === "portrait" ? " portrait" : "");
    gi.src = cardIcon.url;
    gi.alt = "";
    gi.title = cardIcon.label || (game ? game.label : "");
    // Fall back to a neutral glyph if the icon can't load.
    gi.addEventListener("error", () => {
      if (previewIcon && game && gi.src !== game.url) {
        gi.className = "card-game-ico";
        gi.src = game.url;
        gi.title = game.label;
        return;
      }
      const ph = document.createElement("div");
      ph.className = "card-game-ico placeholder";
      ph.title = cardIcon.label || (game ? game.label : "");
      ph.innerHTML = GENERIC_GAME_ICON; // static markup, safe
      gi.replaceWith(ph);
    });
    nameRow.appendChild(gi);
  }
  const name = document.createElement("span");
  name.className = "t";
  name.textContent = cardTitle;
  nameRow.appendChild(name);
  if (uploadBusy) {
    const spinner = document.createElement("span");
    spinner.className = "clip-upload-spinner";
    spinner.title = "Uploading clip";
    nameRow.appendChild(spinner);
  }
  const cloudVisibility = cloudVisibilityEl(cloudRecord);
  if (cloudVisibility) nameRow.appendChild(cloudVisibility);

  const info = document.createElement("div");
  info.className = "card-sub";
  const digest = markerDigest(markers, presentation);
  const infoParts = [];
  if (c.game && c.game.queue && c.game.queue.label) infoParts.push(c.game.queue.label);
  if (Number.isFinite(c.duration_s)) infoParts.push(fmtDur(c.duration_s));
  infoParts.push(fmtMegabytes(c.size_mb));
  infoParts.push(fmtAgo(Date.now() / 1000, c.modified_unix));
  if (!cardPreview.summary && digest) infoParts.push(digest);
  info.textContent = infoParts.join(" · ");

  meta.append(nameRow, info);
  if (cardPreview.summary && !cardTitleUsesSummary) {
    const detail = document.createElement("div");
    detail.className = "game-meta";
    detail.textContent = cardPreview.summary;
    meta.appendChild(detail);
  }

  el.append(thumb, meta);

  // Clicking the open clip's card again closes it (back to the gallery).
  el.addEventListener("click", () => {
    if (selectMode && gallerySource === "local") {
      toggleClipSelection(c.path);
      return;
    }
    if (currentClip && currentClip.path === c.path) closeReview();
    else if (kind === "screenshot") openScreenshotLightbox(c);
    else openClip(c);
  });
  el.addEventListener("contextmenu", (ev) => showClipContextMenu(ev, c));
  del.addEventListener("click", (ev) => {
    ev.stopPropagation();
    deleteClip(c.path);
  });

  return el;
}

/* ---- gallery: multi-select + bulk actions ---- */

// Update a single card's selection UI without a full re-render. Windows
// backslashes make `[data-clip-path="..."]` fragile as a CSS selector, so
// iterate the cards and match `dataset.clipPath` in JS instead.
function applySelectionToCard(card, on) {
  card.classList.toggle("selected", on);
}

function findClipCard(path) {
  for (const card of document.querySelectorAll("#gallery-grid .card[data-clip-path]")) {
    if (card.dataset.clipPath === path) return card;
  }
  return null;
}

function toggleClipSelection(path) {
  const on = selectedClipPaths.has(path);
  if (on) selectedClipPaths.delete(path);
  else selectedClipPaths.add(path);
  const card = findClipCard(path);
  if (card) applySelectionToCard(card, !on);
  syncBulkBar();
}

function selectClipFromContext(path) {
  if (!path || gallerySource !== "local") return;
  selectMode = true;
  selectedClipPaths.add(path);
  const card = findClipCard(path);
  if (card) applySelectionToCard(card, true);
  syncSelectionControls();
}

function clearSelection() {
  selectedClipPaths.clear();
  for (const card of document.querySelectorAll("#gallery-grid .card[data-clip-path]")) {
    applySelectionToCard(card, false);
  }
  syncBulkBar();
}

function selectAllVisible() {
  selectedClipPaths = new Set(selectedClipPaths);
  for (const card of document.querySelectorAll("#gallery-grid .card[data-clip-path]")) {
    if (!card.dataset.clipPath) continue;
    selectedClipPaths.add(card.dataset.clipPath);
    applySelectionToCard(card, true);
  }
  syncBulkBar();
}

function exitSelectMode() {
  selectMode = false;
  clearSelection();
  syncSelectionControls();
}

function syncSelectToggleLabel() {
  const toggle = $("gallery-select-toggle");
  if (toggle) toggle.textContent = selectMode ? "Done" : "Select multiple";
}

function syncSelectionControls() {
  if (gallerySource !== "local" && selectMode) {
    selectMode = false;
    clearSelection();
  }
  const toggle = $("gallery-select-toggle");
  if (toggle) {
    toggle.hidden = gallerySource !== "local";
    toggle.classList.toggle("active", selectMode);
    syncSelectToggleLabel();
  }
  const grid = $("gallery-grid");
  if (grid) grid.classList.toggle("select-mode", selectMode && gallerySource === "local");
  syncBulkBar();
}

function syncBulkBar() {
  const bar = $("gallery-bulk-bar");
  if (!bar) return;
  const count = selectedClipPaths.size;
  const visible = (selectMode || count > 0) && gallerySource === "local";
  bar.hidden = !visible;
  $("bulk-count").textContent = `${count} selected`;
  const del = $("bulk-delete");
  if (del) del.disabled = count === 0;
}

/* ---- gallery: filter / sort / group ---- */

function syncLeagueGameTypeFilter() {
  const present = [];
  const seen = new Set();
  for (const c of clipsCache) {
    if (!c.game || c.game.id !== "league_of_legends") continue;
    const category = c.game.queue && c.game.queue.category ? c.game.queue.category : "unknown";
    if (seen.has(category)) continue;
    seen.add(category);
    present.push(category);
  }
  leagueGameTypePresent = present;
  const optionsKey = present.slice().sort().join("|");
  leagueGameTypeOptionsKey = optionsKey;
  // Empty `present` still clears: a Replay chip with no League clips left
  // would otherwise hide every remaining non-League card.
  if (galleryGameType !== "all" && !seen.has(galleryGameType)) {
    clearGallerySearchToken({ render: false });
  }
  renderGallerySearchToken();
}

function renderGallerySearchToken() {
  const root = $("gallery-search-tokens");
  if (!root) return;
  root.replaceChildren();
  if (!gallerySearchToken) {
    root.hidden = true;
    return;
  }
  const chip = document.createElement("span");
  chip.className = "gallery-search-chip";
  const label = document.createElement("span");
  label.textContent = GallerySearchCore.chipText(gallerySearchToken.key, gallerySearchToken.value);
  const clear = document.createElement("button");
  clear.type = "button";
  clear.className = "gallery-search-chip-clear";
  clear.title = "Clear filter";
  clear.setAttribute("aria-label", "Clear LoL Type filter");
  clear.textContent = "×";
  clear.addEventListener("click", (ev) => {
    ev.preventDefault();
    ev.stopPropagation();
    clearGallerySearchToken({ render: true });
    $("gallery-search").focus();
    updateGallerySearchMenu();
  });
  chip.append(label, clear);
  root.appendChild(chip);
  root.hidden = false;
}

function clearGallerySearchToken({ render = true } = {}) {
  gallerySearchToken = null;
  galleryGameType = "all";
  renderGallerySearchToken();
  if (render) renderClips();
}

function applyGallerySearchToken(key, value, { clearInput = true } = {}) {
  gallerySearchToken = { key, value: value || null };
  galleryGameType = value || "all";
  const input = $("gallery-search");
  if (clearInput && input) input.value = "";
  gallerySearch = input ? input.value.trim().toLowerCase() : "";
  renderGallerySearchToken();
  if (value) hideGallerySearchMenu();
  renderClips();
}

function hideGallerySearchMenu() {
  const menu = $("gallery-search-menu");
  if (!menu) return;
  menu.hidden = true;
  menu.replaceChildren();
  gallerySearchMenuIndex = 0;
}

function gallerySearchMenuItems() {
  return [...document.querySelectorAll("#gallery-search-menu .gallery-search-option")];
}

function highlightGallerySearchMenu(index) {
  const items = gallerySearchMenuItems();
  if (!items.length) return;
  gallerySearchMenuIndex = ((index % items.length) + items.length) % items.length;
  items.forEach((item, i) => item.classList.toggle("is-active", i === gallerySearchMenuIndex));
}

function updateGallerySearchMenu() {
  const menu = $("gallery-search-menu");
  const input = $("gallery-search");
  if (!menu || !input || gallerySource === "cloud") {
    hideGallerySearchMenu();
    return;
  }
  if (gallerySearchToken && gallerySearchToken.value) {
    hideGallerySearchMenu();
    return;
  }
  if (document.activeElement !== input) {
    hideGallerySearchMenu();
    return;
  }

  const pendingKey = gallerySearchToken && !gallerySearchToken.value ? gallerySearchToken.key : null;
  const inspected = GallerySearchCore.inspect(input.value);
  let mode = "none";
  let filterKey = pendingKey;
  let valueDraft = input.value;
  if (pendingKey) {
    mode = "values";
  } else if (inspected.kind === "empty" || inspected.kind === "filters") {
    mode = "filters";
  } else if (inspected.kind === "values") {
    mode = "values";
    filterKey = inspected.filterKey;
    valueDraft = inspected.valueDraft;
  }

  menu.replaceChildren();
  if (mode === "filters") {
    const filters = GallerySearchCore.matchingFilters(input.value);
    if (!filters.length) {
      hideGallerySearchMenu();
      return;
    }
    const head = document.createElement("div");
    head.className = "gallery-search-menu-head";
    head.textContent = "Filters";
    menu.appendChild(head);
    for (const filter of filters) {
      const button = document.createElement("button");
      button.type = "button";
      button.className = "gallery-search-option";
      button.dataset.searchAction = "filter";
      button.dataset.filterKey = filter.key;
      const title = document.createElement("strong");
      title.textContent = `${filter.chipLabel}:`;
      const hint = document.createElement("span");
      hint.textContent = filter.hint;
      button.append(title, hint);
      menu.appendChild(button);
    }
  } else if (mode === "values") {
    const values = GallerySearchCore.matchingValues(filterKey, valueDraft, leagueGameTypePresent);
    if (!values.length) {
      hideGallerySearchMenu();
      return;
    }
    const filter = GallerySearchCore.filterByKey(filterKey);
    const head = document.createElement("div");
    head.className = "gallery-search-menu-head";
    head.textContent = filter ? filter.chipLabel : "Filter";
    menu.appendChild(head);
    for (const item of values) {
      const button = document.createElement("button");
      button.type = "button";
      button.className = "gallery-search-option";
      button.dataset.searchAction = "value";
      button.dataset.filterKey = filterKey;
      button.dataset.filterValue = item.value;
      const title = document.createElement("strong");
      title.textContent = item.label;
      button.appendChild(title);
      menu.appendChild(button);
    }
  } else {
    hideGallerySearchMenu();
    return;
  }
  menu.hidden = false;
  highlightGallerySearchMenu(0);
}

function activateGallerySearchMenuItem(button) {
  if (!button) return;
  if (button.dataset.searchAction === "filter") {
    applyGallerySearchToken(button.dataset.filterKey, null);
    $("gallery-search").focus();
    updateGallerySearchMenu();
    return;
  }
  if (button.dataset.searchAction === "value") {
    applyGallerySearchToken(button.dataset.filterKey, button.dataset.filterValue);
  }
}

function onGallerySearchInput() {
  const input = $("gallery-search");
  if (gallerySource === "cloud") {
    gallerySearch = input.value.trim().toLowerCase();
    hideGallerySearchMenu();
    renderClips();
    return;
  }
  if (gallerySearchToken && gallerySearchToken.value) {
    gallerySearch = input.value.trim().toLowerCase();
    hideGallerySearchMenu();
    renderClips();
    return;
  }
  if (gallerySearchToken && !gallerySearchToken.value) {
    gallerySearch = "";
    updateGallerySearchMenu();
    return;
  }
  const inspected = GallerySearchCore.inspect(input.value);
  if (inspected.kind === "values" && inspected.filterKey) {
    applyGallerySearchToken(inspected.filterKey, null, { clearInput: true });
    input.value = inspected.valueDraft;
    gallerySearch = "";
    const exact = GallerySearchCore.matchingValues(
      inspected.filterKey,
      inspected.valueDraft,
      leagueGameTypePresent,
    );
    if (exact.length === 1 && inspected.valueDraft.trim()) {
      const draft = inspected.valueDraft.trim().toLowerCase();
      if (draft === exact[0].label.toLowerCase() || draft === exact[0].value) {
        applyGallerySearchToken(inspected.filterKey, exact[0].value);
        return;
      }
    }
    updateGallerySearchMenu();
    return;
  }
  gallerySearch = inspected.kind === "query" ? inspected.remainder.trim().toLowerCase() : "";
  updateGallerySearchMenu();
  renderClips();
}

function onGallerySearchKeydown(ev) {
  const menu = $("gallery-search-menu");
  const items = gallerySearchMenuItems();
  if (ev.key === "ArrowDown" && menu && !menu.hidden && items.length) {
    ev.preventDefault();
    highlightGallerySearchMenu(gallerySearchMenuIndex + 1);
    return;
  }
  if (ev.key === "ArrowUp" && menu && !menu.hidden && items.length) {
    ev.preventDefault();
    highlightGallerySearchMenu(gallerySearchMenuIndex - 1);
    return;
  }
  if (ev.key === "Enter" && menu && !menu.hidden && items.length) {
    ev.preventDefault();
    activateGallerySearchMenuItem(items[gallerySearchMenuIndex]);
    return;
  }
  if (ev.key === "Escape") {
    if (menu && !menu.hidden) {
      ev.preventDefault();
      hideGallerySearchMenu();
      return;
    }
    if (gallerySearchToken) {
      ev.preventDefault();
      clearGallerySearchToken({ render: true });
    }
    return;
  }
  if (ev.key === "Backspace" && $("gallery-search").value === "" && gallerySearchToken) {
    ev.preventDefault();
    if (gallerySearchToken.value) {
      applyGallerySearchToken(gallerySearchToken.key, null);
    } else {
      clearGallerySearchToken({ render: true });
      updateGallerySearchMenu();
    }
  }
}

function filterGalleryClips(clips) {
  const items = [];
  let maxModifiedUnix = 0;
  for (const c of clips) {
    const kind = clipKind(c);
    // Screenshots live in their own Gallery view, never the clip Library.
    if (kind === "screenshot") continue;
    if ((galleryFilter === "replay" || galleryFilter === "session" || galleryFilter === "trim")
      && kind !== galleryFilter) continue;
    if (galleryFilter === "marked" && !clipMarkers(c).length) continue;
    if (galleryGameType !== "all") {
      const category = c.game && c.game.id === "league_of_legends"
        ? (c.game.queue && c.game.queue.category || "unknown")
        : null;
      if (category !== galleryGameType) continue;
    }
    if (gallerySearch) {
      const champ = c.markers && c.markers.player_summary ? c.markers.player_summary.champion_name : "";
      const queue = c.game && c.game.queue ? c.game.queue.label : "";
      const hay = `${clipDisplayTitle(c)} ${c.name} ${champ} ${c.session || ""} ${c.game ? c.game.name : ""} ${queue}`.toLowerCase();
      if (!hay.includes(gallerySearch)) continue;
    }
    items.push(c);
    const modifiedUnix = Number(c && c.modified_unix);
    if (Number.isFinite(modifiedUnix)) {
      maxModifiedUnix = Math.max(maxModifiedUnix, modifiedUnix);
    }
  }
  return { items, maxModifiedUnix };
}

function sortGalleryClips(clips) {
  const out = clips.slice();
  const markerCount = (c) => clipMarkers(c).length;
  if (gallerySort === "old") out.sort((a, b) => a.modified_unix - b.modified_unix);
  else if (gallerySort === "big") out.sort((a, b) => b.size_mb - a.size_mb);
  else if (gallerySort === "marks") out.sort((a, b) => markerCount(b) - markerCount(a));
  else out.sort((a, b) => b.modified_unix - a.modified_unix);
  return out;
}

// Bucket clips by an arbitrary key. Clips keep the caller's incoming order
// (already sorted by the chosen gallery sort), so Largest / Most markers
// survives inside each group; only the group order is by recency. A
// null-prototype map keeps game names like "constructor" or "__proto__" from
// colliding with inherited Object properties and skipping bucket creation.
function bucketGroups(clips, keyFor, labelFor) {
  const order = [];
  const by = Object.create(null);
  for (const c of clips) {
    const key = keyFor(c);
    if (!by[key]) { by[key] = { label: labelFor(c), t: 0, clips: [] }; order.push(key); }
    by[key].clips.push(c);
    by[key].t = Math.max(by[key].t, c.modified_unix);
  }
  const groups = order.map((k) => by[k]);
  groups.sort((a, b) => b.t - a.t);
  return groups;
}

function galleryDayGroups(clips) {
  return bucketGroups(
    clips,
    (c) => { const d = new Date(c.modified_unix * 1000); return `${d.getFullYear()}-${d.getMonth()}-${d.getDate()}`; },
    (c) => PresentationCore.formatGalleryDay(new Date(c.modified_unix * 1000)),
  );
}

function galleryGameGroups(clips) {
  const label = (c) => (c.game && c.game.name ? c.game.name : "No game detected");
  return bucketGroups(clips, label, label);
}

// Relative-date buckets (Photos-style); only non-empty buckets are returned.
function gallerySmartGroups(clips) {
  const sod = new Date();
  sod.setHours(0, 0, 0, 0);
  const todayStart = sod.getTime() / 1000;
  const defs = [
    { label: "Today", test: (t) => t >= todayStart },
    { label: "Yesterday", test: (t) => t >= todayStart - 86400 && t < todayStart },
    { label: "Earlier this week", test: (t) => t >= todayStart - 7 * 86400 && t < todayStart - 86400 },
    { label: "Earlier", test: () => true },
  ];
  const out = defs.map((d) => ({ label: d.label, test: d.test, clips: [] }));
  // Clips keep the incoming gallery-sort order; only the buckets themselves
  // carry a fixed Today → Earlier reading order.
  for (const c of clips) out.find((o) => o.test(c.modified_unix)).clips.push(c);
  return out.filter((g) => g.clips.length).map(({ label, clips }) => ({ label, clips }));
}

function galleryGroups(clips) {
  switch (galleryGroup) {
    case "session": return sessionGroups(clips);
    case "day": return galleryDayGroups(clips);
    case "game": return galleryGameGroups(clips);
    case "none": return [{ label: null, clips: clips.slice() }];
    default: return gallerySmartGroups(clips);
  }
}

function galleryIdentityKey(item) {
  return String(
    item && (item.path || item.remote_clip_id || item.name) || "",
  );
}

function groupedGalleryIdentity(
  prefix,
  total,
  firstItem,
  lastItem,
  maxModifiedUnix,
) {
  const modified = Number(maxModifiedUnix);
  return JSON.stringify([
    prefix,
    total,
    galleryIdentityKey(firstItem),
    galleryIdentityKey(lastItem),
    Number.isFinite(modified) ? modified : 0,
  ]);
}

function syncGalleryPagination(info) {
  const pagination = $("gallery-pagination");
  if (!pagination) return;
  const page = info || {
    page: 0,
    pageCount: 0,
    total: 0,
    start: 0,
    end: 0,
    hasPrevious: false,
    hasNext: false,
  };
  galleryPageTotal = page.total;
  pagination.hidden = page.pageCount <= 1;
  $("gallery-page-prev").disabled = !page.hasPrevious;
  $("gallery-page-next").disabled = !page.hasNext;
  $("gallery-page-label").textContent = page.pageCount
    ? `${page.start + 1}\u2013${page.end} of ${page.total} \u00b7 page ${page.page + 1} of ${page.pageCount}`
    : "";
}

function changeGalleryPage(delta) {
  const requested = galleryPageState.page + delta;
  const next = GalleryWindowCore.setPage(
    galleryPageState,
    requested,
    galleryPageTotal,
  );
  if (next.page === galleryPageState.page) return;
  galleryPageState = next;
  renderClips();
  const root = gallerySource === "cloud"
    ? $("cloud-gallery-grid")
    : $("gallery-grid");
  if (root) root.scrollTop = 0;
}

function releaseGalleryRoot(root) {
  if (!root) return;
  root.querySelectorAll("img").forEach((img) => {
    img.removeAttribute("src");
  });
  root.replaceChildren();
}

function beginBoundedGalleryRender() {
  if (posterObserver) posterObserver.disconnect();
  posterRenderGeneration += 1;
  posterQueue = new WeakMap();
  posterWorkQueue = [];
  releaseGalleryRoot($("gallery-grid"));
  releaseGalleryRoot($("cloud-gallery-grid"));
}

// Re-render an open screenshots Gallery AFTER the Library's render pass, so
// its cards queue posters at the newest generation instead of being skipped
// by it (renderClips bumps the generation and drops older queued jobs).
function renderScreenshotsAfterLibraryPass() {
  if (window.__renderScreenshots && window.__screenshotsGalleryActive?.()) {
    window.__renderScreenshots();
  }
}

function renderClips() {
  if (!captureForegroundWork()) {
    requestWindowRefresh();
    return;
  }
  syncUploadClipButton();
  syncReviewLocalActions();
  // Keep the home in sync: empty library shows the capture preview, otherwise
  // the gallery. (Editor/settings arbitration lives in updateViews.)
  updateViews();
  const root = $("gallery-grid");
  const cloudRoot = $("cloud-gallery-grid");
  if (!root) return;
  const showingCloud = gallerySource === "cloud";
  root.hidden = showingCloud;
  if (cloudRoot) cloudRoot.hidden = !showingCloud;
  $("gallery-filter").hidden = showingCloud;
  $("gallery-group").hidden = showingCloud;
  $("gallery-sort").hidden = showingCloud;
  syncSelectionControls();
  document.querySelectorAll("#gallery-source-tabs .source-tab").forEach((tab) => {
    tab.classList.toggle("active", tab.dataset.gallerySource === gallerySource);
  });
  if (showingCloud) {
    renderCloudClips();
    loadCloudClips();
    return;
  }
  syncLeagueGameTypeFilter();
  beginBoundedGalleryRender();
  pruneLocalPosterCache(clipsCache);
  const filteredResult = filterGalleryClips(clipsCache);
  const filtered = filteredResult.items;
  const sorted = sortGalleryClips(filtered);
  const groups = galleryGroups(sorted);
  const firstGroup = groups[0];
  const lastGroup = groups[groups.length - 1];
  const firstItems = firstGroup && firstGroup.clips || [];
  const lastItems = lastGroup && lastGroup.clips || [];
  const identity = groupedGalleryIdentity(
    `local|${galleryFilter}|${galleryGameType}|${gallerySort}|${galleryGroup}|${gallerySearch}`,
    filtered.length,
    firstItems[0],
    lastItems[lastItems.length - 1],
    filteredResult.maxModifiedUnix,
  );
  galleryPageState = GalleryWindowCore.updateState(galleryPageState, {
    identity,
    total: filtered.length,
  });
  const page = GalleryWindowCore.windowGroups(groups, galleryPageState);
  syncGalleryPagination(page);
  $("gallery-count").textContent = clipsCache.length
    ? `${filtered.length} of ${clipsCache.length}`
    : "";
  if (!clipsCache.length) {
    const empty = document.createElement("div");
    empty.className = "gallery-empty";
    empty.textContent = `No clips yet - press ${saveHotkeyLabel()} while something plays.`;
    root.appendChild(empty);
    return;
  }
  if (!filtered.length) {
    const empty = document.createElement("div");
    empty.className = "gallery-empty";
    empty.textContent = "No clips match those filters.";
    root.appendChild(empty);
    return;
  }
  for (const group of page.groups) {
    if (group.label !== null) {
      const head = document.createElement("div");
      head.className = "gallery-group-head";
      const label = document.createElement("span");
      label.textContent = group.label;
      const count = document.createElement("span");
      count.className = "gcount";
      count.textContent = group.totalCount;
      head.append(label, count);
      root.appendChild(head);
    }
    for (const c of group.items) root.appendChild(clipCard(c));
  }
}

function renderCloudClips() {
  if (!captureForegroundWork()) {
    requestWindowRefresh();
    return;
  }
  const root = $("cloud-gallery-grid");
  if (!root) return;
  beginBoundedGalleryRender();
  const entries = cloudLibraryRecords();
  const filtered = [];
  let maxModifiedUnix = 0;
  for (const entry of entries) {
    if (!cloudEntryMatchesSearch(entry)) continue;
    filtered.push(entry);
    const modifiedUnix = Number(entry && (entry.updated_at_unix || entry.modified_unix));
    if (Number.isFinite(modifiedUnix)) {
      maxModifiedUnix = Math.max(maxModifiedUnix, modifiedUnix);
    }
  }
  const groups = [{ label: null, clips: filtered }];
  const identity = groupedGalleryIdentity(
    `cloud|${cloudAccountKey()}|${gallerySearch}`,
    filtered.length,
    filtered[0],
    filtered[filtered.length - 1],
    maxModifiedUnix,
  );
  galleryPageState = GalleryWindowCore.updateState(galleryPageState, {
    identity,
    total: filtered.length,
  });
  const page = GalleryWindowCore.windowItems(filtered, galleryPageState);
  syncGalleryPagination(page);
  $("gallery-count").textContent = entries.length
    ? `${filtered.length} of ${entries.length}`
    : "";
  if (cloudClipsLoading && !entries.length) {
    const empty = document.createElement("div");
    empty.className = "gallery-empty";
    empty.textContent = "Loading cloud clips...";
    root.appendChild(empty);
    return;
  }
  if (cloudClipsError) {
    const error = document.createElement("div");
    error.className = "gallery-empty cloud-error";
    error.textContent = cloudClipsError;
    root.appendChild(error);
    if (!entries.length) return;
  }
  if (!entries.length) {
    const empty = document.createElement("div");
    empty.className = "gallery-empty";
    empty.textContent = cloudConnected() ? "No cloud clips yet." : "Not connected to Clipline Cloud.";
    root.appendChild(empty);
    return;
  }
  if (!filtered.length) {
    const empty = document.createElement("div");
    empty.className = "gallery-empty";
    empty.textContent = "No cloud clips match that search.";
    root.appendChild(empty);
    return;
  }
  for (const entry of page.items) {
    const localClip = cloudLocalClipForEntry(entry);
    root.appendChild(localClip ? clipCard(localClip) : cloudClipCard(entry));
  }
}

function clearHeavyGalleryDom() {
  if (posterObserver) posterObserver.disconnect();
  posterRenderGeneration += 1;
  posterQueue = new WeakMap();
  posterWorkQueue = [];
  cloudThumbnailInflight.clear();
  for (const id of ["gallery-grid", "cloud-gallery-grid"]) {
    releaseGalleryRoot($(id));
  }
  syncGalleryPagination(null);
}

$("gallery-page-prev").addEventListener("click", () => changeGalleryPage(-1));
$("gallery-page-next").addEventListener("click", () => changeGalleryPage(1));

function showClipContextMenu(ev, clip) {
  ev.preventDefault();
  ev.stopPropagation();
  hideRegionMenu();
  clipContextTarget = clip;
  cloudContextTarget = null;
  gamePlayContextTarget = null;
  const record = clipCloudRecord(clip);
  const busy = record && ["queued", "uploading", "processing", "retrying"].includes(record.upload_status);
  const uploaded = cloudRecordUploaded(record);
  const shareable = !!cloudShareUrl(record);
  $("clip-menu-select").hidden = false;
  $("clip-menu-play").hidden = true;
  $("clip-menu-open-cloud-page").hidden = true;
  $("clip-menu-copy-cloud-link").hidden = true;
  $("clip-menu-export-play").hidden = true;
  $("clip-menu-copy").hidden = false;
  $("clip-menu-copy-shareable").hidden = false;
  const upload = $("clip-menu-upload");
  upload.hidden = !cloudUploadControlVisible(uploaded);
  upload.textContent = shareable ? "Copy cloud link" : uploaded ? "Open cloud page" : "Upload";
  upload.disabled = busy || (uploaded ? !record.remote_clip_id : !cloudConnected());
  $("clip-menu-rename").hidden = false;
  $("clip-menu-rename-file").hidden = false;
  $("clip-menu-show-folder").hidden = false;
  $("clip-menu-delete").hidden = false;
  const menu = $("clip-context-menu");
  menu.hidden = false;
  positionContextMenu(menu, ev.clientX, ev.clientY);
}

function showCloudClipContextMenu(ev, entry) {
  ev.preventDefault();
  ev.stopPropagation();
  hideRegionMenu();
  clipContextTarget = null;
  cloudContextTarget = entry;
  gamePlayContextTarget = null;
  $("clip-menu-select").hidden = true;
  $("clip-menu-play").hidden = false;
  $("clip-menu-play").disabled = false;
  $("clip-menu-open-cloud-page").hidden = false;
  $("clip-menu-open-cloud-page").disabled = !entry.remote_clip_id;
  $("clip-menu-copy-cloud-link").hidden = !cloudShareUrl(entry);
  $("clip-menu-copy-cloud-link").disabled = !cloudShareUrl(entry);
  $("clip-menu-export-play").hidden = true;
  $("clip-menu-copy").hidden = true;
  $("clip-menu-copy-shareable").hidden = true;
  $("clip-menu-upload").hidden = true;
  $("clip-menu-rename").hidden = true;
  $("clip-menu-rename-file").hidden = true;
  $("clip-menu-delete").hidden = true;
  const menu = $("clip-context-menu");
  menu.hidden = false;
  positionContextMenu(menu, ev.clientX, ev.clientY);
}

function showGamePlayContextMenu(ev, play) {
  ev.preventDefault();
  ev.stopPropagation();
  hideRegionMenu();
  clipContextTarget = currentClip;
  cloudContextTarget = null;
  gamePlayContextTarget = play;
  $("clip-menu-select").hidden = true;
  $("clip-menu-play").hidden = true;
  $("clip-menu-open-cloud-page").hidden = true;
  $("clip-menu-copy-cloud-link").hidden = true;
  const exportPlay = $("clip-menu-export-play");
  exportPlay.hidden = false;
  exportPlay.disabled = !play || !play.range;
  exportPlay.textContent = "Export play as clip";
  $("clip-menu-copy").hidden = true;
  $("clip-menu-copy-shareable").hidden = true;
  $("clip-menu-upload").hidden = true;
  $("clip-menu-rename").hidden = true;
  $("clip-menu-rename-file").hidden = true;
  $("clip-menu-show-folder").hidden = true;
  $("clip-menu-delete").hidden = true;
  const menu = $("clip-context-menu");
  menu.hidden = false;
  positionContextMenu(menu, ev.clientX, ev.clientY);
}

function hideClipContextMenu() {
  const menu = $("clip-context-menu");
  if (menu) menu.hidden = true;
  clipContextTarget = null;
  cloudContextTarget = null;
  gamePlayContextTarget = null;
}

function clipContextRecord() {
  return clipContextTarget ? clipCloudRecord(clipContextTarget) : null;
}
async function bulkDeleteSelected() {
  const paths = [...selectedClipPaths];
  if (!paths.length) return;
  if (!(await confirmBulkDelete(paths.length))) return;
  try {
    const report = await invoke("delete_clips", { paths });
    await applyDeletion(report.deleted);
    const notice = deletionNotice(report.deleted.length);
    if (notice) setNotice(notice, { transient: true });
    $("error").textContent = formatDeletionFailures(report.failed);
    if (report.deleted.length > 0) exitSelectMode();
    else clearSelection();
  } catch (e) {
    $("error").textContent = String(e);
  }
}
