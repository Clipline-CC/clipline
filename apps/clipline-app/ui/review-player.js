// Review workspace: playback, timeline, trim, export.
/* ---- review player ---- */

var renameFileDialogClip = null;
var renameFilePending = false;

// Sync the review-header upload button to the current clip's cloud state:
// hidden when signed out unless this clip is already uploaded; a link icon
// once uploaded.
function syncUploadClipButton() {
  const btn = $("upload-clip");
  if (!btn) return;
  const group = activeGroup();
  if (group) {
    const compilation = groupCompilationClip(group);
    const record = compilation ? clipCloudRecord(compilation) : null;
    const busy = record && ["queued", "uploading", "processing", "retrying"].includes(record.upload_status);
    const uploaded = cloudRecordUploaded(record);
    const shareable = !!cloudShareUrl(record);
    btn.hidden = !cloudUploadControlVisible(uploaded);
    btn.disabled = busy || (uploaded ? !record.remote_clip_id : !cloudConnected());
    btn.title = shareable
      ? "Copy group cloud link"
      : uploaded ? "Open group cloud page — no public share link" : "Upload group compilation to Clipline Cloud";
    btn.classList.toggle("uploaded", uploaded);
    btn.classList.toggle("busy", Boolean(busy));
    btn.innerHTML = shareable
      ? '<svg viewBox="0 0 24 24"><path d="M10.6 13.4a1 1 0 0 1 0-1.4l3.5-3.5a3 3 0 1 1 4.2 4.2l-1.5 1.5-1.4-1.4 1.5-1.5a1 1 0 1 0-1.4-1.4L12 13.4a1 1 0 0 1-1.4 0zm2.8-2.8a1 1 0 0 1 0 1.4l-3.5 3.5a3 3 0 1 1-4.2-4.2l1.5-1.5 1.4 1.4-1.5 1.5a1 1 0 1 0 1.4 1.4L12 10.6a1 1 0 0 1 1.4 0z"/></svg>'
      : uploaded
        ? '<svg viewBox="0 0 24 24"><path d="M7.2 18h10.2a4.1 4.1 0 0 0 .4-8.2A6.2 6.2 0 0 0 5.9 8.1 5 5 0 0 0 7.2 18zm.2-2a3 3 0 0 1-.5-5.9l.8-.1.3-.8A4.2 4.2 0 0 1 16 10.4l.2 1.2 1.2.1A2.1 2.1 0 0 1 17.4 16H7.4z"/></svg>'
        : '<svg viewBox="0 0 24 24"><path d="M12 3 6.5 8.5 8 10l3-3v10h2V7l3 3 1.5-1.5L12 3zM5 19h14v2H5v-2z"/></svg>';
    return;
  }
  const clip = currentClip;
  if (isCloudOnlyReviewClip(clip)) {
    const shareable = !!(
      clip.cloud_remote_url
      && String(clip.cloud_visibility || "") !== "private"
    );
    const uploaded = !!clip.cloud_remote_clip_id;
    btn.hidden = !cloudUploadControlVisible(uploaded);
    btn.title = shareable
      ? "Copy cloud link"
      : uploaded ? "Open cloud page — no public share link" : "Cloud page unavailable";
    btn.classList.toggle("uploaded", uploaded);
    btn.classList.remove("busy");
    btn.disabled = !uploaded;
    btn.innerHTML = shareable
      ? '<svg viewBox="0 0 24 24"><path d="M10.6 13.4a1 1 0 0 1 0-1.4l3.5-3.5a3 3 0 1 1 4.2 4.2l-1.5 1.5-1.4-1.4 1.5-1.5a1 1 0 1 0-1.4-1.4L12 13.4a1 1 0 0 1-1.4 0zm2.8-2.8a1 1 0 0 1 0 1.4l-3.5 3.5a3 3 0 1 1-4.2-4.2l1.5-1.5 1.4 1.4-1.5 1.5a1 1 0 1 0 1.4 1.4L12 10.6a1 1 0 0 1 1.4 0z"/></svg>'
      : '<svg viewBox="0 0 24 24"><path d="M7.2 18h10.2a4.1 4.1 0 0 0 .4-8.2A6.2 6.2 0 0 0 5.9 8.1 5 5 0 0 0 7.2 18zm.2-2a3 3 0 0 1-.5-5.9l.8-.1.3-.8A4.2 4.2 0 0 1 16 10.4l.2 1.2 1.2.1A2.1 2.1 0 0 1 17.4 16H7.4z"/></svg>';
    return;
  }
  const record = clip ? clipCloudRecord(clip) : null;
  const busy = record && ["queued", "uploading", "processing", "retrying"].includes(record.upload_status);
  const uploaded = cloudRecordUploaded(record);
  const shareable = !!cloudShareUrl(record);
  btn.hidden = !cloudUploadControlVisible(uploaded);
  btn.title = shareable
    ? "Copy cloud link"
    : uploaded ? "Open cloud page — no public share link" : "Upload to Clipline Cloud";
  btn.classList.toggle("uploaded", uploaded);
  btn.classList.toggle("busy", !!busy);
  btn.disabled = !clip || busy || (!uploaded && !cloudConnected());
  btn.innerHTML = shareable
    ? '<svg viewBox="0 0 24 24"><path d="M10.6 13.4a1 1 0 0 1 0-1.4l3.5-3.5a3 3 0 1 1 4.2 4.2l-1.5 1.5-1.4-1.4 1.5-1.5a1 1 0 1 0-1.4-1.4L12 13.4a1 1 0 0 1-1.4 0zm2.8-2.8a1 1 0 0 1 0 1.4l-3.5 3.5a3 3 0 1 1-4.2-4.2l1.5-1.5 1.4 1.4-1.5 1.5a1 1 0 1 0 1.4 1.4L12 10.6a1 1 0 0 1 1.4 0z"/></svg>'
    : uploaded
      ? '<svg viewBox="0 0 24 24"><path d="M7.2 18h10.2a4.1 4.1 0 0 0 .4-8.2A6.2 6.2 0 0 0 5.9 8.1 5 5 0 0 0 7.2 18zm.2-2a3 3 0 0 1-.5-5.9l.8-.1.3-.8A4.2 4.2 0 0 1 16 10.4l.2 1.2 1.2.1A2.1 2.1 0 0 1 17.4 16H7.4z"/></svg>'
      : '<svg viewBox="0 0 24 24"><path d="M12 3 6.5 8.5 8 10l3-3v10h2V7l3 3 1.5-1.5L12 3zM5 19h14v2H5v-2z"/></svg>';
}

function syncReviewLocalActions() {
  const cloudOnly = isCloudOnlyReviewClip();
  const group = Boolean(activeGroup());
  $("rename-clip").hidden = cloudOnly || group;
  for (const id of ["favorite-clip", "open-folder", "copy-clip", "delete-clip"]) {
    const el = $(id);
    if (el) el.hidden = cloudOnly;
  }
  syncFavoriteButton();
  if (cloudOnly || group) setClipTitleEditing(false);
  syncGroupReviewChrome();
}

function syncFavoriteButton() {
  const el = $("favorite-clip");
  if (!el) return;
  const favorite = !!(currentClip && currentClip.favorite);
  el.classList.toggle("favorite-on", favorite);
  el.setAttribute("aria-pressed", String(favorite));
  el.title = favorite ? "Remove from favorites" : "Add to favorites";
  el.innerHTML = favorite
    ? '<svg viewBox="0 0 24 24"><path d="M12 2.6l3 5.9 6.5 1-4.8 4.5 1.1 6.5L12 17.3 6.2 20.6l1.1-6.5L2.5 9.6l6.5-1z"/></svg>'
    : '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M12 2.6l3 5.9 6.5 1-4.8 4.5 1.1 6.5L12 17.3 6.2 20.6l1.1-6.5L2.5 9.6l6.5-1z"/></svg>';
}

function setClipRenameControlsDisabled(disabled) {
  $("rename-input").disabled = disabled;
  $("rename-save").disabled = disabled;
  $("rename-cancel").disabled = disabled;
}

function setClipTitleEditing(editing) {
  $("clip-title-display").hidden = editing;
  $("clip-title-edit").hidden = !editing;
  if (!editing) {
    $("rename-input").value = "";
    setClipRenameControlsDisabled(false);
  }
}

function beginClipRename(clip = currentClip) {
  if (!clip) return;
  if (isCloudOnlyReviewClip(clip)) return;
  if (!currentClip || currentClip.path !== clip.path) openClip(clip);
  setClipTitleEditing(true);
  const activeClip = currentClip && currentClip.path === clip.path ? currentClip : clip;
  $("rename-input").value = clipDisplayTitle(activeClip) || activeClip.name || "";
  $("rename-input").focus();
  $("rename-input").select();
}

function cancelClipRename() {
  if (renamePending) return;
  setClipTitleEditing(false);
}

function renamedClipFromResult(oldClip, result) {
  const hasTitle = result && Object.prototype.hasOwnProperty.call(result, "title");
  return {
    ...oldClip,
    path: result && result.path || oldClip.path,
    name: result && result.name || oldClip.name,
    title: hasTitle ? result.title : oldClip.title,
    kind: result && result.kind || oldClip.kind,
  };
}

function applyRenamedClip(oldClip, result) {
  const renamed = renamedClipFromResult(oldClip, result);
  replaceClipInCache(oldClip.path, renamed);
  replaceCloudRecordPath(result && result.old_path || oldClip.path, renamed.path);
  if (currentClip && currentClip.path === oldClip.path) {
    currentClip = renamed;
    $("pname").textContent = clipDisplayTitle(renamed) || renamed.name;
    const shownDuration = clipDuration();
    $("pmeta").textContent =
      `${shownDuration > 0 ? `${fmtDur(shownDuration)} · ` : ""}${fmtMegabytes(renamed.size_mb)} · ${PlayerCore.clipFileLabel(renamed)}`;
  }
  return renamed;
}

var reviewSourceGeneration = 0;
var reviewSourceErrorHandler = null;
var reviewSeekState = PlayerCore.createLogicalSeekState();
var audioPreviewQueue = PlayerCore.emptyAudioPreviewQueue();

function reviewPlayheadTime() {
  return PlayerCore.logicalPlaybackTime(reviewSeekState, video.currentTime, clipDuration());
}

function reviewAudioTransportState() {
  return {
    currentTime: reviewPlayheadTime(),
    playbackRate: video.playbackRate,
    paused: video.paused,
    ended: video.ended,
  };
}

function disposeReviewAudioSidecarSet(sidecars) {
  for (const sidecar of sidecars || []) {
    const audio = sidecar.element;
    audio.pause();
    audio.removeAttribute("src");
    audio.load();
  }
}

function clearReviewAudioDriftTimer() {
  if (!reviewAudioDriftTimer) return;
  window.clearInterval(reviewAudioDriftTimer);
  reviewAudioDriftTimer = 0;
}

function applyReviewAudioOutput() {
  const decision = PlayerCore.reviewAudioOutputDecision(
    reviewAudioMode,
    reviewAudioMuted,
    reviewAudioVolume,
  );
  video.volume = decision.volume;
  video.muted = decision.videoMuted;
  for (const { element: audio } of activeReviewAudioSidecars) {
    audio.volume = decision.volume;
    audio.muted = decision.sidecarMuted;
  }
}

function clearReviewAudioSidecars(mode = "direct") {
  const stale = activeReviewAudioSidecars;
  reviewAudioSidecarGeneration += 1;
  clearReviewAudioDriftTimer();
  activeReviewAudioSidecars = [];
  disposeReviewAudioSidecarSet(stale);
  reviewAudioMode = mode;
  applyReviewAudioOutput();
}

async function syncReviewAudioSidecarSet(sidecars, options = {}) {
  const videoState = options.videoState || reviewAudioTransportState();
  const playPromises = [];
  for (const { element: audio } of sidecars || []) {
    const decision = PlayerCore.audioSidecarSyncDecision(
      videoState,
      {
        currentTime: audio.currentTime,
        duration: audio.duration,
        ended: audio.ended,
      },
      { forceSeek: options.forceSeek === true },
    );
    if (decision.seekTime != null) audio.currentTime = decision.seekTime;
    audio.playbackRate = decision.playbackRate;
    if (decision.shouldPlay && options.allowPlayback !== false) {
      if (audio.paused) playPromises.push(Promise.resolve(audio.play()));
    } else if (!audio.paused) {
      audio.pause();
    }
  }
  await Promise.all(playPromises);
}

function handleReviewAudioSidecarFailure(generation, error) {
  if (generation !== reviewAudioSidecarGeneration) return;
  clearReviewAudioSidecars("direct");
  if (currentClip) {
    currentReviewAudioTrackIds = PlayerCore.directPlaybackAudioTrackIds(clipAudioTracks(currentClip));
    currentReviewAudioKey = audioSelectionKey(currentClip, currentReviewAudioTrackIds);
    restoreAudibleAudioSelection(`audio playback failed: ${String(error)}`);
  }
}

function syncReviewAudioSidecars(options = {}) {
  if (reviewAudioMode !== "sidecars" || activeReviewAudioSidecars.length === 0) return;
  const generation = reviewAudioSidecarGeneration;
  void syncReviewAudioSidecarSet(activeReviewAudioSidecars, options)
    .catch((error) => handleReviewAudioSidecarFailure(generation, error));
}

function refreshReviewAudioDriftTimer() {
  const shouldRun = reviewAudioMode === "sidecars"
    && activeReviewAudioSidecars.length > 0
    && !video.paused
    && !video.ended;
  if (!shouldRun) {
    clearReviewAudioDriftTimer();
    return;
  }
  if (!reviewAudioDriftTimer) {
    reviewAudioDriftTimer = window.setInterval(() => syncReviewAudioSidecars(), 500);
  }
}

async function prepareReviewAudioSidecars(sidecars, generation) {
  const prepared = (sidecars || []).map((sidecar) => {
    const audio = new Audio();
    audio.preload = "auto";
    audio.muted = true;
    audio.volume = reviewAudioVolume;
    audio.src = convertFileSrc(sidecar.path);
    return {
      audioTrackId: sidecar.audioTrackId,
      path: sidecar.path,
      element: audio,
      generation,
    };
  });

  try {
    await Promise.all(prepared.map((sidecar) => new Promise((resolve, reject) => {
      const { element: audio } = sidecar;
      const stale = () => generation !== reviewAudioSidecarGeneration;
      const ready = () => stale() ? reject(new Error("stale audio sidecar")) : resolve();
      const failed = () => reject(new Error(`could not load audio track ${sidecar.audioTrackId}`));
      audio.addEventListener("canplay", ready, { once: true });
      audio.addEventListener("error", failed, { once: true });
      audio.load();
      if (audio.readyState >= 3) ready();
    })));
    if (generation !== reviewAudioSidecarGeneration) throw new Error("stale audio sidecar");
    await syncReviewAudioSidecarSet(prepared, { forceSeek: true, allowPlayback: false });
    return prepared;
  } catch (error) {
    disposeReviewAudioSidecarSet(prepared);
    throw error;
  }
}

async function activatePreparedReviewAudioSidecars(prepared, request) {
  if (!previewRequestStillCurrent(request)) throw new Error("stale audio selection");
  const activationState = {
    currentTime: reviewPlayheadTime(),
    playbackRate: video.playbackRate,
    paused: video.paused,
    ended: video.ended,
  };
  await syncReviewAudioSidecarSet(prepared, {
    forceSeek: true,
    videoState: activationState,
  });
  if (!previewRequestStillCurrent(request)) throw new Error("stale audio selection");

  const finalState = {
    currentTime: reviewPlayheadTime(),
    playbackRate: video.playbackRate,
    paused: video.paused,
    ended: video.ended,
  };
  await syncReviewAudioSidecarSet(prepared, {
    forceSeek: true,
    videoState: finalState,
  });
  if (!previewRequestStillCurrent(request)) throw new Error("stale audio selection");

  const previous = activeReviewAudioSidecars;
  for (const { element: audio } of previous) audio.muted = true;
  activeReviewAudioSidecars = prepared;
  reviewAudioMode = "sidecars";
  applyReviewAudioOutput();
  disposeReviewAudioSidecarSet(previous);
  refreshReviewAudioDriftTimer();
}

function assignReviewVideoSource(path, options = {}) {
  clearReviewAudioSidecars("direct");
  clearReviewSourceErrorHandler();
  const { resumeTime = 0, onLoadedMetadata = null } = options;
  const assignment = { sourceGeneration: ++reviewSourceGeneration };
  reviewSeekState = PlayerCore.beginSourceAssignment(
    reviewSeekState,
    assignment.sourceGeneration,
    resumeTime,
    clipDuration(),
  );
  video.addEventListener("loadedmetadata", () => {
    const decision = PlayerCore.metadataSeekDecision(
      reviewSeekState,
      assignment.sourceGeneration,
      video.duration,
    );
    reviewSeekState = decision.state;
    if (assignment.sourceGeneration !== reviewSourceGeneration) return;
    if (decision.applyTime != null) video.currentTime = decision.applyTime;
    if (typeof onLoadedMetadata === "function") onLoadedMetadata(assignment);
  }, { once: true });
  reviewSourceErrorHandler = () => reportReviewSourceError(assignment);
  video.addEventListener("error", reviewSourceErrorHandler);
  currentReviewMediaPath = path;
  video.src = convertFileSrc(path);
  return assignment;
}

function reportReviewSourceError(assignment) {
  if (assignment.sourceGeneration !== reviewSourceGeneration) return;
  const error = video.error;
  $("stage-note").textContent = `load error ${error ? error.code : "?"}`;
}

function clearReviewSourceErrorHandler() {
  if (!reviewSourceErrorHandler) return;
  video.removeEventListener("error", reviewSourceErrorHandler);
  reviewSourceErrorHandler = null;
}

function releaseReviewVideoSource() {
  clearReviewAudioSidecars("direct");
  clearReviewSourceErrorHandler();
  const sourceGeneration = ++reviewSourceGeneration;
  reviewSeekState = PlayerCore.beginSourceAssignment(
    reviewSeekState,
    sourceGeneration,
    reviewPlayheadTime(),
    clipDuration(),
  );
  video.removeAttribute("src");
  video.load();
}

function restoreVideoAfterRename(path, time, shouldResume, rate) {
  setReviewVideoSource(path, { resumeTime: time, shouldResume, rate });
  currentReviewAudioKey = null;
  requestSelectedAudioPreview();
}

function setReviewVideoSource(path, options = {}) {
  const {
    resumeTime = 0,
    shouldResume = false,
    rate = video.playbackRate,
    trimRange = null,
  } = options;
  const restore = (assignment) => {
    if (assignment.sourceGeneration !== reviewSourceGeneration) return;
    if (trimRange) setTrim(trimRange.start, trimRange.end);
    if (shouldResume) video.play().catch(() => syncPlayState());
    else syncPlayState();
  };
  assignReviewVideoSource(path, { resumeTime, onLoadedMetadata: restore });
  video.playbackRate = rate;
}

function cancelDesiredAudioPreview() {
  audioPreviewQueue = PlayerCore.cancelAudioPreviewRequest(audioPreviewQueue);
  reviewAudioSidecarGeneration += 1;
}

function restoreAudibleAudioSelection(message) {
  selectedAudioTrackIds = new Set(currentReviewAudioTrackIds);
  renderAudioTrackPanel();
  setDeckStatus(message, { transient: true });
}

function previewRequestStillCurrent(request) {
  return Boolean(currentClip)
    && currentClip.path === request.clipPath
    && request.selectionKey === audioSelectionKey(currentClip)
    && request.sourceGeneration === reviewSourceGeneration
    && request.sidecarGeneration === reviewAudioSidecarGeneration;
}

async function runAudioPreviewRequest(request) {
  let prepared = null;
  let error = null;
  try {
    const protectedPreviewPaths = activeReviewAudioSidecars.map((sidecar) => sidecar.path);
    const sidecars = await invoke("prepare_clip_audio_sidecars", {
      request: {
        path: request.clipPath,
        audioTrackIds: request.trackIds,
        protectedPreviewPaths,
      },
    });
    if (previewRequestStillCurrent(request)) {
      prepared = await prepareReviewAudioSidecars(sidecars, request.sidecarGeneration);
    }
  } catch (e) {
    error = String(e);
  }

  const transition = PlayerCore.finishAudioPreviewRequest(
    audioPreviewQueue,
    request.revision,
    error == null,
  );
  audioPreviewQueue = transition.state;

  if (transition.apply && prepared && previewRequestStillCurrent(transition.apply)) {
    try {
      await activatePreparedReviewAudioSidecars(prepared, transition.apply);
      prepared = null;
      currentReviewAudioTrackIds = [...transition.apply.trackIds];
      currentReviewAudioKey = transition.apply.selectionKey;
      setDeckStatus(audioSelectionLabel(currentClip), { transient: true });
    } catch (e) {
      error = String(e);
    }
  }
  if (prepared) {
    disposeReviewAudioSidecarSet(prepared);
    prepared = null;
  }
  if (error && !transition.start && previewRequestStillCurrent(request)) {
    if (ffmpegRuntimeUnavailable(error)) {
      selectedAudioTrackIds = new Set(currentReviewAudioTrackIds);
      renderAudioTrackPanel();
      setDeckStatus("FFmpeg is needed to preview this audio selection.");
      setDeckStatusAction("Install FFmpeg", () => {
        void ensureFfmpegRuntime(() => {
          if (!currentClip || currentClip.path !== request.clipPath) return;
          selectedAudioTrackIds = new Set(request.trackIds);
          renderAudioTrackPanel();
          requestSelectedAudioPreview();
        }).catch(() => {});
      });
    } else {
      restoreAudibleAudioSelection(`audio preview failed: ${error}`);
    }
  }

  if (transition.start) void runAudioPreviewRequest(transition.start);
}

function requestSelectedAudioPreview() {
  const clip = currentClip;
  if (!clip) return;
  const tracks = clipAudioTracks(clip);
  const selected = selectedAudioTrackIdsForClip(clip);
  const selectionKey = audioSelectionKey(clip, selected);
  if (selected.length === 0) {
    cancelDesiredAudioPreview();
    clearReviewAudioSidecars("muted");
    currentReviewAudioTrackIds = [];
    currentReviewAudioKey = selectionKey;
    setDeckStatus(audioSelectionLabel(clip), { transient: true });
    return;
  }
  if (!PlayerCore.reviewSelectionNeedsPreview(tracks, selected)) {
    cancelDesiredAudioPreview();
    clearReviewAudioSidecars("direct");
    currentReviewAudioTrackIds = [...selected];
    currentReviewAudioKey = selectionKey;
    setDeckStatus(audioSelectionLabel(clip), { transient: true });
    return;
  }
  if (selectionKey === currentReviewAudioKey) {
    cancelDesiredAudioPreview();
    setDeckStatus(audioSelectionLabel(clip), { transient: true });
    return;
  }
  const sidecarGeneration = ++reviewAudioSidecarGeneration;
  const queued = PlayerCore.queueAudioPreviewRequest(audioPreviewQueue, {
    clipPath: clip.path,
    trackIds: [...selected],
    selectionKey,
    sourceGeneration: reviewSourceGeneration,
    sidecarGeneration,
  });
  audioPreviewQueue = queued.state;
  setDeckStatus("switching audio tracks...");
  if (queued.start) void runAudioPreviewRequest(queued.start);
}

async function releaseVideoFileHandle() {
  cancelDesiredAudioPreview();
  clearReviewAudioSidecars("direct");
  video.pause();
  releaseReviewVideoSource();
  const releasedSourceGeneration = reviewSourceGeneration;
  await afterNextPaint();
  return releasedSourceGeneration;
}

function reviewAsyncWorkStillCurrent(
  lifecycleWork,
  expectedClipPath,
  expectedSourceGeneration,
) {
  return isForegroundWorkCurrent(lifecycleWork)
    && !!currentClip
    && PlayerCore.sameClipPath(currentClip.path, expectedClipPath)
    && reviewSourceGeneration === expectedSourceGeneration;
}

function suspendReviewPlayback({ renderGallery = true } = {}) {
  setClipTitleEditing(false);
  cancelDesiredAudioPreview();
  clearReviewAudioSidecars("direct");
  clearOverlayIdleCheck();
  video.pause();
  releaseReviewVideoSource();
  reviewSeekState = PlayerCore.createLogicalSeekState();
  currentClip = null;
  activeGroupName = "";
  clearGroupPlaybackPreload();
  currentReviewMediaPath = null;
  currentReviewAudioKey = null;
  currentReviewAudioTrackIds = [];
  selectedAudioTrackIds = new Set();
  resetZoom();
  syncReviewLocalActions();
  syncUploadClipButton();
  updateViews();
  syncPlayState();
  setDeckStatus("");
  $("stage-note").textContent = "";
  $("play-block-layer").replaceChildren();
  $("marker-layer").replaceChildren();
  renderAudioTrackPanel();
  renderGameEventRail(null);
  renderGamePlayRail(null);
  renderGameMetadataPanel(null);
  if (renderGallery) renderClips();
}

function isRenameFileLockError(error) {
  const text = String(error).toLowerCase();
  return text.includes("access is denied")
    || text.includes("os error 5")
    || text.includes("used by another process");
}

async function saveClipRename(ev) {
  ev.preventDefault();
  if (!currentClip || renamePending) return;
  if (isCloudOnlyReviewClip(currentClip)) return;
  const lifecycleWork = captureForegroundWork();
  if (!lifecycleWork) return;
  const oldClip = currentClip;
  const oldPath = oldClip.path;
  const nextName = $("rename-input").value.trim();
  if (!nextName) {
    $("error").textContent = "Clip name cannot be empty.";
    $("rename-input").focus();
    return;
  }

  const resumeTime = reviewPlayheadTime();
  const shouldResume = !video.paused && !video.ended;
  const rate = video.playbackRate;
  renamePending = true;
  setClipRenameControlsDisabled(true);
  $("error").textContent = "";
  setDeckStatus("renaming clip...");
  let expectedSourceGeneration = reviewSourceGeneration;
  await afterNextPaint();

  let mediaReleased = false;
  try {
    if (!reviewAsyncWorkStillCurrent(lifecycleWork, oldPath, expectedSourceGeneration)) return;
    let result;
    try {
      result = await invoke("rename_clip", { path: oldPath, name: nextName });
    } catch (error) {
      if (!isRenameFileLockError(error)) throw error;
      mediaReleased = true;
      expectedSourceGeneration = await releaseVideoFileHandle();
      result = await invoke("rename_clip", { path: oldPath, name: nextName });
    }
    const renamed = applyRenamedClip(oldClip, result);
    setClipTitleEditing(false);
    renderClips();
    setDeckStatus("clip renamed", { transient: true });
    setNotice("clip renamed", { transient: true });
    if (
      mediaReleased
      && reviewAsyncWorkStillCurrent(
        lifecycleWork,
        renamed.path,
        expectedSourceGeneration,
      )
    ) {
      restoreVideoAfterRename(renamed.path, resumeTime, shouldResume, rate);
    }
  } catch (e) {
    $("error").textContent = String(e);
    if (
      mediaReleased
      && reviewAsyncWorkStillCurrent(
        lifecycleWork,
        oldPath,
        expectedSourceGeneration,
      )
    ) {
      restoreVideoAfterRename(oldPath, resumeTime, shouldResume, rate);
    }
  } finally {
    renamePending = false;
    setClipRenameControlsDisabled(false);
  }
}

function setRenameFileControlsDisabled(disabled) {
  $("rename-file-input").disabled = disabled;
  $("rename-file-save").disabled = disabled;
  $("rename-file-cancel").disabled = disabled;
}

function openRenameFileDialog(clip) {
  if (!clip || isCloudOnlyReviewClip(clip)) return;
  renameFileDialogClip = clip;
  $("rename-file-input").value = clip.name || "";
  $("rename-file-status").textContent = "";
  setRenameFileControlsDisabled(false);
  const dialog = $("rename-file-dialog");
  if (!dialog.open) dialog.showModal();
  $("rename-file-input").focus();
  $("rename-file-input").select();
}

function closeRenameFileDialog(force = false) {
  if (renameFilePending && !force) return;
  renameFileDialogClip = null;
  $("rename-file-status").textContent = "";
  setRenameFileControlsDisabled(false);
  const dialog = $("rename-file-dialog");
  if (dialog.open) dialog.close();
}

async function submitRenameFileDialog() {
  const oldClip = renameFileDialogClip;
  if (!oldClip || renameFilePending) return;
  const lifecycleWork = captureForegroundWork();
  if (!lifecycleWork) return;
  const nextName = $("rename-file-input").value.trim();
  if (!nextName) {
    $("rename-file-status").textContent = "File name is required.";
    $("rename-file-input").focus();
    return;
  }

  const oldPath = oldClip.path;
  const isCurrent = currentClip && currentClip.path === oldPath;
  const resumeTime = isCurrent ? reviewPlayheadTime() : 0;
  const shouldResume = isCurrent && !video.paused && !video.ended;
  const rate = video.playbackRate;
  const trimRange = isCurrent ? { start: trimStart, end: trimEnd } : null;
  renameFilePending = true;
  setRenameFileControlsDisabled(true);
  $("rename-file-status").textContent = "Renaming...";
  let expectedSourceGeneration = reviewSourceGeneration;
  await afterNextPaint();

  let mediaReleased = false;
  try {
    if (!isForegroundWorkCurrent(lifecycleWork)) return;
    if (
      isCurrent
      && !reviewAsyncWorkStillCurrent(
        lifecycleWork,
        oldPath,
        expectedSourceGeneration,
      )
    ) {
      return;
    }
    let result;
    try {
      result = await invoke("rename_clip_file", { path: oldPath, name: nextName });
    } catch (error) {
      if (!isCurrent || !isRenameFileLockError(error)) throw error;
      mediaReleased = true;
      expectedSourceGeneration = await releaseVideoFileHandle();
      result = await invoke("rename_clip_file", { path: oldPath, name: nextName });
    }
    const renamed = applyRenamedClip(oldClip, result);
    closeRenameFileDialog(true);
    renderClips();
    setDeckStatus("file renamed", { transient: true });
    setNotice("file renamed", { transient: true });
    const restoreIsCurrent = reviewAsyncWorkStillCurrent(
      lifecycleWork,
      renamed.path,
      expectedSourceGeneration,
    );
    if (isCurrent && renamed.path !== oldPath && restoreIsCurrent) {
      setReviewVideoSource(renamed.path, { resumeTime, shouldResume, rate, trimRange });
      currentReviewAudioKey = null;
      requestSelectedAudioPreview();
    } else if (mediaReleased && restoreIsCurrent) {
      restoreVideoAfterRename(renamed.path, resumeTime, shouldResume, rate);
    }
  } catch (e) {
    $("rename-file-status").textContent = String(e);
    if (
      mediaReleased
      && reviewAsyncWorkStillCurrent(
        lifecycleWork,
        oldPath,
        expectedSourceGeneration,
      )
    ) {
      restoreVideoAfterRename(oldPath, resumeTime, shouldResume, rate);
    }
  } finally {
    renameFilePending = false;
    setRenameFileControlsDisabled(false);
  }
}

function openClip(clip, { preserveGroup = false, autoplay = true } = {}) {
  if (settingsOpen) {
    syncSettingsDraftFromForm({ resetDiscard: false });
    if (settingsHaveUnsavedChanges()) {
      showSettingsDiscardWarning();
      return;
    }
    toggleSettings(false);
  }
  if (!preserveGroup) {
    activeGroupName = "";
    clearGroupPlaybackPreload();
  }
  cancelDesiredAudioPreview();
  clearReviewAudioSidecars("direct");
  clearOverlayIdleCheck();
  reviewSeekState = PlayerCore.createLogicalSeekState();
  currentClip = clip;
  currentReviewAudioKey = null;
  simpleTrimMode = false;
  resetSelectedAudioTracks(clip);
  currentReviewAudioTrackIds = PlayerCore.directPlaybackAudioTrackIds(clipAudioTracks(clip));
  currentReviewAudioKey = audioSelectionKey(clip, currentReviewAudioTrackIds);
  $("error").textContent = "";
  setDeckStatus("");
  $("stage-note").textContent = "loading…";
  setClipTitleEditing(false);
  const group = activeGroup();
  $("pname").textContent = group ? group.name : clipDisplayTitle(clip) || clip.name;
  $("pmeta").textContent = group
    ? groupReviewMeta(group)
    : `${fmtMegabytes(clip.size_mb)} · ${PlayerCore.clipFileLabel(clip)}`;
  syncReviewLocalActions();
  syncUploadClipButton();
  updateViews();
  updateStageFrame();
  assignReviewVideoSource(clip.path, { resumeTime: 0 });
  video.playbackRate = Number($("rate-select").value);
  resetZoom();
  setTrim(0, clip.duration_s ?? (clip.markers ? clip.markers.duration_s : 0));
  renderOverviewMarkers();
  applyView({ start: 0, span: 0 });
  applyTimelineEditorPreference();
  renderAudioTrackPanel();
  renderGameEventRail(clip);
  renderGamePlayRail(clip);
  renderGameMetadataPanel(clip);
  renderClips();
  noteActivity();
  requestAnimationFrame(updateStageFrame);
  if (autoplay) video.play().catch(() => syncPlayState());
  if (clipAudioTracks(clip).length > 0) {
    requestSelectedAudioPreview();
  }
  syncCloudClipStatus(clip);
}

function closeReview() {
  setClipTitleEditing(false);
  cancelDesiredAudioPreview();
  clearReviewAudioSidecars("direct");
  clearOverlayIdleCheck();
  video.pause();
  releaseReviewVideoSource();
  reviewSeekState = PlayerCore.createLogicalSeekState();
  currentClip = null;
  activeGroupName = "";
  clearGroupPlaybackPreload();
  simpleTrimMode = false;
  currentReviewMediaPath = null;
  currentReviewAudioKey = null;
  currentReviewAudioTrackIds = [];
  syncReviewLocalActions();
  syncUploadClipButton();
  selectedAudioTrackIds = new Set();
  resetZoom();
  applyTimelineEditorPreference();
  updateViews();
  setDeckStatus("");
  $("stage-note").textContent = "";
  $("play-block-layer").replaceChildren();
  $("marker-layer").replaceChildren();
  renderAudioTrackPanel();
  renderGameEventRail(null);
  renderGamePlayRail(null);
  renderGameMetadataPanel(null);
  renderClips();
}
