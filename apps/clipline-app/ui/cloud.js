// Clipline Cloud: auth, library sync, uploads.
function fillCloudSettings(cloud) {
  const connected = Boolean(cloud.connected_user_id && cloud.credential_target);
  $("cloud-host-url").value = connected ? "" : cloud.host_url || "";
  $("cloud-username").value = connected ? "" : cloud.connected_username || "";
  $("cloud-password").value = "";
  $("cloud-default-visibility").value = cloud.default_visibility || "private";
  $("cloud-delete-local-after-upload").checked = !!cloud.delete_local_after_upload;
  $("cloud-auto-upload-rules").checked = false;
  syncCloudHttpWarning();
  const displayName = cloudDisplayName(cloud);
  $("cloud-connection-status").textContent = connected
    ? `Connected as ${displayName}`
    : "Not connected";
  $("cloud-connect-fields").hidden = connected;
  $("cloud-connect").hidden = connected;
  $("cloud-connect").disabled = connected;
  $("cloud-disconnect").hidden = !connected;
  $("cloud-disconnect").disabled = !connected;
  $("cloud-connect-status").textContent = "";
  syncRailProfile(cloud);
}

function cloudDisplayName(cloud) {
  return String(
    cloud.connected_display_name
    || cloud.connected_username
    || cloud.connected_user_id
    || "Cloud"
  ).trim() || "Cloud";
}

function railProfileInitials(name) {
  const parts = String(name || "")
    .trim()
    .split(/[\s._-]+/)
    .filter(Boolean);
  if (parts.length >= 2) {
    return `${Array.from(parts[0])[0] || ""}${Array.from(parts[1])[0] || ""}`.toUpperCase();
  }
  return (Array.from(parts[0] || "C").slice(0, 2).join("") || "C").toUpperCase();
}

function setRailProfileFallback(name) {
  const avatar = $("rail-profile-avatar");
  const fallback = document.createElement("span");
  fallback.textContent = railProfileInitials(name);
  avatar.replaceChildren(fallback);
}

function setRailProfileImage(dataUrl, name) {
  const img = document.createElement("img");
  img.alt = "";
  img.addEventListener("error", () => setRailProfileFallback(name), { once: true });
  img.src = dataUrl;
  $("rail-profile-avatar").replaceChildren(img);
}

function syncRailProfile(cloud = cloudSettings()) {
  const connected = Boolean(cloud.connected_user_id && cloud.credential_target);
  const profile = $("rail-profile");
  const avatar = $("rail-profile-avatar");
  const nameEl = $("rail-profile-name");
  if (!connected) {
    railProfileAvatarKey = "";
    railProfileAvatarRequest += 1;
    profile.hidden = true;
    profile.title = "";
    profile.removeAttribute("aria-label");
    avatar.replaceChildren();
    nameEl.textContent = "";
    return;
  }

  const name = cloudDisplayName(cloud);
  const key = `${cloud.host_url || ""}|${cloud.connected_user_id || ""}|${cloud.credential_target || ""}`;
  profile.hidden = false;
  profile.title = `Clipline Cloud: ${name}`;
  profile.setAttribute("aria-label", `Open Clipline Cloud profile for ${name}`);
  nameEl.textContent = name;
  if (!avatar.querySelector("img")) setRailProfileFallback(name);
  if (railProfileAvatarKey === key) return;

  railProfileAvatarKey = key;
  setRailProfileFallback(name);
  const lifecycleWork = captureForegroundWork();
  if (!lifecycleWork) return;
  refreshRailProfileIdentity(key, lifecycleWork);
  loadRailProfileAvatar(key, name, lifecycleWork);
}

async function refreshRailProfileIdentity(
  key,
  lifecycleWork = captureForegroundWork(),
) {
  if (!lifecycleWork) return;
  try {
    const profile = await invoke("cloud_user_profile");
    if (
      !isForegroundWorkCurrent(lifecycleWork)
      || key !== railProfileAvatarKey
      || !profile
    ) return;
    const previousAccountKey = cloudAccountKey();
    const cloud = cloudSettings();
    cloud.connected_user_id = profile.user_id || cloud.connected_user_id;
    cloud.connected_username = profile.username || cloud.connected_username;
    cloud.connected_display_name = profile.display_name || null;
    if (currentSettings) currentSettings.cloud = cloud;
    if (cloudAccountKey() !== previousAccountKey) {
      resetCloudClipsCache();
      if (gallerySource === "cloud") loadCloudClips({ force: true });
    }
    const name = cloudDisplayName(cloud);
    $("rail-profile").title = `Clipline Cloud: ${name}`;
    $("rail-profile").setAttribute("aria-label", `Open Clipline Cloud profile for ${name}`);
    $("rail-profile-name").textContent = name;
    if (!$("rail-profile-avatar").querySelector("img")) setRailProfileFallback(name);
    $("cloud-connection-status").textContent = `Connected as ${name}`;
  } catch (_) {
    // The saved username remains useful if the identity refresh fails offline.
  }
}

async function loadRailProfileAvatar(
  key,
  name,
  lifecycleWork = captureForegroundWork(),
) {
  if (!lifecycleWork) return;
  const request = ++railProfileAvatarRequest;
  try {
    const dataUrl = await invoke("cloud_user_avatar");
    if (
      !isForegroundWorkCurrent(lifecycleWork)
      || request !== railProfileAvatarRequest
      || key !== railProfileAvatarKey
      || !dataUrl
    ) return;
    setRailProfileImage(dataUrl, name);
  } catch (_) {
    // Keep the initials fallback when the account has no reachable avatar.
  }
}

async function openRailProfile() {
  $("error").textContent = "";
  try {
    await invoke("open_cloud_user_profile");
  } catch (e) {
    $("error").textContent = String(e);
  }
}

function cloudInsecureHttpOrigin(raw = $("cloud-host-url").value.trim()) {
  if (!raw) return "";
  try {
    const url = new URL(raw);
    return url.protocol === "http:" ? url.origin : "";
  } catch (_) {
    return "";
  }
}

function syncCloudHttpWarning() {
  const origin = cloudInsecureHttpOrigin();
  const confirm = $("cloud-http-confirm");
  if (confirm.dataset.origin !== origin) {
    confirm.checked = false;
    confirm.dataset.origin = origin;
  }
  $("cloud-http-origin").textContent = origin;
  $("cloud-http-warning").hidden = !origin;
}

function readCloudSettings() {
  const source = settingsFormSource();
  const existing = source.cloud
    ? source.cloud
    : defaultCloudSettings();
  return {
    ...existing,
    default_visibility: $("cloud-default-visibility").value || "private",
    delete_local_after_upload: $("cloud-delete-local-after-upload").checked,
    auto_upload_rules: false,
    uploads: { ...(existing.uploads || {}) },
  };
}

function cloudSettings() {
  return currentSettings && currentSettings.cloud ? currentSettings.cloud : defaultCloudSettings();
}

function cloudConnected() {
  const cloud = cloudSettings();
  return Boolean(cloud.connected_user_id && cloud.credential_target);
}

function cloudUploadRecordForPath(path) {
  const uploads = cloudSettings().uploads || {};
  return Object.values(uploads).find(
    (record) => record && PlayerCore.sameClipPath(record.path, path)
  ) || null;
}

function clipCloudRecord(clip) {
  return clip ? cloudUploadRecordForPath(clip.path) : null;
}

function clipCloudVisibility(record) {
  if (!record || !String(record.upload_status || "").startsWith("uploaded_")) return null;
  const visibility = String(record.visibility || "").toLowerCase();
  if (["public", "unlisted", "private"].includes(visibility)) return visibility;
  return record.upload_status === "uploaded_private" ? "private" : "public";
}

function cloudRecordUploaded(record) {
  return CloudCore.recordUploaded(record);
}

function cloudShareUrl(record) {
  return CloudCore.shareUrl(record);
}

function cloudLibraryRecords() {
  const cloudListAuthoritative = cloudClipsLoaded
    || cloudClipsLoading
    || !cloudConnected();
  return PlayerCore.cloudLibraryEntries(
    cloudSettings().uploads || {},
    clipsCache,
    cloudClipsCache,
    cloudListAuthoritative,
  );
}

function cloudLocalClipForEntry(entry) {
  if (!entry || !entry.local_available || !entry.path) return null;
  return clipsCache.find(
    (clip) => clip && PlayerCore.sameClipPath(clip.path, entry.path)
  ) || null;
}

function isCloudOnlyReviewClip(clip = currentClip) {
  return !!(
    clip
    && clip.cloud_remote_clip_id
    && !clipsCache.some(
      (localClip) => localClip && PlayerCore.sameClipPath(localClip.path, clip.path)
    )
  );
}

function cloudClipAssetRequest(entry) {
  return {
    remote_clip_id: String(entry && entry.remote_clip_id || ""),
    title: entry && entry.title ? String(entry.title) : null,
    duration_ms: entry && Number.isFinite(Number(entry.duration_ms)) ? Number(entry.duration_ms) : null,
    file_size_bytes: entry && Number.isFinite(Number(entry.file_size_bytes)) ? Number(entry.file_size_bytes) : null,
    updated_at_unix: entry && Number.isFinite(Number(entry.updated_at_unix)) ? Number(entry.updated_at_unix) : null,
  };
}

function cloudAccountKey() {
  return CloudCore.accountKey(cloudSettings());
}

function resetCloudClipsCache() {
  cloudClipsRequestGate.invalidate();
  cloudClipsCache = [];
  cloudClipsLoaded = false;
  cloudClipsLoading = false;
  cloudClipsError = "";
  clearCloudPosterCache();
}

async function loadCloudClips({ force = false } = {}) {
  const lifecycleWork = captureForegroundWork();
  if (!lifecycleWork) {
    requestWindowRefresh();
    return;
  }
  if (!cloudConnected()) {
    resetCloudClipsCache();
    if (gallerySource === "cloud") renderCloudClips();
    return;
  }
  if (cloudClipsLoading && !force) return;
  if (cloudClipsError && !force) return;
  if (cloudClipsLoaded && !force) return;

  const accountKey = cloudAccountKey();
  const request = cloudClipsRequestGate.begin(accountKey);
  const isCurrent = () => (
    isForegroundWorkCurrent(lifecycleWork)
    && cloudClipsRequestGate.isCurrent(request, cloudAccountKey())
  );
  cloudClipsLoading = true;
  cloudClipsError = "";
  if (gallerySource === "cloud") renderClips();
  try {
    const result = await invoke("list_cloud_clips");
    if (!isCurrent()) return;
    cloudClipsCache = result && Array.isArray(result.clips) ? result.clips : [];
    pruneCloudPosterCache(cloudClipsCache);
    cloudClipsError = result && result.truncated
      ? "Showing the first 10,000 unique cloud clips; refine the library on the server to see older items."
      : "";
    cloudClipsLoaded = true;
  } catch (error) {
    if (!isCurrent()) return;
    cloudClipsError = String(error);
  } finally {
    if (!isCurrent()) return;
    cloudClipsLoading = false;
    if (gallerySource === "cloud") renderClips();
  }
}

function cloudEntryMatchesSearch(entry) {
  if (!gallerySearch) return true;
  const hay = [
    entry.title,
    entry.path,
    entry.remote_url,
    entry.visibility,
    entry.upload_status,
  ].join(" ").toLowerCase();
  return hay.includes(gallerySearch);
}

function cloudStatusLabel(status) {
  switch (status) {
    case "processing":
    case "uploaded_processing":
      return "processing";
    case "uploaded_private":
      return "private";
    case "uploaded_public":
      return "public";
    default:
      return String(status || "uploaded").replace(/^uploaded_/, "");
  }
}

async function openCloudClipUrl(entry) {
  if (!entry || !entry.remote_clip_id) return;
  try {
    await invoke("open_cloud_clip", { remoteClipId: entry.remote_clip_id });
  } catch (e) {
    $("error").textContent = String(e);
    setDeckStatus("could not open cloud clip", { transient: true });
  }
}

async function openCloudEntryInApp(entry) {
  const localClip = cloudLocalClipForEntry(entry);
  if (localClip) {
    openClip(localClip);
    return;
  }
  if (!entry || !entry.remote_clip_id) {
    await openCloudClipUrl(entry);
    return;
  }
  const lifecycleWork = captureForegroundWork();
  if (!lifecycleWork) return;
  setDeckStatus("downloading cloud clip...");
  try {
    const clip = await invoke("cache_cloud_clip_media", {
      request: cloudClipAssetRequest(entry),
    });
    if (!isForegroundWorkCurrent(lifecycleWork)) return;
    openClip({
      ...clip,
      cloud_remote_clip_id: entry.remote_clip_id,
      cloud_remote_url: entry.remote_url,
      cloud_visibility: entry.visibility,
    });
    setDeckStatus("");
  } catch (e) {
    if (!isForegroundWorkCurrent(lifecycleWork)) return;
    $("error").textContent = String(e);
    setDeckStatus("could not play cloud clip", { transient: true });
  }
}

function cloudThumbnailKey(entry) {
  return `${CLOUD_POSTER_CACHE_PREFIX}${cloudAccountKey()}:${entry.remote_clip_id}:${entry.updated_at_unix || 0}`;
}

function loadCloudThumbnail(entry, thumb) {
  if (!entry || !entry.remote_clip_id) return;
  const lifecycleWork = captureForegroundWork();
  if (!lifecycleWork) return Promise.resolve();
  const key = cloudThumbnailKey(entry);
  const cached = posterCacheGet(key);
  if (cached === POSTER_UNAVAILABLE) return Promise.resolve();
  if (cached) {
    if (!thumb.querySelector(".card-thumb-img")) insertThumbMedia(thumb, makePosterImg(cached));
    return Promise.resolve();
  }
  let pending = cloudThumbnailInflight.get(key);
  if (!pending) {
    pending = invoke("cloud_clip_thumbnail", { request: cloudClipAssetRequest(entry) })
      .then((posterPath) => {
        if (!isForegroundWorkCurrent(lifecycleWork)) return null;
        if (!posterPath) {
          posterCacheSet(key, POSTER_UNAVAILABLE);
          return null;
        }
        const url = convertFileSrc(posterPath);
        posterCacheSet(key, url);
        return url;
      })
      .catch(() => {
        if (isForegroundWorkCurrent(lifecycleWork)) {
          posterCacheSet(key, POSTER_UNAVAILABLE);
        }
        return null;
      })
      .finally(() => {
        if (cloudThumbnailInflight.get(key) === pending) {
          cloudThumbnailInflight.delete(key);
        }
      });
    cloudThumbnailInflight.set(key, pending);
  }
  return pending
    .then((posterPath) => {
      if (!isForegroundWorkCurrent(lifecycleWork)) return;
      const url = posterPath;
      if (!url) return;
      if (thumb.isConnected && !thumb.querySelector(".card-thumb-img")) {
        insertThumbMedia(thumb, makePosterImg(url));
      }
    })
    .catch(() => {});
}

function observeCloudThumbnail(entry, thumb) {
  if (!entry || !entry.remote_clip_id) return;
  if (!captureForegroundWork()) return;
  const request = { type: "cloud-thumbnail", entry };
  if (!posterObserver) {
    Promise.resolve().then(() => queuePosterWork(request, thumb));
    return;
  }
  posterQueue.set(thumb, request);
  posterObserver.observe(thumb);
}

function clipUploadDefaultTitle(clip) {
  return clipDisplayTitle(clip) || PresentationCore.clipNameStem(clip && clip.name) || "Untitled clip";
}

function upsertCloudUploadRecord(record) {
  if (!record || !record.local_clip_id) return;
  const cloud = cloudSettings();
  cloud.uploads = { ...(cloud.uploads || {}), [record.local_clip_id]: record };
  if (currentSettings) currentSettings.cloud = cloud;
}

function replaceCloudRecordPath(oldPath, newPath) {
  const cloud = cloudSettings();
  const uploads = cloud.uploads || {};
  let changed = false;
  const nextUploads = {};
  for (const [key, record] of Object.entries(uploads)) {
    if (record && PlayerCore.sameClipPath(record.path, oldPath)) {
      nextUploads[key] = { ...record, path: newPath };
      changed = true;
    } else {
      nextUploads[key] = record;
    }
  }
  if (!changed) return;
  cloud.uploads = nextUploads;
  if (currentSettings) currentSettings.cloud = cloud;
}

function removeCloudUploadRecordForPath(path) {
  const cloud = cloudSettings();
  const uploads = cloud.uploads || {};
  const nextUploads = {};
  let changed = false;
  for (const [key, record] of Object.entries(uploads)) {
    if (record && PlayerCore.sameClipPath(record.path, path)) {
      changed = true;
      continue;
    }
    nextUploads[key] = record;
  }
  if (!changed) return false;
  cloud.uploads = nextUploads;
  if (currentSettings) currentSettings.cloud = cloud;
  return true;
}

function applyCloudClipSyncResult(
  result,
  { expectedRecord = null, expectedLocalClipId = "", expectedUpdatedAtUnix = 0 } = {},
) {
  if (!result) return false;
  const current = cloudUploadRecordForPath(result.path);
  if (expectedRecord && current !== expectedRecord) return false;
  if (current && expectedLocalClipId && current.local_clip_id !== expectedLocalClipId) return false;
  if (
    current
    && Number(current.updated_at_unix || 0) > Number(expectedUpdatedAtUnix || 0)
  ) {
    return false;
  }
  let changed = false;
  if (result.removed) changed = removeCloudUploadRecordForPath(result.path);
  if (result.record) {
    upsertCloudUploadRecord(result.record);
    changed = true;
  }
  if (changed) {
    renderClips();
    syncUploadClipButton();
  }
  return changed;
}

async function syncCloudClipStatus(clip) {
  if (!clip || !cloudConnected()) return;
  const record = clipCloudRecord(clip);
  if (!record || !record.remote_clip_id) return;
  const expectedRecord = record;
  const expectedLocalClipId = record.local_clip_id || "";
  const expectedUpdatedAtUnix = record.updated_at_unix || 0;
  try {
    const result = await invoke("sync_cloud_clip_status", { request: { path: clip.path } });
    applyCloudClipSyncResult(result, { expectedRecord, expectedLocalClipId, expectedUpdatedAtUnix });
  } catch (_) {
    // Keep the last known cloud state if the status check is unavailable.
  }
}

function upsertCloudProgress(progress) {
  if (!progress || !progress.local_clip_id) return { record: null, renderRequired: false };
  const current = (cloudSettings().uploads || {})[progress.local_clip_id] || {};
  const update = CloudCore.reconcileUploadProgress(
    current,
    progress,
    cloudSettings().default_visibility,
    Date.now() / 1000,
  );
  upsertCloudUploadRecord(update.record);
  return update;
}
async function reloadSettings() {
  const previousAccountKey = cloudAccountKey();
  const backendSettings = await invoke("get_settings");
  currentSettings = CloudCore.mergeBackendCloudSettings(currentSettings || {}, backendSettings);
  settingsDraft = CloudCore.mergeBackendCloudSettings(
    settingsDraft || currentSettings,
    backendSettings,
  );
  if (settingsIndicatorBaseline) {
    settingsIndicatorBaseline = CloudCore.mergeBackendCloudSettings(
      settingsIndicatorBaseline,
      backendSettings,
    );
  }
  fillCloudSettings(settingsDraft.cloud || defaultCloudSettings());
  syncSettingsDirtyState({ resetDiscard: false });
  if (cloudAccountKey() !== previousAccountKey) resetCloudClipsCache();
  if (clipsCache.length) renderClips();
}

async function connectCloud() {
  syncSettingsDraftFromForm({ resetDiscard: false });
  $("error").textContent = "";
  const hostUrl = $("cloud-host-url").value.trim();
  syncCloudHttpWarning();
  const plainHttpOrigin = cloudInsecureHttpOrigin(hostUrl);
  const plainHttpConfirmed = CloudCore.plainHttpConfirmed(
    plainHttpOrigin,
    $("cloud-http-confirm").dataset.origin,
    $("cloud-http-confirm").checked,
  );
  if (plainHttpOrigin && !plainHttpConfirmed) {
    $("cloud-connect-status").textContent = "Confirm plain HTTP password transmission first.";
    $("cloud-http-confirm").focus();
    return;
  }
  $("cloud-connect-status").textContent = "connecting...";
  try {
    await invoke("cloud_connect", {
      request: {
        host_url: hostUrl,
        username: $("cloud-username").value.trim(),
        password: $("cloud-password").value,
        device_name: "Clipline Desktop",
        plain_http_confirmed: plainHttpConfirmed,
        default_visibility: $("cloud-default-visibility").value,
      },
    });
    $("cloud-connect-status").textContent = "connected";
    resetCloudClipsCache();
    await reloadSettings();
    if (gallerySource === "cloud") loadCloudClips({ force: true });
  } catch (e) {
    $("cloud-connect-status").textContent = String(e);
  }
}

async function disconnectCloud() {
  syncSettingsDraftFromForm({ resetDiscard: false });
  $("cloud-connect-status").textContent = "";
  $("error").textContent = "";
  try {
    await invoke("cloud_disconnect");
    resetCloudClipsCache();
    await reloadSettings();
  } catch (e) {
    $("cloud-connect-status").textContent = String(e);
  }
}

async function copyCloudUrl(record) {
  setDeckStatus("");
  $("error").textContent = "";
  if (!cloudShareUrl(record)) {
    setDeckStatus("this clip has no public share link", { transient: true });
    return;
  }
  try {
    await navigator.clipboard.writeText(record.remote_url);
    setDeckStatus("cloud link copied", { transient: true });
  } catch (e) {
    $("error").textContent = String(e);
  }
}

function openUploadDialog(clip) {
  if (!clip) return;
  if (!cloudConnected()) {
    setDeckStatus("");
    $("error").textContent = "Connect Clipline Cloud before uploading.";
    syncUploadClipButton();
    return;
  }
  uploadDialogClip = clip;
  if (currentClip && currentClip.path === clip.path) {
    uploadSelectedAudioTrackIds = new Set(selectedAudioTrackIdsForClip(clip));
  } else {
    uploadSelectedAudioTrackIds = new Set(defaultAudioTrackIds(clip));
  }
  $("upload-title").value = clipUploadDefaultTitle(clip);
  $("upload-description").value = "";
  $("upload-visibility").value = cloudSettings().default_visibility || "private";
  $("upload-dialog-status").textContent = "";
  $("upload-confirm").disabled = false;
  $("upload-cancel").disabled = false;
  renderUploadAudioTracks(clip);
  const dialog = $("upload-dialog");
  if (!dialog.open) dialog.showModal();
  $("upload-title").focus();
  $("upload-title").select();
}

function closeUploadDialog() {
  const dialog = $("upload-dialog");
  uploadDialogClip = null;
  uploadSelectedAudioTrackIds = new Set();
  $("upload-audio-section").hidden = true;
  $("upload-audio-list").replaceChildren();
  $("upload-dialog-status").textContent = "";
  $("upload-confirm").disabled = false;
  $("upload-cancel").disabled = false;
  if (dialog.open) dialog.close();
}

function uploadDialogRequest() {
  const title = $("upload-title").value.trim();
  if (!title) {
    $("upload-dialog-status").textContent = "Title is required.";
    $("upload-title").focus();
    return null;
  }
  const description = $("upload-description").value.trim();
  return {
    title,
    description,
    visibility: $("upload-visibility").value,
    audioTrackIds: clipAudioTracks(uploadDialogClip).length
      ? selectedAudioTrackIdsForClip(uploadDialogClip, uploadSelectedAudioTrackIds)
      : null,
  };
}

function submitUploadDialog() {
  const clip = uploadDialogClip;
  const request = uploadDialogRequest();
  if (!clip || !request) return;
  closeUploadDialog();
  uploadClipToCloud(clip, request);
}

async function uploadClipToCloud(clip, request = {}) {
  if (!clip) return;
  if (!cloudConnected()) {
    setDeckStatus("");
    $("error").textContent = "Connect Clipline Cloud before uploading.";
    syncUploadClipButton();
    return;
  }
  setDeckStatus("uploading to cloud...");
  $("error").textContent = "";
  try {
    const result = await invoke("upload_clip_to_cloud", {
      request: {
        path: clip.path,
        visibility: request.visibility || cloudSettings().default_visibility || "private",
        title: request.title || clipUploadDefaultTitle(clip),
        description: request.description || null,
        audioTrackIds: request.audioTrackIds || null,
      },
    });
    if (result && result.record) {
      upsertCloudUploadRecord(result.record);
      if (result.record.upload_status === "uploaded_private") {
        setDeckStatus("cloud upload ready — private", { transient: true });
      } else if (result.record.remote_url && result.record.upload_status === "uploaded_processing") {
        setDeckStatus("cloud upload processing; link available", { transient: true });
      } else if (result.record.remote_url && result.record.upload_status.startsWith("uploaded_")) {
        setDeckStatus("cloud upload ready", { transient: true });
      } else if (result.record.upload_status === "uploaded_public") {
        setDeckStatus("cloud upload ready; public link unavailable", { transient: true });
      } else if (result.record.upload_status === "failed") {
        setDeckStatus("");
        $("error").textContent = result.record.error || "cloud upload failed";
      } else {
        setDeckStatus("cloud upload processing");
      }
    }
    const refreshCompleted = await refresh();
    finishPostRefreshFeedback(refreshCompleted, {
      error: result && result.record ? result.record.error : "",
      notice: result && result.local_deleted
        ? "cloud upload ready · local copy deleted"
        : "",
    });
    loadCloudClips({ force: true });
  } catch (e) {
    setDeckStatus("");
    $("error").textContent = String(e);
    renderClips();
  }
}
