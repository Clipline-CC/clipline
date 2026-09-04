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

function libraryItemMeta(durationS, sizeMb, modifiedUnix) {
  const parts = [];
  if (Number.isFinite(durationS)) parts.push(fmtDur(durationS));
  if (Number.isFinite(Number(sizeMb))) parts.push(fmtMegabytes(Number(sizeMb)));
  if (Number.isFinite(Number(modifiedUnix))) {
    parts.push(fmtAgo(Date.now() / 1000, Number(modifiedUnix)));
  }
  return parts;
}

function groupNameKey(name) {
  return String(name || "").trim().toLowerCase();
}

function localGroups() {
  const groups = new Map();
  for (const clip of clipsCache) {
    const membership = clip && clip.group;
    const name = String(membership && membership.name || "").trim();
    if (!name) continue;
    const key = groupNameKey(name);
    if (!groups.has(key)) groups.set(key, { name, members: [], modified_unix: 0 });
    const group = groups.get(key);
    group.members.push(clip);
    group.modified_unix = Math.max(group.modified_unix, Number(clip.modified_unix) || 0);
  }
  for (const group of groups.values()) {
    group.members.sort((left, right) => {
      const order = (Number(left.group && left.group.order) || 0)
        - (Number(right.group && right.group.order) || 0);
      return order || (Number(left.modified_unix) || 0) - (Number(right.modified_unix) || 0)
        || String(left.path).localeCompare(String(right.path));
    });
    group.duration_s = group.members.reduce((sum, clip) => {
      const duration = Number(clip.duration_s ?? clip.markers?.duration_s);
      return sum + (Number.isFinite(duration) ? duration : 0);
    }, 0);
    group.size_mb = group.members.reduce(
      (sum, clip) => sum + (Number(clip.size_mb) || 0),
      0,
    );
  }
  return [...groups.values()].sort((left, right) => right.modified_unix - left.modified_unix);
}

function groupForName(name) {
  const key = groupNameKey(name);
  return localGroups().find((group) => groupNameKey(group.name) === key) || null;
}

function activeGroup() {
  return activeGroupName ? groupForName(activeGroupName) : null;
}

function groupFingerprint(group) {
  return group && group.members
    ? group.members.map((clip) => GalleryWindowCore.clipPathKey(clip.path)).join("\0")
    : "";
}

function groupCompilationClip(group = activeGroup()) {
  if (!group) return null;
  const fingerprint = groupFingerprint(group);
  const candidates = clipsCache
    .filter((clip) => clipKind(clip) === "compilation"
      && groupNameKey(clip.source_group) === groupNameKey(group.name)
      && clip.source_group_fingerprint === fingerprint)
    .sort((left, right) => (Number(right.modified_unix) || 0) - (Number(left.modified_unix) || 0));
  return candidates[0] || null;
}

function topLevelLocalClips(clips = clipsCache) {
  return clips.filter((clip) => !clip.group);
}

function groupReviewMeta(group, currentDuration = NaN) {
  const index = group.members.findIndex((clip) => currentClip
    && PlayerCore.sameClipPath(clip.path, currentClip.path));
  const parts = [
    `${group.members.length} clip${group.members.length === 1 ? "" : "s"}`,
    group.duration_s > 0 ? fmtDur(group.duration_s) : "",
    index >= 0 ? `playing ${index + 1} of ${group.members.length}` : "",
    Number.isFinite(currentDuration) ? fmtDur(currentDuration) : "",
  ];
  return parts.filter(Boolean).join(" · ");
}

function visibleLocalGroups() {
  return localGroups().filter((group) =>
    filterGalleryClips(group.members, { groupName: group.name }).items.length);
}

function groupPosterMosaic(group) {
  const mosaic = document.createElement("div");
  const shown = group.members.slice(0, 4);
  mosaic.className = "group-poster-mosaic";
  mosaic.dataset.count = String(shown.length);
  shown.forEach((clip, index) => {
    const cell = document.createElement("div");
    cell.className = `group-poster-cell group-poster-cell-${index + 1}`;
    cell.style.cssText = thumbGradient(clip);
    observePoster(clip.path, cell);
    mosaic.appendChild(cell);
  });
  if (group.members.length > shown.length) {
    const overflow = document.createElement("span");
    overflow.className = "group-poster-overflow";
    overflow.textContent = `+${group.members.length - shown.length}`;
    mosaic.appendChild(overflow);
  }
  return mosaic;
}

function groupCard(group) {
  const card = document.createElement("article");
  card.className = "card group-card";
  card.tabIndex = 0;
  card.setAttribute("role", "button");
  card.setAttribute("aria-label", `Open group ${group.name}`);
  const art = document.createElement("div");
  art.className = "card-thumb group-card-art";
  art.appendChild(groupPosterMosaic(group));
  const kind = document.createElement("span");
  kind.className = "card-kind group";
  kind.textContent = "Group";
  art.appendChild(kind);
  const meta = document.createElement("div");
  meta.className = "card-meta";
  const title = document.createElement("div");
  title.className = "card-name";
  const text = document.createElement("span");
  text.className = "t";
  text.textContent = group.name;
  title.appendChild(text);
  const info = document.createElement("div");
  info.className = "card-sub";
  info.textContent = libraryItemMeta(group.duration_s, group.size_mb, group.modified_unix).join(" · ");
  meta.append(title, info);
  card.append(art, meta);
  const open = () => {
    if (!selectMode) openGroupView(group.name);
  };
  card.addEventListener("click", open);
  card.addEventListener("keydown", (event) => {
    if (event.key !== "Enter" && event.key !== " ") return;
    event.preventDefault();
    open();
  });
  return card;
}

function renderGroupCards(root, groups) {
  if (!groups.length) return;
  const head = document.createElement("div");
  head.className = "gallery-group-head group-section-head";
  const label = document.createElement("span");
  label.textContent = "Groups";
  const count = document.createElement("span");
  count.className = "gcount";
  count.textContent = groups.length;
  head.append(label, count);
  root.appendChild(head);
  for (const group of groups) root.appendChild(groupCard(group));
}

function syncGroupPickerMode() {
  const creating = $("group-picker-select").value === "";
  $("group-picker-new").hidden = !creating;
  if (creating) $("group-picker-name").focus();
}

function openGroupPicker() {
  if (!currentClip) return;
  const groups = localGroups();
  const select = $("group-picker-select");
  select.replaceChildren();
  for (const group of groups) {
    const option = document.createElement("option");
    option.value = group.name;
    option.textContent = group.name;
    select.appendChild(option);
  }
  const create = document.createElement("option");
  create.value = "";
  create.textContent = "New group…";
  select.appendChild(create);
  select.value = groups.length ? groups[0].name : "";
  $("group-picker-name").value = "";
  $("group-picker-status").textContent = "";
  $("group-picker-confirm").disabled = false;
  syncGroupPickerMode();
  if (!$("group-picker-dialog").open) $("group-picker-dialog").showModal();
  if (groups.length) select.focus();
}

function closeGroupPicker() {
  if ($("group-picker-dialog").open) $("group-picker-dialog").close();
}

async function submitGroupPicker() {
  if ($("group-picker-confirm").disabled) return;
  const creating = $("group-picker-select").value === "";
  const name = (creating ? $("group-picker-name").value : $("group-picker-select").value).trim();
  if (!name) {
    $("group-picker-status").textContent = "Group name is required.";
    $("group-picker-name").focus();
    return;
  }
  $("group-picker-status").textContent = "Creating clip…";
  const exported = await exportRangeAsClip(trimStart, trimEnd, {
    button: $("group-picker-confirm"),
    group: name,
  });
  if (exported) closeGroupPicker();
  else $("group-picker-status").textContent = String($("error").textContent || "Could not add clip to group.");
}

function syncGroupReviewChrome() {
  const group = activeGroup();
  const active = Boolean(group);
  $("review-viewer").classList.toggle("group-review", active);
  $("clip-export-row").hidden = active;
  $("open-folder").title = active ? "Show current group clip in Explorer" : "Show this clip in Explorer";
  $("copy-clip").title = active
    ? "Copy group compilation to clipboard"
    : "Copy shareable clip to clipboard (clips over 5 minutes copy the media file; Shift+click copies the original)";
  $("delete-clip").title = active ? "Delete group and all clips" : "Delete source clip from disk";
  if (active) simpleTrimMode = false;
}

function syncGroupReviewHeader() {
  const group = activeGroup();
  if (!group) return;
  $("pname").textContent = group.name;
  $("pmeta").textContent = groupReviewMeta(group, video.duration);
}

function clearGroupPlaybackPreload() {
  const preload = $("group-preload-video");
  preload.pause();
  preload.hidden = true;
  preload.dataset.groupClipPath = "";
  preload.removeAttribute("poster");
  preload.removeAttribute("src");
  preload.load();
}

function preloadNextGroupMember() {
  const group = activeGroup();
  const preload = $("group-preload-video");
  const index = group && currentClip
    ? group.members.findIndex((clip) => PlayerCore.sameClipPath(clip.path, currentClip.path))
    : -1;
  const next = index >= 0 ? group.members[index + 1] : null;
  if (!next) {
    clearGroupPlaybackPreload();
    return;
  }
  if (PlayerCore.sameClipPath(preload.dataset.groupClipPath, next.path)) return;
  preload.pause();
  preload.hidden = true;
  preload.dataset.groupClipPath = next.path;
  const poster = posterCacheGet(next.path);
  if (typeof poster === "string") preload.poster = poster;
  else preload.removeAttribute("poster");
  preload.src = convertFileSrc(next.path);
  preload.load();
}

function beginGroupPlaybackBridge(nextClip) {
  const preload = $("group-preload-video");
  if (!nextClip || !PlayerCore.sameClipPath(preload.dataset.groupClipPath, nextClip.path)) return;
  if (preload.readyState < 2 && !preload.hasAttribute("poster")) return;
  preload.playbackRate = video.playbackRate;
  try { preload.currentTime = 0; } catch (_) { /* Metadata may still be loading; the poster remains. */ }
  preload.hidden = false;
  if (preload.readyState >= 2) preload.play().catch(() => {});
}

function finishGroupPlaybackBridge() {
  const preload = $("group-preload-video");
  preload.hidden = true;
  preload.pause();
  preloadNextGroupMember();
}

function openGroupMember(clip, { autoplay = true } = {}) {
  const group = activeGroup();
  if (!group || !group.members.some((member) => PlayerCore.sameClipPath(member.path, clip.path))) {
    return;
  }
  if (currentClip && !PlayerCore.sameClipPath(currentClip.path, clip.path)) {
    beginGroupPlaybackBridge(clip);
  }
  openClip(clip, { preserveGroup: true, autoplay });
}

function openGroupView(name) {
  const group = groupForName(name);
  if (!group || !group.members.length) return;
  activeGroupName = group.name;
  gameEventRailCollapsed = false;
  openGroupMember(group.members[0]);
}

function advanceGroupPlayback() {
  const group = activeGroup();
  if (!group || !currentClip) return;
  const index = group.members.findIndex((clip) => PlayerCore.sameClipPath(clip.path, currentClip.path));
  if (index < 0 || index + 1 >= group.members.length) {
    setDeckStatus("group playback finished", { transient: true });
    return;
  }
  openGroupMember(group.members[index + 1]);
}

function applyGroupOrderUpdates(updates) {
  for (const update of updates || []) {
    const target = clipsCache.find((candidate) => PlayerCore.sameClipPath(candidate.path, update.path));
    if (target && target.group) target.group.order = update.order;
  }
}

async function moveGroupClip(clip, direction) {
  const group = activeGroup();
  if (!group || !clip) return;
  const from = group.members.findIndex((member) => PlayerCore.sameClipPath(member.path, clip.path));
  const delta = direction === "up" ? -1 : 1;
  const to = Math.max(0, Math.min(group.members.length - 1, from + delta));
  if (from < 0 || from === to) return;
  const ordered = [...group.members];
  ordered.splice(to, 0, ordered.splice(from, 1)[0]);
  await reorderGroupMembers(group, ordered.map((member) => member.path));
}

async function reorderGroupMembers(group, orderedPaths) {
  if (groupReorderPending || !group) return;
  groupReorderPending = true;
  setDeckStatus("reordering group…");
  try {
    const updates = await invoke("reorder_group", { name: group.name, orderedPaths });
    applyGroupOrderUpdates(updates);
    renderClips();
    renderGroupClipRail();
    syncGroupReviewHeader();
    preloadNextGroupMember();
    setDeckStatus("group order updated", { transient: true });
  } catch (error) {
    setDeckStatus("");
    $("error").textContent = String(error);
  } finally {
    groupReorderPending = false;
  }
}

function dropGroupClip(sourcePath, targetPath) {
  const group = activeGroup();
  if (!group || sourcePath === targetPath) return;
  const from = group.members.findIndex((clip) => PlayerCore.sameClipPath(clip.path, sourcePath));
  const to = group.members.findIndex((clip) => PlayerCore.sameClipPath(clip.path, targetPath));
  if (from < 0 || to < 0 || from === to) return;
  const ordered = [...group.members];
  ordered.splice(to, 0, ordered.splice(from, 1)[0]);
  reorderGroupMembers(group, ordered.map((member) => member.path));
}

async function createOpenGroupCompilation() {
  if (!activeGroupName) return null;
  const name = activeGroupName;
  const key = groupNameKey(name);
  if (groupCompilationInflight.has(key)) return groupCompilationInflight.get(key);
  const pending = (async () => {
    setDeckStatus("exporting compilation…");
    await afterNextPaint();
    try {
      const exportedClip = await invoke("export_group", { name });
      invalidateLocalClipsRefresh();
      clipsCache = [
        exportedClip,
        ...clipsCache.filter((clip) => !PlayerCore.sameClipPath(clip.path, exportedClip.path)),
      ];
      renderClips();
      await refreshStorage();
      setDeckStatus("group compilation ready", { transient: true });
      return exportedClip;
    } catch (error) {
      setDeckStatus("");
      $("error").textContent = String(error);
      return null;
    }
  })();
  groupCompilationInflight.set(key, pending);
  try {
    return await pending;
  } finally {
    if (groupCompilationInflight.get(key) === pending) groupCompilationInflight.delete(key);
  }
}

async function copyOpenGroup(event) {
  $("copy-clip").disabled = true;
  const exportedClip = groupCompilationClip() || await createOpenGroupCompilation();
  if (exportedClip) await copyClipToClipboard(event, exportedClip);
  $("copy-clip").disabled = false;
}

async function uploadOpenGroup() {
  $("upload-clip").disabled = true;
  const exportedClip = groupCompilationClip() || await createOpenGroupCompilation();
  if (exportedClip) openUploadDialog(exportedClip);
  syncUploadClipButton();
}

async function deleteOpenGroup() {
  const group = activeGroup();
  if (!group) return;
  const count = group.members.length;
  if (!(await confirmDeleteDialog(
    `Delete group “${group.name}”?`,
    `This deletes all ${count} clip${count === 1 ? "" : "s"} in the group.`,
  ))) return;
  try {
    const generated = clipsCache.filter((clip) =>
      groupNameKey(clip.source_group) === groupNameKey(group.name));
    const paths = [...group.members, ...generated].map((clip) => clip.path);
    const report = await invoke("delete_clips", { paths });
    await applyDeletion(report.deleted);
    const notice = deletionNotice(report.deleted.length);
    if (notice) setNotice(notice, { transient: true });
    $("error").textContent = formatDeletionFailures(report.failed);
  } catch (error) {
    $("error").textContent = String(error);
  }
}

async function removeClipFromGroup(clip) {
  const group = activeGroup();
  if (!group || !clip || !clip.group) return;
  const index = group.members.findIndex((member) => PlayerCore.sameClipPath(member.path, clip.path));
  if (index < 0) return;
  const replacement = group.members[index + 1] || group.members[index - 1] || null;
  const wasCurrent = currentClip && PlayerCore.sameClipPath(currentClip.path, clip.path);
  try {
    await invoke("remove_from_group", { path: clip.path });
    clip.group = null;
    invalidateLocalClipsRefresh();
    if (wasCurrent && replacement) {
      activeGroupName = group.name;
      openGroupMember(replacement);
    } else if (wasCurrent) {
      closeReview();
    } else {
      renderClips();
      renderGroupClipRail();
      syncGroupReviewHeader();
      preloadNextGroupMember();
    }
    setNotice("removed clip from group", { transient: true });
    await refreshStorage();
  } catch (error) {
    $("error").textContent = String(error);
  }
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
  renderClips();
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
  compilation:
    '<svg viewBox="0 0 24 24"><path d="M4 4h12v10H4zM8 8h12v10H8zM11 11v4l4-2z"/></svg>',
};
const CLIP_KIND_LABELS = {
  replay: "Buffered replay",
  session: "Full session",
  trim: "Trimmed export",
  compilation: "Group compilation",
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
