// Pure review-player logic: formatting, trim clamping, timeline math, marker
// navigation, keyboard intents. No DOM, no Tauri — tests/player_core.rs
// evaluates this file with presentation-core.js in Boa, so both stay DOM-free.
const PlayerCore = (() => {
  const MIN_TRIM_GAP_S = 0.1;
  const MARKER_EPSILON_S = 0.05;
  const OVERLAY_HIDE_MS = 2000;
  // The most you can zoom the edit timeline in: the visible window never shrinks
  // below this many seconds (kept above the trim gap so handles stay grabbable).
  const MIN_VIEW_SPAN_S = 1;
  // Auto-scroll the zoomed window to keep the playhead in view: "page" re-pages
  // only when the playhead leaves the window (cheap, the default).
  const DEFAULT_FOLLOW_MODE = "page";
  // How close (in pixels) a drag must be to a salient time before snapping.
  const SNAP_THRESHOLD_PX = 8;
  // Fine-step fallback when the clip's true frame rate is unknown.
  const DEFAULT_FINE_STEP_S = 1 / 60;
  const QUICK_TRIM_WINDOW_S = 30;
  const AUDIO_SIDECAR_DRIFT_DEADBAND_S = 0.025;
  const AUDIO_SIDECAR_HARD_SEEK_TOLERANCE_S = 0.5;

  // YouTube grammar: controls pin while paused, fade when playing and idle.
  const overlayVisible = (paused, idleMs) => paused || idleMs < OVERLAY_HIDE_MS;

  const fmtBytes = (bytes) => {
    const mb = bytes / (1024 * 1024);
    if (mb >= 1024) return `${(mb / 1024).toFixed(1)} GB`;
    return `${mb.toFixed(1)} MB`;
  };

  const fmtMegabytes = (megabytes) => fmtBytes(megabytes * 1024 * 1024);

  const fmtQuotaGb = (quotaGb) => {
    const value = Number(quotaGb);
    if (!Number.isFinite(value) || value <= 0) return "no limit";
    if (Number.isInteger(value)) return `${value.toFixed(0)} GB`;
    if (value < 0.1) return `${value.toFixed(2)} GB`;
    return `${value.toFixed(1)} GB`;
  };

  const fmtLibraryStorageUsage = (usedBytes, quotaGb) =>
    `${fmtBytes(usedBytes)} / ${fmtQuotaGb(quotaGb)}`;

  const pathBaseName = (path) => {
    const text = String(path || "").trim();
    if (!text) return "";
    return text.split(/[\\/]/).filter(Boolean).pop() || text;
  };

  // Header-safe clip label: the recorded file name, else the final path segment.
  // Never the full path — folder names leak when the header is screen-shared.
  const clipFileLabel = (clip) => {
    const name = String(clip && clip.name || "").trim();
    if (name) return name;
    return pathBaseName(clip && clip.path);
  };

  const windowsClipPathKey = (path) => {
    const text = String(path || "").trim();
    if (!text) return null;
    let normalized = text.replace(/\//g, "\\");
    const lower = normalized.toLowerCase();
    if (lower.startsWith("\\\\?\\unc\\")) {
      normalized = "\\\\" + normalized.slice(8);
    } else if (lower.startsWith("\\\\?\\")) {
      normalized = normalized.slice(4);
    }
    if (!/^[a-z]:\\/i.test(normalized) && !normalized.startsWith("\\\\")) return null;
    return normalized.toLowerCase();
  };

  const sameClipPath = (left, right) => {
    const leftText = String(left || "").trim();
    const rightText = String(right || "").trim();
    if (!leftText || !rightText) return false;
    if (leftText === rightText) return true;
    const leftKey = windowsClipPathKey(leftText);
    const rightKey = windowsClipPathKey(rightText);
    return leftKey !== null && rightKey !== null && leftKey === rightKey;
  };

  const clipNameStem = PresentationCore.clipNameStem;

  const cloudLibraryEntries = (
    uploads,
    localClips = [],
    cloudClips = [],
    cloudListAuthoritative = false,
  ) => {
    const localPaths = (localClips || []).map((clip) => String(clip && clip.path || ""));
    const localPathAvailable = (path) => localPaths.some((localPath) => sameClipPath(localPath, path));
    const uploadRecords = Object.values(uploads || {});
    const uploadsByLocalId = new Map(
      uploadRecords
        .filter((record) => record && record.local_clip_id)
        .map((record) => [String(record.local_clip_id), record])
    );
    const seenLocalIds = new Set();
    const seenRemoteIds = new Set();
    const entries = [];

    for (const clip of cloudClips || []) {
      if (!clip || (!clip.remote_clip_id && !clip.remote_url)) continue;
      const localId = String(clip.local_clip_id || "");
      const upload = localId ? uploadsByLocalId.get(localId) : null;
      const path = String(clip.path || (upload && upload.path) || "");
      const remoteId = String(clip.remote_clip_id || "");
      if (localId) seenLocalIds.add(localId);
      if (remoteId) seenRemoteIds.add(remoteId);
      const entry = {
        local_clip_id: localId,
        path,
        title: String(clip.title || clipNameStem(pathBaseName(path)) || remoteId || "Cloud clip"),
        remote_url: String(clip.remote_url || ""),
        visibility: ["public", "unlisted", "private"].includes(clip.visibility)
          ? clip.visibility
          : "private",
        upload_status: String(clip.upload_status || "uploaded_processing"),
        updated_at_unix: Number(clip.updated_at_unix) || 0,
        local_available: Boolean(path && localPathAvailable(path)),
        remote_clip_id: remoteId,
      };
      const durationMs = Number(clip.duration_ms);
      if (Number.isFinite(durationMs)) entry.duration_ms = durationMs;
      const fileSizeBytes = Number(clip.file_size_bytes);
      if (Number.isFinite(fileSizeBytes)) entry.file_size_bytes = fileSizeBytes;
      entries.push(entry);
    }

    entries.push(...uploadRecords
      .filter((record) => {
        if (!record || (!record.remote_clip_id && !record.remote_url)) return false;
        if (record.local_clip_id && seenLocalIds.has(String(record.local_clip_id))) return false;
        if (record.remote_clip_id && seenRemoteIds.has(String(record.remote_clip_id))) return false;
        const status = String(record.upload_status || "");
        const active = ["queued", "uploading", "processing", "retrying", "uploaded_processing"]
          .includes(status);
        if (cloudListAuthoritative && !active) return false;
        return status !== "failed" && status !== "not_uploaded";
      })
      .map((record) => {
        const path = String(record.path || "");
        const status = String(record.upload_status || "processing");
        const visibility = ["public", "unlisted", "private"].includes(record.visibility)
          ? record.visibility
          : status === "uploaded_private" ? "private" : "public";
        const entry = {
          local_clip_id: String(record.local_clip_id || ""),
          path,
          title: clipNameStem(pathBaseName(path)) || String(record.remote_clip_id || "Cloud clip"),
          remote_url: String(record.remote_url || ""),
          visibility,
          upload_status: status,
          updated_at_unix: Number(record.updated_at_unix) || 0,
          local_available: localPathAvailable(path),
        };
        const remoteId = String(record.remote_clip_id || "");
        if (remoteId) entry.remote_clip_id = remoteId;
        return entry;
      }));

    return entries.sort((a, b) => b.updated_at_unix - a.updated_at_unix || a.title.localeCompare(b.title));
  };

  // Round the total before splitting so 59.6 carries to "1:00", not "0:60".
  const fmtDur = (s) => {
    if (s == null || !Number.isFinite(s)) return "?";
    const total = Math.round(s);
    const minutes = Math.floor(total / 60);
    const seconds = total - minutes * 60;
    return `${minutes}:${String(seconds).padStart(2, "0")}`;
  };

  // m:ss.t — the readout/trim precision. Same carry rule, in tenths.
  const fmtTenths = (s) => {
    if (s == null || !Number.isFinite(s)) return "?";
    const tenths = Math.round(s * 10);
    const minutes = Math.floor(tenths / 600);
    const rest = tenths - minutes * 600;
    const seconds = Math.floor(rest / 10);
    return `${minutes}:${String(seconds).padStart(2, "0")}.${rest % 10}`;
  };

  const fmtAgo = (nowUnixS, thenUnixS) => {
    const d = Math.max(0, nowUnixS - thenUnixS);
    const ago = (n, unit) => `${n} ${unit}${n === 1 ? "" : "s"} ago`;
    if (d < 60) return "just now";
    if (d < 3600) return ago(Math.floor(d / 60), "minute");
    if (d < 86400) return ago(Math.floor(d / 3600), "hour");
    if (d < 7 * 86400) return ago(Math.floor(d / 86400), "day");
    if (d < 30 * 86400) return ago(Math.floor(d / (7 * 86400)), "week");
    if (d < 365 * 86400) return ago(Math.floor(d / (30 * 86400)), "month");
    return ago(Math.floor(d / (365 * 86400)), "year");
  };

  const settingDurationLabel = (seconds) => {
    const total = Math.max(0, Math.round(seconds || 0));
    if (total < 60) return `${total} sec`;
    const minutes = Math.floor(total / 60);
    const rest = total - minutes * 60;
    const minutePart = `${minutes} min`;
    return rest ? `${minutePart} ${rest} sec` : minutePart;
  };

  const QUALITY_PRESETS = [
    { id: "compact", label: "Compact", hint: "smaller files" },
    { id: "balanced", label: "Balanced", hint: "good default" },
    { id: "sharp", label: "Sharp", hint: "more detail" },
    { id: "maximum", label: "Maximum", hint: "largest files" },
  ];

  const QUALITY_BITRATES_MBPS = {
    source: [6, 12, 24, 40],
    "1440p": [6, 12, 24, 40],
    "1080p": [4, 8, 16, 24],
    "720p": [2.5, 5, 8, 12],
    "480p": [1.5, 3, 5, 8],
  };

  const SMOOTHNESS_PRESETS = [
    { fps: 30, label: "30 FPS", hint: "lighter on the PC" },
    { fps: 60, label: "60 FPS", hint: "good default for most games" },
    { fps: 90, label: "90 FPS", hint: "smoother for high-refresh play" },
    { fps: 120, label: "120 FPS", hint: "best for high-refresh footage" },
  ];

  const OUTPUT_RESOLUTION_OPTIONS = [
    { id: "source", label: "Source", hint: "uses the captured size" },
    { id: "1440p", label: "1440p", hint: "up to 2560 x 1440" },
    { id: "1080p", label: "1080p", hint: "up to 1920 x 1080" },
    { id: "720p", label: "720p", hint: "up to 1280 x 720" },
    { id: "480p", label: "480p", hint: "up to 854 x 480" },
  ];

  const nearestPresetIndex = (presets, value, field) => {
    let best = 0;
    let bestDistance = Infinity;
    for (let i = 0; i < presets.length; i++) {
      const distance = Math.abs(presets[i][field] - value);
      if (distance < bestDistance) {
        best = i;
        bestDistance = distance;
      }
    }
    return best;
  };

  const qualityBitrates = (outputResolution) =>
    QUALITY_BITRATES_MBPS[outputResolutionOption(outputResolution).id] || QUALITY_BITRATES_MBPS.source;

  const recordingQualityPreset = (index, outputResolution = "source") => {
    const clamped = Math.max(0, Math.min(QUALITY_PRESETS.length - 1, Math.round(index || 0)));
    return { ...QUALITY_PRESETS[clamped], bitrate: qualityBitrates(outputResolution)[clamped] };
  };

  const mbpsLabel = (mbps) => {
    const value = Number(mbps);
    if (!Number.isFinite(value)) return "0 Mbps";
    return `${Number.isInteger(value) ? value.toFixed(0) : value.toFixed(1)} Mbps`;
  };

  const recordingQualitySummary = (quality) =>
    `${quality.label} quality - ${quality.hint}. ${mbpsLabel(quality.bitrate)}.`;

  const qualityIndexForBitrate = (mbps, outputResolution = "source") => {
    const bitrates = qualityBitrates(outputResolution);
    const presets = QUALITY_PRESETS.map((preset, index) => ({ ...preset, bitrate: bitrates[index] }));
    return nearestPresetIndex(presets, Number(mbps) || bitrates[1], "bitrate");
  };

  const qualityIndexForId = (id) => {
    const index = QUALITY_PRESETS.findIndex((preset) => preset.id === id);
    return index >= 0 ? index : 1;
  };

  const smoothnessPreset = (index) =>
    SMOOTHNESS_PRESETS[Math.max(0, Math.min(SMOOTHNESS_PRESETS.length - 1, Math.round(index || 0)))];

  const smoothnessIndexForFps = (fps) =>
    nearestPresetIndex(SMOOTHNESS_PRESETS, Number(fps) || SMOOTHNESS_PRESETS[1].fps, "fps");

  const outputResolutionOption = (id) =>
    OUTPUT_RESOLUTION_OPTIONS.find((option) => option.id === id) || OUTPUT_RESOLUTION_OPTIONS[0];

  const captureSourceLabel = (settings) => {
    switch (settings && settings.capture_mode) {
      case "window_title":
        return settings.window_title ? `Window: ${settings.window_title}` : "Window";
      case "display_region":
        return "Display region";
      default:
        return "Desktop";
    }
  };

  const clampTime = (value, duration) => {
    // Unknown duration (metadata not loaded yet) must not clamp seeks to zero.
    const max = duration > 0 ? duration : Number.MAX_SAFE_INTEGER;
    return Math.max(0, Math.min(max, value));
  };

  const SEEK_CONFIRM_TOLERANCE_S = 0.1;

  const createLogicalSeekState = () => ({
    targetTime: null,
    sourceGeneration: 0,
    metadataGeneration: null,
    // Provenance of `targetTime`: "user" for an explicit reposition, "assignment"
    // for a source's own resume seek. Only a user reposition may bypass the
    // sidecar drift tolerance — see `sidecarRealignmentForced`.
    targetSource: null,
  });

  const requestLogicalSeek = (state, time, duration) => {
    if (!Number.isFinite(time)) return state;
    return { ...state, targetTime: clampTime(time, duration), targetSource: "user" };
  };

  const beginSourceAssignment = (state, sourceGeneration, resumeTime, duration) => {
    const carried = Number.isFinite(state && state.targetTime);
    const requested = carried
      ? state.targetTime
      : Number.isFinite(resumeTime) ? resumeTime : 0;
    return {
      targetTime: clampTime(requested, duration),
      sourceGeneration,
      metadataGeneration: null,
      // A carried user target keeps its provenance across the replacement;
      // relabelling it would silently downgrade a real reposition.
      targetSource: carried && state.targetSource === "user" ? "user" : "assignment",
    };
  };

  const metadataSeekDecision = (state, sourceGeneration, duration) => {
    if (!state || state.sourceGeneration !== sourceGeneration) {
      return { state, applyTime: null, confirmed: false };
    }
    const targetTime = Number.isFinite(state.targetTime)
      ? clampTime(state.targetTime, duration)
      : null;
    const next = { ...state, targetTime, metadataGeneration: sourceGeneration };
    return { state: next, applyTime: targetTime, confirmed: false };
  };

  const seekedDecision = (state, sourceGeneration, currentTime, duration) => {
    if (!state
        || state.sourceGeneration !== sourceGeneration
        || state.metadataGeneration !== sourceGeneration
        || !Number.isFinite(state.targetTime)
        || !Number.isFinite(currentTime)) {
      return { state, applyTime: null, confirmed: false, confirmedSource: null };
    }
    const targetTime = clampTime(state.targetTime, duration);
    if (Math.abs(currentTime - targetTime) <= SEEK_CONFIRM_TOLERANCE_S) {
      return {
        state: { ...state, targetTime: null, targetSource: null },
        applyTime: null,
        confirmed: true,
        // Reported so the caller can force a sidecar realignment for a user
        // reposition while leaving settlement to the ordinary tolerance path.
        confirmedSource: state.targetSource || null,
      };
    }
    // A completion for a superseded position: re-apply the current target and
    // keep its provenance, so only the real arrival can confirm it.
    return {
      state: { ...state, targetTime },
      applyTime: targetTime,
      confirmed: false,
      confirmedSource: null,
    };
  };

  /// Whether a confirmed seek justifies bypassing the sidecar drift tolerance.
  ///
  /// `forceSeek` skips the tolerance entirely, so forcing on *every* video
  /// `seeked` meant the initial source settlement re-seeked already-audible
  /// sidecars backward for no correction — measured at ~20 ms against a 0.5 s
  /// tolerance, landing back where it started. A user reposition still needs the
  /// bypass: after a scrub the sidecar is far away and tolerance alone would
  /// leave it there.
  const sidecarRealignmentForced = (confirmedSource) => confirmedSource === "user";

  const logicalPlaybackTime = (state, currentTime, duration) => {
    const time = Number.isFinite(state && state.targetTime)
      ? state.targetTime
      : Number.isFinite(currentTime) ? currentTime : 0;
    return clampTime(time, duration);
  };

  const relativeSeekTarget = (currentTime, logicalTarget, delta, duration) => {
    const base = Number.isFinite(logicalTarget)
      ? logicalTarget
      : Number.isFinite(currentTime) ? currentTime : 0;
    return clampTime(base + (Number.isFinite(delta) ? delta : 0), duration);
  };

  const percentFor = (time, duration) => {
    if (!duration) return 0;
    return Math.max(0, Math.min(100, (time / duration) * 100));
  };

  const timelineTime = (clientX, rectLeft, rectWidth, duration) => {
    const x = Math.max(0, Math.min(rectWidth, clientX - rectLeft));
    return clampTime((x / rectWidth) * duration, duration);
  };

  // --- Timeline zoom ---
  // The timeline shows a window [viewStart, viewStart + viewSpan] of the clip.
  // viewSpan === duration is fully zoomed out; smaller spans zoom in. Positions
  // are percentages of the visible window, so off-window content lands outside
  // 0–100% and is clipped by the track. Times stay absolute throughout.

  // Where `time` sits in the visible window, as a percent. Unclamped on purpose:
  // points outside the window return <0 or >100 so the caller can clip them.
  const percentForView = (time, viewStart, viewSpan) => {
    if (!(viewSpan > 0)) return 0;
    return ((time - viewStart) / viewSpan) * 100;
  };

  // Pointer x -> clip time, accounting for the visible window (the zoomed-in
  // counterpart of timelineTime).
  const timelineTimeView = (clientX, rectLeft, rectWidth, viewStart, viewSpan, duration) => {
    if (!(rectWidth > 0)) return clampTime(viewStart, duration);
    const x = Math.max(0, Math.min(rectWidth, clientX - rectLeft));
    return clampTime(viewStart + (x / rectWidth) * viewSpan, duration);
  };

  // Normalize a stored window to a valid one for the current duration: a span of
  // 0 (or any value >= duration) means "whole clip", and the start is pinned so
  // the window never runs past either end.
  const clampView = (viewStart, viewSpan, duration) => {
    if (!(duration > 0)) return { start: 0, span: 0 };
    const span = viewSpan > 0 ? Math.min(viewSpan, duration) : duration;
    const start = Math.max(0, Math.min(duration - span, Number.isFinite(viewStart) ? viewStart : 0));
    return { start, span };
  };

  // Zoom the window by `factor` (>1 zooms out, <1 zooms in) while keeping the
  // clip time under `anchorFrac` (0–1 across the window, e.g. the cursor) fixed.
  const zoomView = (viewStart, viewSpan, duration, anchorFrac, factor, minSpan = MIN_VIEW_SPAN_S) => {
    if (!(duration > 0)) return { start: 0, span: 0 };
    const cur = clampView(viewStart, viewSpan, duration);
    const floor = Math.min(minSpan, duration);
    const nextSpan = Math.max(floor, Math.min(duration, cur.span * factor));
    const frac = Math.max(0, Math.min(1, anchorFrac));
    const anchorTime = cur.start + frac * cur.span;
    return clampView(anchorTime - frac * nextSpan, nextSpan, duration);
  };

  // Slide the window by deltaSeconds, keeping its span. clampView pins it inside
  // the clip, so panning into an edge simply stops and panning while zoomed out
  // (span >= duration) is a natural no-op.
  const panView = (viewStart, viewSpan, duration, deltaSeconds) =>
    clampView(
      (Number.isFinite(viewStart) ? viewStart : 0) +
        (Number.isFinite(deltaSeconds) ? deltaSeconds : 0),
      viewSpan,
      duration
    );

  // Drag one edge of the window to a clip time, leaving the other edge fixed —
  // the navigator's resize grips. Honors the min span and clamps to the clip.
  const setViewEdge = (viewStart, viewSpan, duration, edge, timeAtEdge, minSpan = MIN_VIEW_SPAN_S) => {
    if (!(duration > 0)) return { start: 0, span: 0 };
    const cur = clampView(viewStart, viewSpan, duration);
    const floor = Math.min(minSpan, duration);
    const t = clampTime(timeAtEdge, duration);
    if (edge === "left") {
      const right = cur.start + cur.span; // fixed
      const start = Math.min(t, right - floor);
      return clampView(start, right - start, duration);
    }
    const left = cur.start; // fixed
    const end = Math.max(t, left + floor);
    return clampView(left, end - left, duration);
  };

  // A window that frames [startS, endS] with padding on each side, floored to the
  // min span and clamped to the clip — the "zoom to selection" target.
  const viewForRange = (startS, endS, duration, paddingFrac = 0.05, minSpan = MIN_VIEW_SPAN_S) => {
    if (!(duration > 0)) return { start: 0, span: 0 };
    const lo = Math.min(startS, endS);
    const hi = Math.max(startS, endS);
    const rangeSpan = Math.max(0, hi - lo);
    const padded = rangeSpan + rangeSpan * paddingFrac * 2;
    const span = Math.max(Math.min(minSpan, duration), Math.min(duration, padded));
    const center = (lo + hi) / 2;
    return clampView(center - span / 2, span, duration);
  };

  // Keep the playhead in view during playback. "page" re-pages only once the
  // playhead leaves the window; "smooth" centers it; "none" leaves it alone.
  // Zoomed out (span >= duration) is always a no-op.
  const followView = (viewStart, viewSpan, duration, playhead, mode) => {
    const cur = clampView(viewStart, viewSpan, duration);
    if (!(duration > 0) || cur.span >= duration) return cur;
    const ph = clampTime(playhead, duration);
    const end = cur.start + cur.span;
    if (mode === "smooth") {
      return clampView(ph - cur.span * 0.5, cur.span, duration);
    }
    if (mode === "page") {
      if (ph < cur.start || ph > end) {
        return clampView(ph - cur.span * 0.1, cur.span, duration);
      }
    }
    return cur;
  };

  // Snap t to the nearest candidate within a pixel tolerance (converted to
  // seconds via pxPerSec, so the feel is constant at any zoom). Returns the
  // possibly-snapped time and what it hit.
  const snapTime = (t, candidates, pxPerSec, thresholdPx = SNAP_THRESHOLD_PX) => {
    if (!(pxPerSec > 0) || !candidates || !candidates.length) {
      return { t, snapped: false, target: null };
    }
    const tolerance = thresholdPx / pxPerSec;
    let best = null;
    let bestDist = Infinity;
    for (const c of candidates) {
      const d = Math.abs(c - t);
      if (d < bestDist) {
        bestDist = d;
        best = c;
      }
    }
    if (best != null && bestDist <= tolerance) {
      return { t: best, snapped: true, target: best };
    }
    return { t, snapped: false, target: null };
  };

  // Salient times a drag can snap to: clip ends, markers, the playhead, and the
  // trim edges. `excludeEdge` ("in"|"out"|"playhead", or an array) drops the
  // element being dragged so it never snaps to its own previous position.
  const snapCandidates = (duration, markers, playhead, trimStart, trimEnd, excludeEdge) => {
    const exclude = new Set(
      Array.isArray(excludeEdge) ? excludeEdge : excludeEdge ? [excludeEdge] : []
    );
    const out = [0];
    if (duration > 0) out.push(duration);
    for (const m of markers || []) {
      if (m && Number.isFinite(m.t_s)) out.push(m.t_s);
    }
    if (!exclude.has("playhead") && Number.isFinite(playhead)) out.push(playhead);
    if (!exclude.has("in") && Number.isFinite(trimStart)) out.push(trimStart);
    if (!exclude.has("out") && Number.isFinite(trimEnd)) out.push(trimEnd);
    return [...new Set(out)].sort((a, b) => a - b);
  };

  // Seconds per frame from a frame rate; falls back when fps is unknown (HTML
  // <video> doesn't expose it) so fine-stepping always does something sane.
  const frameStep = (fps, fallback = 1 / 30) =>
    Number.isFinite(fps) && fps > 0 ? 1 / fps : fallback;

  // Sorted-unique navigation stops for Up/Down jumps: clip ends, trim edges, and
  // markers — shaped like markers ({t_s}) so nextMarker/prevMarker can traverse.
  const editPoints = (markers, trimStart, trimEnd, duration) => {
    const out = [0];
    if (duration > 0) out.push(duration);
    if (Number.isFinite(trimStart)) out.push(trimStart);
    if (Number.isFinite(trimEnd)) out.push(trimEnd);
    for (const m of markers || []) {
      if (m && Number.isFinite(m.t_s)) out.push(m.t_s);
    }
    return [...new Set(out)].sort((a, b) => a - b).map((t) => ({ t_s: t }));
  };

  const resolveTrim = (start, end, duration) => {
    let nextStart = clampTime(Number.isFinite(start) ? start : 0, duration);
    let nextEnd = clampTime(
      Number.isFinite(end) && end > 0 ? end : duration,
      duration
    );
    if (duration && nextEnd <= nextStart) {
      nextEnd = Math.min(duration, nextStart + MIN_TRIM_GAP_S);
    }
    if (nextEnd <= nextStart) {
      nextStart = Math.max(0, nextEnd - MIN_TRIM_GAP_S);
    }
    return { start: nextStart, end: nextEnd };
  };

  const quickTrimRange = (playhead, duration, windowS = QUICK_TRIM_WINDOW_S) => {
    if (!(duration > 0)) return { start: 0, end: 0 };
    const span = Math.min(duration, Math.max(MIN_TRIM_GAP_S, windowS));
    const center = clampTime(Number.isFinite(playhead) ? playhead : 0, duration);
    const start = Math.max(0, Math.min(duration - span, center - span / 2));
    return { start, end: start + span };
  };

  const trimPlaybackStopTime = (trimMode, paused, currentTime, trimEnd) =>
    trimMode && !paused && Number.isFinite(currentTime) && currentTime >= trimEnd ? trimEnd : null;

  const trimDrag = (kind, time, start, end, duration) => {
    if (kind === "in") {
      return resolveTrim(Math.min(time, end - MIN_TRIM_GAP_S), end, duration);
    }
    return resolveTrim(start, Math.max(time, start + MIN_TRIM_GAP_S), duration);
  };

  // Slide the whole selection so its start lands at newStart, preserving its
  // length and clamping so it never runs past either end of the clip.
  const slideTrim = (start, end, newStart, duration) => {
    const len = Math.max(0, end - start);
    const max = (duration > 0 ? duration : len) - len;
    const s = Math.max(0, Math.min(max, Number.isFinite(newStart) ? newStart : start));
    return { start: s, end: s + len };
  };

  const trimSummary = (start, end) =>
    `keeps ${fmtTenths(start)} – ${fmtTenths(end)} · ${Math.max(0, end - start).toFixed(1)} s`;

  const nextMarker = (markers, currentS) => {
    if (!markers.length) return null;
    for (const m of markers) {
      if (m.t_s > currentS + MARKER_EPSILON_S) return m;
    }
    return markers[0];
  };

  const prevMarker = (markers, currentS) => {
    if (!markers.length) return null;
    for (let i = markers.length - 1; i >= 0; i--) {
      if (markers[i].t_s < currentS - MARKER_EPSILON_S) return markers[i];
    }
    return markers[markers.length - 1];
  };

  const markerSummary = (markers) => {
    if (!markers.length) return "no markers";
    return markers.length === 1 ? "1 marker" : `${markers.length} markers`;
  };

  const rawPlays = (plays) => Array.isArray(plays) ? plays : [];

  const playTitle = (play) => {
    const artist = String(play && play.artist || "").trim();
    const title = String(play && play.title || "").trim();
    const difficulty = String(play && play.difficulty || "").trim();
    const song = artist && title ? `${artist} - ${title}` : (title || artist || "osu! play");
    return difficulty ? `${song} [${difficulty}]` : song;
  };

  const playArtistTitle = (play) => {
    const artist = String(play && play.artist || "").trim();
    const title = String(play && play.title || "").trim();
    return artist && title ? `${artist} - ${title}` : (title || artist || "osu! play");
  };

  const playDifficulty = (play) => String(play && play.difficulty || "").trim();

  const displayableMods = (play) => {
    const ignored = new Set(["CL", "NM", "NOMOD"]);
    return Array.isArray(play && play.mods)
      ? play.mods
        .map((mod) => String(mod || "").trim().toUpperCase())
        .filter((mod) => mod && !ignored.has(mod))
      : [];
  };

  const playMods = (play) => {
    const mods = displayableMods(play);
    return mods.length ? `+${mods.join("")}` : "";
  };

  const playStarRating = (play) => {
    const raw = Number(play && (play.star_rating || play.starRating));
    return Number.isFinite(raw) && raw > 0 ? `${raw.toFixed(2)}★` : "";
  };

  const playCoverUrl = (play) => {
    const explicit = String(play && (play.cover_url || play.coverUrl) || "").trim();
    if (explicit) return explicit;
    const beatmapsetId = Number(play && (play.beatmapset_id || play.beatmapsetId));
    return Number.isFinite(beatmapsetId) && beatmapsetId > 0
      ? `https://assets.ppy.sh/beatmaps/${Math.trunc(beatmapsetId)}/covers/list.jpg`
      : "";
  };

  const playAccuracy = (play) => {
    const raw = Number(play && play.accuracy);
    if (!Number.isFinite(raw) || raw < 0) return "";
    const percent = raw <= 1 ? raw * 100 : raw;
    return `${percent.toFixed(2)}%`;
  };

  const playPp = (play) => {
    const pp = Number(play && play.pp);
    return Number.isFinite(pp) && pp > 0 ? `${Math.round(pp)}pp` : "";
  };

  const playRank = (play) => String(play && play.rank || "").trim();

  const playIncomplete = (play) => {
    if (play && play.passed) return "";
    if (playPp(play)) return "";
    return playRank(play).toUpperCase() === "F" ? "" : "Incomplete";
  };

  const playDetails = (play) => {
    const parts = [
      playIncomplete(play) || (play && play.passed ? "Passed" : "Failed"),
      playRank(play),
      playAccuracy(play),
      playPp(play),
      playMods(play),
    ].filter(Boolean);
    return parts.join(" · ");
  };

  const normalizedPlay = (play, index, duration = 0) => {
    const startRaw = Number(play && play.t_start_s);
    if (!Number.isFinite(startRaw)) return null;
    const endRaw = Number(play && play.t_end_s);
    const hasEnd = Number.isFinite(endRaw);
    const start = clampTime(startRaw, duration);
    const end = hasEnd ? Math.max(start, clampTime(endRaw, duration)) : start;
    const externalId = String(play && play.external_id || play && play.id || `play-${index}`);
    return {
      externalId,
      play,
      start,
      end,
      hasEnd,
      title: playTitle(play),
      details: playDetails(play),
      estimated: Boolean(play && play.derived_start),
      incomplete: Boolean(playIncomplete(play)),
    };
  };

  const normalizedPlays = (plays, duration = 0) => rawPlays(plays)
    .map((play, index) => normalizedPlay(play, index, duration))
    .filter(Boolean)
    .sort((a, b) => a.start - b.start || a.end - b.end || a.externalId.localeCompare(b.externalId));

  const playBlocks = (plays, duration = 0) => normalizedPlays(plays, duration).map((play) => ({
    ...play,
    leftPct: percentFor(play.start, duration),
    widthPct: play.hasEnd ? Math.max(0, percentFor(play.end, duration) - percentFor(play.start, duration)) : 0,
  }));

  const playRailItem = (play) => {
    const normalized = normalizedPlay(play, 0, 0);
    if (!normalized) return { title: "osu! play", meta: "", time: "" };
    const time = normalized.hasEnd
      ? `${fmtTenths(normalized.start)}-${fmtTenths(normalized.end)}`
      : fmtTenths(normalized.start);
    return {
      title: normalized.title,
      artistTitle: playArtistTitle(play),
      difficulty: playDifficulty(play),
      mods: playMods(play),
      starRating: playStarRating(play),
      coverUrl: playCoverUrl(play),
      rank: playRank(play),
      pp: playPp(play),
      accuracy: playAccuracy(play),
      meta: [
        playRank(play),
        playIncomplete(play),
        playPp(play),
        playAccuracy(play),
      ].filter(Boolean).join(" ▸ "),
      time,
    };
  };

  const playExportRange = (play) => {
    const normalized = normalizedPlay(play, 0, 0);
    if (!normalized || !normalized.hasEnd || !(normalized.end > normalized.start)) return null;
    return { start: normalized.start, end: normalized.end };
  };

  const playSummary = (plays) => {
    const count = rawPlays(plays).length;
    if (!count) return "no submitted plays";
    return count === 1 ? "1 submitted play" : `${count} submitted plays`;
  };

  const playResultSummary = (plays) => {
    const list = rawPlays(plays);
    const passed = list.filter((play) => play && play.passed).length;
    const incomplete = list.filter((play) => playIncomplete(play)).length;
    const failed = Math.max(0, list.length - passed - incomplete);
    const parts = [];
    if (passed) parts.push(`${passed} ${passed === 1 ? "pass" : "passes"}`);
    if (incomplete) parts.push(`${incomplete} incomplete`);
    if (failed) parts.push(`${failed} ${failed === 1 ? "fail" : "fails"}`);
    return parts.join(" · ");
  };

  const playActiveIndex = (plays, currentTime, selectedIndex = -1) => {
    const list = normalizedPlays(plays, 0);
    const current = Number(currentTime);
    if (!Number.isFinite(current)) return -1;
    const selected = Number(selectedIndex);
    if (Number.isInteger(selected) && selected >= 0 && selected < list.length) return selected;
    let active = -1;
    for (let index = 0; index < list.length; index += 1) {
      const play = list[index];
      const contains = play.hasEnd
        ? current >= play.start && current <= play.end
        : Math.abs(current - play.start) <= MARKER_EPSILON_S;
      if (contains && (active < 0 || play.start >= list[active].start)) active = index;
    }
    return active;
  };

  const gameEventActiveIndex = (markers, currentTime, selectedIndex = -1) => {
    const list = Array.isArray(markers) ? markers : [];
    if (!list.length) return -1;
    const t = Number.isFinite(currentTime) ? currentTime : 0;
    const selected = Number(selectedIndex);
    if (Number.isInteger(selected) && selected >= 0 && selected < list.length) {
      const selectedTime = Number(list[selected].t_s) || 0;
      if (t < selectedTime - 0.15) return selected;
    }
    let active = -1;
    for (let i = 0; i < list.length; i += 1) {
      if ((Number(list[i].t_s) || 0) <= t + 0.15) active = i;
      else break;
    }
    return active;
  };

  const audioTrackId = (track) => String((track && track.id) || "");

  const audioTrackKind = (track) => String((track && track.kind) || "");

  const normalizedAudioTracks = (tracks) => Array.isArray(tracks) ? tracks : [];

  const isMixedOutputTrack = (track) =>
    audioTrackId(track) === "output" && audioTrackKind(track) === "output";

  const isProcessOutputTrack = (track) =>
    audioTrackKind(track) === "process_output" || audioTrackId(track).startsWith("process:");

  const processOutputTracks = (tracks) => normalizedAudioTracks(tracks).filter(isProcessOutputTrack);

  const hasSplitOutputTracks = (tracks) =>
    normalizedAudioTracks(tracks).some(isMixedOutputTrack) && processOutputTracks(tracks).length > 0;

  const audioIdSet = (selectedIds) =>
    new Set((Array.isArray(selectedIds) ? selectedIds : []).map((id) => String(id)));

  const selectedAudioTrackIds = (tracks, selectedIds) => {
    const selected = audioIdSet(selectedIds);
    const splitOutput = hasSplitOutputTracks(tracks);
    const processOutputSelected = processOutputTracks(tracks)
      .some((track) => selected.has(audioTrackId(track)));
    return normalizedAudioTracks(tracks)
      .filter((track) => {
        const id = audioTrackId(track);
        return id && selected.has(id)
          && !(splitOutput && processOutputSelected && isMixedOutputTrack(track));
      })
      .map(audioTrackId);
  };

  const defaultAudioTrackIds = (tracks) => {
    const splitOutput = hasSplitOutputTracks(tracks);
    return normalizedAudioTracks(tracks)
      .filter((track) => audioTrackId(track) && !(splitOutput && isProcessOutputTrack(track)))
      .map(audioTrackId);
  };

  const directPlaybackAudioTrackIds = (tracks) => {
    const normalized = normalizedAudioTracks(tracks);
    const streamZero = normalized.find((track) =>
      track && track.track_index === 0 && audioTrackId(track));
    const direct = streamZero || normalized.find((track) => audioTrackId(track));
    return direct ? [audioTrackId(direct)] : [];
  };

  const selectedReviewAudioTrackIds = (tracks, selectedIds) => {
    const selected = audioIdSet(selectedIds);
    const valid = normalizedAudioTracks(tracks).map(audioTrackId).filter(Boolean);
    return valid.filter((id) => selected.has(id));
  };

  const reviewSelectionNeedsPreview = (tracks, selectedIds) => {
    const selected = selectedReviewAudioTrackIds(tracks, selectedIds);
    const direct = directPlaybackAudioTrackIds(tracks);
    return selected.length !== direct.length
      || selected.some((id, index) => id !== direct[index]);
  };

  const reviewAudioTrackRowState = (track, tracks, selectedIds) => ({
    checked: selectedReviewAudioTrackIds(tracks, selectedIds).includes(audioTrackId(track)),
    indeterminate: false,
  });

  const applyReviewAudioTrackToggle = (tracks, selectedIds, trackId, checked) => {
    const allTracks = normalizedAudioTracks(tracks);
    const selected = new Set(selectedReviewAudioTrackIds(allTracks, selectedIds));
    const track = allTracks.find((candidate) => audioTrackId(candidate) === String(trackId));
    if (!track) return [...selected];
    if (checked && isMixedOutputTrack(track) && hasSplitOutputTracks(allTracks)) {
      for (const processTrack of processOutputTracks(allTracks)) {
        selected.delete(audioTrackId(processTrack));
      }
    }
    if (checked && isProcessOutputTrack(track) && hasSplitOutputTracks(allTracks)) {
      for (const candidate of allTracks) {
        if (isMixedOutputTrack(candidate)) selected.delete(audioTrackId(candidate));
      }
    }
    if (checked) selected.add(audioTrackId(track));
    else selected.delete(audioTrackId(track));
    return selectedReviewAudioTrackIds(allTracks, [...selected]);
  };

  const reviewAudioTrackSelectedRowCount = (tracks, selectedIds) =>
    normalizedAudioTracks(tracks).filter((track) =>
      reviewAudioTrackRowState(track, tracks, selectedIds).checked
    ).length;

  const selectionNeedsPreview = (tracks, selectedIds) => {
    const sourceIds = normalizedAudioTracks(tracks).map(audioTrackId).filter(Boolean);
    if (sourceIds.length > 1) return true;
    const selected = selectedAudioTrackIds(tracks, selectedIds);
    return sourceIds.length !== selected.length || sourceIds.some((id, index) => id !== selected[index]);
  };

  const audioTrackRowState = (track, tracks, selectedIds) => {
    const selected = audioIdSet(selectedIds);
    if (hasSplitOutputTracks(tracks) && isMixedOutputTrack(track)) {
      if (selected.has(audioTrackId(track))) {
        return { checked: true, indeterminate: false };
      }
      const processIds = processOutputTracks(tracks).map(audioTrackId).filter(Boolean);
      const checkedCount = processIds.filter((id) => selected.has(id)).length;
      return {
        checked: processIds.length > 0 && checkedCount === processIds.length,
        indeterminate: checkedCount > 0 && checkedCount < processIds.length,
      };
    }
    return { checked: selected.has(audioTrackId(track)), indeterminate: false };
  };

  const applyAudioTrackToggle = (tracks, selectedIds, trackId, checked) => {
    const allTracks = normalizedAudioTracks(tracks);
    const selected = audioIdSet(selectedIds);
    const track = allTracks.find((candidate) => audioTrackId(candidate) === String(trackId));
    if (!track) return selectedAudioTrackIds(allTracks, [...selected]);

    if (hasSplitOutputTracks(allTracks) && isMixedOutputTrack(track)) {
      selected.delete(audioTrackId(track));
      for (const processTrack of processOutputTracks(allTracks)) {
        const id = audioTrackId(processTrack);
        if (!id) continue;
        if (checked) selected.add(id);
        else selected.delete(id);
      }
    } else if (checked) {
      selected.add(audioTrackId(track));
    } else {
      selected.delete(audioTrackId(track));
    }

    return selectedAudioTrackIds(allTracks, [...selected]);
  };

  const audioTrackSelectedRowCount = (tracks, selectedIds) =>
    normalizedAudioTracks(tracks).filter((track) =>
      audioTrackRowState(track, tracks, selectedIds).checked
    ).length;

  // EventKind variant name -> visual category. Unknown kinds degrade to info.
  // Categories also drive the review filters, so annotation echoes that arrive
  // alongside a real kill event (FirstBlood rides with its ChampionKill) must
  // not share the kill category or they'd render twice.
  const DEFAULT_MARKER_KINDS = {
    ChampionKill: "kill",
    ChampionAssist: "assist",
    ChampionDeath: "death",
    FirstBlood: "spree",
    Multikill: "spree",
    Ace: "spree",
    DragonKill: "objective",
    HeraldKill: "objective",
    BaronKill: "objective",
    TurretKilled: "structure",
    InhibKilled: "structure",
    FirstBrick: "structure",
    // User-placed, not game-derived. It has its own category so the review
    // filters (which key on category) can never hide it.
    Bookmark: "bookmark",
  };
  const DEFAULT_MARKER_CATEGORIES = {
    kill: { singular: "kill", plural: "kills", glyph: "✕" },
    assist: { singular: "assist", plural: "assists", glyph: "+" },
    death: { singular: "death", plural: "deaths", glyph: "✕" },
    spree: { singular: "spree", plural: "sprees", glyph: "★" },
    objective: { singular: "objective", plural: "objectives", glyph: "◆" },
    structure: { singular: "structure", plural: "structures", glyph: "▣" },
    bookmark: { singular: "bookmark", plural: "bookmarks", glyph: "⚑" },
    info: { singular: "event", plural: "events", glyph: "•" },
  };
  const BOOKMARK_KIND = "Bookmark";

  const PRESENTATION_KEY = /^[A-Za-z][A-Za-z0-9_-]{0,63}$/;
  const ownObjectValue = (object, key) => typeof key === "string"
    && PRESENTATION_KEY.test(key)
    && key !== "constructor"
    && key !== "prototype"
    && typeof object === "object"
    && object !== null
    && Object.prototype.hasOwnProperty.call(object, key)
      ? object[key]
      : undefined;

  const markerKindConfig = (kind, presentation) => {
    const configured = ownObjectValue(presentation && presentation.marker_kinds, kind);
    return configured && typeof configured === "object" ? configured : {};
  };

  const markerCategoryConfig = (category, presentation) => {
    const configured = ownObjectValue(presentation && presentation.marker_categories, category);
    return configured && typeof configured === "object" ? configured : {};
  };

  const markerCategory = (kind, presentation) => {
    const configured = markerKindConfig(kind, presentation);
    return String(configured.category || ownObjectValue(DEFAULT_MARKER_KINDS, kind) || "info");
  };

  const markerCategoryMeta = (category, presentation) => {
    const configured = markerCategoryConfig(category, presentation);
    const fallback = ownObjectValue(DEFAULT_MARKER_CATEGORIES, category) || DEFAULT_MARKER_CATEGORIES.info;
    return {
      singular: String(configured.singular || fallback.singular),
      plural: String(configured.plural || fallback.plural),
      glyph: String(configured.glyph || fallback.glyph),
    };
  };

  const markerStyle = (kind, presentation = null) => {
    const configured = markerKindConfig(kind, presentation);
    const cls = markerCategory(kind, presentation);
    const category = markerCategoryMeta(cls, presentation);
    return { glyph: String(configured.glyph || category.glyph), cls };
  };

  const markerDigest = (markers, presentation = null) => {
    const counts = {};
    for (const m of markers) {
      const cls = markerCategory(m.kind, presentation);
      counts[cls] = (counts[cls] || 0) + 1;
    }
    const configuredOrder = Object.keys((presentation && presentation.marker_categories) || {});
    const order = [
      ...configuredOrder,
      ...Object.keys(DEFAULT_MARKER_CATEGORIES).filter((cls) => !configuredOrder.includes(cls)),
    ];
    return order
      .filter((cls) => counts[cls])
      .map((cls) => {
        const meta = markerCategoryMeta(cls, presentation);
        return `${counts[cls]} ${counts[cls] === 1 ? meta.singular : meta.plural}`;
      })
      .join(" · ");
  };

  const playerSummaryLabel = (summary) => {
    if (!summary) return "";
    const champion = String(summary.champion_name || "").trim();
    if (!champion) return "";
    const kda = playerSummaryKda(summary);
    if (!kda) return "";
    return `${champion} | ${kda}`;
  };

  const playerSummaryKda = (summary) => {
    if (!summary) return "";
    const stat = (value) => {
      const n = Number(value);
      return Number.isFinite(n) && n >= 0 ? Math.floor(n) : 0;
    };
    return `${stat(summary.kills)}/${stat(summary.deaths)}/${stat(summary.assists)}`;
  };

  const playerSummaryKdaRatio = (summary) => {
    if (!summary) return "";
    const stat = (value) => {
      const n = Number(value);
      return Number.isFinite(n) && n >= 0 ? Math.floor(n) : 0;
    };
    const kills = stat(summary.kills);
    const deaths = stat(summary.deaths);
    const assists = stat(summary.assists);
    const impact = kills + assists;
    if (deaths === 0 && impact > 0) return "Perfect KDA";
    const ratio = deaths === 0 ? 0 : impact / deaths;
    return `${ratio.toFixed(2)} KDA`;
  };

  const summaryStat = (value) => {
    const n = Number(value);
    return Number.isFinite(n) && n >= 0 ? String(Math.floor(n)) : "";
  };

  const playerSummaryValue = (summary, source) => {
    if (!summary || typeof source !== "string" || !source.startsWith("player_summary.")) {
      return "";
    }
    const key = source.slice("player_summary.".length);
    if (!/^[a-z0-9_]+$/i.test(key)) return "";
    const value = summary[key];
    if (typeof value === "string") return value.trim();
    if (typeof value === "number") return summaryStat(value);
    return "";
  };

  const metadataAssetKey = (value) => String(value || "")
    .trim()
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "");

  const dataDragonLookupKey = (value) => String(value || "")
    .trim()
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "");

  const dataDragonChampionKey = (value, aliases = {}) => {
    const raw = String(value || "").trim();
    if (!raw) return "";
    const lookup = dataDragonLookupKey(raw);
    if (aliases && typeof aliases === "object") {
      for (const [alias, key] of Object.entries(aliases)) {
        const resolved = String(key || "").trim();
        if (dataDragonLookupKey(alias) === lookup && /^[A-Za-z0-9]+$/.test(resolved)) {
          return resolved;
        }
      }
    }
    return (raw.match(/[A-Za-z0-9]+/g) || [])
      .map((part) => part ? `${part[0].toUpperCase()}${part.slice(1)}` : "")
      .join("");
  };

  const dataDragonVersion = (options) => {
    const version = options
      && options.data_dragon
      && typeof options.data_dragon.version === "string"
      ? options.data_dragon.version.trim()
      : "";
    return /^\d+\.\d+\.\d+$/.test(version) ? version : "";
  };

  const dataDragonAsset = (segment, assetKey, options, keyPattern) => {
    const version = dataDragonVersion(options);
    const safeSegment = String(segment || "").trim();
    const safeKey = String(assetKey || "").trim();
    if (!version || !/^[a-z]+$/.test(safeSegment) || !keyPattern.test(safeKey)) return "";
    return `https://ddragon.leagueoflegends.com/cdn/${version}/img/${safeSegment}/${safeKey}.png`;
  };

  const playerSummaryArray = (summary, source, fallbackKey) => {
    if (!summary) return [];
    const key = typeof source === "string" && source.startsWith("player_summary.")
      ? source.slice("player_summary.".length)
      : fallbackKey;
    if (!/^[a-z0-9_]+$/i.test(key)) return [];
    return Array.isArray(summary[key]) ? summary[key] : [];
  };

  // Hextech Flashtraption replaces Flash and has no Data Dragon spell art, so
  // map Hexflash names/keys onto SummonerFlash for both fresh and legacy sidecars.
  const summonerSpellAssetKey = (name, assetKey) => {
    const key = String(assetKey || "").trim();
    if (key.startsWith("SummonerFlash")) return "SummonerFlash";
    const lookup = dataDragonLookupKey(name);
    if (lookup === "flash" || lookup === "hexflash" || lookup === "hextechflashtraption") {
      return "SummonerFlash";
    }
    return key;
  };

  const summaryIconItem = (entry, type, field, options) => {
    if (!entry || typeof entry !== "object") return null;
    const provider = String(field.asset_provider || "").trim();
    if (type === "summoner_spells") {
      const value = String(entry.name || entry.display_name || "").trim();
      const assetKey = summonerSpellAssetKey(value, entry.asset_key);
      if (!value && !assetKey) return null;
      const item = { value: value || assetKey, assetKey };
      if (provider === "riot_data_dragon_summoner_spell") {
        const asset = dataDragonAsset("spell", assetKey, options, /^Summoner[A-Za-z0-9]+$/);
        if (asset) item.asset = asset;
      }
      return item;
    }
    if (type === "item_build") {
      const id = Number(entry.id);
      const assetKey = Number.isFinite(id) && id > 0 ? String(Math.floor(id)) : "";
      const value = String(entry.name || assetKey || "").trim();
      if (!assetKey || !value) return null;
      const item = { value, assetKey };
      if (provider === "riot_data_dragon_item") {
        const asset = dataDragonAsset("item", assetKey, options, /^[0-9]+$/);
        if (asset) item.asset = asset;
      }
      return item;
    }
    return null;
  };

  const playerSummaryFields = (summary, fields = [], options = {}) => {
    if (!summary || !Array.isArray(fields)) return [];
    const out = [];
    for (const field of fields) {
      if (!field || typeof field !== "object") continue;
      const type = String(field.type || "stat");
      const label = String(field.label || "").trim();
      if (type === "portrait") {
        const value = playerSummaryValue(summary, field.source || "player_summary.champion_name");
        if (value) {
          const assetKey = field.asset_key_format === "data_dragon_champion"
            ? dataDragonChampionKey(value, field.asset_aliases)
            : metadataAssetKey(value);
          const formatted = { type, label, value, assetKey };
          if (typeof field.asset_template === "string" && field.asset_template.includes("{assetKey}")) {
            formatted.asset = field.asset_template.replaceAll("{assetKey}", assetKey);
          } else if (field.asset_provider === "riot_data_dragon_champion_square") {
            const asset = dataDragonAsset("champion", assetKey, options, /^[A-Za-z0-9]+$/);
            if (asset) formatted.asset = asset;
          }
          out.push(formatted);
        }
      } else if (type === "champion") {
        const value = playerSummaryValue(summary, field.source || "player_summary.champion_name");
        if (value) out.push({ type, label, value });
      } else if (type === "kda") {
        const kills = summaryStat(summary.kills);
        const deaths = summaryStat(summary.deaths);
        const assists = summaryStat(summary.assists);
        if (kills || deaths || assists) {
          const formatted = { type, label, value: `${kills || "0"}/${deaths || "0"}/${assists || "0"}` };
          if (field.secondary === "kda_ratio") {
            const secondary = playerSummaryKdaRatio(summary);
            if (secondary) formatted.secondary = secondary;
          }
          out.push(formatted);
        }
      } else if (type === "summoner_spells" || type === "item_build") {
        const fallbackKey = type === "summoner_spells" ? "summoner_spells" : "items";
        const maxItems = Number(field.max_items);
        const limit = Number.isFinite(maxItems) && maxItems > 0 ? Math.floor(maxItems) : 8;
        const items = playerSummaryArray(summary, field.source, fallbackKey)
          .slice(0, limit)
          .map((entry) => summaryIconItem(entry, type, field, options))
          .filter(Boolean);
        if (items.length) out.push({ type, label, items });
      } else if (type === "stat") {
        const value = playerSummaryValue(summary, field.source);
        if (value) out.push({ type, label, value });
      }
    }
    return out;
  };

  const galleryCardIcon = (summary, iconConfig, options = {}) => {
    if (!iconConfig || typeof iconConfig !== "object") return null;
    const type = String(iconConfig.type || "").trim();
    if (type === "asset") {
      const url = String(iconConfig.src || iconConfig.url || "").trim();
      if (!url) return null;
      const label = String(iconConfig.label || "").trim();
      return { type, url, label };
    }
    if (type === "portrait" || type === "player_summary_portrait") {
      const field = { ...iconConfig, type: "portrait" };
      const [portrait] = playerSummaryFields(summary, [field], options);
      if (!portrait || !portrait.asset) return null;
      return {
        type: "portrait",
        url: portrait.asset,
        label: portrait.value || portrait.label || "",
      };
    }
    return null;
  };

  const playerSummaryCsPerMin = (summary, label = "CS/min") => {
    if (!summary) return "";
    const creepScore = Number(summary.creep_score);
    const gameTimeS = Number(summary.game_time_s);
    if (!Number.isFinite(creepScore) || creepScore < 0) return "";
    if (!Number.isFinite(gameTimeS) || gameTimeS <= 0) return "";
    const value = creepScore / (gameTimeS / 60);
    if (!Number.isFinite(value)) return "";
    const suffix = String(label || "CS/min").trim() || "CS/min";
    return `${value.toFixed(1)} ${suffix}`;
  };

  const playerSummaryStatsLabel = (summary, formatConfig) => {
    if (!summary || !formatConfig || typeof formatConfig !== "object") return "";
    if (formatConfig.type !== "player_summary_stats") return "";
    const stats = Array.isArray(formatConfig.stats) ? formatConfig.stats : [];
    const parts = [];
    for (const statConfig of stats) {
      if (!statConfig || typeof statConfig !== "object") continue;
      const type = String(statConfig.type || "").trim();
      if (type === "kda") {
        const kda = playerSummaryKda(summary);
        if (kda) parts.push(kda);
      } else if (type === "cs_per_min") {
        const csPerMin = playerSummaryCsPerMin(summary, statConfig.label);
        if (csPerMin) parts.push(csPerMin);
      }
    }
    const separator = typeof formatConfig.separator === "string" && formatConfig.separator
      ? formatConfig.separator
      : " | ";
    return parts.join(separator);
  };

  const galleryCardPreview = (clip, kind = "", fallbackTitle = "", presentation = null, options = {}) => {
    const gallery = presentation && presentation.gallery && typeof presentation.gallery === "object"
      ? presentation.gallery
      : {};
    const card = gallery.card && typeof gallery.card === "object" ? gallery.card : {};
    const markers = clip && clip.markers && typeof clip.markers === "object" ? clip.markers : {};
    const summary = markers.player_summary || null;
    const plays = Array.isArray(markers.plays) ? markers.plays : [];
    const summaryLabel = gallery.summary === "player_summary_kda"
      ? playerSummaryLabel(summary)
      : (gallery.summary === "osu_set_plays" ? playSummary(plays) : "");
    const detailSummaryLabel = gallery.summary === "osu_set_plays" ? playResultSummary(plays) : summaryLabel;
    const cardSummaryLabel = playerSummaryStatsLabel(summary, card.title_format) || summaryLabel;
    const fallback = String(fallbackTitle || "").trim();
    const clipName = clip && typeof clip.name === "string" ? clip.name.trim() : "";
    const customTitle = clip && typeof clip.title === "string" ? clip.title.trim() : "";
    const legacyTitlePolicy = gallery.full_session_title === "summary" ? "summary_for_full_session" : "clip";
    const titlePolicy = typeof card.title === "string" && card.title.trim()
      ? card.title.trim()
      : legacyTitlePolicy;
    const clipDisplayTitle = customTitle || clipName.replace(/\.(mp4|mov|mkv|webm)$/i, "").trim() || clipName;
    const usesClipTitle = titlePolicy === "clip" || (titlePolicy === "osu_session_summary" && kind !== "session");
    const usesSummaryTitle = cardSummaryLabel
      && (
        titlePolicy === "summary"
        || (titlePolicy === "summary_for_full_session" && kind === "session")
        || (titlePolicy === "osu_session_summary" && kind === "session")
      );
    const clipTitle = usesClipTitle && clipDisplayTitle ? clipDisplayTitle : fallback;
    const out = {
      title: usesSummaryTitle ? cardSummaryLabel : clipTitle,
      titleSource: usesSummaryTitle ? "summary" : "clip",
      summary: detailSummaryLabel,
    };
    const icon = galleryCardIcon(summary, card.icon, options);
    if (icon) out.icon = icon;
    return out;
  };

  const markerLabel = (kind, presentation) => {
    const configured = markerKindConfig(kind, presentation);
    return String(
      configured.label
        || String(kind || "Other").replace(/([a-z])([A-Z])/g, "$1 $2")
    );
  };

  const eventRailIcon = (kind, presentation) => {
    const icons = presentation
      && presentation.event_rail
      && presentation.event_rail.icons
      && typeof presentation.event_rail.icons === "object"
      ? presentation.event_rail.icons
      : null;
    const iconValue = ownObjectValue(icons, kind);
    const configured = typeof iconValue === "string" ? iconValue : "";
    if (configured.trim()) return configured.trim();
    const markerIcon = markerKindConfig(kind, presentation).icon;
    return typeof markerIcon === "string" ? markerIcon.trim() : "";
  };

  const safeMarkerImage = (value) => {
    const image = typeof value === "string" ? value.trim() : "";
    if (/^assets\/markers\/[a-z0-9][a-z0-9-]*\.png$/i.test(image)) return image;
    if (
      image !== "data:image/png;base64,"
      && /^data:image\/png;base64,(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$/.test(image)
    ) return image;
    return "";
  };

  const markerEventText = (marker, presentation) => {
    const label = markerLabel(marker && marker.kind, presentation);
    const actor = marker && marker.actor ? ` · ${marker.actor}` : "";
    return `${label}${actor}`;
  };

  const playerIdentityKey = (value) => {
    const trimmed = String(value || "").trim();
    const withoutTag = trimmed.split("#")[0].trim();
    return withoutTag.toLowerCase();
  };

  const playerInitials = (value) => {
    const letters = String(value || "").match(/[A-Za-z0-9]/g) || [];
    return (letters.slice(0, 2).join("").toUpperCase() || "?").slice(0, 2);
  };

  const eventRailDataDragonAliases = (presentation) => {
    const fields = presentation
      && presentation.metadata_panel
      && Array.isArray(presentation.metadata_panel.fields)
      ? presentation.metadata_panel.fields
      : [];
    const portrait = fields.find((field) =>
      field
        && field.asset_key_format === "data_dragon_champion"
        && field.asset_aliases
        && typeof field.asset_aliases === "object"
    );
    return portrait ? portrait.asset_aliases : {};
  };

  const participantForName = (summary, name) => {
    const key = playerIdentityKey(name);
    if (!key || !summary || !Array.isArray(summary.participants)) return null;
    return summary.participants.find((participant) =>
      playerIdentityKey(participant && participant.player_name) === key
        || playerIdentityKey(participant && participant.champion_name) === key
    ) || null;
  };

  const localPlayerKey = (summary) =>
    playerIdentityKey(summary && (summary.player_name || summary.champion_name));

  const localTeam = (summary) => String(summary && summary.team || "").trim();

  const participantSlot = (name, summary, presentation, options) => {
    const participant = participantForName(summary, name);
    if (!participant) return null;
    const displayName = String(name || participant.player_name || participant.champion_name || "").trim();
    const champion = String(participant.champion_name || "").trim();
    if (!displayName || !champion) return null;
    const aliases = eventRailDataDragonAliases(presentation);
    const assetKey = dataDragonChampionKey(champion, aliases);
    const asset = dataDragonAsset("champion", assetKey, options, /^[A-Za-z0-9]+$/);
    const slot = {
      name: displayName,
      champion,
      team: String(participant.team || "").trim(),
      assetKey,
    };
    if (asset) slot.asset = asset;
    slot.initials = playerInitials(displayName);
    slot.local = playerIdentityKey(displayName) === localPlayerKey(summary);
    return slot;
  };

  const eventRailActorIconSlot = (name, presentation) => {
    const rawName = String(name || "").trim();
    const actorIcons = presentation
      && presentation.event_rail
      && Array.isArray(presentation.event_rail.actor_icons)
      ? presentation.event_rail.actor_icons
      : [];
    if (!rawName || !actorIcons.length) return null;
    for (const config of actorIcons) {
      if (!config || typeof config !== "object") continue;
      const prefix = String(config.prefix || "").trim();
      const exact = String(config.match || "").trim();
      const matches = exact ? rawName === exact : prefix && rawName.startsWith(prefix);
      if (!matches) continue;
      const asset = String(config.asset || "").trim();
      const displayName = String(config.name || rawName).trim();
      if (!asset || !displayName) return null;
      return {
        name: displayName,
        asset,
        initials: playerInitials(displayName),
        local: false,
      };
    }
    return null;
  };

  const markerRailConfig = (kind, presentation) => {
    const configured = markerKindConfig(kind, presentation).rail;
    return configured && typeof configured === "object" ? configured : {};
  };

  const eventAllegiance = (summary, actorSlot, railConfig = {}) => {
    const configured = String(railConfig.allegiance || "").trim();
    if (configured === "friendly" || configured === "enemy" || configured === "neutral") {
      return configured;
    }
    const actorTeam = actorSlot && actorSlot.team ? actorSlot.team : "";
    const ownTeam = localTeam(summary);
    if (!actorTeam || !ownTeam) return "neutral";
    return actorTeam === ownTeam ? "friendly" : "enemy";
  };

  const matchEventDefaults = () => ({
    enabled: true,
    user_kills: true,
    user_deaths: true,
    user_assists: true,
    team_kills: true,
    team_deaths: true,
    enemy_kills: true,
    enemy_deaths: true,
    objectives: true,
    turrets: true,
  });

  const timelineMarkerDefaults = () => ({
    enabled: true,
    user_kills: true,
    user_deaths: true,
    user_assists: true,
    objectives: true,
    turrets: true,
  });

  const normalizeBooleanGroup = (settings, defaults) => {
    const source = settings && typeof settings === "object" ? settings : {};
    const out = {};
    for (const [key, fallback] of Object.entries(defaults)) {
      out[key] = Object.prototype.hasOwnProperty.call(source, key)
        ? source[key] !== false
        : fallback;
    }
    return out;
  };

  const normalizeGameReviewSettings = (settings) => {
    const source = settings && typeof settings === "object" ? settings : {};
    return {
      enabled: Object.prototype.hasOwnProperty.call(source, "enabled")
        ? source.enabled !== false
        : true,
      match_events: normalizeBooleanGroup(source.match_events, matchEventDefaults()),
      timeline_markers: normalizeBooleanGroup(source.timeline_markers, timelineMarkerDefaults()),
    };
  };

  const markerRelation = (summary, name) => {
    const key = playerIdentityKey(name);
    if (!key || !summary) return "";
    if (key === localPlayerKey(summary)) return "user";
    const participant = participantForName(summary, name);
    if (!participant) return "";
    const participantTeam = String(participant.team || "").trim();
    const ownTeam = localTeam(summary);
    if (!participantTeam || !ownTeam) return "";
    return participantTeam === ownTeam ? "team" : "enemy";
  };

  const markerHasLocalAssist = (marker, summary) => {
    const localKey = localPlayerKey(summary);
    if (!localKey) return marker && marker.involves_local_player === true;
    return Array.isArray(marker && marker.assisters)
      && marker.assisters.some((name) => playerIdentityKey(name) === localKey);
  };

  // Filter semantics key on the marker's category (profile-declared, with the
  // built-in kind table as fallback), not on game-specific kind names. A game
  // profile opts a kind into a surface by giving it a filtered category
  // (kill/death/assist/objective/structure); annotation echoes like sprees and
  // info events stay off both surfaces.
  const isObjectiveMarker = (marker, presentation) =>
    Boolean(marker) && markerCategory(marker.kind, presentation) === "objective";

  const isStructureMarker = (marker, presentation) =>
    Boolean(marker) && markerCategory(marker.kind, presentation) === "structure";

  const isKillMarker = (marker, presentation) =>
    Boolean(marker) && markerCategory(marker.kind, presentation) === "kill";

  const isLocalKill = (marker, summary, presentation) =>
    isKillMarker(marker, presentation)
      && (markerRelation(summary, marker.actor) === "user"
        || (marker.involves_local_player === true && markerRelation(summary, marker.victim) !== "user"));

  const isLocalDeath = (marker, summary, presentation) =>
    Boolean(marker)
      && (markerCategory(marker.kind, presentation) === "death"
        || (isKillMarker(marker, presentation) && markerRelation(summary, marker.victim) === "user"))
      && (markerRelation(summary, marker.victim) === "user" || marker.involves_local_player === true);

  const isLocalAssist = (marker, summary, presentation) =>
    Boolean(marker) && markerCategory(marker.kind, presentation) === "assist"
      && (markerRelation(summary, marker.actor) === "user"
        || markerHasLocalAssist(marker, summary)
        || marker.involves_local_player === true);

  const matchEventEnabled = (marker, summary, settings, presentation) => {
    if (isLocalKill(marker, summary, presentation)) return settings.user_kills;
    if (isLocalDeath(marker, summary, presentation)) return settings.user_deaths;
    if (isLocalAssist(marker, summary, presentation)) return settings.user_assists;
    if (isKillMarker(marker, presentation)) {
      const actorRelation = markerRelation(summary, marker.actor);
      const victimRelation = markerRelation(summary, marker.victim);
      return (actorRelation === "team" && settings.team_kills)
        || (victimRelation === "team" && settings.team_deaths)
        || (actorRelation === "enemy" && settings.enemy_kills)
        || (victimRelation === "enemy" && settings.enemy_deaths);
    }
    if (isObjectiveMarker(marker, presentation)) return settings.objectives;
    // The saved settings key stays "turrets" (its League-era name) so existing
    // user settings keep working; it governs the whole structure category.
    if (isStructureMarker(marker, presentation)) return settings.turrets;
    return false;
  };

  const timelineMarkerEnabled = (marker, summary, settings, presentation) => {
    if (isLocalKill(marker, summary, presentation)) return settings.user_kills;
    if (isLocalDeath(marker, summary, presentation)) return settings.user_deaths;
    if (isLocalAssist(marker, summary, presentation)) return settings.user_assists;
    if (isObjectiveMarker(marker, presentation)) return settings.objectives;
    if (isStructureMarker(marker, presentation)) return settings.turrets;
    return false;
  };

  const reviewMatchEventMarkers = (markers, summary = null, settings = null, presentation = null) => {
    const normalized = normalizeGameReviewSettings(settings);
    if (!normalized.enabled || !normalized.match_events.enabled) return [];
    return (markers || []).filter((marker) =>
      matchEventEnabled(marker, summary, normalized.match_events, presentation)
    );
  };

  const reviewTimelineMarkers = (markers, summary = null, settings = null, presentation = null) => {
    const normalized = normalizeGameReviewSettings(settings);
    if (!normalized.enabled || !normalized.timeline_markers.enabled) return [];
    return (markers || []).filter((marker) =>
      timelineMarkerEnabled(marker, summary, normalized.timeline_markers, presentation)
    );
  };

  // Bookmarks are user-placed, so they deliberately skip the game-review
  // filters above: they must show on clips with no detected game and must not
  // disappear because someone turned a game's marker categories off. Shaping
  // them like markers lets the pins, overview ticks, marker count, prev/next
  // navigation and drag snapping treat them as one list.
  const isBookmarkMarker = (marker) =>
    Boolean(marker) && marker.kind === BOOKMARK_KIND;

  const bookmarkMarkers = (bookmarks) => {
    const seen = new Set();
    const out = [];
    for (const bookmark of Array.isArray(bookmarks) ? bookmarks : []) {
      // Guard the object first: Number(null) is 0, which would plant a phantom
      // bookmark at the very start of the clip.
      if (!bookmark || typeof bookmark !== "object") continue;
      const t_s = Number(bookmark.t_s);
      if (!Number.isFinite(t_s) || t_s < 0 || seen.has(t_s)) continue;
      seen.add(t_s);
      out.push({ t_s, kind: BOOKMARK_KIND });
    }
    return out.sort((a, b) => a.t_s - b.t_s);
  };

  const withBookmarks = (markers, bookmarks) =>
    [...(markers || []), ...bookmarkMarkers(bookmarks)].sort((a, b) => a.t_s - b.t_s);

  const gameEventRailItem = (marker, summary = null, presentation = null, options = {}) => {
    const kind = marker && marker.kind ? marker.kind : "Other";
    const category = markerCategory(kind, presentation);
    const label = markerLabel(kind, presentation);
    const railConfig = markerRailConfig(kind, presentation);
    const item = {
      layout: "text",
      kind,
      category,
      allegiance: eventAllegiance(summary, null, railConfig),
      label,
      text: markerEventText(marker, presentation),
    };
    const icon = eventRailIcon(kind, presentation);
    if (icon) item.icon = icon;

    const railLayout = String(railConfig.layout || "").trim();
    if (railLayout === "duel") {
      const actor = participantSlot(marker && marker.actor, summary, presentation, options);
      const victim = participantSlot(marker && marker.victim, summary, presentation, options);
      if (actor && victim) {
        item.layout = "duel";
        item.allegiance = eventAllegiance(summary, actor, railConfig);
        item.actor = actor;
        item.victim = victim;
      }
    } else if (railLayout === "actor_event" && item.icon) {
      const actor = participantSlot(marker && marker.actor, summary, presentation, options)
        || eventRailActorIconSlot(marker && marker.actor, presentation);
      item.layout = "actor_event";
      if (actor) {
        item.allegiance = eventAllegiance(summary, actor, railConfig);
        item.actor = actor;
      }
    }

    return item;
  };

  const RULER_STEPS_S = [1, 2, 5, 10, 15, 30, 60, 120, 300, 600, 900, 1800, 3600];

  // Labeled marks at "nice" intervals across an arbitrary window. Picks the step
  // from the visible span (not the clip length) so a zoomed-in ruler stays
  // readable, and emits only the marks that fall inside the window.
  const rulerMarksRange = (viewStart, viewSpan, maxMarks) => {
    if (!Number.isFinite(viewSpan) || viewSpan <= 0) return [];
    const step =
      RULER_STEPS_S.find((s) => viewSpan / s <= maxMarks - 1) ??
      RULER_STEPS_S[RULER_STEPS_S.length - 1];
    const viewEnd = viewStart + viewSpan;
    const first = Math.ceil(viewStart / step - 1e-9) * step;
    const marks = [];
    for (let t = first; t <= viewEnd + 1e-6; t += step) {
      marks.push({ t, label: fmtDur(t) });
    }
    return marks;
  };

  // Whole-clip ruler (window starting at 0) — the zoomed-out default.
  const rulerMarks = (duration, maxMarks) => {
    if (!Number.isFinite(duration) || duration <= 0) return [];
    return rulerMarksRange(0, duration, maxMarks);
  };

  // Bucket clips by session folder; legacy root clips fall under "Earlier".
  // Groups and the clips inside them sort newest-first.
  const sessionGroups = (clips) => {
    const order = [];
    const byLabel = {};
    for (const c of clips) {
      const label = c.session ? c.session : "Earlier";
      if (!byLabel[label]) {
        byLabel[label] = [];
        order.push(label);
      }
      byLabel[label].push(c);
    }
    const groups = order.map((label) => ({
      label,
      clips: byLabel[label].slice().sort((a, b) => b.modified_unix - a.modified_unix),
    }));
    groups.sort((a, b) => b.clips[0].modified_unix - a.clips[0].modified_unix);
    return groups;
  };

  const formatClipTitle = PresentationCore.formatClipTitle;

  // Prefer the backend's stable clip kind. Older clips can still be classified
  // from their on-disk names.
  const clipKind = (clip) => {
    const explicit = clip && typeof clip === "object" ? String(clip.kind || "").trim() : "";
    if (explicit === "replay" || explicit === "session" || explicit === "trim") return explicit;
    const n = typeof clip === "string" ? clip : String(clip && clip.name || "");
    if (/_trim_/.test(n)) return "trim";
    if (/^session_/.test(n)) return "session";
    return "replay";
  };

  const keyIntent = (code, shiftKey) => {
    switch (code) {
      case "Space":
      case "KeyK":
        return { kind: "toggle-play" };
      // Arrows are the coarse seek keys (Shift for a shorter nudge); J/L are
      // the frame-aligned step keys.
      case "ArrowLeft":
        return { kind: "seek-by", seconds: shiftKey ? -1 : -5 };
      case "ArrowRight":
        return { kind: "seek-by", seconds: shiftKey ? 1 : 5 };
      case "KeyJ":
        return { kind: "step-frame", dir: -1 };
      case "KeyL":
        return { kind: "step-frame", dir: 1 };
      case "Comma":
        return { kind: "seek-by", seconds: -0.1 };
      case "Period":
        return { kind: "seek-by", seconds: 0.1 };
      case "KeyI":
        return { kind: "set-in" };
      case "KeyO":
        return { kind: "set-out" };
      case "KeyM":
        return { kind: shiftKey ? "prev-marker" : "next-marker" };
      // Zoom: +/- step at the playhead, \ fits the clip, Shift+\ fits the trim.
      case "Equal":
      case "NumpadAdd":
        return { kind: "zoom", factor: 0.5 };
      case "Minus":
      case "NumpadSubtract":
        return { kind: "zoom", factor: 2 };
      case "Backslash":
        return shiftKey ? { kind: "zoom-fit" } : { kind: "zoom-selection" };
      case "KeyZ":
        return shiftKey ? { kind: "zoom-fit" } : null;
      case "Home":
        return { kind: "seek-to", seconds: 0 };
      case "End":
        return { kind: "seek-to-end" };
      case "ArrowUp":
        return { kind: "prev-edit" };
      case "ArrowDown":
        return { kind: "next-edit" };
      case "KeyS":
        return { kind: "toggle-snap" };
      case "KeyF":
        return { kind: "toggle-fullscreen" };
      case "Escape":
        return { kind: "close" };
      default:
        return null;
    }
  };

  const functionKeyNumber = (ev) => {
    const raw = String(ev.code || ev.key || "").toUpperCase();
    const match = raw.match(/^F([1-9]|1[0-9]|2[0-4])$/);
    return match ? Number(match[1]) : null;
  };

  const keyboardHotkeyName = (ev) => {
    const code = String(ev.code || "");
    const keyMatch = code.match(/^Key([A-Z])$/);
    if (keyMatch) return keyMatch[1];
    const digitMatch = code.match(/^Digit([0-9])$/);
    if (digitMatch) return digitMatch[1];
    switch (code) {
      case "ArrowUp":
      case "ArrowDown":
      case "ArrowLeft":
      case "ArrowRight":
      case "Space":
      case "Enter":
      case "Tab":
      case "Backspace":
      case "Delete":
      case "Insert":
      case "Home":
      case "End":
      case "PageUp":
      case "PageDown":
      case "Minus":
      case "Equal":
      case "BracketLeft":
      case "BracketRight":
      case "Backslash":
      case "Semicolon":
      case "Quote":
      case "Comma":
      case "Period":
      case "Slash":
      case "Backquote":
        return code;
      default:
        return null;
    }
  };

  const isReservedHotkey = (key, ctrl, alt, shift) => {
    if (key === "Tab" && alt) return true;
    if (key === "F4" && alt) return true;
    if (key === "Delete" && ctrl && alt) return true;
    return false;
  };

  const hotkeyFromKeyEvent = (ev) => {
    if (ev.code === "Escape" || ev.key === "Escape") return { kind: "cancel" };
    if (
      ev.code === "ControlLeft" ||
      ev.code === "ControlRight" ||
      ev.code === "AltLeft" ||
      ev.code === "AltRight" ||
      ev.code === "ShiftLeft" ||
      ev.code === "ShiftRight"
    ) {
      return { kind: "pending", message: "Now press an F-key, mouse button, or keyboard key." };
    }

    const key = functionKeyNumber(ev);
    let hotkeyKey = null;
    let needsModifier = false;
    if (String(ev.code) === "PrintScreen") {
      hotkeyKey = "PrintScreen";
    } else if (key) {
      if (key === 12) {
        return { kind: "invalid", message: "F12 is reserved by Windows for debuggers." };
      }
      hotkeyKey = `F${key}`;
    } else {
      hotkeyKey = keyboardHotkeyName(ev);
      needsModifier = true;
    }
    if (!hotkeyKey) {
      return { kind: "invalid", message: "Use an F-key, mouse button, or Ctrl/Alt/Shift plus a keyboard key." };
    }
    if (needsModifier && !ev.ctrlKey && !ev.altKey && !ev.shiftKey) {
      return { kind: "invalid", message: "Use Ctrl, Alt, or Shift with this key." };
    }
    if (isReservedHotkey(hotkeyKey, ev.ctrlKey, ev.altKey, ev.shiftKey)) {
      return { kind: "invalid", message: "That shortcut is reserved by Windows." };
    }

    const parts = [];
    if (ev.ctrlKey) parts.push("Ctrl");
    if (ev.altKey) parts.push("Alt");
    if (ev.shiftKey) parts.push("Shift");
    parts.push(hotkeyKey);
    return { kind: "captured", value: parts.join("+") };
  };

  const mouseButtonHotkeyName = (button) => {
    switch (Number(button)) {
      case 1:
        return "Middle";
      case 3:
        return "Mouse4";
      case 4:
        return "Mouse5";
      default:
        return null;
    }
  };

  const hotkeyFromMouseEvent = (ev) => {
    const key = mouseButtonHotkeyName(ev.button);
    if (!key) {
      return { kind: "invalid", message: "Use middle, Mouse4, or Mouse5 as a mouse shortcut." };
    }

    const parts = [];
    if (ev.ctrlKey) parts.push("Ctrl");
    if (ev.altKey) parts.push("Alt");
    if (ev.shiftKey) parts.push("Shift");
    parts.push(key);
    return { kind: "captured", value: parts.join("+") };
  };

  const displayBounds = (displays) => {
    if (!displays.length) return { x: 0, y: 0, width: 0, height: 0 };
    const left = Math.min(...displays.map((d) => d.x));
    const top = Math.min(...displays.map((d) => d.y));
    const right = Math.max(...displays.map((d) => d.x + d.width));
    const bottom = Math.max(...displays.map((d) => d.y + d.height));
    return { x: left, y: top, width: right - left, height: bottom - top };
  };

  const displayMapLayout = (displays, viewportW, viewportH, padding = 10) => {
    const bounds = displayBounds(displays);
    const innerW = Math.max(1, viewportW - padding * 2);
    const innerH = Math.max(1, viewportH - padding * 2);
    const scale = bounds.width && bounds.height
      ? Math.min(innerW / bounds.width, innerH / bounds.height)
      : 1;
    return {
      bounds,
      scale,
      width: bounds.width * scale + padding * 2,
      height: bounds.height * scale + padding * 2,
      displays: displays.map((d) => ({
        id: d.id,
        left: padding + (d.x - bounds.x) * scale,
        top: padding + (d.y - bounds.y) * scale,
        width: d.width * scale,
        height: d.height * scale,
      })),
    };
  };

  const displayMapHeight = (displays, viewportW, padding = 10, minH = 180, maxH = 420) => {
    const bounds = displayBounds(displays);
    if (!bounds.width || !bounds.height) return minH;
    const innerW = Math.max(1, viewportW - padding * 2);
    const height = bounds.height * (innerW / bounds.width) + padding * 2;
    return Math.max(minH, Math.min(maxH, height));
  };

  const regionForDisplay = (display) => ({
    display_id: display.id,
    x: display.x,
    y: display.y,
    width: display.width,
    height: display.height,
  });

  const evenSize = (value, max) => {
    const clamped = Math.max(2, Math.min(Math.round(value || 2), max));
    return clamped % 2 === 0 ? clamped : Math.max(2, clamped - 1);
  };

  const clampRegionToDisplay = (region, display) => {
    const width = evenSize(region.width, display.width);
    const height = evenSize(region.height, display.height);
    const minX = display.x;
    const minY = display.y;
    const maxX = display.x + display.width - width;
    const maxY = display.y + display.height - height;
    const x = Math.max(minX, Math.min(maxX, Math.round(region.x || 0)));
    const y = Math.max(minY, Math.min(maxY, Math.round(region.y || 0)));
    return { display_id: display.id, x, y, width, height };
  };

  const alignRegion = (region, display, align) => {
    const next = clampRegionToDisplay(region, display);
    switch (align) {
      case "left":
        next.x = display.x;
        break;
      case "right":
        next.x = display.x + display.width - next.width;
        break;
      case "top":
        next.y = display.y;
        break;
      case "bottom":
        next.y = display.y + display.height - next.height;
        break;
      case "center":
        next.x = Math.round(display.x + (display.width - next.width) / 2);
        next.y = Math.round(display.y + (display.height - next.height) / 2);
        break;
    }
    return next;
  };

  // --- Audio preview queue ---
  // Serializes async preview renders: at most one in flight (active), with the
  // latest pending request coalesced into desired. Callers never mutate requests.

  const emptyAudioPreviewQueue = () => ({ active: null, desired: null, revision: 0 });

  const queueAudioPreviewRequest = (state, request) => {
    const revision = Number(state && state.revision || 0) + 1;
    const next = { ...request, revision };
    const active = state && state.active ? state.active : null;
    return {
      state: { active: active || next, desired: next, revision },
      start: active ? null : next,
      apply: null,
    };
  };

  const cancelAudioPreviewRequest = (state) => ({
    active: state && state.active ? state.active : null,
    desired: null,
    revision: Number(state && state.revision || 0) + 1,
  });

  const finishAudioPreviewRequest = (state, revision, succeeded) => {
    if (!state || !state.active || state.active.revision !== revision) {
      return { state, start: null, apply: null };
    }
    const desired = state.desired;
    const apply = succeeded && desired && desired.revision === revision ? state.active : null;
    const start = !apply && desired && desired.revision !== revision ? desired : null;
    return {
      state: { active: start, desired: start, revision: state.revision },
      start,
      apply,
    };
  };

  const audioSidecarSyncDecision = (videoState, sidecarState, options = {}) => {
    const videoTime = Number(videoState && videoState.currentTime);
    const sidecarTime = Number(sidecarState && sidecarState.currentTime);
    const sidecarDuration = Number(sidecarState && sidecarState.duration);
    const validVideoTime = Number.isFinite(videoTime) && videoTime >= 0;
    const validSidecarDuration = Number.isFinite(sidecarDuration) && sidecarDuration >= 0;
    const rate = Number(videoState && videoState.playbackRate);
    const playbackRate = Number.isFinite(rate) && rate > 0 ? rate : 1;
    const sidecarExhausted = Boolean(sidecarState && sidecarState.ended)
      && validVideoTime
      && validSidecarDuration
      && videoTime >= sidecarDuration - Number.EPSILON;
    const videoShouldPlay = !(videoState && videoState.paused) && !(videoState && videoState.ended);
    const shouldPlay = videoShouldPlay && !sidecarExhausted;
    const validSidecarTime = Number.isFinite(sidecarTime) && sidecarTime >= 0;
    const drift = validVideoTime && validSidecarTime ? videoTime - sidecarTime : 0;
    const driftMagnitude = Math.abs(drift);
    const forceSeek = options.forceSeek === true
      || !validSidecarTime
      || (validVideoTime && driftMagnitude > AUDIO_SIDECAR_HARD_SEEK_TOLERANCE_S)
      || (validVideoTime && !videoShouldPlay && driftMagnitude > AUDIO_SIDECAR_DRIFT_DEADBAND_S);
    return {
      seekTime: validVideoTime && forceSeek ? videoTime : null,
      playbackRate,
      shouldPlay,
    };
  };

  const reviewAudioOutputDecision = (mode, muted, volume) => {
    const value = Number(volume);
    const normalizedVolume = Number.isFinite(value) ? Math.max(0, Math.min(1, value)) : 1;
    const silenceAll = Boolean(muted) || normalizedVolume === 0;
    return {
      videoMuted: silenceAll || mode !== "direct",
      sidecarMuted: silenceAll || mode !== "sidecars",
      volume: normalizedVolume,
    };
  };

  // --- Encoder codec playback support (Settings) ---
  // Codecs the in-app review player may need an OS extension to decode.
  // H.264 always plays in WebView2; HEVC/AV1 are probed at runtime via
  // canPlayType. The mime strings name concrete profiles the muxer emits.
  const VIDEO_DECODE_PROBES = [
    { codec: "hevc", mime: 'video/mp4; codecs="hvc1.1.6.L93.B0"' },
    { codec: "av1", mime: 'video/mp4; codecs="av01.0.04M.08"' },
  ];

  const videoDecodeProbes = () => VIDEO_DECODE_PROBES.map((p) => ({ ...p }));

  // One-line caveat shown under the encoder dropdown when the selected
  // encoder's codec is not in the set the player can decode; null otherwise.
  const encoderCodecCaveat = (codecKey, decodableCodecs) => {
    if (!codecKey || codecKey === "h264") return null;
    if ((decodableCodecs || []).includes(codecKey)) return null;
    const name =
      codecKey === "hevc" ? "HEVC" : codecKey === "av1" ? "AV1" : String(codecKey).toUpperCase();
    return `${name} may not play in the in-app review player on this PC. The clip still records and opens in other players.`;
  };

  return {
    MIN_TRIM_GAP_S,
    MARKER_EPSILON_S,
    OVERLAY_HIDE_MS,
    MIN_VIEW_SPAN_S,
    DEFAULT_FOLLOW_MODE,
    SNAP_THRESHOLD_PX,
    DEFAULT_FINE_STEP_S,
    QUICK_TRIM_WINDOW_S,
    overlayVisible,
    videoDecodeProbes,
    encoderCodecCaveat,
    fmtBytes,
    fmtMegabytes,
    fmtLibraryStorageUsage,
    sameClipPath,
    clipFileLabel,
    cloudLibraryEntries,
    fmtDur,
    fmtTenths,
    fmtAgo,
    settingDurationLabel,
    recordingQualityPreset,
    recordingQualitySummary,
    qualityIndexForBitrate,
    qualityIndexForId,
    smoothnessPreset,
    smoothnessIndexForFps,
    outputResolutionOption,
    captureSourceLabel,
    clampTime,
    createLogicalSeekState,
    requestLogicalSeek,
    beginSourceAssignment,
    metadataSeekDecision,
    seekedDecision,
    sidecarRealignmentForced,
    logicalPlaybackTime,
    relativeSeekTarget,
    percentFor,
    timelineTime,
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
    resolveTrim,
    quickTrimRange,
    trimPlaybackStopTime,
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
    playResultSummary,
    playActiveIndex,
    gameEventActiveIndex,
    defaultAudioTrackIds,
    directPlaybackAudioTrackIds,
    selectedReviewAudioTrackIds,
    reviewSelectionNeedsPreview,
    reviewAudioTrackRowState,
    applyReviewAudioTrackToggle,
    reviewAudioTrackSelectedRowCount,
    selectedAudioTrackIds,
    selectionNeedsPreview,
    audioTrackRowState,
    applyAudioTrackToggle,
    audioTrackSelectedRowCount,
    markerStyle,
    markerDigest,
    ownObjectValue,
    markerKindConfig,
    safeMarkerImage,
    normalizeGameReviewSettings,
    reviewMatchEventMarkers,
    reviewTimelineMarkers,
    isBookmarkMarker,
    bookmarkMarkers,
    withBookmarks,
    playerSummaryLabel,
    playerSummaryFields,
    galleryCardPreview,
    gameEventRailItem,
    rulerMarks,
    rulerMarksRange,
    sessionGroups,
    formatClipTitle,
    clipKind,
    keyIntent,
    hotkeyFromKeyEvent,
    hotkeyFromMouseEvent,
    displayBounds,
    displayMapLayout,
    displayMapHeight,
    regionForDisplay,
    clampRegionToDisplay,
    alignRegion,
    emptyAudioPreviewQueue,
    queueAudioPreviewRequest,
    cancelAudioPreviewRequest,
    finishAudioPreviewRequest,
    audioSidecarSyncDecision,
    reviewAudioOutputDecision,
  };
})();

globalThis.PlayerCore = PlayerCore;
