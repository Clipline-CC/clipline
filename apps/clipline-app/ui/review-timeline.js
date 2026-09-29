// Review player: timeline, trim, transport, navigator.
/* ---- main pane views: empty / player / settings ---- */

var settingsOpen = false;

function syncSettingsModalBackground() {
  for (const node of [document.querySelector(".sidebar"), $("gallery-view"), $("review-viewer")]) {
    if (!node) continue;
    node.inert = settingsOpen;
    node.setAttribute("aria-hidden", settingsOpen ? "true" : "false");
  }
}

function updateViews() {
  $("settings-page").hidden = !settingsOpen;
  $("review-viewer").hidden = !currentClip;
  // Settings is an overlay; gallery/review visibility follows only clip state.
  $("gallery-view").hidden = !!currentClip;
  syncSettingsModalBackground();
}

function renderVisibleSettingsSection() {
  const active = document.querySelector("#settings-tabs .tab.active");
  if (settingsOpen && active && active.dataset.tab === "capture") {
    requestAnimationFrame(renderRegionEditor);
  }
  if (settingsOpen && active && active.dataset.tab === "games") {
    renderCustomGames();
    updateGameDetectionStatus();
  }
}

function resumeForegroundSettingsWork() {
  if (!settingsOpen) return;
  const lifecycleWork = captureForegroundWork();
  if (!lifecycleWork) return;
  Promise.all([
    ensureDisplaysLoaded(lifecycleWork),
    ensureAudioDevicesLoaded(lifecycleWork),
    ensureVideoEncodersLoaded(lifecycleWork),
    queryFfmpegRuntimeStatus(),
  ])
    .then(() => {
      if (isForegroundWorkCurrent(lifecycleWork)) renderVisibleSettingsSection();
    })
    .catch((e) => {
      if (isForegroundWorkCurrent(lifecycleWork)) $("error").textContent = e;
    });
}

function requestSettingsClose({ allowDiscard = true } = {}) {
  if (!settingsOpen) return;
  syncSettingsDraftFromForm({ resetDiscard: false });
  if (settingsHaveUnsavedChanges()) {
    if (!settingsDiscardWarningArmed || !allowDiscard) {
      showSettingsDiscardWarning();
      return;
    }
  }
  toggleSettings(false);
}

function toggleSettings(open = !settingsOpen) {
  const wasOpen = settingsOpen;
  settingsOpen = open;
  // The clip survives the round-trip; just don't play behind the page.
  if (settingsOpen && !video.paused) video.pause();
  if (settingsOpen && !wasOpen) {
    resetSettingsDiscardWarning();
    syncSettingsDirtyState({ resetDiscard: true });
    resumeForegroundSettingsWork();
  }
  // Closing discards unsaved edits by repainting from last-saved settings.
  if (wasOpen && !settingsOpen && currentSettings) fillSettings(currentSettings);
  updateViews();
  renderVisibleSettingsSection();
}

function setTrim(start, end) {
  const next = resolveTrim(start, end, clipDuration());
  trimStart = next.start;
  trimEnd = next.end;
  $("trim-summary").textContent = trimSummary(trimStart, trimEnd);
  paintTimeline();
}

function stopAtTrimEnd(current) {
  const stopTime = trimPlaybackStopTime(simpleTrimMode, video.paused, current, trimEnd);
  if (stopTime === null) return false;
  video.pause();
  seekTo(stopTime, { keepGameEventSelection: true, keepGamePlaySelection: true });
  return true;
}

var trimBoundaryFrameCallback = 0;

function clearTrimBoundaryCheck() {
  if (!trimBoundaryFrameCallback) return;
  video.cancelVideoFrameCallback(trimBoundaryFrameCallback);
  trimBoundaryFrameCallback = 0;
}

function scheduleTrimBoundaryCheck() {
  clearTrimBoundaryCheck();
  if (!simpleTrimMode || video.paused || video.ended
      || typeof video.requestVideoFrameCallback !== "function") return;
  trimBoundaryFrameCallback = video.requestVideoFrameCallback((_now, metadata) => {
    trimBoundaryFrameCallback = 0;
    if (!stopAtTrimEnd(metadata.mediaTime)) scheduleTrimBoundaryCheck();
  });
}

function legacyTimelineEnabled() {
  return !!(currentSettings && currentSettings.legacy_timeline_editor);
}

function applyTimelineEditorPreference() {
  const deck = document.querySelector(".deck");
  if (!deck) return;
  const legacy = legacyTimelineEnabled();
  const group = Boolean(activeGroup());
  if (legacy || group) simpleTrimMode = false;
  deck.classList.toggle("legacy-timeline", legacy);
  deck.classList.toggle("simple-timeline", !legacy);
  deck.classList.toggle("simple-trim-active", !legacy && simpleTrimMode);

  const toggle = $("trim-mode-toggle");
  $("trim-action-panel").hidden = legacy || group;
  toggle.disabled = legacy || group;
  toggle.hidden = legacy || group;
  toggle.classList.toggle("active", !legacy && simpleTrimMode);
  toggle.setAttribute("aria-pressed", String(!legacy && simpleTrimMode));
  toggle.title = simpleTrimMode ? "Close" : "Clip";
  toggle.setAttribute("aria-label", simpleTrimMode ? "Close" : "Clip");
  const trimLabel = $("trim-mode-label");
  if (trimLabel) trimLabel.textContent = simpleTrimMode ? "Close" : "Clip";

  const exportLabel = $("export-clip").querySelector("span");
  if (exportLabel) exportLabel.textContent = !legacy && simpleTrimMode ? "Create Clip" : "Clip";
  $("timeline").title = legacy
    ? "Click to seek · drag the selection to slide · drag the edges to trim · scroll to zoom"
    : simpleTrimMode
      ? "Drag the handles to trim · drag the selection to slide · click to seek"
      : "Click to seek · press Clip to create a clip";
  paintTimeline();
}

function setSimpleTrimMode(active) {
  if (legacyTimelineEnabled() || activeGroup()) {
    simpleTrimMode = false;
    scheduleTrimBoundaryCheck();
    applyTimelineEditorPreference();
    return;
  }
  simpleTrimMode = !!active;
  if (simpleTrimMode && currentClip) {
    const dur = clipDuration();
    const range = quickTrimRange(video.currentTime || 0, dur);
    setTrim(range.start, range.end);
    if (dur > 0) {
      noteViewActivity();
      applyView(viewForRange(range.start, range.end, dur, 0.08));
    }
  } else if (currentClip) {
    zoomFit();
  }
  scheduleTrimBoundaryCheck();
  applyTimelineEditorPreference();
}

// The slice of the clip the timeline currently shows. Normalized every read so
// a stale zoom from a previous clip (or a shrunk duration) can never escape the
// bounds — when not zoomed this is just [0, duration].
function timelineView() {
  return clampView(zoomStart, zoomSpan, clipDuration());
}

function resetZoom() {
  zoomStart = 0;
  zoomSpan = 0;
}

// Central view setter: normalize the window, store it (collapsing a full-width
// span back to the zoomed-out sentinel), then re-render everything the window
// affects. Every zoom/pan/fit/follow path routes through here so the ruler,
// markers, track, and navigator can never drift out of sync.
function applyView(next) {
  const dur = clipDuration();
  const v = clampView(next.start, next.span, dur);
  zoomStart = v.start;
  zoomSpan = dur > 0 && v.span >= dur ? 0 : v.span;
  renderRuler();
  renderPlayBlocks();
  renderMarkers();
  paintTimeline();
}

// After a manual view change (wheel zoom/pan, zoom buttons, navigator drag) hold
// auto-follow off briefly, so playback doesn't immediately yank the view back to
// the playhead while the user is deliberately looking elsewhere.
const FOLLOW_SUPPRESS_MS = 1500;
var suppressFollowUntil = 0;
function noteViewActivity() {
  suppressFollowUntil = performance.now() + FOLLOW_SUPPRESS_MS;
}

// Keep the playhead in view while it moves on its own (playback, keyboard jumps,
// marker clicks). Gated on no active drag and a quiet period after a manual view
// change so it never pages out from under the user; only re-renders on a change.
function maybeFollow(playhead) {
  if (dragging || overviewDrag) return;
  if (performance.now() < suppressFollowUntil) return;
  if (!(zoomSpan > 0)) return; // zoomed out: the whole clip is already in view
  const v = timelineView();
  const next = followView(v.start, v.span, clipDuration(), playhead, DEFAULT_FOLLOW_MODE);
  if (Math.abs(next.start - v.start) > 1e-3 || Math.abs(next.span - v.span) > 1e-3) {
    applyView(next);
  }
}

/* ---- zoom / snap controls ---- */

// Zoom by a factor (<1 in, >1 out) anchored on the playhead so it stays in view.
function zoomAtPlayhead(factor) {
  const dur = clipDuration();
  if (!(dur > 0)) return;
  noteViewActivity();
  const v = timelineView();
  const ph = clampTime(video.currentTime || 0, dur);
  const frac = v.span > 0 ? Math.max(0, Math.min(1, (ph - v.start) / v.span)) : 0.5;
  applyView(zoomView(v.start, v.span, dur, frac, factor, MIN_VIEW_SPAN_S));
}

function zoomFit() {
  applyView({ start: 0, span: 0 });
}

// Frame the current trim selection (zoom to selection).
function zoomToSelection() {
  const dur = clipDuration();
  if (!(dur > 0)) return;
  noteViewActivity();
  applyView(viewForRange(trimStart, trimEnd, dur));
}

function setSnap(on) {
  snapEnabled = on;
  $("snap-toggle").classList.toggle("active", snapEnabled);
}

function toggleSnap() {
  setSnap(!snapEnabled);
}

// Best-effort clip frame rate: the recorder's configured fps, else a fine
// fallback. HTML <video> doesn't expose true fps, so frameStep degrades safely.
function clipFps() {
  return currentSettings && Number.isFinite(currentSettings.fps) ? currentSettings.fps : 0;
}

// J/L jump several frames at once — one frame is too fine to navigate with, but
// the step stays frame-aligned (nice for landing trims on a frame).
const KEYBOARD_STEP_FRAMES = 10;

function stepFrame(dir) {
  seekBy(dir * KEYBOARD_STEP_FRAMES * frameStep(clipFps(), DEFAULT_FINE_STEP_S));
}

// Jump to the previous/next edit point (clip ends, trim edges, markers).
function jumpEdit(direction) {
  const points = editPoints(clipMarkers(), trimStart, trimEnd, clipDuration());
  const current = reviewPlayheadTime();
  const target = direction > 0 ? nextMarker(points, current) : prevMarker(points, current);
  if (target) seekTo(target.t_s);
}

function paintTimeline() {
  const dur = clipDuration();
  const view = timelineView();
  const current = dur ? clampTime(reviewPlayheadTime(), dur) : 0;
  // Off-window positions fall outside 0–100% and are clipped by the track; the
  // dimmed trim ends are clamped so they fill the visible side they cover.
  const pct = (t) => percentForView(t, view.start, view.span);
  const edge = (t) => Math.max(0, Math.min(100, pct(t)));
  $("time-readout").textContent = `${fmtTenths(current)} / ${fmtTenths(dur)}`;
  $("playhead").style.left = `${pct(current)}%`;
  $("dim-in").style.width = `${edge(trimStart)}%`;
  $("dim-out").style.width = `${100 - edge(trimEnd)}%`;
  $("handle-in").style.left = `${pct(trimStart)}%`;
  $("handle-out").style.left = `${pct(trimEnd)}%`;
  // The slide strip only appears when there's an actual selection to move (not
  // the whole clip), so the top of the track still scrubs by default.
  const band = $("trim-band");
  const full = !dur || (trimStart <= 0.05 && trimEnd >= dur - 0.05);
  band.style.display = full ? "none" : "block";
  if (!full) {
    band.style.left = `${pct(trimStart)}%`;
    band.style.width = `${Math.max(0, pct(trimEnd) - pct(trimStart))}%`;
  }
  paintOverview();
}

// Cheap per-frame navigator update, in whole-clip coordinates: the trim band,
// the playhead, and the visible-window rectangle. The marker ticks are rebuilt
// separately (renderOverviewMarkers) only when the clip changes.
function paintOverview() {
  const win = $("overview-window");
  if (!win) return;
  const dur = clipDuration();
  const view = timelineView();
  const current = dur ? clampTime(reviewPlayheadTime(), dur) : 0;
  const a = percentFor(trimStart, dur);
  const b = percentFor(trimEnd, dur);
  $("overview-trim").style.left = `${a}%`;
  $("overview-trim").style.width = `${Math.max(0, b - a)}%`;
  $("overview-playhead").style.left = `${percentFor(current, dur)}%`;
  win.style.left = `${percentFor(view.start, dur)}%`;
  win.style.width = `${dur ? Math.max(0, Math.min(100, (view.span / dur) * 100)) : 100}%`;
}

// Rebuild the whole-clip marker ticks in the navigator. View-independent, so it
// runs on clip/marker change only — never per frame and never on zoom.
function renderOverviewMarkers() {
  const layer = $("overview-markers");
  if (!layer) return;
  layer.replaceChildren();
  const dur = clipDuration();
  const presentation = currentPluginPresentation();
  for (const m of clipMarkers()) {
    const tick = document.createElement("i");
    tick.className = `ov-marker marker-${markerStyle(m.kind, presentation).cls}`;
    tick.style.left = `${percentFor(m.t_s, dur)}%`;
    layer.appendChild(tick);
  }
}

// Per-event glyphs for the marker pins, keyed by EventKind. Kept here (DOM
// layer) rather than in player-core.js so its tested {glyph,cls} contract stays
// untouched. Each draws in currentColor so the category tint (--mc) colors it.
const MARKER_ICONS = {
  ChampionKill: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"><path d="M4.5 4.5 19.5 19.5M19.5 4.5 4.5 19.5"/><path d="M13 16 16 13M8 13 11 16"/><circle cx="19.5" cy="19.5" r="1.15" fill="currentColor" stroke="none"/><circle cx="4.5" cy="19.5" r="1.15" fill="currentColor" stroke="none"/></svg>`,
  ChampionAssist: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M12 5 12 19M5 12 19 12"/></svg>`,
  ChampionDeath: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"><path d="M12 3.5C7.6 3.5 5 6.7 5 10.5C5 12.8 6 14.4 7.2 15.5C7.6 15.9 7.8 16.3 7.8 16.8L7.8 18.5A1 1 0 0 0 8.8 19.5L15.2 19.5A1 1 0 0 0 16.2 18.5L16.2 16.8C16.2 16.3 16.4 15.9 16.8 15.5C18 14.4 19 12.8 19 10.5C19 6.7 16.4 3.5 12 3.5Z"/><circle cx="9.4" cy="11" r="1.4" fill="currentColor" stroke="none"/><circle cx="14.6" cy="11" r="1.4" fill="currentColor" stroke="none"/></svg>`,
  FirstBlood: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linejoin="round"><path d="M12 3.5C12 3.5 18.5 11 18.5 15.5A6.5 6.5 0 1 1 5.5 15.5C5.5 11 12 3.5 12 3.5Z"/></svg>`,
  Multikill: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round"><path d="M6 11.5A6 6 0 0 1 18 11.5L18 14.5A1.4 1.4 0 0 1 16.6 15.9L16 15.9 16 18.5 8 18.5 8 15.9 7.4 15.9A1.4 1.4 0 0 1 6 14.5Z"/><circle cx="9.6" cy="12.2" r="1.5" fill="currentColor" stroke="none"/><circle cx="14.4" cy="12.2" r="1.5" fill="currentColor" stroke="none"/></svg>`,
  Ace: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"><path d="M12 3 14.12 9.51 20.97 9.51 15.42 13.54 17.55 20.05 12 16.02 6.45 20.05 8.58 13.54 3.03 9.51 9.88 9.51Z"/></svg>`,
  DragonKill: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round"><path d="M13 3C13.5 7 17 9 17 13.5A5 5 0 0 1 7 13.7C7 11.5 8.3 10.3 8.3 10.3C8.6 12 9.8 12.6 9.8 12.6C11 11.2 9.5 7.5 13 3Z"/></svg>`,
  HeraldKill: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linejoin="round"><path d="M3 12C6 6.5 18 6.5 21 12C18 17.5 6 17.5 3 12Z"/><circle cx="12" cy="12" r="2.7" fill="currentColor" stroke="none"/></svg>`,
  BaronKill: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"><path d="M4 18.5 4 8 8.5 11.5 12 5.5 15.5 11.5 20 8 20 18.5Z"/></svg>`,
  TurretKilled: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"><path d="M6 20.5 6 7 8.5 7 8.5 9 11 9 11 7 13 7 13 9 15.5 9 15.5 7 18 7 18 20.5Z"/></svg>`,
  InhibKilled: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round"><path d="M12 3 17 9 14 20.5 10 20.5 7 9Z"/><path d="M7 9 17 9M12 3 12 20.5"/></svg>`,
  FirstBrick: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round"><path d="M5 20.5 5 8.5 7 8.5 7 10 9 10 9 8.5 11 8.5 11 10 13 10 13 8.5 14.5 8.5 14.5 20.5Z"/><path d="M19 3.2 19.7 5.6 22.1 6.3 19.7 7 19 9.4 18.3 7 15.9 6.3 18.3 5.6Z" fill="currentColor" stroke="none"/></svg>`,
  GameStart: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"><path d="M6.5 21 6.5 3"/><path d="M6.5 4 17 7 6.5 10"/></svg>`,
  MinionsSpawning: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="8.5"/><path d="M12 7.5 12 12 15 14"/></svg>`,
  GameEnd: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M7 4 17 4 17 7A5 5 0 0 1 7 7Z"/><path d="M7 5 4.5 5A2 2 0 0 0 7 8.7M17 5 19.5 5A2 2 0 0 1 17 8.7"/><path d="M12 12 12 16M8.5 19.5 15.5 19.5 15 16.5 9 16.5Z"/></svg>`,
  Bookmark: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"><path d="M7 3.5 17 3.5 17 20.5 12 16 7 20.5Z"/></svg>`,
  Other: `<svg viewBox="0 0 24 24" fill="currentColor" stroke="none"><circle cx="12" cy="12" r="3"/></svg>`,
};
// Unknown / future kinds fall back to a representative glyph for their category.
const MARKER_ICON_FALLBACK = {
  kill: MARKER_ICONS.ChampionKill,
  assist: MARKER_ICONS.ChampionAssist,
  spree: MARKER_ICONS.Ace,
  objective: MARKER_ICONS.BaronKill,
  structure: MARKER_ICONS.TurretKilled,
  bookmark: MARKER_ICONS.Bookmark,
  info: MARKER_ICONS.Other,
};
// Game-authentic art for marker kinds shown by the review timeline filter. Used
// as a CSS mask so each silhouette still tints with its category color (--mc);
// kinds without art fall back to the SVGs above.
const MARKER_IMAGES = {
  ChampionKill: "assets/markers/kill.png",
  ChampionAssist: "assets/markers/assist.png",
  ChampionDeath: "assets/markers/death.png",
  DragonKill: "assets/markers/dragon.png",
  BaronKill: "assets/markers/baron.png",
  TurretKilled: "assets/markers/turret.png",
};

function markerImageForKind(kind, presentation) {
  const configured = PlayerCore.markerKindConfig(kind, presentation).icon;
  const fallback = PlayerCore.ownObjectValue(MARKER_IMAGES, kind);
  return PlayerCore.safeMarkerImage(configured) || PlayerCore.safeMarkerImage(fallback);
}

function renderPlayBlocks() {
  const layer = $("play-block-layer");
  if (!layer) return;
  layer.replaceChildren();
  const dur = clipDuration();
  const plays = clipPlays();
  if (!(dur > 0) || !plays.length) return;
  const view = timelineView();
  playBlocks(plays, dur).forEach((play, index) => {
    const left = percentForView(play.start, view.start, view.span);
    const right = percentForView(play.end, view.start, view.span);
    if (right < -2 || left > 102) return;
    const block = document.createElement("button");
    block.type = "button";
    block.className = "play-block"
      + (play.play && play.play.passed ? "" : " failed")
      + (play.incomplete ? " incomplete" : "")
      + (play.estimated ? " estimated" : "");
    block.setAttribute("data-game-play-index", String(index));
    block.style.left = `${left}%`;
    block.style.width = `${Math.max(0.4, right - left)}%`;
    block.title = `${play.title}\n${play.details}\n${fmtTenths(play.start)}-${fmtTenths(play.end)}`;
    block.addEventListener("pointerdown", (ev) => ev.stopPropagation());
    block.addEventListener("click", (ev) => {
      ev.stopPropagation();
      selectGamePlay(index, play.start, play.end);
      seekTo(play.start, { keepGamePlaySelection: true });
      video.play().catch(() => syncPlayState());
    });
    layer.appendChild(block);
  });
}

function renderMarkers() {
  const layer = $("marker-layer");
  layer.replaceChildren();
  const view = timelineView();
  const markers = clipMarkers();
  const presentation = currentPluginPresentation();
  for (const m of markers) {
    const left = percentForView(m.t_s, view.start, view.span);
    // The marker band isn't clipped like the track, so drop glyphs that would
    // ride outside the visible window (a small margin keeps edge glyphs whole).
    if (left < -2 || left > 102) continue;
    const style = markerStyle(m.kind, presentation);
    const marker = document.createElement("button");
    marker.className = `marker marker-${style.cls}`;
    marker.style.left = `${left}%`;
    // A bookmark has no actor or victim to describe — only when it happened.
    marker.title = PlayerCore.isBookmarkMarker(m)
      ? `Bookmark @ ${m.t_s.toFixed(1)}s`
      : `${m.kind}${m.subtype ? ` (${m.subtype})` : ""} — ${m.actor}${m.victim ? " → " + m.victim : ""} @ ${m.t_s.toFixed(1)}s`;

    const glyph = document.createElement("span");
    glyph.className = "glyph";
    const img = markerImageForKind(m.kind, presentation);
    if (img) {
      glyph.classList.add("img");
      glyph.style.setProperty("--marker-img", `url("${img}")`);
    } else {
      glyph.innerHTML = PlayerCore.ownObjectValue(MARKER_ICONS, m.kind)
        || PlayerCore.ownObjectValue(MARKER_ICON_FALLBACK, style.cls)
        || MARKER_ICONS.Other;
    }
    const hair = document.createElement("span");
    hair.className = "hair";
    marker.append(glyph, hair);

    marker.addEventListener("pointerdown", (ev) => ev.stopPropagation());
    marker.addEventListener("click", (ev) => {
      ev.stopPropagation();
      // Start a beat before the event so its lead-up plays, then roll.
      seekTo(m.t_s - MARKER_LEAD_S);
      video.play().catch(() => syncPlayState());
    });
    layer.appendChild(marker);
  }
  $("marker-count").textContent = markerSummary(markers);
  $("prev-marker").disabled = !markers.length;
  $("next-marker").disabled = !markers.length;
}

function renderRuler() {
  const root = $("ruler");
  root.replaceChildren();
  const view = timelineView();
  if (!(view.span > 0)) return;
  const viewEnd = view.start + view.span;
  const pct = (t) => percentForView(t, view.start, view.span);
  const marks = rulerMarksRange(view.start, view.span, 8);
  // Dense ticks between the labeled majors mirror clipping tools: quick spatial
  // reference without turning the timeline into a data graph.
  if (marks.length >= 2) {
    const step = marks[1].t - marks[0].t;
    const minorStep = step / 10;
    const isMajor = (t) => marks.some((m) => Math.abs(m.t - t) < minorStep / 2);
    const firstMinor = Math.ceil(view.start / minorStep - 1e-9) * minorStep;
    for (let t = firstMinor; t <= viewEnd + 1e-6; t += minorStep) {
      if (t <= 0 || isMajor(t)) continue;
      const tick = document.createElement("i");
      const divisionsFromFirst = Math.round((t - marks[0].t) / minorStep);
      const isHalf = divisionsFromFirst % 5 === 0;
      tick.className = isHalf ? "tick minor" : "tick micro";
      tick.style.left = `${pct(t)}%`;
      root.appendChild(tick);
    }
  }
  marks.forEach((mark) => {
    const tick = document.createElement("i");
    tick.className = "tick major";
    tick.style.left = `${pct(mark.t)}%`;
    root.appendChild(tick);
    const lab = document.createElement("span");
    // The 0:00 label hugs the left edge (no centering) only when it sits there.
    const atLeftEdge = view.start === 0 && mark.t <= 1e-6;
    lab.className = atLeftEdge ? "lab first" : "lab";
    lab.style.left = `${pct(mark.t)}%`;
    lab.textContent = mark.label;
    root.appendChild(lab);
  });
}

function seekTo(time, options = {}) {
  if (!currentClip || !Number.isFinite(time)) return;
  if (!options.keepGameEventSelection) clearGameEventSelection();
  if (!options.keepGamePlaySelection) clearGamePlaySelection();
  reviewSeekState = PlayerCore.requestLogicalSeek(reviewSeekState, time, clipDuration());
  const target = reviewSeekState.targetTime;
  if (reviewSeekState.metadataGeneration === reviewSourceGeneration && !video.seeking) {
    video.currentTime = target;
  }
  maybeFollow(target);
  paintTimeline();
  syncGameEventRail(target);
  syncGamePlayRail(target, { keepGamePlaySelection: options.keepGamePlaySelection });
}

video.addEventListener("seeked", () => {
  const decision = PlayerCore.seekedDecision(
    reviewSeekState,
    reviewSourceGeneration,
    video.currentTime,
    clipDuration(),
  );
  reviewSeekState = decision.state;
  if (decision.applyTime != null) video.currentTime = decision.applyTime;
  const current = reviewPlayheadTime();
  maybeFollow(current);
  paintTimeline();
  syncGameEventRail(current);
  syncGamePlayRail(current);
  // Only an explicit user reposition may bypass the sidecar drift tolerance. The
  // initial source settlement reaches this handler too, and forcing there
  // re-seeked already-audible sidecars backward for no correction — the drift it
  // "fixed" was ~20 ms against a 0.5 s tolerance, and the element landed back
  // where it started. That was the audible repeat at the start of every clip.
  syncReviewAudioSidecars({
    forceSeek: PlayerCore.sidecarRealignmentForced(decision.confirmedSource),
  });
});

function seekBy(delta) {
  seekTo(PlayerCore.relativeSeekTarget(
    video.currentTime,
    reviewSeekState.targetTime,
    delta,
    clipDuration(),
  ));
}

function togglePlay() {
  if (!currentClip) return;
  if (video.paused) video.play().catch(() => syncPlayState());
  else video.pause();
}

function syncPlayState() {
  $("play-toggle").classList.toggle("playing", !video.paused);
  $("play-toggle").setAttribute("aria-pressed", String(!video.paused));
  updateOverlay();
}

function reviewFullscreenActive() {
  return document.fullscreenElement === stageFrame;
}

function syncReviewFullscreenState() {
  const active = reviewFullscreenActive();
  const button = $("fullscreen-toggle");
  button.classList.toggle("active", active);
  button.setAttribute("aria-pressed", String(active));
  button.setAttribute("aria-label", active ? "Exit full screen" : "Enter full screen");
  button.title = active ? "Exit full screen (F or Esc)" : "Full screen (F)";
  updateStageFrame();
  noteActivity();
}

async function toggleReviewFullscreen() {
  if (!currentClip) return;
  $("error").textContent = "";
  try {
    if (reviewFullscreenActive()) {
      await document.exitFullscreen();
    } else {
      await stageFrame.requestFullscreen();
    }
  } catch (error) {
    $("error").textContent = `full screen: ${error}`;
  }
}

function syncVolume() {
  $("mute-toggle").classList.toggle("muted", reviewAudioMuted || reviewAudioVolume === 0);
  $("volume-slider").value = String(reviewAudioMuted ? 0 : reviewAudioVolume);
  syncRangeProgress($("volume-slider"));
}

/* ---- overlay visibility (PlayerCore.overlayVisible policy) ---- */

var lastActivityMs = 0;
var overlayIdle = null;

function noteActivity() {
  lastActivityMs = performance.now();
  scheduleOverlayIdleCheck();
}

function updateOverlay() {
  const idleMs = performance.now() - lastActivityMs;
  const nextIdle = !overlayVisible(video.paused, idleMs);
  if (overlayIdle === nextIdle) return;
  stage.classList.toggle("idle", nextIdle);
  overlayIdle = nextIdle;
}

function clearOverlayIdleCheck() {
  clearTimeout(overlayTimerId);
  overlayTimerId = 0;
}

function scheduleOverlayIdleCheck() {
  clearOverlayIdleCheck();
  updateOverlay();
  if (video.paused || video.ended) return;
  const remainingMs = Math.max(0, OVERLAY_HIDE_MS - (performance.now() - lastActivityMs));
  overlayTimerId = setTimeout(() => {
    overlayTimerId = 0;
    updateOverlay();
  }, remainingMs + 30);
}

function toggleMute() {
  if (reviewAudioMuted || reviewAudioVolume === 0) {
    reviewAudioMuted = false;
    if (reviewAudioVolume === 0) reviewAudioVolume = 1;
  } else {
    reviewAudioMuted = true;
  }
  applyReviewAudioOutput();
  syncVolume();
}

function jumpMarker(direction) {
  const markers = clipMarkers();
  const current = video.currentTime || 0;
  const target = direction > 0 ? nextMarker(markers, current) : prevMarker(markers, current);
  if (target) seekTo(target.t_s);
}

/* ---- timeline pointer interaction ---- */

var resumeAfterDrag = false;
// Snap targets snapshotted at pointerdown so a drag never snaps to its own
// moving position (the dragged edge and the playhead are excluded up front).
var dragCandidates = [];
// Sliding the whole selection: offset from pointer to selection start, the click
// time, and whether the pointer moved enough to count as a drag (vs a seek).
var slideGrab = 0;
var slideClickT = 0;
var slideStartX = 0;
var slideMoved = false;
const SLIDE_THRESHOLD_PX = 4;

function clearSnapFeedback() {
  $("playhead").classList.remove("snapped");
  $("handle-in").classList.remove("snapped");
  $("handle-out").classList.remove("snapped");
  $("trim-band").classList.remove("snapped");
}

function startDrag(kind, ev) {
  if (!currentClip) return;
  dragging = kind;
  // Scrub paused so every pointer position shows its frame, then restore.
  resumeAfterDrag = !video.paused;
  if (resumeAfterDrag) video.pause();
  // Exclude the element(s) being moved so a drag never snaps to itself.
  const playhead = video.currentTime || 0;
  let exclude;
  if (kind === "scrub") {
    exclude = ["playhead"]; // the playhead rides the pointer
  } else if (kind === "slide") {
    exclude = ["in", "out"]; // both edges move together
  } else {
    // Trim edge: always drop the dragged edge. The playhead rides that edge once
    // the drag starts, so drop it too ONLY if it's already within snap range of
    // the edge (else the handle would stick to its own start) — a playhead parked
    // elsewhere stays a useful snap target.
    exclude = [kind];
    const rect = $("timeline").getBoundingClientRect();
    const v = timelineView();
    const pps = rect.width && v.span > 0 ? rect.width / v.span : 0;
    const tol = pps > 0 ? SNAP_THRESHOLD_PX / pps : 0.05;
    const edge = kind === "in" ? trimStart : trimEnd;
    if (Math.abs(playhead - edge) <= tol) exclude.push("playhead");
  }
  dragCandidates = snapCandidates(clipDuration(), clipMarkers(), playhead, trimStart, trimEnd, exclude);
  if (kind === "slide") {
    const rect = $("timeline").getBoundingClientRect();
    const v = timelineView();
    const t = timelineTimeView(ev.clientX, rect.left, rect.width, v.start, v.span, clipDuration());
    slideGrab = t - trimStart;
    slideClickT = t;
    slideStartX = ev.clientX;
    slideMoved = false;
    $("trim-band").classList.add("grabbing");
  }
  $("timeline").setPointerCapture(ev.pointerId);
  moveDrag(ev);
}

function moveDrag(ev) {
  if (!dragging) return;
  const rect = $("timeline").getBoundingClientRect();
  const view = timelineView();
  const dur = clipDuration();
  const rawT = timelineTimeView(ev.clientX, rect.left, rect.width, view.start, view.span, dur);
  const pps = rect.width && view.span > 0 ? rect.width / view.span : 0;
  const doSnap = snapEnabled && !ev.altKey && pps > 0;
  clearSnapFeedback();

  if (dragging === "slide") {
    // Hold still and release to seek; move past the threshold to start sliding.
    if (!slideMoved && Math.abs(ev.clientX - slideStartX) <= SLIDE_THRESHOLD_PX) return;
    slideMoved = true;
    // Move the whole selection, keeping its length. Snap whichever edge lands
    // closest to a salient time so either end can lock cleanly.
    const len = trimEnd - trimStart;
    let newStart = rawT - slideGrab;
    let snapped = false;
    if (doSnap) {
      const a = snapTime(newStart, dragCandidates, pps, SNAP_THRESHOLD_PX);
      const b = snapTime(newStart + len, dragCandidates, pps, SNAP_THRESHOLD_PX);
      const da = a.snapped ? Math.abs(a.t - newStart) : Infinity;
      const db = b.snapped ? Math.abs(b.t - (newStart + len)) : Infinity;
      if (da <= db && a.snapped) { newStart = a.t; snapped = true; }
      else if (b.snapped) { newStart = b.t - len; snapped = true; }
    }
    const next = slideTrim(trimStart, trimEnd, newStart, dur);
    setTrim(next.start, next.end);
    if (snapped) $("trim-band").classList.add("snapped");
    return;
  }

  let t = rawT;
  let snapped = false;
  if (doSnap) {
    const res = snapTime(t, dragCandidates, pps, SNAP_THRESHOLD_PX);
    t = res.t;
    snapped = res.snapped;
  }
  if (dragging === "scrub") {
    if (snapped) $("playhead").classList.add("snapped");
    seekTo(t);
  } else {
    if (snapped) $(dragging === "in" ? "handle-in" : "handle-out").classList.add("snapped");
    const next = trimDrag(dragging, t, trimStart, trimEnd, dur);
    setTrim(next.start, next.end);
    // The playhead rides the dragged edge — you trim on the frame you see.
    seekTo(dragging === "in" ? next.start : next.end);
  }
}

function endDrag() {
  if (!dragging) return;
  // A press-and-release on the selection without dragging just seeks there.
  const clickSeek = dragging === "slide" && !slideMoved;
  dragging = null;
  dragCandidates = [];
  clearSnapFeedback();
  $("trim-band").classList.remove("grabbing");
  if (clickSeek) seekTo(slideClickT);
  if (resumeAfterDrag) {
    resumeAfterDrag = false;
    video.play().catch(() => syncPlayState());
  }
}

// Higher = faster zoom per wheel notch. e^(±notch·sensitivity) is the span
// multiplier, so it zooms by the same ratio whichever way you scroll.
const ZOOM_SENSITIVITY = 0.0015;

// Scroll over the timeline to zoom, keeping the clip moment under the cursor
// pinned. Scroll up (deltaY < 0) zooms in, down zooms back out.
function onTimelineWheel(ev) {
  const dur = clipDuration();
  if (!currentClip || !(dur > 0)) return;
  if (!legacyTimelineEnabled() && !simpleTrimMode) return;
  ev.preventDefault();
  noteViewActivity();
  const rect = $("timeline").getBoundingClientRect();
  if (!rect.width) return;
  // Normalize line/page wheels (Firefox-style) to roughly pixel scale.
  const unit = ev.deltaMode === 1 ? 33 : ev.deltaMode === 2 ? rect.width : 1;
  const view = timelineView();
  // Shift+wheel, or a genuinely horizontal trackpad gesture, pans instead of
  // zooming. Requiring |deltaX| > |deltaY| keeps trackpad noise during a
  // vertical scroll from misfiring a pan.
  if (ev.shiftKey || Math.abs(ev.deltaX) > Math.abs(ev.deltaY)) {
    const raw = ev.shiftKey ? ev.deltaY || ev.deltaX : ev.deltaX;
    const seconds = ((raw * unit) / rect.width) * view.span;
    applyView(panView(view.start, view.span, dur, seconds));
    return;
  }
  const anchorFrac = (ev.clientX - rect.left) / rect.width;
  const factor = Math.max(0.5, Math.min(2, Math.exp(ev.deltaY * unit * ZOOM_SENSITIVITY)));
  applyView(zoomView(view.start, view.span, dur, anchorFrac, factor, MIN_VIEW_SPAN_S));
}

/* ---- navigator (whole-clip minimap) drag: body pans, grips zoom ---- */

// Clip time under the pointer in the whole-clip navigator.
function overviewTime(ev) {
  const rect = $("overview").getBoundingClientRect();
  const dur = clipDuration();
  if (!rect.width || !dur) return 0;
  const x = Math.max(0, Math.min(rect.width, ev.clientX - rect.left));
  return (x / rect.width) * dur;
}

function onOverviewPointerDown(ev) {
  if (!currentClip || !(clipDuration() > 0)) return;
  ev.preventDefault();
  const dur = clipDuration();
  const v = timelineView();
  const t = overviewTime(ev);
  if (ev.target === $("overview-window-l")) {
    overviewDrag = { mode: "left", pointerId: ev.pointerId };
    moveOverviewDrag(ev);
  } else if (ev.target === $("overview-window-r")) {
    overviewDrag = { mode: "right", pointerId: ev.pointerId };
    moveOverviewDrag(ev);
  } else if (ev.target === $("overview-window")) {
    // Grab the box where you clicked it and pan, keeping that point under the cursor.
    overviewDrag = { mode: "pan", grab: t - v.start, pointerId: ev.pointerId };
  } else {
    // Clicking the empty track jumps the window to center on the click, then pans.
    const nv = clampView(t - v.span / 2, v.span, dur);
    applyView(nv);
    overviewDrag = { mode: "pan", grab: t - nv.start, pointerId: ev.pointerId };
  }
  $("overview").setPointerCapture(ev.pointerId);
  $("overview-window").classList.add("grabbing");
}

function moveOverviewDrag(ev) {
  if (!overviewDrag) return;
  const dur = clipDuration();
  const v = timelineView();
  const t = overviewTime(ev);
  if (overviewDrag.mode === "pan") {
    applyView(clampView(t - overviewDrag.grab, v.span, dur));
  } else {
    applyView(setViewEdge(v.start, v.span, dur, overviewDrag.mode, t));
  }
}

function endOverviewDrag() {
  if (!overviewDrag) return;
  overviewDrag = null;
  $("overview-window").classList.remove("grabbing");
  noteViewActivity(); // don't snap back to the playhead the instant the drag ends
}

// Navigator scroll pans the visible window left/right. The strip spans the whole
// clip, so map pixels scrolled to clip seconds (no-op when fully zoomed out).
function onOverviewWheel(ev) {
  const dur = clipDuration();
  if (!currentClip || !(dur > 0)) return;
  ev.preventDefault();
  noteViewActivity();
  const rect = $("overview").getBoundingClientRect();
  if (!rect.width) return;
  const unit = ev.deltaMode === 1 ? 33 : ev.deltaMode === 2 ? rect.width : 1;
  const raw = Math.abs(ev.deltaX) > Math.abs(ev.deltaY) ? ev.deltaX : ev.deltaY;
  const view = timelineView();
  applyView(panView(view.start, view.span, dur, ((raw * unit) / rect.width) * dur));
}
