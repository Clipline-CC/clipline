// Review player: export, delete, updates, clipboard, folders.
/* ---- clip actions ---- */

async function exportRangeAsClip(startS, endS, {
  button = null,
  label = "",
  title = "",
  includeMarkers = true,
  group = "",
} = {}) {
  const sourceClip = currentClip;
  if (!sourceClip) return;
  $("error").textContent = "";
  if (button) button.disabled = true;
  setDeckStatus("exporting…");
  await afterNextPaint();
  try {
    const request = {
      path: sourceClip.path,
      startS,
      endS,
      includeMarkers,
    };
    if (title) request.title = title;
    if (group) request.group = group;
    const exported = await invoke("export_clip", request);
    const exportedLabel = label ? `${label} ${exported.name}` : exported.name;
    setDeckStatus(`exported ${exportedLabel} · keyframe-aligned ${fmtTenths(exported.aligned_start_s)} – ${fmtTenths(exported.aligned_end_s)}`, { transient: true });
    const exportedClip = {
      path: exported.path,
      name: exported.name,
      title: title || null,
      kind: "trim",
      session: sourceClip.session || null,
      size_mb: Number(exported.size_mb) || 0,
      modified_unix: exported.modified_unix || Math.floor(Date.now() / 1000),
      duration_s: exported.duration_s,
      markers: exported.markers || null,
      game: sourceClip.game || null,
      group: exported.group || null,
    };
    invalidateLocalClipsRefresh();
    clipsCache = [exportedClip, ...clipsCache.filter((clip) => clip.path !== exportedClip.path)];
    renderClips();
    if (exportedClip.group) {
      setDeckStatusAction("Open group", () => openGroupView(exportedClip.group.name));
    } else {
      setDeckStatusAction("Open clip", () => openClip(exportedClip));
    }
    await refreshStorage();
    return exportedClip;
  } catch (e) {
    setDeckStatus("");
    $("error").textContent = e;
    return null;
  } finally {
    if (button) button.disabled = false;
  }
}

async function exportTrim() {
  await exportRangeAsClip(trimStart, trimEnd, { button: $("export-clip") });
}

async function exportPlayClip() {
  const target = gamePlayContextTarget;
  if (!target || !target.range) return;
  await exportRangeAsClip(target.range.start, target.range.end, {
    label: "play",
    title: target.title,
    includeMarkers: false,
  });
}

const DEFAULT_DELETE_CONFIRM_TITLE = $("confirm-title").textContent;

// In-app modal — the native browser prompt renders "tauri.localhost says".
function confirmDelete(name) {
  return confirmDeleteDialog("Delete this clip?", name);
}

function confirmBulkDelete(count) {
  return confirmDeleteDialog(`Delete ${count} clips?`, "This cannot be undone.");
}

function confirmDeleteDialog(title, detail) {
  return new Promise((resolve) => {
    const dlg = $("confirm-dialog");
    const titleEl = $("confirm-title");
    titleEl.textContent = title;
    $("confirm-detail").textContent = detail;
    const finish = (ok) => {
      dlg.removeEventListener("close", onClose);
      if (dlg.open) dlg.close();
      titleEl.textContent = DEFAULT_DELETE_CONFIRM_TITLE;
      resolve(ok);
    };
    const onClose = () => finish(false); // Esc / backdrop paths
    dlg.addEventListener("close", onClose);
    $("confirm-cancel").onclick = () => finish(false);
    $("confirm-accept").onclick = () => finish(true);
    dlg.showModal();
  });
}

function formatDeletionFailures(failed) {
  return (failed || []).map(([p, m]) => `${p.split(/[\\/]/).pop()}: ${m}`).join("; ");
}

function deletionNotice(count) {
  if (count <= 0) return "";
  return count === 1 ? "deleted 1 clip" : `deleted ${count} clips`;
}

async function applyDeletion(removedPaths) {
  const removed = new Set((removedPaths || []).map(GalleryWindowCore.clipPathKey).filter(Boolean));
  if (!removed.size) return;
  const wasCurrent = currentClip && removed.has(GalleryWindowCore.clipPathKey(currentClip.path));
  const groupBefore = activeGroup();
  let replacement = null;
  if (wasCurrent && groupBefore) {
    const index = groupBefore.members.findIndex((clip) =>
      PlayerCore.sameClipPath(clip.path, currentClip.path));
    const survives = (clip) => !removed.has(GalleryWindowCore.clipPathKey(clip.path));
    replacement = groupBefore.members.slice(index + 1).find(survives)
      || groupBefore.members.slice(0, index).reverse().find(survives)
      || null;
  }
  invalidateLocalClipsRefresh();
  clipsCache = clipsCache.filter(
    (clip) => !removed.has(GalleryWindowCore.clipPathKey(clip.path)),
  );
  if (wasCurrent && replacement) {
    activeGroupName = groupBefore.name;
    openGroupMember(replacement);
  } else if (wasCurrent) closeReview();
  else {
    renderClips();
    if (groupBefore) renderGameEventRail();
  }
  await refreshStorage();
}

function confirmQuit() {
  return new Promise((resolve) => {
    const dlg = $("quit-dialog");
    const finish = (ok) => {
      dlg.removeEventListener("close", onClose);
      if (dlg.open) dlg.close();
      resolve(ok);
    };
    const onClose = () => finish(false); // Esc / backdrop paths
    dlg.addEventListener("close", onClose);
    $("quit-cancel").onclick = () => finish(false);
    $("quit-accept").onclick = () => finish(true);
    dlg.showModal();
  });
}

async function requestWindowClose() {
  if (currentSettings && currentSettings.close_to_tray === false) {
    if (!(await confirmQuit())) return;
  }
  await appWindow.close();
}

function setUpdateStatus(message) {
  $("update-status").textContent = message;
}

function updateUpToDateStatus(update) {
  const version = update.current_version ? ` ${update.current_version}` : "";
  return `${update.channel_label}${version} is up to date`;
}

// The full notes live on the official changelog page — the dialog links there
// ("What's new?") rather than inlining a truncated preview here.

// Reveal the rail button rather than seizing the window. A modal that opens on
// its own interrupts whatever the user came here to do, and Clipline runs for
// days at a time — the button waits until they are ready to deal with it.
function announceUpdate(update) {
  if (!update || !update.available) return;
  pendingUpdate = update;
  const button = $("rail-update");
  button.hidden = false;
  button.title = update.version
    ? `Update to ${update.version}`
    : "A new version is ready";
}

function showUpdateDialog(update) {
  pendingUpdate = update;
  updateDialogUpdate = update;
  $("update-install").disabled = false;
  $("update-cancel").disabled = false;
  $("update-dialog-title").textContent = `${update.channel_label} update available`;
  $("update-dialog-body").textContent =
    `Clipline ${update.version} is available. Current version: ${update.current_version}.`;
  $("update-dialog").showModal();
}

async function checkForUpdates({ manual = false } = {}) {
  if (updateCheckRunning) return;
  updateCheckRunning = true;
  if (manual) setUpdateStatus("checking...");
  try {
    // The dropdown can hold an unsaved channel; a manual Check must honor
    // the selection on screen, while automatic checks use the saved channel.
    const args = manual ? { channel: $("set-update-channel").value } : {};
    const update = await invoke("check_for_updates", args);
    if (update.available) {
      setUpdateStatus(`${update.channel_label} ${update.version} available`);
      // Both callers of this are moments the user is already looking at
      // Clipline — launching it, or clicking Check for updates — so the modal
      // is not an interruption. The button outlives dismissing it.
      announceUpdate(update);
      showUpdateDialog(update);
    } else if (manual) {
      setUpdateStatus(update.status || updateUpToDateStatus(update));
    }
  } catch (e) {
    if (manual) {
      setUpdateStatus(String(e));
    } else {
      console.warn("update check failed:", e);
    }
  } finally {
    updateCheckRunning = false;
  }
}

async function installPendingUpdate() {
  $("update-install").disabled = true;
  $("update-cancel").disabled = true;
  setUpdateStatus("installing update...");
  try {
    // Install re-checks before downloading; keep it on the update the dialog
    // is showing, even if a background event has since replaced the rail
    // button's `pendingUpdate` or the channel is still unsaved in Settings.
    const target = updateDialogUpdate || pendingUpdate;
    await invoke("install_update", target ? { channel: target.channel } : {});
  } catch (e) {
    $("update-install").disabled = false;
    $("update-cancel").disabled = false;
    setUpdateStatus(String(e));
  }
}

async function deleteClip(path = currentClip && currentClip.path) {
  if (currentClip && currentClip.path === path && isCloudOnlyReviewClip(currentClip)) return;
  if (!path) return;
  const name = path.split(/[\\/]/).pop();
  if (!(await confirmDelete(name))) return;
  try {
    await invoke("delete_clip", { path });
    await applyDeletion([path]);
    setNotice("clip deleted", { transient: true });
    $("error").textContent = "";
  } catch (e) {
    $("error").textContent = e;
  }
}

async function toggleClipFavorite(clip) {
  if (!clip || !clip.path) return;
  if (isCloudOnlyReviewClip(clip)) return;
  try {
    const result = await invoke("set_clip_favorite", {
      path: clip.path,
      favorite: !clip.favorite,
    });
    const updated = { ...clip, favorite: !!result.favorite };
    replaceClipInCache(clip.path, updated);
    if (currentClip && currentClip.path === clip.path) currentClip = updated;
    syncFavoriteButton();
    renderClips();
    setNotice(result.favorite ? "added to favorites" : "removed from favorites", {
      transient: true,
    });
    $("error").textContent = "";
  } catch (e) {
    $("error").textContent = e;
  }
}

async function openFolder() {
  if (!currentClip) return;
  if (isCloudOnlyReviewClip(currentClip)) return;
  try {
    await invoke("reveal_clip", { path: currentClip.path });
  } catch (e) {
    $("error").textContent = e;
  }
}

async function copyClipToClipboard(event, clip = currentClip, originalOverride = null) {
  if (!clip) return;
  if (isCloudOnlyReviewClip(clip)) return;
  const reviewingClip = currentClip && PlayerCore.sameClipPath(currentClip.path, clip.path);
  const original = originalOverride
    ?? (Boolean(event?.shiftKey) || Number(clip.duration_s) > 5 * 60);
  const audioTrackIds = reviewingClip
    ? selectedAudioTrackIdsForClip(clip)
    : defaultAudioTrackIds(clip);
  if (reviewingClip) $("copy-clip").disabled = true;
  $("error").textContent = "";
  if (original) {
    if (reviewingClip) setDeckStatus("");
  } else {
    if (reviewingClip) setDeckStatus("preparing shareable clip...");
    else setNotice("preparing shareable clip...");
  }
  try {
    await invoke("copy_clip_to_clipboard", {
      request: {
        path: clip.path,
        audioTrackIds: original
          ? null
          : clipAudioTracks(clip).length
            ? audioTrackIds
            : null,
        original,
      },
    });
    const message = original ? "original clip copied" : "shareable clip copied";
    if (reviewingClip) {
      setDeckStatus(message, { transient: true });
    }
    setNotice(message, { transient: true });
  } catch (e) {
    const error = String(e);
    if (error === "shareable clipboard export cancelled") {
      if (reviewingClip) setDeckStatus("");
      setNotice("");
    } else if (reviewingClip && !original && ffmpegRuntimeUnavailable(error)) {
      setDeckStatus("FFmpeg is needed to prepare a shareable clip.");
      setDeckStatusAction("Install FFmpeg", () => {
        void ensureFfmpegRuntime(() => copyClipToClipboard(null, clip, false)).catch(() => {});
      });
    } else {
      if (reviewingClip) setDeckStatus("");
      setNotice("");
      $("error").textContent = error;
    }
  } finally {
    if (reviewingClip) $("copy-clip").disabled = false;
  }
}

async function chooseMediaFolder() {
  try {
    const selected = await invoke("choose_media_folder");
    if (selected) {
      $("set-media-dir").value = selected;
      syncSettingsDraftFromForm();
      $("settings-status").textContent = "folder selected - save to apply";
    }
  } catch (e) {
    $("error").textContent = e;
  }
}

async function chooseReplayCacheFolder() {
  try {
    const selected = await invoke("choose_replay_cache_folder");
    if (selected) {
      $("set-replay-disk-dir").value = selected;
      syncSettingsDraftFromForm();
      $("settings-status").textContent = "replay cache folder selected - save to apply";
    }
  } catch (e) {
    $("error").textContent = e;
  }
}
